use super::*;
use wiremock::matchers::{header, method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

const TOKEN: &str = "synthetic-private-token";
const REPORT_EPOCH: i64 = 1_789_963_200;

fn source(map: &[(&str, &str)]) -> HaPassthrough {
    HaPassthrough::new(
        "ha_test",
        HaPassthroughConfig {
            base_url: "http://example.invalid".into(),
            bearer_token: TOKEN.into(),
            field_map: map
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            soil_zone_map: Default::default(),
        },
    )
}

fn state(entity: &str, value: &str, unit: &str) -> Value {
    serde_json::json!({
        "entity_id": entity, "state": value,
        "last_reported": chrono::DateTime::from_timestamp(REPORT_EPOCH, 0).unwrap().to_rfc3339(),
        "attributes": {"unit_of_measurement": unit}
    })
}

fn client() -> reqwest::Client {
    // Only the test transport permits loopback. Production obtains this
    // client and URL from the DNS-pinned safe-fetch builder.
    reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap()
}

async fn fetch(source: &HaPassthrough, server: &MockServer) -> anyhow::Result<Vec<StateEntry>> {
    poll_with_deadline(source.fetch_states_with_client(
        &client(),
        format!("{}/api/states", server.uri()).parse().unwrap(),
    ))
    .await
}

async fn endpoint(server: &MockServer, uri: &str, response: ResponseTemplate) {
    Mock::given(method("GET"))
        .and(path(uri))
        .and(header("authorization", format!("Bearer {TOKEN}")))
        .respond_with(response)
        .expect(1)
        .mount(server)
        .await;
}

#[tokio::test]
async fn healthy_bulk_read_stays_one_request() {
    let server = MockServer::start().await;
    endpoint(
        &server,
        "/api/states",
        ResponseTemplate::new(200).set_body_json(vec![state("sensor.temp", "0", "°C")]),
    )
    .await;
    let source = source(&[("AirTempF", "sensor.temp")]);
    let states = fetch(&source, &server).await.unwrap();
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].state, "0");
    assert_eq!(states[0].at_epoch, Some(REPORT_EPOCH));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn bulk_500_recovers_only_deduplicated_mapped_weather_and_soil() {
    let server = MockServer::start().await;
    endpoint(
        &server,
        "/api/states",
        ResponseTemplate::new(500).set_body_string("unrelated entity is not JSON serializable"),
    )
    .await;
    for (entity, value, unit) in [
        ("sensor.temp", "0", "°C"),
        ("sensor.rain", "0", "mm"),
        ("sensor.soil", "42", "%"),
    ] {
        endpoint(
            &server,
            &format!("/api/states/{entity}"),
            ResponseTemplate::new(200).set_body_json(state(entity, value, unit)),
        )
        .await;
    }
    let mut source = source(&[
        ("AirTempF", "sensor.temp"),
        ("DewPointF", "sensor.temp"),
        ("RainTodayIn", "sensor.rain"),
    ]);
    source
        .config
        .soil_zone_map
        .insert("sensor.soil".into(), "garden".into());
    source
        .config
        .soil_zone_map
        .insert("sensor.unbound".into(), " ".into());
    let states = fetch(&source, &server).await.unwrap();
    assert_eq!(states.len(), 3);
    assert!(states.iter().all(|s| s.at_epoch == Some(REPORT_EPOCH)));
    let poll = source.observations(&states, REPORT_EPOCH + 300);
    let mut seen_temp = false;
    let mut seen_rain = false;
    let mut seen_soil = false;
    for event in poll.events {
        match event {
            SourceEvent::Observation {
                fields, at_epoch, ..
            } => {
                assert_eq!(
                    at_epoch, REPORT_EPOCH,
                    "fallback must not refresh report age"
                );
                seen_temp |= fields.contains(&(WeatherField::AirTempF, 32.0));
                seen_rain |= fields.contains(&(WeatherField::RainTodayIn, 0.0));
            }
            SourceEvent::KeyedReading {
                value, at_epoch, ..
            } => {
                assert_eq!(at_epoch, REPORT_EPOCH);
                assert_eq!(value, 42.0);
                seen_soil = true;
            }
            _ => panic!("unexpected observation event"),
        }
    }
    assert!(seen_temp && seen_rain && seen_soil);
    let requests = server.received_requests().await.unwrap();
    assert_eq!(
        requests.len(),
        4,
        "one bulk + three unique configured entities"
    );
    assert!(requests
        .iter()
        .all(|r| r.method == "GET" && r.body.is_empty()));
}

#[tokio::test]
async fn missing_mapped_entity_stays_absent_instead_of_zero() {
    let server = MockServer::start().await;
    endpoint(&server, "/api/states", ResponseTemplate::new(500)).await;
    endpoint(
        &server,
        "/api/states/sensor.temp",
        ResponseTemplate::new(200).set_body_json(state("sensor.temp", "22", "°C")),
    )
    .await;
    endpoint(
        &server,
        "/api/states/sensor.rain",
        ResponseTemplate::new(404),
    )
    .await;
    let source = source(&[("AirTempF", "sensor.temp"), ("RainTodayIn", "sensor.rain")]);
    let states = fetch(&source, &server).await.unwrap();
    assert_eq!(states.len(), 1);
    let poll = source.observations(&states, REPORT_EPOCH + 30);
    assert!(!poll.events.iter().any(|ev| matches!(ev,
        SourceEvent::Observation { fields, .. } if fields.iter().any(|(f, _)| *f == WeatherField::RainTodayIn))));
}

