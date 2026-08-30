//! Provenance layer (§4.1): immutable source records + the mention M:N substrate.
//! `UNIQUE(source, source_id)` + `content_hash` make re-ingest idempotent — a
//! full re-run is a no-op (§5.3).

use crate::error::Result;
use crate::ids::{content_hash, new_uid};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Episode {
    #[serde(default)]
    pub id: i64,
    #[serde(default)]
    pub uid: String,
    pub source: String,
    pub source_id: String,
    pub source_ref: Option<String>,
    pub body: String,
    pub occurred_at: String,
    pub occurred_end: Option<String>,
    #[serde(default)]
    pub ingested_at: String,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
    pub location: Option<String>,
    #[serde(default = "default_sensitivity")]
    pub sensitivity: String,
    pub scope_id: Option<String>,
    pub meta: Option<serde_json::Value>,
    /// Full raw source content, populated by sources that support
    /// capture-retention. Never persisted on the Episode row itself —
    /// the ingest driver routes it to `episode_raw` (encrypted DB archive).
    #[serde(skip)]
    pub raw: Option<String>,
}

fn default_sensitivity() -> String {
    "personal".to_string()
}

#[derive(Debug, PartialEq, Eq)]
pub enum IngestOutcome {
    Inserted,
    Updated,
    Unchanged,
    /// A previously deleted episode with this (source, source_id) — skipped.
    /// The returned rowid is 0: no episode row exists, do no follow-up work.
    Tombstoned,
}

pub fn row_to_episode(row: &rusqlite::Row) -> std::result::Result<Episode, rusqlite::Error> {
    let meta_str: Option<String> = row.get("meta")?;
    Ok(Episode {
        id: row.get("id")?,
        uid: row.get("uid")?,
        source: row.get("source")?,
        source_id: row.get("source_id")?,
        source_ref: row.get("source_ref")?,
        body: row.get("body")?,
        occurred_at: row.get("occurred_at")?,
        occurred_end: row.get("occurred_end")?,
        ingested_at: row.get("ingested_at")?,
        lat: row.get("lat")?,
        lon: row.get("lon")?,
        location: row.get("location")?,
        sensitivity: row.get("sensitivity")?,
        scope_id: row.get("scope_id")?,
        meta: meta_str.and_then(|s| serde_json::from_str(&s).ok()),
        raw: None,
    })
}

// ─── Raw-capture archive (retention 'capture'/'capture_delete') ──────────────

pub fn store_raw(conn: &Connection, episode_id: i64, content: &str) -> Result<()> {
    conn.execute(
        "INSERT OR REPLACE INTO episode_raw (episode_id, content) VALUES (?1, ?2)",
        params![episode_id, content],
    )?;
    Ok(())
}

pub fn get_raw(conn: &Connection, episode_id: i64) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT content FROM episode_raw WHERE episode_id = ?1",
            params![episode_id],
            |r| r.get(0),
        )
        .optional()?)
}

pub fn has_raw(conn: &Connection, episode_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) > 0 FROM episode_raw WHERE episode_id = ?1",
        params![episode_id],
        |r| r.get(0),
    )?)
}

/// Insert or update an episode idempotently. Returns (rowid, outcome).
/// Unchanged content (same hash) is skipped entirely — no FTS churn, no
/// re-embedding, no re-enrichment. A tombstoned (source, source_id) —
/// i.e. a deleted episode the source is re-presenting — returns
/// (0, Tombstoned) and touches nothing.
pub fn upsert_episode(conn: &Connection, ep: &Episode) -> Result<(i64, IngestOutcome)> {
    let tombstoned: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM episode_tombstone WHERE source = ?1 AND source_id = ?2",
        params![ep.source, ep.source_id],
        |r| r.get(0),
    )?;
    if tombstoned {
        return Ok((0, IngestOutcome::Tombstoned));
    }

    let hash = content_hash(&ep.body);

    let existing: Option<(i64, String)> = conn
        .query_row(
            "SELECT id, content_hash FROM episode WHERE source = ?1 AND source_id = ?2",
            params![ep.source, ep.source_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;

    match existing {
        Some((id, old_hash)) if old_hash == hash => Ok((id, IngestOutcome::Unchanged)),
        Some((id, _)) => {
            conn.execute(
                "UPDATE episode SET body = ?2, occurred_at = ?3, occurred_end = ?4,
                        content_hash = ?5, lat = ?6, lon = ?7, location = ?8,
                        sensitivity = ?9, meta = ?10, source_ref = ?11
                 WHERE id = ?1",
                params![
                    id,
                    ep.body,
                    ep.occurred_at,
                    ep.occurred_end,
                    hash,
                    ep.lat,
                    ep.lon,
                    ep.location,
                    ep.sensitivity,
                    ep.meta.as_ref().map(|m| m.to_string()),
                    ep.source_ref,
                ],
            )?;
            // Content changed: cached embedding and enrichment are stale.
            conn.execute("DELETE FROM vec_episode WHERE episode_id = ?1", params![id])?;
            conn.execute(
                "DELETE FROM episode_enrichment WHERE episode_id = ?1",
                params![id],
            )?;
            Ok((id, IngestOutcome::Updated))
        }
        None => {
            conn.execute(
                "INSERT INTO episode (uid, source, source_id, source_ref, body, occurred_at,
                                      occurred_end, content_hash, lat, lon, location,
                                      sensitivity, scope_id, meta)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
                params![
                    new_uid(),
                    ep.source,
                    ep.source_id,
                    ep.source_ref,
                    ep.body,
                    ep.occurred_at,
                    ep.occurred_end,
                    hash,
                    ep.lat,
                    ep.lon,
                    ep.location,
                    ep.sensitivity,
                    ep.scope_id,
                    ep.meta.as_ref().map(|m| m.to_string()),
                ],
            )?;
            Ok((conn.last_insert_rowid(), IngestOutcome::Inserted))
        }
    }
}

pub fn get_episode(conn: &Connection, id: i64) -> Result<Option<Episode>> {
    Ok(conn
        .query_row(
            "SELECT * FROM episode WHERE id = ?1",
            params![id],
            row_to_episode,
        )
        .optional()?)
}

pub fn get_episode_by_uid(conn: &Connection, uid: &str) -> Result<Option<Episode>> {
    Ok(conn
        .query_row(
            "SELECT * FROM episode WHERE uid = ?1",
            params![uid],
            row_to_episode,
        )
        .optional()?)
}

// ─── Mentions ────────────────────────────────────────────────────────────────

