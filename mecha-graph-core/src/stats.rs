//! Operational health (§11): each number maps to a specific action — a number
//! without an action is decoration.

use crate::error::Result;
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
pub struct HealthStats {
    pub episodes_by_source: Vec<(String, i64)>,
    pub nodes_by_type: Vec<(String, i64)>,
    pub facts_total: i64,
    pub facts_live: i64,
    pub enriched_pct: f64,
    pub embedded_pct: f64,
    /// Nodes with no edges AND no mentions. Rising → linking is failing —
    /// check Tier 1/2 linkers, not the review queue (§11.4).
    pub isolated_pct: f64,
    pub merge_queue_depth: i64,
    /// >1 live fact on same (subject, predicate) — usually a missed supersession.
    pub live_contradictions: i64,
    /// Facts asserted only by LLM extraction, never corroborated (§11.5).
    pub llm_only_facts: i64,
    /// Beliefs closed by world-change rather than error: valid time ended,
    /// system time never invalidated (the decay sweep). Rising fast means
    /// a class's threshold or λ is mis-tuned — a calibration signal, NOT
    /// a trust signal (decay does not demote; corrections do).
    pub decayed_beliefs: i64,
    /// Retrieval per extractor origin: live facts, how many were EVER
    /// served in a context pack, and total serves. The graph's only ground
    /// truth of usefulness — `retrieval_touch` has recorded it since day
    /// one and nothing read it, which is how the queue could grow at
    /// 15–25× review throughput while 83% of live facts had never once
    /// been retrieved. Review-accept rate says a fact isn't wrong; this
    /// says whether it was ever worth having.
    pub fact_usage: Vec<FactUsageRow>,
    /// Review-on-use: live unreviewed facts, how many a pack ever served,
    /// and how many the verdict queue is surfacing right now. The three
    /// numbers that say whether the demand loop is moving.
    pub shadow_live: i64,
    pub shadow_served: i64,
    pub shadow_surfaced: i64,
    pub ingest_state: Vec<IngestStateRow>,
}

#[derive(Debug, Serialize)]
pub struct FactUsageRow {
    pub extractor: String,
    pub live: i64,
    pub retrieved: i64,
    /// None when `live` is 0 — a rate over an empty denominator is not 0%.
    pub retrieved_pct: Option<f64>,
    pub touches: i64,
}

#[derive(Debug, Serialize)]
pub struct IngestStateRow {
    pub source: String,
    pub cursor: Option<String>,
    pub last_ok_at: Option<String>,
    pub items_seen: i64,
    pub last_error: Option<String>,
    /// Stale > 24h means a source silently stopped — ops problem (§11.4).
    pub stale: bool,
}

