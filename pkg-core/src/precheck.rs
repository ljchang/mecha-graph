//! Candidate precheck (§11.1 direction): shrink the review queue to genuine
//! decisions. Most extraction candidates are re-observations of facts the
//! graph already holds, or repeats within the queue itself — neither needs a
//! human. This pass:
//!
//! 1. auto-rejects duplicates of live facts (and bumps the existing fact's
//!    `observation_count` — a re-observation is evidence, not noise),
//! 2. auto-rejects duplicates within the queue (first occurrence survives),
//! 3. detects semantic duplicates via embeddings when ollama is up
//!    (candidates are compared against same-subject live facts; ≥ the dup
//!    threshold auto-resolves, the band below is flagged in the payload for
//!    a one-glance review),
//! 4. flags contradictions with live facts on single-valued predicates —
//!    these are exactly the candidates a human MUST see,
//! 5. optionally auto-accepts the clean remainder (resolvable subject, no
//!    contradiction) — the §11.1 "review only conflicts" mode.
//!
//! Commitments (kind=commitment) are never auto-handled: they materialize
//! tasks, and a wrong task on the GTD board costs attention daily.

use crate::embed::OllamaEmbedder;
use crate::error::Result;
use crate::fact::{self, FactCandidate};
use crate::graph;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::{HashMap, HashSet};

/// Same-subject cosine at or above this is a duplicate statement.
pub const SEMANTIC_DUP_THRESHOLD: f64 = 0.93;
/// Band [flag, dup): kept, but annotated with the similar existing fact.
pub const SEMANTIC_FLAG_THRESHOLD: f64 = 0.83;

/// Predicates where two different live objects are a contradiction (§11.5) —
/// keep in sync with `fact::live_contradictions`.
pub(crate) const SINGLE_VALUED: &[&str] = &[
    "works_at",
    "located_in",
    "assigned_to",
    "pursued_via",
    "has_role",
];

/// Predicates that must NEVER auto-accept regardless of ladder rung —
/// settled 2026-08-12: having emailed someone does not make them a
/// colleague; ego-relation claims about people always face a human.
/// `knows_of` is deliberately absent: the guard is about claims of social
/// standing, and "I sat in their talk" is not one (V014).
pub(crate) const NEVER_AUTO: &[&str] = &["colleague_of", "friend_of", "family_of", "mentors"];

/// Conversation-recap predicates: the episode already captures that this was
/// talked about — a fact restating it is pure bloat. Auto-rejected.
const EPHEMERAL_PREDICATES: &[&str] = &[
    "discussed",
    "discusses",
    "discussing",
    "discussed_with",
    "mentioned",
    "mentions",
    "shared",
    "said",
    "talked_about",
    "talked_to",
    "spoke_about",
];

/// Property predicates that the extractor routinely hangs EVENTS on. The
/// class history is stark — `llm·has_role` runs 2% accepted — and the
/// rejected items are TRUE sentences under the wrong predicate: "Ana
/// finished main analyses", "Tess was in a silly mood that day" — the
/// recurring failure shape this precheck exists to catch.
const PROPERTY_PREDICATES: &[&str] = &["has_role", "is", "has", "demonstrated"];

/// Past-anchored markers: a statement anchored to a past moment is an
/// event, and the episode already captures events. Deliberately narrow —
/// word-boundary matched against the lowercased statement; present-tense
/// generalisations ("Omar is the managing director") always stay for the
/// human, because a durable property is exactly what these predicates are
/// FOR when the extractor uses them right.
const EVENTIVE_MARKERS: &[&str] = &[
    " was ",
    " were ",
    "that day",
    "yesterday",
    "this morning",
    "last night",
    "last week",
];

/// True when a property-predicate statement is anchored to a past moment.
fn eventive_under_property(predicate: &str, statement: &str) -> bool {
    if !PROPERTY_PREDICATES.contains(&predicate) {
        return false;
    }
    let s = format!(" {} ", statement.to_lowercase());
    EVENTIVE_MARKERS.iter().any(|m| s.contains(m))
}

/// Durable relations safe to auto-accept, keyed by **(proposer, predicate)**
/// — the same class the ladder and the review queue are keyed by, and the
/// only key that is honest here.
///
/// It used to be predicate alone, which was safe only by accident: every
/// producer of these seven happened to be `llm`. Extending the list on
/// 2026-08-16 is what exposed the flaw. `related_to` looks like a strong
/// class at 61% — until you split it by who proposed it:
///
///   llm            61% over 23 verdicts       8 pending
///   linker:knn     10% over 88 verdicts      42 pending
///   bee:suggested   0% over  3 verdicts     397 pending
///
/// A predicate-keyed entry would have opened the gate for all three, and
/// Bee's 397 are the ones carrying known misattributed-speaker risk. The
/// vet witness would still be required per candidate, but a witness is a
/// check on the extraction, not on a proposer whose speaker attribution is
/// unreliable in the first place.
///
/// The bar for entry is a class at or above ~50% acceptance over enough
/// verdicts to mean it — comparable to `authored` (59% over 155), which
/// was here from day one. Everything else (episodic `attended`, catch-all
/// `is`/`has`) still needs a human even in auto-accept mode: noise
/// compounds, review time does not.
const DURABLE_CLASSES: &[(&str, &str)] = &[
    ("llm", "works_at"),
    ("llm", "works_on"),
    ("llm", "collaborates_with"),
    ("llm", "member_of"),
    ("llm", "located_in"),
    ("llm", "authored"),
    ("llm", "uses"),
    // Added 2026-08-16 on the owner's call, from verdict history:
    ("llm", "related_to"),       // 61% / 23 — llm ONLY, see above
    ("llm", "contains"),         // 55% / 51
    ("llm", "discussed_during"), // 51% / 59
];

#[derive(Debug, Default, Serialize)]
pub struct PrecheckReport {
    pub scanned: usize,
    pub dup_of_fact: usize,
    pub dup_in_queue: usize,
    pub semantic_dup: usize,
    pub ephemeral_rejected: usize,
    pub contradiction_flagged: usize,
    pub similar_flagged: usize,
    pub auto_accepted: usize,
    pub left_for_review: usize,
    pub subject_backfilled: usize,
    pub predicate_canonicalized: usize,
    pub eventive_rejected: usize,
    pub subject_phrased: usize,
    pub subjects_minted: usize,
    pub subject_implied: usize,
    pub rejected_dup: usize,
}

