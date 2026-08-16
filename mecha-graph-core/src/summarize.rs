//! Scope summaries (§4.5): refresh `node_context.summary` — the generated,
//! refreshable half of a node's context (the hand-authored `instruction` is
//! NEVER touched here). A summary is a materialized view over the node's
//! facts + episode neighborhood; staleness is computed dynamically by
//! comparing `summary_updated_at` against the newest ingested episode that
//! mentions the node, so no trigger maintenance is needed and backfilled
//! episodes (old occurred_at, fresh ingested_at) still mark it stale.
//!
//! Privacy: private+ episodes (private, secret) are EXCLUDED from summary
//! evidence — allowlist form, so an unknown or future tier fails closed.
//! Summaries are served un-gated by every consumer surface (context packs,
//! kg_entity), so private content must not launder into them.

use crate::context;
use crate::error::Result;
use crate::extract::OllamaChat;
use rusqlite::{params, Connection};
use serde::Serialize;

/// Node types that accumulate enough narrative to be worth a summary.
/// Tasks/events/documents are point-like — their name and facts suffice.
const SUMMARY_TYPES: &str = "('person','project','org','topic','area','goal','place')";

/// Below this many mentioning episodes a summary would just restate the name.
pub const SUMMARY_MIN_EPISODES: i64 = 3;

pub const SUMMARY_EPISODE_SNIPPETS: usize = 8;
pub const SUMMARY_SNIPPET_CHARS: usize = 400;
pub const SUMMARY_MAX_FACTS: usize = 15;

#[derive(Debug, Default, Serialize)]
pub struct SummarizeReport {
    pub refreshed: usize,
    pub errors: Vec<String>,
}

