//! Stable, safe diagnostics at the application boundary. Codes describe observed
//! failures, never an inferred exception inside another server. Raw URLs,
//! response bodies and arbitrary upstream error strings do not belong here.

use serde::Serialize;

macro_rules! codes {
    ($( $name:ident => ($code:literal, $message:literal, $next:literal) ),+ $(,)?) => {
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
        pub enum FailureCode {
            $(#[serde(rename = $code)] $name),+
        }
        impl FailureCode {
            #[cfg(test)]
            const ALL: &'static [Self] = &[$(Self::$name),+];
            pub fn as_str(self) -> &'static str {
                match self { $(Self::$name => $code),+ }
            }
            fn message(self) -> &'static str {
                match self { $(Self::$name => $message),+ }
            }
            fn next_step(self) -> &'static str {
                match self { $(Self::$name => $next),+ }
            }
        }
    };
}

codes! {
    HttpUnauthorized => ("LS_HTTP_401", "upstream rejected authentication", "Check the source credential and proxy Authorization forwarding."),
    HttpForbidden => ("LS_HTTP_403", "upstream denied access", "Check the source account permissions and proxy access rules."),
    HttpNotFound => ("LS_HTTP_404", "upstream API route was not found", "Check the configured API base address and supported API version."),
    HttpRateLimited => ("LS_HTTP_429", "upstream rate limited the request", "Check the provider quota and polling interval."),
    HttpRedirect => ("LS_HTTP_REDIRECT", "upstream redirected the API request", "Configure the API address directly, without an interactive login redirect."),
    HttpServer => ("LS_HTTP_SERVER", "upstream returned a server error", "Check source and proxy logs at this timestamp; compare direct and proxied API responses. HTTP status alone does not identify the server exception."),
    HttpRejected => ("LS_HTTP_REJECTED", "upstream rejected the request", "Check the reported status against the provider API contract."),
    Timeout => ("LS_NET_TIMEOUT", "request deadline expired", "Check source availability and latency from the LocalSky host."),
    Connection => ("LS_NET_CONNECT", "connection to the source failed", "Check DNS, routing, port and TLS trust from the LocalSky host; use the OS error kind/code when present."),
    TlsCertificate => ("LS_TLS_CERTIFICATE", "source TLS certificate validation failed", "Use the certificate reason to fix the certificate chain, validity or hostname. Keep certificate verification enabled."),
    TlsProtocol => ("LS_TLS_PROTOCOL", "TLS negotiation with the source failed", "Check the source HTTPS port and TLS configuration; use the reported TLS reason."),
    Request => ("LS_NET_REQUEST", "HTTP request could not be sent", "Check the configured API address and credential header format."),
    Body => ("LS_NET_BODY", "response body transfer failed", "Check for interrupted or malformed HTTP responses in source/proxy logs."),
    Decode => ("LS_DATA_DECODE", "response could not be decoded", "Check that the endpoint returns the documented data format, not a login page."),
    JsonSyntax => ("LS_JSON_SYNTAX", "response contains invalid JSON syntax", "Check the API/proxy response format at the reported line and column."),
    JsonShape => ("LS_JSON_SHAPE", "JSON does not match the expected response schema", "Check the source API version and response schema at the reported line and column."),
    JsonTruncated => ("LS_JSON_TRUNCATED", "JSON response ended before the value was complete", "Check source/proxy logs for a truncated response."),
    InvalidUrl => ("LS_CONFIG_URL", "configured API address is invalid", "Set a complete HTTP or HTTPS API base address with a host."),
    UnsupportedScheme => ("LS_CONFIG_SCHEME", "API address must use HTTP or HTTPS", "Correct the source URL scheme."),
    Dns => ("LS_NET_DNS", "source hostname did not resolve", "Check name resolution inside the LocalSky container; inspect the reported resolver error kind/code."),
    BlockedTarget => ("LS_NET_TARGET_BLOCKED", "source address is not a permitted device endpoint", "Use a normal LAN or public address; loopback, link-local, metadata and multicast targets are rejected."),
    ClientBuild => ("LS_NET_CLIENT_INIT", "HTTP client initialization failed", "Check the LocalSky runtime TLS trust configuration."),
    BodyTooLarge => ("LS_DATA_SIZE_LIMIT", "response exceeded the permitted byte limit", "Check the API response size and the reported limit; the response was not ingested."),
    HaEntityState => ("LS_HA_ENTITY_STATE", "HA response was not the requested state", "Check the mapped entity endpoint and HA/proxy routing; no partial readings were published."),
    HaNoMappings => ("LS_HA_NO_MAPPINGS", "no mapped entities are available for HA recovery", "Configure weather or soil entity mappings and inspect the bulk API failure in HA logs."),
    HaFallback => ("LS_HA_FALLBACK_FAILED", "HA bulk read and mapped-entity fallback both failed", "Inspect both recorded causes; check HA/proxy logs at this timestamp. No partial readings were published."),
    StreamClosed => ("LS_STREAM_CLOSED", "upstream stream closed", "Check source availability and reconnect; retained observations keep their original age."),
    StreamProtocol => ("LS_STREAM_PROTOCOL", "stream protocol failed", "Use the protocol category to check endpoint compatibility and proxy WebSocket support."),
    MqttProtocol => ("LS_MQTT_PROTOCOL", "MQTT connection or protocol failed", "Use the broker return code or protocol category to check broker credentials, permissions and connectivity."),
    UdpIo => ("LS_UDP_IO", "UDP listener operation failed", "Use the OS error kind/code to check the bind address, port ownership and host network configuration."),
    UpdateManifest => ("LS_UPDATE_MANIFEST", "release manifest contains an invalid or missing value", "Check the identified manifest field and release feed; the previous successful version check has been retained."),
    ProviderOffline => ("LS_PROVIDER_OFFLINE", "provider is unavailable", "Check the provider connection and its last recorded transport failure."),
    LlmModel => ("LS_LLM_MODEL_UNAVAILABLE", "requested language model is unavailable", "Check the configured model name and the provider's installed model list."),
    RestoreSchema => ("LS_RESTORE_SCHEMA", "restore schema or recovery state is inconsistent", "Keep the recovery journal, stages and previous files. Inspect the named validation step and restore a complete compatible backup before restarting."),
    ApiRejected => ("LS_API_REJECTED", "LocalSky rejected the API request", "Check the response validation details, method and route. Use the request ID to correlate server logs."),
    ApiServer => ("LS_API_SERVER", "LocalSky could not complete the API operation", "Use the request ID, route, timestamp and response detail to locate the failure in LocalSky logs."),
    BrowserNetwork => ("LS_BROWSER_NETWORK", "browser could not reach LocalSky", "Check the LocalSky address, proxy and connection. Browser security hides DNS/TLS details; use the browser network panel and server logs."),
    ConfigMissing => ("LS_CONFIG_MISSING", "configuration file was not found", "Check the data mount and complete setup if this is a new installation."),
    ConfigSchema => ("LS_CONFIG_SCHEMA", "configuration schema is not supported by this binary", "Check the reported schema versions and use a compatible LocalSky release before restoring or loading this configuration."),
    SnapshotMissing => ("LS_CONFIG_SNAPSHOT_MISSING", "requested configuration snapshot was not found", "Refresh the available snapshots and select an existing version."),
    TomlParse => ("LS_TOML_PARSE", "configuration contains invalid TOML or an incompatible value", "Check the reported byte offset and field against the configuration schema; do not share secrets from the file."),
    Serialize => ("LS_DATA_SERIALIZE", "data could not be serialized", "Check the reported operation and data schema; this is a LocalSky error requiring a reproducible report."),
    DateParse => ("LS_DATA_DATE", "stored date could not be parsed", "Check the reported field and parser category against the expected ISO date format."),
    CheckpointBusy => ("LS_STORAGE_CHECKPOINT_BUSY", "database checkpoint could not finish", "Check long-lived database readers and retry the operation; durability has not been confirmed."),
    StorageIo => ("LS_STORAGE_IO", "filesystem operation failed", "Use the operation, OS error kind and code to check storage permissions, available space and mount availability."),
    Sqlite => ("LS_SQLITE", "database operation failed", "Use the SQLite extended code and operation to check schema, constraints, locks, disk space or database integrity."),
    SqliteShape => ("LS_SQLITE_SHAPE", "database result does not match the expected schema", "Inspect the identified database operation and column index; verify migrations completed on this database."),
    TaskCancelled => ("LS_TASK_CANCELLED", "background operation was cancelled", "Check shutdown or worker cancellation at this timestamp; completion was not confirmed."),
    TaskPanic => ("LS_TASK_PANIC", "background operation panicked", "Check the panic trace at this timestamp and include the operation and LocalSky revision in a bug report."),
    WateringHeld => ("LS_WATERING_HELD", "watering is held before dispatch", "Read the accompanying hold reason and system readiness; no controller command was sent."),
    ControllerOffline => ("LS_CONTROLLER_OFFLINE", "no current controller status is available", "Check the controller connection and the preceding status failure at this timestamp."),
    ZoneMapping => ("LS_CONTROLLER_ZONE_MAPPING", "zone has no station mapping on this controller", "Edit the zone's Controller station and select the matching physical valve."),
    UnsupportedOperation => ("LS_CONTROLLER_UNSUPPORTED", "controller does not support this operation", "Check the controller's reported capabilities and choose a supported operation."),
    ConfigField => ("LS_CONFIG_FIELD", "configuration field is invalid", "Correct the identified field and its validation message before saving."),
    MqttDisconnected => ("LS_MQTT_DISCONNECTED", "MQTT broker is disconnected; command was not delivered", "Check the broker connection and credentials; the shutoff deadline remains armed."),
    MqttDeviceOffline => ("LS_MQTT_DEVICE_OFFLINE", "MQTT device reports offline; command was not delivered", "Check device power and its availability topic; the shutoff deadline remains armed."),
    MqttQueue => ("LS_MQTT_QUEUE", "MQTT request could not be queued", "Check the broker worker lifecycle and request channel; no delivery was confirmed."),
    RetryExhausted => ("LS_RETRY_EXHAUSTED", "all permitted attempts failed", "Inspect each recorded attempt and its operation; correct the underlying failure before retrying."),
    Compression => ("LS_DATA_COMPRESSION", "compressed payload could not be decoded", "Check the source content encoding and payload integrity; inspect the decoder OS error kind when present."),
    BatchFailed => ("LS_BATCH_FAILED", "every requested item failed", "Inspect the per-item causes and mapping indexes; no successful result was available."),
    BatchPartial => ("LS_BATCH_PARTIAL", "some requested items failed", "Inspect the per-item causes and mapping indexes; successful readings retain their own age."),
    QueryEmpty => ("LS_QUERY_NO_SAMPLE", "query returned no numeric sample", "Run the identified query against the source and check the selected series, time range and value type."),
    QueryRejected => ("LS_QUERY_REJECTED", "source rejected the query", "Check the identified query and source query logs at this timestamp."),
    MissingField => ("LS_DATA_FIELD_MISSING", "response is missing a required field", "Check the identified response field against the provider API version and mapped device."),
    DeviceMissing => ("LS_SOURCE_DEVICE_MISSING", "no matching configured device was found", "Check the account, device identifier and source mappings."),
    ProviderRejected => ("LS_PROVIDER_REJECTED", "provider reported an application-level failure", "Look up the reported provider code and operation; inspect provider logs when no code was supplied."),
    SigningKey => ("LS_CONFIG_SIGNING_KEY", "signing key is not a valid supported PKCS#8 private key", "Check the configured key format and algorithm; do not paste the key into diagnostic reports."),
    GribMessage => ("LS_GRIB_MESSAGE", "payload contains no readable GRIB message", "Check the selected radar product and provider response format."),
    GribDecode => ("LS_GRIB_DECODE", "GRIB data decoding failed", "Check the reported decoder stage and product; retain the product timestamp for a reproducible report."),
    GribOutside => ("LS_GRIB_OUTSIDE_GRID", "deployment location lies outside the product grid", "Check deployment coordinates and select a product covering the location."),
    GribGrid => ("LS_GRIB_GRID", "GRIB grid geometry or cell index is invalid", "Check the selected product grid type and the identified geometry field."),
    DiagnosticMissing => ("LS_SOURCE_DIAGNOSTIC_MISSING", "source adapter returned an untyped failure", "Include the source ID, operation and LocalSky revision in a bug report: this adapter discarded the failure type and needs instrumentation."),
}

