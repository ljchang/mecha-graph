//! Eval harness (§11): build the ruler before the thing it measures.
//! Gold queries live at `~/.mecha-graph/eval/gold.jsonl` — outside the
//! repository, because they are mined from real episodes. See
//! [`default_gold_path`]. `run` reports recall@10 and MRR
//! per job so a change that helps one job and hurts another is visible.

/// Default gold-set path: `~/.mecha-graph/eval/gold.jsonl`, overridable via
/// `MECHA_GRAPH_GOLD`.
///
/// **Outside the repository, because the gold set is not code.** Its queries
/// are mined from real episodes, so it was one of ten files an export script
/// stripped before this repo could be published — and keeping it in-tree was
/// the reason a second, private checkout had to exist at all. Same shape as
/// [`crate::db::default_db_path`]: the data lives in `~/.mecha-graph/`, the
/// code lives here, and nothing has to be stripped on the way out.
pub fn default_gold_path() -> std::path::PathBuf {
    gold_path_from(
        std::env::var("MECHA_GRAPH_GOLD").ok().as_deref(),
        std::env::var("HOME").ok().as_deref(),
    )
}

/// The resolution rule, as a pure function of its two inputs.
///
/// Split out so it can be tested without touching process state. The first
/// draft of that test called `set_var("HOME", ..)` and broke an unrelated
/// test in `similar` — Rust runs a crate's tests as threads of one process,
/// so an env write is not test-local however carefully the test is written.
/// A pure function needs no such care.
pub fn gold_path_from(env_override: Option<&str>, home: Option<&str>) -> std::path::PathBuf {
    if let Some(p) = env_override {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from(home.unwrap_or("."))
        .join(".mecha-graph")
        .join("eval")
        .join("gold.jsonl")
}

use crate::embed::Embedder;
use crate::error::Result;
use crate::router;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoldQuery {
    pub query: String,
    /// memory | tasks | insight — the three jobs (§1).
    pub job: String,
    /// Expected item ids: episode uids, node ids, or `source:source_id`
    /// references (resolved at run time so gold files survive re-ingest).
    #[serde(default)]
    pub expect_ids: Vec<String>,
    /// Alternative: a substring the top result's text must contain.
    #[serde(default)]
    pub expect_contains: Option<String>,
    #[serde(default)]
    pub note: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct QueryResult {
    pub query: String,
    pub job: String,
    pub hit_rank: Option<usize>, // 1-based rank of first correct item
    pub recall_at_10: bool,
}

#[derive(Debug, Serialize)]
pub struct EvalReport {
    pub per_job: Vec<JobMetrics>,
    pub results: Vec<QueryResult>,
}

#[derive(Debug, Serialize)]
pub struct JobMetrics {
    pub job: String,
    pub n: usize,
    pub recall_at_10: f64,
    pub mrr: f64,
}

/// Resolve `source:source_id` gold refs to episode uids.
fn resolve_expect(conn: &Connection, id: &str) -> String {
    if let Some((source, source_id)) = id.split_once(':') {
        if let Ok(uid) = conn.query_row(
            "SELECT uid FROM episode WHERE source = ?1 AND source_id = ?2",
            rusqlite::params![source, source_id],
            |r| r.get::<_, String>(0),
        ) {
            return uid;
        }
    }
    id.to_string()
}

pub fn load_gold(path: &std::path::Path) -> Result<Vec<GoldQuery>> {
    let text = std::fs::read_to_string(path)?;
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let q: GoldQuery = serde_json::from_str(line)
            .map_err(|e| crate::error::Error::Parse(format!("gold.jsonl line {}: {e}", i + 1)))?;
        out.push(q);
    }
    Ok(out)
}

pub fn run(
    conn: &Connection,
    embedder: Option<&Embedder>,
    gold: &[GoldQuery],
) -> Result<EvalReport> {
    let mut results = Vec::new();

    for g in gold {
        let pack = router::query(conn, embedder, &g.query, 10, 8000, true, None)?;
        let expected: Vec<String> = g
            .expect_ids
            .iter()
            .map(|e| resolve_expect(conn, e))
            .collect();

        let hit_rank = pack.items.iter().position(|item| {
            let id_hit = expected.iter().any(|e| e == &item.id);
            let text_hit = g
                .expect_contains
                .as_ref()
                .is_some_and(|s| item.text.to_lowercase().contains(&s.to_lowercase()));
            id_hit || text_hit
        });

        results.push(QueryResult {
            query: g.query.clone(),
            job: g.job.clone(),
            hit_rank: hit_rank.map(|r| r + 1),
            recall_at_10: hit_rank.is_some_and(|r| r < 10),
        });
    }

    let jobs: Vec<String> = {
        let mut j: Vec<String> = results.iter().map(|r| r.job.clone()).collect();
        j.sort();
        j.dedup();
        j
    };
    let per_job = jobs
        .into_iter()
        .map(|job| {
            let rs: Vec<&QueryResult> = results.iter().filter(|r| r.job == job).collect();
            let n = rs.len();
            let recall = rs.iter().filter(|r| r.recall_at_10).count() as f64 / n.max(1) as f64;
            let mrr = rs
                .iter()
                .map(|r| r.hit_rank.map_or(0.0, |k| 1.0 / k as f64))
                .sum::<f64>()
                / n.max(1) as f64;
            JobMetrics {
                job,
                n,
                recall_at_10: recall,
                mrr,
            }
        })
        .collect();

    Ok(EvalReport { per_job, results })
}

#[cfg(test)]
mod gold_path_tests {
    use super::gold_path_from;
    use std::path::PathBuf;

    /// No env mutation: the rule is a pure function, so the test cannot
    /// disturb a test running beside it.
    #[test]
    fn the_gold_path_defaults_outside_the_repo_and_yields_to_the_env() {
        let p = gold_path_from(None, Some("/home/someone"));
        assert_eq!(
            p,
            PathBuf::from("/home/someone/.mecha-graph/eval/gold.jsonl")
        );
        // No second assert on the same value: `assert_eq!` above pins `p`
        // exactly, so `!p.starts_with("eval/")` had no input that could fail
        // it. A test that cannot fail is not a check.

        assert_eq!(
            gold_path_from(Some("/tmp/other-gold.jsonl"), Some("/home/someone")),
            PathBuf::from("/tmp/other-gold.jsonl"),
            "the env override must win, so a checkout can point at a fixture"
        );

        // No HOME is not a panic: it degrades to a relative path, the same
        // fallback `db::default_db_path` uses.
        assert_eq!(
            gold_path_from(None, None),
            PathBuf::from("./.mecha-graph/eval/gold.jsonl")
        );
    }
}
