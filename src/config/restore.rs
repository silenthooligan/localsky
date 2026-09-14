//! Verified, restartable restore publication and activation.
//!
//! Immutable before/after copies support deterministic completion after a
//! crash. Unknown files still hold startup; only journaled states are resumed.

mod files;
use files::{Candidate, FileSet};

use std::io::{self, Read};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::store::{staged_path, write_atomic_durable};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum Phase {
    Publishing,
    Ready,
    Applying,
    Activated,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Part {
    live: PathBuf,
    /// None explicitly means this request publishes no stage for this slot.
    sha256: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Marker {
    version: u32,
    transaction: String,
    phase: Phase,
    /// Fixed order: config, ledger, database. The database is always present.
    parts: [Part; 3],
    #[serde(default)]
    files: Option<FileSet>,
}

pub(crate) fn marker_path(db_path: &Path) -> PathBuf {
    append(db_path, ".restore-state.json")
}

fn hot_marker_path(config: &Path) -> PathBuf {
    append(config, ".restore-hot-apply.pending")
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct HotJournal {
    version: u32,
    transaction: String,
    phase: Phase,
    files: FileSet,
}

/// The same verified file-set protocol for a hot config/ledger pair. The API
/// owns the config writer lock until disk and runtime application finish.
pub(crate) struct HotRestore {
    path: PathBuf,
    journal: HotJournal,
}

impl HotRestore {
    pub(crate) fn begin(
        config: &Path,
        db: &Path,
        config_text: String,
        ledger_text: Option<String>,
    ) -> io::Result<Self> {
        let expected = paths(config, db)?;
        let path = hot_marker_path(&expected[0]);
        if exists(&path)? || exists(&marker_path(&expected[2]))? {
            return Err(recovery_error(&path, "an earlier restore is pending"));
        }
        let transaction = format!("{:032x}", rand::random::<u128>());
        let journal = HotJournal {
            version: 2,
            transaction: transaction.clone(),
            phase: Phase::Applying,
            files: FileSet::prepare(
                vec![
                    (
                        expected[0].clone(),
                        Some(Candidate::Bytes(config_text.into_bytes())),
                    ),
                    (
                        expected[1].clone(),
                        ledger_text.map(|s| Candidate::Bytes(s.into_bytes())),
                    ),
                ],
                &transaction,
            )?,
        };
        save_hot_marker(&path, &journal)?;
        Ok(Self { path, journal })
    }

    pub(crate) fn install(&mut self) -> io::Result<()> {
        self.journal.files.install()?;
        self.journal.phase = Phase::Activated;
        save_hot_marker(&self.path, &self.journal)
    }

    pub(crate) fn complete(self) -> io::Result<()> {
        if self.journal.phase != Phase::Activated {
            return Err(recovery_error(
                &self.path,
                "config pair was not fully installed",
            ));
        }
        ActivatedRestore { marker: self.path }.complete()
    }
}

fn save_hot_marker(path: &Path, journal: &HotJournal) -> io::Result<()> {
    write_atomic_durable(
        path,
        &serde_json::to_vec(journal).map_err(io::Error::other)?,
    )?;
    sync_parent(path)
}

fn recover_hot(expected: &[PathBuf; 3], path: &Path) -> io::Result<ActivatedRestore> {
    regular(path)?;
    let mut journal: HotJournal = serde_json::from_slice(&std::fs::read(path)?).map_err(|_| {
        io::Error::other("legacy or unreadable hot restore has no recovery journal")
    })?;
    if journal.version != 2 || !valid_transaction(&journal.transaction) {
        return Err(io::Error::other("unsupported hot restore journal"));
    }
    journal
        .files
        .validate(&expected[..2], &journal.transaction)?;
    match journal.phase {
        Phase::Applying => {
            journal.files.install()?;
            journal.phase = Phase::Activated;
            save_hot_marker(path, &journal)?;
        }
        Phase::Activated => {}
        _ => return Err(io::Error::other("unsupported hot restore phase")),
    }
    journal.files.verify_installed_presence()?;
    super::loader::load_from_path(&expected[0]).map_err(io::Error::other)?;
    if exists(&expected[1])? {
        toml::from_str::<super::ledger::Ledger>(&std::fs::read_to_string(&expected[1])?)
            .map_err(io::Error::other)?;
    }
    Ok(ActivatedRestore {
        marker: path.to_path_buf(),
    })
}

fn append(path: &Path, suffix: &str) -> PathBuf {
    let mut value = path.as_os_str().to_os_string();
    value.push(suffix);
    PathBuf::from(value)
}

fn paths(config: &Path, db: &Path) -> io::Result<[PathBuf; 3]> {
    let config = std::path::absolute(config)?;
    let ledger = super::ledger::Ledger::path_for(&config);
    let db = std::path::absolute(db)?;
    if config == db || ledger == db {
        return Err(io::Error::other(
            "restore config, ledger and database paths must be distinct",
        ));
    }
    Ok([config, ledger, db])
}

fn exists(path: &Path) -> io::Result<bool> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e),
    }
}

