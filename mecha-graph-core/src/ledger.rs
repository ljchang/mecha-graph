//! The query ledger + retrieval-touch demand signal + event log (V009,
//! PLAN.md Wave 1b). The router calls [`record`] once per answered query;
//! everything here is **best-effort by design** — telemetry must never
//! break retrieval, so callers ignore the Result at the call site and the
//! functions themselves keep to single statements that cannot half-apply.
//!
//! `pkg eval` passes `tool: None` and is never recorded: gold queries run
//! repeatedly and would corrupt the demand signal (the same reproducibility
//! rule that makes `mecha eval` force messaging off).

use crate::error::Result;
use crate::ids::now;
use crate::router::{ContextPack, Intent};
use rusqlite::{params, Connection};

/// Coverage verdict for one pack, computed from what the router already
/// knows. `thin` is intent-aware: a LOOKUP answered by one
/// person_interaction row is a *perfect* answer, not a thin one — only
/// RECALL wants breadth.
pub fn coverage_flags(pack: &ContextPack) -> Vec<&'static str> {
    let mut flags = vec![];
    if pack.items.is_empty() {
        flags.push("empty");
    } else if matches!(pack.intent, Intent::Recall) && pack.items.len() < 3 {
        flags.push("thin");
    }
    if !pack.ambiguous.is_empty() {
        flags.push("ambiguous");
    }
    flags
}

/// Append the query to the ledger and bump demand for everything the pack
/// returned. One row in `query_log` (status `gap` when coverage flagged),
/// one upsert per distinct (kind, ref) in `retrieval_touch`.
pub fn record(conn: &Connection, tool: &str, pack: &ContextPack) -> Result<i64> {
    let flags = coverage_flags(pack);
    // Only empty/ambiguous mark a gap (the deferred-research work queue).
    // `thin` stays descriptive — §1's division: pkg describes, the model
    // judges whether one strong item actually answered the question.
    let status = if flags.contains(&"empty") || flags.contains(&"ambiguous") {
        "gap"
    } else {
        "ok"
    };
    let anchor_ids: Vec<&str> = pack.entities.iter().map(|e| e.node_id.as_str()).collect();
    let intent = format!("{:?}", pack.intent).to_lowercase();

    conn.execute(
        "INSERT INTO query_log (tool, query, intent, anchor_ids, top_score,
                                result_count, coverage_flags, status)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            tool,
            pack.query,
            intent,
            serde_json::to_string(&anchor_ids)?,
            pack.items.first().map(|i| i.score),
            pack.items.len() as i64,
            serde_json::to_string(&flags)?,
            status
        ],
    )?;
    let log_id = conn.last_insert_rowid();

    // A probe's own reads are not demand.
    //
    // `retrieval_touch` answers "what does the user actually reach for", and
    // the gossip Selector ranks probe targets by it. So a probe that reads
    // the graph about its target raises that target's demand and elects it
    // again, harder — measured 2026-08-16: one probe took a single target
    // from touches=2 (score 2.20) to 28 (6.73), on a demand-gated pool of
    // nine. The Selector was reading gossip's own footprint back as evidence
    // of interest.
    //
    // The query still enters `query_log`: an instrumentation read is real
    // and worth being able to audit. It just does not vote on what matters.
    // Marked by the CALLER, because only the caller knows why it is reading;
    // nothing here can infer it from the query.
    if tool.ends_with(".probe") {
        return Ok(log_id);
    }

    // Demand: every returned item, plus every resolved anchor (an anchor is
    // demanded even when its pack came back empty — *especially* then).
    let ts = now();
    let mut touch = conn.prepare_cached(
        "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
         VALUES (?1, ?2, 1, ?3, ?3)
         ON CONFLICT(kind, ref_id)
         DO UPDATE SET touches = touches + 1, last_at = excluded.last_at",
    )?;
    for e in &pack.entities {
        touch.execute(params!["node", e.node_id, ts])?;
    }
    for item in &pack.items {
        // person_interaction rows are node-shaped demand; everything else
        // keeps its own kind so activation can join per-table later.
        let kind = match item.kind.as_str() {
            "person_interaction" | "node" => "node",
            "fact" => "fact",
            _ => "episode",
        };
        touch.execute(params![kind, item.id, ts])?;
    }
    Ok(log_id)
}

/// ACT-R base-level activation from the demand ledger (mechanism #3):
/// the optimized-learning approximation A = ln(n/(1−d)) − d·ln(lifetime),
/// d = 0.5, lifetime in days since first touch — so no per-event
/// timestamps are needed, just the (touches, first_at) this table already
/// keeps. `None` = never retrieved (the caller decides what silence
/// means; ranking treats it as 0, §11.5 decay will treat it as sinking).
/// Clamped to [-2, 4]: activation is a tie-break arm, and even a
/// thousand-touch node must stay a nudge, not a takeover.
pub fn activation(conn: &Connection, kind: &str, ref_id: &str) -> Result<Option<f64>> {
    use rusqlite::OptionalExtension;
    let row: Option<(f64, f64)> = conn
        .query_row(
            "SELECT touches,
                    MAX(julianday('now') - julianday(first_at), 0.02)
             FROM retrieval_touch WHERE kind = ?1 AND ref_id = ?2",
            params![kind, ref_id],
            |r| Ok((r.get::<_, i64>(0)? as f64, r.get(1)?)),
        )
        .optional()?;
    Ok(row.map(|(n, days)| ((n / 0.5).ln() - 0.5 * days.ln()).clamp(-2.0, 4.0)))
}

