// What the LAN listener is doing, as plain data.
//
// Deliberately NOT in `listener`, which is ssr-only because it binds a
// socket. This type travels on `/api/health` and is rendered in the
// browser, so it has to compile for wasm.
//
// Why it exists: a Tempest station is read over the network, and only one
// program per machine can read it. That is an ordinary thing to run into
// and an awful thing to diagnose, because the symptom is an empty station
// panel and the cause was previously a single line in a container log.
//
// The message is split into a headline, one plain sentence, and a list of
// actions, rather than a paragraph. An operator hitting this is already
// confused, and a wall of text about broadcast semantics does not help
// them. The technical string is kept separate for the person who wants
// it.

use serde::{Deserialize, Serialize};

/// The LAN listener's current state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ListenerStatus {
    /// No enabled `tempest_udp` source, so nothing is bound.
    NotConfigured,
    /// Bound and receiving.
    Listening {
        bind_addr: String,
        /// The hub filter, when the operator set one.
        hub_serial: Option<String>,
    },
    /// The address is already taken by another program on this machine.
    AddressInUse { bind_addr: String, detail: String },
    /// Bound and then failed, or could not bind for another reason.
    Error { bind_addr: String, detail: String },
}

impl ListenerStatus {
    /// Short label. What a status chip shows.
    pub fn headline(&self) -> &'static str {
        match self {
            Self::NotConfigured => "Not reading a Tempest",
            Self::Listening { .. } => "Reading your Tempest",
            Self::AddressInUse { .. } => "Another app is using your Tempest",
            Self::Error { .. } => "Cannot read your Tempest",
        }
    }

    /// One sentence. What is true, in the operator's terms.
    pub fn detail(&self) -> String {
        match self {
            Self::NotConfigured => "No Tempest station is set up here.".to_string(),
            Self::Listening { hub_serial, .. } => {
                match hub_serial.as_deref().filter(|s| !s.is_empty()) {
                    Some(hub) => format!("Listening to hub {hub}."),
                    None => "Listening to any Tempest hub on your network.".to_string(),
                }
            }
            Self::AddressInUse { .. } => {
                "Home Assistant, or another app on this machine, is reading it already. \
                 Only one app per machine can. Your station is working normally."
                    .to_string()
            }
            Self::Error { detail, .. } => format!("The network connection failed: {detail}"),
        }
    }

    /// What the operator can do about it. Empty when nothing is wrong.
    ///
    /// Ordered by what most people should pick first.
    pub fn actions(&self) -> Vec<&'static str> {
        match self {
            Self::AddressInUse { .. } => vec![
                "Read it through Home Assistant instead. Remove the Tempest source here, \
                 then add a Home Assistant source.",
                "Or run LocalSky on a different machine. Two machines can read the same \
                 station at the same time.",
                "Or stop the other app from reading it.",
            ],
            Self::Error { .. } => vec![
                "Check that this machine is on the same network as your Tempest hub.",
                "LocalSky keeps retrying, so this clears on its own once the network is back.",
            ],
            _ => Vec::new(),
        }
    }

    /// The address and the underlying error, for whoever wants them.
    pub fn technical(&self) -> Option<String> {
        match self {
            Self::NotConfigured => None,
            Self::Listening { bind_addr, .. } => Some(bind_addr.clone()),
            Self::AddressInUse { bind_addr, detail } | Self::Error { bind_addr, detail } => {
                Some(format!("{bind_addr}: {detail}"))
            }
        }
    }

    /// True when the operator needs to do something.
    pub fn needs_attention(&self) -> bool {
        matches!(self, Self::AddressInUse { .. } | Self::Error { .. })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn in_use() -> ListenerStatus {
        ListenerStatus::AddressInUse {
            bind_addr: "0.0.0.0:50222".into(),
            detail: "Address already in use (os error 98)".into(),
        }
    }

    /// The message an operator actually reads has to be short and has to
    /// lead with the answer. This is the state someone hits while already
    /// confused about why their station panel is empty.
    #[test]
    fn the_conflict_message_is_short_and_leads_with_the_cause() {
        let s = in_use();
        assert!(s.needs_attention());
        assert_eq!(s.headline(), "Another app is using your Tempest");

        let d = s.detail();
        // Two sentences of plain language, and no jargon.
        assert!(d.len() < 220, "detail is {} chars: {d}", d.len());
        for jargon in [
            "port",
            "broadcast",
            "bind",
            "UDP",
            "50222",
            "namespace",
            "socket",
        ] {
            assert!(
                !d.contains(jargon),
                "the plain-language line must not say {jargon:?}: {d}"
            );
        }
        // It must correct the conclusion people jump to.
        assert!(d.contains("working normally"));
    }

    /// The fix is a list, not a sentence buried in a paragraph, and the
    /// option most people should take is first.
    #[test]
    fn the_actions_are_a_list_with_the_best_option_first() {
        let a = in_use().actions();
        assert_eq!(a.len(), 3);
        assert!(a[0].contains("through Home Assistant"));
        assert!(a[1].contains("different machine"));
        assert!(a[2].contains("stop the other app"));
        for step in &a {
            assert!(step.len() < 130, "action too long: {step}");
        }
    }

    /// The address belongs in a technical line, not in the sentence a
    /// non-technical operator reads.
    #[test]
    fn the_address_is_available_but_out_of_the_way() {
        let t = in_use().technical().expect("has technical detail");
        assert!(t.contains("0.0.0.0:50222"));
        assert!(t.contains("os error 98"));
    }

    #[test]
    fn a_quiet_state_is_not_an_alarm_and_offers_nothing_to_do() {
        for s in [
            ListenerStatus::NotConfigured,
            ListenerStatus::Listening {
                bind_addr: "0.0.0.0:50222".into(),
                hub_serial: None,
            },
        ] {
            assert!(!s.needs_attention());
            assert!(s.actions().is_empty());
        }
    }

    #[test]
    fn listening_says_what_it_is_listening_to() {
        assert!(ListenerStatus::Listening {
            bind_addr: "0.0.0.0:50222".into(),
            hub_serial: None,
        }
        .detail()
        .contains("any Tempest hub"));
        assert!(ListenerStatus::Listening {
            bind_addr: "0.0.0.0:50222".into(),
            hub_serial: Some("HB-00012345".into()),
        }
        .detail()
        .contains("HB-00012345"));
    }

    /// The wire shape is a tagged object, so a client switches on the
    /// state rather than parsing prose.
    #[test]
    fn the_state_is_machine_readable() {
        let json = serde_json::to_string(&ListenerStatus::NotConfigured).expect("serializes");
        assert_eq!(json, "{\"state\":\"not_configured\"}");
        let held = serde_json::to_string(&in_use()).expect("serializes");
        assert!(held.contains("\"state\":\"address_in_use\""));
        assert!(held.contains("\"bind_addr\":\"0.0.0.0:50222\""));
    }
}