/// Record that an episode mentions a node. The M:N substrate for co-occurrence,
/// salience, and person-filtered search (§4.1).
pub fn add_mention(
    conn: &Connection,
    episode_id: i64,
    node_id: &str,
    extractor: &str,
    confidence: f64,
) -> Result<()> {
    conn.execute(
        "INSERT INTO mention (episode_id, node_id, extractor, confidence)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(episode_id, node_id) DO UPDATE SET
             confidence = MAX(confidence, excluded.confidence)",
        params![episode_id, node_id, extractor, confidence],
    )?;
    Ok(())
}

/// Which sources actually say anything about this node, and how much —
/// with the newest and oldest, so a caller can tell live coverage from a
/// stream that stopped years ago.
///
/// The input to building a *vantage*: two readers can only be
/// independent witnesses if each is given a source that genuinely
/// covers the subject, and asking a source that has nothing produces a
/// confident "I don't know" that reads like a finding. Completes what
/// the rollup starts — it already collapses sources into per-channel
/// recency (`last_meeting_at` from calendar, `last_spoken_at` from Bee),
/// which is the same question asked coarsely.
pub fn source_coverage(
    conn: &Connection,
    node_id: &str,
) -> Result<Vec<(String, i64, String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.source, COUNT(*), MIN(e.occurred_at), MAX(e.occurred_at)
         FROM episode e JOIN mention m ON m.episode_id = e.id
         WHERE m.node_id = ?1
         GROUP BY e.source
         ORDER BY COUNT(*) DESC",
    )?;
    let rows = stmt
        .query_map(params![node_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

pub fn episodes_for_node(conn: &Connection, node_id: &str, limit: i64) -> Result<Vec<Episode>> {
    let mut stmt = conn.prepare_cached(
        "SELECT e.* FROM episode e
         JOIN mention m ON m.episode_id = e.id
         WHERE m.node_id = ?1
         ORDER BY e.occurred_at DESC LIMIT ?2",
    )?;
    let eps = stmt
        .query_map(params![node_id, limit], row_to_episode)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(eps)
}

/// Every (name, node, weak) the scanner may match, with the ambiguity guard
/// already applied. Shared by the live linker and the reconcile pass so a
/// backfill cannot judge against a different vocabulary than the one that
/// made the mentions.
///
/// `weak` marks a match reachable only through a bare first name. That is
/// the one alias kind that says nothing about *which* person is meant, and
/// it is the exact mechanism behind the 2026-08-24 repair: a student seen
/// once on a calendar invitation carried the alias "marisol", so a
/// thousand kitchen conversations about somebody's toddler landed on her
/// node — silently, because a first name held by exactly ONE node passes
/// the ambiguity guard. Canonical names stay strong however short they
/// are; matching "flowmail" or "tidelab" is distinctive, not a guess.
/// MAX(weak) is the conservative fold: a name reachable both ways is
/// treated as the weaker of the two.
fn alias_pairs(conn: &Connection) -> Result<Vec<(String, String, bool)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT name, MIN(nid), MAX(weak) FROM (
             SELECT a.alias AS name, a.node_id AS nid, a.source = 'firstname' AS weak
             FROM node_alias a
             UNION ALL
             SELECT n.canonical_name, n.id, 0 FROM nodes n
             WHERE n.node_type NOT IN ('event','event_series','document','artifact')
         ) WHERE length(name) >= 3
         GROUP BY name HAVING COUNT(DISTINCT nid) = 1",
    )?;
    let pairs: Vec<(String, String, bool)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(pairs)
}

/// Whole-word containment. Shared for the same reason as `alias_pairs`:
/// a reconcile that matched differently from the linker would retract
/// mentions the live path would still make.
fn appears_in(body_lower: &str, alias: &str) -> bool {
    let is_boundary = |c: Option<char>| c.is_none_or(|c| !c.is_alphanumeric());
    let mut start = 0;
    while let Some(pos) = body_lower[start..].find(alias) {
        let abs = start + pos;
        let before = body_lower[..abs].chars().next_back();
        let after = body_lower[abs + alias.len()..].chars().next();
        if is_boundary(before) && is_boundary(after) {
            return true;
        }
        start = abs + alias.len();
    }
    false
}

/// Cheap alias-scan linker: match known `node_alias` values against an episode
/// body (word-boundary, case-insensitive) and record mentions. This is what
/// makes calendar-seeded aliases pay off for name-only sources like Bee (§5.1).
pub fn link_by_alias_scan(conn: &Connection, episode_id: i64, body: &str) -> Result<usize> {
    let body_lower = body.to_lowercase();

    // Aliases of length < 3 (initials) match too promiscuously, and aliases
    // mapping to MULTIPLE nodes (a shared first name — two Victors) must not
    // auto-link at all: crediting every candidate inflates all of them
    // identically. Ambiguity is the disambiguation envelope's job (§8.1),
    // not the linker's.
    //
    // Canonical NAMES are scanned too, not just the alias table: project/
    // org/topic nodes never get alias rows (the alias backfill is persons-
    // only), so text mentions of "flowmail" produced no mention edges and
    // entity-filtered retrieval silently excluded those episodes. Event/
    // document names stay out — matching meeting titles in prose is noise.
    // `weak` marks a match reachable only through a bare first name. That is
    // the one alias kind that says nothing about *which* person is meant,
    // and it is the exact mechanism behind the 2026-08-24 repair: a student
    // seen once on a calendar invitation carried the alias "marisol", so a
    // thousand kitchen conversations about somebody's toddler landed on her
    // node — silently, because a first name held by exactly ONE node passes
    // the ambiguity guard below. Canonical names stay strong however short
    // they are; matching "flowmail" or "tidelab" is distinctive, not a
    // guess. MAX(weak) is the conservative fold: a name reachable both ways
    // is treated as the weaker of the two.
    let pairs = alias_pairs(conn)?;

    let appears = |alias: &str| appears_in(&body_lower, alias);

    let verdict = alias_verdict(conn, episode_id, &pairs, &appears)?;
    let n = verdict.keep.len();
    for node_id in &verdict.keep {
        add_mention(conn, episode_id, node_id, "alias", 0.8)?;
    }
    for (alias, node_id) in &verdict.refuse {
        // Refused, and recorded. A first name that keeps appearing and can
        // never be corroborated is not noise — it is a person the graph has
        // no node for, which is the only cheap signal there is for an entity
        // that is missing rather than wrong.
        conn.execute(
            "INSERT OR IGNORE INTO unlinked_mention (alias, node_id, episode_id)
             VALUES (?1, ?2, ?3)",
            params![alias, node_id, episode_id],
        )?;
    }
    Ok(n)
}