/// Append one observability event (PLAN.md event_log kinds). Payload is
/// caller-shaped JSON; keep it small.
pub fn log_event(
    conn: &Connection,
    kind: &str,
    r#ref: Option<&str>,
    payload: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO event_log (kind, ref, payload) VALUES (?1, ?2, ?3)",
        params![kind, r#ref, payload],
    )?;
    Ok(conn.last_insert_rowid())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{upsert_episode, Episode};
    use crate::graph::{upsert_node, Node};
    use crate::router;

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
    fn test_query_logged_and_demand_bumped() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("iris", "person", "Iris")).unwrap();
        let (id, _) = upsert_episode(&conn, &ep("a", "pilot data discussion with Iris")).unwrap();
        crate::episode::add_mention(&conn, id, "iris", "manual", 1.0).unwrap();

        // Twice, so the touch upsert path is exercised.
        for _ in 0..2 {
            let pack = router::query(
                &conn,
                None,
                "pilot data Iris",
                10,
                4000,
                false,
                Some("cli.query"),
            )
            .unwrap();
            assert!(!pack.items.is_empty());
        }

        let (logged, flags): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MAX(coverage_flags) FROM query_log
                 WHERE tool='cli.query' AND status='ok'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(logged, 2, "thin is descriptive, not a gap");
        assert!(
            flags.contains("thin"),
            "one-item recall still carries the flag"
        );

        let touches: i64 = conn
            .query_row(
                "SELECT touches FROM retrieval_touch WHERE kind='node' AND ref_id='iris'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(touches, 2, "anchor demand accumulates across queries");
    }

    #[test]
    fn test_empty_result_becomes_gap() {
        let conn = open_memory().unwrap();
        router::query(
            &conn,
            None,
            "completely unknown topic",
            10,
            4000,
            false,
            Some("mcp.kg_search"),
        )
        .unwrap();
        let (status, flags): (String, String) = conn
            .query_row("SELECT status, coverage_flags FROM query_log", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(status, "gap");
        assert!(flags.contains("empty"));
    }

    /// A probe read is logged but does not vote.
    ///
    /// The Selector ranks probe targets by `retrieval_touch`, so a probe
    /// that bumped its own target's demand would elect it again next night
    /// — measured on the live graph, one probe took an entity from 2 touches
    /// to 28 and tripled its score. The query still lands in `query_log`,
    /// because an instrumentation read is real and worth auditing.
    #[test]
    fn a_probe_read_is_logged_but_bumps_no_demand() {
        let conn = open_memory().unwrap();
        crate::graph::upsert_node(&conn, &crate::graph::Node::new("w", "person", "Nadia"))
            .unwrap();

        router::query(
            &conn,
            None,
            "Nadia",
            10,
            4000,
            true,
            Some("mcp.kg_search.probe"),
        )
        .unwrap();
        let touches: i64 = conn
            .query_row("SELECT COUNT(*) FROM retrieval_touch", [], |r| r.get(0))
            .unwrap();
        assert_eq!(touches, 0, "a probe must not manufacture demand");
        let logged: i64 = conn
            .query_row("SELECT COUNT(*) FROM query_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(logged, 1, "but it is still auditable");

        // The same read from a real caller does count.
        router::query(&conn, None, "Nadia", 10, 4000, true, Some("mcp.kg_search")).unwrap();
        let touches: i64 = conn
            .query_row("SELECT COUNT(*) FROM retrieval_touch", [], |r| r.get(0))
            .unwrap();
        assert!(touches > 0, "an ordinary read is demand");
    }

    #[test]
    fn test_eval_path_records_nothing() {
        let conn = open_memory().unwrap();
        router::query(&conn, None, "anything at all", 10, 4000, true, None).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM query_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n, 0, "tool=None (eval) must never pollute the ledger");
    }

    #[test]
    fn test_activation_formula_and_clamp() {
        let conn = open_memory().unwrap();
        assert!(
            activation(&conn, "fact", "nope").unwrap().is_none(),
            "never touched = None"
        );

        conn.execute(
            "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
             VALUES ('fact', 'f1', 8, datetime('now', '-4 days'), datetime('now'))",
            [],
        )
        .unwrap();
        let a = activation(&conn, "fact", "f1").unwrap().unwrap();
        // A = ln(8/0.5) − 0.5·ln(4) ≈ 2.77 − 0.69 = 2.08
        assert!(
            (a - 2.08).abs() < 0.05,
            "optimized-learning approximation, got {a}"
        );

        // A thousand touches stays a nudge: the clamp holds.
        conn.execute(
            "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
             VALUES ('fact', 'f2', 100000, datetime('now'), datetime('now'))",
            [],
        )
        .unwrap();
        assert_eq!(activation(&conn, "fact", "f2").unwrap().unwrap(), 4.0);
    }
}
