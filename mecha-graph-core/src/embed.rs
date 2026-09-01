//! Embedding client: an OpenAI-compatible `/v1/embeddings` server (llama-server).
//!
//! Replaced the ollama client on 2026-08-20, alongside the chat path — see
//! [`crate::llm`] for why the box stopped running two inference engines.
//!
//! Three things here are load-bearing and were not obvious:
//!
//! **The dimension is configuration, not a constant.** It used to be a
//! `const EMBED_DIMS: usize = 768` beside a `FLOAT[768]` baked into the `vec0`
//! DDL — two copies of one fact, and the constant was read by nothing, so only
//! the DDL was real. [`ensure_vec_dims`] makes the table the single source of
//! truth: it reads the declared width back out of `sqlite_master` and rebuilds
//! the vector tables when it disagrees with the configured model.
//!
//! **Changing the embedder invalidates every stored vector, silently.** There
//! is no version marker inside a vector and no way to tell a nomic 768 from a
//! Qwen3-truncated 768 by looking at one. So the (model, instruction, dims)
//! triple is recorded in `embed_meta` at write time and can be checked at read
//! time — a mismatch is an error, not a slow drift in what a cosine means.
//!
//! **Cosine scales are not comparable across models, and thresholds encode
//! one.** Measured on identical text — the same claim in different words,
//! versus unrelated text:
//!
//!   nomic-embed-text-v1.5   same 0.8650   unrelated 0.5579   gap 0.3071
//!   Qwen3-Embedding-0.6B    same 0.6926   unrelated 0.2786   gap 0.4140
//!
//! `precheck::SEMANTIC_DUP_THRESHOLD` is 0.93 — a number that only means
//! "duplicate" on nomic's compressed scale, where even unrelated text sits at
//! 0.56. Carried onto a model whose genuine paraphrases score 0.69, it would
//! silently stop matching anything. Recalibration is part of a model change,
//! not an optimisation to do afterwards.

use crate::error::{Error, Result};
use rusqlite::{params, Connection};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// Where the embedding server listens. Deliberately NOT :8080 — that is the
/// chat model, and llama-server serves one model per process.
pub const DEFAULT_EMBED_URL: &str = "http://127.0.0.1:8081";

/// Fallback when nothing is configured. 768 is what the store already holds
/// (nomic-embed-text-v1.5), so an unconfigured install keeps working.
pub const DEFAULT_EMBED_DIMS: usize = 768;

/// What an embedding is *for*. The old signature was `is_query: bool`, which is
/// this enum with two variants and no room for the third.
///
/// `Dedup` exists because `precheck` compares statement against statement —
/// symmetric semantic similarity — while embedding both sides as retrieval
/// *documents*. A `search_document:` representation is trained to make a
/// passage findable by a query, not to place two phrasings of one claim close
/// together, which is plausibly why the duplicate threshold has to sit as high
/// as it does. Instruction-aware models can express the difference; nomic's two
/// fixed prefixes could not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbedTask {
    /// Corpus text. Instruction-free for the Qwen and Harrier families, which
    /// put the instruction on the query side only.
    Document,
    /// A search query.
    Retrieval,
    /// "Is this the same claim?" — precheck's question.
    Dedup,
}

impl EmbedTask {
    /// Qwen3-Embedding format: `Instruct: {task}\nQuery: {text}`. A different
    /// model family words these differently, and a wrong instruction is a
    /// silent quality loss rather than an error — which is why the instruction
    /// is part of the index identity recorded in `embed_meta`.
    fn default_instruction(self) -> Option<&'static str> {
        match self {
            EmbedTask::Document => None,
            EmbedTask::Retrieval => Some(
                "Given a search query, retrieve relevant facts and episodes \
                 from a personal knowledge graph",
            ),
            EmbedTask::Dedup => {
                Some("Retrieve statements that assert the same fact as the given statement")
            }
        }
    }

    pub fn tag(self) -> &'static str {
        match self {
            EmbedTask::Document => "document",
            EmbedTask::Retrieval => "retrieval",
            EmbedTask::Dedup => "dedup",
        }
    }
}