/// What the linker believes about one episode: which nodes it links, and
/// which weak matches it refuses.
pub struct AliasVerdict {
    pub keep: Vec<String>,
    /// (alias text, node it would have linked to)
    pub refuse: Vec<(String, String)>,
}

/// The linker's decision, factored out so that ADDING mentions and
/// RECONCILING existing ones cannot disagree about what it believes. Two
/// implementations of "should this link" is how a backfill ends up
/// retracting things the live path would still make.
///
/// Two passes, and the order is the whole mechanism. Strong matches commit
/// first and become the context a weak match is judged against:
/// "Marisol" in an episode that already mentions Avery, Ingrid and Wren
/// means the daughter; the same word in an episode with no connection to
/// her means nothing anyone can act on.
fn alias_verdict(
    conn: &Connection,
    episode_id: i64,
    pairs: &[(String, String, bool)],
    appears: &dyn Fn(&str) -> bool,
) -> Result<AliasVerdict> {
    let mut keep: Vec<String> = Vec::new();
    for (alias, node_id, weak) in pairs {
        if !*weak && appears(alias) {
            keep.push(node_id.clone());
        }
    }
    let strong = keep.clone();
    let mut refuse = Vec::new();
    for (alias, node_id, weak) in pairs {
        if !*weak || !appears(alias) {
            continue;
        }
        if corroborates(conn, episode_id, node_id, &strong)? {
            keep.push(node_id.clone());
        } else {
            refuse.push((alias.clone(), node_id.clone()));
        }
    }
    Ok(AliasVerdict { keep, refuse })
}

/// Re-judge the alias mentions already on file against the corroboration
/// rule, and report (or retract) the ones it would no longer make.
///
/// The gate guards new links; every mention made before it existed was
/// made under the old rule, which committed any unambiguous alias match.
/// 18,720 of them on this graph — including the thousand that put one
/// person's decade onto another person's node. A gate that only applies
/// going forward leaves the damage it was written for exactly where it is.
///
/// **Reporting is the default and `apply` is opt-in**, the opposite of
/// `promote_human_names`. That verb's `--dry-run` defaults to false and it
/// rewrote seven nodes for somebody who ran it expecting a survey; a pass
/// that can retract thousands of mentions should not be able to do that.
///
/// One alias mention the reconcile would no longer make.
pub struct Retraction {
    pub alias: String,
    pub node_id: String,
    pub episode_id: i64,
}

/// Returns (episodes examined, mentions that would be or were retracted).
pub fn relink_alias_mentions(
    conn: &Connection,
    apply: bool,
    limit: Option<i64>,
) -> Result<(usize, Vec<Retraction>)> {
    let mut stmt = conn.prepare(
        "SELECT DISTINCT e.id, e.body FROM episode e
         JOIN mention m ON m.episode_id = e.id AND m.extractor = 'alias'
         ORDER BY e.id LIMIT ?1",
    )?;
    let episodes: Vec<(i64, String)> = stmt
        .query_map(params![limit.unwrap_or(i64::MAX)], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<std::result::Result<_, _>>()?;

    let pairs = alias_pairs(conn)?;
    let mut retracted = Vec::new();
    for (episode_id, body) in &episodes {
        let body_lower = body.to_lowercase();
        let appears = |alias: &str| appears_in(&body_lower, alias);
        let verdict = alias_verdict(conn, *episode_id, &pairs, &appears)?;
        for (alias, node_id) in verdict.refuse {
            // Only retract what is actually on file as an alias mention.
            // The verdict speaks about what the linker WOULD do; a node it
            // refuses that was never linked needs no retraction, and
            // counting it would inflate the report.
            let present: bool = conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM mention
                  WHERE episode_id = ?1 AND node_id = ?2 AND extractor = 'alias')",
                params![episode_id, node_id],
                |r| r.get(0),
            )?;
            if !present {
                continue;
            }
            if apply {
                conn.execute(
                    "DELETE FROM mention WHERE episode_id = ?1 AND node_id = ?2
                     AND extractor = 'alias'",
                    params![episode_id, node_id],
                )?;
                conn.execute(
                    "INSERT OR IGNORE INTO unlinked_mention (alias, node_id, episode_id)
                     VALUES (?1, ?2, ?3)",
                    params![alias, node_id, episode_id],
                )?;
            }
            retracted.push(Retraction {
                alias,
                node_id,
                episode_id: *episode_id,
            });
        }
    }
    Ok((episodes.len(), retracted))
}

/// Does anything about this episode support a bare-first-name match?
///
/// Two signals, either sufficient, both answerable from data already here:
///
/// - **Company.** The episode already mentions someone this node has a live
///   fact with. Everything the graph knows about who goes with whom is in
///   that table, and it is what separates a daughter from a stranger who
///   shares her name.
/// - **Familiarity.** The node has been mentioned before from this kind of
///   source. Somebody who turns up in your conversations has turned up
///   there before; somebody known only from one calendar invitation has not.
///
/// Deliberately permissive — one signal is enough. The cost of a false
/// negative is an unlinked mention, which is recoverable and now recorded;
/// the cost of a false positive is a decade of somebody's life filed under
/// the wrong person, which took a day to undo.
fn corroborates(
    conn: &Connection,
    episode_id: i64,
    node_id: &str,
    linked: &[String],
) -> Result<bool> {
    if !linked.is_empty() {
        let list = linked
            .iter()
            .map(|id| format!("'{}'", id.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",");
        let company: bool = conn.query_row(
            &format!(
                "SELECT EXISTS (SELECT 1 FROM fact
                  WHERE valid_to IS NULL
                    AND ((subject_id = ?1 AND object_id IN ({list}))
                      OR (object_id = ?1 AND subject_id IN ({list}))))"
            ),
            params![node_id],
            |r| r.get(0),
        )?;
        if company {
            return Ok(true);
        }
    }
    let familiar: bool = conn.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM mention m
             JOIN episode e  ON e.id = m.episode_id
             JOIN episode me ON me.id = ?2
             WHERE m.node_id = ?1 AND m.episode_id <> ?2 AND e.source = me.source)",
        params![node_id, episode_id],
        |r| r.get(0),
    )?;
    Ok(familiar)
}

// ─── Annotations ─────────────────────────────────────────────────────────────

#[derive(Debug, Clone, serde::Serialize)]
pub struct Annotation {
    pub id: i64,
    pub kind: String, // tag | note
    pub body: String,
    pub created_at: String,
}

