//! The Selector (PLAN.md gossip roles, row 1): picks probe targets by
//! **demand × gap × λ-staleness**. SQL only — no model in the loop, by
//! design: the role that chooses what to examine must not share failure
//! modes with the roles that examine it (no role judges its own output,
//! and a model-driven Selector would smuggle model priors into what gets
//! checked).
//!
//! Inputs, all already maintained elsewhere:
//! - demand — `retrieval_touch` (V009): what retrieval actually served;
//! - gaps — `node_slot` (V013): required predicate slots with no live
//!   positive fact (goal-1 slot completeness);
//! - staleness — `predicate.lambda` (V013) against the world-time clock
//!   over `fact_observation` (V010), same rule as the pack-flag detector:
//!   λ·age > ln 2 (goal-2 fact currency).
//!
//! Score = ln(1 + touches) · (missing_required + stale). Multiplicative
//! on purpose: an untouched node scores 0 no matter how incomplete —
//! accuracy is non-uniform by design, high where the graph is used
//! (the expected-loss argument; rot in never-read facts costs nothing).
//! The nightly cold-region audit, when it exists, is the complement.

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::Result;

#[derive(Debug, Serialize)]
pub struct ProbeTarget {
    pub node_id: String,
    pub name: String,
    /// Best human-readable label for asking a QUESTION about this node.
    /// Many person nodes are email-named (identity merges pending), and
    /// the killer experiment showed Answerers will not bridge
    /// "iris.calder@example.com" in a question to "Iris Calder" in a
    /// fact — they answer UNKNOWN with the right fact at rank 1. So the
    /// Selector, which knows the aliases, does the bridging.
    pub display: String,
    /// Every alias, so a prompt can say "also known as".
    pub aliases: Vec<String>,
    pub node_type: String,
    /// Demand: times this node was served or resolved (retrieval_touch).
    pub touches: i64,
    /// How many distinct episode sources hold a mention of this node.
    ///
    /// Gossip is two readers over *independent sources*, so a node every
    /// witness to which is one source cannot be gossiped at all — the run
    /// refuses with "one witness cannot gossip", exits 0 and produces
    /// nothing. Carried so a caller can see the precondition rather than
    /// discover it a probe later.
    pub sources: i64,
    /// Required predicate slots with no live positive fact.
    pub missing_slots: Vec<String>,
    /// Live facts past their predicate's half-life: (predicate, statement).
    pub stale_facts: Vec<(String, String)>,
    /// Live, still-fresh facts on re-verifiable (λ>0) predicates —
    /// Tier-1 verification probes ("we believe X; does the evidence
    /// agree?"). Not gaps, so they don't score; they ride along because
    /// a probe session about a node should also check what it thinks
    /// it knows.
    pub verify_facts: Vec<(String, String)>,
    pub score: f64,
}

/// Prefer a human name over an identifier: `name`, then
/// `canonical_name`, then the longest multi-word alias — skipping
/// anything that looks like an email or handle. Falls back to `name`
/// when a node has no human-shaped label at all.
fn best_label(name: &str, canonical: &str, aliases: &[String]) -> String {
    let human = |s: &str| !s.contains('@') && !s.starts_with('U') || s.contains(' ');
    for cand in [name, canonical] {
        if human(cand) && !cand.contains('@') {
            return cand.to_string();
        }
    }
    aliases
        .iter()
        .filter(|a| !a.contains('@') && a.contains(' '))
        .max_by_key(|a| a.len())
        .cloned()
        .unwrap_or_else(|| name.to_string())
}

/// Rank probe targets. Reads only; the caller (gossip harness, nightly,
/// an experiment script) decides what to do with them.
pub fn probe_targets(conn: &Connection, limit: usize) -> Result<Vec<ProbeTarget>> {
    probe_targets_opts(conn, limit, false)
}

