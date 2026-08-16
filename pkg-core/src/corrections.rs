//! D3 wiring, pkg side — the error contract's data-repair duties.
//!
//! mecha's session-end distiller ships every correction as an episode
//! whose meta carries `corrections: [{fact_uid?, wrong?, right?, about?}]`
//! (the one distiller change PLAN.md requires). This module is what those
//! arrays hit. One correction is feature-level feedback — it says
//! something about the *generator*, not just the instance — so each one
//! does four things, all automatic:
//!
//! 1. **Supersede the wrong fact** (bi-temporal close, history kept) and
//!    record a `corrected` observation — V011's posterior then falls.
//! 2. **Stage the replacement** (`right` present) as a fact_candidate at
//!    confidence 0.95 — the queue orders by confidence, so priority
//!    routing needs no new mechanics. A pure rejection (no `right`)
//!    writes the negation instead: rejection memory, stops re-asking.
//! 3. **Demote the producing class** on the autonomy ladder — in-use
//!    corrections are the primary demotion signal, ahead of review
//!    verdicts (a verdict judges plausibility; a correction judges
//!    against reality).
//! 4. **Emit the blast-radius sweep**: the class's other live facts
//!    become `sweep_target` rows in event_log — a probe target list for
//!    the Wave-3 Selector, generated for free. Deliberately NOT a
//!    confidence hit: "probably also wrong" is a hypothesis, not
//!    evidence, and must not poison a class without verification.
//!
//! A correction that cannot be resolved to exactly one live fact is
//! never guessed at: it lands in the review queue as its own candidate
//! (proposed_by `correction:unresolved`) so a human closes the loop.

use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::error::Result;
use crate::fact::{self, Fact, ProposedFact};

/// Max sweep_target rows per correction — a runaway class still yields a
/// bounded probe list.
const SWEEP_CAP: i64 = 50;

/// One correction from an episode's `meta.corrections` array. `fact_uid`
/// (the pack carried it — packs are provenance) beats text matching.
#[derive(Debug, Clone, Deserialize)]
pub struct Correction {
    #[serde(default)]
    pub fact_uid: Option<String>,
    /// The wrong claim, as text — fallback resolver when no uid.
    #[serde(default)]
    pub wrong: Option<String>,
    /// The corrected claim; absent = pure rejection (negation).
    #[serde(default)]
    pub right: Option<String>,
    /// Entity the correction is about — narrows text resolution.
    #[serde(default)]
    pub about: Option<String>,
}

#[derive(Debug, Default, Serialize)]
pub struct CorrectionSummary {
    pub processed: usize,
    pub superseded: usize,
    pub staged: usize,
    pub negated: usize,
    pub demoted: usize,
    pub sweep_targets: usize,
    pub unresolved: usize,
}

/// Process the `meta.corrections` array of one episode (no-op without
/// one). Idempotent per fact: an already-superseded target is counted
/// processed but touched no further, so a distiller retry cannot
/// double-repair.
pub fn process_episode(conn: &Connection, episode_id: i64) -> Result<CorrectionSummary> {
    let mut summary = CorrectionSummary::default();
    let meta: Option<String> = conn
        .query_row(
            "SELECT meta FROM episode WHERE id = ?1",
            params![episode_id],
            |r| r.get(0),
        )
        .optional()?
        .flatten();
    let Some(meta) = meta else { return Ok(summary) };
    let Ok(meta_v) = serde_json::from_str::<serde_json::Value>(&meta) else {
        return Ok(summary);
    };
    let Some(arr) = meta_v.get("corrections").and_then(|c| c.as_array()) else {
        return Ok(summary);
    };

    for c in arr {
        let Ok(c) = serde_json::from_value::<Correction>(c.clone()) else {
            summary.unresolved += 1;
            continue;
        };
        summary.processed += 1;
        match resolve_target(conn, &c)? {
            Target::Live(f) => apply(conn, episode_id, &c, &f, &mut summary)?,
            Target::AlreadyClosed => { /* retry of a processed correction */ }
            Target::Unresolved => {
                stage_unresolved(conn, episode_id, &c)?;
                summary.unresolved += 1;
            }
        }
    }

    // Mark processed so nightly backfill scans skip this episode. A
    // re-upsert overwrites the marker; per-fact idempotence above makes
    // the re-run harmless.
    let mut meta_v = meta_v;
    meta_v["corrections_processed_at"] = serde_json::Value::String(crate::ids::now());
    conn.execute(
        "UPDATE episode SET meta = ?2 WHERE id = ?1",
        params![episode_id, meta_v.to_string()],
    )?;
    Ok(summary)
}