fn normalize(s: &str) -> String {
    s.to_lowercase()
        .chars()
        .map(|c| if c.is_alphanumeric() { c } else { ' ' })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn cosine(a: &[f32], b: &[f32]) -> f64 {
    let (mut dot, mut na, mut nb) = (0.0f64, 0.0f64, 0.0f64);
    for (x, y) in a.iter().zip(b) {
        dot += (*x as f64) * (*y as f64);
        na += (*x as f64) * (*x as f64);
        nb += (*y as f64) * (*y as f64);
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

struct LiveFact {
    id: i64,
    uid: String,
    subject_id: String,
    predicate: String,
    object_id: Option<String>,
    object_value: Option<String>,
    statement: String,
    norm: String,
}

fn candidate_str(c: &FactCandidate, key: &str) -> Option<String> {
    c.payload
        .get(key)
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

/// Resolve a name to a node id only when unambiguous; ambiguity means the
/// candidate needs the human path (or the entity page) — never guess here.
fn resolve_unique(conn: &Connection, name: &str) -> Result<Option<String>> {
    let mut matches = graph::resolve_entity_all(conn, name)?;
    if matches.len() == 1 {
        Ok(Some(matches.remove(0).id))
    } else {
        Ok(None)
    }
}

fn bump_observation(conn: &Connection, fact_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE fact SET observation_count = observation_count + 1 WHERE id = ?1",
        params![fact_id],
    )?;
    Ok(())
}

pub fn precheck_pending(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    auto_accept: bool,
) -> Result<PrecheckReport> {
    precheck_pending_opts(conn, embedder, auto_accept, false)
}

/// A subject named by 3+ distinct pending claims and known to nothing in
/// the graph is a real thing in the owner's life — mint it as a topic node
/// so the claims about it can resolve, dedup, and be reviewed as a class.
const MINT_RECURRENCE: usize = 3;

/// Mint topic nodes for recurring unresolvable subjects.
///
/// Deliberately here in the sweep, never in `detect_entities` — detection
/// runs on every query, and a read path that writes would mint entities
/// from search text. And deliberately gated on recurrence: a node minted
/// on first sight compounds (it becomes a detection target and starts
/// collecting mentions — the phantom shape), while three independent
/// claims about one name is the same evidence bar summarize-eligibility
/// and the kNN linker already use. Only subjects that resolve to NOTHING
/// qualify — an ambiguous name still belongs to the human path.
fn mint_recurring_subjects(conn: &Connection, dry_run: bool) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload, '$.subject') FROM fact_candidate
         WHERE status = 'proposed'
           AND COALESCE(json_extract(payload, '$.subject'), '') != ''",
    )?;
    let subjects: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    // Recurrence cannot launder a non-referent: "they" said three times is
    // still nobody.
    const MINT_STOP: &[&str] = &[
        "he",
        "she",
        "they",
        "them",
        "it",
        "we",
        "you",
        "this",
        "that",
        "these",
        "those",
        "someone",
        "somebody",
        "everyone",
        "people",
        "user",
        "the user",
        "the team",
        "the group",
    ];
    let mut counts: HashMap<String, (String, usize)> = HashMap::new();
    for s in subjects {
        let key = s.trim().to_lowercase();
        let words = key.split_whitespace().count();
        if key.len() < 3 || words > 5 || MINT_STOP.contains(&key.as_str()) {
            continue;
        }
        let e = counts.entry(key).or_insert((s.trim().to_string(), 0));
        e.1 += 1;
    }
    let mut minted = 0;
    for (_, (name, n)) in counts {
        if n < MINT_RECURRENCE {
            continue;
        }
        if !graph::resolve_entity_all(conn, &name)?.is_empty() {
            continue; // known, or ambiguous — either way, not ours to mint
        }
        if !dry_run {
            let id = format!("topic-{}", crate::ids::new_uid());
            let mut node = graph::Node::new(&id, "topic", &name);
            node.source = "precheck:mint".into();
            graph::upsert_node(conn, &node)?;
        }
        minted += 1;
    }
    Ok(minted)
}

