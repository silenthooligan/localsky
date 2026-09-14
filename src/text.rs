// Two small text rules the whole crate shares, written once and
// compiled for both targets: percent-encoding for the URLs LocalSky
// builds, and the slug a name becomes. Six encoders with two escaping
// rules and three slug derivations used to live in the wizard, the
// settings pages, a controller, two cloud adapters and the MQTT
// publisher; the wizard and the settings geocoder could encode the same
// query differently, and a zone slug (immutable once written) had no
// owner.

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~')
}

fn push_escaped(out: &mut String, b: u8) {
    out.push_str(&format!("%{b:02X}"));
}

/// Strict RFC 3986 encoding of one path segment: everything outside the
/// unreserved set is escaped, a space included.
pub fn path_segment(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if is_unreserved(b) {
            out.push(b as char);
        } else {
            push_escaped(&mut out, b);
        }
    }
    out
}

/// A query-string value (`?q=<value>`): the strict encoding, with a space
/// as `+` the way a form field is sent. Every geocoder call and every
/// cloud query parameter goes through this.
pub fn query_value(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b == b' ' {
            out.push('+');
        } else if is_unreserved(b) {
            out.push(b as char);
        } else {
            push_escaped(&mut out, b);
        }
    }
    out
}

/// An `application/x-www-form-urlencoded` body value (an OAuth token
/// request): the strict encoding, every byte outside the unreserved set
/// escaped, a space included. Named so a call site says which it is
/// building.
pub fn form_value(s: &str) -> String {
    path_segment(s)
}

/// The slug a zone, schedule, rule or MQTT node takes from its name:
/// lowercase ASCII alphanumerics, every run of anything else collapsed
/// to one underscore, no leading or trailing underscore. Empty when the
/// name has nothing usable; callers that need a fallback supply it.
pub fn slugify(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for c in name.trim().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c.to_ascii_lowercase());
        } else if !out.is_empty() && !out.ends_with('_') {
            out.push('_');
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodings() {
        assert_eq!(path_segment("back yard/1"), "back%20yard%2F1");
        assert_eq!(query_value("St. Augustine, FL"), "St.+Augustine%2C+FL");
        assert_eq!(form_value("a b&c=d"), "a%20b%26c%3Dd");
        assert_eq!(path_segment("safe-._~"), "safe-._~");
        assert_eq!(query_value("caf\u{e9}"), "caf%C3%A9");
    }

    #[test]
    fn slugs() {
        assert_eq!(slugify("Back Yard"), "back_yard");
        assert_eq!(slugify("  Front  (drip) zone!  "), "front_drip_zone");
        assert_eq!(slugify("Zone-3"), "zone_3");
        assert_eq!(slugify("___"), "");
        assert_eq!(slugify("Caf\u{e9} Lawn"), "caf_lawn");
    }
}
