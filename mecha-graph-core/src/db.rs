//! Database open/init. Registers sqlite-vec before any connection is opened
//! (vec0 virtual tables are created by migrations), then applies pragmas and
//! runs migrations.

use crate::error::Result;
use crate::migrations;
use rusqlite::Connection;
use std::path::{Path, PathBuf};
use std::sync::Once;

static VEC_INIT: Once = Once::new();

/// Register sqlite-vec as an auto-extension so every subsequently opened
/// connection has vec0 virtual tables available. Same pattern as FlowMail.
pub fn register_vec_extension() {
    VEC_INIT.call_once(|| unsafe {
        // c_char is u8 on aarch64, i8 on x86 — go through c_char.
        rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::os::raw::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
        >(
            sqlite_vec::sqlite3_vec_init as *const ()
        )));
    });
}

/// Default DB path: `~/.mecha-graph/graph.db` (spec §8.4), overridable via `MECHA_GRAPH_DB`.
pub fn default_db_path() -> PathBuf {
    if let Ok(p) = std::env::var("MECHA_GRAPH_DB") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join(".mecha-graph").join("graph.db")
}

/// Default keyfile location: `db.key` next to the database.
pub fn keyfile_path(db_path: &Path) -> PathBuf {
    if let Ok(p) = std::env::var("MECHA_GRAPH_DB_KEYFILE") {
        return PathBuf::from(p);
    }
    db_path.with_file_name("db.key")
}

/// Resolve the SQLCipher key, if any:
/// 1. `MECHA_GRAPH_DB_KEY` env — used verbatim (passphrase, or `x'HEX'` raw form)
/// 2. keyfile next to the DB — 64 hex chars, wrapped as a raw `x'…'` key
/// 3. none → plaintext
pub fn resolve_key(db_path: &Path) -> Option<String> {
    if let Ok(key) = std::env::var("MECHA_GRAPH_DB_KEY") {
        if !key.is_empty() {
            return Some(key);
        }
    }
    let kf = keyfile_path(db_path);
    if let Ok(hex) = std::fs::read_to_string(&kf) {
        let hex = hex.trim();
        if !hex.is_empty() {
            return Some(format!("x'{hex}'"));
        }
    }
    None
}

/// Open (creating if necessary) the database at `path` and run migrations.
///
/// Encryption (§10): if `MECHA_GRAPH_DB_KEY` is set or a `db.key` file sits next to
/// the DB, the database is opened with SQLCipher (raw-key form for keyfiles —
/// no per-open KDF cost). `pkg encrypt` migrates an existing plaintext DB.
/// The DuckDB analysis path (§8.4) can't read SQLCipher — use
/// `pkg decrypt --out <snapshot>` for analytics on an encrypted store.
pub fn open(path: &Path) -> Result<Connection> {
    register_vec_extension();

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
        // Concentrated personal data: restrict the directory (spec §10).
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }

    let conn = Connection::open(path)?;
    if let Some(key) = resolve_key(path) {
        // Must be the first operation after opening.
        conn.pragma_update(None, "key", &key)?;
    }
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5000)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    migrations::run_migrations(&conn).map_err(|e| {
        crate::error::Error::Other(format!(
            "cannot read {} ({e}) — if the DB is encrypted, the key is wrong or missing \
             (MECHA_GRAPH_DB_KEY / {})",
            path.display(),
            keyfile_path(path).display()
        ))
    })?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

// ─── Encryption migration (§10) ──────────────────────────────────────────────