/// `dry_run` counts every outcome without writing anything — no rejects, no
/// observation bumps, no payload annotations, no accepts.
pub fn precheck_pending_opts(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    auto_accept: bool,
    dry_run: bool,
) -> Result<PrecheckReport> {
    let mut report = PrecheckReport::default();
    let candidates = fact::pending_candidates(conn, 10_000)?;
    report.scanned = candidates.len();
    if candidates.is_empty() {
        return Ok(report);
    }
    // Mint before the loop so this pass's resolve/dedup tiers already see
    // the new nodes.
    report.subjects_minted = mint_recurring_subjects(conn, dry_run)?;

    // Live facts, keyed for the deterministic tiers.
    let mut stmt = conn.prepare(
        "SELECT id, uid, subject_id, predicate, object_id, object_value, statement
         FROM fact_current",
    )?;
    let live: Vec<LiveFact> = stmt
        .query_map([], |r| {
            let statement: String = r.get(6)?;
            Ok(LiveFact {
                id: r.get(0)?,
                uid: r.get(1)?,
                subject_id: r.get(2)?,
                predicate: r.get(3)?,
                object_id: r.get(4)?,
                object_value: r.get(5)?,
                norm: String::new(),
                statement,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    let live: Vec<LiveFact> = live
        .into_iter()
        .map(|mut f| {
            f.norm = normalize(&f.statement);
            f
        })
        .collect();

    // (subject, predicate, object) triple → fact index; norm statement → idx.
    let mut by_triple: HashMap<(String, String, String), usize> = HashMap::new();
    let mut by_norm: HashMap<(String, String), usize> = HashMap::new();
    let mut by_subject: HashMap<String, Vec<usize>> = HashMap::new();
    for (i, f) in live.iter().enumerate() {
        let obj = f
            .object_id
            .clone()
            .or_else(|| f.object_value.clone())
            .unwrap_or_default();
        by_triple.insert((f.subject_id.clone(), f.predicate.clone(), obj), i);
        by_norm.insert((f.subject_id.clone(), f.norm.clone()), i);
        by_subject.entry(f.subject_id.clone()).or_default().push(i);
    }

    // Rejection memory: normalized statements the OWNER already said no to.
    // Re-extraction (a prompt-version bump re-queues every episode) would
    // otherwise resurrect rejected claims — an identical sentence
    // re-proposed is not new evidence, it is the same extraction happening
    // again. Machine rejections (reason 'precheck: …') are deliberately
    // excluded: a dup-in-queue reject shares its norm with the surviving
    // twin, and counting it as memory would kill the survivor next sweep.
    let mut rejected_stmt = conn.prepare(
        "SELECT COALESCE(json_extract(payload, '$.statement'), '')
         FROM fact_candidate
         WHERE status = 'rejected'
           AND COALESCE(reject_reason, '') NOT LIKE 'precheck:%'",
    )?;
    let rejected_norms: std::collections::HashSet<String> = rejected_stmt
        .query_map([], |r| r.get::<_, String>(0))?
        .filter_map(|s| s.ok())
        .map(|s| normalize(&s))
        .filter(|n| !n.is_empty())
        .collect();
    drop(rejected_stmt);

    // Stored fact embeddings (statement vectors, search_document space).
    // LIVE facts only: `vec_fact` keeps a row for every fact ever embedded,
    // so an unconstrained load lets a RETRACTED belief block its own
    // re-learning — a candidate resembling a corrected, decayed or
    // invalidated fact would be auto-rejected as a dup of something we no
    // longer believe, silently and forever. `fact_current` is also
    // positive-only, which is right here: a candidate is not a duplicate
    // of its own negation (contradictions have their own tier below).
    // Matches the deterministic tiers, which already read fact_current.
    let mut fact_vecs: HashMap<i64, Vec<f32>> = HashMap::new();
    if embedder.is_some() {
        let mut stmt = conn.prepare(
            "SELECT v.fact_id, v.embedding FROM vec_fact v
             JOIN fact_current f ON f.id = v.fact_id",
        )?;
        let rows = stmt.query_map([], |r| {
            let raw = r.get_ref(1)?;
            let v = match raw {
                rusqlite::types::ValueRef::Blob(b) if b.len() % 4 == 0 => Some(
                    b.chunks_exact(4)
                        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                        .collect::<Vec<f32>>(),
                ),
                rusqlite::types::ValueRef::Text(t) => serde_json::from_slice(t).ok(),
                _ => None,
            };
            Ok((r.get::<_, i64>(0)?, v))
        })?;
        for row in rows {
            let (id, v) = row?;
            if let Some(v) = v {
                fact_vecs.insert(id, v);
            }
        }
    }

    // Embed every candidate statement in one batch (document space, matching
    // vec_fact). Failure degrades to the deterministic tiers only.
    let statements: Vec<String> = candidates
        .iter()
        .map(|c| {
            candidate_str(c, "statement")
                .or_else(|| candidate_str(c, "what"))
                .unwrap_or_default()
        })
        .collect();
    let cand_vecs: Vec<Option<Vec<f32>>> = match embedder {
        Some(e) if !fact_vecs.is_empty() || auto_accept => match e.embed(&statements, false) {
            Ok(vs) => vs.into_iter().map(Some).collect(),
            Err(_) => vec![None; candidates.len()],
        },
        _ => vec![None; candidates.len()],
    };

    // Queue-local dedup state (first occurrence wins).
    let mut seen_triple: HashSet<(String, String, String)> = HashSet::new();
    let mut seen_norm: HashSet<(String, String)> = HashSet::new();
    let mut kept_vecs: HashMap<String, Vec<(i64, Vec<f32>)>> = HashMap::new(); // subject → kept candidate vectors

    for (ci, c) in candidates.iter().enumerate() {
        let is_commitment = c.payload.get("kind").and_then(|k| k.as_str()) == Some("commitment");
        if is_commitment {
            report.left_for_review += 1;
            continue;
        }
        let statement = &statements[ci];
        let norm = normalize(statement);
        let raw_predicate = candidate_str(c, "predicate").unwrap_or_default();
        // Canonicalize before ANY predicate-keyed lane: the queue clusters
        // on the payload predicate, and raw extractor spellings mint tiny
        // classes with no history — 661 tail clusters averaging two items.
        // Read-only lookup: a queue sweep must never register extractor
        // typos as vocabulary. The original spelling rides along in
        // `predicate_was`, because a rewrite that keeps no record of what
        // it rewrote cannot be audited.
        // Every payload write in this pass compounds into ONE value —
        // rebuilding from `c.payload` per lane let a later annotation
        // silently revert an earlier lane's edit (last write wins, with
        // stale data: the same shape as the indexes-outlive-row-state bugs).
        let mut live_payload = c.payload.clone();
        let predicate = if raw_predicate.is_empty() {
            raw_predicate.clone()
        } else {
            fact::resolve_predicate(conn, &raw_predicate)?
        };
        if predicate != raw_predicate {
            if let serde_json::Value::Object(map) = &mut live_payload {
                map.insert(
                    "predicate".into(),
                    serde_json::Value::String(predicate.clone()),
                );
                map.insert(
                    "predicate_was".into(),
                    serde_json::Value::String(raw_predicate.clone()),
                );
            }
            if !dry_run {
                fact::update_candidate_payload(conn, c.id, &live_payload)?;
            }
            report.predicate_canonicalized += 1;
        }
        if !norm.is_empty() && rejected_norms.contains(&norm) {
            if !dry_run {
                fact::reject_candidate_opts(
                    conn,
                    c.id,
                    "precheck: re-proposal of a claim the review already rejected",
                    false,
                )?;
            }
            report.rejected_dup += 1;
            continue;
        }
        if EPHEMERAL_PREDICATES.contains(&predicate.as_str()) {
            if !dry_run {
                fact::reject_candidate_opts(
                    conn,
                    c.id,
                    "precheck: conversational recap — the episode already captures this",
                    false,
                )?;
            }
            report.ephemeral_rejected += 1;
            continue;
        }
        // Event-vs-property typing: a past-anchored statement under a
        // property predicate is a true sentence and a wrong fact — the
        // episode already holds the event. Same doctrine as the recap lane,
        // one level up: the predicate promised a property and the statement
        // delivered a moment.
        if eventive_under_property(&predicate, statement) {
            if !dry_run {
                fact::reject_candidate_opts(
                    conn,
                    c.id,
                    "precheck: eventive statement under a property predicate — true of a moment, and the episode already captures it",
                    false,
                )?;
            }
            report.eventive_rejected += 1;
            continue;
        }
        // Subject healing: staging resolved the subject against the graph of
        // its day, and a later merge can make a then-ambiguous name resolve.
        // When the staged subject is empty, re-detect against today's graph
        // and persist the binding — unambiguous detections only; ambiguity
        // still belongs to the human path.
        let mut subject_name = candidate_str(c, "subject").unwrap_or_default();
        if subject_name.trim().is_empty() && !statement.is_empty() {
            let (detected, _ambiguous) = crate::router::detect_entities(conn, statement)?;
            let identity = detected
                .iter()
                .find(|d| d.node_type == "person")
                .or_else(|| detected.first())
                .map(|d| d.name.clone());
            // A known entity wins; failing that, the statement's own noun
            // phrase — a topic-shaped subject, marked as such so nobody
            // mistakes it for a resolved identity. Owner-binding is the
            // LAST rung and only for Bee's fact API: those claims are about
            // the wearer by contract, and a verb-first sentence with no
            // subject noun ("Takes propranolol before presentations")
            // implies the owner. Marked subject_implied — whether the claim
            // is TRUE stays review's question, since the wearable credits
            // unknown speakers to the owner; but an unaddressable claim can
            // never even be judged.
            let (found, marker) = match identity {
                Some(name) => (Some(name), "subject_backfilled"),
                None => match crate::router::subject_phrase(statement) {
                    Some(p) => (Some(p), "subject_phrase"),
                    None if c.proposed_by.as_deref() == Some("bee:suggested") => {
                        (graph::owner_node(conn)?.map(|n| n.name), "subject_implied")
                    }
                    None => (None, ""),
                },
            };
            if let Some(name) = found {
                if let serde_json::Value::Object(map) = &mut live_payload {
                    map.insert("subject".into(), serde_json::Value::String(name.clone()));
                    map.insert(marker.into(), serde_json::Value::Bool(true));
                }
                if !dry_run {
                    fact::update_candidate_payload(conn, c.id, &live_payload)?;
                }
                match marker {
                    "subject_backfilled" => report.subject_backfilled += 1,
                    "subject_phrase" => report.subject_phrased += 1,
                    _ => report.subject_implied += 1,
                }
                subject_name = name;
            }
        }
        let subject_id = resolve_unique(conn, &subject_name)?;
        // Dedup scope: the resolved node id when we have one, else the
        // literal subject string. Identical statements are duplicates
        // whether or not we know who "The Windmill River" is — gating
        // dedup on resolution left the unresolvable majority of the queue
        // completely undeduplicated.
        let subject_key = subject_id
            .clone()
            .unwrap_or_else(|| format!("~{}", subject_name.trim().to_lowercase()));

        // Tier 1 (resolved subjects only): exact duplicate of a live fact.
        let mut triple: Option<(String, String, String)> = None;
        if let Some(sid) = &subject_id {
            let object_id = match candidate_str(c, "object") {
                Some(o) if !o.is_empty() => resolve_unique(conn, &o)?,
                _ => None,
            };
            let obj_key = object_id
                .or_else(|| candidate_str(c, "object_value"))
                .unwrap_or_default();
            let t = (sid.clone(), predicate.clone(), obj_key);
            let norm_key = (sid.clone(), norm.clone());
            let dup_idx = (!t.2.is_empty())
                .then(|| by_triple.get(&t))
                .flatten()
                .or_else(|| (!norm.is_empty()).then(|| by_norm.get(&norm_key)).flatten());
            if let Some(&i) = dup_idx {
                if !dry_run {
                    bump_observation(conn, live[i].id)?;
                    fact::reject_candidate_opts(
                        conn,
                        c.id,
                        &format!(
                            "precheck: duplicate of fact {} (observation bumped)",
                            live[i].uid
                        ),
                        false,
                    )?;
                }
                report.dup_of_fact += 1;
                continue;
            }
            triple = Some(t);
        }

        // Tier 2: duplicate within the queue — ALL candidates, resolved or not.
        let norm_key = (subject_key.clone(), norm.clone());
        if (triple
            .as_ref()
            .map(|t| !t.2.is_empty() && !seen_triple.insert(t.clone()))
            .unwrap_or(false))
            || (!norm.is_empty() && !seen_norm.insert(norm_key))
        {
            if !dry_run {
                fact::reject_candidate_opts(
                    conn,
                    c.id,
                    "precheck: duplicate within the queue",
                    false,
                )?;
            }
            report.dup_in_queue += 1;
            continue;
        }

        // Tier 3: semantic duplicate / similar. Fact comparison needs a
        // resolved subject; candidate-vs-candidate works for everyone via
        // the subject_key grouping.
        let mut flagged_similar: Option<(f64, String)> = None;
        if let Some(cv) = &cand_vecs[ci] {
            let mut best: Option<(f64, String, Option<i64>)> = None; // (sim, statement, fact_id)
            if let Some(sid) = &subject_id {
                for &i in by_subject.get(sid).map(|v| v.as_slice()).unwrap_or(&[]) {
                    if let Some(fv) = fact_vecs.get(&live[i].id) {
                        let sim = cosine(cv, fv);
                        if best.as_ref().map(|b| sim > b.0).unwrap_or(true) {
                            best = Some((sim, live[i].statement.clone(), Some(live[i].id)));
                        }
                    }
                }
            }
            for (kid, kv) in kept_vecs
                .get(&subject_key)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                let sim = cosine(cv, kv);
                if best.as_ref().map(|b| sim > b.0).unwrap_or(true) {
                    best = Some((sim, format!("candidate #{kid}"), None));
                }
            }
            match best {
                Some((sim, what, fid)) if sim >= SEMANTIC_DUP_THRESHOLD => {
                    if !dry_run {
                        if let Some(fid) = fid {
                            bump_observation(conn, fid)?;
                        }
                        fact::reject_candidate_opts(
                            conn,
                            c.id,
                            &format!("precheck: semantic duplicate of {what} (cosine {sim:.2})"),
                            false,
                        )?;
                    }
                    report.semantic_dup += 1;
                    continue;
                }
                Some((sim, what, _)) if sim >= SEMANTIC_FLAG_THRESHOLD => {
                    flagged_similar = Some((sim, what));
                }
                _ => {}
            }
        }

        // Dedup is done; contradictions and auto-accept need identity.
        let Some(subject_id) = subject_id else {
            if !dry_run {
                if let Some((sim, what)) = &flagged_similar {
                    if let serde_json::Value::Object(map) = &mut live_payload {
                        map.insert(
                            "precheck_similar_to".into(),
                            serde_json::Value::String(format!("{what} (cosine {sim:.2})")),
                        );
                    }
                    fact::update_candidate_payload(conn, c.id, &live_payload)?;
                }
            }
            if flagged_similar.is_some() {
                report.similar_flagged += 1;
            }
            if let Some(cv) = &cand_vecs[ci] {
                kept_vecs
                    .entry(subject_key)
                    .or_default()
                    .push((c.id, cv.clone()));
            }
            report.left_for_review += 1;
            continue;
        };

        // Tier 4: contradiction on single-valued predicates → must be human-reviewed.
        let mut contradiction = None;
        if SINGLE_VALUED.contains(&predicate.as_str()) {
            let object_id = match candidate_str(c, "object") {
                Some(o) if !o.is_empty() => resolve_unique(conn, &o)?,
                _ => None,
            };
            for &i in by_subject
                .get(&subject_id)
                .map(|v| v.as_slice())
                .unwrap_or(&[])
            {
                let f = &live[i];
                if f.predicate == predicate && f.object_id.is_some() && f.object_id != object_id {
                    contradiction = Some(f.statement.clone());
                    break;
                }
            }
        }

        // Annotate the payload so the TUI detail pane shows why it's held.
        if !dry_run && (flagged_similar.is_some() || contradiction.is_some()) {
            if let serde_json::Value::Object(map) = &mut live_payload {
                if let Some((sim, what)) = &flagged_similar {
                    map.insert(
                        "precheck_similar_to".into(),
                        serde_json::Value::String(format!("{what} (cosine {sim:.2})")),
                    );
                }
                if let Some(existing) = &contradiction {
                    map.insert(
                        "precheck_contradicts".into(),
                        serde_json::Value::String(existing.clone()),
                    );
                }
            }
            fact::update_candidate_payload(conn, c.id, &live_payload)?;
        }
        if contradiction.is_some() {
            report.contradiction_flagged += 1;
            report.left_for_review += 1;
            continue;
        }
        if flagged_similar.is_some() {
            report.similar_flagged += 1;
            report.left_for_review += 1;
            continue;
        }

        // Tier 5: clean and novel — auto-accept durable relations, plus
        // whatever the autonomy ladder has earned: trusted classes accept
        // outright; sampled classes accept 9-in-10 and hold a spot-check
        // (candidate id mod 10 — deterministic, so re-runs agree).
        let rung_ok = if auto_accept {
            let (key, commitment) = cluster_key(&c.payload);
            if commitment {
                false
            } else {
                match crate::ladder::get_rung(conn, c.proposed_by.as_deref().unwrap_or("?"), &key)?
                {
                    crate::ladder::Rung::Trusted => true,
                    crate::ladder::Rung::Sampled => c.id % 10 != 0,
                    crate::ladder::Rung::Staged => false,
                }
            }
        } else {
            false
        };
        // The day-one durable allowlist additionally requires a supported
        // verification verdict — the vet judge saw the claim BESIDE its
        // origin evidence and its relation label, which is the only tier
        // that catches an extractor mistype wearing a durable predicate
        // ("Ada read 'Big Red Barn'" typed as authored would auto-enter
        // otherwise; a lexical lane cannot see that). Ladder rungs are
        // exempt: Trusted/Sampled were EARNED from the owner's own verdict
        // history, and vet-gating them would demote what review promoted.
        let vetted_supported = || -> Result<bool> {
            let ok: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM agent_verdict
                 WHERE candidate_id = ?1 AND mechanism = 'verification' AND verdict = 'supported'",
                params![c.id],
                |r| r.get(0),
            )?;
            Ok(ok)
        };
        // Matched on the (proposer, predicate) class, never the predicate
        // alone — a strong predicate from a weak proposer is a weak claim.
        let durable_class = DURABLE_CLASSES
            .contains(&(c.proposed_by.as_deref().unwrap_or("?"), predicate.as_str()));
        let auto_ok = auto_accept
            && !NEVER_AUTO.contains(&predicate.as_str())
            && (rung_ok || (durable_class && vetted_supported()?));
        if auto_ok && dry_run {
            report.auto_accepted += 1; // resolvable + conflict-free would accept
        } else if auto_ok {
            // Auto-lane: no human looked at this fact — not verified/user.
            match fact::accept_candidate_opts(conn, c.id, false, false) {
                Ok(_) => {
                    report.auto_accepted += 1;
                    if let Some(cv) = &cand_vecs[ci] {
                        kept_vecs
                            .entry(subject_key.clone())
                            .or_default()
                            .push((c.id, cv.clone()));
                    }
                    continue;
                }
                Err(_) => {
                    report.left_for_review += 1; // e.g. unknown predicate — human path
                }
            }
        } else {
            report.left_for_review += 1;
        }
        if let Some(cv) = &cand_vecs[ci] {
            kept_vecs
                .entry(subject_key)
                .or_default()
                .push((c.id, cv.clone()));
        }
    }
    Ok(report)
}

// ─── Live-fact near-duplicate pass (`pkg dedupe-facts`) ─────────────────────

#[derive(Debug, Clone, Serialize)]
pub struct FactDupPair {
    pub keep_uid: String,
    pub keep_statement: String,
    pub drop_uid: String,
    pub drop_statement: String,
    pub similarity: f64,
}

/// Near-duplicate live facts on the same subject. `exact` compares
/// punctuation/case-normalized statements for equality — the only tier safe
/// to auto-apply (embeddings score "attended X on 03-31" vs "on 04-02" as
/// near-identical, but those are different events). Non-exact uses stored
/// statement embeddings (no ollama round-trip). Keep side: more
/// observations, then newer. Never applies anything — callers list, or
/// supersede the `drop_uid`s explicitly.
pub fn live_fact_dups(conn: &Connection, threshold: f64, exact: bool) -> Result<Vec<FactDupPair>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, f.uid, f.subject_id, f.statement, f.observation_count,
                f.ingested_at, v.embedding
         FROM fact_current f LEFT JOIN vec_fact v ON v.fact_id = f.id",
    )?;
    struct Row {
        uid: String,
        subject: String,
        statement: String,
        observations: i64,
        ingested: String,
        vec: Vec<f32>,
    }
    let rows: Vec<Row> = stmt
        .query_map([], |r| {
            let raw = r.get_ref(6)?;
            let vec = match raw {
                rusqlite::types::ValueRef::Blob(b) if b.len() % 4 == 0 => b
                    .chunks_exact(4)
                    .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                    .collect(),
                rusqlite::types::ValueRef::Text(t) => serde_json::from_slice(t).unwrap_or_default(),
                _ => Vec::new(), // includes NULL from the LEFT JOIN
            };
            Ok(Row {
                uid: r.get(1)?,
                subject: r.get(2)?,
                statement: r.get(3)?,
                observations: r.get(4)?,
                ingested: r.get(5)?,
                vec,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;

    let mut by_subject: HashMap<&str, Vec<&Row>> = HashMap::new();
    for r in rows.iter().filter(|r| exact || !r.vec.is_empty()) {
        by_subject.entry(r.subject.as_str()).or_default().push(r);
    }

    let norms: HashMap<&str, String> = rows
        .iter()
        .map(|r| (r.uid.as_str(), normalize(&r.statement)))
        .collect();
    let mut pairs = Vec::new();
    for group in by_subject.values() {
        for i in 0..group.len() {
            for j in (i + 1)..group.len() {
                let sim = if exact {
                    let (a, b) = (&norms[group[i].uid.as_str()], &norms[group[j].uid.as_str()]);
                    if a.is_empty() || a != b {
                        continue;
                    }
                    1.0
                } else {
                    cosine(&group[i].vec, &group[j].vec)
                };
                if sim < threshold {
                    continue;
                }
                // Keep more-observed, then newer.
                let keep_i = match group[i].observations.cmp(&group[j].observations) {
                    std::cmp::Ordering::Greater => true,
                    std::cmp::Ordering::Less => false,
                    std::cmp::Ordering::Equal => group[i].ingested >= group[j].ingested,
                };
                let (keep, drop) = if keep_i {
                    (group[i], group[j])
                } else {
                    (group[j], group[i])
                };
                pairs.push(FactDupPair {
                    keep_uid: keep.uid.clone(),
                    keep_statement: keep.statement.clone(),
                    drop_uid: drop.uid.clone(),
                    drop_statement: drop.statement.clone(),
                    similarity: sim,
                });
            }
        }
    }
    pairs.sort_by(|a, b| {
        b.similarity
            .partial_cmp(&a.similarity)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{upsert_node, Node};

    /// Stage under a chosen proposer. The durable lane is keyed on the
    /// (proposer, predicate) class, so a test exercising it has to say who
    /// proposed — `stage`'s "test" is deliberately not on the allowlist.
    fn stage_by(
        conn: &Connection,
        proposer: &str,
        subject: &str,
        predicate: &str,
        object: Option<&str>,
        statement: &str,
    ) -> i64 {
        let p = fact::ProposedFact {
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.map(Into::into),
            object_value: None,
            statement: statement.into(),
            valid_from: None,
            confidence: Some(0.9),
            tags: None,
        };
        fact::propose_fact(conn, &p, proposer, None).unwrap()
    }

    fn stage(
        conn: &Connection,
        subject: &str,
        predicate: &str,
        object: Option<&str>,
        statement: &str,
    ) -> i64 {
        let p = fact::ProposedFact {
            subject: subject.into(),
            predicate: predicate.into(),
            object: object.map(Into::into),
            object_value: None,
            statement: statement.into(),
            valid_from: None,
            confidence: Some(0.9),
            tags: None,
        };
        fact::propose_fact(conn, &p, "test", None).unwrap()
    }

    /// Stage a candidate under a named proposer, and file the vet witness
    /// the durable lane requires.
    fn stage_as(conn: &Connection, proposer: &str, predicate: &str, statement: &str) -> i64 {
        let p = fact::ProposedFact {
            subject: "Nadia".into(),
            predicate: predicate.into(),
            object: Some("Aim 2".into()),
            object_value: None,
            statement: statement.into(),
            valid_from: None,
            confidence: Some(0.9),
            tags: None,
        };
        let id = fact::propose_fact(conn, &p, proposer, None).unwrap();
        conn.execute(
            "INSERT INTO agent_verdict (candidate_id, mechanism, verdict, basis)
             VALUES (?1, 'verification', 'supported', 'test witness')",
            params![id],
        )
        .unwrap();
        id
    }

    /// The durable lane is keyed on the CLASS, not the predicate.
    ///
    /// It was predicate-only, and safe only by accident: every producer of
    /// the original seven happened to be `llm`. Extending the list exposed
    /// it — `related_to` reads as a 61% class until you split it by
    /// proposer, where `bee:suggested` is 0% over 397 pending items whose
    /// speaker attribution is known to be unreliable. A predicate-keyed
    /// entry would have auto-accepted all of them on a vet witness that
    /// checks the extraction, not the attribution.
    #[test]
    fn the_durable_lane_matches_a_class_not_a_bare_predicate() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();

        // Same predicate, same witness, different proposer.
        stage_as(&conn, "llm", "related_to", "Nadia relates to Aim 2.");
        let rep = precheck_pending(&conn, None, true).unwrap();
        assert_eq!(
            rep.auto_accepted, 1,
            "llm/related_to is on the allowlist and vetted — it may enter"
        );

        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        stage_as(
            &conn,
            "bee:suggested",
            "related_to",
            "Nadia relates to Aim 2.",
        );
        let rep = precheck_pending(&conn, None, true).unwrap();
        assert_eq!(
            rep.auto_accepted, 0,
            "the SAME predicate from an unlisted proposer must still queue"
        );
    }

    #[test]
    fn a_rejected_claim_stays_rejected_through_reextraction() {
        // A prompt-version bump re-queues every episode, and the same
        // extraction happening again is not new evidence. Without this
        // guard, re-extraction resurrects every claim the owner said no to.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        let first = stage(
            &conn,
            "Nadia",
            "related_to",
            None,
            "Nadia prefers morning meetings.",
        );
        fact::reject_candidate(&conn, first, "not worth keeping").unwrap();

        // Re-extraction proposes the same sentence again.
        let again = stage(
            &conn,
            "Nadia",
            "related_to",
            None,
            "Nadia prefers morning meetings!",
        );
        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.rejected_dup, 1);
        let status: String = conn
            .query_row(
                "SELECT status FROM fact_candidate WHERE id = ?1",
                params![again],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "rejected", "rejection memory holds");
    }

    #[test]
    fn a_verb_first_bee_claim_binds_to_the_owner_and_only_then() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-ada", "person", "Ada B Lovelace")).unwrap();

        // No owner set yet: the claim stays unaddressed — implied-subject
        // binding must never guess who the graph is about.
        let p = fact::ProposedFact {
            subject: "".into(),
            predicate: "related_to".into(),
            object: None,
            object_value: None,
            statement: "Takes propranolol before giving presentations.".into(),
            valid_from: None,
            confidence: Some(0.5),
            tags: None,
        };
        let bee = fact::propose_fact(&conn, &p, "bee:suggested", None).unwrap();
        let llm = fact::propose_fact(&conn, &p, "llm", None).unwrap();
        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.subject_implied, 0, "no owner, no binding");

        crate::graph::set_owner(&conn, "person-ada").unwrap();
        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.subject_implied, 1, "bee binds; llm does not");

        let get = |id: i64| -> serde_json::Value {
            let s: String = conn
                .query_row(
                    "SELECT payload FROM fact_candidate WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&s).unwrap()
        };
        let v = get(bee);
        assert_eq!(v["subject"], "Ada B Lovelace");
        assert_eq!(v["subject_implied"], true, "marked implied, not detected");
        let v = get(llm);
        assert_eq!(
            v["subject"], "",
            "an llm extraction's missing subject implies nobody — only Bee's \
             fact API is about the wearer by contract"
        );
    }

    #[test]
    fn a_recurring_unknown_subject_earns_a_node_and_a_stranger_does_not() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();

        // Three distinct claims about a name the graph does not know.
        stage(
            &conn,
            "Volvo V60",
            "has",
            None,
            "The Volvo V60 has a new roof rack.",
        );
        stage(
            &conn,
            "Volvo V60",
            "related_to",
            None,
            "The Volvo V60 registration renews in May.",
        );
        stage(
            &conn,
            "volvo v60",
            "related_to",
            None,
            "The Volvo V60 needs new wiper blades.",
        );
        // Two claims only: below the recurrence bar.
        stage(
            &conn,
            "Toyota Sienna",
            "related_to",
            None,
            "The Toyota Sienna has a rattle at idle.",
        );
        stage(
            &conn,
            "Toyota Sienna",
            "related_to",
            None,
            "The Toyota Sienna seats seven.",
        );
        // Recurring but KNOWN — never re-minted.
        stage(
            &conn,
            "Nadia",
            "related_to",
            None,
            "Nadia runs the pilot.",
        );
        stage(
            &conn,
            "Nadia",
            "related_to",
            None,
            "Nadia presented on Tuesday!",
        );
        stage(
            &conn,
            "Nadia",
            "related_to",
            None,
            "Nadia likes oat lattes.",
        );
        // Recurring pronoun — recurrence cannot launder a non-referent.
        stage(&conn, "they", "related_to", None, "They fixed the fence.");
        stage(
            &conn,
            "they",
            "related_to",
            None,
            "They repainted the barn.",
        );
        stage(
            &conn,
            "they",
            "related_to",
            None,
            "They cleared the driveway.",
        );

        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.subjects_minted, 1, "only the recurring unknown mints");

        let minted: (String, String) = conn
            .query_row(
                "SELECT node_type, source FROM nodes WHERE canonical_name = 'volvo v60'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(minted, ("topic".into(), "precheck:mint".into()));
        let toyota: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE canonical_name LIKE '%toyota%'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(toyota, 0);
        // Minted before the loop: this same pass already resolves the
        // subject, so the claims dedup and review as a class.
        let they: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE canonical_name = 'they'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(they, 0);

        // A second pass mints nothing new — the node now resolves.
        let r2 = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r2.subjects_minted, 0, "minting is idempotent");
    }

    #[test]
    fn payload_edits_compound_rather_than_clobber() {
        // Regression: each lane used to rebuild its write from the payload
        // as loaded at scan start, so the contradiction annotation written
        // second silently reverted the canonicalization written first —
        // last write wins, with stale data.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("bayview", "org", "Bayview Institute")).unwrap();
        upsert_node(&conn, &Node::new("nyu", "org", "NYU")).unwrap();
        fact::assert_fact(
            &conn,
            "nadia",
            "works_at",
            Some("bayview"),
            None,
            "Nadia works at Bayview Institute.",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();
        let cid = stage(
            &conn,
            "Nadia",
            "works_at",
            Some("NYU"),
            "Nadia is employed by NYU now.",
        );
        conn.execute(
            "UPDATE fact_candidate SET payload = json_set(payload, '$.predicate', 'employed_by')
             WHERE id = ?1",
            params![cid],
        )
        .unwrap();

        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.predicate_canonicalized, 1);
        assert_eq!(r.contradiction_flagged, 1);

        let payload: String = conn
            .query_row(
                "SELECT payload FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["predicate"], "works_at", "canonicalization survives");
        assert_eq!(v["predicate_was"], "employed_by");
        assert!(
            v["precheck_contradicts"]
                .as_str()
                .unwrap()
                .contains("Bayview Institute"),
            "and the annotation landed beside it"
        );
    }

    #[test]
    fn a_claim_about_nothing_known_gets_its_own_noun_phrase() {
        // 85 of 200 bee candidates named no entity the graph knows — "The
        // gutter cleaning service is scheduled…" is about the service, and
        // the subject must be its noun phrase headed for a topic node,
        // never a default to the owner.
        let conn = open_memory().unwrap();
        let cid = stage(
            &conn,
            "",
            "related_to",
            None,
            "The gutter cleaning service is scheduled for July 6.",
        );
        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.subject_phrased, 1);
        assert_eq!(r.subject_backfilled, 0);

        let payload: String = conn
            .query_row(
                "SELECT payload FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["subject"], "gutter cleaning service");
        assert_eq!(
            v["subject_phrase"], true,
            "marked as a phrase, not an identity"
        );
        // Still waits for a human: a topic subject resolves to no node
        // until accept creates one.
        let status: String = conn
            .query_row(
                "SELECT status FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "proposed");
    }

    #[test]
    fn a_moment_is_not_a_property() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("tess", "person", "Tess")).unwrap();
        // Past-anchored under a property predicate: rejected — the episode
        // already holds the event.
        let eventive = stage(
            &conn,
            "Tess",
            "is",
            None,
            "Tess was in a particularly silly mood that day.",
        );
        // Present-tense property claim: stays for the human.
        let property = stage(&conn, "Tess", "is", None, "Tess is the younger twin.");
        // The same past-anchored wording under an EVENT predicate is fine —
        // attended is supposed to hold moments.
        let event_pred = stage(
            &conn,
            "Tess",
            "attended",
            None,
            "Tess was at the library on Tuesday.",
        );
        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.eventive_rejected, 1);

        let status = |id: i64| -> String {
            conn.query_row(
                "SELECT status FROM fact_candidate WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(status(eventive), "rejected");
        assert_eq!(status(property), "proposed");
        assert_eq!(status(event_pred), "proposed");
    }

    #[test]
    fn a_spelling_variant_joins_its_canonical_class() {
        // The 661-cluster tail is mostly spelling: `working_on` and
        // "was used for" cluster apart from anything with history because
        // the queue keys on the raw payload predicate. Canonicalize at the
        // sweep, keep the original in predicate_was, register nothing.
        let conn = open_memory().unwrap();
        // propose_fact now canonicalizes at staging, so legacy rows — staged
        // before that change — are simulated by rewriting the payload back
        // to the raw spelling. The sweep lane exists exactly for them.
        let cid_alias = stage(
            &conn,
            "Nadia",
            "works_on",
            None,
            "Nadia is working on the pilot study.",
        );
        conn.execute(
            "UPDATE fact_candidate SET payload = json_set(payload, '$.predicate', 'working_on')
             WHERE id = ?1",
            params![cid_alias],
        )
        .unwrap();
        let cid_spacing = stage(
            &conn,
            "The scanner",
            "was_used_for",
            None,
            "The scanner was used for the pilot.",
        );
        conn.execute(
            "UPDATE fact_candidate SET payload = json_set(payload, '$.predicate', 'was used for')
             WHERE id = ?1",
            params![cid_spacing],
        )
        .unwrap();
        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(r.predicate_canonicalized, 2);

        let get = |id: i64| -> serde_json::Value {
            let s: String = conn
                .query_row(
                    "SELECT payload FROM fact_candidate WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            serde_json::from_str(&s).unwrap()
        };
        let v = get(cid_alias);
        assert_eq!(v["predicate"], "works_on", "alias folds to canonical");
        assert_eq!(v["predicate_was"], "working_on", "the rewrite is auditable");
        let v = get(cid_spacing);
        assert_eq!(v["predicate"], "was_used_for", "spacing folds");
        // An unknown predicate is folded but NEVER registered: a queue
        // sweep must not grow the vocabulary.
        let registered: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM predicate WHERE name = 'was_used_for'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!registered);
    }

    #[test]
    fn an_empty_subject_heals_once_identity_resolves() {
        // A staged subject is a snapshot: a candidate staged while "ada" was
        // ambiguous keeps an empty subject after the merge fixes the graph.
        // Precheck re-detects against today's graph and persists the binding.
        let conn = open_memory().unwrap();
        let mut ada = Node::new("person-ada", "person", "Ada B Lovelace");
        ada.aliases = vec!["ada".into()];
        upsert_node(&conn, &ada).unwrap();

        let cid = stage(
            &conn,
            "",
            "related_to",
            None,
            "Ada is prototyping a new eye tracker.",
        );
        let report = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(report.subject_backfilled, 1);

        let payload: String = conn
            .query_row(
                "SELECT payload FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["subject"], "Ada B Lovelace");
        assert_eq!(v["subject_backfilled"], true);
        // Healing binds; it never decides. related_to is not a durable
        // predicate, so the candidate still waits for review.
        let status: String = conn
            .query_row(
                "SELECT status FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "proposed");
    }

    #[test]
    fn test_precheck_dedups_and_flags() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("bayview", "org", "Bayview Institute")).unwrap();
        upsert_node(&conn, &Node::new("nyu", "org", "NYU")).unwrap();

        // Live fact the graph already holds.
        fact::assert_fact(
            &conn,
            "nadia",
            "works_at",
            Some("bayview"),
            None,
            "Nadia works at Bayview Institute.",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();

        // 1: exact dup of the live fact (same triple).
        stage(
            &conn,
            "Nadia",
            "works_at",
            Some("Bayview Institute"),
            "Nadia is working at Bayview Institute",
        );
        // 2+3: same statement twice in the queue (first survives, second drops).
        stage(
            &conn,
            "Nadia",
            "related_to",
            None,
            "Nadia runs the pilot study.",
        );
        stage(
            &conn,
            "Nadia",
            "related_to",
            None,
            "Nadia runs the pilot study!",
        );
        // 4: contradiction on a single-valued predicate.
        stage(
            &conn,
            "Nadia",
            "works_at",
            Some("NYU"),
            "Nadia works at NYU now.",
        );

        let report = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(report.scanned, 4);
        assert_eq!(report.dup_of_fact, 1);
        assert_eq!(report.dup_in_queue, 1);
        assert_eq!(report.contradiction_flagged, 1);
        assert_eq!(report.left_for_review, 2); // the survivor + the contradiction

        // Re-observation strengthened the existing fact.
        let obs: i64 = conn.query_row(
            "SELECT observation_count FROM fact WHERE subject_id='nadia' AND predicate='works_at' AND valid_to IS NULL",
            [], |r| r.get(0)).unwrap();
        assert_eq!(obs, 2);

        // The contradiction carries its context for the reviewer.
        let pending = fact::pending_candidates(&conn, 10).unwrap();
        let contra = pending
            .iter()
            .find(|c| c.payload.get("precheck_contradicts").is_some());
        assert!(contra.is_some(), "contradiction must be flagged in payload");
    }

    #[test]
    fn test_retracted_fact_does_not_block_relearning() {
        // Regression (2026-08-13): the deterministic dup tier reads
        // fact_current, but the semantic tier loaded every vec_fact row —
        // so a corrected/decayed/invalidated belief silently auto-rejected
        // any candidate resembling it, forever. A retraction must not
        // become a permanent veto on learning the thing again.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("bayview", "org", "Bayview Institute")).unwrap();
        let uid = fact::assert_fact(
            &conn,
            "nadia",
            "works_at",
            Some("bayview"),
            None,
            "Nadia works at Bayview Institute.",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();
        fact::supersede_fact(&conn, &uid, None).unwrap();

        // The same claim arrives again — it must reach review, not die.
        stage(
            &conn,
            "Nadia",
            "works_at",
            Some("Bayview Institute"),
            "Nadia works at Bayview Institute.",
        );
        let report = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(
            report.dup_of_fact, 0,
            "a retracted belief is not a live duplicate"
        );
        assert_eq!(report.left_for_review, 1);
    }

    #[test]
    fn test_precheck_dedups_unresolvable_subjects() {
        let conn = open_memory().unwrap();
        // No node for "The Windmill River" — subjects don't resolve, and
        // dedup must work anyway (this was the gap: unresolved candidates
        // skipped every dedup tier).
        stage(
            &conn,
            "The Windmill River",
            "related_to",
            None,
            "The Windmill River is a quiet river.",
        );
        stage(
            &conn,
            "The Windmill River",
            "related_to",
            None,
            "The Windmill River is a quiet river!",
        );
        stage(
            &conn,
            "The Windmill River",
            "related_to",
            None,
            "The Windmill River has many sandbars.",
        );

        let r = precheck_pending(&conn, None, false).unwrap();
        assert_eq!(
            r.dup_in_queue, 1,
            "normalized-identical statements dedupe without resolution"
        );
        assert_eq!(r.left_for_review, 2);
    }

    #[test]
    fn test_precheck_ephemeral_and_durable_gates() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();

        // Recap predicate: auto-rejected outright.
        stage(
            &conn,
            "Nadia",
            "discussed",
            None,
            "Nadia discussed the pilot results.",
        );
        // Durable predicate WITH a supported verification verdict:
        // auto-accepted.
        let vetted = stage_by(
            &conn,
            "llm", // the durable lane is keyed on (proposer, predicate)
            "Nadia",
            "works_on",
            Some("Aim 2"),
            "Nadia works on Aim 2.",
        );
        fact::record_verdict(
            &conn,
            vetted,
            "verification",
            "supported",
            "the episode says so",
            None,
        )
        .unwrap();
        // Durable but UNVETTED: stays. The day-one allowlist alone no
        // longer admits — vet must have seen the claim beside its evidence,
        // because a lexical lane cannot catch an extractor mistype wearing
        // a durable predicate ("read a book" filed as authored).
        stage_by(
            &conn,
            "llm",
            "Nadia",
            "works_on",
            None,
            "Nadia leads the Aim 2 outreach effort.",
        );
        // Valid but non-durable predicate: stays for review even in auto mode.
        stage(
            &conn,
            "Nadia",
            "related_to",
            Some("Aim 2"),
            "Nadia is related to Aim 2.",
        );

        let r = precheck_pending(&conn, None, true).unwrap();
        assert_eq!(r.ephemeral_rejected, 1);
        assert_eq!(r.auto_accepted, 1);
        assert_eq!(r.left_for_review, 2);
    }

    #[test]
    fn test_review_clusters_groups_history_and_samples() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("iris", "person", "Iris")).unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        upsert_node(&conn, &Node::new("aim3", "project", "Aim 3")).unwrap();
        // Four pending works_on + one pending has_role, then verdict history
        // on works_on: 1 accepted, 1 rejected.
        for i in 0..4 {
            stage(
                &conn,
                "Nadia",
                "works_on",
                Some("Aim 2"),
                &format!("Nadia works on Aim 2 (v{i})"),
            );
        }
        stage(&conn, "Nadia", "has_role", None, "Nadia is a postdoc");
        let a = stage(
            &conn,
            "Iris",
            "works_on",
            Some("Aim 2"),
            "Iris works on Aim 2",
        );
        let r = stage(
            &conn,
            "Iris",
            "works_on",
            Some("Aim 3"),
            "Iris works on Aim 3",
        );
        fact::accept_candidate(&conn, a).unwrap();
        fact::reject_candidate(&conn, r, "test").unwrap();

        let clusters = review_clusters(&conn, 2).unwrap();
        assert_eq!(clusters.len(), 2, "two (proposer, predicate) clusters");
        let top = &clusters[0];
        assert_eq!(top.predicate, "works_on");
        assert_eq!(top.pending, 4, "largest cluster first");
        assert_eq!(
            (top.accepted_hist, top.rejected_hist),
            (1, 1),
            "verdict history joins on the same key"
        );
        assert_eq!(top.samples.len(), 2);
        assert!(!top.commitment);
        assert_eq!(clusters[1].predicate, "has_role");
    }

    #[test]
    fn test_precheck_auto_accept_leaves_conflicts() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();

        let vetted = stage_by(
            &conn,
            "llm", // the durable lane is keyed on (proposer, predicate)
            "Nadia",
            "works_on",
            Some("Aim 2"),
            "Nadia works on Aim 2.",
        );
        fact::record_verdict(
            &conn,
            vetted,
            "verification",
            "supported",
            "the episode says so",
            None,
        )
        .unwrap();
        // Unknown subject: must stay for the human A-accept path.
        stage(
            &conn,
            "Dr Unknown",
            "related_to",
            None,
            "Dr Unknown said something.",
        );

        let report = precheck_pending(&conn, None, true).unwrap();
        assert_eq!(report.auto_accepted, 1);
        assert_eq!(report.left_for_review, 1);
        let pending = fact::pending_candidates(&conn, 10).unwrap();
        assert_eq!(pending.len(), 1);
    }

    #[test]
    fn test_ladder_rung_extends_auto_accept() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();

        // 'about' is NOT in DURABLE_PREDICATES — staged, it queues.
        stage(
            &conn,
            "Nadia",
            "about",
            Some("Aim 2"),
            "Nadia note about Aim 2.",
        );
        let report = precheck_pending(&conn, None, true).unwrap();
        assert_eq!(report.auto_accepted, 0, "staged class must queue");

        // Promote the (test, about) class to trusted, then a fresh
        // candidate in the same class auto-accepts.
        conn.execute(
            "INSERT INTO class_ledger (proposer, predicate, rung) VALUES ('test','about','trusted')",
            [],
        )
        .unwrap();
        stage(
            &conn,
            "Nadia",
            "about",
            Some("Aim 2"),
            "Nadia talked about Aim 2 plans.",
        );
        let report = precheck_pending(&conn, None, true).unwrap();
        assert_eq!(
            report.auto_accepted, 1,
            "trusted class auto-accepts beyond the allowlist"
        );

        // Machine rejects must not have created/moved ladder streaks.
        let streak: i64 = conn
            .query_row(
                "SELECT streak FROM class_ledger WHERE proposer='test' AND predicate='about'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(streak, 0, "auto-lane must not move the ladder it feeds");
    }
}