/// The stable diagnostic envelope shared by logs, the bus and privileged
/// health/diagnostics. Optional evidence is absent when the client cannot
/// observe it. In particular, a remote HTTP 500 never becomes a guessed cause.
#[derive(Debug, Clone, Serialize)]
pub struct Failure {
    pub code: FailureCode,
    pub operation: &'static str,
    pub message: &'static str,
    pub next_step: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub http_status: Option<u16>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub response_format: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub entity_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub io_kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub os_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sqlite_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_kind: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub byte_offset: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tls_reason: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub column: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_ms: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub item_index: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_code: Option<i64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resource_id: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub causes: Vec<Failure>,
    #[serde(skip_serializing_if = "is_zero")]
    pub omitted_causes: usize,
}

/// One recorded failure shared by source health and API responses.
#[derive(Debug, Clone, Serialize)]
pub struct FailureRecord {
    pub at_epoch: i64,
    pub failure: Failure,
}

#[cfg(feature = "ssr")]
impl FailureRecord {
    pub fn now(failure: Failure) -> Self {
        Self {
            at_epoch: chrono::Utc::now().timestamp(),
            failure,
        }
    }
}

impl Failure {
    pub fn new(code: FailureCode, operation: &'static str) -> Self {
        Self {
            code,
            operation,
            message: code.message(),
            next_step: code.next_step(),
            http_status: None,
            response_format: None,
            entity_id: None,
            io_kind: None,
            os_code: None,
            sqlite_code: None,
            error_kind: None,
            byte_offset: None,
            tls_reason: None,
            line: None,
            column: None,
            timeout_ms: None,
            limit_bytes: None,
            item_index: None,
            field: None,
            provider_code: None,
            resource_id: None,
            causes: Vec::new(),
            omitted_causes: 0,
        }
    }