/// Tables copied verbatim between stores (encrypt, decrypt, fork). FTS tables
/// are repopulated by the episode/fact insert triggers; vec0 rows copy via
/// SELECT. `predicate*` use INSERT OR IGNORE (migrations pre-seed them).
///
/// EVERY new ordinary table a migration adds MUST be added here — the
/// `test_copy_tables_covers_schema` canary fails otherwise. (V004/V006/V007
/// were missed for a while: encrypt/decrypt copies silently dropped
/// annotations, undo history and tombstones until the fork work caught it.)
const COPY_TABLES: &[&str] = &[
    "embed_meta",
    "node_alias",
    "node_identifier",
    "episode",
    "episode_raw",
    "mention",
    "fact",
    "fact_candidate",
    // Agent opinions on candidates. Copied, because a fork that drops
    // them loses the only record of how a mechanism has been performing.
    "agent_verdict",
    "episode_enrichment",
    "task_detail",
    "project_detail",
    "goal_detail",
    "node_context",
    "assign_rule",
    "node_field",
    "person_interaction",
    "external_ref",
    "ingest_state",
    "extract_state",
    "vec_episode",
    "vec_fact",
    "vec_rejected",
    "episode_annotation", // V004
    "undo_log",           // V006
    "episode_tombstone",  // V007
    "query_log",          // V009
    "retrieval_touch",    // V009
    "event_log",          // V009
    "fact_observation",   // V010
    "class_ledger",       // V012
    // Entity maintenance proposals. Copied for the same reason
    // `agent_verdict` is: the decided ones are the only record of what has
    // already been asked and answered, and a fork that drops them re-files
    // every rejection the next time the audit runs.
    "entity_proposal", // V018
    // Refused weak alias matches. Copied because they are the evidence the
    // missing-entity detector reads: a fork that dropped them would forget
    // which names keep appearing with nobody to attach them to.
    "unlinked_mention", // V019
    // The review queue's vectors. Derivable, and copied for the same reason
    // `vec_rejected` is: rebuilding it is a run of the embedding server over
    // the whole pending queue, and an encrypt or a decrypt is a bad moment to
    // hand someone that bill. Copying is correct rather than merely kind —
    // the candidate ids and their statements cross unchanged, so every row
    // still keys to exactly what it was computed from.
    "candidate_embedding", // V023
];
/// Copied with `INSERT OR IGNORE`, because a freshly migrated target already
/// holds rows the source is about to send.
///
/// `nodes` is here for **one row**, not because it is a seed table: V020 seeds
/// the `agent-mecha` node into every new schema, so a straight `INSERT` from a
/// source that also has it fails the primary key and takes encrypt, decrypt
/// and fork down with it. Skipping it is right rather than merely convenient —
/// the target's copy is the current seed, and the rest of the table is
/// untouched because a fresh target has no other nodes to collide with.
///
/// The lesson generalises: seeding *data* in a migration is not free while a
/// copy path exists, and the copy path is where you find out.
const SEEDED_TABLES: &[&str] = &["predicate", "predicate_alias", "node_slot", "nodes"];

/// Copy all data from the attached schema `src` into `main` on `conn`.
/// `conn` must already have the full (empty) schema.
/// Copy every table from an attached `src` into `main`.
///
/// **The vector width is reconciled here, not by the callers.** `main`'s
/// schema came from `run_migrations`, which creates the `vec0` tables at the
/// compiled-in default; `src` may hold a different one, because changing the
/// embedding model rebuilds them through [`crate::embed::ensure_vec_dims`].
/// Copying wide vectors into a narrow table fails with a sqlite-vec
/// "Dimension mismatch for inserted vector … Expected 768 dimensions but
/// received 1024" that names the column and not the cause.
///
/// It lives in here because putting it at the call sites is what broke: the
/// reconciliation was added to `export_plaintext` alone, and `encrypt_in_place`
/// and `fork_db` — which run the identical migrate-then-copy sequence twelve
/// and a hundred lines away — went on failing. Fork was the one anybody
/// noticed, because it is the only one of the three that people run on a
/// whim, so a broken test-bed copy read as "forking is broken" rather than as
/// "two of our three copy paths are". This trio has now been broken together
/// twice by a change made to the destination's schema before the copy; the
/// first time was a migration seeding a node. A step that every copy path
/// needs belongs in the function every copy path calls.
///
/// Read off `src`'s own schema rather than off config, because **a copy must
/// reproduce the database it is a copy of**, even when config has since moved
/// on to a different model.
fn copy_all_tables(conn: &Connection) -> Result<()> {
    if let Some(dims) = crate::embed::declared_vec_dims_in(conn, "src")? {
        crate::embed::ensure_vec_dims(conn, dims)?;
    }
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    for t in SEEDED_TABLES {
        conn.execute_batch(&format!(
            "INSERT OR IGNORE INTO main.{t} SELECT * FROM src.{t};"
        ))?;
    }
    for t in COPY_TABLES {
        conn.execute_batch(&format!("INSERT INTO main.{t} SELECT * FROM src.{t};"))?;
    }
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(())
}

