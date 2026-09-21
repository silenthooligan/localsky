//! Idempotent file-set installation, used only while its durable parent journal
//! says Applying. The Installed boundary must precede opening SQLite: database
//! migrations and normal writes may then change bytes without replaying a backup.

use super::{append, digest, exists, regular, sync_parent};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::io::{self, Write};
use std::path::{Path, PathBuf};

pub(super) enum Candidate {
    File(PathBuf),
    Bytes(Vec<u8>),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    target: PathBuf,
    before: Option<String>,
    after: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct FileSet {
    transaction: String,
    entries: Vec<Entry>,
}

// A recovery SQLite file must find its own WAL at <recovery-db>-wal.
fn before_path(target: &Path, transaction: &str) -> PathBuf {
    let text = target.as_os_str().to_string_lossy();
    for suffix in ["-wal", "-shm", "-journal"] {
        if let Some(base) = text.strip_suffix(suffix) {
            return append(
                Path::new(base),
                &format!(".pre-restore.{transaction}{suffix}"),
            );
        }
    }
    append(target, &format!(".pre-restore.{transaction}"))
}

fn after_path(target: &Path, transaction: &str) -> PathBuf {
    append(target, &format!(".restore-payload.{transaction}"))
}

fn hash_if_present(path: &Path) -> io::Result<Option<String>> {
    if exists(path)? {
        digest(path).map(Some)
    } else {
        Ok(None)
    }
}

fn create_copy(candidate: &Candidate, to: &Path) -> io::Result<String> {
    let expected = match candidate {
        Candidate::File(from) => digest(from)?,
        Candidate::Bytes(bytes) => hex::encode(Sha256::digest(bytes)),
    };
    if exists(to)? {
        if digest(to)? == expected {
            return Ok(expected);
        }
        return Err(super::invariant(
            "restore recovery copy already exists with different bytes",
        ));
    }
    // Publish only complete immutable copies. A crash during preparation leaves
    // a disposable partial temporary, never a half-written recovery original.
    let temporary = append(to, ".copying");
    if exists(&temporary)? {
        regular(&temporary)?;
        std::fs::remove_file(&temporary)?;
    }
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut output = options.open(&temporary)?;
    match candidate {
        Candidate::File(from) => {
            std::io::copy(&mut std::fs::File::open(from)?, &mut output)?;
        }
        Candidate::Bytes(bytes) => output.write_all(bytes)?,
    }
    output.sync_all()?;
    if digest(&temporary)? != expected {
        return Err(super::invariant(
            "restore source changed while being preserved",
        ));
    }
    // Atomic no-clobber publication on the same filesystem.
    std::fs::hard_link(&temporary, to)?;
    sync_parent(to)?;
    std::fs::remove_file(&temporary)?;
    sync_parent(to)?;
    Ok(expected)
}

#[cfg(test)]
std::thread_local! {
    static CRASH_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn interrupt_after(steps: usize) {
    CRASH_AFTER.set(Some(steps));
}

#[cfg(test)]
fn interruption_point() -> io::Result<()> {
    match CRASH_AFTER.get() {
        Some(0) => {
            CRASH_AFTER.set(None);
            Err(super::invariant("simulated process interruption"))
        }
        Some(n) => {
            CRASH_AFTER.set(Some(n - 1));
            Ok(())
        }
        None => Ok(()),
    }
}

impl FileSet {
    /// Copies become immutable recovery evidence before the caller publishes
    /// its journal. A failure here has made no changes to any live target.
    pub(super) fn prepare(
        targets: Vec<(PathBuf, Option<Candidate>)>,
        transaction: &str,
    ) -> io::Result<Self> {
        let mut entries = Vec::new();
        for (target, candidate) in targets {
            let before = hash_if_present(&target)?;
            if before.is_some() {
                let copied = create_copy(
                    &Candidate::File(target.clone()),
                    &before_path(&target, transaction),
                )?;
                if before.as_ref() != Some(&copied) {
                    return Err(super::invariant(
                        "restore target changed during preparation",
                    ));
                }
            } else if exists(&before_path(&target, transaction))? {
                return Err(super::invariant("unexpected prior restore recovery file"));
            }
            let after = candidate
                .as_ref()
                .map(|c| create_copy(c, &after_path(&target, transaction)))
                .transpose()?;
            if candidate.is_none() && exists(&after_path(&target, transaction))? {
                return Err(super::invariant("unexpected restore payload file"));
            }
            entries.push(Entry {
                target,
                before,
                after,
            });
        }
        Ok(Self {
            transaction: transaction.to_string(),
            entries,
        })
    }

    pub(super) fn validate(&self, targets: &[PathBuf], transaction: &str) -> io::Result<()> {
        if self.transaction != transaction
            || self.entries.len() != targets.len()
            || self
                .entries
                .iter()
                .zip(targets)
                .any(|(e, p)| &e.target != p)
        {
            return Err(super::invariant(
                "restore journal has mismatched paths or transaction",
            ));
        }
        Ok(())
    }

    pub(super) fn verify_installed_presence(&self) -> io::Result<()> {
        for entry in &self.entries {
            if entry.after.is_some() || exists(&entry.target)? {
                regular(&entry.target)?;
            }
        }
        Ok(())
    }

    /// Preflight the entire set before modifying any target. The only accepted
    /// intermediate live contents are its recorded before and after states.
    pub(super) fn preflight(&self) -> io::Result<()> {
        for entry in &self.entries {
            if hash_if_present(&before_path(&entry.target, &self.transaction))? != entry.before
                || hash_if_present(&after_path(&entry.target, &self.transaction))? != entry.after
            {
                return Err(super::invariant("restore recovery copy digest mismatch"));
            }
            let actual = hash_if_present(&entry.target)?;
            if actual != entry.before && actual != entry.after {
                return Err(super::invariant("unexpected bytes at restore target"));
            }
        }
        Ok(())
    }

    pub(super) fn install(&self) -> io::Result<()> {
        self.preflight()?;
        #[cfg(test)]
        interruption_point()?;
        for index in 0..self.entries.len() {
            self.install_entry(index)?;
            #[cfg(test)]
            interruption_point()?;
        }
        Ok(())
    }

    pub(super) fn install_entry(&self, index: usize) -> io::Result<()> {
        let entry = &self.entries[index];
        if hash_if_present(&entry.target)? == entry.after {
            return Ok(());
        }
        if entry.after.is_some() {
            let temporary = append(
                &entry.target,
                &format!(".restore-install.{}", self.transaction),
            );
            if exists(&temporary)? {
                regular(&temporary)?;
                std::fs::remove_file(&temporary)?;
            }
            create_copy(
                &Candidate::File(after_path(&entry.target, &self.transaction)),
                &temporary,
            )?;
            std::fs::rename(&temporary, &entry.target)?;
        } else if exists(&entry.target)? {
            regular(&entry.target)?;
            std::fs::remove_file(&entry.target)?;
        }
        sync_parent(&entry.target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn directory() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "localsky-restore-files-{}-{:032x}",
            std::process::id(),
            rand::random::<u128>()
        ));
        std::fs::create_dir(&path).unwrap();
        path
    }
    #[test]
    fn preparation_resumes_complete_copies_and_replaces_only_owned_partial_temporaries() {
        let directory = directory();
        let target = directory.join("config");
        std::fs::write(&target, b"before").unwrap();
        let recovery = before_path(&target, "resume");
        std::fs::write(append(&recovery, ".copying"), b"interrupted").unwrap();
        let make = || {
            FileSet::prepare(
                vec![(target.clone(), Some(Candidate::Bytes(b"after".to_vec())))],
                "resume",
            )
        };
        make().unwrap();
        let set = make().unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"before");
        set.install().unwrap();
        set.install().unwrap();
        assert_eq!(std::fs::read(&recovery).unwrap(), b"before");
        assert_eq!(std::fs::read(&target).unwrap(), b"after");
    }

    #[test]
    fn a_changed_later_target_or_payload_cannot_partially_install_earlier_files() {
        for change_payload in [false, true] {
            let directory = directory();
            let a = directory.join("config");
            let b = directory.join("ledger");
            std::fs::write(&a, b"old-config").unwrap();
            std::fs::write(&b, b"old-ledger").unwrap();
            let set = FileSet::prepare(
                vec![
                    (a.clone(), Some(Candidate::Bytes(b"new-config".to_vec()))),
                    (b.clone(), Some(Candidate::Bytes(b"new-ledger".to_vec()))),
                ],
                "tamper",
            )
            .unwrap();
            let changed = if change_payload {
                after_path(&b, "tamper")
            } else {
                b.clone()
            };
            std::fs::write(&changed, b"unknown").unwrap();
            assert!(set.install().is_err());
            assert_eq!(std::fs::read(&a).unwrap(), b"old-config");
            assert_eq!(std::fs::read(&changed).unwrap(), b"unknown");
        }
    }
}