    pub fn with_entity(mut self, entity: &str) -> Self {
        // Normal HA identifiers are useful evidence. Never echo arbitrary
        // configured URL-like text, control characters or credential fragments.
        if entity.len() <= 255
            && entity.split_once('.').is_some_and(|(domain, name)| {
                !domain.is_empty()
                    && !name.is_empty()
                    && domain
                        .bytes()
                        .chain(name.bytes())
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'_')
            })
        {
            self.entity_id = Some(entity.to_owned());
        }
        self
    }

    pub fn with_item(mut self, index: usize) -> Self {
        self.item_index = Some(index);
        self
    }

    pub fn with_field(mut self, field: &'static str) -> Self {
        self.field = Some(field);
        self
    }

    pub fn with_provider_code(mut self, code: Option<i64>) -> Self {
        self.provider_code = code;
        self
    }

    /// Public device/product identifiers only, never request URLs or secrets.
    pub fn with_resource(mut self, id: &str) -> Self {
        if !id.is_empty()
            && id.len() <= 128
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_.-:".contains(&c))
        {
            self.resource_id = Some(id.to_owned());
        }
        self
    }

    pub fn from_io(code: FailureCode, operation: &'static str, error: &std::io::Error) -> Self {
        let mut failure = Self::new(code, operation);
        failure.io_kind = Some(format!("{:?}", error.kind()));
        failure.os_code = error.raw_os_error();
        failure
    }

    pub fn attempts(operation: &'static str, causes: Vec<Self>) -> Self {
        let mut failure = Self::new(FailureCode::RetryExhausted, operation);
        failure.causes = causes;
        failure.bound_causes(4);
        failure
    }

    pub fn batch(operation: &'static str, causes: Vec<Self>, partial: bool) -> Self {
        let mut failure = Self::new(
            if partial {
                FailureCode::BatchPartial
            } else {
                FailureCode::BatchFailed
            },
            operation,
        );
        failure.causes = causes;
        failure.bound_causes(4);
        failure
    }

    // A faulty provider/device list must not grow health or support payloads without bound.
    fn bound_causes(&mut self, depth: usize) {
        let retained = if depth == 0 {
            0
        } else {
            self.causes.len().min(16)
        };
        self.omitted_causes += self.causes.len() - retained;
        self.causes.truncate(retained);
        for cause in &mut self.causes {
            cause.bound_causes(depth.saturating_sub(1));
        }
    }

    pub fn http(status: u16, format: Option<&'static str>, operation: &'static str) -> Self {
        use FailureCode::*;
        let code = match status {
            401 => HttpUnauthorized,
            403 => HttpForbidden,
            404 => HttpNotFound,
            429 => HttpRateLimited,
            300..=399 => HttpRedirect,
            500..=599 => HttpServer,
            _ => HttpRejected,
        };
        let mut failure = Self::new(code, operation);
        failure.http_status = Some(status);
        failure.response_format = format;
        failure
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl std::fmt::Display for Failure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} {}: {}",
            self.code.as_str(),
            self.operation,
            self.message
        )?;
        if let Some(status) = self.http_status {
            write!(f, "; HTTP {status}")?;
        }
        if let Some(format) = self.response_format {
            write!(f, "; format={format}")?;
        }
        if let Some(entity) = &self.entity_id {
            write!(f, "; entity={entity}")?;
        }
        if let Some(kind) = &self.io_kind {
            write!(f, "; io_kind={kind}")?;
        }
        if let Some(code) = self.os_code {
            write!(f, "; os_code={code}")?;
        }
        if let Some(code) = self.sqlite_code {
            write!(f, "; sqlite_code={code}")?;
        }
        if let Some(offset) = self.byte_offset {
            write!(f, "; byte_offset={offset}")?;
        }
        if let Some(kind) = self.error_kind {
            write!(f, "; error_kind={kind}")?;
        }
        if let Some(reason) = self.tls_reason {
            write!(f, "; tls_reason={reason}")?;
        }
        if let Some(line) = self.line {
            write!(f, "; line={line}")?;
        }
        if let Some(column) = self.column {
            write!(f, "; column={column}")?;
        }
        if let Some(timeout) = self.timeout_ms {
            write!(f, "; timeout_ms={timeout}")?;
        }
        if let Some(limit) = self.limit_bytes {
            write!(f, "; limit_bytes={limit}")?;
        }
        if let Some(index) = self.item_index {
            write!(f, "; item_index={index}")?;
        }
        if let Some(field) = self.field {
            write!(f, "; field={field}")?;
        }
        if let Some(code) = self.provider_code {
            write!(f, "; provider_code={code}")?;
        }
        if let Some(id) = &self.resource_id {
            write!(f, "; resource_id={id}")?;
        }
        write!(f, "; next: {}", self.next_step)?;
        if self.omitted_causes > 0 {
            write!(f, " omitted_causes={}", self.omitted_causes)?;
        }
        for cause in &self.causes {
            write!(f, "; cause=[{cause}]")?;
        }
        Ok(())
    }
}

impl std::error::Error for Failure {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn published_codes_are_unique_and_have_documented_resolution_guidance() {
        let docs = include_str!("../docs/src/source-errors.md");
        let mut codes = std::collections::HashSet::new();
        for &code in FailureCode::ALL {
            assert!(
                codes.insert(code.as_str()),
                "duplicate diagnostic code: {code:?}"
            );
            assert!(
                docs.contains(code.as_str()),
                "undocumented diagnostic code: {code:?}"
            );
            assert!(!code.next_step().is_empty());
        }
    }

    #[test]
    fn entity_context_does_not_echo_url_or_header_fragments() {
        for entity in [
            "sensor.temp?token=private",
            "https://private.example",
            "sensor.temp\nAuthorization: private",
        ] {
            let error = Failure::new(FailureCode::HaEntityState, "HA state").with_entity(entity);
            assert!(error.entity_id.is_none());
            assert!(!error.to_string().contains("private"));
        }
        let error =
            Failure::new(FailureCode::HaEntityState, "HA state").with_entity("sensor.temp_1");
        assert_eq!(error.entity_id.as_deref(), Some("sensor.temp_1"));
    }
}
