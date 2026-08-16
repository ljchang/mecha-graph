//! Ingestion (§5). One `Source` per stream. `deterministic_links` is the
//! important method: each source declares what it knows for certain, so
//! expensive tiers only handle genuine ambiguity.

pub mod bee;
pub mod github;
pub mod ics;
pub mod imessage;
pub mod mbox;
pub mod reflect;
pub mod sessions;
pub mod slack;

use crate::episode::{self, Episode, IngestOutcome};
use crate::error::Result;
use crate::graph;
use crate::rollup;
use rusqlite::{params, Connection};

/// A link a source asserts deterministically (Tier 1, ~100% precision).
#[derive(Debug, Clone)]
pub enum ProposedLink {
    /// Person known by (email?, phone?, display_name); mention + identifier + alias.
    Person {
        email: Option<String>,
        phone: Option<String>,
        display_name: String,
        /// Extra role fact, e.g. ("attended", event_node_id).
        fact: Option<(String, String)>,
    },
    /// Mention of an existing/creatable non-person node.
    NodeMention { node_id: String },
    /// Project known by filesystem path (agent sessions: cwd/git root, §5.1).
    /// Deterministic: node_identifier kind='path'. Zero AI.
    Project { root: String, name: String },
}

pub trait Source {
    fn id(&self) -> &'static str;
    /// Episodes newer than `since` (source-defined cursor semantics).
    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>>;
    /// Identity this source already knows — calendar: attendees; email:
    /// From/To; sessions: cwd. Runs before any fuzzy or LLM tier.
    fn deterministic_links(&self, ep: &Episode) -> Vec<ProposedLink>;
    /// True when each episode's `source_ref` is its own dedicated file, safe
    /// to delete individually under capture_delete retention (Bee: yes;
    /// mbox: one shared file — handled at the integration layer instead).
    fn per_episode_files(&self) -> bool {
        false
    }
}

/// Retention policy per source (user decision 2026-08-02): raw files are a
/// stopgap until extraction quality is trusted, then the encrypted DB becomes
/// the system of record.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Retention {
    /// Leave source files alone (default).
    #[default]
    Keep,
    /// Archive full raw content into `episode_raw` (inside the encrypted DB);
    /// keep the files too. The trust-building phase.
    Capture,
    /// Archive into `episode_raw`, then DELETE the source file — only after
    /// the archive row is verified present. Zero plaintext residue.
    CaptureDelete,
}

impl Retention {
    pub fn parse(s: &str) -> Option<Retention> {
        match s {
            "keep" => Some(Retention::Keep),
            "capture" => Some(Retention::Capture),
            "capture_delete" => Some(Retention::CaptureDelete),
            _ => None,
        }
    }
}

#[derive(Debug, Default)]
pub struct IngestReport {
    pub inserted: usize,
    pub updated: usize,
    pub unchanged: usize,
    pub tombstoned: usize,
    pub mentions: usize,
    pub alias_mentions: usize,
    pub captured: usize,
    pub deleted_files: usize,
}

impl IngestReport {
    /// Sync-summary suffix; empty unless re-ingest hit deletion tombstones.
    pub fn tombstone_note(&self) -> String {
        if self.tombstoned == 0 {
            String::new()
        } else {
            format!(", {} deleted-skipped", self.tombstoned)
        }
    }
}

/// Pipeline driver with default retention (Keep).
pub fn ingest(conn: &Connection, source: &dyn Source, since: Option<&str>) -> Result<IngestReport> {
    ingest_with(conn, source, since, Retention::Keep)
}

