// Stable per-install instance id. Generated once into /data/instance-id
// (alongside the SQLite DB, NOT in the TOML config: restoring a config
// backup onto new hardware should keep the new box's identity). Used as
// the mDNS TXT uuid and the HACS unique_id so re-pairing after an IP
// change dedupes correctly.

use std::path::Path;
use std::sync::OnceLock;

static INSTANCE_ID: OnceLock<String> = OnceLock::new();

/// Load or create the instance id at `<data_dir>/instance-id`. Call once
/// at boot before anything reads `get()`. Idempotent.
pub fn init(data_dir: &Path) {
    let path = data_dir.join("instance-id");
    let id = match std::fs::read_to_string(&path) {
        Ok(s) if !s.trim().is_empty() => s.trim().to_string(),
        _ => {
            let fresh = generate();
            if let Err(e) =
                std::fs::create_dir_all(data_dir).and_then(|()| std::fs::write(&path, &fresh))
            {
                tracing::warn!(error = %e, path = %path.display(),
                    "could not persist instance-id; using ephemeral id this boot");
            }
            fresh
        }
    };
    let _ = INSTANCE_ID.set(id);
}

/// The instance id, if init ran. UUID-shaped, lowercase.
pub fn get() -> Option<&'static str> {
    INSTANCE_ID.get().map(|s| s.as_str())
}

fn generate() -> String {
    use rand::Rng;
    let mut b = [0u8; 16];
    rand::rng().fill_bytes(&mut b);
    // RFC 4122 v4 shape.
    b[6] = (b[6] & 0x0f) | 0x40;
    b[8] = (b[8] & 0x3f) | 0x80;
    format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_id_is_uuid_shaped() {
        let id = generate();
        assert_eq!(id.len(), 36);
        assert_eq!(id.matches('-').count(), 4);
        let ver = id.as_bytes()[14] as char;
        assert_eq!(ver, '4');
    }
}