fn regular(path: &Path) -> io::Result<()> {
    if !std::fs::symlink_metadata(path)?.is_file() {
        return Err(io::Error::other(format!(
            "not a regular restore file: {}",
            path.display()
        )));
    }
    Ok(())
}

fn digest(path: &Path) -> io::Result<String> {
    regular(path)?;
    let mut file = std::fs::File::open(path)?;
    let mut hash = Sha256::new();
    let mut bytes = [0u8; 64 * 1024];
    loop {
        let n = file.read(&mut bytes)?;
        if n == 0 {
            break;
        }
        hash.update(&bytes[..n]);
    }
    Ok(hex::encode(hash.finalize()))
}

/// Linux is the deployment target. Directory fsync failure there must be an
/// error: Ready must not outlive a stage rename lost to a power failure.
fn sync_parent(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    std::fs::File::open(path.parent().unwrap_or_else(|| Path::new(".")))?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn save_marker(path: &Path, marker: &Marker) -> io::Result<()> {
    let bytes = serde_json::to_vec(marker).map_err(io::Error::other)?;
    write_atomic_durable(path, &bytes)?;
    sync_parent(path)
}

fn read_marker(path: &Path) -> io::Result<Option<Marker>> {
    if !exists(path)? {
        return Ok(None);
    }
    regular(path)?;
    let bytes = std::fs::read(path)?;
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(io::Error::other)
}

fn valid_transaction(transaction: &str) -> bool {
    !transaction.is_empty()
        && transaction.len() <= 100
        && transaction
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
}

fn validate_marker(marker: &Marker, expected: &[PathBuf; 3]) -> io::Result<()> {
    if !matches!(marker.version, 1 | 2)
        || !valid_transaction(&marker.transaction)
        || marker
            .parts
            .iter()
            .zip(expected)
            .any(|(part, path)| &part.live != path)
        || marker.parts[2].sha256.is_none()
        || (marker.parts[1].sha256.is_some() && marker.parts[0].sha256.is_none())
    {
        return Err(io::Error::other(
            "restore marker has unsupported or mismatched paths/parts",
        ));
    }
    Ok(())
}

fn verify_stages(marker: &Marker) -> io::Result<()> {
    for part in &marker.parts {
        let stage = staged_path(&part.live);
        match &part.sha256 {
            Some(expected) if digest(&stage)? != *expected => {
                return Err(io::Error::other(format!(
                    "restore digest mismatch: {}",
                    stage.display()
                )));
            }
            None if exists(&stage)? => {
                return Err(io::Error::other(format!(
                    "unexpected restore stage: {}",
                    stage.display()
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

/// Owns the prior Ready marker so a fully rolled-back ordinary error can
/// preserve the previously accepted request. Otherwise Publishing remains.
pub(crate) struct Publication {
    path: PathBuf,
    marker: Marker,
}

impl Publication {
    pub(crate) fn begin(
        config: &Path,
        db: &Path,
        candidates: &[Option<PathBuf>; 3],
        transaction: String,
    ) -> io::Result<Self> {
        if !valid_transaction(&transaction) {
            return Err(io::Error::other("invalid restore transaction"));
        }
        let expected = paths(config, db)?;
        let path = marker_path(&expected[2]);
        if exists(&hot_marker_path(&expected[0]))? {
            return Err(recovery_error(&path, "a configuration restore is pending"));
        }
        let previous = read_marker(&path)?;
        if let Some(previous) = &previous {
            validate_marker(previous, &expected)?;
            if previous.phase != Phase::Ready {
                return Err(recovery_error(&path, "an earlier restore is incomplete"));
            }
            verify_stages(previous)?;
        }
        let hashes: Vec<_> = candidates
            .iter()
            .map(|p| p.as_deref().map(digest).transpose())
            .collect::<io::Result<_>>()?;
        let marker = Marker {
            version: 2,
            transaction: transaction.clone(),
            phase: Phase::Publishing,
            parts: std::array::from_fn(|n| Part {
                live: expected[n].clone(),
                sha256: hashes[n].clone(),
            }),
            files: Some(FileSet::prepare(
                expected
                    .iter()
                    .zip(candidates)
                    .map(|(live, candidate)| {
                        (staged_path(live), candidate.clone().map(Candidate::File))
                    })
                    .collect(),
                &transaction,
            )?),
        };
        validate_marker(&marker, &expected)?;
        save_marker(&path, &marker)?;
        Ok(Self { path, marker })
    }

    pub(crate) fn finish(mut self) -> io::Result<()> {
        resume_publication(&self.path, &mut self.marker)?;
        verify_stages(&self.marker)?;
        for part in &self.marker.parts {
            let stage = staged_path(&part.live);
            if part.sha256.is_some() {
                std::fs::File::open(&stage)?.sync_all()?;
            }
            sync_parent(&stage)?;
        }
        self.marker.phase = Phase::Ready;
        self.marker.files = None;
        save_marker(&self.path, &self.marker)
    }
}

fn recovery_error(marker: &Path, detail: impl std::fmt::Display) -> io::Error {
    io::Error::other(format!(
        "LocalSky startup refused: {detail}. Restore state: {}. Keep this marker, all .restore stages and .pre-restore recovery files; recover a complete verified bundle before restarting. No controllers or schedulers were started.",
        marker.display()
    ))
}

/// Kept until the restored database opens and the config loads successfully.
/// Dropping the receipt retains Activated, so boot validates the installed set
/// without reinstalling the database or discarding writes made after activation.
pub struct ActivatedRestore {
    marker: PathBuf,
}

impl ActivatedRestore {
    pub fn complete(&self) -> io::Result<()> {
        std::fs::remove_file(&self.marker)
            .and_then(|()| sync_parent(&self.marker))
            .map_err(|e| recovery_error(&self.marker, e))
    }
}

/// Called once before opening the history database or loading the live config.
/// No marker and no stages is the normal fresh/migrated-install path. Unmarked
/// legacy stages require explicit recovery, since their bundle identity is lost.
pub fn activate_at_boot(config: &Path, db: &Path) -> io::Result<Option<ActivatedRestore>> {
    let expected = paths(config, db)?;
    let hot_marker = hot_marker_path(&expected[0]);
    if exists(&hot_marker)? {
        if exists(&marker_path(&expected[2]))? {
            return Err(recovery_error(&hot_marker, "conflicting restore journals"));
        }
        return recover_hot(&expected, &hot_marker)
            .map(Some)
            .map_err(|e| recovery_error(&hot_marker, e));
    }
    let path = marker_path(&expected[2]);
    activate(&expected, &path).map_err(|e| recovery_error(&path, e))
}

fn resume_publication(path: &Path, marker: &mut Marker) -> io::Result<()> {
    let targets: Vec<_> = marker.parts.iter().map(|p| staged_path(&p.live)).collect();
    let files = marker
        .files
        .as_ref()
        .filter(|_| marker.version == 2)
        .ok_or_else(|| io::Error::other("legacy Publishing restore has no recovery journal"))?;
    files.validate(&targets, &marker.transaction)?;
    files.install()?;
    verify_stages(marker)?;
    marker.phase = Phase::Ready;
    marker.files = None;
    save_marker(path, marker)
}

fn activation_targets(marker: &Marker) -> Vec<PathBuf> {
    let mut targets: Vec<_> = marker.parts[..2]
        .iter()
        .filter(|p| p.sha256.is_some())
        .map(|p| p.live.clone())
        .collect();
    // Remove old journals before publishing the replacement DB. Their preserved
    // copies sit beside the old DB, where SQLite recovery can actually use them.
    targets.extend(["-wal", "-shm", "-journal"].map(|ext| append(&marker.parts[2].live, ext)));
    targets.push(marker.parts[2].live.clone());
    targets
}

fn preflight_schema(marker: &Marker, staged: bool) -> io::Result<()> {
    let config = if staged && marker.parts[0].sha256.is_some() {
        staged_path(&marker.parts[0].live)
    } else {
        marker.parts[0].live.clone()
    };
    if exists(&config)? {
        super::loader::load_from_path(&config).map_err(io::Error::other)?;
    }
    let ledger = if staged && marker.parts[1].sha256.is_some() {
        staged_path(&marker.parts[1].live)
    } else {
        marker.parts[1].live.clone()
    };
    if exists(&ledger)? {
        toml::from_str::<super::ledger::Ledger>(&std::fs::read_to_string(ledger)?)
            .map_err(io::Error::other)?;
    }
    let db = if staged {
        staged_path(&marker.parts[2].live)
    } else {
        marker.parts[2].live.clone()
    };
    regular(&db)?;
    crate::persistence::restore_probe::probe_localsky_db(
        db.to_str()
            .ok_or_else(|| io::Error::other("database path is not UTF-8"))?,
    )
    .map_err(io::Error::other)?;
    Ok(())
}

fn remove_consumed_stages(marker: &Marker) -> io::Result<()> {
    // Verify all remaining slots before removing any. Missing means an earlier
    // recovery pass already consumed it.
    for part in &marker.parts {
        let stage = staged_path(&part.live);
        if exists(&stage)? && Some(digest(&stage)?) != part.sha256 {
            return Err(io::Error::other(
                "unexpected restore stage during completion",
            ));
        }
    }
    for part in &marker.parts {
        let stage = staged_path(&part.live);
        if exists(&stage)? {
            std::fs::remove_file(&stage)?;
            sync_parent(&stage)?;
        }
    }
    Ok(())
}

fn activate(expected: &[PathBuf; 3], path: &Path) -> io::Result<Option<ActivatedRestore>> {
    let Some(mut marker) = read_marker(path)? else {
        for live in expected {
            let stage = staged_path(live);
            if exists(&stage)? {
                return Err(io::Error::other(format!(
                    "unmarked legacy restore stage: {}",
                    stage.display()
                )));
            }
        }
        return Ok(None);
    };
    validate_marker(&marker, expected)?;
    if marker.phase == Phase::Publishing {
        resume_publication(path, &mut marker)?;
    }
    if marker.phase == Phase::Ready {
        verify_stages(&marker)?;
        preflight_schema(&marker, true)?;
        let targets = activation_targets(&marker);
        let replacements = targets
            .into_iter()
            .map(|target| {
                let candidate = if marker.parts.iter().any(|p| p.live == target) {
                    Some(Candidate::File(staged_path(&target)))
                } else {
                    None
                };
                (target, candidate)
            })
            .collect();
        let files = FileSet::prepare(replacements, &marker.transaction)?;
        marker.version = 2;
        marker.files = Some(files);
        marker.phase = Phase::Applying;
        save_marker(path, &marker)?;
    }
    if marker.phase == Phase::Applying {
        let files = marker
            .files
            .as_ref()
            .filter(|_| marker.version == 2)
            .ok_or_else(|| io::Error::other("legacy Applying restore has no recovery journal"))?;
        files.validate(&activation_targets(&marker), &marker.transaction)?;
        files.install()?;
        remove_consumed_stages(&marker)?;
        // Commit the complete file set BEFORE SQLite opens/migrates it. After
        // this boundary a retry validates live state and never replays a DB.
        marker.phase = Phase::Activated;
        save_marker(path, &marker)?;
    }
    if marker.phase != Phase::Activated || marker.version != 2 {
        return Err(io::Error::other("unsupported restore recovery phase"));
    }
    let files = marker
        .files
        .as_ref()
        .ok_or_else(|| io::Error::other("missing activation journal"))?;
    files.validate(&activation_targets(&marker), &marker.transaction)?;
    for part in &marker.parts {
        if part.sha256.is_some() {
            regular(&part.live)?;
        }
    }
    preflight_schema(&marker, false)?;
    Ok(Some(ActivatedRestore {
        marker: path.to_path_buf(),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(tag: &str) -> [PathBuf; 3] {
        let dir =
            std::env::temp_dir().join(format!("localsky-restore-{}-{tag}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let paths = paths(&dir.join("localsky.toml"), &dir.join("irrigation.db")).unwrap();
        let mut cfg = super::super::schema::Config::default();
        cfg.deployment.location.lat = 30.07;
        cfg.deployment.location.lon = -81.47;
        cfg.deployment.display_name = "Before restore".into();
        std::fs::write(&paths[0], toml::to_string_pretty(&cfg).unwrap()).unwrap();
        std::fs::write(&paths[1], "seeded_source_ids = ['before']\n").unwrap();
        make_db(&paths[2], "before");
        paths
    }

    fn make_db(path: &Path, label: &str) {
        let mut db = rusqlite::Connection::open(path).unwrap();
        crate::persistence::run_migrations(&mut db).unwrap();
        db.execute_batch("CREATE TABLE restore_test (label TEXT NOT NULL)")
            .unwrap();
        db.execute("INSERT INTO restore_test (label) VALUES (?1)", [label])
            .unwrap();
    }

    fn candidates(paths: &[PathBuf; 3], with_config: bool) -> [Option<PathBuf>; 3] {
        let out = std::array::from_fn(|n| {
            (n == 2 || with_config).then(|| append(&paths[n], ".candidate"))
        });
        if with_config {
            let mut cfg = super::super::loader::load_from_path(&paths[0]).unwrap();
            cfg.deployment.display_name = "After restore".into();
            std::fs::write(
                out[0].as_ref().unwrap(),
                toml::to_string_pretty(&cfg).unwrap(),
            )
            .unwrap();
            std::fs::write(out[1].as_ref().unwrap(), "seeded_source_ids = ['after']\n").unwrap();
        }
        make_db(out[2].as_ref().unwrap(), "after");
        out
    }

    fn ready(paths: &[PathBuf; 3], with_config: bool, token: &str) {
        let candidates = candidates(paths, with_config);
        let publication =
            Publication::begin(&paths[0], &paths[2], &candidates, token.into()).unwrap();
        for (live, candidate) in paths.iter().zip(candidates) {
            if let Some(candidate) = candidate {
                std::fs::rename(candidate, staged_path(live)).unwrap();
            }
        }
        publication.finish().unwrap();
    }

    fn rejected(paths: &[PathBuf; 3]) -> String {
        match activate_at_boot(&paths[0], &paths[2]) {
            Ok(_) => panic!("an incomplete restore must not permit startup"),
            Err(error) => error.to_string(),
        }
    }

    #[test]
    fn interrupted_hot_restore_resumes_each_write_and_preserves_the_database() {
        for point in 0..=2 {
            let paths = fixture(&format!("hot-resume-{point}"));
            let before_db = std::fs::read(&paths[2]).unwrap();
            let mut cfg = super::super::loader::load_from_path(&paths[0]).unwrap();
            cfg.engine.skip_rules.max_wind_mph = 27.0;
            let mut hot = HotRestore::begin(
                &paths[0],
                &paths[2],
                toml::to_string_pretty(&cfg).unwrap(),
                Some("seeded_source_ids = ['completed']\n".into()),
            )
            .unwrap();
            files::interrupt_after(point);
            assert!(hot.install().is_err());
            drop(hot);
            let recovered = activate_at_boot(&paths[0], &paths[2]).unwrap().unwrap();
            assert_eq!(
                super::super::loader::load_from_path(&paths[0])
                    .unwrap()
                    .engine
                    .skip_rules
                    .max_wind_mph,
                27.0
            );
            assert!(std::fs::read_to_string(&paths[1])
                .unwrap()
                .contains("completed"));
            assert_eq!(std::fs::read(&paths[2]).unwrap(), before_db);
            recovered.complete().unwrap();
            assert!(activate_at_boot(&paths[0], &paths[2]).unwrap().is_none());
        }
    }

    #[test]
    fn ordinary_boot_is_allowed_but_unmarked_legacy_stages_are_not() {
        let paths = fixture("legacy-stage");
        assert!(activate_at_boot(&paths[0], &paths[2]).unwrap().is_none());
        let before = std::fs::read(&paths[0]).unwrap();
        std::fs::write(staged_path(&paths[0]), b"uncoordinated older restore").unwrap();
        assert!(rejected(&paths).contains("unmarked legacy restore stage"));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), before);
    }

    #[test]
    fn interrupted_publication_resumes_each_slot_before_activating() {
        for point in 0..=3 {
            let paths = fixture(&format!("publish-resume-{point}"));
            let candidate = candidates(&paths, true);
            let publication =
                Publication::begin(&paths[0], &paths[2], &candidate, format!("publish-{point}"))
                    .unwrap();
            files::interrupt_after(point);
            assert!(publication.finish().is_err());
            let receipt = activate_at_boot(&paths[0], &paths[2]).unwrap().unwrap();
            assert_eq!(
                super::super::loader::load_from_path(&paths[0])
                    .unwrap()
                    .deployment
                    .display_name,
                "After restore"
            );
            receipt.complete().unwrap();
            assert!(activate_at_boot(&paths[0], &paths[2]).unwrap().is_none());
        }
    }

    #[test]
    fn interrupted_activation_resumes_each_file_and_never_replays_an_opened_database() {
        for point in 0..=6 {
            let paths = fixture(&format!("activate-resume-{point}"));
            ready(&paths, true, &format!("activate-{point}"));
            files::interrupt_after(point);
            assert!(activate_at_boot(&paths[0], &paths[2]).is_err());
            let receipt = activate_at_boot(&paths[0], &paths[2]).unwrap().unwrap();
            // Simulate a crash after SQLite opened but before boot acknowledged
            // completion. Retrying must keep these new rows, not reinstall DB.
            {
                let db = rusqlite::Connection::open(&paths[2]).unwrap();
                db.execute(
                    "INSERT INTO restore_test(label) VALUES ('after-activation')",
                    [],
                )
                .unwrap();
            }
            let retry = activate_at_boot(&paths[0], &paths[2]).unwrap().unwrap();
            let db = rusqlite::Connection::open(&paths[2]).unwrap();
            let count: i64 = db
                .query_row(
                    "SELECT count(*) FROM restore_test WHERE label='after-activation'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(count, 1);
            drop(db);
            drop(receipt);
            retry.complete().unwrap();
            assert!(activate_at_boot(&paths[0], &paths[2]).unwrap().is_none());
        }
    }

    #[test]
    fn preflight_rejects_changed_bytes_and_unexpected_optional_parts_before_activation() {
        let paths = fixture("digest-mismatch");
        ready(&paths, true, "digest-1");
        let before = std::fs::read(&paths[0]).unwrap();
        std::fs::write(staged_path(&paths[2]), b"different database bytes").unwrap();
        assert!(rejected(&paths).contains("digest mismatch"));
        assert_eq!(std::fs::read(&paths[0]).unwrap(), before);

        let db_only = fixture("unexpected-part");
        ready(&db_only, false, "absent-1");
        std::fs::write(staged_path(&db_only[0]), b"leftover earlier config").unwrap();
        assert!(rejected(&db_only).contains("unexpected restore stage"));
    }

    #[test]
    fn complete_restore_preserves_recovery_files_and_requires_successful_boot_completion() {
        let paths = fixture("complete-activation");
        let old: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        ready(&paths, true, "complete-1");
        // Old journals are retained beside the OLD DB, never the new one.
        std::fs::write(append(&paths[2], "-wal"), b"previous WAL").unwrap();
        std::fs::write(append(&paths[2], "-shm"), b"previous SHM").unwrap();
        std::fs::write(append(&paths[2], "-journal"), b"previous rollback journal").unwrap();
        let activated = activate_at_boot(&paths[0], &paths[2]).unwrap().unwrap();
        assert_eq!(
            super::super::loader::load_from_path(&paths[0])
                .unwrap()
                .deployment
                .display_name,
            "After restore"
        );
        for (path, bytes) in paths.iter().zip(old) {
            assert_eq!(
                std::fs::read(append(path, ".pre-restore.complete-1")).unwrap(),
                bytes
            );
            assert!(!staged_path(path).exists());
        }
        assert_eq!(
            std::fs::read(append(&paths[2], ".pre-restore.complete-1-wal")).unwrap(),
            b"previous WAL"
        );
        assert!(!append(&paths[2], "-wal").exists());
        assert!(!append(&paths[2], "-shm").exists());
        assert_eq!(
            std::fs::read(append(&paths[2], ".pre-restore.complete-1-journal")).unwrap(),
            b"previous rollback journal"
        );
        assert!(!append(&paths[2], "-journal").exists());
        // The installed boundary survives a second boot without replay.
        assert!(activate_at_boot(&paths[0], &paths[2]).unwrap().is_some());
        activated.complete().unwrap();
        assert!(activate_at_boot(&paths[0], &paths[2]).unwrap().is_none());
    }

    #[test]
    fn an_unrecognized_recovery_file_holds_before_any_live_file_changes() {
        let paths = fixture("recovery-collision");
        let before: Vec<_> = paths.iter().map(|p| std::fs::read(p).unwrap()).collect();
        ready(&paths, true, "collision-1");
        let collision = append(&paths[1], ".pre-restore.collision-1");
        std::fs::write(&collision, b"older recovery evidence").unwrap();
        for _ in 0..2 {
            assert!(rejected(&paths).contains("different bytes"));
            for (path, bytes) in paths.iter().zip(&before) {
                assert_eq!(&std::fs::read(path).unwrap(), bytes);
            }
        }
        assert_eq!(
            std::fs::read(&collision).unwrap(),
            b"older recovery evidence"
        );
    }

    #[test]
    fn a_replacement_db_only_restore_does_not_inherit_earlier_config() {
        let paths = fixture("replacement-db-only");
        let old_config = std::fs::read(&paths[0]).unwrap();
        ready(&paths, true, "earlier-1");
        let candidate = candidates(&paths, false);
        let publication =
            Publication::begin(&paths[0], &paths[2], &candidate, "later-2".into()).unwrap();
        files::interrupt_after(1);
        assert!(publication.finish().is_err());
        activate_at_boot(&paths[0], &paths[2])
            .unwrap()
            .unwrap()
            .complete()
            .unwrap();
        assert_eq!(std::fs::read(&paths[0]).unwrap(), old_config);
        assert!(!staged_path(&paths[0]).exists());
        assert!(!staged_path(&paths[1]).exists());
    }
    #[test]
    #[ignore = "requires a hash-verified private offline SQLite recovery copy"]
    fn private_database_copy_resumes_every_activation_boundary() {
        let source = PathBuf::from(
            std::env::var("LOCALSKY_RESTORE_TEST_DB").expect("offline recovery DB path"),
        );
        let expected_sha = std::env::var("LOCALSKY_RESTORE_TEST_SHA256").expect("recovery SHA256");
        assert_eq!(digest(&source).unwrap(), expected_sha);
        let before = rusqlite::Connection::open_with_flags(
            &source,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )
        .unwrap();
        let columns: Vec<String> = before
            .prepare("SELECT name FROM pragma_table_info('runs') ORDER BY cid")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!columns.is_empty());
        assert!(columns
            .iter()
            .all(|c| c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '_')));
        let query = format!("SELECT {} FROM runs ORDER BY id", columns.join(","));
        let rows = |db: &rusqlite::Connection| {
            let mut statement = db.prepare(&query).unwrap();
            statement
                .query_map([], |row| {
                    (0..columns.len())
                        .map(|i| row.get::<_, rusqlite::types::Value>(i))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        };
        let baseline = rows(&before);
        assert!(!baseline.is_empty());
        drop(before);
        for point in 0..=6 {
            let paths = fixture(&format!("private-db-{point}"));
            let candidate = candidates(&paths, true);
            std::fs::copy(&source, candidate[2].as_ref().unwrap()).unwrap();
            Publication::begin(&paths[0], &paths[2], &candidate, format!("private-{point}"))
                .unwrap()
                .finish()
                .unwrap();
            files::interrupt_after(point);
            assert!(activate_at_boot(&paths[0], &paths[2]).is_err());
            let first = activate_at_boot(&paths[0], &paths[2]).unwrap().unwrap();
            let mut db = rusqlite::Connection::open(&paths[2]).unwrap();
            crate::persistence::run_migrations(&mut db).unwrap();
            assert_eq!(rows(&db), baseline);
            db.execute_batch("CREATE TABLE recovery_boot_proof (id INTEGER PRIMARY KEY); INSERT INTO recovery_boot_proof VALUES (1)").unwrap();
            drop(db);
            drop(first); // Crash after normal migrations/writes, before boot acknowledgment.
            let second = activate_at_boot(&paths[0], &paths[2]).unwrap().unwrap();
            let db = rusqlite::Connection::open(&paths[2]).unwrap();
            assert_eq!(rows(&db), baseline);
            assert_eq!(
                db.query_row("SELECT count(*) FROM recovery_boot_proof", [], |r| r
                    .get::<_, i64>(0))
                    .unwrap(),
                1
            );
            assert_eq!(
                db.query_row("PRAGMA integrity_check", [], |r| r.get::<_, String>(0))
                    .unwrap(),
                "ok"
            );
            drop(db);
            second.complete().unwrap();
            assert!(activate_at_boot(&paths[0], &paths[2]).unwrap().is_none());
        }
        assert_eq!(digest(&source).unwrap(), expected_sha);
        println!("Preserved all {} original run rows through seven interruption boundaries and repeated boot.", baseline.len());
    }
}
