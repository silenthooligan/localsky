//! Safe MQTT/WebSocket error classification. No packet bodies or URLs escape.
use crate::failure::{Failure, FailureCode as Code};

pub fn mqtt(error: &rumqttc::ConnectionError, operation: &'static str) -> Failure {
    use rumqttc::{ConnectionError as E, StateError as S};
    let mut failure = Failure::new(Code::MqttProtocol, operation);
    failure.error_kind = Some(match error {
        E::NetworkTimeout => {
            failure.code = Code::Timeout;
            "network_timeout"
        }
        E::FlushTimeout => {
            failure.code = Code::Timeout;
            "flush_timeout"
        }
        E::Io(_) => {
            failure.code = Code::Connection;
            "socket_io"
        }
        E::ConnectionRefused(code) => {
            failure.provider_code = Some(*code as i64);
            "connack_rejected"
        }
        E::NotConnAck(_) => "expected_connack",
        E::RequestsDone => "request_channel_closed",
        E::Tls(_) => {
            failure.code = Code::TlsProtocol;
            "mqtt_tls"
        }
        E::MqttState(state) => match state {
            S::Io(_) => "socket_io",
            S::InvalidState => "invalid_state",
            S::Unsolicited(_) => "unsolicited_ack",
            S::AwaitPingResp => "ping_unacknowledged",
            S::WrongPacket => "unexpected_packet",
            S::CollisionTimeout => "collision_timeout",
            S::EmptySubscription => "empty_subscription",
            S::Deserialization(_) => "packet_decode",
            S::ConnectionAborted => "peer_aborted",
        },
    });
    // Reconstruct after choosing a code so message and guidance agree with it.
    let mut result = Failure::new(failure.code, operation);
    result.error_kind = failure.error_kind;
    result.provider_code = failure.provider_code;
    super::source_failure::append_causes(&mut result, error);
    result
}

pub fn websocket(
    error: &tokio_tungstenite::tungstenite::Error,
    operation: &'static str,
) -> Failure {
    use tokio_tungstenite::tungstenite::{error::ProtocolError as P, Error as E};
    let (code, kind) = match error {
        E::ConnectionClosed => (Code::StreamClosed, "peer_closed"),
        E::AlreadyClosed => (Code::StreamClosed, "use_after_close"),
        E::Io(_) => (Code::Connection, "socket_io"),
        E::Tls(_) => (Code::TlsProtocol, "websocket_tls"),
        E::Capacity(_) => (Code::BodyTooLarge, "websocket_capacity"),
        E::WriteBufferFull(_) => (Code::StreamProtocol, "write_buffer_full"),
        E::Utf8(_) => (Code::Decode, "invalid_utf8"),
        E::AttackAttempt => (Code::StreamProtocol, "attack_attempt"),
        E::Url(_) => (Code::InvalidUrl, "websocket_url"),
        E::Http(response) => return Failure::http(response.status().as_u16(), None, operation),
        E::HttpFormat(_) => (Code::StreamProtocol, "http_handshake_format"),
        E::Protocol(protocol) => (
            Code::StreamProtocol,
            match protocol {
                P::WrongHttpMethod => "wrong_http_method",
                P::WrongHttpVersion => "wrong_http_version",
                P::MissingConnectionUpgradeHeader => "missing_connection_upgrade",
                P::MissingUpgradeWebSocketHeader => "missing_websocket_upgrade",
                P::MissingSecWebSocketVersionHeader => "missing_websocket_version",
                P::MissingSecWebSocketKey => "missing_websocket_key",
                P::InvalidSecWebSocketKey => "invalid_websocket_key",
                P::SecWebSocketAcceptKeyMismatch => "accept_key_mismatch",
                P::SecWebSocketSubProtocolError(_) => "subprotocol_mismatch",
                P::JunkAfterRequest => "junk_after_request",
                P::CustomResponseSuccessful => "invalid_success_response",
                P::InvalidHeader(_) => "invalid_handshake_header",
                P::HandshakeIncomplete => "handshake_incomplete",
                P::HttparseError(_) => "http_parse",
                P::SendAfterClosing => "send_after_close",
                P::ReceivedAfterClosing => "received_after_close",
                P::NonZeroReservedBits => "reserved_frame_bits",
                P::UnmaskedFrameFromClient => "unmasked_client_frame",
                P::MaskedFrameFromServer => "masked_server_frame",
                P::FragmentedControlFrame => "fragmented_control_frame",
                P::ControlFrameTooBig => "control_frame_too_big",
                P::UnknownControlFrameType(_) => "unknown_control_frame",
                P::UnknownDataFrameType(_) => "unknown_data_frame",
                P::UnexpectedContinueFrame => "unexpected_continuation",
                P::ExpectedFragment(_) => "expected_fragment",
                P::ResetWithoutClosingHandshake => "reset_without_close_handshake",
                P::InvalidOpcode(_) => "invalid_opcode",
                P::InvalidCloseSequence => "invalid_close_sequence",
            },
        ),
    };
    let mut failure = Failure::new(code, operation);
    failure.error_kind = Some(kind);
    super::source_failure::append_causes(&mut failure, error);
    failure
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn mqtt_retains_broker_rejection_and_socket_evidence() {
        let failure = mqtt(
            &rumqttc::ConnectionError::ConnectionRefused(
                rumqttc::ConnectReturnCode::BadUserNamePassword,
            ),
            "MQTT connect",
        );
        assert_eq!(failure.provider_code, Some(4));
        let failure = mqtt(
            &rumqttc::ConnectionError::Io(std::io::Error::from_raw_os_error(111)),
            "MQTT connect",
        );
        assert_eq!(failure.os_code, Some(111));
    }
    #[test]
    fn websocket_protocol_reports_handshake_detail() {
        let error = tokio_tungstenite::tungstenite::Error::Protocol(
            tokio_tungstenite::tungstenite::error::ProtocolError::MissingUpgradeWebSocketHeader,
        );
        assert_eq!(
            websocket(&error, "connect").error_kind,
            Some("missing_websocket_upgrade")
        );
    }
}

/// SUBACK failure is an authorization result, not a successful subscription.
pub fn mqtt_subscription(ack: &rumqttc::SubAck) -> Option<Failure> {
    ack.return_codes
        .iter()
        .position(|code| matches!(code, rumqttc::SubscribeReasonCode::Failure))
        .map(|index| {
            let mut failure = Failure::new(
                Code::MqttProtocol,
                "MQTT broker subscription acknowledgement",
            )
            .with_item(index)
            .with_provider_code(Some(128));
            failure.error_kind = Some("subscription_rejected");
            failure.resource_id = Some(format!("packet:{}", ack.pkid));
            failure
        })
}

#[cfg(test)]
mod subscription_tests {
    #[test]
    fn broker_denial_is_not_reported_as_a_healthy_subscription() {
        let ack = rumqttc::SubAck::new(42, vec![rumqttc::SubscribeReasonCode::Failure]);
        let failure = super::mqtt_subscription(&ack).unwrap();
        assert_eq!(failure.provider_code, Some(128));
        assert_eq!(failure.resource_id.as_deref(), Some("packet:42"));
        assert_eq!(failure.error_kind, Some("subscription_rejected"));
        assert!(super::mqtt_subscription(&rumqttc::SubAck::new(
            43,
            vec![rumqttc::SubscribeReasonCode::Success(
                rumqttc::QoS::AtMostOnce
            )]
        ))
        .is_none());
    }
}