fn quick_counts(conn: &Connection, schema: &str) -> Result<(i64, i64, i64, i64)> {
    let c = |t: &str| -> Result<i64> {
        Ok(
            conn.query_row(&format!("SELECT COUNT(*) FROM {schema}.{t}"), [], |r| {
                r.get(0)
            })?,
        )
    };
    Ok((c("episode")?, c("fact")?, c("nodes")?, c("fact_candidate")?))
}

/// Encrypt a plaintext DB in place: generates a keyfile next to it, copies
/// everything into a fresh encrypted store, verifies counts, swaps files.
/// The plaintext original is moved to `<db>.plain.bak` — caller decides its
/// fate. Fails if a key already resolves (already encrypted).
pub fn encrypt_in_place(db_path: &Path) -> Result<PathBuf> {
    use crate::error::Error;
    register_vec_extension();

    if resolve_key(db_path).is_some() {
        return Err(Error::Other(
            "a key already resolves — is the DB already encrypted?".into(),
        ));
    }
    if !db_path.exists() {
        return Err(Error::Other(format!(
            "{} does not exist",
            db_path.display()
        )));
    }

    // Checkpoint the WAL so the main file is complete, then close.
    {
        let src = Connection::open(db_path)?;
        src.pragma_update(None, "wal_checkpoint", "TRUNCATE")?;
    }

    // Generate a random 32-byte raw key.
    let key_hex: String = {
        let uid1 = uuid::Uuid::new_v4();
        let uid2 = uuid::Uuid::new_v4();
        uid1.simple().to_string() + &uid2.simple().to_string()
    };
    let keyfile = keyfile_path(db_path);
    std::fs::write(&keyfile, &key_hex)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&keyfile, std::fs::Permissions::from_mode(0o600))?;
    }

    // Build the encrypted copy alongside.
    let enc_path = db_path.with_extension("db.enc-tmp");
    let _ = std::fs::remove_file(&enc_path);
    {
        let conn = Connection::open(&enc_path)?;
        conn.pragma_update(None, "key", format!("x'{key_hex}'"))?;
        conn.pragma_update(None, "foreign_keys", "OFF")?;
        migrations::run_migrations(&conn)?;
        conn.execute_batch(&format!(
            "ATTACH DATABASE '{}' AS src KEY '';",
            db_path.display()
        ))?;
        // One transaction pins a consistent read snapshot of src, so a
        // concurrent writer can't tear the copy or fail verification.
        conn.execute_batch("BEGIN;")?;
        copy_all_tables(&conn)?;

        let src_counts = quick_counts(&conn, "src")?;
        let dst_counts = quick_counts(&conn, "main")?;
        if src_counts != dst_counts {
            let _ = conn.execute_batch("ROLLBACK;");
            return Err(Error::Other(format!(
                "verification failed: src {src_counts:?} != encrypted {dst_counts:?}"
            )));
        }
        conn.execute_batch("COMMIT; DETACH DATABASE src;")?;
    }

    // Swap: plaintext → .plain.bak, encrypted → live. Remove stale WAL/SHM.
    let bak = db_path.with_extension("db.plain.bak");
    std::fs::rename(db_path, &bak)?;
    std::fs::rename(&enc_path, db_path)?;
    for ext in ["db-wal", "db-shm"] {
        let _ = std::fs::remove_file(db_path.with_extension(ext));
    }
    Ok(bak)
}

