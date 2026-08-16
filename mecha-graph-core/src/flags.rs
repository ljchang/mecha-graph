//! Pack flags — point-of-use surfacing (PLAN mechanism #4, doctrine D1).
//!
//! When retrieval notices a problem in what it is about to return, the
//! pack says so. The consumer is already engaged with that exact entity,
//! so the context-switch cost is zero — strictly cheaper than a nightly
//! batch question about a node the owner has to page back in.
//!
//! The division of labour is §1's: **pkg detects with provenance; the
//! model judges** whether to interrupt, ask, or silently note. No flag
//! ever changes what the pack contains — flags describe it.
//!
//! Three detectors, all deterministic SQL over what the pack serves
//! (pack-scoped IS the answer-changing criterion — a problem on a fact
//! nobody retrieved changes no answer):
//!
//! - `contradiction` — >1 live positive object on a single-valued
//!   predicate the pack touches (the `live_contradictions` rule, scoped).
//! - `denial` — a live negation coexists with a live positive on a
//!   (subject, predicate) the pack touches: contested state (V013).
//! - `staleness` — a served fact is past its predicate's half-life
//!   (λ from V013, last-observation clock from V010's fact_observation).
//!
//! `thin` deliberately stays OUT of the envelope: its definition ("few/no
//! facts *for the asked relation*") needs an asked-relation parser the
//! router doesn't have yet, and V009 already records descriptive `thin`
//! in query_log. A relation-blind thin flag would nag on ordinary packs.
//!
//! Budget: at most [`MAX_FLAGS_PER_PACK`], ranked by expected loss
//! (contradiction > denial > staleness). If every pack carried five,
//! the consuming agent goes noisy and the owner learns to ignore them.

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::Result;
use crate::router::ContextPack;

/// Flag budget per pack (D1: nag discipline).
pub const MAX_FLAGS_PER_PACK: usize = 2;

/// One surfaced problem, with enough provenance to act on it: every
/// referenced fact is dereferenceable by uid (the Verifier's contract).
#[derive(Debug, Clone, Serialize)]
pub struct PackFlag {
    /// contradiction | denial | staleness
    pub kind: String,
    pub subject_id: String,
    pub predicate: String,
    /// Human-readable, self-contained description.
    pub detail: String,
    /// The facts involved — served ones and their off-pack peers alike.
    pub fact_uids: Vec<String>,
}

/// A (subject, predicate) key the pack touches, with the serving fact.
struct PackKey {
    subject_id: String,
    predicate: String,
    uid: String,
}

/// Detect and attach flags to a finished pack (call after ranking and
/// truncation — flags describe what is actually served). `include_private`
/// mirrors retrieval: an off-pack peer fact above the caller's tier must
/// not launder its statement into a flag (the V008 lesson — hops don't
/// launder).
pub fn flag_pack(conn: &Connection, pack: &mut ContextPack, include_private: bool) -> Result<()> {
    let keys = pack_keys(conn, pack)?;
    if keys.is_empty() {
        return Ok(());
    }

    let mut flags: Vec<PackFlag> = vec![];
    let mut seen: std::collections::HashSet<(String, String, &str)> = Default::default();

    // Severity order — the truncation below keeps the head.
    for key in &keys {
        if seen.insert((
            key.subject_id.clone(),
            key.predicate.clone(),
            "contradiction",
        )) {
            if let Some(f) = detect_contradiction(conn, key, include_private)? {
                flags.push(f);
            }
        }
    }
    for key in &keys {
        if seen.insert((key.subject_id.clone(), key.predicate.clone(), "denial")) {
            if let Some(f) = detect_denial(conn, key, include_private)? {
                flags.push(f);
            }
        }
    }
    for key in &keys {
        if let Some(f) = detect_staleness(conn, key)? {
            flags.push(f);
        }
    }

    flags.truncate(MAX_FLAGS_PER_PACK);
    pack.flags = flags;
    Ok(())
}

