// RETIRED. Config history does not live in SQLite, and must not.
//
// This module wrapped the `config_snapshots` table from migration M0002:
// save() inserted a JSON Config blob, list() enumerated the rows, load()
// read one back. NOTHING in production ever called save(). The only wiring
// that ever existed was the read side: GET /api/v1/backup/snapshots listed
// the table, so that endpoint answered `{"snapshots": []}` for the life of
// every install while its own header, and docs/src/api.md, promised the
// history behind POST /api/v1/config/rollback. It now serves that history
// for real (src/api/backup.rs).
//
// The real history is on disk. ConfigStore::save copies the previous
// localsky.toml to <config_dir>/snapshots/<ts>.toml (newest 20 kept), and
// POST /api/v1/config/rollback restores from exactly those files
// (src/config/store.rs). One history, one key space.
//
// The type survives only because src/persistence/mod.rs still re-exports
// the name. It is deliberately empty so that standing a second config
// history back up is a compile error instead of a quiet second source of
// truth. DO NOT give it methods back. The two histories key differently:
// this table's `version` was an AUTOINCREMENT rowid, while rollback treats
// `version` as the snapshot's unix `ts`, so "version 7" from one lister
// would name a different config than "version 7" from the other, and a
// caller trusting the wrong lister would roll the yard back to a config it
// never asked for.
//
// To finish the retirement, delete this file together with the `pub mod`
// and `pub use` lines in src/persistence/mod.rs (plus its header entry),
// and drop M0002's table, index and retention trigger in a new migration.

/// Retired marker. Carries no state and no behavior on purpose; see the
/// module header.
pub struct ConfigSnapshotStore;
