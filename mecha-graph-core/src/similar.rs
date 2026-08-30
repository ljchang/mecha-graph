//! Semantic grouping over the review queue: find the near-repeats of a
//! pending candidate, so one human verdict can honestly cover its whole
//! group.
//!
//! The queue's bulk is repetition — Bee proposes "Luke plays with his
//! children" a thousand slightly different ways — and precheck's dedup only
//! removes the near-identical (≥ [`crate::precheck::SEMANTIC_DUP_THRESHOLD`]).
//! Everything between "similar" and "duplicate" queued for one-at-a-time
//! review, which is how 7,000 items became unmanageable.
//!
//! Three rules carry the design:
//!
//! - **A cascade never crosses a class uninvited.** [`similar_to`] draws
//!   only from the seed's own (proposer, cluster-key) class, structurally —
//!   verdicts are class decisions, and a keystroke that silently reached
//!   across classes would be the predicate-keyed durable-list mistake
//!   again, one hop over. The one invited crossing is the global layer
//!   ([`groups_across_classes`]): the owner asks for it by flag, the floor
//!   is stricter ([`GLOBAL_GROUP_THRESHOLD`] — out there the class no
//!   longer vouches for kinship), and every group names the classes it
//!   spans, because the blast radius is part of the reviewable object.
//! - **One keystroke is one human verdict.** The seed is accepted or
//!   rejected as the owner's; every member goes through the cascade paths
//!   in `fact.rs`, which label `reviewed_by = "cascade:<seed>"` so the
//!   ladder's human record never sees them. A cascade that counted as N
//!   human verdicts would promote classes on their own volume.
//! - **The threshold is precheck's flag threshold.** Cosine scales are a
//!   property of the embedding model; one shared constant means a model
//!   swap recalibrates every consumer together (`dedupe-facts` learned
//!   this as a drifted literal).
//!
//! Grouping is deterministic: candidates in id order, greedy leader
//! clustering — the same inputs always produce the same groups, so what a
//! person saw in a listing is what a later cascade acts on.

use crate::embed::{EmbedTask, Embedder};
use crate::error::{Error, Result};
use crate::ladder::KEY_SQL;
use crate::precheck::cosine;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Grouping threshold: precheck's "similar" flag line, shared on purpose.
pub const GROUP_THRESHOLD: f64 = crate::precheck::SEMANTIC_FLAG_THRESHOLD;

/// The cross-class grouping floor — the global layer's default, above the
/// within-class line and below precheck's duplicate line, both on purpose.
/// Within a class, sharing (proposer, predicate) already vouches for
/// kinship, so [`GROUP_THRESHOLD`] can sit at "similar"; across classes the
/// statement text carries the whole argument, so the floor rises. It cannot
/// usefully rise to [`crate::precheck::SEMANTIC_DUP_THRESHOLD`], because
/// everything at that line was already removed by precheck's dedup — a
/// global layer defaulted there would group almost nothing and read as
/// broken.
///
/// **Measured 2026-08-29** (`pkg calibrate-groups`, 2,423 human verdicts):
/// this floor is a judgement call no longer — and the measurement indicts
/// the layer, not the value. Cross-class pairs at ≥0.90 carried the SAME
/// human verdict only **63%** of the time (59–67% across every floor from
/// 0.80 to 0.96; the ≥0.97 band is precheck's dedup territory). A
/// cross-class cascade at any usable floor overwrites the owner's own
/// counterfactual verdict on roughly one pair in three. Same-class pairs
/// ran ~89–90% flat across the whole range — the class carries the
/// kinship signal, the cosine barely adds to it. So: the global layer
/// remains a *listing* (seeing the blast radius is harmless), the
/// `--across-classes` cascade warns with these numbers at use, and the
/// bulk-verdict surface (TUI groups) is deliberately within-class only.
pub const GLOBAL_GROUP_THRESHOLD: f64 = 0.90;

/// The same-class cascade agreement measured 2026-08-29 at
/// [`GROUP_THRESHOLD`] — one keystroke on a group of ten mis-verdicts
/// about one member, which the cascade design absorbs (machine-labeled,
/// revisable, invisible to the ladder). Kept as a named number so
/// surfaces can print what a group verdict costs instead of implying it
/// is free.
pub const MEASURED_SAME_CLASS_AGREEMENT: f64 = 0.89;

/// What the same measurement said about crossing classes. See
/// [`GLOBAL_GROUP_THRESHOLD`].
pub const MEASURED_CROSS_CLASS_AGREEMENT: f64 = 0.63;