pub struct Embedder {
    pub base_url: String,
    pub model: String,
    pub dims: usize,
    /// Characters kept per input. Generous by default: the models in play carry
    /// 32k-token windows, where nomic's 8k was the reason for the old
    /// 8,000-char clip. Still bounded, because one pathological episode should
    /// not become one pathological request.
    pub max_chars: usize,
    timeout: Duration,
}

impl Default for Embedder {
    fn default() -> Self {
        let cfg = crate::integrations::load_config()
            .map(|c| c.llm)
            .unwrap_or_default();
        Embedder {
            base_url: std::env::var("MECHA_GRAPH_EMBED_URL")
                .ok()
                .or(cfg.embed_url)
                .unwrap_or_else(|| DEFAULT_EMBED_URL.to_string()),
            model: std::env::var("MECHA_GRAPH_EMBED_MODEL")
                .ok()
                .or(cfg.embed_model)
                .unwrap_or_else(|| "embed".to_string()),
            dims: std::env::var("MECHA_GRAPH_EMBED_DIMS")
                .ok()
                .and_then(|s| s.parse().ok())
                .or(cfg.embed_dims)
                .unwrap_or(DEFAULT_EMBED_DIMS),
            max_chars: std::env::var("MECHA_GRAPH_EMBED_MAX_CHARS")
                .ok()
                .and_then(|s| s.parse().ok())
                .or(cfg.embed_max_chars)
                .unwrap_or(24_000),
            timeout: Duration::from_secs(300),
        }
    }
}

impl Embedder {
    /// Embed a batch. Returns one vector per input, in order.
    ///
    /// A batch is one HTTP request and llama-server bills the whole request
    /// against `-c`, so a batch of ordinarily-sized inputs can overflow a
    /// context that every individual input fits in comfortably. That is a
    /// property of the batching, not of the data, and it killed a full
    /// re-embed at 348 s in on a single 9,292-token request. Splitting and
    /// retrying converges on the one input that genuinely does not fit, which
    /// then fails with a message about that input — instead of aborting a
    /// corpus-wide job and leaving a half-written index that still answers
    /// queries.
    pub fn embed(&self, texts: &[String], task: EmbedTask) -> Result<Vec<Vec<f32>>> {
        match self.embed_once(texts, task) {
            Ok(v) => Ok(v),
            Err(e) if texts.len() > 1 && is_context_overflow(&e) => {
                let mid = texts.len() / 2;
                let mut out = self.embed(&texts[..mid], task)?;
                out.extend(self.embed(&texts[mid..], task)?);
                Ok(out)
            }
            Err(e) => Err(e),
        }
    }

    fn embed_once(&self, texts: &[String], task: EmbedTask) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }
        let instruction = task.default_instruction();
        let inputs: Vec<String> = texts
            .iter()
            .map(|t| {
                let t: String = t.chars().take(self.max_chars).collect();
                match instruction {
                    Some(i) => format!("Instruct: {i}\nQuery: {t}"),
                    None => t,
                }
            })
            .collect();

        let resp = ureq::post(&format!("{}/v1/embeddings", self.base_url))
            .timeout(self.timeout)
            .send_json(serde_json::json!({ "model": self.model, "input": inputs }))
            .map_err(|e| match e {
                ureq::Error::Status(code, r) => {
                    let detail = r.into_string().unwrap_or_default();
                    Error::Embed(format!(
                        "embedding server {code}: {}",
                        detail.chars().take(300).collect::<String>()
                    ))
                }
                other => Error::Embed(format!(
                    "embedding server at {} unreachable: {other}. Start one with \
                     `llama-server -m <gguf> --port 8081 --embeddings --pooling last`.",
                    self.base_url
                )),
            })?;

        let body: serde_json::Value = resp
            .into_json()
            .map_err(|e| Error::Embed(format!("bad embedding response: {e}")))?;
        let data = body["data"]
            .as_array()
            .ok_or_else(|| Error::Embed("no data[] in embedding response".into()))?;
        if data.len() != inputs.len() {
            // Position is the only thing tying a vector to its row.
            return Err(Error::Embed(format!(
                "asked for {} embeddings, got {}",
                inputs.len(),
                data.len()
            )));
        }

        let mut out = Vec::with_capacity(data.len());
        for item in data {
            let v: Vec<f32> = item["embedding"]
                .as_array()
                .ok_or_else(|| Error::Embed("embedding not an array".into()))?
                .iter()
                .filter_map(|x| x.as_f64().map(|f| f as f32))
                .collect();
            if v.len() != self.dims {
                // Would otherwise fail later inside sqlite-vec with a message
                // about the table, pointing at the wrong thing entirely.
                return Err(Error::Embed(format!(
                    "model returned {}-dim vectors but embed_dims is {} — the \
                     configured model and the vector tables disagree",
                    v.len(),
                    self.dims
                )));
            }
            out.push(v);
        }
        Ok(out)
    }

    pub fn embed_query(&self, text: &str) -> Result<Vec<f32>> {
        self.embed(&[text.to_string()], EmbedTask::Retrieval)?
            .into_iter()
            .next()
            .ok_or_else(|| Error::Embed("empty embedding batch".into()))
    }

    pub fn available(&self) -> bool {
        ureq::get(&format!("{}/health", self.base_url))
            .timeout(Duration::from_millis(1500))
            .call()
            .is_ok()
    }
}