/// Nodes whose summary is missing or older than the newest ingested episode
/// mentioning them. Ordered by episode volume — the busiest scopes are the
/// most valuable summaries and get refreshed first under a nightly limit.
pub fn stale_summary_nodes(conn: &Connection, limit: usize) -> Result<Vec<String>> {
    let sql = format!(
        "WITH activity AS (
             SELECT m.node_id, COUNT(*) AS eps, MAX(e.ingested_at) AS latest
             FROM mention m JOIN episode e ON e.id = m.episode_id
             GROUP BY m.node_id
         )
         SELECT n.id FROM nodes n
         JOIN activity a ON a.node_id = n.id
         LEFT JOIN node_context c ON c.node_id = n.id
         WHERE n.node_type IN {SUMMARY_TYPES}
           AND a.eps >= ?1
           AND (c.node_id IS NULL OR c.summary = ''
                OR c.summary_updated_at IS NULL
                OR c.summary_updated_at < a.latest)
         ORDER BY a.eps DESC
         LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let ids = stmt
        .query_map(params![SUMMARY_MIN_EPISODES, limit as i64], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
}

/// Assemble the evidence block the model summarizes from. Public for tests.
pub fn summary_evidence(conn: &Connection, node_id: &str) -> Result<Option<String>> {
    let Some(node) = crate::graph::get_node(conn, node_id)? else {
        return Ok(None);
    };

    let mut out = format!("Entity: {} (type: {})\n", node.name, node.node_type);
    if !node.aliases.is_empty() {
        out.push_str(&format!("Also known as: {}\n", node.aliases.join(", ")));
    }

    if let Some(pi) = crate::rollup::get_person_interaction(conn, node_id)? {
        out.push_str(&format!(
            "Interactions: {} total; first seen {}; last seen {}\n",
            pi.interaction_count,
            pi.first_seen_at.as_deref().unwrap_or("?"),
            pi.last_seen_at.as_deref().unwrap_or("?"),
        ));
    }

    let facts = crate::fact::facts_for_node(conn, node_id, SUMMARY_MAX_FACTS as i64)?;
    if !facts.is_empty() {
        out.push_str("\nKnown facts:\n");
        for f in &facts {
            // Denials are marked structurally, not left to the model
            // noticing a "not" mid-sentence: a summary that inverts one
            // would assert the opposite of a recorded answer, and
            // summaries are served un-gated (§4.5).
            let neg = if f.polarity == "negative" {
                "[KNOWN FALSE] "
            } else {
                ""
            };
            match &f.valid_from {
                Some(v) => out.push_str(&format!("- {neg}(as of {v}) {}\n", f.statement)),
                None => out.push_str(&format!("- {neg}{}\n", f.statement)),
            }
        }
    }

    // Newest first; private episodes never feed a summary (see module doc).
    let mut stmt = conn.prepare(
        "SELECT e.source, e.occurred_at, e.body FROM episode e
         JOIN mention m ON m.episode_id = e.id
         WHERE m.node_id = ?1 AND e.sensitivity IN ('public','personal')
         ORDER BY e.occurred_at DESC LIMIT ?2",
    )?;
    let episodes: Vec<(String, String, String)> = stmt
        .query_map(params![node_id, SUMMARY_EPISODE_SNIPPETS as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    if !episodes.is_empty() {
        out.push_str("\nRecent episodes (newest first):\n");
        for (source, at, body) in &episodes {
            let snippet: String = body.chars().take(SUMMARY_SNIPPET_CHARS).collect();
            out.push_str(&format!(
                "- [{at} {source}] {}\n",
                snippet.replace('\n', " ")
            ));
        }
    }
    Ok(Some(out))
}

const SYSTEM_PROMPT: &str =
    "You write reference summaries for entities in a personal knowledge graph. \
Given the evidence block, return JSON: {\"summary\": \"...\"}. \
The summary is 2-4 sentences, third person, present tense where still true. \
State only what the evidence supports — never speculate or embellish. \
Prefer concrete specifics (roles, affiliations, projects, meeting cadence, current status) \
over generic filler. Do not mention the evidence block or the knowledge graph itself.";

/// Generate and store the summary for one node. Returns false if the node has
/// no evidence worth summarizing.
pub fn summarize_node(conn: &Connection, chat: &OllamaChat, node_id: &str) -> Result<bool> {
    let Some(evidence) = summary_evidence(conn, node_id)? else {
        return Ok(false);
    };
    let v = chat.complete_json(SYSTEM_PROMPT, &evidence)?;
    let summary = v["summary"].as_str().unwrap_or_default().trim().to_string();
    if summary.is_empty() {
        return Ok(false);
    }
    context::set_summary(conn, node_id, &summary)?;
    Ok(true)
}

/// Refresh up to `limit` stale summaries, busiest scopes first. Per-node
/// failures are collected, not fatal — one bad generation must not stall the
/// nightly pipeline.
pub fn refresh_summaries(
    conn: &Connection,
    chat: &OllamaChat,
    limit: usize,
) -> Result<SummarizeReport> {
    let mut report = SummarizeReport::default();
    for id in stale_summary_nodes(conn, limit)? {
        match summarize_node(conn, chat, &id) {
            Ok(true) => report.refreshed += 1,
            Ok(false) => {}
            Err(e) => report.errors.push(format!("{id}: {e}")),
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{add_mention, upsert_episode, Episode};
    use crate::graph::{upsert_node, Node};

    fn ep(sid: &str, at: &str, sensitivity: &str, body: &str) -> Episode {
        Episode {
            id: 0,
            uid: String::new(),
            source: "note".into(),
            source_id: sid.into(),
            source_ref: None,
            body: body.into(),
            occurred_at: at.into(),
            occurred_end: None,
            ingested_at: String::new(),
            lat: None,
            lon: None,
            location: None,
            sensitivity: sensitivity.into(),
            scope_id: None,
            meta: None,
            raw: None,
        }
    }

    fn seed(conn: &Connection, node: &str, n: usize, sensitivity: &str) {
        for k in 0..n {
            let (id, _) = upsert_episode(
                conn,
                &ep(
                    &format!("{node}-{k}"),
                    "2026-01-01 10:00:00",
                    sensitivity,
                    &format!("episode {k} about {node}"),
                ),
            )
            .unwrap();
            add_mention(conn, id, node, "manual", 1.0).unwrap();
        }
    }

    #[test]
    fn test_stale_selection_lifecycle() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        upsert_node(&conn, &Node::new("thin", "project", "Thin")).unwrap();
        upsert_node(&conn, &Node::new("t1", "task", "A task")).unwrap();
        seed(&conn, "aim2", 4, "personal");
        seed(&conn, "thin", 2, "personal"); // below the episode floor
        seed(&conn, "t1", 4, "personal"); // type excluded

        assert_eq!(stale_summary_nodes(&conn, 10).unwrap(), vec!["aim2"]);

        // A stored summary satisfies the node until newer episodes arrive.
        context::set_summary(&conn, "aim2", "A project.").unwrap();
        assert!(stale_summary_nodes(&conn, 10).unwrap().is_empty());

        // New episode with a later ingested_at re-stales it.
        conn.execute(
            "UPDATE node_context SET summary_updated_at = '2020-01-01 00:00:00'
             WHERE node_id = 'aim2'",
            [],
        )
        .unwrap();
        assert_eq!(stale_summary_nodes(&conn, 10).unwrap(), vec!["aim2"]);
    }

    #[test]
    fn test_evidence_excludes_private_episodes() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p", "project", "Project P")).unwrap();
        seed(&conn, "p", 3, "personal");
        let (id, _) = upsert_episode(
            &conn,
            &ep(
                "secret",
                "2026-01-02 10:00:00",
                "private",
                "the private matter",
            ),
        )
        .unwrap();
        add_mention(&conn, id, "p", "manual", 1.0).unwrap();

        let ev = summary_evidence(&conn, "p").unwrap().unwrap();
        assert!(ev.contains("episode 0 about p"));
        assert!(
            !ev.contains("private matter"),
            "private episodes must not feed summaries"
        );
    }
}
