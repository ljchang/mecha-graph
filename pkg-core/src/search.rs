//! Hybrid search (§8.2): BM25 (FTS5) and vector (sqlite-vec) run independently,
//! fused by rank via Reciprocal Rank Fusion (scores aren't comparable across
//! engines). The entity filter is applied FIRST — never let the vector index
//! see a candidate the entity filter already excluded (§8.1).
//!
//! Note on the FTS5-vs-tantivy open decision (§13): this is the FTS5 arm.
//! RRF fusion is engine-agnostic, so a tantivy arm can be swapped in behind
//! the same interface later.

use crate::embed::OllamaEmbedder;
use crate::error::Result;
use rusqlite::{params, Connection};

pub const RRF_K: f64 = 60.0;

#[derive(Debug, Clone)]
pub struct Hit {
    pub id: i64,
    pub score: f64,
    pub via: &'static str, // "bm25" | "vec" | "bm25+vec"
}

/// Sanitize a user query for FTS5 MATCH: quote each term, drop operators.
/// Terms under 3 chars are dropped — the length check runs BEFORE quoting
/// (it used to run after, which let every 1–2 letter word through as an OR
/// term and drowned rare terms in noise).
fn fts_query(q: &str) -> String {
    q.split_whitespace()
        .filter_map(|t| {
            let clean: String = t.chars().filter(|c| c.is_alphanumeric()).collect();
            (clean.len() > 2).then(|| format!("\"{clean}\""))
        })
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn bm25_ranked(
    conn: &Connection,
    fts_table: &str,
    q: &str,
    candidates: Option<&[i64]>,
    limit: usize,
) -> Result<Vec<i64>> {
    let match_expr = fts_query(q);
    if match_expr.is_empty() {
        return Ok(vec![]);
    }
    let sql = format!(
        "SELECT rowid FROM {fts_table} WHERE {fts_table} MATCH ?1 ORDER BY bm25({fts_table}) LIMIT ?2"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let ids: Vec<i64> = stmt
        .query_map(params![match_expr, limit as i64], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(filter_candidates(ids, candidates))
}

fn vec_ranked(
    conn: &Connection,
    vec_table: &str,
    key_col: &str,
    qvec: &[f32],
    candidates: Option<&[i64]>,
    limit: usize,
) -> Result<Vec<i64>> {
    let qjson = serde_json::to_string(qvec)?;
    let sql = format!(
        "SELECT {key_col} FROM {vec_table} WHERE embedding MATCH ?1 AND k = ?2 ORDER BY distance"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let ids: Vec<i64> = stmt
        .query_map(params![qjson, limit as i64], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(filter_candidates(ids, candidates))
}

/// Filter-first semantics: when a candidate set exists (from `mention`), both
/// arms are constrained to it before fusion.
fn filter_candidates(ids: Vec<i64>, candidates: Option<&[i64]>) -> Vec<i64> {
    match candidates {
        None => ids,
        Some(set) => {
            let allowed: std::collections::HashSet<i64> = set.iter().copied().collect();
            ids.into_iter().filter(|i| allowed.contains(i)).collect()
        }
    }
}

/// Reciprocal Rank Fusion, k=60 (§8.2).
pub fn rrf_fuse(bm25: &[i64], vec: &[i64], limit: usize) -> Vec<Hit> {
    let mut scores: std::collections::HashMap<i64, (f64, bool, bool)> =
        std::collections::HashMap::new();
    for (rank, id) in bm25.iter().enumerate() {
        let e = scores.entry(*id).or_insert((0.0, false, false));
        e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        e.1 = true;
    }
    for (rank, id) in vec.iter().enumerate() {
        let e = scores.entry(*id).or_insert((0.0, false, false));
        e.0 += 1.0 / (RRF_K + rank as f64 + 1.0);
        e.2 = true;
    }
    let mut hits: Vec<Hit> = scores
        .into_iter()
        .map(|(id, (score, b, v))| Hit {
            id,
            score,
            via: match (b, v) {
                (true, true) => "bm25+vec",
                (true, false) => "bm25",
                _ => "vec",
            },
        })
        .collect();
    hits.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    hits.truncate(limit);
    hits
}

/// Candidate episode ids for an entity, via `mention` (the §8.1 collapse).
pub fn mention_candidates(conn: &Connection, node_id: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached("SELECT episode_id FROM mention WHERE node_id = ?1")?;
    let ids = stmt
        .query_map(params![node_id], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
}

/// Every episode belonging to one of `sources` — the candidate collapse for
/// a source-scoped read.
///
/// Filter-FIRST, like tags and unlike sensitivity, and the difference
/// matters: post-filtering a fused result set would ask "what are the best
/// hits overall, of which show me the Bee ones", which returns nothing when
/// the top of the ranking is calendar. Two agents reading different sources
/// need each source ranked on its own terms.
pub fn source_candidates(conn: &Connection, sources: &[String]) -> Result<Vec<i64>> {
    if sources.is_empty() {
        return Ok(vec![]);
    }
    let ph = sources.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let mut stmt = conn.prepare(&format!("SELECT id FROM episode WHERE source IN ({ph})"))?;
    let ids = stmt
        .query_map(rusqlite::params_from_iter(sources), |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
}

/// The distinct episode sources present in the graph, for validating a
/// source filter against reality instead of letting a typo return silence.
pub fn known_sources(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT DISTINCT source FROM episode ORDER BY source")?;
    let rows = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Hybrid episode search. `entity_filter`, `tag_filter` and `source_filter`
/// each collapse the candidate set first and intersect when combined (§8.1
/// filter-first); `include_private` gates the §10 sensitivity tiers (default
/// retrieval excludes private+).
#[allow(clippy::too_many_arguments)]
pub fn hybrid_episodes(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    query: &str,
    entity_filter: Option<&str>,
    tag_filter: Option<&[i64]>,
    source_filter: Option<&[i64]>,
    include_private: bool,
    k: usize,
) -> Result<Vec<Hit>> {
    let entity_cands: Option<Vec<i64>> = match entity_filter {
        Some(node_id) => Some(mention_candidates(conn, node_id)?),
        None => None,
    };
    // Intersect whichever filters are present. Folded rather than matched
    // pairwise: three filters is where the 2×2 match stopped scaling, and a
    // fourth would not need touching this again.
    let mut candidates: Option<Vec<i64>> = None;
    for present in [
        entity_cands,
        tag_filter.map(<[i64]>::to_vec),
        source_filter.map(<[i64]>::to_vec),
    ] {
        candidates = match (candidates, present) {
            (None, next) => next,
            (Some(acc), None) => Some(acc),
            (Some(acc), Some(next)) => {
                let set: std::collections::HashSet<i64> = next.into_iter().collect();
                Some(acc.into_iter().filter(|i| set.contains(i)).collect())
            }
        };
    }
    let cand_slice = candidates.as_deref();

    let pool = k * 3; // over-fetch each arm before fusion
    let bm25 = bm25_ranked(conn, "fts_episode", query, cand_slice, pool)?;
    let vec = match embedder {
        Some(e) if e.available() => {
            let qvec = e.embed_query(query)?;
            vec_ranked(conn, "vec_episode", "episode_id", &qvec, cand_slice, pool)?
        }
        _ => vec![],
    };

    let mut hits = rrf_fuse(&bm25, &vec, k * 2);

    if !include_private {
        let mut allowed: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut stmt = conn
            .prepare_cached("SELECT id FROM episode WHERE sensitivity IN ('public','personal')")?;
        for id in stmt.query_map([], |r| r.get::<_, i64>(0))? {
            allowed.insert(id?);
        }
        hits.retain(|h| allowed.contains(&h.id));
    }
    hits.truncate(k);
    Ok(hits)
}

/// Hybrid fact search over NL statements. `include_private` gates the §10
/// tiers exactly as in [`hybrid_episodes`]: derived facts inherit their
/// evidence's sensitivity (V008), so a belief extracted from a private Bee
/// transcript is filtered out of default retrieval along with the transcript.
pub fn hybrid_facts(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    query: &str,
    include_private: bool,
    k: usize,
) -> Result<Vec<Hit>> {
    let pool = k * 3;
    let bm25 = bm25_ranked(conn, "fts_fact", query, None, pool)?;
    let vec = match embedder {
        Some(e) if e.available() => {
            let qvec = e.embed_query(query)?;
            vec_ranked(conn, "vec_fact", "fact_id", &qvec, None, pool)?
        }
        _ => vec![],
    };
    let mut hits = rrf_fuse(&bm25, &vec, k * 2);

    // Liveness, unconditionally: `fts_fact`/`vec_fact` index every fact
    // ever written, so without this a retracted belief keeps being served
    // — superseded by a correction, decayed, deduped, or invalidated as
    // never-true, it made no difference. Found 2026-08-13 while
    // retracting 536 phantom co-occurrence facts and watching them come
    // straight back out of a query. Polarity is deliberately NOT filtered
    // here: a live negation is real knowledge ("X does NOT work at Y") and
    // the pack's `denial` flag is what surfaces contested state.
    {
        let mut live: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut stmt = conn.prepare_cached(
            "SELECT id FROM fact WHERE valid_to IS NULL AND invalidated_at IS NULL",
        )?;
        for id in stmt.query_map([], |r| r.get::<_, i64>(0))? {
            live.insert(id?);
        }
        hits.retain(|h| live.contains(&h.id));
    }

    if !include_private {
        let mut allowed: std::collections::HashSet<i64> = std::collections::HashSet::new();
        let mut stmt =
            conn.prepare_cached("SELECT id FROM fact WHERE sensitivity IN ('public','personal')")?;
        for id in stmt.query_map([], |r| r.get::<_, i64>(0))? {
            allowed.insert(id?);
        }
        hits.retain(|h| allowed.contains(&h.id));
    }
    hits.truncate(k);
    Ok(hits)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{add_mention, upsert_episode, Episode};
    use crate::graph::{upsert_node, Node};

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
    fn test_private_derived_fact_excluded_by_default() {
        // The V008 leak regression: a fact extracted from a private episode
        // must not surface through default fact retrieval even though the
        // episode itself is already excluded.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("bayview", "org", "Bayview Institute")).unwrap();

        let mut private_ep = ep("bee-1", "confidential chat about the offer");
        private_ep.sensitivity = "private".into();
        let (eid, _) = upsert_episode(&conn, &private_ep).unwrap();

        crate::fact::assert_fact(
            &conn,
            "nadia",
            "works_at",
            Some("bayview"),
            None,
            "Nadia works at Bayview Institute",
            Some(eid),
            None,
            0.9,
            "llm",
        )
        .unwrap();

        let default = hybrid_facts(&conn, None, "Nadia Bayview Institute", false, 10).unwrap();
        assert!(
            default.is_empty(),
            "private-derived fact must not leak into default retrieval"
        );

        let opted_in = hybrid_facts(&conn, None, "Nadia Bayview Institute", true, 10).unwrap();
        assert_eq!(opted_in.len(), 1, "include_private must still surface it");
    }

    #[test]
    fn test_retracted_beliefs_are_not_served() {
        // Regression for a live bug (2026-08-13): fts_fact/vec_fact index
        // every fact ever written, so retracted beliefs kept being served
        // — every supersede since the correction loop shipped was
        // cosmetic as far as retrieval was concerned.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("ada", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("westfield", "org", "Westfield")).unwrap();
        let keep = crate::fact::assert_fact(
            &conn,
            "ada",
            "works_at",
            Some("westfield"),
            None,
            "Ada works at Westfield Neuroscience",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        let superseded = crate::fact::assert_fact(
            &conn,
            "ada",
            "member_of",
            Some("westfield"),
            None,
            "Ada belongs to Westfield Neuroscience lab",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        let decayed = crate::fact::assert_fact(
            &conn,
            "ada",
            "related_to",
            Some("westfield"),
            None,
            "Ada and Westfield Neuroscience frequently co-occur",
            None,
            None,
            0.5,
            "npmi",
        )
        .unwrap();
        let phantom = crate::fact::assert_fact(
            &conn,
            "ada",
            "about",
            Some("westfield"),
            None,
            "Ada mentioned Westfield Neuroscience once",
            None,
            None,
            0.5,
            "npmi",
        )
        .unwrap();

        assert_eq!(
            hybrid_facts(&conn, None, "Westfield Neuroscience", true, 10)
                .unwrap()
                .len(),
            4
        );

        crate::fact::supersede_fact(&conn, &superseded, None).unwrap();
        crate::fact::close_valid_time(&conn, &decayed, None).unwrap();
        crate::fact::invalidate_never_true(&conn, &phantom).unwrap();

        let hits = hybrid_facts(&conn, None, "Westfield Neuroscience", true, 10).unwrap();
        let ids: Vec<i64> = hits.iter().map(|h| h.id).collect();
        let id_of = |uid: &str| {
            crate::fact::get_fact_by_uid(&conn, uid)
                .unwrap()
                .unwrap()
                .id
        };
        assert_eq!(
            ids,
            vec![id_of(&keep)],
            "all three retraction semantics must remove a fact from retrieval"
        );
    }

    #[test]
    fn test_live_negation_is_still_served() {
        // Liveness is filtered; polarity is not. A live negation is real
        // knowledge and the pack's `denial` flag surfaces the conflict.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("ada", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("westfield", "org", "Westfield")).unwrap();
        crate::fact::assert_negative_fact(
            &conn,
            "ada",
            "works_at",
            Some("westfield"),
            None,
            "Ada does NOT work at Westfield Neuroscience",
            None,
            0.9,
            "user",
        )
        .unwrap();
        assert_eq!(
            hybrid_facts(&conn, None, "Westfield Neuroscience", true, 10)
                .unwrap()
                .len(),
            1
        );
    }

    #[test]
    fn test_rrf_fusion_prefers_agreement() {
        // Doc 5 appears mid-rank in both arms; doc 1 tops one arm only.
        let bm25 = vec![1, 5, 2];
        let vecr = vec![9, 5, 7];
        let hits = rrf_fuse(&bm25, &vecr, 10);
        assert_eq!(hits[0].id, 5, "doc in both arms should win");
        assert_eq!(hits[0].via, "bm25+vec");
    }

    #[test]
    fn test_entity_filter_first() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("iris", "person", "Iris")).unwrap();

        // Two episodes about "pilot", only one mentions Iris.
        let (id1, _) = upsert_episode(&conn, &ep("a", "pilot data discussion with Iris")).unwrap();
        let (_id2, _) = upsert_episode(&conn, &ep("b", "pilot program for undergrads")).unwrap();
        add_mention(&conn, id1, "iris", "manual", 1.0).unwrap();

        let unfiltered = hybrid_episodes(&conn, None, "pilot", None, None, None, true, 10).unwrap();
        assert_eq!(unfiltered.len(), 2);

        let filtered =
            hybrid_episodes(&conn, None, "pilot", Some("iris"), None, None, true, 10).unwrap();
        assert_eq!(
            filtered.len(),
            1,
            "entity filter must collapse the candidate set"
        );
        assert_eq!(filtered[0].id, id1);
    }

    #[test]
    fn test_private_excluded_by_default() {
        let conn = open_memory().unwrap();
        let mut e = ep("p1", "private conversation about health");
        e.sensitivity = "private".into();
        upsert_episode(&conn, &e).unwrap();

        let default = hybrid_episodes(
            &conn,
            None,
            "health conversation",
            None,
            None,
            None,
            false,
            10,
        )
        .unwrap();
        assert!(
            default.is_empty(),
            "private must be excluded from default retrieval"
        );

        let opted_in = hybrid_episodes(
            &conn,
            None,
            "health conversation",
            None,
            None,
            None,
            true,
            10,
        )
        .unwrap();
        assert_eq!(opted_in.len(), 1);
    }

    #[test]
    fn test_vec_roundtrip() {
        // Verifies vec0 virtual tables work end-to-end with JSON-text vectors.
        let conn = open_memory().unwrap();
        let (id, _) = upsert_episode(&conn, &ep("v1", "vector test episode")).unwrap();
        let fake: Vec<f32> = (0..768).map(|i| (i as f32) / 768.0).collect();
        conn.execute(
            "INSERT INTO vec_episode (episode_id, embedding) VALUES (?1, ?2)",
            params![id, serde_json::to_string(&fake).unwrap()],
        )
        .unwrap();

        let got = vec_ranked(&conn, "vec_episode", "episode_id", &fake, None, 5).unwrap();
        assert_eq!(got, vec![id]);
    }
}