/// Recognised by message text, because llama-server gives the condition a
/// `type` in the body but the transport only surfaces the status. Same
/// approach, and the same reason, as mecha's `is_context_overflow`.
fn is_context_overflow(e: &Error) -> bool {
    let s = e.to_string();
    s.contains("exceed_context_size") || s.contains("exceeds the available context size")
}

/// The declared width of the vector tables, read back out of the schema. The
/// table is the source of truth: a constant beside it can be, and was, wrong.
pub fn declared_vec_dims(conn: &Connection) -> Result<Option<usize>> {
    declared_vec_dims_in(conn, "main")
}

/// The same, for an ATTACHed database.
///
/// Needed because `db::export_plaintext` builds its destination with
/// `run_migrations`, which creates the vector tables at the default width, and
/// then copies rows out of a source that may have been rebuilt to a different
/// one by a model change. The failure is a sqlite-vec "Dimension mismatch …
/// expected 768, received 1024" that names the column and not the cause.
pub fn declared_vec_dims_in(conn: &Connection, schema: &str) -> Result<Option<usize>> {
    use rusqlite::OptionalExtension;
    let sql: Option<String> = conn
        .query_row(
            &format!("SELECT sql FROM {schema}.sqlite_master WHERE name = 'vec_episode'"),
            [],
            |r| r.get(0),
        )
        .optional()?;
    let Some(sql) = sql else { return Ok(None) };
    let Some(start) = sql.find("FLOAT[") else {
        return Ok(None);
    };
    let rest = &sql[start + 6..];
    let end = rest.find(']').unwrap_or(0);
    Ok(rest[..end].parse::<usize>().ok())
}

/// Make the vector tables match `dims`, rebuilding them if they do not.
///
/// Destructive by necessity and by design: vectors of a different width, or
/// from a different model, are not convertible, and keeping them would leave a
/// table holding two incompatible geometries with nothing to tell them apart.
/// The caller re-embeds afterwards. Returns true when a rebuild happened.
pub fn ensure_vec_dims(conn: &Connection, dims: usize) -> Result<bool> {
    match declared_vec_dims(conn)? {
        Some(d) if d == dims => Ok(false),
        _ => {
            conn.execute_batch(&format!(
                "DROP TABLE IF EXISTS vec_episode;
                 DROP TABLE IF EXISTS vec_fact;
                 DROP TABLE IF EXISTS vec_rejected;
                 CREATE VIRTUAL TABLE vec_episode USING vec0(episode_id INTEGER PRIMARY KEY, embedding FLOAT[{dims}]);
                 CREATE VIRTUAL TABLE vec_fact    USING vec0(fact_id    INTEGER PRIMARY KEY, embedding FLOAT[{dims}]);
                 CREATE VIRTUAL TABLE vec_rejected USING vec0(candidate_id INTEGER PRIMARY KEY, embedding FLOAT[{dims}]);"
            ))?;
            Ok(true)
        }
    }
}