/// The (subject, predicate) keys of every fact item the pack serves.
fn pack_keys(conn: &Connection, pack: &ContextPack) -> Result<Vec<PackKey>> {
    let mut keys = vec![];
    let mut stmt = conn.prepare_cached("SELECT subject_id, predicate FROM fact WHERE uid = ?1")?;
    for item in pack.items.iter().filter(|i| i.kind == "fact") {
        if let Ok((subject_id, predicate)) = stmt.query_row(params![item.id], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }) {
            keys.push(PackKey {
                subject_id,
                predicate,
                uid: item.id.clone(),
            });
        }
    }
    Ok(keys)
}

/// SQL fragment guarding off-pack peer facts by sensitivity tier.
fn tier_guard(include_private: bool) -> &'static str {
    if include_private {
        ""
    } else {
        " AND sensitivity IN ('public','personal')"
    }
}

/// >1 distinct live positive object on a single-valued predicate.
fn detect_contradiction(
    conn: &Connection,
    key: &PackKey,
    include_private: bool,
) -> Result<Option<PackFlag>> {
    if !crate::precheck::SINGLE_VALUED.contains(&key.predicate.as_str()) {
        return Ok(None);
    }
    let sql = format!(
        "SELECT uid, statement FROM fact_current
         WHERE subject_id = ?1 AND predicate = ?2 AND object_id IS NOT NULL{}
         ORDER BY COALESCE(valid_from, ingested_at) DESC",
        tier_guard(include_private)
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows: Vec<(String, String)> = stmt
        .query_map(params![key.subject_id, key.predicate], |r| {
            Ok((r.get(0)?, r.get(1)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    if rows.len() < 2 {
        return Ok(None);
    }
    Ok(Some(PackFlag {
        kind: "contradiction".into(),
        subject_id: key.subject_id.clone(),
        predicate: key.predicate.clone(),
        detail: format!(
            "{} live values on single-valued '{}': {}",
            rows.len(),
            key.predicate,
            rows.iter()
                .map(|(_, s)| s.as_str())
                .collect::<Vec<_>>()
                .join(" ⇄ ")
        ),
        fact_uids: rows.into_iter().map(|(u, _)| u).collect(),
    }))
}

/// A live negation coexisting with a live positive — contested state.
/// Symmetric: fires whether the pack served the positive or the negation.
fn detect_denial(
    conn: &Connection,
    key: &PackKey,
    include_private: bool,
) -> Result<Option<PackFlag>> {
    let sql = format!(
        "SELECT uid, statement, polarity FROM fact
         WHERE subject_id = ?1 AND predicate = ?2
           AND valid_to IS NULL AND invalidated_at IS NULL{}",
        tier_guard(include_private)
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let rows: Vec<(String, String, String)> = stmt
        .query_map(params![key.subject_id, key.predicate], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let negs: Vec<_> = rows.iter().filter(|(_, _, p)| p == "negative").collect();
    let poss: Vec<_> = rows.iter().filter(|(_, _, p)| p == "positive").collect();
    if negs.is_empty() || poss.is_empty() {
        return Ok(None);
    }
    Ok(Some(PackFlag {
        kind: "denial".into(),
        subject_id: key.subject_id.clone(),
        predicate: key.predicate.clone(),
        detail: format!("contested: \"{}\" vs \"{}\"", poss[0].1, negs[0].1),
        fact_uids: rows.iter().map(|(u, _, _)| u.clone()).collect(),
    }))
}

/// The served fact is past its predicate's half-life: λ·age > ln 2, i.e.
/// under the Poisson change model (Cho & Garcia-Molina) the chance the
/// world moved on exceeds 50%. λ NULL/0 = never re-verified: no flag.
///
/// The clock is WORLD time, not system time: an episode-rooted
/// observation counts at its episode's occurred_at — a fact extracted
/// last week from a 2015 transcript was last *evidenced* in 2015, and
/// the backfill date must not launder it fresh. Episode-less
/// observations (user assertions, web verifications) count at
/// observed_at. Fallbacks: valid_from, then ingested_at.
fn detect_staleness(conn: &Connection, key: &PackKey) -> Result<Option<PackFlag>> {
    let row = conn
        .query_row(
            "SELECT f.uid, f.statement, p.lambda,
                    COALESCE(MAX(COALESCE(e.occurred_at, o.observed_at)),
                             f.valid_from, f.ingested_at) AS last_seen,
                    (julianday('now') -
                     julianday(COALESCE(MAX(COALESCE(e.occurred_at, o.observed_at)),
                                        f.valid_from, f.ingested_at)))
                    / 365.25 AS age_years
             FROM fact f
             JOIN predicate p ON p.name = f.predicate
             LEFT JOIN fact_observation o
                    ON o.fact_id = f.id
                   AND o.kind IN ('asserted','corroborated','verified')
             LEFT JOIN episode e ON e.id = o.episode_id
             WHERE f.uid = ?1 AND f.polarity = 'positive'
               AND p.lambda IS NOT NULL AND p.lambda > 0
             GROUP BY f.id",
            params![key.uid],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, f64>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, f64>(4)?,
                ))
            },
        )
        .map(Some)
        .or_else(|e| match e {
            rusqlite::Error::QueryReturnedNoRows => Ok(None),
            e => Err(e),
        })?;
    let Some((uid, statement, lambda, last_seen, age_years)) = row else {
        return Ok(None);
    };
    if lambda * age_years <= std::f64::consts::LN_2 {
        return Ok(None);
    }
    let p_changed = 1.0 - (-lambda * age_years).exp();
    Ok(Some(PackFlag {
        kind: "staleness".into(),
        subject_id: key.subject_id.clone(),
        predicate: key.predicate.clone(),
        detail: format!(
            "\"{}\" last observed {} — past its ~{:.1}y half-life (P(changed) ≈ {:.0}%)",
            statement,
            &last_seen[..last_seen.len().min(10)],
            std::f64::consts::LN_2 / lambda,
            p_changed * 100.0
        ),
        fact_uids: vec![uid],
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::fact::{assert_fact, assert_negative_fact};
    use crate::graph::{upsert_node, Node};
    use crate::router::{Intent, PackItem, Scope};

    fn pack_with_fact(conn: &Connection, uid: &str) -> ContextPack {
        let _ = conn;
        ContextPack {
            v: 1,
            query: "test".into(),
            intent: Intent::Recall,
            entities: vec![],
            tags: vec![],
            ambiguous: vec![],
            items: vec![PackItem {
                kind: "fact".into(),
                id: uid.into(),
                score: 1.0,
                occurred_at: None,
                valid_from: None,
                source: None,
                tags: vec![],
                text: String::new(),
            }],
            truncated: false,
            budget_tokens: 4000,
            generated_at: crate::ids::now(),
            scope: Scope::Both,
            sources: vec![],
            window: None,
            flags: vec![],
        }
    }

    fn nodes(conn: &Connection) {
        for (id, ty, name) in [
            ("person-a", "person", "Ada"),
            ("org-x", "org", "X Corp"),
            ("org-y", "org", "Y Labs"),
        ] {
            upsert_node(conn, &Node::new(id, ty, name)).unwrap();
        }
    }

    #[test]
    fn test_contradiction_flag_on_served_single_valued_fact() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let uid = assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-x"),
            None,
            "Ada works at X Corp",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-y"),
            None,
            "Ada works at Y Labs",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, true).unwrap();
        assert_eq!(pack.flags.len(), 1);
        assert_eq!(pack.flags[0].kind, "contradiction");
        assert_eq!(pack.flags[0].fact_uids.len(), 2);
        assert!(pack.flags[0].detail.contains("X Corp"));
        assert!(pack.flags[0].detail.contains("Y Labs"));
    }

    #[test]
    fn test_multivalued_predicates_never_flag_contradiction() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let uid = assert_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("org-x"),
            None,
            "Ada collaborates with X Corp",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("org-y"),
            None,
            "Ada collaborates with Y Labs",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, true).unwrap();
        assert!(
            pack.flags.is_empty(),
            "multi-valued predicates are not contradictions"
        );
    }

    #[test]
    fn test_denial_flag_on_contested_state() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let uid = assert_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("org-x"),
            None,
            "Ada collaborates with X Corp",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_negative_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("org-x"),
            None,
            "Ada does NOT collaborate with X Corp",
            None,
            0.9,
            "test",
        )
        .unwrap();

        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, true).unwrap();
        assert_eq!(pack.flags.len(), 1);
        assert_eq!(pack.flags[0].kind, "denial");
        assert!(pack.flags[0].detail.contains("NOT"));
    }

    fn ep_at(conn: &Connection, sid: &str, occurred_at: &str) -> i64 {
        crate::episode::upsert_episode(
            conn,
            &crate::episode::Episode {
                id: 0,
                uid: String::new(),
                source: "note".into(),
                source_id: sid.into(),
                source_ref: None,
                body: format!("evidence {sid}"),
                occurred_at: occurred_at.into(),
                occurred_end: None,
                ingested_at: String::new(),
                lat: None,
                lon: None,
                location: None,
                sensitivity: "personal".into(),
                scope_id: None,
                meta: None,
                raw: None,
            },
        )
        .unwrap()
        .0
    }

    #[test]
    fn test_staleness_flag_past_half_life() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // works_at: λ = 0.23/yr → ~3y half-life. The realistic backfill
        // shape: extracted TODAY from a 2019 episode. The observation's
        // observed_at is now, but the world-time clock is the episode's —
        // ingestion must not launder the fact fresh.
        let eid = ep_at(&conn, "old", "2019-01-01 10:00:00");
        let uid = assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-x"),
            None,
            "Ada works at X Corp",
            Some(eid),
            Some("2019-01-01"),
            0.9,
            "test",
        )
        .unwrap();

        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, true).unwrap();
        assert_eq!(pack.flags.len(), 1);
        assert_eq!(pack.flags[0].kind, "staleness");
        assert!(pack.flags[0].detail.contains("half-life"));

        // A fresh evidence-rooted verification resets the clock: no flag.
        let fresh = ep_at(&conn, "new", &crate::ids::now());
        let fid: i64 = conn
            .query_row("SELECT id FROM fact WHERE uid=?1", params![uid], |r| {
                r.get(0)
            })
            .unwrap();
        crate::fact::record_observation(&conn, fid, Some(fresh), "verified", "test", None).unwrap();
        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, true).unwrap();
        assert!(
            pack.flags.iter().all(|f| f.kind != "staleness"),
            "a fresh verification resets the staleness clock"
        );
    }

    #[test]
    fn test_flag_budget_and_severity_order() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // One fact that is contradicted, contested AND stale.
        let uid = assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-x"),
            None,
            "Ada works at X Corp",
            None,
            Some("2019-01-01"),
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-y"),
            None,
            "Ada works at Y Labs",
            None,
            Some("2019-01-01"),
            0.9,
            "test",
        )
        .unwrap();
        assert_negative_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-x"),
            None,
            "Ada does NOT work at X Corp",
            None,
            0.9,
            "test",
        )
        .unwrap();

        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, true).unwrap();
        assert_eq!(pack.flags.len(), MAX_FLAGS_PER_PACK, "budget holds");
        assert_eq!(
            pack.flags[0].kind, "contradiction",
            "highest expected loss first"
        );
        assert_eq!(pack.flags[1].kind, "denial");
    }

    #[test]
    fn test_private_peer_fact_never_launders_into_flag_detail() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let uid = assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-x"),
            None,
            "Ada works at X Corp",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        let uid2 = assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-y"),
            None,
            "Ada secretly works at Y Labs",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        conn.execute(
            "UPDATE fact SET sensitivity='private' WHERE uid=?1",
            params![uid2],
        )
        .unwrap();

        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, false).unwrap();
        assert!(
            pack.flags.is_empty(),
            "the only contradicting peer is private — no flag at default tier"
        );
        let mut pack = pack_with_fact(&conn, &uid);
        flag_pack(&conn, &mut pack, true).unwrap();
        assert_eq!(pack.flags.len(), 1, "opted-in tier sees the contradiction");
    }
}