// ─── Cluster review (PLAN.md Wave 1d) ────────────────────────────────────────

/// One review cluster: pending candidates sharing (proposer, predicate),
/// with historical verdicts on the same key as a precision prior. The
/// feature-feedback result (Raghavan et al., JMLR 2006): one verdict on a
/// class is worth hundreds on instances.
#[derive(Debug, Serialize)]
pub struct ReviewCluster {
    pub proposed_by: String,
    /// Predicate, or "(kind)" for kind-shaped candidates (e.g. commitments).
    pub predicate: String,
    pub pending: usize,
    pub conf_min: f64,
    pub conf_max: f64,
    /// Historical verdicts on this same (proposer, predicate) key —
    /// the free precision prior mined from past review decisions.
    pub accepted_hist: i64,
    pub rejected_hist: i64,
    /// Commitments materialize tasks — never bulk-verdict them.
    pub commitment: bool,
    /// Autonomy-ladder rung (V012): staged | sampled | trusted.
    pub rung: String,
    /// Consecutive human accepts toward the next promotion.
    pub streak: i64,
    /// Spread sample of statements (stride over the cluster, not top-N,
    /// so the sample is typical rather than the highest-confidence edge).
    pub samples: Vec<String>,
}

/// The (predicate, is_commitment) grouping key for a candidate payload.
/// Public because every surface that verdicts a cluster (CLI bulk flags,
/// TUI cluster view) must select members by exactly this rule.
pub fn cluster_key(payload: &serde_json::Value) -> (String, bool) {
    if let Some(p) = payload.get("predicate").and_then(|v| v.as_str()) {
        return (p.to_string(), false);
    }
    let kind = payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("none");
    (format!("({kind})"), kind == "commitment")
}

