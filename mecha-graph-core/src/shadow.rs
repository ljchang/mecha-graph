//! The surfaced-verdict queue (review-on-use, docs/REVIEW-ON-USE.md §2).
//!
//! A shadow fact earns a human verdict when it is *about to matter*, and
//! this module is the one place that decides what "about to matter"
//! means. Three triggers, in priority order:
//!
//! 1. **Contradiction** — a shadow fact disagreeing with a reviewed fact
//!    on a single-valued predicate. Two live answers to a question that
//!    has one; somebody will be served the wrong one soon.
//! 2. **Retrieval** — the fact was actually served in a context pack
//!    (`retrieval_touch`, the only ground truth of usefulness). Most
//!    recently served first: that is the fact most likely to be served
//!    again.
//! 3. **Spot-check** — the ladder's `sampled` rung, applied to shadow
//!    facts the way precheck applies it to candidates: a deterministic
//!    1-in-10 by fact id, so re-runs agree and the draw cannot chase the
//!    reviewer's attention.
//!
//! The set is small by construction — [`DEFAULT_SURFACE_LIMIT`] — because
//! the whole point of the inversion is that the human is the scarcest
//! resource. The entity-view trigger needs no function here: every
//! fact reader already carries `tier`, so an entity page filters its own
//! facts. Verdicts land through [`crate::fact::confirm_shadow_fact`] /
//! [`crate::fact::refute_shadow_fact`], and only from human surfaces
//! (CLI, TUI) — the MCP surface reads this queue but cannot vote on it,
//! because a lane must not promote itself.

use crate::error::Result;
use crate::fact::{self, Fact};
use rusqlite::Connection;
use serde::Serialize;
use std::collections::HashMap;

/// How many facts one surfacing hands a human. Review arrives a handful
/// at a time, in context, about facts doing work — never a backlog.
pub const DEFAULT_SURFACE_LIMIT: usize = 10;

#[derive(Debug, Serialize)]
pub struct SurfacedFact {
    pub fact: Fact,
    /// Times a context pack served this fact (0 = surfaced another way).
    pub touches: i64,
    pub last_served: Option<String>,
    /// Why it surfaced, in words a person can act on. Merged when more
    /// than one trigger fired.
    pub reasons: Vec<String>,
}

/// The live shadow facts a human should look at now, most urgent first,
/// truncated to `limit` — plus how many were surfaced BEFORE the
/// truncation, because a consumer that reports the page length as the
/// queue depth is reporting its own page size (the `--top` trap, one
/// repo over: a capped listing silently read as the whole).
pub fn surfaced_counted(conn: &Connection, limit: usize) -> Result<(Vec<SurfacedFact>, usize)> {
    let mut all = surfaced(conn, usize::MAX)?;
    let total = all.len();
    all.truncate(limit);
    Ok((all, total))
}

/// The live shadow facts a human should look at now, most urgent first.
pub fn surfaced(conn: &Connection, limit: usize) -> Result<Vec<SurfacedFact>> {
    // Trigger 1: contradiction with a reviewed fact. The predicate list
    // is precheck's — one notion of single-valued, not two.
    let single_valued = crate::precheck::SINGLE_VALUED
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(",");
    let sql = format!(
        "SELECT s.uid, r.statement FROM fact s
         JOIN fact r ON r.subject_id = s.subject_id AND r.predicate = s.predicate
         WHERE s.tier <> 'reviewed' AND r.tier = 'reviewed'
           AND s.valid_to IS NULL AND s.invalidated_at IS NULL
           AND r.valid_to IS NULL AND r.invalidated_at IS NULL
           AND s.object_id IS NOT NULL AND r.object_id IS NOT NULL
           AND s.object_id <> r.object_id
           AND s.predicate IN ({single_valued})
         ORDER BY s.uid"
    );
    let mut contradictions: HashMap<String, String> = HashMap::new();
    {
        let mut stmt = conn.prepare(&sql)?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (uid, reviewed_stmt) = row?;
            contradictions.entry(uid).or_insert(reviewed_stmt);
        }
    }

    // Trigger 3's class lookup: (proposer, predicate) classes at the
    // sampled rung. The extractor on a minted fact IS the proposer.
    let mut sampled_classes: std::collections::HashSet<(String, String)> =
        std::collections::HashSet::new();
    {
        let mut stmt =
            conn.prepare("SELECT proposer, predicate FROM class_ledger WHERE rung = 'sampled'")?;
        for row in stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))? {
            let (p, k) = row?;
            sampled_classes.insert((p, k));
        }
    }

    // One walk over the live shadow rows, joining the touch ledger.
    let mut out: Vec<SurfacedFact> = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT f.*, COALESCE(t.touches, 0) AS _touches, t.last_at AS _last_at
         FROM fact f
         LEFT JOIN retrieval_touch t ON t.kind = 'fact' AND t.ref_id = f.uid
         WHERE f.tier <> 'reviewed'
           AND f.valid_to IS NULL AND f.invalidated_at IS NULL",
    )?;
    let rows = stmt.query_map([], |r| {
        let fact = fact::row_to_fact(r)?;
        let touches: i64 = r.get("_touches")?;
        let last_at: Option<String> = r.get("_last_at")?;
        Ok((fact, touches, last_at))
    })?;
    for row in rows {
        let (fact, touches, last_served) = row?;
        let mut reasons = Vec::new();
        if let Some(reviewed_stmt) = contradictions.get(&fact.uid) {
            reasons.push(format!("contradicts a reviewed fact: {reviewed_stmt}"));
        }
        if touches > 0 {
            reasons.push(match &last_served {
                Some(at) => format!("served {touches}× (last {at})"),
                None => format!("served {touches}×"),
            });
        }
        let class = (
            fact.extractor.clone().unwrap_or_default(),
            fact.predicate.clone(),
        );
        if fact.id % 10 == 0 && sampled_classes.contains(&class) {
            reasons.push("spot-check (sampled class)".into());
        }
        if reasons.is_empty() {
            continue;
        }
        out.push(SurfacedFact {
            fact,
            touches,
            last_served,
            reasons,
        });
    }

    // Contradictions first; then by recency of serving; spot-checks
    // trail. Within a band, most-served first.
    out.sort_by(|a, b| {
        let ac = a.reasons.iter().any(|r| r.starts_with("contradicts"));
        let bc = b.reasons.iter().any(|r| r.starts_with("contradicts"));
        bc.cmp(&ac)
            .then_with(|| b.last_served.cmp(&a.last_served))
            .then_with(|| b.touches.cmp(&a.touches))
            .then_with(|| a.fact.uid.cmp(&b.fact.uid))
    });
    out.truncate(limit);
    Ok(out)
}