/// Attach a tag or note to an episode. Tags are canonicalized (lowercase,
/// trimmed) so `#Recommendation` and `recommendation` collide; notes keep
/// their text as typed. Returns false when the annotation already exists.
pub fn annotate_episode(
    conn: &Connection,
    episode_id: i64,
    kind: &str,
    body: &str,
) -> Result<bool> {
    let body = match kind {
        "tag" => body.trim().trim_start_matches('#').to_lowercase(),
        "note" => body.trim().to_string(),
        other => {
            return Err(crate::error::Error::Other(format!(
                "unknown annotation kind '{other}'"
            )))
        }
    };
    if body.is_empty() {
        return Err(crate::error::Error::Other("empty annotation".into()));
    }
    let n = conn.execute(
        "INSERT OR IGNORE INTO episode_annotation (episode_id, kind, body) VALUES (?1, ?2, ?3)",
        params![episode_id, kind, body],
    )?;
    Ok(n > 0)
}

pub fn annotations_for(conn: &Connection, episode_id: i64) -> Result<Vec<Annotation>> {
    let mut stmt = conn.prepare_cached(
        "SELECT id, kind, body, created_at FROM episode_annotation
         WHERE episode_id = ?1 ORDER BY kind DESC, created_at ASC", // tags before notes
    )?;
    let anns = stmt
        .query_map(params![episode_id], |r| {
            Ok(Annotation {
                id: r.get(0)?,
                kind: r.get(1)?,
                body: r.get(2)?,
                created_at: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(anns)
}

/// Episode ids carrying a given tag — the candidate set for the router's
/// `#tag` filter (§8.1 filter-first, same collapse as the entity filter).
pub fn episode_ids_with_tag(conn: &Connection, tag: &str) -> Result<Vec<i64>> {
    let tag = tag.trim().trim_start_matches('#').to_lowercase();
    let mut stmt = conn.prepare_cached(
        "SELECT episode_id FROM episode_annotation WHERE kind = 'tag' AND body = ?1",
    )?;
    let ids = stmt
        .query_map(params![tag], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
}

/// Just the tags on an episode (annotations minus notes), for surfacing on
/// search hits.
pub fn tags_for(conn: &Connection, episode_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "SELECT body FROM episode_annotation
         WHERE episode_id = ?1 AND kind = 'tag' ORDER BY body",
    )?;
    let tags = stmt
        .query_map(params![episode_id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(tags)
}

/// Is this tag in the vocabulary (i.e. applied to at least one episode)?
/// The router only treats `#foo` as a filter when this is true — otherwise
/// the token stays in the text query, so `#papers`-style Slack channel
/// references still search instead of matching nothing.
pub fn tag_exists(conn: &Connection, tag: &str) -> Result<bool> {
    let tag = tag.trim().trim_start_matches('#').to_lowercase();
    let n: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM episode_annotation WHERE kind = 'tag' AND body = ?1)",
        params![tag],
        |r| r.get(0),
    )?;
    Ok(n != 0)
}

/// The tag vocabulary: every tag in use with its episode count, most-used
/// first (ties newest-first is not guaranteed; secondary order is the tag).
pub fn list_tags(conn: &Connection) -> Result<Vec<(String, i64)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT body, COUNT(*) FROM episode_annotation
         WHERE kind = 'tag' GROUP BY body ORDER BY COUNT(*) DESC, body",
    )?;
    let tags = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(tags)
}

/// Episodes carrying a given tag, newest first — the revisit surface
/// (`pkg episodes --tag recommendation`).
pub fn episodes_by_tag(conn: &Connection, tag: &str, limit: i64) -> Result<Vec<Episode>> {
    let tag = tag.trim().trim_start_matches('#').to_lowercase();
    let mut stmt = conn.prepare_cached(
        "SELECT e.* FROM episode e
         JOIN episode_annotation a ON a.episode_id = e.id
         WHERE a.kind = 'tag' AND a.body = ?1
         ORDER BY e.occurred_at DESC LIMIT ?2",
    )?;
    let eps = stmt
        .query_map(params![tag, limit], row_to_episode)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(eps)
}

/// True redaction (§10): purge an episode plus everything derived from it —
/// mentions, embeddings, FTS rows (via trigger), enrichment, and facts whose
/// provenance is this episode. The one sanctioned delete in a bi-temporal store.
/// §10 tiers, weakest → strongest. Cycle order for UI surfaces.
pub const SENSITIVITY_TIERS: &[&str] = &["public", "personal", "private", "secret"];

/// Rank a tier for MAX-over-evidence comparisons. Unknown strings rank as
/// `secret` — fail closed: a typo'd tier hides a fact rather than leaking it.
pub fn sensitivity_rank(s: &str) -> usize {
    SENSITIVITY_TIERS
        .iter()
        .position(|t| *t == s)
        .unwrap_or(SENSITIVITY_TIERS.len() - 1)
}

pub fn set_sensitivity(conn: &Connection, episode_id: i64, sensitivity: &str) -> Result<()> {
    if !SENSITIVITY_TIERS.contains(&sensitivity) {
        return Err(crate::error::Error::Other(format!(
            "sensitivity '{sensitivity}' not in {SENSITIVITY_TIERS:?}"
        )));
    }
    conn.execute(
        "UPDATE episode SET sensitivity = ?2 WHERE id = ?1",
        params![episode_id, sensitivity],
    )?;
    Ok(())
}

// ─── Undo (TUI deletes/edits only — `pkg redact` stays a true delete) ───────

fn value_to_json(v: rusqlite::types::ValueRef<'_>) -> serde_json::Value {
    use rusqlite::types::ValueRef::*;
    match v {
        Null => serde_json::Value::Null,
        Integer(i) => serde_json::json!(i),
        Real(f) => serde_json::json!(f),
        Text(t) => serde_json::Value::String(String::from_utf8_lossy(t).into_owned()),
        Blob(b) => {
            serde_json::json!({ "_hex": b.iter().map(|x| format!("{x:02x}")).collect::<String>() })
        }
    }
}

fn json_to_sql(v: &serde_json::Value) -> rusqlite::types::Value {
    use rusqlite::types::Value::*;
    match v {
        serde_json::Value::Null => Null,
        serde_json::Value::Bool(b) => Integer(*b as i64),
        serde_json::Value::Number(n) if n.is_i64() => Integer(n.as_i64().unwrap()),
        serde_json::Value::Number(n) => Real(n.as_f64().unwrap_or(0.0)),
        serde_json::Value::String(t) => Text(t.clone()),
        serde_json::Value::Object(o) => {
            if let Some(hex) = o.get("_hex").and_then(|h| h.as_str()) {
                let bytes = (0..hex.len())
                    .step_by(2)
                    .filter_map(|i| u8::from_str_radix(&hex[i..(i + 2).min(hex.len())], 16).ok())
                    .collect();
                Blob(bytes)
            } else {
                Text(v.to_string())
            }
        }
        other => Text(other.to_string()),
    }
}

fn dump_rows(conn: &Connection, sql: &str, id: i64) -> Result<Vec<Vec<serde_json::Value>>> {
    let mut stmt = conn.prepare(sql)?;
    let ncols = stmt.column_count();
    let rows = stmt
        .query_map(params![id], |r| {
            let mut row = Vec::with_capacity(ncols);
            for i in 0..ncols {
                row.push(value_to_json(r.get_ref(i)?));
            }
            Ok(row)
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

fn restore_rows(
    conn: &Connection,
    table: &str,
    cols: &str,
    rows: &serde_json::Value,
) -> Result<usize> {
    let Some(rows) = rows.as_array() else {
        return Ok(0);
    };
    let mut n = 0;
    for row in rows {
        let Some(vals) = row.as_array() else { continue };
        let placeholders = (1..=vals.len())
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let sql = format!("INSERT OR IGNORE INTO {table} ({cols}) VALUES ({placeholders})");
        let params: Vec<rusqlite::types::Value> = vals.iter().map(json_to_sql).collect();
        n += conn.execute(&sql, rusqlite::params_from_iter(params))?;
    }
    Ok(n)
}

const EPISODE_COLS: &str = "id, uid, source, source_id, source_ref, body, occurred_at, \
    occurred_end, ingested_at, content_hash, lat, lon, location, sensitivity, scope_id, meta";
const FACT_COLS: &str = "id, uid, subject_id, predicate, object_id, object_value, statement, \
    episode_id, valid_from, valid_to, ingested_at, invalidated_at, confidence, weight, \
    observation_count, extractor, tags";
const CAND_COLS: &str =
    "id, payload, status, proposed_by, episode_id, confidence, created_at, reviewed_at, reject_reason";
const MENTION_COLS: &str = "episode_id, node_id, extractor, confidence";
const ANN_COLS: &str = "id, episode_id, kind, body, created_at";

fn snapshot_episode_json(conn: &Connection, id: i64) -> Result<serde_json::Value> {
    Ok(serde_json::json!({
        "episode": dump_rows(conn, &format!("SELECT {EPISODE_COLS} FROM episode WHERE id = ?1"), id)?,
        "raw": dump_rows(conn, "SELECT episode_id, content FROM episode_raw WHERE episode_id = ?1", id)?,
        "mentions": dump_rows(conn, &format!("SELECT {MENTION_COLS} FROM mention WHERE episode_id = ?1"), id)?,
        "annotations": dump_rows(conn, &format!("SELECT {ANN_COLS} FROM episode_annotation WHERE episode_id = ?1"), id)?,
        "facts": dump_rows(conn, &format!("SELECT {FACT_COLS} FROM fact WHERE episode_id = ?1"), id)?,
        "candidates": dump_rows(conn, &format!("SELECT {CAND_COLS} FROM fact_candidate WHERE episode_id = ?1"), id)?,
    }))
}

/// Redact with an undo snapshot — the TUI's delete. `pkg redact` (privacy,
/// §10) calls [`redact_episode`] directly and leaves NO copy behind.
pub fn redact_episode_undoable(conn: &Connection, uid: &str) -> Result<bool> {
    let id: Option<i64> = conn
        .query_row("SELECT id FROM episode WHERE uid = ?1", params![uid], |r| {
            r.get(0)
        })
        .optional()?;
    let Some(id) = id else { return Ok(false) };
    let snapshot = snapshot_episode_json(conn, id)?;
    conn.execute(
        "INSERT INTO undo_log (action, ref_uid, snapshot) VALUES ('delete', ?1, ?2)",
        params![uid, snapshot.to_string()],
    )?;
    redact_episode(conn, uid)
}

/// Snapshot body+raw before an in-place edit, for undo.
pub fn snapshot_edit(conn: &Connection, id: i64) -> Result<()> {
    let uid: String =
        conn.query_row("SELECT uid FROM episode WHERE id = ?1", params![id], |r| {
            r.get(0)
        })?;
    let snapshot = serde_json::json!({
        "episode": dump_rows(conn, &format!("SELECT {EPISODE_COLS} FROM episode WHERE id = ?1"), id)?,
        "raw": dump_rows(conn, "SELECT episode_id, content FROM episode_raw WHERE episode_id = ?1", id)?,
    });
    conn.execute(
        "INSERT INTO undo_log (action, ref_uid, snapshot) VALUES ('edit', ?1, ?2)",
        params![uid, snapshot.to_string()],
    )?;
    Ok(())
}

/// Undo the most recent TUI delete/edit. Returns a description, or None if
/// the log is empty. Restored episodes re-embed in the next nightly.
pub fn undo_last(conn: &Connection) -> Result<Option<String>> {
    let row: Option<(i64, String, Option<String>, String)> = conn
        .query_row(
            "SELECT id, action, ref_uid, snapshot FROM undo_log ORDER BY id DESC LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
        )
        .optional()?;
    let Some((log_id, action, ref_uid, snapshot)) = row else {
        return Ok(None);
    };
    let snap: serde_json::Value = serde_json::from_str(&snapshot)
        .map_err(|e| crate::error::Error::Parse(format!("undo snapshot: {e}")))?;

    match action.as_str() {
        "delete" => {
            // Undoing the delete lifts its tombstone — re-ingest may resume.
            if let Some(row) = snap["episode"]
                .as_array()
                .and_then(|r| r.first())
                .and_then(|r| r.as_array())
            {
                if let (Some(source), Some(source_id)) = (
                    row.get(2).and_then(|v| v.as_str()),
                    row.get(3).and_then(|v| v.as_str()),
                ) {
                    conn.execute(
                        "DELETE FROM episode_tombstone WHERE source = ?1 AND source_id = ?2",
                        params![source, source_id],
                    )?;
                }
            }
            restore_rows(conn, "episode", EPISODE_COLS, &snap["episode"])?;
            restore_rows(conn, "episode_raw", "episode_id, content", &snap["raw"])?;
            restore_rows(conn, "mention", MENTION_COLS, &snap["mentions"])?;
            restore_rows(conn, "episode_annotation", ANN_COLS, &snap["annotations"])?;
            restore_rows(conn, "fact", FACT_COLS, &snap["facts"])?;
            restore_rows(conn, "fact_candidate", CAND_COLS, &snap["candidates"])?;
            // The restored facts' observation trail was cascade-deleted;
            // regenerate the founding 'asserted' rows (same shape as the
            // V010 backfill — later corroborations are not reconstructible).
            if let Some(id) = snap["episode"]
                .as_array()
                .and_then(|r| r.first())
                .and_then(|r| r.as_array())
                .and_then(|r| r.first())
                .and_then(|v| v.as_i64())
            {
                conn.execute(
                    "INSERT INTO fact_observation
                         (fact_id, episode_id, observed_at, kind, method, confidence)
                     SELECT f.id, f.episode_id, f.ingested_at, 'asserted',
                            COALESCE(f.extractor, 'unknown'), f.confidence
                     FROM fact f
                     WHERE f.episode_id = ?1
                       AND NOT EXISTS (SELECT 1 FROM fact_observation o
                                       WHERE o.fact_id = f.id)",
                    params![id],
                )?;
            }
        }
        "edit" => {
            // UPDATE in place — a delete/re-insert would cascade away
            // mentions and annotations that aren't in an edit snapshot.
            if let Some(row) = snap["episode"]
                .as_array()
                .and_then(|r| r.first())
                .and_then(|r| r.as_array())
            {
                // EPISODE_COLS order: …, body(5), occurred_at(6), occurred_end(7),
                // …, content_hash(9), lat(10), lon(11), location(12), sensitivity(13), …, meta(15)
                let get = |i: usize| json_to_sql(row.get(i).unwrap_or(&serde_json::Value::Null));
                let id: i64 = row.first().and_then(|v| v.as_i64()).unwrap_or(0);
                conn.execute(
                    "UPDATE episode SET body = ?2, occurred_at = ?3, occurred_end = ?4,
                            content_hash = ?5, lat = ?6, lon = ?7, location = ?8,
                            sensitivity = ?9, meta = ?10
                     WHERE id = ?1",
                    rusqlite::params![
                        id,
                        get(5),
                        get(6),
                        get(7),
                        get(9),
                        get(10),
                        get(11),
                        get(12),
                        get(13),
                        get(15)
                    ],
                )?;
                // Restored body: cached vector/enrichment are stale.
                conn.execute("DELETE FROM vec_episode WHERE episode_id = ?1", params![id])?;
                conn.execute(
                    "DELETE FROM episode_enrichment WHERE episode_id = ?1",
                    params![id],
                )?;
                if let Some(raw_row) = snap["raw"]
                    .as_array()
                    .and_then(|r| r.first())
                    .and_then(|r| r.as_array())
                {
                    if let Some(content) = raw_row.get(1).and_then(|v| v.as_str()) {
                        store_raw(conn, id, content)?;
                    }
                }
            }
        }
        _ => {}
    }
    conn.execute("DELETE FROM undo_log WHERE id = ?1", params![log_id])?;
    Ok(Some(format!(
        "restored {} of episode {}",
        if action == "delete" { "delete" } else { "edit" },
        ref_uid
            .as_deref()
            .map(|u| &u[..8.min(u.len())])
            .unwrap_or("?")
    )))
}

pub fn redact_episode(conn: &Connection, uid: &str) -> Result<bool> {
    let row: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, source, source_id FROM episode WHERE uid = ?1",
            params![uid],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((id, source, source_id)) = row else {
        return Ok(false);
    };

    // Tombstone first: whole-file sources (ICS, reflect, mbox) re-present
    // every item on every sync and would otherwise resurrect this episode.
    conn.execute(
        "INSERT OR IGNORE INTO episode_tombstone (source, source_id) VALUES (?1, ?2)",
        params![source, source_id],
    )?;

    conn.execute("DELETE FROM fact WHERE episode_id = ?1", params![id])?;
    // Candidates extracted FROM this episode carry its content in their
    // payloads — true delete takes them too, reviewed or not.
    conn.execute(
        "DELETE FROM fact_candidate WHERE episode_id = ?1",
        params![id],
    )?;
    conn.execute("DELETE FROM vec_episode WHERE episode_id = ?1", params![id])?;
    // mention + episode_enrichment cascade; fts_episode handled by trigger.
    conn.execute("DELETE FROM episode WHERE id = ?1", params![id])?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    // ── the corroboration gate ────────────────────────────────────────────

    use crate::graph::{create_person, get_or_create_person};

    fn ep_from(conn: &rusqlite::Connection, source: &str, id: &str, body: &str) -> i64 {
        let ep = Episode {
            id: 0,
            uid: String::new(),
            source: source.into(),
            source_id: id.into(),
            source_ref: None,
            body: body.into(),
            occurred_at: "2026-08-01 12:00:00".into(),
            occurred_end: None,
            ingested_at: String::new(),
            lat: None,
            lon: None,
            location: None,
            sensitivity: "personal".into(),
            scope_id: None,
            meta: None,
            raw: None,
        };
        upsert_episode(conn, &ep).unwrap().0
    }

    /// The 2026-08-24 conflation, reconstructed. A student seen once on a
    /// calendar invitation carries the first-name alias "marisol"; a
    /// kitchen conversation names her daughter. Before the gate this linked
    /// silently — the alias is held by exactly one node, so the ambiguity
    /// guard passed — and a thousand such episodes landed on the wrong
    /// person.
    #[test]
    fn a_bare_first_name_does_not_link_to_a_stranger_who_shares_it() {
        let conn = crate::db::open_memory().unwrap();
        let student = get_or_create_person(
            &conn,
            Some("marisol.b.farrow.27@ostrander.edu"),
            "Marisol B. Farrow",
            "llm",
        )
        .unwrap();
        // Her one appearance: a calendar event, a different source entirely.
        let cal = ep_from(&conn, "calendar.event", "advising", "Marisol Farrow");
        add_mention(&conn, cal, &student.id, "attendee", 1.0).unwrap();

        let talk = ep_from(
            &conn,
            "bee.conversation",
            "bath",
            "Bath time with Marisol and her sister.",
        );
        link_by_alias_scan(&conn, talk, "Bath time with Marisol and her sister.").unwrap();

        let landed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id = ?1 AND node_id = ?2",
                params![talk, student.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(landed, 0, "an uncorroborated first name must not link");

        // And the refusal is recorded, because a name nobody can place is
        // the only cheap signal that a person is missing.
        let recorded: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM unlinked_mention WHERE alias = 'marisol'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(recorded, 1);
    }

    /// Company: the same weak name links when the episode also names
    /// somebody this node is actually connected to.
    #[test]
    fn a_bare_first_name_links_when_the_episode_names_her_family() {
        let conn = crate::db::open_memory().unwrap();
        let avery = create_person(&conn, "Avery J Calder", "t").unwrap();
        let jo = create_person(&conn, "Marisol Calder", "t").unwrap();
        conn.execute(
            "INSERT INTO fact (uid, subject_id, predicate, object_id, statement, polarity,
                               confidence, observation_count, valid_from)
             VALUES (hex(randomblob(8)), ?1, 'related_to', ?2, 'family', 'positive', 1.0, 1,
                     datetime('now'))",
            params![avery.id, jo.id],
        )
        .unwrap();

        let body = "Avery J Calder made dinner while Marisol played.";
        let e = ep_from(&conn, "bee.conversation", "dinner", body);
        link_by_alias_scan(&conn, e, body).unwrap();

        let landed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id = ?1 AND node_id = ?2",
                params![e, jo.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            landed, 1,
            "a strong match in the same episode corroborates it"
        );
    }

    /// Familiarity: somebody who has turned up in this kind of source before
    /// links on a first name without needing company.
    #[test]
    fn a_bare_first_name_links_for_someone_already_seen_in_that_source() {
        let conn = crate::db::open_memory().unwrap();
        let emma = create_person(&conn, "Emma Calloway", "t").unwrap();
        let earlier = ep_from(&conn, "bee.conversation", "lab", "Emma Calloway came by.");
        add_mention(&conn, earlier, &emma.id, "alias", 0.8).unwrap();

        let body = "Emma mentioned the paper again.";
        let e = ep_from(&conn, "bee.conversation", "again", body);
        link_by_alias_scan(&conn, e, body).unwrap();

        let landed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id = ?1 AND node_id = ?2",
                params![e, emma.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(landed, 1);
    }

    /// The gate is aimed at first names and nothing else. A distinctive
    /// canonical name is not a guess about which person is meant, and
    /// requiring corroboration for it would break the case the scan was
    /// widened for — text mentions of a project by name.
    #[test]
    fn a_canonical_name_still_links_without_corroboration() {
        let conn = crate::db::open_memory().unwrap();
        let proj = crate::graph::create_node(&conn, "project", "flowmail", "t").unwrap();
        let body = "Spent the evening on flowmail.";
        let e = ep_from(&conn, "bee.conversation", "eve", body);
        link_by_alias_scan(&conn, e, body).unwrap();
        let landed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id = ?1 AND node_id = ?2",
                params![e, proj.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(landed, 1);
    }

    /// A multi-word alias is evidence about which person is meant, so it is
    /// strong however the node was created.
    #[test]
    fn a_full_name_alias_is_never_weak() {
        let conn = crate::db::open_memory().unwrap();
        let n = create_person(&conn, "Somebody Else", "t").unwrap();
        add_alias(&conn, &n.id, "Thalia P. Wheatley", "manual").unwrap();
        let body = "Met Thalia P. Wheatley today.";
        let e = ep_from(&conn, "bee.conversation", "met", body);
        link_by_alias_scan(&conn, e, body).unwrap();
        let landed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id = ?1 AND node_id = ?2",
                params![e, n.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(landed, 1);
    }

    use super::*;
    use crate::db::open_memory;
    use crate::graph::{add_alias, upsert_node, Node};

    #[test]
    fn test_redact_takes_candidates_and_sensitivity_validates() {
        let conn = open_memory().unwrap();
        let (id, _) = upsert_episode(&conn, &ep("kill-me", "junk reference note")).unwrap();
        let uid: String = conn
            .query_row("SELECT uid FROM episode WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();
        // A candidate extracted from this episode.
        conn.execute(
            "INSERT INTO fact_candidate (payload, proposed_by, episode_id)
             VALUES ('{\"statement\":\"junk\"}', 'llm', ?1)",
            params![id],
        )
        .unwrap();

        set_sensitivity(&conn, id, "private").unwrap();
        assert!(set_sensitivity(&conn, id, "nonsense").is_err());

        assert!(redact_episode(&conn, &uid).unwrap());
        let leftovers: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_candidate WHERE episode_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            leftovers, 0,
            "redact must take the episode's candidates with it"
        );
    }

    #[test]
    fn test_undo_restores_delete_and_edit() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        let (id, _) =
            upsert_episode(&conn, &ep("undo-me", "met with Nadia about the pilot")).unwrap();
        add_mention(&conn, id, "nadia", "manual", 1.0).unwrap();
        store_raw(&conn, id, "met with Nadia about the pilot").unwrap();
        annotate_episode(&conn, id, "tag", "pilot").unwrap();
        let uid: String = conn
            .query_row("SELECT uid FROM episode WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();

        // Delete → gone → Ctrl-Z → everything back.
        assert!(redact_episode_undoable(&conn, &uid).unwrap());
        assert!(get_episode(&conn, id).unwrap().is_none());
        let msg = undo_last(&conn).unwrap().expect("undo entry");
        assert!(msg.contains("delete"));
        let restored = get_episode(&conn, id).unwrap().expect("episode back");
        assert_eq!(restored.body, "met with Nadia about the pilot");
        let mentions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mentions, 1, "mention restored");
        let anns = annotations_for(&conn, id).unwrap();
        assert_eq!(anns.len(), 1, "annotation restored");
        assert!(has_raw(&conn, id).unwrap(), "raw restored");

        // Edit → undo → original body, mention survives.
        snapshot_edit(&conn, id).unwrap();
        let mut edited = restored.clone();
        edited.body = "totally rewritten".into();
        upsert_episode(&conn, &edited).unwrap();
        undo_last(&conn).unwrap().expect("edit undo");
        let back = get_episode(&conn, id).unwrap().unwrap();
        assert_eq!(back.body, "met with Nadia about the pilot");
        let mentions: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mentions, 1, "edit undo must not cascade mentions away");

        assert!(undo_last(&conn).unwrap().is_none(), "log consumed");
    }

    #[test]
    fn test_tombstone_blocks_reingest_until_lifted() {
        let conn = open_memory().unwrap();
        let (_, o) = upsert_episode(&conn, &ep("cal-1", "standup meeting")).unwrap();
        assert_eq!(o, IngestOutcome::Inserted);
        let uid: String = conn
            .query_row(
                "SELECT uid FROM episode WHERE source_id = 'cal-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        // Redact, then simulate the nightly re-presenting the same item.
        assert!(redact_episode(&conn, &uid).unwrap());
        let (id, o) = upsert_episode(&conn, &ep("cal-1", "standup meeting")).unwrap();
        assert_eq!(
            o,
            IngestOutcome::Tombstoned,
            "re-ingest must not resurrect a deleted episode"
        );
        assert_eq!(id, 0);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);

        // Lifting the tombstone (pkg tombstone rm) re-opens the door.
        conn.execute(
            "DELETE FROM episode_tombstone WHERE source = 'note' AND source_id = 'cal-1'",
            [],
        )
        .unwrap();
        let (_, o) = upsert_episode(&conn, &ep("cal-1", "standup meeting")).unwrap();
        assert_eq!(o, IngestOutcome::Inserted);
    }

    #[test]
    fn test_undo_delete_lifts_tombstone() {
        let conn = open_memory().unwrap();
        let (id, _) = upsert_episode(&conn, &ep("undo-ts", "a note")).unwrap();
        let uid: String = conn
            .query_row("SELECT uid FROM episode WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();

        assert!(redact_episode_undoable(&conn, &uid).unwrap());
        let (_, o) = upsert_episode(&conn, &ep("undo-ts", "a note")).unwrap();
        assert_eq!(o, IngestOutcome::Tombstoned);

        undo_last(&conn).unwrap().expect("undo entry");
        assert!(
            get_episode(&conn, id).unwrap().is_some(),
            "episode restored"
        );
        let ts: i64 = conn
            .query_row("SELECT COUNT(*) FROM episode_tombstone", [], |r| r.get(0))
            .unwrap();
        assert_eq!(ts, 0, "undoing a delete must lift its tombstone");
        // Re-ingest of the restored item is a normal unchanged upsert again.
        let (_, o) = upsert_episode(&conn, &ep("undo-ts", "a note")).unwrap();
        assert_eq!(o, IngestOutcome::Unchanged);
    }

    fn ep(source_id: &str, body: &str) -> Episode {
        Episode {
            id: 0,
            uid: String::new(),
            source: "note".into(),
            source_id: source_id.into(),
            source_ref: None,
            body: body.into(),
            occurred_at: "2026-08-01 12:00:00".into(),
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
    fn test_idempotent_ingest() {
        let conn = open_memory().unwrap();
        let e = ep("n1", "met with Nadia about pilot data");

        let (id1, o1) = upsert_episode(&conn, &e).unwrap();
        assert_eq!(o1, IngestOutcome::Inserted);
        let (id2, o2) = upsert_episode(&conn, &e).unwrap();
        assert_eq!(o2, IngestOutcome::Unchanged);
        assert_eq!(id1, id2);

        let mut changed = e.clone();
        changed.body = "met with Nadia about pilot data and Aim 2".into();
        let (id3, o3) = upsert_episode(&conn, &changed).unwrap();
        assert_eq!(o3, IngestOutcome::Updated);
        assert_eq!(id1, id3);

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn test_fts_stays_in_sync() {
        let conn = open_memory().unwrap();
        let (id, _) = upsert_episode(&conn, &ep("n1", "quarterly budget review")).unwrap();

        let hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_episode WHERE fts_episode MATCH 'budget'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(hits, 1);

        // Update flows through the trigger.
        let mut changed = ep("n1", "annual planning session");
        let (_, o) = upsert_episode(&conn, &changed).unwrap();
        assert_eq!(o, IngestOutcome::Updated);
        changed.body = "annual planning session".into();

        let old_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_episode WHERE fts_episode MATCH 'budget'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(old_hits, 0);
        let new_hits: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_episode WHERE fts_episode MATCH 'planning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(new_hits, 1);
        let _ = id;
    }

    #[test]
    fn test_alias_scan_linker() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia Petrova")).unwrap();
        add_alias(&conn, "nadia", "Nadia", "attendee").unwrap();

        let (id, _) = upsert_episode(&conn, &ep("b1", "Talked to Nadia about the pilot.")).unwrap();
        let n = link_by_alias_scan(&conn, id, "Talked to Nadia about the pilot.").unwrap();
        assert_eq!(n, 1);

        let eps = episodes_for_node(&conn, "nadia", 10).unwrap();
        assert_eq!(eps.len(), 1);

        // No substring false positives: "Nadiania" should not match alias "nadia".
        let (id2, _) = upsert_episode(&conn, &ep("b2", "Visited Nadiania resort")).unwrap();
        let n2 = link_by_alias_scan(&conn, id2, "Visited Nadiania resort").unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn test_redaction_purges_derived_data() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("w", "person", "Nadia")).unwrap();
        let (id, _) = upsert_episode(&conn, &ep("s1", "secret conversation")).unwrap();
        add_mention(&conn, id, "w", "manual", 1.0).unwrap();
        crate::fact::assert_fact(
            &conn,
            "w",
            "about",
            None,
            Some("secret"),
            "Nadia discussed a secret",
            Some(id),
            None,
            0.8,
            "manual",
        )
        .unwrap();

        let uid: String = conn
            .query_row("SELECT uid FROM episode WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(redact_episode(&conn, &uid).unwrap());

        let n_ep: i64 = conn
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        let n_mention: i64 = conn
            .query_row("SELECT COUNT(*) FROM mention", [], |r| r.get(0))
            .unwrap();
        let n_fact: i64 = conn
            .query_row("SELECT COUNT(*) FROM fact", [], |r| r.get(0))
            .unwrap();
        let n_fts: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fts_episode WHERE fts_episode MATCH 'secret'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!((n_ep, n_mention, n_fact, n_fts), (0, 0, 0, 0));
    }

    #[test]
    fn test_annotations_tag_note_and_query() {
        let conn = open_memory().unwrap();
        let (id, _) = upsert_episode(&conn, &ep("a1", "Rik recommended nnU-Net")).unwrap();
        let (other, _) = upsert_episode(&conn, &ep("a2", "unrelated")).unwrap();

        // Tags canonicalize (# stripped, lowercased) and dedupe.
        assert!(annotate_episode(&conn, id, "tag", "#Recommendation").unwrap());
        assert!(!annotate_episode(&conn, id, "tag", "recommendation").unwrap());
        assert!(annotate_episode(&conn, id, "note", "try on the fMRI segmentation task").unwrap());
        assert!(annotate_episode(&conn, other, "tag", "software").unwrap());
        assert!(annotate_episode(&conn, id, "bookmark", "x").is_err());
        assert!(annotate_episode(&conn, id, "note", "   ").is_err());

        let anns = annotations_for(&conn, id).unwrap();
        assert_eq!(anns.len(), 2);
        assert_eq!(
            (anns[0].kind.as_str(), anns[0].body.as_str()),
            ("tag", "recommendation")
        );
        assert_eq!(anns[1].kind, "note");

        let hits = episodes_by_tag(&conn, "Recommendation", 10).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, id);

        // Redaction takes annotations with it (cascade).
        let uid: String = conn
            .query_row("SELECT uid FROM episode WHERE id = ?1", params![id], |r| {
                r.get(0)
            })
            .unwrap();
        redact_episode(&conn, &uid).unwrap();
        let n: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM episode_annotation WHERE episode_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(n, 0);
    }
}
