//! Auto-linking cascade (§7): cheapest and most precise first. Tier 1
//! (deterministic keys) lives in the sources; this module implements:
//!
//! - **Tier 2** — NPMI co-occurrence over `mention`. Plain co-occurrence links
//!   everything to the graph's center; NPMI corrects for frequency.
//!   Threshold ~0.3, ≥3 co-occurrences. Writes `related_to` facts.
//! - **Tier 3** — temporal join: Bee conversations × calendar events by
//!   interval overlap → attendees plausibly present in the recording.
//!   Probabilistic, not fact (§7 warning): confidence well under 1.0,
//!   extractor='temporal_join', weight = coverage fraction.
//! - **Tier 4** — embedding kNN over node centroids (mean of the node's
//!   episode embeddings, mean-centered to strip corpus anisotropy). Catches
//!   paraphrase: same-type nodes that live in semantically similar contexts
//!   but never co-occur. Gated by node_type (no persons — their centroids
//!   collapse onto the corpus house style). Speculative → staged as
//!   fact_candidates, never direct.
//! - **Tier 5** — structural link prediction (Adamic-Adar). Speculative →
//!   staged as fact_candidates, never direct.

use crate::episode::add_mention;
use crate::error::Result;
use crate::fact;
use rusqlite::{params, Connection};
use serde::Serialize;
use std::collections::HashMap;

#[derive(Debug, Default, Serialize)]
pub struct LinkReport {
    pub npmi_facts: usize,
    pub temporal_mentions: usize,
    pub knn_candidates: usize,
    pub structural_candidates: usize,
    pub rule_candidates: usize,
    /// Why the temporal join attributed as little as it did.
    pub temporal: TemporalReport,
}

/// The temporal join's own accounting.
///
/// `temporal_mentions` alone cannot answer the question anyone actually has,
/// which is whether a small number means "the wearable was off" or "the join
/// threw the recording away". Those have opposite remedies — one is physics,
/// the other is a tunable — and they were indistinguishable from one line of
/// output. Every counter here is a rejection reason, so the shortfall is
/// attributable.
#[derive(Debug, Default, Serialize)]
pub struct TemporalReport {
    /// Bee recordings usable by the join at all — an episode with no
    /// `occurred_end` is invisible to it, since coverage needs a duration.
    pub bee_with_end: usize,
    pub bee_without_end: usize,
    /// Overlapping (recording, event) pairs found before any filtering.
    pub overlaps: usize,
    /// Overlaps discarded for covering less than [`TEMPORAL_MIN_COVERAGE`]
    /// of the meeting — the tunable, and the one to look at first.
    pub below_coverage: usize,
    /// Overlaps that passed coverage but whose event listed no person
    /// attendee, so there was nobody to attribute. A calendar habit, not a
    /// recording problem.
    pub no_attendees: usize,
    /// Overlaps that produced at least one attribution.
    pub attributed_pairs: usize,
    /// A few of the events behind `no_attendees`, because the count alone
    /// cannot separate the two cases it lumps together: a solo block that
    /// SHOULD have no attendees ("Childcare", "Write") and a real meeting
    /// whose invitees never resolved to person nodes. The first is correct
    /// behaviour; the second is recoverable precision. Titles tell them
    /// apart at a glance, and nothing else in the pipeline does.
    pub no_attendee_samples: Vec<String>,
    /// Attributions from a person NAMED IN THE TITLE rather than invited —
    /// `extractor='temporal_title'`, its own weaker confidence band, and
    /// counted separately so its precision can be judged (and reverted) on
    /// its own evidence rather than hidden inside the attendee number.
    pub title_attributions: usize,
    /// Overlaps that yielded people ONLY via the title — the pairs this
    /// tier rescued, which produced nothing at all before.
    pub title_only_pairs: usize,
}

// ─── Tier 2: NPMI co-occurrence ──────────────────────────────────────────────

pub const NPMI_THRESHOLD: f64 = 0.3;
pub const NPMI_MIN_COOCCUR: i64 = 3;

/// Node types worth co-occurrence linking. Event nodes co-occur trivially
/// with their own attendees (same episode) — that's already a Tier-1 fact.
const NPMI_TYPES: &str = "('person','project','org','topic','place')";

/// Episodes mentioning both nodes — a co-occurrence fact's input set.
/// Also the Verifier's re-derivation input (see `verify::rederive_npmi`).
pub fn shared_episodes(conn: &Connection, a: &str, b: &str) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT ma.episode_id FROM mention ma
         JOIN mention mb ON mb.episode_id = ma.episode_id
         WHERE ma.node_id = ?1 AND mb.node_id = ?2",
    )?;
    let ids = stmt
        .query_map(params![a, b], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
}