#[derive(Debug, Clone, Serialize)]
pub struct SimilarGroup {
    pub leader_id: i64,
    pub leader_statement: String,
    /// Members beyond the leader: (candidate id, cosine to the leader).
    pub members: Vec<(i64, f64)>,
    /// A few member statements, for a listing to show beside the leader.
    pub sample: Vec<String>,
}

impl SimilarGroup {
    pub fn size(&self) -> usize {
        self.members.len() + 1
    }
}

/// Pending candidates of one class, in id order (determinism), with the
/// text a vector will be computed from.
fn class_pending(conn: &Connection, proposer: &str, key: &str) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT id, COALESCE(json_extract(payload,'$.statement'), payload)
         FROM fact_candidate
         WHERE status = 'proposed'
           AND COALESCE(proposed_by,'?') = ?1 AND {KEY_SQL} = ?2
         ORDER BY id"
    ))?;
    let rows = stmt
        .query_map(params![proposer, key], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// The seed's class, read from its own row. Errors if the seed is not
/// pending — a cascade from a decided row would re-decide history.
fn seed_class(conn: &Connection, seed_id: i64) -> Result<(String, String, bool)> {
    let row: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT proposed_by, payload FROM fact_candidate
             WHERE id = ?1 AND status = 'proposed'",
            params![seed_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((proposer, payload)) = row else {
        return Err(Error::Other(format!("no pending candidate {seed_id}")));
    };
    let payload: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
    let (key, commitment) = crate::precheck::cluster_key(&payload);
    Ok((proposer.unwrap_or_else(|| "?".into()), key, commitment))
}

/// Deterministic greedy leader clustering. Pure: index pairs in, so it can
/// be tested without an embedding server. Each vector joins the FIRST
/// earlier leader within `threshold`, else becomes a leader itself.
pub fn cluster(vecs: &[Vec<f32>], threshold: f64) -> Vec<(usize, Vec<(usize, f64)>)> {
    let mut groups: Vec<(usize, Vec<(usize, f64)>)> = Vec::new();
    for (i, v) in vecs.iter().enumerate() {
        let mut placed = false;
        for (leader, members) in groups.iter_mut() {
            let sim = cosine(&vecs[*leader], v);
            if sim >= threshold {
                members.push((i, sim));
                placed = true;
                break;
            }
        }
        if !placed {
            groups.push((i, Vec::new()));
        }
    }
    groups
}

/// Group one class's pending candidates by semantic similarity, largest
/// groups first — and the singletons after them, so the listing covers the
/// WHOLE class. Singletons used to be dropped ("a group of one is just the
/// queue"), which made the view show 31 of a class's 159 and stranded the
/// rest in another surface; a triage listing that hides most of the work is
/// a listing people leave. A group of one is simply a row whose verdict
/// cascades to nobody.
pub fn groups_for_class(
    conn: &Connection,
    embedder: &Embedder,
    proposer: &str,
    key: &str,
    threshold: f64,
) -> Result<Vec<SimilarGroup>> {
    let pending = class_pending(conn, proposer, key)?;
    if pending.is_empty() {
        return Ok(Vec::new());
    }
    let texts: Vec<String> = pending.iter().map(|(_, s)| s.clone()).collect();
    let vecs = embedder.embed(&texts, EmbedTask::Document)?;
    let mut out: Vec<SimilarGroup> = cluster(&vecs, threshold)
        .into_iter()
        .map(|(leader, members)| SimilarGroup {
            leader_id: pending[leader].0,
            leader_statement: pending[leader].1.clone(),
            sample: members
                .iter()
                .take(3)
                .map(|(i, _)| pending[*i].1.clone())
                .collect(),
            members: members
                .into_iter()
                .map(|(i, sim)| (pending[i].0, sim))
                .collect(),
        })
        .collect();
    out.sort_by(|a, b| b.size().cmp(&a.size()).then(a.leader_id.cmp(&b.leader_id)));
    Ok(out)
}

/// One cross-class group: near-repeats drawn from the whole pending queue.
#[derive(Debug, Clone, Serialize)]
pub struct GlobalGroup {
    pub leader_id: i64,
    pub leader_statement: String,
    /// The leader's "proposer . key" class.
    pub leader_class: String,
    /// Members beyond the leader: (candidate id, cosine to the leader).
    pub members: Vec<(i64, f64)>,
    /// Class → how many of this group's candidates (leader included) sit in
    /// it. The blast radius: a listing that hid this would be asking for a
    /// verdict about classes the reviewer never saw named.
    pub classes: std::collections::BTreeMap<String, usize>,
    /// A few member statements, for the listing.
    pub sample: Vec<String>,
}

impl GlobalGroup {
    pub fn size(&self) -> usize {
        self.members.len() + 1
    }
}

/// Every pending non-commitment candidate, id order, with its statement and
/// class label. Commitments are skipped at the source — they materialize
/// tasks one at a time and never cascade, so a layer built for fan-out has
/// no row for them.
fn all_pending(conn: &Connection, proposer: Option<&str>) -> Result<Vec<(i64, String, String)>> {
    let mut stmt = conn.prepare(
        "SELECT id, COALESCE(json_extract(payload,'$.statement'), payload),
                COALESCE(proposed_by,'?'), payload
         FROM fact_candidate
         WHERE status = 'proposed'
         ORDER BY id",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut out = Vec::new();
    for (id, statement, prop, payload) in rows {
        if proposer.is_some_and(|p| p != prop) {
            continue;
        }
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
        let (key, commitment) = crate::precheck::cluster_key(&payload);
        if commitment {
            continue;
        }
        out.push((id, statement, format!("{prop} . {key}")));
    }
    Ok(out)
}

/// Pure assembly of the global view, split from the query and the embedder
/// so the crossing behaviour is a unit test rather than a server run.
/// Singletons are dropped — this layer exists for fan-out, and the class
/// listings already cover every candidate one at a time — and the drop is
/// the caller's to report, because a view that silently shows less than the
/// queue reads as having covered it.
fn assemble_global_groups(
    rows: &[(i64, String, String)],
    vecs: &[Vec<f32>],
    threshold: f64,
) -> Vec<GlobalGroup> {
    let mut out: Vec<GlobalGroup> = cluster(vecs, threshold)
        .into_iter()
        .filter(|(_, members)| !members.is_empty())
        .map(|(leader, members)| {
            let mut classes = std::collections::BTreeMap::new();
            *classes.entry(rows[leader].2.clone()).or_insert(0) += 1;
            for (i, _) in &members {
                *classes.entry(rows[*i].2.clone()).or_insert(0) += 1;
            }
            GlobalGroup {
                leader_id: rows[leader].0,
                leader_statement: rows[leader].1.clone(),
                leader_class: rows[leader].2.clone(),
                sample: members
                    .iter()
                    .take(3)
                    .map(|(i, _)| rows[*i].1.clone())
                    .collect(),
                members: members
                    .into_iter()
                    .map(|(i, sim)| (rows[i].0, sim))
                    .collect(),
                classes,
            }
        })
        .collect();
    out.sort_by(|a, b| b.size().cmp(&a.size()).then(a.leader_id.cmp(&b.leader_id)));
    out
}

/// The top layer over the whole queue: pending candidates from every class
/// (optionally one proposer's), grouped by semantic similarity at the
/// stricter global floor, largest first. Deterministic like the class view:
/// id order in, greedy leader clustering, so what a person saw is what a
/// later cascade acts on. Returns the groups and how many pending
/// candidates were considered, so a caller can say "N groups covering M of
/// K pending" honestly.
pub fn groups_across_classes(
    conn: &Connection,
    embedder: &Embedder,
    threshold: f64,
    proposer: Option<&str>,
) -> Result<(Vec<GlobalGroup>, usize)> {
    let rows = all_pending(conn, proposer)?;
    if rows.is_empty() {
        return Ok((Vec::new(), 0));
    }
    let texts: Vec<String> = rows.iter().map(|(_, s, _)| s.clone()).collect();
    let vecs = embedder.embed(&texts, EmbedTask::Document)?;
    Ok((assemble_global_groups(&rows, &vecs, threshold), rows.len()))
}

/// Vet an explicit cascade set for a verdict that was ALLOWED to cross
/// classes — the global listing's counterpart of [`vet_cascade_ids`].
/// Same contract otherwise: ids the caller read off a listing, pending ones
/// kept, decided-since dropped silently, unknown ids an error. What stays
/// strict: the seed must be pending, and no commitment is ever swept — the
/// global listing never shows one, so a commitment id here means the list
/// did not come from this queue.
pub fn vet_cascade_ids_across(conn: &Connection, seed_id: i64, ids: &[i64]) -> Result<Vec<i64>> {
    // Errors if the seed is not pending; commitments refuse below.
    let (_, _, seed_commitment) = seed_class(conn, seed_id)?;
    if seed_commitment {
        return Err(Error::Other(
            "commitments do not cascade — each one materializes its own task".into(),
        ));
    }
    let mut out = Vec::new();
    for &id in ids {
        if id == seed_id {
            continue;
        }
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT status, payload FROM fact_candidate WHERE id = ?1",
                params![id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        match row {
            Some((status, payload)) if status == "proposed" => {
                let payload: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
                let (_, commitment) = crate::precheck::cluster_key(&payload);
                if commitment {
                    return Err(Error::Other(format!(
                        "candidate {id} is a commitment — the global listing never shows one,                          so this cascade list is not from this queue"
                    )));
                }
                out.push(id);
            }
            Some(_) => {} // decided since the listing — nothing to re-decide
            None => {
                return Err(Error::Other(format!(
                    "candidate {id} does not exist — the cascade list is not from this queue"
                )))
            }
        }
    }
    Ok(out)
}

/// The pending candidates a verdict on `seed_id` may cascade to: same
/// class, cosine to the seed at or above `threshold`. The seed itself is
/// not in the result. Commitments refuse — they materialize tasks one at a
/// time, and `note_verdict` will not ride them either.
pub fn similar_to(
    conn: &Connection,
    embedder: &Embedder,
    seed_id: i64,
    threshold: f64,
) -> Result<Vec<(i64, f64)>> {
    let (proposer, key, commitment) = seed_class(conn, seed_id)?;
    if commitment {
        return Err(Error::Other(
            "commitments do not cascade — each one materializes its own task".into(),
        ));
    }
    let pending = class_pending(conn, &proposer, &key)?;
    let seed_idx = pending
        .iter()
        .position(|(id, _)| *id == seed_id)
        .ok_or_else(|| Error::Other(format!("candidate {seed_id} not in its own class listing")))?;
    let texts: Vec<String> = pending.iter().map(|(_, s)| s.clone()).collect();
    let vecs = embedder.embed(&texts, EmbedTask::Document)?;
    let seed_vec = &vecs[seed_idx];
    let mut out: Vec<(i64, f64)> = pending
        .iter()
        .zip(vecs.iter())
        .filter(|((id, _), _)| *id != seed_id)
        .filter_map(|((id, _), v)| {
            let sim = cosine(seed_vec, v);
            (sim >= threshold).then_some((*id, sim))
        })
        .collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    Ok(out)
}

/// Vet an EXPLICIT cascade set — member ids a caller read off a groups
/// listing — against the seed's class. Returns the ids still pending;
/// already-decided ones are dropped silently (the nightly may have raced
/// this sitting, and re-deciding history is not this verb's job).
///
/// A cross-class id is an error, never a skip: the no-crossing rule is
/// structural, and a caller naming one is a caller with a bug — silently
/// dropping it would hide exactly the mistake the rule exists to catch.
/// The reason explicit sets exist at all: a listing someone READ is what
/// their verdict is about, so the cascade must land on those ids — not on
/// whatever a re-embedding of a queue that moved since would derive. It is
/// also what lets a group verdict skip the embedder entirely.
pub fn vet_cascade_ids(conn: &Connection, seed_id: i64, ids: &[i64]) -> Result<Vec<i64>> {
    let (proposer, key, commitment) = seed_class(conn, seed_id)?;
    if commitment {
        return Err(Error::Other(
            "commitments do not cascade — each one materializes its own task".into(),
        ));
    }
    let pending: std::collections::HashSet<i64> = class_pending(conn, &proposer, &key)?
        .into_iter()
        .map(|(id, _)| id)
        .collect();
    let mut out = Vec::new();
    for &id in ids {
        if id == seed_id {
            continue;
        }
        if pending.contains(&id) {
            out.push(id);
            continue;
        }
        // Not pending in the seed's class: decided since, or another class.
        let other: Option<String> = conn
            .query_row(
                "SELECT status FROM fact_candidate WHERE id = ?1",
                params![id],
                |r| r.get(0),
            )
            .optional()?;
        match other.as_deref() {
            Some("proposed") => {
                return Err(Error::Other(format!(
                    "candidate {id} is pending in a different class than seed {seed_id} — \
                     a cascade never crosses a class"
                )))
            }
            Some(_) => {} // decided since the listing — nothing to re-decide
            None => {
                return Err(Error::Other(format!(
                    "candidate {id} does not exist — the cascade list is not from this queue"
                )))
            }
        }
    }
    Ok(out)
}

// ─── Calibrating the global floor against the human record ──────────────
// (review-on-use §4: "midway is a judgement call, not a measurement" —
// this is the measurement.)

/// One row of the calibration curve: at `threshold`, how many decided
/// pairs sat at-or-above it, and in how many of those the owner's two
/// verdicts agreed. `agreement` is `None` over zero pairs — no evidence
/// is not the same as perfect agreement.
#[derive(Debug, Clone, Serialize)]
pub struct CalibrationPoint {
    pub threshold: f64,
    pub pairs: usize,
    pub agree: usize,
    pub agreement: Option<f64>,
}

#[derive(Debug, Serialize)]
pub struct CalibrationReport {
    /// Which embedding space produced the vectors ("document" | "dedup").
    pub task: String,
    /// Decided human verdicts the curve rests on (after the cap).
    pub verdicts: usize,
    pub points: Vec<CalibrationPoint>,
    /// The same curve over pairs that share a (proposer, predicate) class
    /// — the within-class cascade's floor is judged by THESE, because in
    /// that lane the class vouches for kinship beside the cosine.
    pub same_class_points: Vec<CalibrationPoint>,
    /// And over pairs that cross classes — what the global layer acts on.
    pub cross_class_points: Vec<CalibrationPoint>,
}

/// Pure half: pair (cosine, verdicts-agree) observations → the curve at
/// each threshold from 0.80 to 0.99. Split from the query and the
/// embedder so the arithmetic is a unit test, not a server run.
pub(crate) fn calibration_points(sims: &[(f64, bool)]) -> Vec<CalibrationPoint> {
    (80..100)
        .map(|t| {
            let threshold = t as f64 / 100.0;
            let (mut pairs, mut agree) = (0usize, 0usize);
            for &(sim, same) in sims {
                if sim >= threshold {
                    pairs += 1;
                    if same {
                        agree += 1;
                    }
                }
            }
            CalibrationPoint {
                threshold,
                pairs,
                agree,
                agreement: (pairs > 0).then(|| agree as f64 / pairs as f64),
            }
        })
        .collect()
}

/// Measure what a cascade at each threshold WOULD have done to the
/// owner's own history: over every pair of human-decided candidates whose
/// statements sit at-or-above a threshold, how often did the two verdicts
/// agree? A cascade fans one verdict across a group, so the disagreement
/// rate at a threshold is exactly the rate at which that cascade would
/// have overwritten a verdict the owner actually gave differently.
///
/// Human verdicts only ([`crate::ladder::HUMAN_VERDICT_SQL`]): cascade
/// rows are excluded structurally — they were MADE by a similarity
/// threshold, and letting them into the measurement would validate the
/// threshold with its own output. Deterministically capped (stride, not
/// head — the head of an id-ordered table is the oldest era of the
/// corpus) so the pair count stays computable.
pub fn calibrate_global_threshold(
    conn: &Connection,
    embedder: &Embedder,
    task: EmbedTask,
) -> Result<CalibrationReport> {
    const CAP: usize = 4000;
    let mut stmt = conn.prepare(&format!(
        "SELECT COALESCE(json_extract(payload,'$.statement'), ''), status = 'accepted',
                COALESCE(proposed_by,'?') || ' . ' || {KEY_SQL}
         FROM fact_candidate
         WHERE status IN ('accepted','rejected')
           AND {}
           AND COALESCE(json_extract(payload,'$.statement'), '') <> ''
           AND COALESCE(json_extract(payload,'$.kind'), '') <> 'commitment'
         ORDER BY id",
        crate::ladder::HUMAN_VERDICT_SQL
    ))?;
    let mut rows: Vec<(String, bool, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if rows.len() > CAP {
        let stride = rows.len().div_ceil(CAP);
        rows = rows
            .into_iter()
            .enumerate()
            .filter(|(i, _)| i % stride == 0)
            .map(|(_, r)| r)
            .collect();
    }

    let texts: Vec<String> = rows.iter().map(|(s, _, _)| s.clone()).collect();
    let vecs = embedder.embed(&texts, task)?;

    let mut sims: Vec<(f64, bool)> = Vec::new();
    let mut same_class: Vec<(f64, bool)> = Vec::new();
    let mut cross_class: Vec<(f64, bool)> = Vec::new();
    for i in 0..vecs.len() {
        for j in (i + 1)..vecs.len() {
            let sim = cosine(&vecs[i], &vecs[j]);
            if sim >= 0.80 {
                let agree = rows[i].1 == rows[j].1;
                sims.push((sim, agree));
                if rows[i].2 == rows[j].2 {
                    same_class.push((sim, agree));
                } else {
                    cross_class.push((sim, agree));
                }
            }
        }
    }

    Ok(CalibrationReport {
        task: task.tag().to_string(),
        verdicts: rows.len(),
        points: calibration_points(&sims),
        same_class_points: calibration_points(&same_class),
        cross_class_points: calibration_points(&cross_class),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::fact;
    use rusqlite::params;

    /// The curve is cumulative from each floor upward, disagreements
    /// below a floor stop counting once the floor passes them, and an
    /// empty band reports None — no pairs is not perfect agreement.
    #[test]
    fn calibration_points_count_pairs_at_or_above_each_floor() {
        let sims = vec![
            (0.99, true),  // twins, same verdict
            (0.95, false), // close, owner split — the pair a cascade would get wrong
            (0.85, true),
        ];
        let points = calibration_points(&sims);
        let at = |t: f64| {
            points
                .iter()
                .find(|p| (p.threshold - t).abs() < 1e-9)
                .unwrap()
        };
        assert_eq!((at(0.80).pairs, at(0.80).agree), (3, 2));
        assert_eq!((at(0.90).pairs, at(0.90).agree), (2, 1));
        assert_eq!(at(0.90).agreement, Some(0.5));
        assert_eq!((at(0.96).pairs, at(0.96).agree), (1, 1));
        assert_eq!(at(0.96).agreement, Some(1.0));
        // Nothing sits at 0.995+ — but the last floor is 0.99, covered by
        // the 0.99 pair. An empty curve reports None everywhere.
        let empty = calibration_points(&[]);
        assert!(empty.iter().all(|p| p.agreement.is_none() && p.pairs == 0));
    }

    fn seed_candidate(conn: &Connection, statement: &str) -> i64 {
        conn.execute(
            "INSERT INTO fact_candidate (payload, proposed_by, status)
             VALUES (json_object('predicate','related_to','subject','Luke',
                                 'statement', ?1), 'bee:suggested', 'proposed')",
            params![statement],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    #[test]
    fn clustering_is_deterministic_and_greedy() {
        let a = vec![1.0, 0.0];
        let b = vec![0.99, 0.14]; // ~0.99 to a
        let c = vec![0.0, 1.0];
        let groups = cluster(&[a, b, c], 0.9);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0, 0, "first vector leads");
        assert_eq!(groups[0].1.len(), 1, "the near-duplicate joins it");
        assert_eq!(groups[1].0, 2, "the orthogonal one leads its own group");
    }

    /// The safety property of the whole feature: a cascade of N is ONE
    /// human verdict. Fails on any implementation that routes members
    /// through the human accept/reject paths.
    #[test]
    fn a_cascade_is_one_human_verdict() {
        let conn = open_memory().unwrap();
        let seed = seed_candidate(&conn, "Luke plays with his children in the evening");
        let m1 = seed_candidate(&conn, "Luke plays with his kids after dinner");
        let m2 = seed_candidate(&conn, "Luke spends evenings playing with the children");

        // The owner rejects the seed; the two members cascade.
        fact::reject_candidate(&conn, seed, "not graph-worthy").unwrap();
        fact::reject_candidate_cascade(&conn, m1, seed, Some(0.91)).unwrap();
        fact::reject_candidate_cascade(&conn, m2, seed, Some(0.88)).unwrap();

        let (accepted, judged) =
            crate::ladder::human_record(&conn, "bee:suggested", "related_to").unwrap();
        assert_eq!((accepted, judged), (0, 1), "one human verdict, not three");

        // And the rows say who decided them.
        let by: Vec<Option<String>> = conn
            .prepare("SELECT reviewed_by FROM fact_candidate ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        assert_eq!(by[0].as_deref(), Some("user"));
        assert!(by[1].as_deref().unwrap().starts_with("cascade:"));
        assert!(by[2].as_deref().unwrap().starts_with("cascade:"));
    }

    /// The accept side of the same property, through the ladder's own gate:
    /// cascaded accepts must not push a class toward promotion.
    #[test]
    fn cascaded_accepts_never_feed_the_ladder() {
        let conn = open_memory().unwrap();
        let node = crate::graph::Node::new("person-luke", "person", "Luke");
        crate::graph::upsert_node(&conn, &node).unwrap();
        let seed = seed_candidate(&conn, "Luke plays with his children in the evening");
        let m1 = seed_candidate(&conn, "Luke plays with his kids after dinner");

        fact::accept_candidate_opts(&conn, seed, false, true).unwrap();
        fact::accept_candidate_cascade(&conn, m1, seed).unwrap();

        let (accepted, judged) =
            crate::ladder::human_record(&conn, "bee:suggested", "related_to").unwrap();
        assert_eq!(
            (accepted, judged),
            (1, 1),
            "the cascade member is not the owner's accept"
        );
    }

    #[test]
    fn a_cascade_refuses_a_decided_seed() {
        let conn = open_memory().unwrap();
        let seed = seed_candidate(&conn, "already handled");
        fact::reject_candidate(&conn, seed, "no").unwrap();
        let e = seed_class(&conn, seed).unwrap_err();
        assert!(e.to_string().contains("no pending candidate"));
    }

    /// The NULL trap in the shared human predicate, pinned. A legacy row
    /// (`reviewed_by` NULL, machine reject reason) made `reviewed_by='user'`
    /// evaluate SQL NULL, NULL survives OR-with-false, and a class whose
    /// decided rows were ALL machine rejects summed to NULL — which errored
    /// the cluster view and everything reading it. Fails on the pre-COALESCE
    /// spelling of HUMAN_VERDICT_SQL.
    #[test]
    fn a_class_of_only_legacy_machine_rejects_still_reads() {
        let conn = open_memory().unwrap();
        for i in 0..3 {
            conn.execute(
                "INSERT INTO fact_candidate
                     (payload, proposed_by, status, reviewed_at, reject_reason, reviewed_by)
                 VALUES (json_object('predicate','related_to','subject','X',
                                     'statement','s'||?1),
                         'linker:knn', 'rejected', '2026-08-01 00:00:00',
                         'precheck: duplicate', NULL)",
                params![i],
            )
            .unwrap();
        }
        let clusters = crate::precheck::review_clusters(&conn, 0).unwrap();
        // The class has history but no pending rows, so it may not render a
        // cluster row — the assertion is that reading did not error, and
        // that the human record over the same predicate is empty.
        drop(clusters);
        let (a, j) = crate::ladder::human_record(&conn, "linker:knn", "related_to").unwrap();
        assert_eq!((a, j), (0, 0), "machine rejects are nobody's verdicts");
    }

    /// The way through `cannot resolve subject` at group scale: the seed is
    /// accepted with subject creation (the human pressing `A` is the review
    /// that creation requires), and the cascade's members — who share the
    /// subject, which is what made them a group — resolve against the node
    /// the seed just created. No member needs creation rights of its own.
    #[test]
    fn a_created_subject_unblocks_the_cascade_behind_it() {
        let conn = open_memory().unwrap();
        let mk = |st: &str| {
            conn.execute(
                "INSERT INTO fact_candidate (payload, proposed_by, status)
                 VALUES (json_object('predicate','uses','subject','Hungary',
                                     'statement', ?1), 'llm', 'proposed')",
                params![st],
            )
            .unwrap();
            conn.last_insert_rowid()
        };
        let seed = mk("Hungary uses a 230 V standard");
        let m1 = mk("Hungary uses Type C and F plugs");

        // Without creation the seed refuses — the failure the hint names.
        let err = fact::accept_candidate_opts(&conn, seed, false, true).unwrap_err();
        assert!(err.to_string().contains("cannot resolve subject"));

        // With it, the seed lands and the member follows with none.
        fact::accept_candidate_opts(&conn, seed, true, true).unwrap();
        fact::accept_candidate_cascade(&conn, m1, seed).unwrap();
        let (accepted, judged) = crate::ladder::human_record(&conn, "llm", "uses").unwrap();
        assert_eq!((accepted, judged), (1, 1), "still one human verdict");
    }

    /// An explicit cascade set is vetted, not trusted: pending same-class
    /// ids pass, decided ones drop silently (the nightly may have raced the
    /// sitting), and a cross-class id is an error — the no-crossing rule is
    /// structural, and silently dropping a violation would hide the bug.
    #[test]
    fn an_explicit_cascade_set_is_vetted_against_the_seeds_class() {
        let conn = open_memory().unwrap();
        let seed = seed_candidate(&conn, "Luke plays with his children");
        let ok = seed_candidate(&conn, "Luke plays with his kids");
        let decided = seed_candidate(&conn, "Luke plays peekaboo");
        fact::reject_candidate(&conn, decided, "no").unwrap();
        conn.execute(
            "INSERT INTO fact_candidate (payload, proposed_by, status)
             VALUES (json_object('predicate','uses','subject','X','statement','other class'),
                     'llm', 'proposed')",
            [],
        )
        .unwrap();
        let foreign = conn.last_insert_rowid();

        let vetted = vet_cascade_ids(&conn, seed, &[ok, decided, seed]).unwrap();
        assert_eq!(vetted, vec![ok], "decided and the seed itself drop out");

        let err = vet_cascade_ids(&conn, seed, &[ok, foreign]).unwrap_err();
        assert!(err.to_string().contains("never crosses a class"), "{err}");

        let err = vet_cascade_ids(&conn, seed, &[999_999]).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }

    /// The listing covers the whole class: singletons are rows now, not
    /// omissions. Fails on the version that filtered them out of
    /// `groups_for_class` — which showed 31 of a class's 159 and stranded
    /// the rest in another surface. Needs the embedding server; skips
    /// without it, like the confinement tests skip without their backend.
    #[test]
    fn grouping_covers_the_whole_class_singletons_included() {
        let e = Embedder::default();
        if !e.available() {
            eprintln!("skipping: no embedding server at :8081");
            return;
        }
        let conn = open_memory().unwrap();
        seed_candidate(&conn, "Luke plays with his children in the evening");
        seed_candidate(&conn, "Luke plays with his kids after dinner");
        seed_candidate(&conn, "The scanner requires a safety briefing");
        let groups =
            groups_for_class(&conn, &e, "bee:suggested", "related_to", GROUP_THRESHOLD).unwrap();
        let covered: usize = groups.iter().map(SimilarGroup::size).sum();
        assert_eq!(covered, 3, "every pending row is in exactly one group");
    }

    fn seed_candidate_in(
        conn: &Connection,
        proposer: &str,
        predicate: &str,
        statement: &str,
    ) -> i64 {
        conn.execute(
            "INSERT INTO fact_candidate (payload, proposed_by, status)
             VALUES (json_object('predicate', ?2, 'subject','Luke',
                                 'statement', ?3), ?1, 'proposed')",
            params![proposer, predicate, statement],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// The global layer's whole point, as a pure test: a group forms across
    /// two classes, and the blast radius names both. Fails on any assembly
    /// that filters members to the leader's class.
    #[test]
    fn the_global_view_crosses_classes_and_names_the_blast_radius() {
        let rows = vec![
            (
                1_i64,
                "Luke has twin daughters".to_string(),
                "llm . has".to_string(),
            ),
            (
                2_i64,
                "Luke has twin girls".to_string(),
                "bee:suggested . family".to_string(),
            ),
            (
                3_i64,
                "The scanner needs a briefing".to_string(),
                "llm . uses".to_string(),
            ),
        ];
        let vecs = vec![vec![1.0, 0.0], vec![0.98, 0.19], vec![0.0, 1.0]];
        let groups = assemble_global_groups(&rows, &vecs, 0.9);
        assert_eq!(
            groups.len(),
            1,
            "singletons are dropped from the global view"
        );
        let g = &groups[0];
        assert_eq!(g.leader_id, 1);
        assert_eq!(
            g.members,
            vec![(2, crate::precheck::cosine(&vecs[0], &vecs[1]))]
        );
        assert_eq!(g.classes.len(), 2, "both classes are named");
        assert_eq!(g.classes["llm . has"], 1);
        assert_eq!(g.classes["bee:suggested . family"], 1);
    }

    /// The across-vet keeps exactly what the class vet refuses — a pending
    /// member of another class — while staying strict about everything
    /// else: decided ids are dropped, unknown ids are an error.
    #[test]
    fn the_across_vet_admits_other_classes_and_nothing_else_new() {
        let conn = open_memory().unwrap();
        let seed = seed_candidate_in(&conn, "llm", "has", "Luke has twin daughters");
        let foreign = seed_candidate_in(&conn, "bee:suggested", "family", "Luke has twin girls");
        let decided = seed_candidate_in(&conn, "llm", "has", "Luke has a dog");
        fact::reject_candidate(&conn, decided, "no").unwrap();

        // The class vet refuses the foreign id; the across vet keeps it.
        let err = vet_cascade_ids(&conn, seed, &[foreign]).unwrap_err();
        assert!(err.to_string().contains("never crosses a class"), "{err}");
        let kept = vet_cascade_ids_across(&conn, seed, &[foreign, decided, seed]).unwrap();
        assert_eq!(
            kept,
            vec![foreign],
            "pending crosses; decided drops; the seed never rides"
        );

        let err = vet_cascade_ids_across(&conn, seed, &[999_999]).unwrap_err();
        assert!(err.to_string().contains("does not exist"), "{err}");
    }
}
