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
    pub ingest_state: Vec<IngestStateRow>,
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
        ingest_state,
    })
}