pub fn link_npmi(conn: &Connection) -> Result<usize> {
    // N = episodes that have at least one mention (the co-occurrence universe).
    let n_total: i64 =
        conn.query_row("SELECT COUNT(DISTINCT episode_id) FROM mention", [], |r| {
            r.get(0)
        })?;
    if n_total < 10 {
        return Ok(0); // not enough signal to compute meaningful probabilities
    }

    // Pair counts and marginals in one pass.
    let sql = format!(
        "WITH m AS (
             SELECT DISTINCT mention.episode_id, mention.node_id FROM mention
             JOIN nodes ON nodes.id = mention.node_id
             WHERE nodes.node_type IN {NPMI_TYPES}
         ),
         counts AS (SELECT node_id, COUNT(*) c FROM m GROUP BY node_id)
         SELECT a.node_id, b.node_id, COUNT(*) AS co, ca.c, cb.c
         FROM m a
         JOIN m b ON a.episode_id = b.episode_id AND a.node_id < b.node_id
         JOIN counts ca ON ca.node_id = a.node_id
         JOIN counts cb ON cb.node_id = b.node_id
         GROUP BY a.node_id, b.node_id
         HAVING co >= ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(String, String, i64, i64, i64)> = stmt
        .query_map(params![NPMI_MIN_COOCCUR], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<std::result::Result<_, _>>()?;

    let n = n_total as f64;
    let mut created = 0;
    for (a, b, co, ca, cb) in rows {
        let p_ab = co as f64 / n;
        let p_a = ca as f64 / n;
        let p_b = cb as f64 / n;
        let pmi = (p_ab / (p_a * p_b)).ln();
        let npmi = pmi / -(p_ab.ln());
        if npmi < NPMI_THRESHOLD {
            continue;
        }

        // Skip pairs already connected by any live fact (either direction).
        let connected: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM fact_current
             WHERE (subject_id = ?1 AND object_id = ?2)
                OR (subject_id = ?2 AND object_id = ?1)",
            params![a, b],
            |r| r.get(0),
        )?;
        if connected {
            continue;
        }

        let name = |id: &str| -> Result<String> {
            Ok(
                conn.query_row("SELECT name FROM nodes WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })?,
            )
        };
        let (na, nb) = (name(&a)?, name(&b)?);
        let uid = fact::assert_fact(
            conn,
            &a,
            "related_to",
            Some(&b),
            None,
            &format!("{na} and {nb} frequently co-occur ({co} shared episodes, NPMI {npmi:.2})"),
            None,
            None,
            (npmi).min(0.85), // Tier-2 precision bar is 0.85 — cap confidence there
            "npmi",
        )?;
        // The derivation's full input set: sensitivity MAX over all of it
        // (aggregation is a hop — V008), clock anchored to the newest.
        // Not one row per input: those would read as N corroborations.
        fact::attach_derivation(conn, &uid, &shared_episodes(conn, &a, &b)?)?;
        created += 1;
    }
    Ok(created)
}

// ─── Tier 3: temporal join (calendar × Bee) ──────────────────────────────────

/// Minimum fraction of the meeting covered by the recording before we
/// attribute attendance. Time-only overlap is weak evidence (§7).
pub const TEMPORAL_MIN_COVERAGE: f64 = 0.25;

pub fn link_temporal(conn: &Connection) -> Result<usize> {
    Ok(link_temporal_reported(conn)?.0)
}

/// As [`link_temporal`], and says why it attributed what it did.
pub fn link_temporal_reported(conn: &Connection) -> Result<(usize, TemporalReport)> {
    let mut rep = TemporalReport::default();

    // How much of the wearable's output the join can even see. A recording
    // with no end time has no duration, so coverage is undefined and the
    // SQL below skips it silently — counted here so "the join found little"
    // can be told apart from "the join was handed little".
    let (with_end, without_end): (i64, i64) = conn.query_row(
        "SELECT SUM(occurred_end IS NOT NULL), SUM(occurred_end IS NULL)
         FROM episode WHERE source = 'bee.conversation'",
        [],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            ))
        },
    )?;
    rep.bee_with_end = with_end as usize;
    rep.bee_without_end = without_end as usize;

    // Bee conversations overlapping calendar events, with coverage weight.
    let mut stmt = conn.prepare(
        "SELECT bee.id, cal.id,
                (julianday(MIN(cal.occurred_end, bee.occurred_end))
               - julianday(MAX(cal.occurred_at, bee.occurred_at)))
              / NULLIF(julianday(cal.occurred_end) - julianday(cal.occurred_at), 0) AS w
         FROM episode cal
         JOIN episode bee
           ON bee.source = 'bee.conversation'
          AND bee.occurred_end IS NOT NULL
          AND bee.occurred_at < cal.occurred_end
          AND bee.occurred_end > cal.occurred_at
         WHERE cal.source = 'calendar.event' AND cal.occurred_end IS NOT NULL",
    )?;
    let overlaps: Vec<(i64, i64, Option<f64>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    rep.overlaps = overlaps.len();

    let mut created = 0;
    for (bee_id, cal_id, w) in overlaps {
        let w = w.unwrap_or(0.0).clamp(0.0, 1.0);
        if w < TEMPORAL_MIN_COVERAGE {
            rep.below_coverage += 1;
            continue;
        }

        // Attendees of the calendar episode (Tier-1 mentions, persons only)
        // were plausibly present for what Bee recorded — attributed discussion
        // without diarization. Overlap ≠ attendance: keep confidence < 1.
        let mut stmt = conn.prepare_cached(
            "SELECT m.node_id FROM mention m
             JOIN nodes n ON n.id = m.node_id
             WHERE m.episode_id = ?1 AND m.extractor = 'attendee' AND n.node_type = 'person'",
        )?;
        let attendees: Vec<String> = stmt
            .query_map(params![cal_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;

        // A named person with no invite: the second tier.
        //
        // 312 of 808 overlapping pairs had no ATTENDEE at all, and sampling
        // their titles showed why — they are meetings the owner booked by
        // writing the name down rather than sending an invite: "Meet with
        // Ana", "Lunch w/ Bob?", "Dinner w/ Alice Smith", one even
        // carrying the address ("booked: Sam sam.smith.gr@…"). The
        // alias scan had ALREADY found those people and written mentions on
        // the calendar episode; this join simply refused to look at them,
        // filtering on extractor='attendee'.
        //
        // So look — but never at the same strength. An ATTENDEE field is a
        // record that someone was invited; a name in a title is the owner's
        // shorthand, and shorthand lies in a specific way: "Read Casey's
        // Dissertation" names Casey, who is emphatically not in the room.
        // The evidence is weaker per attribution AND wrong in a whole class
        // of cases, so it gets its own lower band rather than a discount on
        // the same one. Both stay far below Tier-1, which is the promise
        // this join has always made: overlap is not attendance.
        let mut stmt = conn.prepare_cached(
            "SELECT m.node_id FROM mention m
             JOIN nodes n ON n.id = m.node_id
             WHERE m.episode_id = ?1 AND m.extractor = 'alias' AND n.node_type = 'person'",
        )?;
        let named: Vec<String> = stmt
            .query_map(params![cal_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;

        if attendees.is_empty() {
            rep.no_attendees += 1;
            if named.is_empty() && rep.no_attendee_samples.len() < 15 {
                // Sample only what STILL yields nobody — with the title tier
                // in place, these are the genuinely attendee-less events
                // ("Haircut"), and that is the residue worth eyeballing.
                if let Ok(title) = conn.query_row(
                    "SELECT COALESCE(body,'') FROM episode WHERE id = ?1",
                    params![cal_id],
                    |r| r.get::<_, String>(0),
                ) {
                    let first = title.lines().next().unwrap_or("").trim().to_string();
                    if !first.is_empty() {
                        rep.no_attendee_samples
                            .push(first.chars().take(70).collect());
                    }
                }
            }
        } else {
            rep.attributed_pairs += 1;
        }

        let confidence = 0.35 + 0.4 * w; // 0.35..0.75, never Tier-1 territory
        for person in &attendees {
            add_mention(conn, bee_id, person, "temporal_join", confidence)?;
            created += 1;
        }

        // Title-named people the invite list did not already cover.
        let title_confidence = 0.20 + 0.25 * w; // 0.20..0.45, below every attendee
        for person in named.iter().filter(|p| !attendees.contains(p)) {
            add_mention(conn, bee_id, person, "temporal_title", title_confidence)?;
            created += 1;
            rep.title_attributions += 1;
        }
        if attendees.is_empty() && !named.is_empty() {
            rep.title_only_pairs += 1;
        }

        // Also link the event node itself to the recording episode.
        let mut stmt = conn.prepare_cached(
            "SELECT m.node_id FROM mention m
             JOIN nodes n ON n.id = m.node_id
             WHERE m.episode_id = ?1 AND n.node_type = 'event'",
        )?;
        let events: Vec<String> = stmt
            .query_map(params![cal_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        for ev in events {
            add_mention(conn, bee_id, &ev, "temporal_join", confidence)?;
        }
    }
    Ok((created, rep))
}

// ─── Tier 4: embedding kNN over node centroids ───────────────────────────────

/// Centroids need enough episodes to be stable; a 1-episode "centroid" is just
/// that episode's vector and pairs trivially with its neighbors.
pub const KNN_MIN_MENTIONS: usize = 3;
/// Threshold on MEAN-CENTERED cosine, so it is far lower than a raw-cosine
/// bar would be: raw cosines in this corpus cluster at 0.9+ for unrelated
/// nodes (anisotropy), while centered cosines put unrelated pairs near 0.
/// Calibrated on the live DB 2026-08-03: true pairs scored 0.17–0.59, the
/// first junk pair 0.03.
pub const KNN_SIM_THRESHOLD: f64 = 0.15;
pub const KNN_MAX_CANDIDATES: usize = 40;

/// Paraphrase-linkable types only. Persons are deliberately excluded: their
/// centroids collapse onto the corpus house style (every calendar body reads
/// alike), so person×person cosine is anisotropy, not relatedness — identity
/// keys and co-occurrence own that space. Verified on the live DB 2026-08-03:
/// with persons in, 39/40 staged pairs were calendar-style artifacts.
const KNN_TYPES: &str = "('project','org','topic','place')";

fn parse_embedding(raw: rusqlite::types::ValueRef<'_>) -> Option<Vec<f32>> {
    match raw {
        rusqlite::types::ValueRef::Blob(b) if b.len() % 4 == 0 => Some(
            b.chunks_exact(4)
                .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
                .collect(),
        ),
        rusqlite::types::ValueRef::Text(t) => serde_json::from_slice::<Vec<f32>>(t).ok(),
        _ => None,
    }
}

pub fn link_knn(conn: &Connection) -> Result<usize> {
    // Episode vectors, loaded once (dominates the mention join otherwise).
    let mut stmt = conn.prepare("SELECT episode_id, embedding FROM vec_episode")?;
    let vectors: HashMap<i64, Vec<f32>> = stmt
        .query_map([], |r| {
            Ok((r.get::<_, i64>(0)?, parse_embedding(r.get_ref(1)?)))
        })?
        .filter_map(|row| match row {
            Ok((id, Some(v))) => Some(Ok((id, v))),
            Ok((_, None)) => None,
            Err(e) => Some(Err(e)),
        })
        .collect::<std::result::Result<_, _>>()?;
    if vectors.len() < 10 {
        return Ok(0); // nothing meaningful to compare yet
    }

    // Accumulate per-node centroids and per-node episode sets.
    let mut stmt = conn.prepare(&format!(
        "SELECT m.node_id, n.node_type, m.episode_id FROM mention m
         JOIN nodes n ON n.id = m.node_id
         WHERE n.node_type IN {KNN_TYPES}"
    ))?;
    let mentions: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    struct Acc {
        node_type: String,
        sum: Vec<f64>,
        episodes: Vec<i64>,
    }
    let mut acc: HashMap<String, Acc> = HashMap::new();
    for (node_id, node_type, episode_id) in mentions {
        let Some(vec) = vectors.get(&episode_id) else {
            continue;
        };
        let a = acc.entry(node_id).or_insert_with(|| Acc {
            node_type,
            sum: vec![0.0; vec.len()],
            episodes: Vec::new(),
        });
        if a.sum.len() != vec.len() {
            continue; // mixed dimensionality — shouldn't happen, but never panic
        }
        for (s, x) in a.sum.iter_mut().zip(vec) {
            *s += *x as f64;
        }
        a.episodes.push(episode_id);
    }

    // Raw centroids (mean of episode vectors) for nodes with enough signal.
    struct Centroid {
        id: String,
        node_type: String,
        unit: Vec<f64>,
        episodes: Vec<i64>,
    }
    let mut centroids: Vec<Centroid> = Vec::new();
    for (id, mut a) in acc {
        if a.episodes.len() < KNN_MIN_MENTIONS {
            continue;
        }
        let count = a.episodes.len() as f64;
        for x in a.sum.iter_mut() {
            *x /= count;
        }
        a.episodes.sort_unstable();
        centroids.push(Centroid {
            id,
            node_type: a.node_type,
            unit: a.sum,
            episodes: a.episodes,
        });
    }
    if centroids.len() < 2 {
        return Ok(0);
    }

    // Mean-center before cosine: embedding spaces are anisotropic — every
    // centroid shares a large "this corpus" component, which inflates all
    // pairwise cosines toward 1. Subtracting the global mean leaves the
    // part of each centroid that distinguishes it from the corpus.
    let dims = centroids[0].unit.len();
    let mut mean = vec![0.0f64; dims];
    for c in &centroids {
        for (m, x) in mean.iter_mut().zip(&c.unit) {
            *m += x;
        }
    }
    for m in mean.iter_mut() {
        *m /= centroids.len() as f64;
    }
    centroids.retain_mut(|c| {
        for (x, m) in c.unit.iter_mut().zip(&mean) {
            *x -= m;
        }
        let norm = c.unit.iter().map(|x| x * x).sum::<f64>().sqrt();
        if norm <= 1e-9 {
            return false; // indistinguishable from the corpus mean
        }
        for x in c.unit.iter_mut() {
            *x /= norm;
        }
        true
    });
    centroids.sort_by(|a, b| a.id.cmp(&b.id)); // deterministic pair order

    // Same-type pairs above the similarity bar. Pairs that co-occur enough for
    // NPMI are Tier 2's domain — kNN's value is pairs that never (or barely)
    // share an episode yet live in the same semantic neighborhood.
    let mut ranked: Vec<(usize, usize, f64)> = Vec::new();
    for i in 0..centroids.len() {
        for j in (i + 1)..centroids.len() {
            let (a, b) = (&centroids[i], &centroids[j]);
            if a.node_type != b.node_type {
                continue;
            }
            let shared = {
                let (mut x, mut y, mut n) = (0, 0, 0i64);
                while x < a.episodes.len() && y < b.episodes.len() {
                    match a.episodes[x].cmp(&b.episodes[y]) {
                        std::cmp::Ordering::Less => x += 1,
                        std::cmp::Ordering::Greater => y += 1,
                        std::cmp::Ordering::Equal => {
                            n += 1;
                            x += 1;
                            y += 1;
                        }
                    }
                }
                n
            };
            if shared >= NPMI_MIN_COOCCUR {
                continue;
            }
            let sim: f64 = a.unit.iter().zip(&b.unit).map(|(x, y)| x * y).sum();
            if sim >= KNN_SIM_THRESHOLD {
                ranked.push((i, j, sim));
            }
        }
    }
    ranked.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

    let mut created = 0;
    for (i, j, sim) in ranked.into_iter().take(KNN_MAX_CANDIDATES) {
        let (a, b) = (&centroids[i].id, &centroids[j].id);
        let connected: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM fact_current
             WHERE (subject_id = ?1 AND object_id = ?2)
                OR (subject_id = ?2 AND object_id = ?1)",
            params![a, b],
            |r| r.get(0),
        )?;
        if connected {
            continue;
        }
        let staged: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM fact_candidate
             WHERE proposed_by = 'linker:knn' AND status != 'rejected'
               AND payload LIKE '%' || ?1 || '%' AND payload LIKE '%' || ?2 || '%'",
            params![a, b],
            |r| r.get(0),
        )?;
        if staged {
            continue;
        }

        let name = |id: &str| -> Result<String> {
            Ok(
                conn.query_row("SELECT name FROM nodes WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })?,
            )
        };
        let (na, nb) = (name(a)?, name(b)?);
        let proposed = fact::ProposedFact {
            subject: a.clone(),
            predicate: "related_to".into(),
            object: Some(b.clone()),
            object_value: None,
            statement: format!(
                "{na} and {nb} appear in semantically similar contexts (centered cosine {sim:.2}) — likely related"
            ),
            valid_from: None,
            // 0.80..1.0 similarity → 0.4..0.7: speculative territory, always.
            confidence: Some((0.4 + (sim - KNN_SIM_THRESHOLD) * 1.5).clamp(0.4, 0.7)),
            tags: None,
        };
        fact::propose_fact(conn, &proposed, "linker:knn", None)?;
        created += 1;
    }
    Ok(created)
}

// ─── Tier 5: structural link prediction (Adamic-Adar) ────────────────────────

pub const AA_THRESHOLD: f64 = 1.2;
pub const AA_MAX_CANDIDATES: usize = 50;
/// Resource Allocation (Zhou/Lü/Zhang): AA with 1/degree instead of
/// 1/log(degree) — punishes celebrity hubs harder. RA scores run ~4×
/// smaller than AA at typical degrees (1/d vs 1/ln d), hence the
/// scaled threshold. Hand-calibrated like AA_THRESHOLD; revisit
/// against the per-class ledger once verdicts accumulate.
pub const RA_THRESHOLD: f64 = 0.3;

pub fn link_structural(conn: &Connection) -> Result<usize> {
    // Undirected adjacency over the current edges view.
    let mut stmt = conn.prepare("SELECT from_id, to_id FROM edges")?;
    let edges: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if edges.len() < 10 {
        return Ok(0); // §7: useful past a few thousand edges; pointless below
    }

    let mut adj: HashMap<String, Vec<String>> = HashMap::new();
    for (a, b) in &edges {
        adj.entry(a.clone()).or_default().push(b.clone());
        adj.entry(b.clone()).or_default().push(a.clone());
    }
    for v in adj.values_mut() {
        v.sort();
        v.dedup();
    }

    // Score distance-2 pairs by Adamic-Adar AND Resource Allocation
    // through their common neighbors — same pass, two weightings.
    // Agreement between the two is a confidence signal (and its own
    // class in the ledger, so its precision is measurable).
    let mut scores: HashMap<(String, String), (f64, f64)> = HashMap::new();
    for (z, neighbors) in &adj {
        let deg = neighbors.len() as f64;
        if deg < 2.0 {
            continue;
        }
        let w_aa = 1.0 / deg.ln().max(0.1);
        let w_ra = 1.0 / deg;
        let _ = z;
        for i in 0..neighbors.len() {
            for j in (i + 1)..neighbors.len() {
                let (u, w) = (&neighbors[i], &neighbors[j]);
                let key = if u < w {
                    (u.clone(), w.clone())
                } else {
                    (w.clone(), u.clone())
                };
                let e = scores.entry(key).or_insert((0.0, 0.0));
                e.0 += w_aa;
                e.1 += w_ra;
            }
        }
    }

    let mut ranked: Vec<((String, String), (f64, f64))> = scores
        .into_iter()
        .filter(|(_, (aa, ra))| *aa >= AA_THRESHOLD || *ra >= RA_THRESHOLD)
        .collect();
    ranked.sort_by(|a, b| {
        (b.1 .0 + 4.0 * b.1 .1)
            .partial_cmp(&(a.1 .0 + 4.0 * a.1 .1))
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    let mut created = 0;
    for ((a, b), (aa, ra)) in ranked.into_iter().take(AA_MAX_CANDIDATES) {
        // Skip already-connected pairs and pairs already staged.
        let connected: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM fact_current
             WHERE (subject_id = ?1 AND object_id = ?2)
                OR (subject_id = ?2 AND object_id = ?1)",
            params![a, b],
            |r| r.get(0),
        )?;
        if connected {
            continue;
        }
        let staged: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM fact_candidate
             WHERE proposed_by LIKE 'linker:%' AND status = 'proposed'
               AND payload LIKE '%' || ?1 || '%' AND payload LIKE '%' || ?2 || '%'",
            params![a, b],
            |r| r.get(0),
        )?;
        if staged {
            continue;
        }

        let name = |id: &str| -> Result<String> {
            Ok(
                conn.query_row("SELECT name FROM nodes WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })?,
            )
        };
        let (na, nb) = (name(&a)?, name(&b)?);
        // Three classes with distinct ledger rows: agreement is the
        // high-precision hypothesis; each heuristic alone earns (or
        // loses) trust separately.
        let aa_conf = (aa / 5.0).clamp(0.3, 0.7);
        let ra_conf = (ra / 1.2).clamp(0.3, 0.7);
        let (proposer, conf, evidence) = match (aa >= AA_THRESHOLD, ra >= RA_THRESHOLD) {
            (true, true) => (
                "linker:aa+ra",
                (aa_conf.max(ra_conf) + 0.1).min(0.8),
                format!("Adamic-Adar {aa:.2} and Resource-Allocation {ra:.2} agree"),
            ),
            (true, false) => (
                "linker:adamic_adar",
                aa_conf,
                format!("Adamic-Adar {aa:.2}"),
            ),
            _ => (
                "linker:resource_allocation",
                ra_conf,
                format!("Resource-Allocation {ra:.2}"),
            ),
        };
        let proposed = fact::ProposedFact {
            subject: a.clone(),
            predicate: "related_to".into(),
            object: Some(b.clone()),
            object_value: None,
            statement: format!("{na} and {nb} share graph structure ({evidence}) — likely related"),
            valid_from: None,
            confidence: Some(conf),
            tags: None,
        };
        fact::propose_fact(conn, &proposed, proposer, None)?;
        created += 1;
    }
    Ok(created)
}

/// Retrofit provenance onto co-occurrence facts written before
/// `attach_derivation` existed: they cite no episode, so they were
/// unverifiable, defaulted to the `personal` tier regardless of what
/// they were derived from, and had no world-time clock. Recomputes each
/// one's contributing set and attaches it. Returns (scanned, retiered).
pub fn backfill_npmi_derivation(conn: &Connection) -> Result<(usize, usize)> {
    let rows: Vec<(String, String, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT uid, subject_id, object_id, sensitivity FROM fact
             WHERE extractor = 'npmi' AND object_id IS NOT NULL
               AND episode_id IS NULL
               AND valid_to IS NULL AND invalidated_at IS NULL",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    let (mut scanned, mut retiered) = (0, 0);
    for (uid, a, b, before) in rows {
        let shared = shared_episodes(conn, &a, &b)?;
        if shared.is_empty() {
            continue; // the mentions are gone; leave it for the Verifier to refute
        }
        fact::attach_derivation(conn, &uid, &shared)?;
        scanned += 1;
        if let Some(f) = fact::get_fact_by_uid(conn, &uid)? {
            if f.sensitivity != before {
                retiered += 1;
            }
        }
    }
    Ok((scanned, retiered))
}

/// Backfill first-name aliases for existing persons (new persons get them at
/// creation). Deterministic; collisions are disambiguation's job.
pub fn backfill_firstname_aliases(conn: &Connection) -> Result<usize> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO node_alias (node_id, alias, source)
         SELECT id, substr(canonical_name, 1, instr(canonical_name, ' ') - 1), 'firstname'
         FROM nodes
         WHERE node_type = 'person' AND instr(canonical_name, ' ') > 3",
        [],
    )?;
    Ok(n)
}

/// Run the full cascade in order (§7: cheapest and most precise first).
pub fn run_cascade(conn: &Connection) -> Result<LinkReport> {
    let mut report = LinkReport::default();
    backfill_firstname_aliases(conn)?;
    let (n, trep) = link_temporal_reported(conn)?; // 3 before 2: its mentions feed NPMI
    report.temporal_mentions = n;
    report.temporal = trep;
    report.npmi_facts = link_npmi(conn)?;
    report.knn_candidates = link_knn(conn)?;
    report.structural_candidates = link_structural(conn)?;
    report.rule_candidates = crate::rules::run_rules(conn)?.iter().map(|(_, n)| n).sum();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{upsert_episode, Episode};
    use crate::graph::{upsert_node, Node};

    fn ep(src: &str, sid: &str, at: &str, end: Option<&str>) -> Episode {
        Episode {
            id: 0,
            uid: String::new(),
            source: src.into(),
            source_id: sid.into(),
            source_ref: None,
            body: format!("episode {sid}"),
            occurred_at: at.into(),
            occurred_end: end.map(|s| s.into()),
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
    fn test_npmi_links_frequent_pairs_not_hubs() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        upsert_node(&conn, &Node::new("hub", "person", "Hub Person")).unwrap();

        // 12 episodes. Nadia+Aim2 co-occur in 4 (exclusively together);
        // hub appears in all 12 (co-occurs with everyone incidentally).
        for i in 0..12 {
            let (id, _) = upsert_episode(
                &conn,
                &ep("note", &format!("e{i}"), "2026-01-01 10:00:00", None),
            )
            .unwrap();
            add_mention(&conn, id, "hub", "manual", 1.0).unwrap();
            if i < 4 {
                add_mention(&conn, id, "nadia", "manual", 1.0).unwrap();
                add_mention(&conn, id, "aim2", "manual", 1.0).unwrap();
            }
        }

        let n = link_npmi(&conn).unwrap();
        assert!(n >= 1);
        let facts = fact::facts_for_node(&conn, "nadia", 10).unwrap();
        // Pair is ordered lexicographically: (aim2, nadia).
        let to_aim2 = facts
            .iter()
            .find(|f| f.object_id.as_deref() == Some("aim2") || f.subject_id == "aim2");
        assert!(to_aim2.is_some(), "exclusive pair must link");
        // The hub's NPMI with nadia: p(hub,nadia)=4/12 = p(nadia) → npmi vs
        // frequency-corrected — hub-nadia has npmi ~ pmi/-(ln p_ab) where
        // pmi = ln(1/p_hub)=ln(12/12)=0 → npmi 0 < threshold. No hub link.
        let hub_link = facts
            .iter()
            .find(|f| f.object_id.as_deref() == Some("hub") || f.subject_id == "hub");
        assert!(
            hub_link.is_none(),
            "hub co-occurrence must be frequency-corrected away"
        );
    }

    #[test]
    fn test_temporal_join_attributes_attendees() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("june", "person", "June")).unwrap();

        // Meeting 14:00–15:00; Bee recording 14:10–14:50 (coverage 0.67).
        let (cal, _) = upsert_episode(
            &conn,
            &ep(
                "calendar.event",
                "c1",
                "2026-07-30 14:00:00",
                Some("2026-07-30 15:00:00"),
            ),
        )
        .unwrap();
        add_mention(&conn, cal, "june", "attendee", 1.0).unwrap();
        let (bee, _) = upsert_episode(
            &conn,
            &ep(
                "bee.conversation",
                "b1",
                "2026-07-30 14:10:00",
                Some("2026-07-30 14:50:00"),
            ),
        )
        .unwrap();
        // Non-overlapping recording: must NOT be attributed.
        upsert_episode(
            &conn,
            &ep(
                "bee.conversation",
                "b2",
                "2026-07-30 20:00:00",
                Some("2026-07-30 20:30:00"),
            ),
        )
        .unwrap();

        let n = link_temporal(&conn).unwrap();
        assert_eq!(n, 1);

        let m: (String, f64) = conn.query_row(
            "SELECT extractor, confidence FROM mention WHERE episode_id = ?1 AND node_id = 'june'",
            params![bee], |r| Ok((r.get(0)?, r.get(1)?))).unwrap();
        assert_eq!(m.0, "temporal_join");
        assert!(
            m.1 < 1.0,
            "overlap ≠ attendance: confidence must stay under 1.0"
        );
        assert!(m.1 > 0.5, "2/3 coverage should score reasonably");
    }

    fn put_vec(conn: &Connection, episode_id: i64, hot: &[(usize, f32)]) {
        let mut v = vec![0.0f32; 768];
        for &(i, x) in hot {
            v[i] = x;
        }
        conn.execute(
            "INSERT OR REPLACE INTO vec_episode (episode_id, embedding) VALUES (?1, ?2)",
            params![episode_id, serde_json::to_string(&v).unwrap()],
        )
        .unwrap();
    }

    /// A node of `node_type` mentioned in `n` fresh episodes, each embedded
    /// with the given hot components.
    fn seed_node(conn: &Connection, id: &str, node_type: &str, n: usize, hot: &[(usize, f32)]) {
        upsert_node(conn, &Node::new(id, node_type, id)).unwrap();
        for k in 0..n {
            let (ep, _) = upsert_episode(
                conn,
                &ep("note", &format!("{id}-{k}"), "2026-01-01 10:00:00", None),
            )
            .unwrap();
            add_mention(conn, ep, id, "manual", 1.0).unwrap();
            put_vec(conn, ep, hot);
        }
    }

    #[test]
    fn test_knn_stages_similar_same_type_pairs_only() {
        let conn = open_memory().unwrap();
        // Two topics in near-identical embedding neighborhoods, never
        // co-occurring; one orthogonal topic; one person sharing the topics'
        // neighborhood (must not cross the type gate).
        seed_node(&conn, "hyperalignment", "topic", 4, &[(0, 1.0), (1, 0.1)]);
        seed_node(
            &conn,
            "functional-alignment",
            "topic",
            4,
            &[(0, 1.0), (1, 0.2)],
        );
        seed_node(&conn, "gardening", "topic", 4, &[(5, 1.0)]);
        seed_node(&conn, "alice", "person", 4, &[(0, 1.0), (1, 0.1)]);

        let n = link_knn(&conn).unwrap();
        assert_eq!(n, 1, "exactly the similar same-type pair");
        let staged = fact::pending_candidates(&conn, 100).unwrap();
        assert_eq!(staged.len(), 1);
        let payload = staged[0].payload.to_string();
        assert!(payload.contains("hyperalignment") && payload.contains("functional-alignment"));
        assert!(staged[0].proposed_by.as_deref() == Some("linker:knn"));

        // Idempotent: a second run must not re-stage the same pair.
        assert_eq!(link_knn(&conn).unwrap(), 0);
    }

    #[test]
    fn test_knn_skips_cooccurring_and_thin_nodes() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("a", "topic", "a")).unwrap();
        upsert_node(&conn, &Node::new("b", "topic", "b")).unwrap();
        // a and b co-occur in 3 episodes (NPMI's domain, not kNN's).
        for k in 0..3 {
            let (ep_id, _) = upsert_episode(
                &conn,
                &ep("note", &format!("co-{k}"), "2026-01-01 10:00:00", None),
            )
            .unwrap();
            add_mention(&conn, ep_id, "a", "manual", 1.0).unwrap();
            add_mention(&conn, ep_id, "b", "manual", 1.0).unwrap();
            put_vec(&conn, ep_id, &[(0, 1.0)]);
        }
        // Thin node: similar context but only 2 mentions — below the floor.
        seed_node(&conn, "thin", "topic", 2, &[(0, 1.0)]);
        // Padding so the ≥10-vector guard passes.
        seed_node(&conn, "pad", "topic", 5, &[(9, 1.0)]);

        assert_eq!(link_knn(&conn).unwrap(), 0);
    }

    #[test]
    fn test_structural_stages_candidates_not_facts() {
        let conn = open_memory().unwrap();
        // Star around 'core': a,b,c,d all connect to core and to filler nodes.
        for id in ["core", "a", "b", "c", "d", "e", "f", "g", "h", "i", "j"] {
            upsert_node(&conn, &Node::new(id, "topic", id)).unwrap();
        }
        for x in ["a", "b", "c", "d"] {
            fact::assert_fact(
                &conn,
                x,
                "related_to",
                Some("core"),
                None,
                &format!("{x} related core"),
                None,
                None,
                0.9,
                "manual",
            )
            .unwrap();
        }
        // Extra edges so edge count ≥ 10 and 'a','b' share a second neighbor.
        for (s, o) in [
            ("a", "e"),
            ("b", "e"),
            ("c", "f"),
            ("d", "g"),
            ("e", "h"),
            ("f", "i"),
            ("g", "j"),
            ("h", "i"),
        ] {
            fact::assert_fact(
                &conn,
                s,
                "related_to",
                Some(o),
                None,
                &format!("{s} related {o}"),
                None,
                None,
                0.9,
                "manual",
            )
            .unwrap();
        }

        let n = link_structural(&conn).unwrap();
        assert!(n > 0, "should stage some structural candidates");
        // Everything went to the queue, not the graph — under one of the
        // three neighborhood-heuristic classes (AA, RA, or agreement).
        let staged = fact::pending_candidates(&conn, 100).unwrap();
        assert_eq!(staged.len(), n);
        assert!(staged.iter().all(|c| matches!(
            c.proposed_by.as_deref(),
            Some("linker:adamic_adar" | "linker:resource_allocation" | "linker:aa+ra")
        )));
    }
}