/// Pipeline driver (§5.3): fetch → upsert (idempotent) → raw capture →
/// tier-1 links → alias-scan linker → rollups → cursor.
pub fn ingest_with(
    conn: &Connection,
    source: &dyn Source,
    since: Option<&str>,
    retention: Retention,
) -> Result<IngestReport> {
    let started = crate::ids::now();
    let mut report = IngestReport::default();
    let mut max_occurred: Option<String> = since.map(|s| s.to_string());

    let episodes = match source.fetch(since) {
        Ok(eps) => eps,
        Err(e) => {
            conn.execute(
                "INSERT INTO ingest_state (source, last_run_at, last_error) VALUES (?1, ?2, ?3)
                 ON CONFLICT(source) DO UPDATE SET last_run_at = ?2, last_error = ?3",
                params![source.id(), started, e.to_string()],
            )?;
            return Err(e);
        }
    };

    for ep in &episodes {
        let (episode_id, outcome) = episode::upsert_episode(conn, ep)?;
        if outcome == IngestOutcome::Tombstoned {
            // User deleted this episode; do not resurrect it (and leave the
            // source file alone — there is no archive row to justify deletion).
            report.tombstoned += 1;
            continue;
        }

        // Raw capture + optional source-file deletion (retention policy).
        // Runs for Unchanged episodes too: files ingested before capture mode
        // still need archiving before they may be deleted.
        if retention != Retention::Keep {
            if let Some(raw) = &ep.raw {
                if !episode::has_raw(conn, episode_id)? {
                    episode::store_raw(conn, episode_id, raw)?;
                    report.captured += 1;
                }
                if retention == Retention::CaptureDelete && source.per_episode_files() {
                    // Delete ONLY after the archive row is verified present.
                    if episode::has_raw(conn, episode_id)? {
                        if let Some(path) = ep.source_ref.as_deref() {
                            if std::path::Path::new(path).is_file()
                                && std::fs::remove_file(path).is_ok()
                            {
                                report.deleted_files += 1;
                            }
                        }
                    }
                }
            }
        }

        match outcome {
            IngestOutcome::Inserted => report.inserted += 1,
            IngestOutcome::Updated => report.updated += 1,
            IngestOutcome::Unchanged => {
                report.unchanged += 1;
                continue; // links already recorded on first ingest
            }
            IngestOutcome::Tombstoned => unreachable!("handled above"),
        }

        let ep_uid: String = conn.query_row(
            "SELECT uid FROM episode WHERE id = ?1",
            params![episode_id],
            |r| r.get(0),
        )?;

        // Tier 1: deterministic links the source asserts.
        for link in source.deterministic_links(ep) {
            match link {
                ProposedLink::Person {
                    email,
                    phone,
                    display_name,
                    fact,
                } => {
                    // Phone is a deterministic key too: reuse the node it
                    // already maps to before creating anything.
                    let person = match phone
                        .as_deref()
                        .map(|p| graph::get_node_by_identifier(conn, "phone", p))
                        .transpose()?
                        .flatten()
                    {
                        Some(existing) => existing,
                        None => {
                            let p = graph::get_or_create_person(
                                conn,
                                email.as_deref(),
                                &display_name,
                                source.id(),
                            )?;
                            if let Some(ph) = phone.as_deref() {
                                graph::upsert_identifier(conn, "phone", ph, &p.id, source.id())?;
                            }
                            p
                        }
                    };
                    episode::add_mention(conn, episode_id, &person.id, "attendee", 1.0)?;
                    rollup::touch_person(conn, &person.id, &ep_uid, &ep.source, &ep.occurred_at)?;
                    report.mentions += 1;
                    if let Some((predicate, object_id)) = fact {
                        crate::fact::assert_fact(
                            conn,
                            &person.id,
                            &predicate,
                            Some(&object_id),
                            None,
                            &format!(
                                "{} {} {}",
                                person.name,
                                predicate.replace('_', " "),
                                object_id
                            ),
                            Some(episode_id),
                            Some(&ep.occurred_at),
                            0.95,
                            "attendee",
                        )?;
                    }
                }
                ProposedLink::NodeMention { node_id } => {
                    episode::add_mention(conn, episode_id, &node_id, "manual", 1.0)?;
                    report.mentions += 1;
                }
                ProposedLink::Project { root, name } => {
                    let project = match graph::get_node_by_identifier(conn, "path", &root)? {
                        Some(n) => n,
                        None => {
                            let id = format!(
                                "project-{}",
                                crate::ids::content_hash(&root)[..12].to_string()
                            );
                            let mut node = graph::Node::new(&id, "project", &name);
                            node.source = source.id().to_string();
                            node.source_ref = Some(root.clone());
                            graph::upsert_node(conn, &node)?;
                            graph::upsert_identifier(conn, "path", &root, &id, source.id())?;
                            graph::get_node(conn, &id)?.expect("just created")
                        }
                    };
                    episode::add_mention(conn, episode_id, &project.id, "attendee", 1.0)?;
                    report.mentions += 1;
                }
            }
        }

        // Cheap alias-scan linker: pays off calendar-seeded aliases on
        // name-only sources like Bee (§5.1). Persons found here also update
        // the rollup (channel-correct: Bee → last_spoken_at).
        let before: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mention WHERE episode_id = ?1",
            params![episode_id],
            |r| r.get(0),
        )?;
        episode::link_by_alias_scan(conn, episode_id, &ep.body)?;
        let mut stmt = conn.prepare_cached(
            "SELECT m.node_id FROM mention m JOIN nodes n ON n.id = m.node_id
             WHERE m.episode_id = ?1 AND m.extractor = 'alias' AND n.node_type = 'person'",
        )?;
        let alias_people: Vec<String> = stmt
            .query_map(params![episode_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        for node_id in alias_people {
            rollup::touch_person(conn, &node_id, &ep_uid, &ep.source, &ep.occurred_at)?;
        }
        let after: i64 = conn.query_row(
            "SELECT COUNT(*) FROM mention WHERE episode_id = ?1",
            params![episode_id],
            |r| r.get(0),
        )?;
        report.alias_mentions += (after - before).max(0) as usize;

        if max_occurred
            .as_deref()
            .map_or(true, |m| ep.occurred_at.as_str() > m)
        {
            max_occurred = Some(ep.occurred_at.clone());
        }
    }

    conn.execute(
        "INSERT INTO ingest_state (source, cursor, last_run_at, last_ok_at, items_seen, last_error)
         VALUES (?1, ?2, ?3, ?3, ?4, NULL)
         ON CONFLICT(source) DO UPDATE SET
             cursor = COALESCE(excluded.cursor, cursor),
             last_run_at = excluded.last_run_at,
             last_ok_at = excluded.last_ok_at,
             items_seen = items_seen + excluded.items_seen,
             last_error = NULL",
        params![
            source.id(),
            max_occurred,
            started,
            (report.inserted + report.updated) as i64
        ],
    )?;

    Ok(report)
}

pub fn get_cursor(conn: &Connection, source_id: &str) -> Result<Option<String>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT cursor FROM ingest_state WHERE source = ?1",
            params![source_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten())
}