/// Backfill scan: agent episodes carrying an unprocessed corrections
/// array (kg_upsert processes inline; this catches anything that arrived
/// another way, and re-runs after failures).
pub fn process_pending(conn: &Connection, limit: i64) -> Result<CorrectionSummary> {
    let ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM episode
             WHERE meta LIKE '%\"corrections\"%'
               AND meta NOT LIKE '%\"corrections_processed_at\"%'
             ORDER BY ingested_at ASC LIMIT ?1",
        )?;
        let ids = stmt
            .query_map(params![limit], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        ids
    };
    let mut total = CorrectionSummary::default();
    for id in ids {
        let s = process_episode(conn, id)?;
        total.processed += s.processed;
        total.superseded += s.superseded;
        total.staged += s.staged;
        total.negated += s.negated;
        total.demoted += s.demoted;
        total.sweep_targets += s.sweep_targets;
        total.unresolved += s.unresolved;
    }
    Ok(total)
}

enum Target {
    Live(Fact),
    AlreadyClosed,
    Unresolved,
}

/// uid first (packs carry provenance — the distiller should pass it);
/// else exact-statement match among live facts, narrowed by `about` when
/// it resolves to exactly one entity. Never guesses: 0 or >1 hits are
/// unresolved.
fn resolve_target(conn: &Connection, c: &Correction) -> Result<Target> {
    if let Some(uid) = &c.fact_uid {
        return Ok(match fact::get_fact_by_uid(conn, uid)? {
            Some(f) if f.invalidated_at.is_none() && f.valid_to.is_none() => Target::Live(f),
            Some(_) => Target::AlreadyClosed,
            None => Target::Unresolved,
        });
    }
    let Some(wrong) = c.wrong.as_deref().map(str::trim).filter(|w| !w.is_empty()) else {
        return Ok(Target::Unresolved);
    };
    let subject: Option<String> = match c.about.as_deref() {
        Some(about) => {
            let mut nodes = crate::graph::resolve_entity_all(conn, about)?;
            if nodes.len() == 1 {
                Some(nodes.remove(0).id)
            } else {
                None // ambiguous or unknown `about` — don't narrow wrongly
            }
        }
        None => None,
    };
    let mut stmt = conn.prepare_cached(
        "SELECT * FROM fact
         WHERE statement = ?1 COLLATE NOCASE
           AND (?2 IS NULL OR subject_id = ?2)",
    )?;
    let hits: Vec<Fact> = stmt
        .query_map(params![wrong, subject], fact::row_to_fact)?
        .collect::<std::result::Result<_, _>>()?;
    let (mut live, closed): (Vec<Fact>, Vec<Fact>) = hits
        .into_iter()
        .partition(|f| f.valid_to.is_none() && f.invalidated_at.is_none());
    Ok(match (live.len(), closed.len()) {
        (1, _) => Target::Live(live.remove(0)),
        // No live match but a closed one: a distiller retry of a
        // correction that already landed — not an unresolved mystery.
        (0, 1..) => Target::AlreadyClosed,
        _ => Target::Unresolved,
    })
}

/// The four duties, in order.
fn apply(
    conn: &Connection,
    episode_id: i64,
    c: &Correction,
    f: &Fact,
    summary: &mut CorrectionSummary,
) -> Result<()> {
    // 1. Corrected observation (drops the V011 posterior) + supersede.
    fact::record_observation(conn, f.id, Some(episode_id), "corrected", "user", None)?;
    fact::supersede_fact(conn, &f.uid, None)?;
    summary.superseded += 1;

    // 2. Replacement staged at queue-top confidence, or negation written.
    if let Some(right) = c.right.as_deref().map(str::trim).filter(|r| !r.is_empty()) {
        fact::propose_fact(
            conn,
            &ProposedFact {
                subject: f.subject_id.clone(),
                predicate: f.predicate.clone(),
                object: None,
                object_value: None,
                statement: right.to_string(),
                valid_from: None,
                confidence: Some(0.95),
                tags: None,
            },
            "correction",
            Some(episode_id),
        )?;
        summary.staged += 1;
    } else {
        fact::assert_negative_fact(
            conn,
            &f.subject_id,
            &f.predicate,
            f.object_id.as_deref(),
            f.object_value.as_deref(),
            &format!("It is not the case that: {}", f.statement),
            Some(episode_id),
            0.95,
            "correction",
        )?;
        summary.negated += 1;
    }

    // 3. Demote the producing class — in-use corrections outrank review
    //    verdicts as a demotion signal.
    if let Some(extractor) = f.extractor.as_deref().filter(|e| !e.is_empty()) {
        crate::ladder::demote_class(conn, extractor, &f.predicate, "in-use correction")?;
        summary.demoted += 1;
    }

    // 4. Blast-radius sweep: what else did this class produce? Probe
    //    targets only — no belief changes without verification.
    if let Some(extractor) = f.extractor.as_deref() {
        let mut stmt = conn.prepare_cached(
            "SELECT uid FROM fact_current
             WHERE extractor = ?1 AND predicate = ?2 AND uid != ?3
             LIMIT ?4",
        )?;
        let peers: Vec<String> = stmt
            .query_map(params![extractor, f.predicate, f.uid, SWEEP_CAP], |r| {
                r.get(0)
            })?
            .collect::<std::result::Result<_, _>>()?;
        for peer in &peers {
            let payload = serde_json::json!({
                "class": format!("{extractor}·{}", f.predicate),
                "trigger_fact": f.uid,
                "trigger_episode": episode_id,
            });
            crate::ledger::log_event(conn, "sweep_target", Some(peer), Some(&payload.to_string()))?;
        }
        summary.sweep_targets += peers.len();
    }

    let payload = serde_json::json!({
        "episode_id": episode_id,
        "action": if c.right.is_some() { "superseded+staged" } else { "superseded+negated" },
        "right": c.right,
    });
    crate::ledger::log_event(conn, "correction", Some(&f.uid), Some(&payload.to_string()))?;
    Ok(())
}