/// Make `vec_rejected` exist at the store's current vector width.
///
/// V022 creates it at the compiled-in default, but a store whose vectors
/// were rebuilt to another width BEFORE V022 existed gets the migration's
/// default-width table beside non-default siblings — and sqlite-vec's
/// dimension-mismatch error names the column, not the cause. The index is
/// a derivable cache, so on mismatch it is dropped and rebuilt empty; the
/// next `pkg embed` refills it.
pub fn ensure_vec_rejected(conn: &Connection) -> Result<()> {
    use rusqlite::OptionalExtension;
    let Some(dims) = declared_vec_dims(conn)? else {
        return Ok(());
    };
    let sql: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name = 'vec_rejected'",
            [],
            |r| r.get(0),
        )
        .optional()?;
    let current = sql
        .as_deref()
        .and_then(|s| s.find("FLOAT[").map(|i| (s, i)))
        .and_then(|(s, i)| s[i + 6..].split(']').next()?.parse::<usize>().ok());
    if current == Some(dims) {
        return Ok(());
    }
    conn.execute_batch(&format!(
        "DROP TABLE IF EXISTS vec_rejected;
         CREATE VIRTUAL TABLE vec_rejected USING vec0(candidate_id INTEGER PRIMARY KEY, embedding FLOAT[{dims}]);"
    ))?;
    Ok(())
}