/// Export a plaintext snapshot of an (encrypted) DB — the DuckDB analytics
/// path (§8.4) for an encrypted store. The snapshot is chmod 600; treat it
/// as ephemeral.
pub fn export_plaintext(db_path: &Path, out: &Path) -> Result<()> {
    use crate::error::Error;
    register_vec_extension();

    let key = resolve_key(db_path)
        .ok_or_else(|| Error::Other("DB has no key — it is already plaintext".into()))?;

    let _ = std::fs::remove_file(out);
    let conn = Connection::open(out)?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    migrations::run_migrations(&conn)?;
    conn.execute_batch(&format!(
        "ATTACH DATABASE '{}' AS src KEY \"{}\";",
        db_path.display(),
        key.replace('"', "")
    ))?;
    // The vector width is reconciled inside copy_all_tables, which is where
    // every copy path gets it — see its doc for why it is not here.
    // Transaction pins a consistent read snapshot vs concurrent writers.
    conn.execute_batch("BEGIN;")?;
    copy_all_tables(&conn)?;
    let src_counts = quick_counts(&conn, "src")?;
    let dst_counts = quick_counts(&conn, "main")?;
    if src_counts != dst_counts {
        let _ = conn.execute_batch("ROLLBACK;");
        return Err(Error::Other(format!(
            "verification failed: src {src_counts:?} != snapshot {dst_counts:?}"
        )));
    }
    conn.execute_batch("COMMIT; DETACH DATABASE src;")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// Fork the database: a full encrypted copy under a **fresh** key — the test
/// bed for probing experiments and eval (PLAN.md Wave 1c). Unlike
/// `export_plaintext` the result stays encrypted (a fork holds private
/// episodes for days, not minutes); unlike `encrypt_in_place` the source is
/// untouched. Returns the fork's keyfile path.
///
/// The keyfile convention is directory-scoped (`db.key` next to the DB), so
/// a fork **must land in its own directory** — forking into the source's
/// directory would clobber the live key. Refused, along with any destination
/// whose `db.key` already exists.
pub fn fork_db(db_path: &Path, out: &Path) -> Result<PathBuf> {
    use crate::error::Error;
    register_vec_extension();

    if !db_path.exists() {
        return Err(Error::Other(format!(
            "{} does not exist",
            db_path.display()
        )));
    }
    if out.exists() {
        return Err(Error::Other(format!(
            "{} already exists — deleting a fork is a deliberate act, do it yourself",
            out.display()
        )));
    }
    // Same-directory fork would overwrite the live db.key. Compare parents
    // (canonicalized where possible — the destination may not exist yet).
    let src_dir = db_path
        .parent()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.into()));
    let dst_dir = out
        .parent()
        .map(|p| p.canonicalize().unwrap_or_else(|_| p.into()));
    if src_dir == dst_dir {
        return Err(Error::Other(
            "fork into a different directory — db.key is directory-scoped and a \
             same-dir fork would clobber the live key"
                .into(),
        ));
    }
    // Deliberately NOT keyfile_path(): a MECHA_GRAPH_DB_KEYFILE env override points
    // at the LIVE key and must never name where a fork's fresh key lands.
    let dest_key = out.with_file_name("db.key");
    if dest_key.exists() {
        return Err(Error::Other(format!(
            "{} already exists — refusing to overwrite a key",
            dest_key.display()
        )));
    }

    if let Some(dir) = out.parent() {
        std::fs::create_dir_all(dir)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700));
        }
    }

    // Fresh random 32-byte raw key, same recipe as encrypt_in_place.
    let key_hex: String = {
        let uid1 = uuid::Uuid::new_v4();
        let uid2 = uuid::Uuid::new_v4();
        uid1.simple().to_string() + &uid2.simple().to_string()
    };
    std::fs::write(&dest_key, &key_hex)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dest_key, std::fs::Permissions::from_mode(0o600))?;
    }

    let src_key = resolve_key(db_path).unwrap_or_default(); // '' = plaintext source
    let conn = Connection::open(out)?;
    conn.pragma_update(None, "key", format!("x'{key_hex}'"))?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    migrations::run_migrations(&conn)?;
    conn.execute_batch(&format!(
        "ATTACH DATABASE '{}' AS src KEY \"{}\";",
        db_path.display(),
        src_key.replace('"', "")
    ))?;
    // One transaction pins a consistent read snapshot of src (WAL-safe).
    conn.execute_batch("BEGIN;")?;
    copy_all_tables(&conn)?;
    let src_counts = quick_counts(&conn, "src")?;
    let dst_counts = quick_counts(&conn, "main")?;
    if src_counts != dst_counts {
        let _ = conn.execute_batch("ROLLBACK;");
        let _ = std::fs::remove_file(&dest_key);
        return Err(Error::Other(format!(
            "verification failed: src {src_counts:?} != fork {dst_counts:?}"
        )));
    }
    conn.execute_batch("COMMIT; DETACH DATABASE src;")?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(out, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(dest_key)
}