/// [`probe_targets`] with cold sampling — **an experiment mode, not a
/// production ranking.**
///
/// Production is demand-gated on purpose: an untouched node scores zero
/// however incomplete it is, because accuracy is non-uniform by design
/// and rot in never-read facts costs nothing. That is the right rule for
/// spending probe budget, and the wrong one for *measuring whether
/// probing works at all* — the demand ledger is young, so it yields a
/// handful of targets, and a handful of probes cannot separate a real
/// disagreement rate from noise.
///
/// So under `include_cold`, evidence volume stands in for demand: nodes
/// with mentions but no retrieval history are scored by the same formula
/// with mention count substituted for touches. A node the graph has a
/// lot of evidence about is a node a probe can actually check something
/// against. Keep this out of the nightly — it inverts the expected-loss
/// argument that makes the ladder affordable.
pub fn probe_targets_opts(
    conn: &Connection,
    limit: usize,
    include_cold: bool,
) -> Result<Vec<ProbeTarget>> {
    // Demanded nodes first — the multiplicative score zeroes everything
    // else anyway, so only they can rank.
    let mut demanded: Vec<(String, i64)> = {
        let mut stmt = conn.prepare(
            "SELECT t.ref_id, t.touches
             FROM retrieval_touch t
             JOIN nodes n ON n.id = t.ref_id
             WHERE t.kind = 'node'
             ORDER BY t.touches DESC LIMIT 200",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };

    if include_cold {
        // Nodes with evidence but no retrieval history. Mention count
        // stands in for touches, so both populations score on one scale.
        let seen: std::collections::HashSet<String> =
            demanded.iter().map(|(id, _)| id.clone()).collect();
        let mut stmt = conn.prepare(
            "SELECT m.node_id, COUNT(*) c
             FROM mention m JOIN nodes n ON n.id = m.node_id
             WHERE n.node_type IN ('person','project','org','topic','place')
             GROUP BY m.node_id HAVING c >= 3
             ORDER BY c DESC LIMIT 400",
        )?;
        let cold: Vec<(String, i64)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        demanded.extend(cold.into_iter().filter(|(id, _)| !seen.contains(id)));
    }

    let mut out = vec![];
    for (node_id, touches) in demanded {
        let (name, canonical, node_type): (String, String, String) = conn.query_row(
            "SELECT name, canonical_name, node_type FROM nodes WHERE id = ?1",
            params![node_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )?;
        let aliases = crate::graph::load_aliases(conn, &node_id)?;
        let display = best_label(&name, &canonical, &aliases);

        // Goal 1 — required predicate slots this node has no live positive
        // fact for. A live negation on the predicate counts as FILLED:
        // "we asked, the answer was no" is knowledge, not a gap
        // (rejection memory closing the re-ask loop again).
        let missing_slots: Vec<String> = {
            let mut stmt = conn.prepare_cached(
                "SELECT s.slot FROM node_slot s
                 WHERE s.node_type = ?1 AND s.kind = 'predicate' AND s.required = 1
                   AND NOT EXISTS (
                       SELECT 1 FROM fact f
                       WHERE f.subject_id = ?2 AND f.predicate = s.predicate
                         AND f.valid_to IS NULL AND f.invalidated_at IS NULL)",
            )?;
            let rows = stmt
                .query_map(params![node_type, node_id], |r| r.get(0))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };

        // Goal 2 — live facts past their half-life, world-time clock
        // (episode occurred_at beats observation observed_at; the same
        // rule as flags::detect_staleness).
        let stale_facts: Vec<(String, String)> = {
            let mut stmt = conn.prepare_cached(
                "SELECT f.predicate, f.statement
                 FROM fact_current f
                 JOIN predicate p ON p.name = f.predicate
                 LEFT JOIN fact_observation o
                        ON o.fact_id = f.id
                       AND o.kind IN ('asserted','corroborated','verified')
                 LEFT JOIN episode e ON e.id = o.episode_id
                 WHERE f.subject_id = ?1
                   AND p.lambda IS NOT NULL AND p.lambda > 0
                 GROUP BY f.id
                 HAVING p.lambda *
                        ((julianday('now') -
                          julianday(COALESCE(MAX(COALESCE(e.occurred_at, o.observed_at)),
                                             f.valid_from, f.ingested_at))) / 365.25)
                        > 0.6931",
            )?;
            let rows = stmt
                .query_map(params![node_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };

        let gaps = (missing_slots.len() + stale_facts.len()) as f64;
        if gaps == 0.0 {
            continue; // complete and current — nothing to probe
        }

        // How many distinct sources witness this node.
        //
        // **Reported, never enforced here.** This module ranks and the
        // caller disposes — gossip's two-source rule is gossip's, and a
        // fork experiment measuring single-source behaviour is a legitimate
        // caller that a filter in the ranker would silently starve. The
        // production filter is `--min-sources`, applied by the one consumer
        // that has the precondition.
        // `prepare_cached` like the three sibling queries in this loop —
        // it was the only statement here re-prepared per node.
        let sources: i64 = {
            let mut stmt = conn.prepare_cached(
                "SELECT COUNT(DISTINCT e.source)
                 FROM mention m JOIN episode e ON e.id = m.episode_id
                 WHERE m.node_id = ?1",
            )?;
            stmt.query_row(params![node_id], |r| r.get(0))?
        };

        // Fresh λ>0 facts, minus the stale set — verification candidates.
        let verify_facts: Vec<(String, String)> = {
            let mut stmt = conn.prepare_cached(
                "SELECT f.predicate, f.statement
                 FROM fact_current f
                 JOIN predicate p ON p.name = f.predicate
                 WHERE f.subject_id = ?1 AND p.lambda IS NOT NULL AND p.lambda > 0
                 ORDER BY f.observation_count ASC, f.ingested_at DESC LIMIT 3",
            )?;
            let rows: Vec<(String, String)> = stmt
                .query_map(params![node_id], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            rows.into_iter()
                .filter(|f| !stale_facts.contains(f))
                .collect()
        };

        out.push(ProbeTarget {
            score: ((1 + touches) as f64).ln() * gaps,
            node_id,
            name,
            display,
            aliases,
            node_type,
            touches,
            sources,
            missing_slots,
            stale_facts,
            verify_facts,
        });
    }

    out.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    out.truncate(limit);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::fact::{assert_fact, assert_negative_fact};
    use crate::graph::{upsert_node, Node};

    fn touch(conn: &Connection, node: &str, n: i64) {
        conn.execute(
            "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
             VALUES ('node', ?1, ?2, datetime('now','-10 days'), datetime('now'))",
            params![node, n],
        )
        .unwrap();
    }

    #[test]
    fn test_selector_ranks_demanded_incomplete_nodes() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-hot", "person", "Hot")).unwrap();
        upsert_node(&conn, &Node::new("person-cold", "person", "Cold")).unwrap();
        upsert_node(&conn, &Node::new("person-done", "person", "Done")).unwrap();
        upsert_node(&conn, &Node::new("org-x", "org", "X Corp")).unwrap();
        touch(&conn, "person-hot", 40);
        touch(&conn, "person-done", 40);
        // person-done fills every required person predicate slot.
        assert_fact(
            &conn,
            "person-done",
            "works_at",
            Some("org-x"),
            None,
            "Done works at X Corp",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-done",
            "has_role",
            None,
            Some("professor"),
            "Done is a professor",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        let targets = probe_targets(&conn, 10).unwrap();
        let ids: Vec<&str> = targets.iter().map(|t| t.node_id.as_str()).collect();
        assert!(ids.contains(&"person-hot"), "demanded + incomplete ranks");
        assert!(
            !ids.contains(&"person-cold"),
            "no demand = score 0 (accuracy is non-uniform by design)"
        );
        assert!(
            !ids.contains(&"person-done"),
            "complete and current = nothing to probe"
        );
        let hot = targets.iter().find(|t| t.node_id == "person-hot").unwrap();
        assert!(hot.missing_slots.contains(&"employer".into()));
        assert!(hot.missing_slots.contains(&"role".into()));
    }

    #[test]
    fn test_cold_sampling_is_opt_in_and_finds_undemanded_nodes() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-cold", "person", "Cold Person")).unwrap();
        // Evidence exists (3 mentions) but nothing ever retrieved it.
        for i in 0..3 {
            let e = crate::episode::upsert_episode(
                &conn,
                &crate::episode::Episode {
                    id: 0,
                    uid: String::new(),
                    source: "note".into(),
                    source_id: format!("c{i}"),
                    source_ref: None,
                    body: "evidence".into(),
                    occurred_at: "2026-01-01 10:00:00".into(),
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
            .0;
            crate::episode::add_mention(&conn, e, "person-cold", "manual", 1.0).unwrap();
        }

        // Production stays demand-gated: no demand, no probe.
        assert!(
            probe_targets(&conn, 10).unwrap().is_empty(),
            "an untouched node never ranks in production"
        );

        // The experiment mode reaches it, scoring evidence for demand.
        let cold = probe_targets_opts(&conn, 10, true).unwrap();
        let t = cold
            .iter()
            .find(|t| t.node_id == "person-cold")
            .expect("cold sampling finds nodes with evidence but no demand");
        assert_eq!(t.touches, 3, "mention count stands in for touches");
        assert!(t.score > 0.0);
    }

    #[test]
    fn test_email_named_node_gets_a_human_display_label() {
        let conn = open_memory().unwrap();
        let p = crate::graph::get_or_create_person(
            &conn,
            Some("iris.calder@example.com"),
            "iris.calder@example.com",
            "t",
        )
        .unwrap();
        crate::graph::add_alias(&conn, &p.id, "iris", "manual").unwrap();
        crate::graph::add_alias(&conn, &p.id, "Iris Calder", "manual").unwrap();
        touch(&conn, &p.id, 10);

        let targets = probe_targets(&conn, 10).unwrap();
        let t = targets.iter().find(|t| t.node_id == p.id).unwrap();
        // Aliases are stored lowercased; casing is irrelevant to a prompt,
        // the bridge from identifier to human name is the point.
        assert_eq!(
            t.display.to_lowercase(),
            "iris calder",
            "the Selector bridges the identifier so the Answerer never has to"
        );
        assert!(t.name.contains('@'), "the raw name is still reported");
        assert!(t.aliases.contains(&"iris".to_string()));
    }

    #[test]
    fn test_negation_fills_a_slot() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-a", "person", "Ada")).unwrap();
        touch(&conn, "person-a", 10);
        assert_negative_fact(
            &conn,
            "person-a",
            "has_role",
            None,
            Some("any"),
            "Ada holds no formal role",
            None,
            0.9,
            "user",
        )
        .unwrap();

        let targets = probe_targets(&conn, 10).unwrap();
        let t = targets.iter().find(|t| t.node_id == "person-a").unwrap();
        assert!(
            !t.missing_slots.contains(&"role".into()),
            "a live negation is an answer, not a gap — never re-ask"
        );
        assert!(t.missing_slots.contains(&"employer".into()));
    }

    #[test]
    fn test_stale_fact_makes_a_current_node_a_target() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-a", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("org-x", "org", "X Corp")).unwrap();
        touch(&conn, "person-a", 10);
        // Fill both required slots, but employer evidence is from 2019 —
        // past works_at's ~3y half-life.
        let old_ep = crate::episode::upsert_episode(
            &conn,
            &crate::episode::Episode {
                id: 0,
                uid: String::new(),
                source: "note".into(),
                source_id: "old".into(),
                source_ref: None,
                body: "evidence".into(),
                occurred_at: "2019-01-01 10:00:00".into(),
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
        .0;
        assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-x"),
            None,
            "Ada works at X Corp",
            Some(old_ep),
            Some("2019-01-01"),
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "has_role",
            None,
            Some("professor"),
            "Ada is a professor",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        let targets = probe_targets(&conn, 10).unwrap();
        let t = targets.iter().find(|t| t.node_id == "person-a").unwrap();
        assert!(t.missing_slots.is_empty());
        assert_eq!(t.stale_facts.len(), 1);
        assert_eq!(t.stale_facts[0].0, "works_at");
    }
}