/// Embed HUMAN-rejected candidate statements lacking vectors — the
/// incremental build of the semantic rejection memory (review-on-use §5).
/// Machine rejects stay out: precheck's own duplicate-rejections must not
/// enter the memory that judges the next sweep's input.
pub fn embed_pending_rejects(
    conn: &Connection,
    embedder: &Embedder,
    limit: usize,
    batch_size: usize,
) -> Result<usize> {
    ensure_vec_rejected(conn)?;
    let mut stmt = conn.prepare(&format!(
        "SELECT c.id, json_extract(c.payload, '$.statement') AS stmt
         FROM fact_candidate c
         WHERE c.status = 'rejected'
           AND {}
           AND COALESCE(json_extract(c.payload, '$.statement'), '') <> ''
           AND c.id NOT IN (SELECT candidate_id FROM vec_rejected)
         ORDER BY c.id LIMIT ?1",
        crate::ladder::HUMAN_VERDICT_SQL
    ))?;
    let rows: Vec<(i64, String)> = stmt
        .query_map(params![limit as i64], |r| Ok((r.get(0)?, r.get("stmt")?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut total = 0;
    for chunk in rows.chunks(batch_size.max(1)) {
        let texts: Vec<String> = chunk.iter().map(|(_, s)| s.clone()).collect();
        let vecs = embedder.embed(&texts, EmbedTask::Document)?;
        for ((id, _), vec) in chunk.iter().zip(vecs.iter()) {
            conn.execute(
                "INSERT OR REPLACE INTO vec_rejected (candidate_id, embedding) VALUES (?1, ?2)",
                params![id, vec_to_json(vec)],
            )?;
            total += 1;
        }
    }
    Ok(total)
}

/// What a cached candidate vector is keyed on: everything that decides what
/// the numbers mean. The text, the model that produced it, and the task —
/// via its instruction rather than its name, because the instruction is what
/// the server actually saw, and two tasks that share one are interchangeable.
///
/// Hashed rather than spread across columns so a miss is one comparison and
/// no reader has to know the list. Adding a dimension to the identity later
/// means adding it here, and every existing row becomes a miss on its own —
/// which is the correct behaviour, and needs no migration to get it.
fn candidate_key(model: &str, task: EmbedTask, text: &str) -> String {
    let mut hasher = Sha256::new();
    // Length-prefixed: a statement is arbitrary text and may contain whatever
    // byte a delimiter would use, so fields are framed rather than joined.
    for part in [model, task.default_instruction().unwrap_or(""), text] {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{:x}", hasher.finalize())
}

/// Embed candidate statements, reusing vectors already stored for them.
///
/// The review queue's grouping calls this instead of [`Embedder::embed`].
/// One vector per input, in input order, exactly as `embed` returns — the
/// caller cannot tell a hit from a miss, which is the point: the clustering
/// downstream is unchanged and its output is identical either way.
///
/// A pending statement's text is fixed while it waits, so its vector is too,
/// and the queue turns over slowly — one sweep's new candidates against
/// thousands already sitting there. That is the difference between embedding
/// the whole queue every time somebody looks at it and embedding only what
/// arrived since.
///
/// **Falls through to a plain embed on any storage trouble.** The cache is
/// derivable and this is a read path: a store that cannot be read or written
/// — opened read-only, mid-migration, locked by another writer — must give a
/// slow grouping, never a failed one. The one thing it must not do is answer
/// with vectors it is not sure about, which is what the key is for.
pub fn embed_candidates(
    conn: &Connection,
    embedder: &Embedder,
    ids: &[i64],
    texts: &[String],
    task: EmbedTask,
) -> Result<Vec<Vec<f32>>> {
    if ids.len() != texts.len() {
        // The caller zipped two lists that disagree. Position is the only
        // thing tying a vector to its candidate, so this cannot be guessed at.
        return Err(Error::Embed(format!(
            "embed_candidates got {} ids for {} texts",
            ids.len(),
            texts.len()
        )));
    }
    if texts.is_empty() {
        return Ok(vec![]);
    }
    let keys: Vec<String> = texts
        .iter()
        .map(|t| candidate_key(&embedder.model, task, t))
        .collect();

    // Hits, by position. A row whose key has moved on is simply absent here,
    // gets re-embedded below, and overwrites itself on the way out.
    let mut cached: Vec<Option<Vec<f32>>> = vec![None; texts.len()];
    if let Ok(mut stmt) = conn.prepare_cached(
        "SELECT embedding FROM candidate_embedding
         WHERE candidate_id = ?1 AND text_hash = ?2 AND dims = ?3",
    ) {
        for (i, id) in ids.iter().enumerate() {
            let row: std::result::Result<Vec<u8>, _> =
                stmt.query_row(params![id, &keys[i], embedder.dims as i64], |r| r.get(0));
            // A stored vector of the wrong width is a corrupt row, not a hit.
            // Dropping it costs one embed and keeps a malformed cache out of
            // the clustering, where a short vector would quietly change every
            // cosine it touched.
            if let Ok(bytes) = row {
                cached[i] = vec_from_bytes(&bytes, embedder.dims);
            }
        }
    }

    let misses: Vec<usize> = (0..texts.len()).filter(|i| cached[*i].is_none()).collect();
    if !misses.is_empty() {
        let batch: Vec<String> = misses.iter().map(|i| texts[*i].clone()).collect();
        // `embed` already bisects a batch that overflows the server's context,
        // so the misses go through it whole rather than being re-chunked here.
        let fresh = embedder.embed(&batch, task)?;
        for (slot, vec) in misses.iter().zip(fresh) {
            let _ = conn.execute(
                "INSERT OR REPLACE INTO candidate_embedding
                     (candidate_id, text_hash, dims, embedding, written_at)
                 VALUES (?1, ?2, ?3, ?4, datetime('now'))",
                params![
                    ids[*slot],
                    &keys[*slot],
                    embedder.dims as i64,
                    vec_to_bytes(&vec)
                ],
            );
            cached[*slot] = Some(vec);
        }
    }

    // Every slot is filled: a miss was either embedded or the `?` above
    // returned. `unwrap_or_default` is unreachable, and an empty vector would
    // be dropped by the clustering's own width check rather than scored.
    Ok(cached.into_iter().map(Option::unwrap_or_default).collect())
}

/// Drop cached vectors for candidates that have left the queue.
///
/// A verdict is what makes a row dead: the grouping only ever asks about
/// `proposed` candidates, so anything else is weight that will never be asked
/// for again. Run from the grouping itself rather than from a maintenance
/// verb, because the queue is emptied by people who will not run one, and
/// this is a single delete against work measured in tens of seconds.
///
/// Best-effort by construction: a store that cannot be written still groups.
pub fn prune_candidate_embeddings(conn: &Connection) -> usize {
    conn.execute(
        "DELETE FROM candidate_embedding
         WHERE candidate_id NOT IN (SELECT id FROM fact_candidate WHERE status = 'proposed')",
        [],
    )
    .unwrap_or(0)
}

/// Little-endian `f32`s, the storage format of `candidate_embedding`. Not
/// `vec_to_json`: nothing parses this table's vectors on sqlite's behalf, so
/// it holds them raw at a third of the size. Fixed-endian rather than native
/// because a store is a file people move between machines.
fn vec_to_bytes(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

/// A stored vector, if it is intact and the width the caller expects. A
/// partial write, a truncated blob, or a row left by another embedding model
/// all land here as `None`, which is a miss and re-embeds.
fn vec_from_bytes(bytes: &[u8], dims: usize) -> Option<Vec<f32>> {
    (bytes.len() == dims * 4).then(|| {
        bytes
            .chunks_exact(4)
            .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
            .collect()
    })
}

/// Record what produced the vectors currently in the store.
///
/// Without this, swapping a model leaves a table of numbers that look fine,
/// answer queries, and mean something different than the thresholds reading
/// them assume. Nothing about a vector reveals that from the outside.
pub fn set_embed_meta(
    conn: &Connection,
    model: &str,
    dims: usize,
    instruction: &str,
) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS embed_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            model TEXT NOT NULL, dims INTEGER NOT NULL,
            instruction TEXT NOT NULL, written_at TEXT NOT NULL DEFAULT (datetime('now')))",
    )?;
    conn.execute(
        "INSERT INTO embed_meta (id, model, dims, instruction, written_at)
         VALUES (1, ?1, ?2, ?3, datetime('now'))
         ON CONFLICT(id) DO UPDATE SET model=?1, dims=?2, instruction=?3, written_at=datetime('now')",
        params![model, dims as i64, instruction],
    )?;
    Ok(())
}

pub fn get_embed_meta(conn: &Connection) -> Result<Option<(String, usize, String)>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT model, dims, instruction FROM embed_meta WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, String>(0)?,
                    r.get::<_, i64>(1)? as usize,
                    r.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .unwrap_or(None))
}

