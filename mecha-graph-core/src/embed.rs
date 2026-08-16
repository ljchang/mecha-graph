//! Embedding client: Ollama HTTP API with `nomic-embed-text` (768 dims), per
//! the verified environment (§3). Cheap ingestion is separated from expensive
//! embedding — call [`embed_pending`] in batches, e.g. nightly (§5.4).

use crate::error::{Error, Result};
use rusqlite::{params, Connection};

pub const EMBED_DIMS: usize = 768;

pub struct OllamaEmbedder {
    pub base_url: String,
    pub model: String,
}

impl Default for OllamaEmbedder {
    fn default() -> Self {
        OllamaEmbedder {
            base_url: std::env::var("MECHA_GRAPH_OLLAMA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            model: std::env::var("MECHA_GRAPH_EMBED_MODEL")
                .unwrap_or_else(|_| "nomic-embed-text".to_string()),
        }
    }
}

impl OllamaEmbedder {
    /// Embed a batch of texts. nomic-embed-text expects task prefixes:
    /// `search_document:` for corpus text, `search_query:` for queries.
    pub fn embed(&self, texts: &[String], is_query: bool) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let prefix = if is_query {
            "search_query: "
        } else {
            "search_document: "
        };
        let inputs: Vec<String> = texts
            .iter()
            .map(|t| {
                // Truncate very long bodies; nomic's window is ~8k tokens.
                let t: String = t.chars().take(8000).collect();
                format!("{prefix}{t}")
            })
            .collect();

        let resp = ureq::post(&format!("{}/api/embed", self.base_url))
            .send_json(serde_json::json!({ "model": self.model, "input": inputs }))
            .map_err(|e| Error::Embed(format!("ollama request failed: {e}")))?;

        let body: serde_json::Value = resp
            .into_json()
            .map_err(|e| Error::Embed(format!("bad ollama response: {e}")))?;

        let embeddings = body["embeddings"]
            .as_array()
            .ok_or_else(|| Error::Embed("no embeddings in response".into()))?;

        embeddings
            .iter()
            .map(|e| {
                e.as_array()
                    .ok_or_else(|| Error::Embed("embedding not an array".into()))
                    .map(|v| {
                        v.iter()
                            .filter_map(|x| x.as_f64().map(|f| f as f32))
                            .collect()
                    })
            })
            .collect()
    }

    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        Ok(self
            .embed(&[text.to_string()], true)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embed("empty embedding batch".into()))?)
    }

    pub fn available(&self) -> bool {
        ureq::get(&format!("{}/api/tags", self.base_url))
            .timeout(std::time::Duration::from_millis(1500))
            .call()
            .is_ok()
    }
}

fn vec_to_json(v: &[f32]) -> String {
    // sqlite-vec accepts vectors as JSON text.
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// Embed episodes that don't yet have a vector. Returns count embedded.
pub fn embed_pending_episodes(
    conn: &Connection,
    embedder: &OllamaEmbedder,
    limit: usize,
    batch_size: usize,
) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.body FROM episode e
         WHERE e.id NOT IN (SELECT episode_id FROM vec_episode)
         ORDER BY e.id LIMIT ?1",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut total = 0;
    for chunk in rows.chunks(batch_size.max(1)) {
        let texts: Vec<String> = chunk.iter().map(|(_, b)| b.clone()).collect();
        let vecs = embedder.embed(&texts, false)?;
        for ((id, _), vec) in chunk.iter().zip(vecs.iter()) {
            conn.execute(
                "INSERT OR REPLACE INTO vec_episode (episode_id, embedding) VALUES (?1, ?2)",
                params![id, vec_to_json(vec)],
            )?;
            total += 1;
        }
    }
    Ok(total)
}

/// Embed facts (their NL `statement` — the embed target, §4.3) lacking vectors.
pub fn embed_pending_facts(
    conn: &Connection,
    embedder: &OllamaEmbedder,
    limit: usize,
    batch_size: usize,
) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.statement FROM fact f
         WHERE f.id NOT IN (SELECT fact_id FROM vec_fact)
         ORDER BY f.id LIMIT ?1",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut total = 0;
    for chunk in rows.chunks(batch_size.max(1)) {
        let texts: Vec<String> = chunk.iter().map(|(_, s)| s.clone()).collect();
        let vecs = embedder.embed(&texts, false)?;
        for ((id, _), vec) in chunk.iter().zip(vecs.iter()) {
            conn.execute(
                "INSERT OR REPLACE INTO vec_fact (fact_id, embedding) VALUES (?1, ?2)",
                params![id, vec_to_json(vec)],
            )?;
            total += 1;
        }
    }
    Ok(total)
}