/// In-memory database for tests.
pub fn open_memory() -> Result<Connection> {
    register_vec_extension();
    let conn = Connection::open_in_memory()?;
    conn.pragma_update(None, "foreign_keys", "OFF")?;
    migrations::run_migrations(&conn)?;
    conn.pragma_update(None, "foreign_keys", "ON")?;
    Ok(conn)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::episode::{upsert_episode, Episode};

    fn sample_episode(sid: &str) -> Episode {
        Episode {
            id: 0,
            uid: String::new(),
            source: "note".into(),
            source_id: sid.into(),
            source_ref: None,
            body: format!("encrypted roundtrip test {sid}"),
            occurred_at: "2026-08-01 10:00:00".into(),
            occurred_end: None,
            ingested_at: String::new(),
            lat: None,
            lon: None,
            location: None,
            sensitivity: "personal".into(),
            scope_id: None,
            meta: None,
            raw: None,
        }
    }

    #[test]
    fn test_copy_tables_covers_schema() {
        // Canary: every ordinary table the migrations create must be in
        // COPY_TABLES (or SEEDED_TABLES), or encrypt/decrypt/fork silently
        // drop it. Fails the moment a migration adds a table without
        // updating the list — V004/V006/V007 slipped through exactly this
        // way before the canary existed.
        let conn = crate::db::open_memory().unwrap();
        let mut stmt = conn
            .prepare(
                "SELECT name FROM sqlite_master WHERE type='table'
                   AND name NOT LIKE 'sqlite_%'
                   AND name != '_migrations'
                   -- FTS5 external-content tables + shadows: repopulated by triggers
                   AND name NOT LIKE 'fts_%'
                   -- vec0 shadow tables: the virtual tables themselves are listed
                   AND name NOT LIKE 'vec_episode_%'
                   AND name NOT LIKE 'vec_fact_%'
                   AND name NOT LIKE 'vec_rejected_%'",
            )
            .unwrap();
        let schema: std::collections::BTreeSet<String> = stmt
            .query_map([], |r| r.get::<_, String>(0))
            .unwrap()
            .map(|r| r.unwrap())
            .collect();
        let listed: std::collections::BTreeSet<String> = COPY_TABLES
            .iter()
            .chain(SEEDED_TABLES.iter())
            .map(|s| s.to_string())
            .collect();
        let missing: Vec<_> = schema.difference(&listed).collect();
        let stale: Vec<_> = listed.difference(&schema).collect();
        assert!(
            missing.is_empty(),
            "tables not copied by encrypt/decrypt/fork: {missing:?}"
        );
        assert!(
            stale.is_empty(),
            "COPY_TABLES lists tables that no longer exist: {stale:?}"
        );
    }

    #[test]
    fn test_fork_fresh_key_and_independence() {
        let src_dir = tempfile::tempdir().unwrap();
        let fork_dir = tempfile::tempdir().unwrap();
        let db_path = src_dir.path().join("graph.db");
        let out = fork_dir.path().join("probe.db");

        {
            let conn = open(&db_path).unwrap();
            upsert_episode(&conn, &sample_episode("e1")).unwrap();
            upsert_episode(&conn, &sample_episode("e2")).unwrap();
        }
        let bak = encrypt_in_place(&db_path).unwrap();
        std::fs::remove_file(bak).unwrap();

        let fork_key = fork_db(&db_path, &out).unwrap();
        assert!(fork_key.exists());
        assert_ne!(
            std::fs::read_to_string(&fork_key).unwrap(),
            std::fs::read_to_string(keyfile_path(&db_path)).unwrap(),
            "fork must have a FRESH key"
        );

        // The fork opens via its own sibling keyfile and holds the data.
        let fork_conn = open(&out).unwrap();
        let n: i64 = fork_conn
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);

        // Writes to the fork do not touch the source.
        upsert_episode(&fork_conn, &sample_episode("fork-only")).unwrap();
        let src_conn = open(&db_path).unwrap();
        let src_n: i64 = src_conn
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(src_n, 2, "fork writes must not reach the source");

        // Same-directory fork is refused (it would clobber the live key).
        let same_dir = src_dir.path().join("probe2.db");
        assert!(fork_db(&db_path, &same_dir).is_err());
        // Existing destination is refused.
        assert!(fork_db(&db_path, &out).is_err());
    }

    /// **Every copy path survives a store whose vectors are wider than the
    /// migrations' default**, which is the state any embedding-model change
    /// leaves behind.
    ///
    /// `fork_db` and `encrypt_in_place` both failed here with sqlite-vec's
    /// "Expected 768 dimensions but received 1024" — a message that names the
    /// column and not the cause — while `export_plaintext` passed, because the
    /// reconciliation had been added to that one call site alone. The
    /// assertion is the copy itself: it fails on the old code for two of the
    /// three, and the third is in the loop so the fix cannot be undone for one
    /// path without the test noticing.
    #[test]
    fn every_copy_path_carries_a_store_wider_than_the_default() {
        for (name, run) in [("fork", 0usize), ("encrypt", 1usize), ("export", 2usize)] {
            let dir = tempfile::tempdir().unwrap();
            let db_path = dir.path().join("graph.db");
            {
                let conn = open(&db_path).unwrap();
                // What a model change leaves behind: the vec0 tables rebuilt
                // wider than `run_migrations` creates them.
                assert!(crate::embed::ensure_vec_dims(&conn, 1024).unwrap());
                let (id, _) = upsert_episode(&conn, &sample_episode("wide")).unwrap();
                let wide: Vec<f32> = (0..1024).map(|i| i as f32).collect();
                conn.execute(
                    "INSERT INTO vec_episode (episode_id, embedding) VALUES (?1, ?2)",
                    rusqlite::params![id, serde_json::to_string(&wide).unwrap()],
                )
                .unwrap();
            }
            match run {
                0 => {
                    let out_dir = tempfile::tempdir().unwrap();
                    fork_db(&db_path, &out_dir.path().join("fork.db"))
                        .unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                1 => {
                    encrypt_in_place(&db_path).unwrap_or_else(|e| panic!("{name}: {e}"));
                }
                _ => {
                    encrypt_in_place(&db_path).unwrap();
                    let out = dir.path().join("plain.db");
                    export_plaintext(&db_path, &out).unwrap_or_else(|e| panic!("{name}: {e}"));
                }
            }
        }
    }

    #[test]
    fn test_encrypt_roundtrip_and_snapshot() {
        // NOTE: relies on MECHA_GRAPH_DB_KEY / MECHA_GRAPH_DB_KEYFILE not being set in the
        // test environment; keyfile resolution is per-tempdir so tests don't
        // collide.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("graph.db");

        // Plaintext DB with data, including an embedding row.
        {
            let conn = open(&db_path).unwrap();
            let (id, _) = upsert_episode(&conn, &sample_episode("e1")).unwrap();
            upsert_episode(&conn, &sample_episode("e2")).unwrap();
            let fake: Vec<f32> = (0..768).map(|i| i as f32).collect();
            conn.execute(
                "INSERT INTO vec_episode (episode_id, embedding) VALUES (?1, ?2)",
                rusqlite::params![id, serde_json::to_string(&fake).unwrap()],
            )
            .unwrap();
            crate::episode::store_raw(&conn, id, "the full raw archive").unwrap();
        }

        let bak = encrypt_in_place(&db_path).unwrap();
        assert!(bak.exists());
        assert!(keyfile_path(&db_path).exists());

        // Raw open without the key must NOT read it.
        {
            let raw = Connection::open(&db_path).unwrap();
            assert!(raw
                .query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r
                    .get::<_, i64>(0))
                .is_err());
        }

        // open() picks the keyfile up automatically; data + vectors survived,
        // FTS was repopulated by the triggers.
        {
            let conn = open(&db_path).unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 2);
            let v: i64 = conn
                .query_row("SELECT COUNT(*) FROM vec_episode", [], |r| r.get(0))
                .unwrap();
            assert_eq!(v, 1);
            let f: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM fts_episode WHERE fts_episode MATCH 'roundtrip'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(f, 2);
            // The capture archive must survive encryption migration.
            let raw = crate::episode::get_raw(&conn, 1).unwrap();
            assert_eq!(raw.as_deref(), Some("the full raw archive"));
        }

        // Double-encrypt refuses.
        assert!(encrypt_in_place(&db_path).is_err());

        // Plaintext snapshot for analytics reads without any key.
        let snap = dir.path().join("analytics.db");
        export_plaintext(&db_path, &snap).unwrap();
        let plain = Connection::open(&snap).unwrap();
        let n: i64 = plain
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 2);
    }
}