#[tokio::test]
async fn authentication_redirects_and_other_server_failures_do_not_fan_out() {
    for status in [401, 403, 404, 302, 307, 429, 502, 503] {
        let server = MockServer::start().await;
        endpoint(
            &server,
            "/api/states",
            ResponseTemplate::new(status)
                .set_body_raw("private-response-body", "text/html; charset=utf-8")
                .insert_header(
                    "Location",
                    "http://example.invalid/private-login?token=private-redirect",
                ),
        )
        .await;
        let source = source(&[("AirTempF", "sensor.temp")]);
        let failure = fetch(&source, &server)
            .await
            .unwrap_err()
            .downcast::<SourceFailure>()
            .unwrap();
        assert_eq!(failure.http_status, Some(status));
        assert_eq!(failure.code, SourceFailure::http(status, None, "test").code);
        let error = failure.to_string();
        assert!(error.contains(&format!("HTTP {status}")), "{error}");
        assert!(error.contains("HTML"), "{error}");
        for secret in [
            TOKEN,
            "private-response-body",
            "private-redirect",
            "example.invalid",
        ] {
            assert!(
                !error.contains(secret),
                "error leaked upstream/config content"
            );
        }
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
}

#[tokio::test]
async fn failed_mapped_endpoint_does_not_publish_a_partial_success() {
    let server = MockServer::start().await;
    endpoint(&server, "/api/states", ResponseTemplate::new(500)).await;
    // The other sensor succeeds before the failing response arrives.
    endpoint(
        &server,
        "/api/states/sensor.temp",
        ResponseTemplate::new(200).set_body_json(state("sensor.temp", "22", "°C")),
    )
    .await;
    endpoint(
        &server,
        "/api/states/sensor.rain",
        ResponseTemplate::new(500)
            .set_body_string("private-upstream-trace")
            .set_delay(Duration::from_millis(50)),
    )
    .await;
    let source = source(&[("AirTempF", "sensor.temp"), ("RainTodayIn", "sensor.rain")]);
    let error = fetch(&source, &server).await.unwrap_err().to_string();
    assert!(error.contains("LS_HA_FALLBACK_FAILED"));
    assert!(error.contains("LS_HTTP_SERVER"));
    assert!(error.contains("entity=sensor.rain"));
    assert!(!error.contains("private-upstream-trace"));
}

#[tokio::test]
async fn mapped_response_must_identify_the_requested_entity() {
    let server = MockServer::start().await;
    endpoint(&server, "/api/states", ResponseTemplate::new(500)).await;
    endpoint(
        &server,
        "/api/states/sensor.temp",
        ResponseTemplate::new(200).set_body_json(state("sensor.other", "90", "°C")),
    )
    .await;
    let source = source(&[("AirTempF", "sensor.temp")]);
    let error = fetch(&source, &server).await.unwrap_err().to_string();
    assert!(error.contains("not the requested state"));
}

#[tokio::test]
async fn successful_status_with_login_html_is_not_weather() {
    let server = MockServer::start().await;
    endpoint(
        &server,
        "/api/states",
        ResponseTemplate::new(200).set_body_string("<html>private-login-body</html>"),
    )
    .await;
    let source = source(&[("AirTempF", "sensor.temp")]);
    let error = fetch(&source, &server).await.unwrap_err().to_string();
    assert!(error.contains("LS_JSON_SYNTAX"));
    assert!(!error.contains("private-login-body"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn fallback_without_mappings_does_not_claim_recovery() {
    let server = MockServer::start().await;
    endpoint(&server, "/api/states", ResponseTemplate::new(500)).await;
    let source = source(&[]);
    let error = fetch(&source, &server).await.unwrap_err().to_string();
    assert!(error.contains("no mapped entities"));
    assert_eq!(server.received_requests().await.unwrap().len(), 1);
}

#[tokio::test]
async fn complete_fallback_shares_one_eight_second_budget() {
    let server = MockServer::start().await;
    endpoint(
        &server,
        "/api/states",
        ResponseTemplate::new(500).set_delay(Duration::from_secs(5)),
    )
    .await;
    endpoint(
        &server,
        "/api/states/sensor.temp",
        ResponseTemplate::new(200)
            .set_body_json(state("sensor.temp", "22", "°C"))
            .set_delay(Duration::from_secs(5)),
    )
    .await;
    let source = source(&[("AirTempF", "sensor.temp")]);
    let error = fetch(&source, &server).await.unwrap_err().to_string();
    assert!(error.contains("LS_NET_TIMEOUT"), "{error}");
    assert!(error.contains("timeout_ms=8000"), "{error}");
}

#[tokio::test]
async fn entity_mapping_cannot_add_a_query_or_change_the_endpoint_host() {
    let server = MockServer::start().await;
    endpoint(&server, "/api/states", ResponseTemplate::new(500)).await;
    let entity = "sensor.temp/other?token=not-a-query";
    Mock::given(method("GET"))
        .and(path_regex("^/api/states/"))
        .respond_with(ResponseTemplate::new(200).set_body_json(state(entity, "22", "°C")))
        .mount(&server)
        .await;
    let source = source(&[("AirTempF", entity)]);
    fetch(&source, &server).await.unwrap();
    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 2);
    assert!(requests[1].url.query().is_none());
    assert!(requests[1]
        .url
        .path()
        .starts_with("/api/states/sensor.temp%2F"));
}