pub fn health(conn: &Connection) -> Result<HealthStats> {
    let pairs = |sql: &str| -> Result<Vec<(String, i64)>> {
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        Ok(rows)
    };
    let scalar = |sql: &str| -> Result<i64> { Ok(conn.query_row(sql, [], |r| r.get(0))?) };

    let n_episodes = scalar("SELECT COUNT(*) FROM episode")?.max(0);
    let n_nodes = scalar("SELECT COUNT(*) FROM nodes")?.max(0);

    let enriched = scalar("SELECT COUNT(*) FROM episode_enrichment")?;
    let embedded = scalar("SELECT COUNT(*) FROM vec_episode")?;
    let isolated = scalar(
        "SELECT COUNT(*) FROM nodes n
         WHERE NOT EXISTS (SELECT 1 FROM fact_current f
                           WHERE (f.subject_id = n.id OR f.object_id = n.id)
                             AND f.object_id IS NOT NULL)
           AND NOT EXISTS (SELECT 1 FROM mention m WHERE m.node_id = n.id)",
    )?;

    let pct = |num: i64, den: i64| {
        if den == 0 {
            0.0
        } else {
            100.0 * num as f64 / den as f64
        }
    };

    let mut stmt = conn.prepare(
        "SELECT source, cursor, last_ok_at, items_seen, last_error,
                COALESCE(last_ok_at, '') < datetime('now', '-1 day')
         FROM ingest_state",
    )?;
    let ingest_state = stmt
        .query_map([], |r| {
            Ok(IngestStateRow {
                source: r.get(0)?,
                cursor: r.get(1)?,
                last_ok_at: r.get(2)?,
                items_seen: r.get(3)?,
                last_error: r.get(4)?,
                stale: r.get::<_, i64>(5)? != 0,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT COALESCE(f.extractor, '(none)') AS ex,
                COUNT(*),
                SUM(rt.ref_id IS NOT NULL),
                COALESCE(SUM(rt.touches), 0)
         FROM fact_current f
         LEFT JOIN retrieval_touch rt ON rt.kind = 'fact' AND rt.ref_id = f.uid
         GROUP BY ex ORDER BY COUNT(*) DESC",
    )?;
    let fact_usage = stmt
        .query_map([], |r| {
            let live: i64 = r.get(1)?;
            let retrieved: i64 = r.get(2)?;
            Ok(FactUsageRow {
                extractor: r.get(0)?,
                live,
                retrieved,
                retrieved_pct: (live > 0).then(|| 100.0 * retrieved as f64 / live as f64),
                touches: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    let shadow_live_served = crate::shadow::shadow_counts(conn)?;
    Ok(HealthStats {
        episodes_by_source: pairs(
            "SELECT source, COUNT(*) FROM episode GROUP BY source ORDER BY COUNT(*) DESC",
        )?,
        nodes_by_type: pairs(
            "SELECT node_type, COUNT(*) FROM nodes GROUP BY node_type ORDER BY COUNT(*) DESC",
        )?,
        facts_total: scalar("SELECT COUNT(*) FROM fact")?,
        facts_live: scalar("SELECT COUNT(*) FROM fact_current")?,
        enriched_pct: pct(enriched, n_episodes),
        embedded_pct: pct(embedded, n_episodes),
        isolated_pct: pct(isolated, n_nodes),
        merge_queue_depth: scalar("SELECT COUNT(*) FROM fact_candidate WHERE status = 'proposed'")?,
        live_contradictions: crate::fact::live_contradictions(conn)?.len() as i64,
        decayed_beliefs: conn.query_row(
            "SELECT COUNT(*) FROM fact
             WHERE valid_to IS NOT NULL AND invalidated_at IS NULL",
            [],
            |r| r.get(0),
        )?,
        llm_only_facts: scalar(
            "SELECT COUNT(*) FROM fact_current WHERE extractor = 'llm' AND observation_count = 1",
        )?,
        fact_usage,
        shadow_live: shadow_live_served.0,
        shadow_served: shadow_live_served.1,
        shadow_surfaced: crate::shadow::surfaced(conn, crate::shadow::DEFAULT_SURFACE_LIMIT)?.len()
            as i64,
        ingest_state,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{upsert_node, Node};

    #[test]
    fn fact_usage_counts_serves_not_beliefs() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-a", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("project-p", "project", "Hypercourse")).unwrap();
        let served = crate::fact::assert_fact(
            &conn,
            "person-a",
            "works_on",
            Some("project-p"),
            None,
            "Ada works on Hypercourse",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        crate::fact::assert_fact(
            &conn,
            "person-a",
            "uses",
            None,
            Some("git"),
            "Ada uses git",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
             VALUES ('fact', ?1, 3, datetime('now'), datetime('now'))",
            rusqlite::params![served],
        )
        .unwrap();

        let h = health(&conn).unwrap();
        let llm = h.fact_usage.iter().find(|u| u.extractor == "llm").unwrap();
        assert_eq!(llm.live, 2);
        assert_eq!(llm.retrieved, 1, "one of the two was ever served");
        assert_eq!(llm.touches, 3);
        assert_eq!(llm.retrieved_pct, Some(50.0));
    }
}