/// A correction we could not pin to one live fact goes to the human, as
/// a review-queue item carrying the whole correction — never silently
/// dropped, never guessed.
fn stage_unresolved(conn: &Connection, episode_id: i64, c: &Correction) -> Result<()> {
    let statement = format!(
        "UNRESOLVED CORRECTION — wrong: {} | right: {} | about: {}",
        c.wrong
            .as_deref()
            .unwrap_or(c.fact_uid.as_deref().unwrap_or("?")),
        c.right.as_deref().unwrap_or("(rejection)"),
        c.about.as_deref().unwrap_or("?"),
    );
    fact::propose_fact(
        conn,
        &ProposedFact {
            subject: c.about.clone().unwrap_or_else(|| "unknown".into()),
            predicate: "correction".into(),
            object: None,
            object_value: None,
            statement,
            valid_from: None,
            confidence: Some(0.94), // just under resolved corrections, above the flood
            tags: None,
        },
        "correction:unresolved",
        Some(episode_id),
    )?;
    crate::ledger::log_event(
        conn,
        "correction_unresolved",
        None,
        Some(
            &serde_json::json!({"episode_id": episode_id, "wrong": c.wrong, "about": c.about})
                .to_string(),
        ),
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{upsert_episode, Episode};
    use crate::fact::assert_fact;
    use crate::graph::{upsert_node, Node};
    use crate::ladder::{get_rung, Rung};

    fn agent_episode(conn: &Connection, corrections: serde_json::Value) -> i64 {
        upsert_episode(
            conn,
            &Episode {
                id: 0,
                uid: String::new(),
                source: "agent:mecha".into(),
                source_id: "sess-1".into(),
                source_ref: None,
                body: "session distillation".into(),
                occurred_at: crate::ids::now(),
                occurred_end: None,
                ingested_at: String::new(),
                lat: None,
                lon: None,
                location: None,
                sensitivity: "personal".into(),
                scope_id: None,
                meta: Some(serde_json::json!({ "corrections": corrections })),
                raw: None,
            },
        )
        .unwrap()
        .0
    }

    fn setup(conn: &Connection) -> String {
        upsert_node(conn, &Node::new("person-a", "person", "Ada")).unwrap();
        upsert_node(conn, &Node::new("org-x", "org", "X Corp")).unwrap();
        assert_fact(
            conn,
            "person-a",
            "works_at",
            Some("org-x"),
            None,
            "Ada works at X Corp",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap()
    }

    #[test]
    fn test_correction_supersedes_stages_demotes_sweeps() {
        let conn = open_memory().unwrap();
        let uid = setup(&conn);
        // A same-class peer that the sweep should target.
        upsert_node(&conn, &Node::new("person-b", "person", "Bo")).unwrap();
        let peer = assert_fact(
            &conn,
            "person-b",
            "works_at",
            Some("org-x"),
            None,
            "Bo works at X Corp",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        // Promote the class first so the demotion is observable.
        conn.execute(
            "INSERT INTO class_ledger (proposer, predicate, rung, streak)
             VALUES ('llm','works_at','trusted',0)
             ON CONFLICT(proposer, predicate) DO UPDATE SET rung='trusted'",
            [],
        )
        .unwrap();

        let eid = agent_episode(
            &conn,
            serde_json::json!([
                {"fact_uid": uid, "right": "Ada works at Y Labs"}
            ]),
        );
        let s = process_episode(&conn, eid).unwrap();
        assert_eq!(
            (s.processed, s.superseded, s.staged, s.negated),
            (1, 1, 1, 0)
        );

        // Superseded: no longer live.
        let f = fact::get_fact_by_uid(&conn, &uid).unwrap().unwrap();
        assert!(f.invalidated_at.is_some() && f.valid_to.is_some());
        // Corrected observation recorded.
        let corrected: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_observation WHERE fact_id=?1 AND kind='corrected'",
                params![f.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(corrected, 1);
        // Replacement staged at the top of the queue.
        let cands = fact::pending_candidates(&conn, 5).unwrap();
        assert_eq!(cands[0].proposed_by.as_deref(), Some("correction"));
        assert_eq!(cands[0].payload["statement"], "Ada works at Y Labs");
        // Class demoted from trusted to staged.
        assert_eq!(get_rung(&conn, "llm", "works_at").unwrap(), Rung::Staged);
        assert_eq!(s.demoted, 1);
        // Sweep targeted the peer, and only the peer.
        let targets: Vec<String> = conn
            .prepare("SELECT ref FROM event_log WHERE kind='sweep_target'")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(targets, vec![peer]);
    }

    #[test]
    fn test_pure_rejection_writes_negation() {
        let conn = open_memory().unwrap();
        let uid = setup(&conn);
        let eid = agent_episode(&conn, serde_json::json!([{"fact_uid": uid}]));
        let s = process_episode(&conn, eid).unwrap();
        assert_eq!((s.superseded, s.negated, s.staged), (1, 1, 0));
        let (n, pol): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), polarity FROM fact
             WHERE subject_id='person-a' AND predicate='works_at'
               AND polarity='negative' AND valid_to IS NULL",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!((n, pol.as_str()), (1, "negative"));
    }

    #[test]
    fn test_text_resolution_and_retry_idempotence() {
        let conn = open_memory().unwrap();
        setup(&conn);
        let eid = agent_episode(
            &conn,
            serde_json::json!([
                {"wrong": "ada works at x corp", "right": "Ada works at Y Labs", "about": "Ada"}
            ]),
        );
        let s = process_episode(&conn, eid).unwrap();
        assert_eq!(
            s.superseded, 1,
            "case-insensitive statement match, narrowed by about"
        );

        // A distiller retry re-processes the same correction: the target is
        // already closed, so nothing double-fires.
        let s2 = process_episode(&conn, eid).unwrap();
        assert_eq!((s2.superseded, s2.staged, s2.unresolved), (0, 0, 0));
        let cands = fact::pending_candidates(&conn, 10).unwrap();
        assert_eq!(cands.len(), 1, "no duplicate staged replacement");
    }

    #[test]
    fn test_unresolved_goes_to_review_not_guessed() {
        let conn = open_memory().unwrap();
        setup(&conn);
        // Two live facts share a statement shape; no `about` to narrow.
        upsert_node(&conn, &Node::new("person-b", "person", "Ada Jr")).unwrap();
        assert_fact(
            &conn,
            "person-b",
            "works_at",
            Some("org-x"),
            None,
            "Ada works at X Corp",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();

        let eid = agent_episode(
            &conn,
            serde_json::json!([
                {"wrong": "Ada works at X Corp", "right": "Ada works at Y Labs"}
            ]),
        );
        let s = process_episode(&conn, eid).unwrap();
        assert_eq!((s.superseded, s.unresolved), (0, 1), ">1 hit never guesses");
        let cands = fact::pending_candidates(&conn, 5).unwrap();
        assert_eq!(
            cands[0].proposed_by.as_deref(),
            Some("correction:unresolved")
        );
        assert!(cands[0].payload["statement"]
            .as_str()
            .unwrap()
            .contains("UNRESOLVED"));
    }

    #[test]
    fn test_pending_scan_marks_processed() {
        let conn = open_memory().unwrap();
        let uid = setup(&conn);
        let eid = agent_episode(&conn, serde_json::json!([{"fact_uid": uid}]));
        let s = process_pending(&conn, 10).unwrap();
        assert_eq!(s.superseded, 1);
        // Marker written: a second scan finds nothing.
        let s2 = process_pending(&conn, 10).unwrap();
        assert_eq!(s2.processed, 0);
        let meta: String = conn
            .query_row("SELECT meta FROM episode WHERE id=?1", params![eid], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(meta.contains("corrections_processed_at"));
    }
}