/// How many live shadow facts exist, and how many have ever been served —
/// the two numbers that say whether the demand loop is moving.
pub fn shadow_counts(conn: &Connection) -> Result<(i64, i64)> {
    let live: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fact
         WHERE tier <> 'reviewed' AND valid_to IS NULL AND invalidated_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    let served: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fact f
         JOIN retrieval_touch t ON t.kind = 'fact' AND t.ref_id = f.uid
         WHERE f.tier <> 'reviewed' AND f.valid_to IS NULL AND f.invalidated_at IS NULL",
        [],
        |r| r.get(0),
    )?;
    Ok((live, served))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{upsert_node, Node};
    use rusqlite::params;

    fn mint(
        conn: &Connection,
        subject: &str,
        predicate: &str,
        object: Option<&str>,
        statement: &str,
    ) -> String {
        let p = fact::ProposedFact {
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.map(Into::into),
            statement: statement.into(),
            confidence: Some(0.8),
            ..Default::default()
        };
        let cid = fact::propose_fact(conn, &p, "llm", None).unwrap();
        fact::mint_shadow_candidate(conn, cid).unwrap()
    }

    fn touch(conn: &Connection, uid: &str, times: i64) {
        conn.execute(
            "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
             VALUES ('fact', ?1, ?2, datetime('now'), datetime('now'))
             ON CONFLICT(kind, ref_id) DO UPDATE SET touches = ?2",
            params![uid, times],
        )
        .unwrap();
    }

    /// The queue is demand: an untouched, unconflicted, unsampled shadow
    /// fact surfaces nowhere, however long it waits.
    #[test]
    fn an_idle_shadow_fact_never_surfaces() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("ada", "person", "Ada")).unwrap();
        mint(&conn, "Ada", "works_on", None, "Ada charts the reef survey");
        assert!(surfaced(&conn, 10).unwrap().is_empty());
    }

    /// A served shadow fact surfaces with its serving history, and a
    /// contradiction outranks any amount of serving.
    #[test]
    fn contradiction_outranks_retrieval_and_both_surface() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("ada", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("lab-a", "org", "Harbor Lab")).unwrap();
        upsert_node(&conn, &Node::new("lab-b", "org", "Summit Lab")).unwrap();

        // Reviewed truth.
        fact::assert_fact(
            &conn,
            "ada",
            "works_at",
            Some("lab-a"),
            None,
            "Ada works at Harbor Lab",
            None,
            None,
            0.9,
            "user",
        )
        .unwrap();
        // Shadow contradiction (different object, single-valued predicate).
        let contra = mint(
            &conn,
            "Ada",
            "works_at",
            Some("Summit Lab"),
            "Ada works at Summit Lab",
        );
        // Shadow fact that was served twice.
        let served = mint(&conn, "Ada", "works_on", None, "Ada charts the reef survey");
        touch(&conn, &served, 2);

        let q = surfaced(&conn, 10).unwrap();
        let uids: Vec<&str> = q.iter().map(|s| s.fact.uid.as_str()).collect();
        assert_eq!(uids, vec![contra.as_str(), served.as_str()]);
        assert!(q[0].reasons[0].starts_with("contradicts a reviewed fact"));
        assert!(q[1].reasons[0].starts_with("served 2×"));
    }

    /// The sampled rung's spot-check reaches shadow facts: deterministic
    /// 1-in-10 by id, same convention precheck holds for candidates.
    #[test]
    fn a_sampled_class_spot_checks_one_in_ten() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("ada", "person", "Ada")).unwrap();
        conn.execute(
            "INSERT INTO class_ledger (proposer, predicate, rung)
             VALUES ('llm', 'works_on', 'sampled')",
            [],
        )
        .unwrap();
        let mut surfaced_uids = Vec::new();
        for i in 0..20 {
            let uid = mint(
                &conn,
                "Ada",
                "works_on",
                None,
                &format!("Ada charts transect number {i}"),
            );
            let id: i64 = conn
                .query_row("SELECT id FROM fact WHERE uid = ?1", params![uid], |r| {
                    r.get(0)
                })
                .unwrap();
            if id % 10 == 0 {
                surfaced_uids.push(uid);
            }
        }
        let q = surfaced(&conn, 50).unwrap();
        let got: Vec<&str> = q.iter().map(|s| s.fact.uid.as_str()).collect();
        assert_eq!(
            got,
            surfaced_uids.iter().map(String::as_str).collect::<Vec<_>>()
        );
        assert!(q
            .iter()
            .all(|s| s.reasons == vec!["spot-check (sampled class)"]));
    }
}
