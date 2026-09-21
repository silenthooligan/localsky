//! Classify typed operational errors once, without reflecting raw payloads.
use crate::failure::{Failure, FailureCode as Code};
use crate::net::{safe_fetch::SafeFetchError, source_failure};

pub fn from_anyhow(error: &anyhow::Error, operation: &'static str) -> Failure {
    from_error(error.as_ref(), operation)
}

pub fn from_error(error: &(dyn std::error::Error + 'static), operation: &'static str) -> Failure {
    let mut next = Some(error);
    while let Some(cause) = next {
        if let Some(e) = cause.downcast_ref::<crate::ports::config_store::ConfigStoreError>() {
            return e.diagnostic();
        }
        if let Some(e) =
            cause.downcast_ref::<crate::ports::irrigation_controller::ControllerError>()
        {
            return e.diagnostic();
        }
        if let Some(f) = cause.downcast_ref::<Failure>() {
            return f.clone();
        }
        if let Some(f) = cause.downcast_ref::<Box<Failure>>() {
            return (**f).clone();
        }
        if let Some(e) = cause.downcast_ref::<SafeFetchError>() {
            return source_failure::from_safe(e, operation);
        }
        if let Some(e) = cause.downcast_ref::<reqwest::Error>() {
            return source_failure::from_reqwest(e, operation);
        }
        if let Some(e) = cause.downcast_ref::<axum::extract::multipart::MultipartError>() {
            let mut failure = Failure::new(Code::ApiRejected, operation);
            failure.http_status = Some(e.status().as_u16());
            failure.error_kind = Some("multipart_upload");
            return failure;
        }
        if let Some(e) = cause.downcast_ref::<serde_json::Error>() {
            return source_failure::from_json(e, operation);
        }
        if let Some(e) = cause.downcast_ref::<gribberish::error::GribberishError>() {
            return source_failure::from_grib(e, operation);
        }
        if let Some(e) = cause.downcast_ref::<rusqlite::Error>() {
            return from_sqlite(e, operation);
        }
        if let Some(e) = cause.downcast_ref::<toml::de::Error>() {
            let mut failure = Failure::new(Code::TomlParse, operation);
            failure.byte_offset = e.span().map(|span| span.start);
            return failure;
        }
        if cause.is::<toml::ser::Error>() {
            return Failure::new(Code::Serialize, operation);
        }
        if let Some(e) = cause.downcast_ref::<chrono::ParseError>() {
            let mut failure = Failure::new(Code::DateParse, operation);
            use chrono::format::ParseErrorKind::*;
            failure.error_kind = Some(match e.kind() {
                OutOfRange => "out_of_range",
                Impossible => "impossible_date",
                NotEnough => "missing_date_fields",
                Invalid => "invalid_date_character",
                TooShort => "truncated_date",
                TooLong => "trailing_date_data",
                BadFormat => "invalid_format_specifier",
                _ => "date_parser_unclassified",
            });
            return failure;
        }
        if let Some(e) = cause.downcast_ref::<rumqttc::ConnectionError>() {
            return crate::net::stream_failure::mqtt(e, operation);
        }
        if let Some(e) = cause.downcast_ref::<tokio_tungstenite::tungstenite::Error>() {
            return crate::net::stream_failure::websocket(e, operation);
        }
        if let Some(e) =
            cause.downcast_ref::<crate::persistence::restore_probe::RestoreProbeError>()
        {
            return e.diagnostic();
        }
        if let Some(e) = cause.downcast_ref::<crate::config::loader::LoadError>() {
            use crate::config::loader::LoadError::*;
            return match e {
                NotFound(_) => Failure::new(Code::ConfigMissing, operation),
                Parse(e) => from_error(e, operation),
                Io(_, e) => from_error(e, operation),
                SchemaTooNew { .. } => Failure::new(Code::ConfigSchema, operation),
                UnsetEnvVar(name) => Failure::new(Code::ConfigField, operation).with_resource(name),
                Validation(_) => Failure::new(Code::ConfigField, operation),
            };
        }
        if let Some(e) = cause.downcast_ref::<std::io::Error>() {
            if let Some(inner) = e.get_ref() {
                let failure = from_error(inner, operation);
                if failure.code != Code::DiagnosticMissing {
                    return failure;
                }
            }
            return Failure::from_io(Code::StorageIo, operation, e);
        }
        if let Some(e) = cause.downcast_ref::<tokio::task::JoinError>() {
            return Failure::new(
                if e.is_cancelled() {
                    Code::TaskCancelled
                } else {
                    Code::TaskPanic
                },
                operation,
            );
        }
        if let Some(e) = cause.downcast_ref::<rumqttc::ClientError>() {
            let (request, kind) = match e {
                rumqttc::ClientError::Request(request) => (request, "request_channel_closed"),
                // rumqttc discards TrySendError's full/closed distinction.
                rumqttc::ClientError::TryRequest(request) => {
                    (request, "request_queue_full_or_closed")
                }
            };
            let invalid = match request {
                rumqttc::Request::Publish(publish) if !rumqttc::valid_topic(&publish.topic) => {
                    Some(("topic", "invalid_publish_topic", None))
                }
                rumqttc::Request::Subscribe(subscribe) if subscribe.filters.is_empty() => {
                    Some(("subscriptions", "empty_subscription_list", None))
                }
                rumqttc::Request::Subscribe(subscribe) => subscribe
                    .filters
                    .iter()
                    .position(|filter| !rumqttc::valid_filter(&filter.path))
                    .map(|index| ("subscriptions", "invalid_topic_filter", Some(index))),
                _ => None,
            };
            let mut failure = Failure::new(
                if invalid.is_some() {
                    Code::ConfigField
                } else {
                    Code::MqttQueue
                },
                operation,
            );
            failure.error_kind = Some(kind);
            if let Some((field, kind, index)) = invalid {
                failure.field = Some(field);
                failure.error_kind = Some(kind);
                failure.item_index = index;
            }
            return failure;
        }
        next = cause.source();
    }
    Failure::new(Code::DiagnosticMissing, operation)
}

pub fn from_sqlite(error: &rusqlite::Error, operation: &'static str) -> Failure {
    use rusqlite::Error::*;
    if let Some(code) = error.sqlite_error() {
        let mut failure = Failure::new(Code::Sqlite, operation);
        failure.sqlite_code = Some(code.extended_code);
        return failure;
    }

    let mut f = Failure::new(Code::SqliteShape, operation);
    f.error_kind = Some(match error {
        SqliteFailure(code, _) => {
            f = Failure::new(Code::Sqlite, operation);
            f.sqlite_code = Some(code.extended_code);
            "sqlite_engine"
        }
        SqliteSingleThreadedMode => "single_threaded_mode",
        FromSqlConversionFailure(index, _, _) => {
            f.item_index = Some(*index);
            "column_conversion"
        }
        IntegralValueOutOfRange(index, _) => {
            f.item_index = Some(*index);
            "column_integer_range"
        }
        Utf8Error(index, _) => {
            f.item_index = Some(*index);
            "column_utf8"
        }
        NulError(_) => "embedded_nul",
        InvalidParameterName(_) => "parameter_name",
        InvalidPath(_) => "database_path",
        ExecuteReturnedResults => "execute_returned_rows",
        QueryReturnedNoRows => "expected_row_missing",
        QueryReturnedMoreThanOneRow => "expected_single_row",
        InvalidColumnIndex(index) => {
            f.item_index = Some(*index);
            "column_index"
        }
        InvalidColumnName(_) => "column_name",
        InvalidColumnType(index, _, _) => {
            f.item_index = Some(*index);
            "column_type"
        }
        StatementChangedRows(_) => "affected_row_count",
        ToSqlConversionFailure(_) => "parameter_conversion",
        InvalidQuery => "invalid_query",
        UnwindingPanic => "sqlite_function_panic",
        MultipleStatement => "multiple_statements",
        InvalidParameterCount(_, _) => "parameter_count",
        _ => "sqlite_variant_unclassified",
    });
    f
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sqlite_extended_code_survives_controller_and_anyhow_wrappers() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE sample (id INTEGER PRIMARY KEY); INSERT INTO sample VALUES (1);",
        )
        .unwrap();
        let error = conn
            .execute("INSERT INTO sample VALUES (1)", [])
            .unwrap_err();
        let controller = crate::ports::irrigation_controller::ControllerError::init(from_error(
            &error,
            "watering_commands.request",
        ));
        let wrapped = anyhow::Error::new(controller).context("private SQL value");
        let failure = from_anyhow(&wrapped, "dispatch");
        assert_eq!(failure.code, Code::Sqlite);
        assert_eq!(failure.sqlite_code, Some(1555));
        assert_eq!(failure.operation, "watering_commands.request");
        assert!(!failure.to_string().contains("private SQL value"));
    }

    #[tokio::test]
    async fn worker_panic_is_not_misreported_as_sqlite_failure() {
        let error = tokio::spawn(async {
            panic!("secret panic payload");
        })
        .await
        .unwrap_err();
        let failure = from_error(&error, "read history");
        assert_eq!(failure.code, Code::TaskPanic);
        assert!(!failure.to_string().contains("secret panic payload"));
    }
}

#[cfg(test)]
mod mqtt_client_tests {
    #[test]
    fn invalid_topic_is_not_misdiagnosed_as_a_closed_queue() {
        let (client, eventloop) = rumqttc::AsyncClient::new(
            rumqttc::MqttOptions::new("diagnostic", "localhost", 1883),
            1,
        );
        let error = client
            .try_publish(
                "bad/#/topic",
                rumqttc::QoS::AtMostOnce,
                false,
                "secret payload",
            )
            .unwrap_err();
        let failure = super::from_error(&error, "MQTT test publish");
        assert_eq!(failure.code, crate::failure::FailureCode::ConfigField);
        assert_eq!(failure.error_kind, Some("invalid_publish_topic"));
        assert!(!failure.to_string().contains("secret payload"));
        drop(eventloop);
        let error = client
            .try_publish(
                "valid/topic",
                rumqttc::QoS::AtMostOnce,
                false,
                "secret payload",
            )
            .unwrap_err();
        let failure = super::from_error(&error, "MQTT test publish");
        assert_eq!(failure.code, crate::failure::FailureCode::MqttQueue);
        assert_eq!(failure.error_kind, Some("request_queue_full_or_closed"));
    }
}