fn candidate_text(payload: &serde_json::Value) -> String {
    for k in ["statement", "what"] {
        if let Some(s) = payload.get(k).and_then(|v| v.as_str()) {
            return s.to_string();
        }
    }
    "-".into()
}

/// Group the pending queue by (proposer, predicate) with per-key verdict
/// history and spread samples. Sorted by pending count, largest first —
/// the order in which one interaction resolves the most items.
pub fn review_clusters(conn: &Connection, sample_n: usize) -> Result<Vec<ReviewCluster>> {
    // Verdict history per key (the whole table, not just pending).
    let mut hist: HashMap<(String, String), (i64, i64)> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT COALESCE(proposed_by,'?'),
                    COALESCE(json_extract(payload,'$.predicate'),
                             '(' || COALESCE(json_extract(payload,'$.kind'),'none') || ')'),
                    SUM(status='accepted'), SUM(status='rejected')
             FROM fact_candidate WHERE status IN ('accepted','rejected')
             GROUP BY 1, 2",
        )?;
        for row in stmt.query_map([], |r| {
            Ok((
                (r.get::<_, String>(0)?, r.get::<_, String>(1)?),
                (r.get(2)?, r.get(3)?),
            ))
        })? {
            let (k, v) = row?;
            hist.insert(k, v);
        }
    }

    let pending = fact::pending_candidates(conn, 100_000)?;
    let mut groups: std::collections::BTreeMap<(String, String, bool), Vec<&FactCandidate>> =
        std::collections::BTreeMap::new();
    for c in &pending {
        let (pred, commitment) = cluster_key(&c.payload);
        let pb = c.proposed_by.clone().unwrap_or_else(|| "?".into());
        groups.entry((pb, pred, commitment)).or_default().push(c);
    }

    // Ladder state per class (absent = staged, streak 0).
    let rungs: HashMap<(String, String), (String, i64)> = crate::ladder::ladder_rows(conn)?
        .into_iter()
        .map(|(p, pr, r, s)| ((p, pr), (r.as_str().to_string(), s)))
        .collect();

    let mut clusters = Vec::new();
    for ((pb, pred, commitment), members) in groups {
        let mut confs: Vec<f64> = members.iter().filter_map(|c| c.confidence).collect();
        if confs.is_empty() {
            confs.push(0.0); // keep min/max finite (JSON has no Infinity)
        }
        // Stride sample: typical members, not the top-confidence edge.
        let stride = (members.len() / sample_n.max(1)).max(1);
        let samples: Vec<String> = members
            .iter()
            .step_by(stride)
            .take(sample_n)
            .map(|c| candidate_text(&c.payload))
            .collect();
        let (a, r) = hist
            .get(&(pb.clone(), pred.clone()))
            .copied()
            .unwrap_or((0, 0));
        let (rung, streak) = rungs
            .get(&(pb.clone(), pred.clone()))
            .cloned()
            .unwrap_or_else(|| ("staged".into(), 0));
        clusters.push(ReviewCluster {
            proposed_by: pb,
            predicate: pred,
            pending: members.len(),
            conf_min: confs.iter().cloned().fold(f64::INFINITY, f64::min),
            conf_max: confs.iter().cloned().fold(f64::NEG_INFINITY, f64::max),
            accepted_hist: a,
            rejected_hist: r,
            commitment,
            rung,
            streak,
            samples,
        });
    }
    clusters.sort_by(|a, b| b.pending.cmp(&a.pending));
    Ok(clusters)
}