fn vec_to_json(v: &[f32]) -> String {
    // sqlite-vec accepts vectors as JSON text.
    serde_json::to_string(v).unwrap_or_else(|_| "[]".to_string())
}

/// Embed episodes that don't yet have a vector. Returns count embedded.
pub fn embed_pending_episodes(
    conn: &Connection,
    embedder: &Embedder,
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
        let vecs = embedder.embed(&texts, EmbedTask::Document)?;
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
    embedder: &Embedder,
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
        let vecs = embedder.embed(&texts, EmbedTask::Document)?;
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

#[cfg(test)]
mod cache_tests {
    use super::*;

    /// An embedder that cannot reach anything. Any call that actually needs
    /// the server fails, which is what makes "did not embed" an assertion
    /// rather than a hope — the alternative, counting requests against a live
    /// llama-server, is the kind of test that passes when it is skipped.
    fn unreachable_embedder() -> Embedder {
        Embedder {
            // Port 0 is never listening.
            base_url: "http://127.0.0.1:0".into(),
            model: "test-embed".into(),
            dims: 4,
            max_chars: 1000,
            timeout: Duration::from_millis(200),
        }
    }

    fn store(conn: &Connection, e: &Embedder, id: i64, text: &str, v: &[f32]) {
        conn.execute(
            "INSERT OR REPLACE INTO candidate_embedding
                 (candidate_id, text_hash, dims, embedding, written_at)
             VALUES (?1, ?2, ?3, ?4, datetime('now'))",
            params![
                id,
                candidate_key(&e.model, EmbedTask::Document, text),
                e.dims as i64,
                vec_to_bytes(v)
            ],
        )
        .unwrap();
    }

    /// **The queue is embedded once, not once per look.**
    ///
    /// Grouping the review queue re-embedded every pending statement on every
    /// call — the whole queue, measured in tens of seconds, paid again for
    /// each threshold the stepper visited and each group a reviewer stepped
    /// out of. A pending statement's text does not change while it waits, so
    /// the second call has nothing to ask the server.
    ///
    /// Proven by making the server unreachable: before the cache this call
    /// could only fail.
    #[test]
    fn a_cached_candidate_is_never_embedded_again() {
        let conn = crate::db::open_memory().unwrap();
        let e = unreachable_embedder();
        store(&conn, &e, 1, "Sage plays the cello", &[1.0, 0.0, 0.0, 0.0]);
        store(&conn, &e, 2, "Sage plays cello", &[0.0, 1.0, 0.0, 0.0]);

        let got = embed_candidates(
            &conn,
            &e,
            &[1, 2],
            &["Sage plays the cello".into(), "Sage plays cello".into()],
            EmbedTask::Document,
        )
        .expect("a fully cached batch must not reach the embedding server");
        assert_eq!(
            got,
            vec![vec![1.0, 0.0, 0.0, 0.0], vec![0.0, 1.0, 0.0, 0.0]]
        );
    }

    /// A hit is keyed on the text, not the id. A candidate whose statement
    /// was edited must be re-embedded rather than answered with the vector of
    /// what it used to say — a wrong vector does not fail, it silently
    /// regroups the queue around a statement nobody wrote.
    #[test]
    fn an_edited_statement_is_a_miss() {
        let conn = crate::db::open_memory().unwrap();
        let e = unreachable_embedder();
        store(&conn, &e, 1, "Sage plays the cello", &[1.0, 0.0, 0.0, 0.0]);

        let err = embed_candidates(
            &conn,
            &e,
            &[1],
            &["Sage plays the viola".into()],
            EmbedTask::Document,
        );
        assert!(err.is_err(), "an edited statement must not hit the cache");
    }

    /// Same for the model and the task's instruction: the vectors of two
    /// models are not comparable, and nothing about a stored row reveals
    /// which one wrote it. Both ride in the key, so a swap invalidates by
    /// construction instead of by anyone remembering to clear a table.
    #[test]
    fn another_model_or_task_does_not_read_this_cache() {
        let conn = crate::db::open_memory().unwrap();
        let e = unreachable_embedder();
        store(&conn, &e, 1, "Sage plays the cello", &[1.0, 0.0, 0.0, 0.0]);

        let mut swapped = unreachable_embedder();
        swapped.model = "some-other-embed".into();
        assert!(
            embed_candidates(
                &conn,
                &swapped,
                &[1],
                &["Sage plays the cello".into()],
                EmbedTask::Document
            )
            .is_err(),
            "a different model must not read another model's vectors"
        );

        // Document carries no instruction and Dedup does, so the two ask the
        // server different questions about the same text.
        assert!(
            embed_candidates(
                &conn,
                &e,
                &[1],
                &["Sage plays the cello".into()],
                EmbedTask::Dedup
            )
            .is_err(),
            "a different embed task must not read Document's vectors"
        );
    }

    /// A row of the wrong width is corrupt, not a hit. Letting it through
    /// would put a short vector into the clustering, where it changes every
    /// cosine it touches without failing anything.
    #[test]
    fn a_vector_of_the_wrong_width_is_not_a_hit() {
        let conn = crate::db::open_memory().unwrap();
        let e = unreachable_embedder();
        conn.execute(
            "INSERT INTO candidate_embedding (candidate_id, text_hash, dims, embedding)
             VALUES (1, ?1, 4, ?2)",
            params![
                candidate_key(&e.model, EmbedTask::Document, "Sage plays the cello"),
                // Two floats where the row claims four — a truncated write.
                vec_to_bytes(&[1.0, 0.0])
            ],
        )
        .unwrap();
        assert!(
            embed_candidates(
                &conn,
                &e,
                &[1],
                &["Sage plays the cello".into()],
                EmbedTask::Document
            )
            .is_err(),
            "a stored vector that is not `dims` wide must be re-embedded"
        );
    }

    /// The cache does not outlive the queue: a candidate that has been
    /// judged will never be grouped again, so its vector is weight.
    #[test]
    fn a_judged_candidate_loses_its_cached_vector() {
        let conn = crate::db::open_memory().unwrap();
        let e = unreachable_embedder();
        for (id, status) in [(1, "proposed"), (2, "accepted"), (3, "rejected")] {
            conn.execute(
                "INSERT INTO fact_candidate (id, payload, status, created_at)
                 VALUES (?1, json_object('statement', 'x'), ?2, datetime('now'))",
                params![id, status],
            )
            .unwrap();
            store(&conn, &e, id, "x", &[1.0, 0.0, 0.0, 0.0]);
        }
        assert_eq!(prune_candidate_embeddings(&conn), 2);
        let left: i64 = conn
            .query_row("SELECT candidate_id FROM candidate_embedding", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(left, 1, "the pending candidate keeps its vector");
    }

    /// Position is the only thing tying a vector to its candidate, so two
    /// lists that disagree is a caller bug and cannot be guessed at.
    #[test]
    fn mismatched_ids_and_texts_are_refused() {
        let conn = crate::db::open_memory().unwrap();
        assert!(embed_candidates(
            &conn,
            &unreachable_embedder(),
            &[1, 2],
            &["only one".into()],
            EmbedTask::Document
        )
        .is_err());
    }
}

#[cfg(test)]
mod tests {
    /// The rejected index follows every width rebuild, and re-aligns
    /// itself when it was created at the migration default beside
    /// siblings rebuilt earlier.
    #[test]
    fn vec_rejected_tracks_the_vector_width() {
        let conn = crate::db::open_memory().unwrap();
        let dims_of = |name: &str| -> Option<usize> {
            use rusqlite::OptionalExtension;
            let sql: Option<String> = conn
                .query_row(
                    "SELECT sql FROM sqlite_master WHERE name = ?1",
                    rusqlite::params![name],
                    |r| r.get(0),
                )
                .optional()
                .unwrap();
            let s = sql?;
            let i = s.find("FLOAT[")?;
            s[i + 6..].split(']').next()?.parse().ok()
        };
        assert_eq!(dims_of("vec_rejected"), Some(768), "V022 default");
        assert!(super::ensure_vec_dims(&conn, 1024).unwrap());
        assert_eq!(dims_of("vec_rejected"), Some(1024), "rebuild carries it");
        // Simulate a store rebuilt before V022: recreate at the wrong width.
        conn.execute_batch(
            "DROP TABLE vec_rejected;
             CREATE VIRTUAL TABLE vec_rejected USING vec0(candidate_id INTEGER PRIMARY KEY, embedding FLOAT[768]);",
        )
        .unwrap();
        super::ensure_vec_rejected(&conn).unwrap();
        assert_eq!(
            dims_of("vec_rejected"),
            Some(1024),
            "re-aligned to siblings"
        );
    }

    use super::*;

    #[test]
    fn a_document_carries_no_instruction() {
        // Both candidate families instruct the query side only. Prefixing a
        // document would bake the instruction text into the stored vector.
        assert!(EmbedTask::Document.default_instruction().is_none());
        assert!(EmbedTask::Retrieval.default_instruction().is_some());
    }

    #[test]
    fn dedup_and_retrieval_ask_different_questions() {
        assert_ne!(
            EmbedTask::Dedup.default_instruction(),
            EmbedTask::Retrieval.default_instruction()
        );
    }

    #[test]
    fn the_embed_url_is_not_the_chat_url() {
        // llama-server serves one model per process; pointing these at one port
        // silently sends embedding requests to the chat model.
        assert_ne!(DEFAULT_EMBED_URL, crate::llm::DEFAULT_BASE_URL);
    }

    #[test]
    fn declared_dims_round_trip_through_the_schema() {
        let conn = crate::db::open_memory().expect("memory db");
        // The schema, not a constant, is what the embedder must agree with.
        assert!(ensure_vec_dims(&conn, 1024).expect("rebuild"));
        assert_eq!(declared_vec_dims(&conn).unwrap(), Some(1024));
        assert!(!ensure_vec_dims(&conn, 1024).expect("idempotent"));
        assert!(ensure_vec_dims(&conn, 768).expect("rebuild back"));
        assert_eq!(declared_vec_dims(&conn).unwrap(), Some(768));
    }

    #[test]
    fn a_context_overflow_is_recognised_by_message() {
        // The split-and-retry hinges on this. A false negative aborts a
        // corpus-wide re-embed; a false positive costs one extra request.
        assert!(is_context_overflow(&Error::Embed(
            "embedding server 400: {\"error\":{\"type\":\"exceed_context_size_error\"}}".into()
        )));
        assert!(is_context_overflow(&Error::Embed(
            "request (9292 tokens) exceeds the available context size (8192 tokens)".into()
        )));
        assert!(!is_context_overflow(&Error::Embed(
            "connection refused".into()
        )));
    }

    #[test]
    fn embed_meta_records_the_index_identity() {
        let conn = crate::db::open_memory().expect("memory db");
        set_embed_meta(&conn, "qwen3-embedding-0.6b", 1024, "retrieval-v1").unwrap();
        assert_eq!(
            get_embed_meta(&conn).unwrap(),
            Some(("qwen3-embedding-0.6b".into(), 1024, "retrieval-v1".into()))
        );
        // Overwrite, not append: there is one live index, not a history.
        set_embed_meta(&conn, "other", 768, "x").unwrap();
        assert_eq!(get_embed_meta(&conn).unwrap().unwrap().1, 768);
    }
}
