//! Fact layer (§4.3): one bi-temporal table. Two timelines — valid time (true
//! in the world) and system time (when we learned it). Supersede by setting
//! `valid_to` + `invalidated_at`; never delete (except redaction, §10).

use crate::error::{Error, Result};
use crate::ids::{new_uid, now};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Fact {
    pub id: i64,
    pub uid: String,
    pub subject_id: String,
    pub predicate: String,
    pub object_id: Option<String>,
    pub object_value: Option<String>,
    pub statement: String,
    pub episode_id: Option<i64>,
    pub valid_from: Option<String>,
    pub valid_to: Option<String>,
    pub ingested_at: String,
    pub invalidated_at: Option<String>,
    pub confidence: f64,
    pub weight: f64,
    pub observation_count: i64,
    pub extractor: Option<String>,
    pub tags: Option<String>,
    /// §10: inherited MAX of the evidence episodes' tiers (V008). A belief
    /// derived from a private transcript is itself private.
    pub sensitivity: String,
    /// V013: 'positive' | 'negative'. Negative facts are rejection
    /// memory — "X does NOT work at Y" stops the system re-asking; the
    /// statement text carries the negation.
    pub polarity: String,
}

pub fn row_to_fact(row: &rusqlite::Row) -> std::result::Result<Fact, rusqlite::Error> {
    Ok(Fact {
        id: row.get("id")?,
        uid: row.get("uid")?,
        subject_id: row.get("subject_id")?,
        predicate: row.get("predicate")?,
        object_id: row.get("object_id")?,
        object_value: row.get("object_value")?,
        statement: row.get("statement")?,
        episode_id: row.get("episode_id")?,
        valid_from: row.get("valid_from")?,
        valid_to: row.get("valid_to")?,
        ingested_at: row.get("ingested_at")?,
        invalidated_at: row.get("invalidated_at")?,
        confidence: row.get("confidence")?,
        weight: row.get("weight")?,
        observation_count: row.get("observation_count")?,
        extractor: row.get("extractor")?,
        tags: row.get("tags")?,
        sensitivity: row.get("sensitivity")?,
        polarity: row.get("polarity")?,
    })
}

/// Normalize a predicate through `predicate_alias`, auto-registering unknown
/// predicates. The controlled vocabulary is enforced at extractor level (LLM
/// extraction uses the closed list); deterministic/manual writes may extend it.
pub fn normalize_predicate(conn: &Connection, predicate: &str) -> Result<String> {
    let p = predicate.trim().to_lowercase().replace(' ', "_");
    if let Some(name) = conn
        .query_row(
            "SELECT name FROM predicate_alias WHERE alias = ?1",
            params![p],
            |r| r.get::<_, String>(0),
        )
        .optional()?
    {
        return Ok(name);
    }
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM predicate WHERE name = ?1",
        params![p],
        |r| r.get(0),
    )?;
    if !exists {
        conn.execute(
            "INSERT INTO predicate (name, description) VALUES (?1, 'auto-registered')",
            params![p],
        )?;
    }
    Ok(p)
}

/// Assert a (positive) fact. If an identical live triple exists, it is
/// corroborated (sighting recorded, distinct-episode counter, posterior
/// confidence) instead of duplicated. Returns the fact uid.
#[allow(clippy::too_many_arguments)]
pub fn assert_fact(
    conn: &Connection,
    subject_id: &str,
    predicate: &str,
    object_id: Option<&str>,
    object_value: Option<&str>,
    statement: &str,
    episode_id: Option<i64>,
    valid_from: Option<&str>,
    confidence: f64,
    extractor: &str,
) -> Result<String> {
    assert_fact_polarity(
        conn,
        subject_id,
        predicate,
        object_id,
        object_value,
        statement,
        episode_id,
        valid_from,
        confidence,
        extractor,
        "positive",
    )
}

/// Assert a NEGATIVE fact (V013, mechanism #6): "subject predicate
/// object is NOT true" — rejection memory that stops re-asking. The
/// statement text must carry the negation ("Vera does not work at…").
#[allow(clippy::too_many_arguments)]
pub fn assert_negative_fact(
    conn: &Connection,
    subject_id: &str,
    predicate: &str,
    object_id: Option<&str>,
    object_value: Option<&str>,
    statement: &str,
    episode_id: Option<i64>,
    confidence: f64,
    extractor: &str,
) -> Result<String> {
    assert_fact_polarity(
        conn,
        subject_id,
        predicate,
        object_id,
        object_value,
        statement,
        episode_id,
        None,
        confidence,
        extractor,
        "negative",
    )
}

#[allow(clippy::too_many_arguments)]
fn assert_fact_polarity(
    conn: &Connection,
    subject_id: &str,
    predicate: &str,
    object_id: Option<&str>,
    object_value: Option<&str>,
    statement: &str,
    episode_id: Option<i64>,
    valid_from: Option<&str>,
    confidence: f64,
    extractor: &str,
    polarity: &str,
) -> Result<String> {
    let predicate = normalize_predicate(conn, predicate)?;

    // §10: the fact inherits its evidence's tier. No episode ⇒ 'personal'.
    let sensitivity: String = match episode_id {
        Some(eid) => conn
            .query_row(
                "SELECT sensitivity FROM episode WHERE id = ?1",
                params![eid],
                |r| r.get(0),
            )
            .optional()?
            .unwrap_or_else(|| "personal".into()),
        None => "personal".into(),
    };

    // Corroboration path: same live (subject, predicate, object) AND
    // polarity — a positive sighting must never corroborate a negation.
    let existing: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, uid, sensitivity FROM fact
             WHERE subject_id = ?1 AND predicate = ?2
               AND ((object_id IS NULL AND ?3 IS NULL) OR object_id = ?3)
               AND ((?4 IS NULL) OR object_value IS NULL OR object_value = ?4)
               AND polarity = ?5
               AND valid_to IS NULL AND invalidated_at IS NULL",
            params![subject_id, predicate, object_id, object_value, polarity],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;

    if let Some((id, uid, old_sens)) = existing {
        // Sensitivity is MAX over all contributing evidence: a personal fact
        // re-observed in a private transcript becomes private, never the
        // other way (hops don't launder, and corroboration is a hop too).
        use crate::episode::sensitivity_rank;
        let sens = if sensitivity_rank(&sensitivity) > sensitivity_rank(&old_sens) {
            sensitivity.as_str()
        } else {
            old_sens.as_str()
        };
        // Every re-sighting is recorded; the corroboration COUNTER moves
        // only for a new distinct episode from a non-agent source —
        // probe/agent:* evidence must not inflate support (PLAN.md Wave 2:
        // the evidence-rooted-support guard needs an honest count).
        record_observation(
            conn,
            id,
            episode_id,
            "corroborated",
            extractor,
            Some(confidence),
        )?;
        let counts = match episode_id {
            Some(eid) => {
                // > 1: the row just inserted is this episode's first sighting.
                let seen_before: bool = conn.query_row(
                    "SELECT COUNT(*) > 1 FROM fact_observation
                     WHERE fact_id = ?1 AND episode_id = ?2",
                    params![id, eid],
                    |r| r.get(0),
                )?;
                let agent_sourced: bool = conn
                    .query_row(
                        "SELECT source LIKE 'probe%' OR source LIKE 'agent:%'
                         FROM episode WHERE id = ?1",
                        params![eid],
                        |r| r.get(0),
                    )
                    .optional()?
                    .unwrap_or(false);
                !seen_before && !agent_sourced
            }
            None => true, // episodeless assertions (manual/user) always count
        };
        conn.execute(
            "UPDATE fact SET observation_count = observation_count + ?3,
                             sensitivity = ?2
             WHERE id = ?1",
            params![id, sens, counts as i64],
        )?;
        recompute_confidence(conn, id)?;
        return Ok(uid);
    }

    let uid = new_uid();
    conn.execute(
        "INSERT INTO fact (uid, subject_id, predicate, object_id, object_value, statement,
                           episode_id, valid_from, confidence, extractor, sensitivity, polarity)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
        params![
            uid,
            subject_id,
            predicate,
            object_id,
            object_value,
            statement,
            episode_id,
            valid_from,
            confidence,
            extractor,
            sensitivity,
            polarity
        ],
    )?;
    let fact_id = conn.last_insert_rowid();
    record_observation(
        conn,
        fact_id,
        episode_id,
        "asserted",
        extractor,
        Some(confidence),
    )?;
    recompute_confidence(conn, fact_id)?;
    Ok(uid)
}

/// One sighting of a fact — the how-known / how-verified trail (V010).
#[derive(Debug, Serialize)]
pub struct Observation {
    pub episode_id: Option<i64>,
    pub observed_at: String,
    pub kind: String,
    pub method: String,
    /// Declared confidence at this sighting (V011); the founding
    /// 'asserted' value anchors the no-history prior.
    pub confidence: Option<f64>,
}

/// Record a `fact_observation` row. `kind` is one of asserted |
/// corroborated | verified | disputed | corrected (CHECK-enforced);
/// `method` is the extractor name or the checking mechanism
/// (verifier-deref, gossip:tier1/2, research:web, user).
pub fn record_observation(
    conn: &Connection,
    fact_id: i64,
    episode_id: Option<i64>,
    kind: &str,
    method: &str,
    confidence: Option<f64>,
) -> Result<()> {
    conn.execute(
        "INSERT INTO fact_observation (fact_id, episode_id, kind, method, confidence)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![fact_id, episode_id, kind, method, confidence],
    )?;
    Ok(())
}

/// The observation trail for a fact, oldest first.
pub fn observations_for_fact(conn: &Connection, fact_uid: &str) -> Result<Vec<Observation>> {
    let mut stmt = conn.prepare(
        "SELECT o.episode_id, o.observed_at, o.kind, o.method, o.confidence
         FROM fact_observation o JOIN fact f ON f.id = o.fact_id
         WHERE f.uid = ?1 ORDER BY o.observed_at, o.id",
    )?;
    let rows = stmt
        .query_map(params![fact_uid], |r| {
            Ok(Observation {
                episode_id: r.get(0)?,
                observed_at: r.get(1)?,
                kind: r.get(2)?,
                method: r.get(3)?,
                confidence: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

// ─── Confidence: Beta posterior over the observation trail (Option D) ───────
//
// PLAN.md, settled 2026-08-12. Confidence is derived, recomputable
// state — not a MAX ratchet. Posterior mean:
//
//     (α + supports) / (α + β + supports + disputes)
//
// supports  = distinct non-agent evidence episodes with kind IN
//             (asserted, corroborated) + episodeless such rows
//             (user say-so counts; probe/agent:* evidence never does).
// disputes  = rows with kind IN (disputed, corrected).
// prior     = in order of preference:
//   1. user-verified (a human accepted THIS fact in review): strong
//      high prior — post-selection, the class rate no longer applies;
//   2. class history — the (proposer, predicate) acceptance rate over
//      past review verdicts, the same prior `review --clusters` shows;
//   3. the founding declared confidence (deterministic extractors
//      never pass review; their declaration is the only signal).

/// Prior strength (pseudo-observations) for class-history and
/// declared-confidence priors.
const PRIOR_STRENGTH: f64 = 10.0;
/// Prior for human-verified facts: 0.95 at strength 20.
const USER_VERIFIED_PRIOR: (f64, f64) = (19.0, 1.0);
/// Class history below this many verdicts is too thin to be a prior.
const MIN_CLASS_HISTORY: i64 = 5;

fn beta_from_rate(p: f64) -> (f64, f64) {
    let p = p.clamp(0.05, 0.95);
    (PRIOR_STRENGTH * p, PRIOR_STRENGTH * (1.0 - p))
}

/// Read-only alias/spelling normalization — [`normalize_predicate`]
/// auto-registers unknowns, which a prior lookup must never do.
pub(crate) fn normalize_readonly(conn: &Connection, predicate: &str) -> Result<String> {
    let p = predicate.trim().to_lowercase().replace(' ', "_");
    Ok(conn
        .query_row(
            "SELECT name FROM predicate_alias WHERE alias = ?1",
            params![p],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or(p))
}

/// Crude per-token suffix stemming for predicate matching. Tokens keep at
/// least three characters, so "has"/"is"/"for" never collapse into each
/// other — a wrong stem-match rewrites meaning, and this must stay too
/// dumb to be wrong.
fn stem_token(t: &str) -> &str {
    for suf in ["ing", "ies", "ed", "es", "s"] {
        if let Some(base) = t.strip_suffix(suf) {
            if base.len() >= 3 {
                return base;
            }
        }
    }
    t
}

fn stem_predicate(p: &str) -> String {
    p.split('_').map(stem_token).collect::<Vec<_>>().join("_")
}

/// The predicate resolution ladder, one rung past the alias table:
/// an unknown predicate whose stem matches exactly ONE known predicate's
/// stem is a morphological variant of it — `collaborating_with` is
/// `collaborates_with` wearing a different tense. The match is learned
/// into `predicate_alias`, which maps INTO the closed vocabulary and
/// never grows it (that asymmetry is the point: the predicate table is
/// interpolated into the extraction prompt, so registering a predicate
/// teaches the extractor to produce more of it — an owner decision;
/// learning an alias only stops a known predicate from fragmenting).
/// No match, or more than one, changes nothing.
pub(crate) fn resolve_predicate(conn: &Connection, predicate: &str) -> Result<String> {
    let p = normalize_readonly(conn, predicate)?;
    let known: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM predicate WHERE name = ?1",
        params![p],
        |r| r.get(0),
    )?;
    if known {
        return Ok(p);
    }
    let mut stmt = conn.prepare_cached("SELECT name FROM predicate")?;
    let vocab: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    let target = stem_predicate(&p);
    let mut matches = vocab.iter().filter(|v| stem_predicate(v) == target);
    match (matches.next(), matches.next()) {
        (Some(canonical), None) => {
            conn.execute(
                "INSERT OR IGNORE INTO predicate_alias (alias, name) VALUES (?1, ?2)",
                params![p, canonical],
            )?;
            Ok(canonical.clone())
        }
        _ => Ok(p),
    }
}

/// The (proposer, predicate) review-history prior: acceptance rate over
/// past verdicts, at [`PRIOR_STRENGTH`]. None when history is too thin.
fn class_prior(conn: &Connection, proposer: &str, predicate: &str) -> Result<Option<(f64, f64)>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(json_extract(payload,'$.predicate'), ''),
                SUM(status='accepted'), SUM(status='rejected')
         FROM fact_candidate
         WHERE proposed_by = ?1 AND status IN ('accepted','rejected')
         GROUP BY 1",
    )?;
    let rows: Vec<(String, i64, i64)> = stmt
        .query_map(params![proposer], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let (mut acc, mut rej) = (0i64, 0i64);
    for (raw, a, r) in rows {
        if !raw.is_empty() && normalize_readonly(conn, &raw)? == predicate {
            acc += a;
            rej += r;
        }
    }
    if acc + rej < MIN_CLASS_HISTORY {
        return Ok(None);
    }
    Ok(Some(beta_from_rate(acc as f64 / (acc + rej) as f64)))
}

/// Methods whose observations are ONE derivation, not independent
/// sightings: co-occurrence statistics, structural heuristics, typed
/// closure rules. Two namespaces exist historically — direct-write
/// tiers use a bare extractor name (`npmi`, `temporal_join`), staged
/// tiers a prefixed proposer (`linker:aa+ra`, `rule:coauthors-…`) — so
/// this matches both rather than picking a side.
///
/// Why they are exempt from corroboration support (settled 2026-08-13):
/// citing a derivation's N inputs as N supports would rocket an NPMI
/// artifact to ~0.99 confidence — more confident than a fact you
/// verified yourself, since `supports` counts distinct cited episodes.
/// A derived class's trustworthiness belongs to the **class ledger**,
/// which already prices `npmi`/`linker:*`/`rule:*` by verdict history,
/// not to a per-fact input count. Disputes still count against them.
pub const AGGREGATE_METHOD_PREFIXES: &[&str] = &["linker:", "rule:"];
pub const AGGREGATE_METHODS: &[&str] = &[
    "npmi",
    "temporal_join",
    "knn",
    "adamic_adar",
    "resource_allocation",
    "aa+ra",
];

/// Is this observation method a derivation rather than a sighting?
pub fn is_aggregate_method(method: &str) -> bool {
    AGGREGATE_METHODS.contains(&method)
        || AGGREGATE_METHOD_PREFIXES
            .iter()
            .any(|p| method.starts_with(p))
}

/// SQL fragment excluding aggregate-method rows from a support count.
/// Kept as one string so every consumer of the trail shares the rule.
fn not_aggregate(alias: &str) -> String {
    let mut cs: Vec<String> = AGGREGATE_METHODS
        .iter()
        .map(|m| format!("{alias}.method != '{m}'"))
        .collect();
    for p in AGGREGATE_METHOD_PREFIXES {
        cs.push(format!("{alias}.method NOT LIKE '{p}%'"));
    }
    cs.join(" AND ")
}

/// Raise a derived fact's sensitivity to the MAX over its FULL
/// contributing set, and anchor its clock to the newest contributor.
///
/// The point of decoupling (settled 2026-08-13): neither privacy
/// soundness nor the staleness clock needs one stored row per input.
/// A single citation — the newest contributing episode — gives the
/// λ-staleness clock a real world-time anchor AND the Verifier a
/// dereferenceable ref, while the tier MAX is computed here over every
/// contributor and stored on the fact. Aggregation is a hop, and hops
/// don't launder (V008).
pub fn attach_derivation(conn: &Connection, uid: &str, contributors: &[i64]) -> Result<()> {
    if contributors.is_empty() {
        return Ok(());
    }
    let Some(f) = get_fact_by_uid(conn, uid)? else {
        return Ok(());
    };
    let ph = contributors
        .iter()
        .map(|_| "?")
        .collect::<Vec<_>>()
        .join(",");

    // MAX tier over every contributor, not just the cited one.
    let tiers: Vec<String> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT DISTINCT sensitivity FROM episode WHERE id IN ({ph})"
        ))?;
        let rows = stmt
            .query_map(rusqlite::params_from_iter(contributors), |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    use crate::episode::sensitivity_rank;
    let mut top = f.sensitivity.clone();
    for t in tiers {
        if sensitivity_rank(&t) > sensitivity_rank(&top) {
            top = t;
        }
    }

    // Clock anchor: the newest contributor by world time.
    let newest: Option<i64> = conn
        .query_row(
            &format!(
                "SELECT id FROM episode WHERE id IN ({ph})
                 ORDER BY occurred_at DESC LIMIT 1"
            ),
            rusqlite::params_from_iter(contributors),
            |r| r.get(0),
        )
        .optional()?;

    conn.execute(
        "UPDATE fact SET sensitivity = ?2, episode_id = COALESCE(?3, episode_id)
         WHERE id = ?1",
        params![f.id, top, newest],
    )?;
    // Point the founding observation at the anchor too, so the trail and
    // the fact agree about what this belief cites.
    if let Some(anchor) = newest {
        conn.execute(
            "UPDATE fact_observation SET episode_id = ?2
             WHERE fact_id = ?1 AND kind = 'asserted' AND episode_id IS NULL",
            params![f.id, anchor],
        )?;
    }
    recompute_confidence(conn, f.id)?;
    Ok(())
}

/// Recompute a fact's confidence from its observation trail and store
/// it. Called after every trail write; also the batch path for
/// `pkg recompute-confidence`.
pub fn recompute_confidence(conn: &Connection, fact_id: i64) -> Result<f64> {
    // Aggregate-method rows are one derivation, not independent
    // sightings — excluded from support so a derived class's confidence
    // rests on its class-ledger prior instead of its input count.
    let supports: i64 = conn.query_row(
        &format!(
            "SELECT (SELECT COUNT(DISTINCT o.episode_id) FROM fact_observation o
                     JOIN episode e ON e.id = o.episode_id
                     WHERE o.fact_id = ?1 AND o.kind IN ('asserted','corroborated')
                       AND e.source NOT LIKE 'probe%' AND e.source NOT LIKE 'agent:%'
                       AND {agg})
                  + (SELECT COUNT(*) FROM fact_observation o
                     WHERE o.fact_id = ?1 AND o.episode_id IS NULL
                       AND o.kind IN ('asserted','corroborated')
                       AND {agg})",
            agg = not_aggregate("o")
        ),
        params![fact_id],
        |r| r.get(0),
    )?;
    let disputes: i64 = conn.query_row(
        "SELECT COUNT(*) FROM fact_observation
         WHERE fact_id = ?1 AND kind IN ('disputed','corrected')",
        params![fact_id],
        |r| r.get(0),
    )?;
    let user_verified: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM fact_observation
         WHERE fact_id = ?1 AND kind = 'verified' AND method = 'user'",
        params![fact_id],
        |r| r.get(0),
    )?;

    let (alpha, beta) = if user_verified {
        USER_VERIFIED_PRIOR
    } else {
        let (extractor, predicate): (Option<String>, String) = conn.query_row(
            "SELECT extractor, predicate FROM fact WHERE id = ?1",
            params![fact_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?;
        match class_prior(conn, extractor.as_deref().unwrap_or("?"), &predicate)? {
            Some(prior) => prior,
            None => {
                // Founding declared confidence, else uninformative.
                let c0: Option<f64> = conn
                    .query_row(
                        "SELECT confidence FROM fact_observation
                         WHERE fact_id = ?1 AND kind = 'asserted'
                         ORDER BY id LIMIT 1",
                        params![fact_id],
                        |r| r.get(0),
                    )
                    .optional()?
                    .flatten();
                match c0 {
                    Some(c) => beta_from_rate(c),
                    None => (1.0, 1.0),
                }
            }
        }
    };

    let conf = (alpha + supports as f64) / (alpha + beta + supports as f64 + disputes as f64);
    conn.execute(
        "UPDATE fact SET confidence = ?2 WHERE id = ?1",
        params![fact_id, conf],
    )?;
    Ok(conf)
}

/// Supersede a live fact: sets both valid time end and system-time
/// invalidation. History remains queryable via [`timeline`] / [`facts_as_of`].
/// Close a belief's VALID time without invalidating it: it stopped being
/// true in the world, and we were not wrong to have believed it.
///
/// The distinction §4.3 declares but [`supersede_fact`] collapses (it
/// sets both timestamps, i.e. "false now AND we erred"). Decay of a
/// recomputable derivation is the pure valid-time case: the linker
/// derived a real pattern that later dissolved. Keeping
/// `invalidated_at` NULL means `facts_as_of` still answers correctly for
/// the period it held, and no class gets blamed for being right at the
/// time.
pub fn close_valid_time(conn: &Connection, uid: &str, valid_to: Option<&str>) -> Result<()> {
    let n = conn.execute(
        "UPDATE fact SET valid_to = COALESCE(?2, ?3)
         WHERE uid = ?1 AND valid_to IS NULL AND invalidated_at IS NULL",
        params![uid, valid_to, now()],
    )?;
    if n == 0 {
        return Err(Error::Other(format!("no live fact with uid {uid}")));
    }
    Ok(())
}

/// Retract a belief that was **never true** — as distinct from
/// [`close_valid_time`] (true then, false now) and [`supersede_fact`]
/// (replaced by a better value).
///
/// Sets `invalidated_at` (we were wrong, and when we learned it) AND
/// collapses valid time to a **zero-length window** — BOTH endpoints at
/// `COALESCE(valid_from, ingested_at)`. Both matter because
/// [`facts_as_of`] filters on valid time alone and treats a NULL
/// `valid_from` as "always been true": pinning only `valid_to` would
/// leave a phantom answering every as-of date before it, which is
/// exactly the claim being retracted. With both pinned, `valid_from <=
/// t` and `valid_to > t` cannot hold for any `t`, so no as-of date
/// serves it — while `timeline` still shows the whole episode of having
/// believed it.
pub fn invalidate_never_true(conn: &Connection, uid: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE fact
         SET invalidated_at = ?2,
             valid_from = COALESCE(valid_from, ingested_at),
             valid_to   = COALESCE(valid_from, ingested_at)
         WHERE uid = ?1 AND invalidated_at IS NULL",
        params![uid, now()],
    )?;
    if n == 0 {
        return Err(Error::Other(format!("no live fact with uid {uid}")));
    }
    Ok(())
}

/// Has a human personally vouched for this fact? Human judgment outranks
/// any recomputation, so automated sweeps must leave these alone.
pub fn is_user_verified(conn: &Connection, fact_id: i64) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) > 0 FROM fact_observation
         WHERE fact_id = ?1 AND kind = 'verified' AND method = 'user'",
        params![fact_id],
        |r| r.get(0),
    )?)
}

pub fn supersede_fact(conn: &Connection, uid: &str, valid_to: Option<&str>) -> Result<()> {
    let ts = now();
    let n = conn.execute(
        "UPDATE fact SET valid_to = COALESCE(?2, ?3), invalidated_at = ?3
         WHERE uid = ?1 AND invalidated_at IS NULL",
        params![uid, valid_to, ts],
    )?;
    if n == 0 {
        return Err(Error::Other(format!("no live fact with uid {uid}")));
    }
    Ok(())
}

pub fn get_fact_by_uid(conn: &Connection, uid: &str) -> Result<Option<Fact>> {
    Ok(conn
        .query_row(
            "SELECT * FROM fact WHERE uid = ?1",
            params![uid],
            row_to_fact,
        )
        .optional()?)
}

/// Current facts for a subject (both directions).
/// Live beliefs about a node, **both polarities** — this is a display
/// path, not graph traversal.
///
/// It deliberately does NOT use `fact_current`, which V013 made
/// positive-only so that edges/linkers/GTD/stats mean "current positive
/// beliefs" by it (a negative edge in traversal would be a bug). But a
/// live negation is knowledge: rejection memory exists to stop the
/// system re-asking, and every surface that reads this function
/// — kg_entity, context packs, node summaries, the TUI entity page — is
/// exactly where an agent or the owner decides whether to ask again.
/// Hiding denials there meant the memory could never do its job (found
/// 2026-08-13, alongside `hybrid_facts`, which serves negations for the
/// same reason). Callers that mean "positive beliefs" must say so:
/// filter on `polarity`, or read `fact_current` directly.
pub fn facts_for_node(conn: &Connection, node_id: &str, limit: i64) -> Result<Vec<Fact>> {
    let mut stmt = conn.prepare_cached(
        "SELECT * FROM fact
         WHERE (subject_id = ?1 OR object_id = ?1)
           AND valid_to IS NULL AND invalidated_at IS NULL
         ORDER BY weight DESC, observation_count DESC, ingested_at DESC LIMIT ?2",
    )?;
    let facts = stmt
        .query_map(params![node_id, limit], row_to_fact)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(facts)
}

/// Bi-temporal point query: what was true (VALID time) about a node at `as_of`.
/// Deliberately does NOT filter on `invalidated_at` — supersession sets both
/// timelines, and a superseded fact was still true during its validity window.
/// ("What did I believe on date D" is the separate system-time query.)
pub fn facts_as_of(conn: &Connection, node_id: &str, as_of: &str, limit: i64) -> Result<Vec<Fact>> {
    let mut stmt = conn.prepare_cached(
        "SELECT * FROM fact
         WHERE (subject_id = ?1 OR object_id = ?1)
           AND (valid_from IS NULL OR valid_from <= ?2)
           AND (valid_to IS NULL OR valid_to > ?2)
         ORDER BY ingested_at DESC LIMIT ?3",
    )?;
    let facts = stmt
        .query_map(params![node_id, as_of, limit], row_to_fact)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(facts)
}

/// Full history for a node, including superseded facts (kg_timeline).
pub fn timeline(
    conn: &Connection,
    node_id: &str,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Vec<Fact>> {
    let mut stmt = conn.prepare_cached(
        "SELECT * FROM fact
         WHERE (subject_id = ?1 OR object_id = ?1)
           AND (?2 IS NULL OR COALESCE(valid_from, ingested_at) >= ?2)
           AND (?3 IS NULL OR COALESCE(valid_from, ingested_at) <= ?3)
         ORDER BY COALESCE(valid_from, ingested_at) ASC",
    )?;
    let facts = stmt
        .query_map(params![node_id, from, to], row_to_fact)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(facts)
}

/// Live contradictions: >1 live fact on the same (subject, predicate) with
/// different objects — but ONLY for predicates that are semantically
/// single-valued. `related_to`/`attended`/`works_on` are multi-valued by
/// nature; flagging them buried the real signal under hundreds of false
/// alarms. Detection query for §11.5; supersession stays a human/agent call.
pub fn live_contradictions(conn: &Connection) -> Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT subject_id, predicate, COUNT(*) c FROM fact_current
         WHERE object_id IS NOT NULL
           AND predicate IN ('works_at', 'located_in', 'assigned_to', 'pursued_via', 'has_role')
         GROUP BY subject_id, predicate
         HAVING COUNT(DISTINCT object_id) > 1",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

// ─── Fact candidates: staging before promotion (§4.3) ────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FactCandidate {
    pub id: i64,
    pub payload: serde_json::Value,
    pub status: String,
    pub proposed_by: Option<String>,
    pub episode_id: Option<i64>,
    pub confidence: Option<f64>,
    pub created_at: String,
    pub reviewed_at: Option<String>,
    pub reject_reason: Option<String>,
}

/// Proposed-fact payload shape (also the `kg_upsert` wire format).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProposedFact {
    pub subject: String,
    pub predicate: String,
    #[serde(default)]
    pub object: Option<String>,
    #[serde(default)]
    pub object_value: Option<String>,
    pub statement: String,
    #[serde(default)]
    pub valid_from: Option<String>,
    #[serde(default)]
    pub confidence: Option<f64>,
    /// Comma-separated labels, e.g. "recommendation,software" — the revisit
    /// handle (`pkg facts --tag recommendation`).
    #[serde(default)]
    pub tags: Option<String>,
}

/// Stage a proposed fact. The sole write path for anything non-deterministic
/// (LLM extraction, agent kg_upsert, commitment detection).
///
/// The predicate is canonicalized here, at staging, through the read-only
/// alias lookup — the queue clusters on the payload predicate, so raw
/// extractor spellings (`working_on`, `was used for`) otherwise mint their
/// own tiny classes with no history. Unknown predicates keep their spelling
/// (folded to lowercase_underscore) and are never registered; the
/// vocabulary only grows at accept time.
pub fn propose_fact(
    conn: &Connection,
    proposed: &ProposedFact,
    proposed_by: &str,
    episode_id: Option<i64>,
) -> Result<i64> {
    let mut proposed = proposed.clone();
    if !proposed.predicate.trim().is_empty() {
        proposed.predicate = resolve_predicate(conn, &proposed.predicate)?;
    }
    let proposed = &proposed;
    conn.execute(
        "INSERT INTO fact_candidate (payload, proposed_by, episode_id, confidence)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            serde_json::to_string(proposed)?,
            proposed_by,
            episode_id,
            proposed.confidence.unwrap_or(0.7)
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

fn row_to_candidate(row: &rusqlite::Row) -> std::result::Result<FactCandidate, rusqlite::Error> {
    let payload_str: String = row.get("payload")?;
    Ok(FactCandidate {
        id: row.get("id")?,
        payload: serde_json::from_str(&payload_str).unwrap_or(serde_json::Value::Null),
        status: row.get("status")?,
        proposed_by: row.get("proposed_by")?,
        episode_id: row.get("episode_id")?,
        confidence: row.get("confidence")?,
        created_at: row.get("created_at")?,
        reviewed_at: row.get("reviewed_at")?,
        reject_reason: row.get("reject_reason")?,
    })
}

pub fn pending_candidates(conn: &Connection, limit: i64) -> Result<Vec<FactCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM fact_candidate WHERE status = 'proposed'
         ORDER BY confidence DESC, created_at DESC LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], row_to_candidate)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Pending candidates narrowed to one review class.
///
/// The queue is not one problem. Clustered by (proposer, predicate) it
/// splits into classes with wildly different histories — `llm·uses` runs 78%
/// accepted while `llm·has_role` runs 2% — and an agent sent to work on it
/// needs a class, not the top of an undifferentiated 3,634-deep list. The
/// oldest first, because a candidate that has waited longest has had the
/// most chance for corroborating evidence to arrive since.
/// `unjudged_by`: skip candidates this mechanism has already filed a
/// verdict on. Without it a batch mechanism re-serves the same oldest N on
/// every run — `record_verdict` keeps history rather than upserting, so
/// re-judging duplicates opinions instead of extending coverage.
pub fn pending_in_class(
    conn: &Connection,
    proposed_by: &str,
    predicate: &str,
    limit: i64,
    unjudged_by: Option<&str>,
) -> Result<Vec<FactCandidate>> {
    let mut stmt = conn.prepare(
        "SELECT * FROM fact_candidate
         WHERE status = 'proposed'
           AND proposed_by = ?1
           AND json_extract(payload, '$.predicate') = ?2
           AND (?4 IS NULL OR NOT EXISTS (
                SELECT 1 FROM agent_verdict v
                WHERE v.candidate_id = fact_candidate.id AND v.mechanism = ?4))
         ORDER BY created_at ASC LIMIT ?3",
    )?;
    let rows = stmt
        .query_map(
            params![proposed_by, predicate, limit, unjudged_by],
            row_to_candidate,
        )?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Pending candidates that mention an entity, across every class.
///
/// The class-at-a-time discipline of [`pending_in_class`] is right for the
/// queue's own workflow — a class shares a history, so it earns one decision.
/// This is the other axis, and it exists for a reader that has just spent
/// real effort understanding ONE entity: gossip finishes a probe holding two
/// vantages, three rounds of dialogue and the cited evidence, and that
/// context is worth more against the eleven pending claims about that person
/// than against the next eleven items of one predicate.
///
/// Matching is textual against the resolved surface forms rather than a join
/// on `subject_id`, because the subject a candidate was staged with is a
/// snapshot: 189 of 200 bee candidates carry an empty subject whose statement
/// names someone the graph resolves cleanly today. Matching the alias set
/// finds those; a join would not.
///
/// `unjudged_by` behaves exactly as it does for a class, so a nightly probe
/// extends coverage instead of re-judging what it judged last week.
pub fn pending_about_entity(
    conn: &Connection,
    surfaces: &[String],
    limit: i64,
    unjudged_by: Option<&str>,
) -> Result<Vec<FactCandidate>> {
    // No surfaces would otherwise build `WHERE ... AND ()`, and an entity
    // with no name is not a question worth answering.
    let surfaces: Vec<String> = surfaces
        .iter()
        .map(|s| s.trim().to_lowercase())
        .filter(|s| s.len() >= 3) // "Mi" would match half the graph
        .collect();
    if surfaces.is_empty() {
        return Ok(Vec::new());
    }

    // One OR-group of LIKEs per surface form, all parameterised — an alias
    // is user data and has no business being concatenated into SQL.
    let ors: Vec<String> = (0..surfaces.len())
        .map(|i| {
            let p = i + 4; // ?1..?3 are proposer-independent args below
            format!(
                "LOWER(COALESCE(json_extract(payload,'$.statement'),'')) LIKE ?{p} \
                 OR LOWER(COALESCE(json_extract(payload,'$.subject'),'')) LIKE ?{p} \
                 OR LOWER(COALESCE(json_extract(payload,'$.object'),'')) LIKE ?{p}"
            )
        })
        .collect();
    let sql = format!(
        "SELECT * FROM fact_candidate
         WHERE status = 'proposed'
           AND (?2 IS NULL OR NOT EXISTS (
                SELECT 1 FROM agent_verdict v
                WHERE v.candidate_id = fact_candidate.id AND v.mechanism = ?2))
           AND ({})
         ORDER BY created_at ASC LIMIT ?3",
        ors.join(" OR ")
    );

    let mut binds: Vec<Box<dyn rusqlite::ToSql>> = vec![
        Box::new(Option::<String>::None), // ?1 unused, keeps numbering readable
        Box::new(unjudged_by.map(str::to_string)),
        Box::new(limit),
    ];
    for s in &surfaces {
        binds.push(Box::new(format!("%{s}%")));
    }
    let refs: Vec<&dyn rusqlite::ToSql> = binds.iter().map(|b| b.as_ref()).collect();

    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<FactCandidate> = stmt
        .query_map(refs.as_slice(), row_to_candidate)?
        .collect::<std::result::Result<_, _>>()?;

    // SQL LIKE is the prefilter; the word boundary is the filter. `%ana%`
    // matches "Anastasia", so a probe of Ana Sorel would be handed
    // "Anastasia secured her fellowship funding" to adjudicate. SQLite has
    // no regex, so the boundary check happens here, over the rows LIKE
    // already narrowed to.
    Ok(rows
        .into_iter()
        .filter(|c| {
            let hay = format!(
                "{} {} {}",
                c.payload["statement"].as_str().unwrap_or(""),
                c.payload["subject"].as_str().unwrap_or(""),
                c.payload["object"].as_str().unwrap_or("")
            )
            .to_lowercase();
            surfaces.iter().any(|s| contains_word(&hay, s))
        })
        .collect())
}

/// Does `needle` appear in `haystack` bounded by non-alphanumerics?
///
/// Both are already lowercased. Bounds are checked in bytes because the
/// candidates are surface forms of names: an ASCII-alphanumeric neighbour
/// means the match ran into a longer word ("ana" inside "Anastasia"), and
/// anything else — space, punctuation, an accented letter starting a
/// different word, end of string — is a boundary.
fn contains_word(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return false;
    }
    let (h, n) = (haystack.as_bytes(), needle.as_bytes());
    let mut from = 0;
    while let Some(rel) = haystack[from..].find(needle) {
        let start = from + rel;
        let end = start + n.len();
        let before_ok = start == 0 || !h[start - 1].is_ascii_alphanumeric();
        let after_ok = end == h.len() || !h[end].is_ascii_alphanumeric();
        if before_ok && after_ok {
            return true;
        }
        from = start + 1;
    }
    false
}

/// The source an episode came from, for a candidate's originating episode.
///
/// Corroboration needs this to EXCLUDE it. A reader that can see the
/// episode a claim was extracted from will find the claim there and call it
/// corroborated, which is the same witness twice — the exact failure the
/// facts-versus-evidence split had, rediscovered one level up.
pub fn candidate_origin_source(conn: &Connection, candidate_id: i64) -> Result<Option<String>> {
    let src = conn
        .query_row(
            "SELECT e.source FROM fact_candidate c
             JOIN episode e ON e.id = c.episode_id
             WHERE c.id = ?1",
            params![candidate_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(src)
}

/// Record what an agent mechanism concluded about a candidate.
///
/// Deliberately not a decision: `fact_candidate.status` is untouched. A
/// mechanism must be able to be wrong in public long enough to be measured,
/// and this is the store that makes the measuring possible.
pub fn record_verdict(
    conn: &Connection,
    candidate_id: i64,
    mechanism: &str,
    verdict: &str,
    basis: &str,
    model: Option<&str>,
) -> Result<i64> {
    conn.execute(
        "INSERT INTO agent_verdict (candidate_id, mechanism, verdict, basis, model)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![candidate_id, mechanism, verdict, basis, model],
    )?;
    Ok(conn.last_insert_rowid())
}

/// How one mechanism's verdicts have fared against the owner's decisions.
///
/// `(verdict, outcome, n)`. Rows with a NULL outcome are the not-yet-scored
/// backlog and are returned as `"pending"` rather than dropped: a mechanism
/// looks perfect if you only count the verdicts somebody got round to
/// checking, and how much is unscored is the first thing to know about a
/// precision figure.
pub fn verdict_scorecard(conn: &Connection, mechanism: &str) -> Result<Vec<(String, String, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT verdict, COALESCE(outcome, 'pending') AS outcome, COUNT(*)
         FROM agent_verdict WHERE mechanism = ?1
         GROUP BY verdict, outcome ORDER BY verdict, outcome",
    )?;
    let rows = stmt
        .query_map(params![mechanism], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Score every outstanding verdict on a candidate against what was decided.
///
/// Called from the accept/reject paths, so the ledger fills as a side
/// effect of ordinary review rather than needing a separate scoring chore
/// nobody would run.
pub fn score_verdicts(conn: &Connection, candidate_id: i64, outcome: &str) -> Result<usize> {
    let n = conn.execute(
        "UPDATE agent_verdict SET outcome = ?2 WHERE candidate_id = ?1 AND outcome IS NULL",
        params![candidate_id, outcome],
    )?;
    Ok(n)
}

/// Replace a pending candidate's payload (human edit before acceptance).
pub fn update_candidate_payload(
    conn: &Connection,
    candidate_id: i64,
    payload: &serde_json::Value,
) -> Result<()> {
    let n = conn.execute(
        "UPDATE fact_candidate SET payload = ?2 WHERE id = ?1 AND status = 'proposed'",
        params![candidate_id, payload.to_string()],
    )?;
    if n == 0 {
        return Err(Error::Other(format!("no pending candidate {candidate_id}")));
    }
    Ok(())
}

/// Accept a candidate — see [`accept_candidate_opts`]. Fails (leaving the
/// candidate pending) if the subject cannot be resolved.
pub fn accept_candidate(conn: &Connection, candidate_id: i64) -> Result<String> {
    accept_candidate_opts(conn, candidate_id, false, true)
}

/// Accept a candidate: resolve subject/object names against the graph and
/// promote to a real fact. When `create_missing_subject` is set, an
/// unresolvable subject becomes a new topic node — the human accepting IS
/// the high bar the LLM alone doesn't clear (§4.2).
///
/// `reviewed_by_user`: true for the human review surfaces (CLI accept,
/// TUI) — records a verified/user observation, which is the strong
/// post-selection prior in the confidence posterior. Precheck's
/// auto-accept lane passes false: nothing human looked at THAT fact.
pub fn accept_candidate_opts(
    conn: &Connection,
    candidate_id: i64,
    create_missing_subject: bool,
    reviewed_by_user: bool,
) -> Result<String> {
    let cand = conn
        .query_row(
            "SELECT * FROM fact_candidate WHERE id = ?1 AND status = 'proposed'",
            params![candidate_id],
            row_to_candidate,
        )
        .optional()?
        .ok_or_else(|| Error::Other(format!("no pending candidate {candidate_id}")))?;

    let proposed: ProposedFact = serde_json::from_value(cand.payload.clone())?;

    if proposed.subject.trim().is_empty() {
        return Err(Error::Other(format!(
            "candidate {candidate_id} has no subject — bind one (review `b`/edit) before accepting"
        )));
    }
    let subject = match crate::graph::resolve_entity(conn, &proposed.subject)? {
        Some(node) => node,
        None if create_missing_subject && !proposed.subject.trim().is_empty() => {
            let id = format!("topic-{}", crate::ids::new_uid());
            let mut node = crate::graph::Node::new(&id, "topic", proposed.subject.trim());
            node.source = "review".into();
            crate::graph::upsert_node(conn, &node)?;
            crate::graph::get_node(conn, &id)?.expect("just created")
        }
        None => {
            return Err(Error::Other(format!(
                "cannot resolve subject '{}'",
                proposed.subject
            )))
        }
    };
    let object_node = match &proposed.object {
        Some(o) => crate::graph::resolve_entity(conn, o)?,
        None => None,
    };
    // If an object was named but didn't resolve to a node, keep it as a literal.
    let object_value = match (&proposed.object, &object_node) {
        (Some(o), None) => Some(o.clone()),
        _ => proposed.object_value.clone(),
    };

    let uid = assert_fact(
        conn,
        &subject.id,
        &proposed.predicate,
        object_node.as_ref().map(|n| n.id.as_str()),
        object_value.as_deref(),
        &proposed.statement,
        cand.episode_id,
        proposed.valid_from.as_deref(),
        proposed.confidence.unwrap_or(0.7),
        cand.proposed_by.as_deref().unwrap_or("candidate"),
    )?;
    if let Some(tags) = proposed.tags.as_deref().filter(|t| !t.trim().is_empty()) {
        conn.execute(
            "UPDATE fact SET tags = ?2 WHERE uid = ?1",
            params![uid, tags],
        )?;
    }
    if reviewed_by_user {
        let fact_id: i64 =
            conn.query_row("SELECT id FROM fact WHERE uid = ?1", params![uid], |r| {
                r.get(0)
            })?;
        record_observation(conn, fact_id, cand.episode_id, "verified", "user", None)?;
        recompute_confidence(conn, fact_id)?;
        // Human verdicts move the autonomy ladder (auto-lanes must not).
        let (key, commitment) = crate::precheck::cluster_key(&cand.payload);
        crate::ladder::note_verdict(
            conn,
            cand.proposed_by.as_deref().unwrap_or("?"),
            &key,
            true,
            commitment,
        )?;
    }

    conn.execute(
        "UPDATE fact_candidate SET status = 'accepted', reviewed_at = datetime('now') WHERE id = ?1",
        params![candidate_id],
    )?;
    // Score any agent verdicts as a side effect of ordinary review. A
    // separate scoring chore is one nobody runs, and an unscored ledger
    // measures nothing. Machine lanes score under a distinct label: a
    // precheck dedup-reject is not the owner's judgement, and folding the
    // two together would let the scorecard claim human ground truth it
    // never had.
    score_verdicts(
        conn,
        candidate_id,
        if reviewed_by_user {
            "accepted"
        } else {
            "accepted:auto"
        },
    )?;
    Ok(uid)
}

/// Live facts carrying a tag (comma-separated matching) — the revisit list.
pub fn facts_by_tag(conn: &Connection, tag: &str, limit: i64) -> Result<Vec<Fact>> {
    let pattern = format!("%{}%", tag.trim().to_lowercase());
    let mut stmt = conn.prepare_cached(
        "SELECT * FROM fact_current WHERE LOWER(COALESCE(tags,'')) LIKE ?1
         ORDER BY ingested_at DESC LIMIT ?2",
    )?;
    let facts = stmt
        .query_map(params![pattern, limit], row_to_fact)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(facts)
}

/// Reject a candidate. `reason` feeds Reflexion/LEAP-style feedback (§6).
/// Human-review reject — see [`reject_candidate_opts`].
pub fn reject_candidate(conn: &Connection, candidate_id: i64, reason: &str) -> Result<()> {
    reject_candidate_opts(conn, candidate_id, reason, true)
}

/// Reject a pending candidate. `by_user`: true for human surfaces —
/// resets the class's ladder streak; precheck's machine rejects pass
/// false (a lane must not move the ladder it feeds).
pub fn reject_candidate_opts(
    conn: &Connection,
    candidate_id: i64,
    reason: &str,
    by_user: bool,
) -> Result<()> {
    let cand: Option<(Option<String>, String)> = conn
        .query_row(
            "SELECT proposed_by, payload FROM fact_candidate
             WHERE id = ?1 AND status = 'proposed'",
            params![candidate_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((proposed_by, payload)) = cand else {
        return Err(Error::Other(format!("no pending candidate {candidate_id}")));
    };
    conn.execute(
        "UPDATE fact_candidate SET status = 'rejected', reviewed_at = datetime('now'),
                reject_reason = ?2
         WHERE id = ?1",
        params![candidate_id, reason],
    )?;
    score_verdicts(
        conn,
        candidate_id,
        if by_user { "rejected" } else { "rejected:auto" },
    )?;
    if by_user {
        let payload: serde_json::Value = serde_json::from_str(&payload).unwrap_or_default();
        let (key, commitment) = crate::precheck::cluster_key(&payload);
        crate::ladder::note_verdict(
            conn,
            proposed_by.as_deref().unwrap_or("?"),
            &key,
            false,
            commitment,
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The entity axis crosses classes, and finds candidates whose staged
    /// subject never named the entity at all.
    ///
    /// That second half is the point: a candidate's `subject` is a snapshot
    /// of what resolved on the day it was staged, and most bee candidates
    /// carry an empty one while their statement names someone the graph
    /// resolves cleanly today. Matching the alias set finds those; a join on
    /// subject_id would silently miss exactly the candidates most in need of
    /// a second look.
    #[test]
    fn pending_about_an_entity_crosses_classes_and_matches_the_statement() {
        let conn = crate::db::open_memory().unwrap();
        let mk = |subject: &str, predicate: &str, statement: &str, proposer: &str| {
            let p = ProposedFact {
                subject: subject.into(),
                predicate: predicate.into(),
                object: None,
                object_value: None,
                statement: statement.into(),
                valid_from: None,
                confidence: Some(0.8),
                tags: None,
            };
            propose_fact(&conn, &p, proposer, None).unwrap()
        };
        // Two different classes, both naming her in the subject.
        mk("Nadia", "works_on", "Nadia works on Hypercourse.", "llm");
        mk("Nadia", "attended", "Nadia attended the retreat.", "llm");
        // Subject empty — the bee shape — but the statement names her.
        mk(
            "",
            "related_to",
            "Nadia is considering PhD programs.",
            "bee:suggested",
        );
        // Someone else entirely.
        mk("Iris", "works_on", "Iris works on sigtools.", "llm");

        let found = pending_about_entity(&conn, &["nadia".into()], 50, None).unwrap();
        assert_eq!(found.len(), 3, "three mention her, across two proposers");
        assert!(
            found.iter().any(|c| c.payload["subject"] == ""),
            "the empty-subject candidate must be found by its statement"
        );
        assert!(
            !found
                .iter()
                .any(|c| c.payload["statement"].as_str().unwrap().contains("sigtools")),
            "someone else's candidate must not come back"
        );

        // A surface form too short to be safe is refused rather than
        // matching half the graph.
        assert!(pending_about_entity(&conn, &["Na".into()], 50, None)
            .unwrap()
            .is_empty());
        assert!(pending_about_entity(&conn, &[], 50, None)
            .unwrap()
            .is_empty());
    }

    /// A name must match a WORD, not a prefix.
    ///
    /// The failure shape: a probe of Ana Sorel gets handed "Anastasia
    /// secured her fellowship funding" to adjudicate, because SQL LIKE
    /// '%ana%' is happy inside "Anastasia". Wasting a model call is the
    /// mild cost; filing a verdict about the wrong person against a real
    /// candidate is the actual one.
    #[test]
    fn an_entity_surface_matches_a_word_not_a_prefix() {
        let conn = crate::db::open_memory().unwrap();
        let mk = |statement: &str| {
            let p = ProposedFact {
                subject: "".into(),
                predicate: "related_to".into(),
                object: None,
                object_value: None,
                statement: statement.into(),
                valid_from: None,
                confidence: Some(0.8),
                tags: None,
            };
            propose_fact(&conn, &p, "llm", None).unwrap()
        };
        mk("Anastasia secured her fellowship funding.");
        mk("Ada mentored Ana after her departure.");
        mk("Ana's plot needs cleaner axes.");

        let found = pending_about_entity(&conn, &["ana".into()], 50, None).unwrap();
        let texts: Vec<String> = found
            .iter()
            .map(|c| c.payload["statement"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(found.len(), 2, "Anastasia is not Ana: {texts:?}");
        assert!(texts.iter().all(|t| !t.contains("Anastasia")));
        assert!(texts.iter().any(|t| t.contains("Ana's")));
    }

    #[test]
    fn word_boundaries_are_checked_on_both_sides() {
        assert!(contains_word("ada mentored ana after", "ana"));
        assert!(contains_word("ana's plot", "ana"));
        assert!(contains_word("met ana.", "ana"));
        assert!(contains_word("ana", "ana"));
        assert!(!contains_word("anastasia secured", "ana"));
        assert!(!contains_word("lana is a model", "ana"));
        assert!(!contains_word("anything", ""));
    }

    /// `unjudged_by` extends coverage on this axis too, so a nightly probe
    /// of the same person does not re-judge what it judged last week.
    #[test]
    fn pending_about_an_entity_skips_what_the_mechanism_already_judged() {
        let conn = crate::db::open_memory().unwrap();
        let p = ProposedFact {
            subject: "Nadia".into(),
            predicate: "works_on".into(),
            object: None,
            object_value: None,
            statement: "Nadia works on Hypercourse.".into(),
            valid_from: None,
            confidence: Some(0.8),
            tags: None,
        };
        let id = propose_fact(&conn, &p, "llm", None).unwrap();
        assert_eq!(
            pending_about_entity(&conn, &["nadia".into()], 50, Some("gossip"))
                .unwrap()
                .len(),
            1
        );
        record_verdict(&conn, id, "gossip", "supported", "the probe saw it", None).unwrap();
        assert!(
            pending_about_entity(&conn, &["nadia".into()], 50, Some("gossip"))
                .unwrap()
                .is_empty(),
            "already judged by this mechanism"
        );
        assert_eq!(
            pending_about_entity(&conn, &["nadia".into()], 50, Some("verification"))
                .unwrap()
                .len(),
            1,
            "a different mechanism still sees it"
        );
    }

    #[test]
    fn accepting_a_candidate_with_no_subject_is_an_error() {
        // Regression: an empty subject used to slip through resolve_entity's
        // fuzzy tier and bind the fact to an arbitrary node.
        let conn = crate::db::open_memory().unwrap();
        let p = ProposedFact {
            subject: "".into(),
            predicate: "related_to".into(),
            object: None,
            object_value: None,
            statement: "Something about nobody in particular.".into(),
            valid_from: None,
            confidence: Some(0.5),
            tags: None,
        };
        let cid = propose_fact(&conn, &p, "bee:suggested", None).unwrap();
        let err = accept_candidate_opts(&conn, cid, true, true).unwrap_err();
        assert!(
            err.to_string().contains("no subject"),
            "error names the missing subject, got: {err}"
        );
        let still: String = conn
            .query_row(
                "SELECT status FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            still, "proposed",
            "a refused accept leaves the candidate pending"
        );
    }

    #[test]
    fn a_morphological_variant_folds_in_and_learns_its_alias() {
        let conn = crate::db::open_memory().unwrap();
        // Stem rung: unknown predicate, unique stem match to vocabulary.
        assert_eq!(
            resolve_predicate(&conn, "collaborating with").unwrap(),
            "collaborates_with"
        );
        // The match is LEARNED as an alias — so accept-time
        // normalize_predicate maps it too, instead of registering a new
        // predicate. Aliases map into the closed set; they never grow it.
        let learned: String = conn
            .query_row(
                "SELECT name FROM predicate_alias WHERE alias = 'collaborating_with'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(learned, "collaborates_with");
        assert_eq!(resolve_predicate(&conn, "attends").unwrap(), "attended");
        // No unique stem match → unchanged, and NOT registered.
        assert_eq!(
            resolve_predicate(&conn, "is_traveling_to").unwrap(),
            "is_traveling_to"
        );
        let registered: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM predicate WHERE name = 'is_traveling_to'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!registered);
    }

    #[test]
    fn staging_canonicalizes_the_predicate_without_registering() {
        let conn = crate::db::open_memory().unwrap();
        let p = ProposedFact {
            subject: "Alice".into(),
            predicate: "advises".into(), // seeded alias of `mentors`
            object: Some("Bob".into()),
            object_value: None,
            statement: "Alice advises Bob.".into(),
            valid_from: None,
            confidence: Some(0.8),
            tags: None,
        };
        let cid = propose_fact(&conn, &p, "llm", None).unwrap();
        let payload: String = conn
            .query_row(
                "SELECT payload FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(v["predicate"], "mentors");

        // Unknown predicates fold their spelling but never grow the
        // vocabulary at staging.
        let p2 = ProposedFact {
            predicate: "is traveling to".into(),
            statement: "Alice is traveling to Boston.".into(),
            ..p
        };
        let cid2 = propose_fact(&conn, &p2, "llm", None).unwrap();
        let payload2: String = conn
            .query_row(
                "SELECT payload FROM fact_candidate WHERE id = ?1",
                params![cid2],
                |r| r.get(0),
            )
            .unwrap();
        let v2: serde_json::Value = serde_json::from_str(&payload2).unwrap();
        assert_eq!(v2["predicate"], "is_traveling_to");
        let registered: bool = conn
            .query_row(
                "SELECT COUNT(*) > 0 FROM predicate WHERE name = 'is_traveling_to'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!registered, "staging must not register vocabulary");
    }

    #[test]
    fn a_verdict_is_an_opinion_until_review_scores_it() {
        let conn = crate::db::open_memory().unwrap();
        let p = ProposedFact {
            subject: "Ada".into(),
            predicate: "related_to".into(),
            object: Some("DIY".into()),
            object_value: None,
            statement: "Ada prefers DIY approaches.".into(),
            valid_from: None,
            confidence: Some(0.5),
            tags: None,
        };
        let cid = propose_fact(&conn, &p, "bee:suggested", None).unwrap();
        record_verdict(
            &conn,
            cid,
            "corroboration",
            "single_source",
            "only bee shows it",
            Some("m"),
        )
        .unwrap();

        // Recording a verdict must not decide anything.
        let still: String = conn
            .query_row(
                "SELECT status FROM fact_candidate WHERE id = ?1",
                params![cid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still, "proposed", "a verdict is not a decision");

        // Unscored verdicts are reported as pending, never dropped: a
        // mechanism looks perfect if you only count what was checked.
        let card = verdict_scorecard(&conn, "corroboration").unwrap();
        assert_eq!(
            card,
            vec![("single_source".to_string(), "pending".to_string(), 1)]
        );

        // Review scores it, as a side effect rather than a chore.
        reject_candidate(&conn, cid, "not corroborated").unwrap();
        let card = verdict_scorecard(&conn, "corroboration").unwrap();
        assert_eq!(
            card,
            vec![("single_source".to_string(), "rejected".to_string(), 1)]
        );

        // Other mechanisms are not swept up in one mechanism's scorecard.
        assert!(verdict_scorecard(&conn, "persistence").unwrap().is_empty());
    }

    #[test]
    fn a_class_is_the_unit_of_review_not_the_queue() {
        // The queue is 3,634 deep across 754 (proposer, predicate) classes
        // whose acceptance histories differ by 40x. Handing an agent the
        // top of the undifferentiated list hands it the wrong problem.
        let conn = crate::db::open_memory().unwrap();
        let stage = |proposer: &str, predicate: &str, statement: &str| {
            let p = ProposedFact {
                subject: "Ada".into(),
                predicate: predicate.into(),
                object: Some("x".into()),
                object_value: None,
                statement: statement.into(),
                valid_from: None,
                confidence: Some(0.5),
                tags: None,
            };
            propose_fact(&conn, &p, proposer, None).unwrap();
        };
        stage(
            "bee:suggested",
            "related_to",
            "Ada prefers DIY approaches.",
        );
        stage(
            "bee:suggested",
            "related_to",
            "Ada interrupts in meetings.",
        );
        stage("bee:suggested", "has_role", "Ada is a PI.");
        stage("llm", "related_to", "Ada uses SSO.");

        let got = pending_in_class(&conn, "bee:suggested", "related_to", 20, None).unwrap();
        assert_eq!(got.len(), 2, "predicate and proposer must BOTH narrow");
        assert!(got.iter().all(|c| c.payload["predicate"] == "related_to"));
        assert!(got
            .iter()
            .all(|c| c.proposed_by.as_deref() == Some("bee:suggested")));

        // The limit is a limit.
        assert_eq!(
            pending_in_class(&conn, "bee:suggested", "related_to", 1, None)
                .unwrap()
                .len(),
            1
        );
        // An empty class is empty, not an error.
        assert!(pending_in_class(&conn, "nobody", "related_to", 20, None)
            .unwrap()
            .is_empty());

        // A mechanism that names itself skips what it has already judged —
        // otherwise every batch run re-serves the same oldest N, and
        // record_verdict (which keeps history) duplicates opinions.
        record_verdict(&conn, got[0].id, "corroboration", "unseen", "b", None).unwrap();
        let rest = pending_in_class(
            &conn,
            "bee:suggested",
            "related_to",
            20,
            Some("corroboration"),
        )
        .unwrap();
        assert_eq!(rest.len(), 1, "the judged candidate is skipped");
        assert_ne!(rest[0].id, got[0].id);
        // A different mechanism still sees everything.
        assert_eq!(
            pending_in_class(
                &conn,
                "bee:suggested",
                "related_to",
                20,
                Some("persistence")
            )
            .unwrap()
            .len(),
            2
        );
    }

    use super::*;
    use crate::db::open_memory;
    use crate::graph::{upsert_node, Node};

    fn setup() -> Connection {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        conn
    }

    #[test]
    fn test_assert_and_corroborate() {
        let conn = setup();
        let uid1 = assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            None,
            Some("2026-01-01"),
            0.7,
            "manual",
        )
        .unwrap();
        // Same triple again: corroborates, does not duplicate.
        let uid2 = assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        assert_eq!(uid1, uid2);

        let facts = facts_for_node(&conn, "nadia", 10).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].observation_count, 2);
        // V011 posterior: prior (7,3) from founding declared 0.7 (no class
        // history for 'manual'), two episodeless supports → 9/12.
        assert!(
            (facts[0].confidence - 0.75).abs() < 1e-9,
            "posterior, not MAX ratchet"
        );
    }

    #[test]
    fn test_confidence_can_fall_on_dispute() {
        let conn = setup();
        let uid = assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();
        let before = get_fact_by_uid(&conn, &uid).unwrap().unwrap().confidence;

        let fact_id: i64 = conn
            .query_row("SELECT id FROM fact WHERE uid = ?1", params![uid], |r| {
                r.get(0)
            })
            .unwrap();
        record_observation(&conn, fact_id, None, "disputed", "gossip:tier1", None).unwrap();
        let after = recompute_confidence(&conn, fact_id).unwrap();

        assert!(
            after < before,
            "a dispute must lower confidence ({before} → {after})"
        );
        // Prior (9,1) from declared 0.9 clamped to 0.9? (0.9 within clamp),
        // S=1, D=1 → 10/12 vs before 10/11.
        assert!((after - 10.0 / 12.0).abs() < 1e-9);
    }

    #[test]
    fn test_class_prior_drives_unreviewed_confidence() {
        let conn = setup();
        // Seed review history: proposer 'llm' × works_on runs 10% accepted.
        let proposed = ProposedFact {
            subject: "Nadia".into(),
            predicate: "works_on".into(),
            object: Some("Aim 2".into()),
            object_value: None,
            statement: "s".into(),
            valid_from: None,
            confidence: Some(0.9),
            tags: None,
        };
        for i in 0..10 {
            let cid = propose_fact(&conn, &proposed, "llm", None).unwrap();
            conn.execute(
                "UPDATE fact_candidate SET status = ?2, reviewed_at = datetime('now')
                 WHERE id = ?1",
                params![cid, if i == 0 { "accepted" } else { "rejected" }],
            )
            .unwrap();
        }

        // An unreviewed llm fact starts at the class prior, not its
        // declared 0.9: prior (1,9) from p̂=0.1, S=1 → 2/11.
        let uid = assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        let conf = get_fact_by_uid(&conn, &uid).unwrap().unwrap().confidence;
        assert!(
            (conf - 2.0 / 11.0).abs() < 1e-9,
            "class history is the prior: {conf}"
        );
    }

    #[test]
    fn test_user_acceptance_dominates_class_prior() {
        let conn = setup();
        let proposed = ProposedFact {
            subject: "Nadia".into(),
            predicate: "works_on".into(),
            object: Some("Aim 2".into()),
            object_value: None,
            statement: "Nadia works on Aim 2".into(),
            valid_from: None,
            confidence: Some(0.9),
            tags: None,
        };
        // Same 10%-acceptance history as above.
        for i in 0..10 {
            let cid = propose_fact(&conn, &proposed, "llm", None).unwrap();
            conn.execute(
                "UPDATE fact_candidate SET status = ?2, reviewed_at = datetime('now')
                 WHERE id = ?1",
                params![cid, if i == 0 { "accepted" } else { "rejected" }],
            )
            .unwrap();
        }
        // A HUMAN-accepted candidate from that same class is post-selection:
        // verified/user prior (19,1), S=1 → 20/21, not the 2/11 class rate.
        let cid = propose_fact(&conn, &proposed, "llm", None).unwrap();
        let uid = accept_candidate(&conn, cid).unwrap();
        let conf = get_fact_by_uid(&conn, &uid).unwrap().unwrap().confidence;
        assert!(
            (conf - 20.0 / 21.0).abs() < 1e-9,
            "human verdict outranks class prior: {conf}"
        );
    }

    #[test]
    fn test_sensitivity_inherited_and_bumped_never_lowered() {
        use crate::episode::{upsert_episode, Episode};
        let conn = setup();
        let mk = |sid: &str, sens: &str| Episode {
            id: 0,
            uid: String::new(),
            source: "note".into(),
            source_id: sid.into(),
            source_ref: None,
            body: "…".into(),
            occurred_at: "2026-08-01 12:00:00".into(),
            occurred_end: None,
            ingested_at: String::new(),
            lat: None,
            lon: None,
            location: None,
            sensitivity: sens.into(),
            scope_id: None,
            meta: None,
            raw: None,
        };
        let (personal_ep, _) = upsert_episode(&conn, &mk("a", "personal")).unwrap();
        let (private_ep, _) = upsert_episode(&conn, &mk("b", "private")).unwrap();

        // Born personal (from a personal episode).
        let uid = assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            Some(personal_ep),
            None,
            0.7,
            "llm",
        )
        .unwrap();
        assert_eq!(
            get_fact_by_uid(&conn, &uid).unwrap().unwrap().sensitivity,
            "personal"
        );

        // Corroborated by a private episode: bumps to private…
        assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            Some(private_ep),
            None,
            0.7,
            "llm",
        )
        .unwrap();
        assert_eq!(
            get_fact_by_uid(&conn, &uid).unwrap().unwrap().sensitivity,
            "private"
        );

        // …and a later personal re-observation must NOT lower it back.
        assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            Some(personal_ep),
            None,
            0.7,
            "llm",
        )
        .unwrap();
        let f = get_fact_by_uid(&conn, &uid).unwrap().unwrap();
        assert_eq!(f.sensitivity, "private", "sensitivity is a ratchet upward");
        // V010: distinct-episode-only counting — personal_ep already
        // contributed the founding assertion, so its re-sighting is
        // recorded in fact_observation but does not move the counter.
        assert_eq!(f.observation_count, 2);
    }

    #[test]
    fn test_observation_trail_and_distinct_episode_counting() {
        use crate::episode::{upsert_episode, Episode};
        let conn = setup();
        let mk = |src: &str, sid: &str| Episode {
            id: 0,
            uid: String::new(),
            source: src.into(),
            source_id: sid.into(),
            source_ref: None,
            body: "…".into(),
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
        };
        let (ep1, _) = upsert_episode(&conn, &mk("note", "a")).unwrap();
        let (ep2, _) = upsert_episode(&conn, &mk("note", "b")).unwrap();
        let (agent_ep, _) = upsert_episode(&conn, &mk("agent:mecha", "s1")).unwrap();

        let assert_from = |ep: i64, extractor: &str| {
            assert_fact(
                &conn,
                "nadia",
                "works_on",
                Some("aim2"),
                None,
                "Nadia works on Aim 2",
                Some(ep),
                None,
                0.7,
                extractor,
            )
            .unwrap()
        };
        let count = |uid: &str| {
            get_fact_by_uid(&conn, uid)
                .unwrap()
                .unwrap()
                .observation_count
        };

        // Founding assertion → one 'asserted' row carrying the evidence.
        let uid = assert_from(ep1, "llm");
        let obs = observations_for_fact(&conn, &uid).unwrap();
        assert_eq!(obs.len(), 1);
        assert_eq!(
            (
                obs[0].kind.as_str(),
                obs[0].method.as_str(),
                obs[0].episode_id
            ),
            ("asserted", "llm", Some(ep1))
        );

        // Same episode again: sighting recorded, counter unmoved.
        assert_from(ep1, "llm");
        assert_eq!(count(&uid), 1, "same episode must never double-count");

        // A new distinct episode moves the counter.
        assert_from(ep2, "llm");
        assert_eq!(count(&uid), 2);

        // Agent-sourced evidence: recorded, but support does not inflate
        // (probe/agent:* is excluded from corroboration counting — PLAN.md).
        assert_from(agent_ep, "llm");
        assert_eq!(count(&uid), 2, "agent evidence must not inflate support");

        let obs = observations_for_fact(&conn, &uid).unwrap();
        assert_eq!(obs.len(), 4, "every sighting is in the trail");
        assert!(obs[1..].iter().all(|o| o.kind == "corroborated"));
    }

    #[test]
    fn test_aggregate_method_exempt_from_support_count() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-a", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("person-b", "person", "Bo")).unwrap();
        let uid = assert_fact(
            &conn,
            "person-a",
            "related_to",
            Some("person-b"),
            None,
            "Ada and Bo co-occur",
            None,
            None,
            0.5,
            "npmi",
        )
        .unwrap();
        let f = get_fact_by_uid(&conn, &uid).unwrap().unwrap();
        let before = f.confidence;

        // 40 more inputs from the same derivation. Under the old rule each
        // distinct cited episode was a support, which would drive a
        // statistical artifact toward ~0.99.
        for i in 0..40 {
            let (e, _) = crate::episode::upsert_episode(
                &conn,
                &crate::episode::Episode {
                    id: 0,
                    uid: String::new(),
                    source: "note".into(),
                    source_id: format!("agg{i}"),
                    source_ref: None,
                    body: "shared".into(),
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
            .unwrap();
            record_observation(&conn, f.id, Some(e), "corroborated", "npmi", Some(0.5)).unwrap();
        }
        let after = get_fact_by_uid(&conn, &uid).unwrap().unwrap().confidence;
        assert!(
            (after - before).abs() < 1e-9,
            "one derivation is not N corroborations: {before} → {after}"
        );

        // A real independent sighting DOES move it.
        let (e, _) = crate::episode::upsert_episode(
            &conn,
            &crate::episode::Episode {
                id: 0,
                uid: String::new(),
                source: "note".into(),
                source_id: "real".into(),
                source_ref: None,
                body: "Ada and Bo were both there".into(),
                occurred_at: "2026-03-01 10:00:00".into(),
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
        .unwrap();
        record_observation(&conn, f.id, Some(e), "corroborated", "llm", Some(0.9)).unwrap();
        recompute_confidence(&conn, f.id).unwrap();
        assert!(
            get_fact_by_uid(&conn, &uid).unwrap().unwrap().confidence > after,
            "an independent sighting still counts"
        );
    }

    #[test]
    fn test_attach_derivation_takes_max_tier_and_newest_anchor() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-a", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("person-b", "person", "Bo")).unwrap();
        let mk = |sid: &str, tier: &str, at: &str| -> i64 {
            crate::episode::upsert_episode(
                &conn,
                &crate::episode::Episode {
                    id: 0,
                    uid: String::new(),
                    source: "bee.conversation".into(),
                    source_id: sid.into(),
                    source_ref: None,
                    body: "shared".into(),
                    occurred_at: at.into(),
                    occurred_end: None,
                    ingested_at: String::new(),
                    lat: None,
                    lon: None,
                    location: None,
                    sensitivity: tier.into(),
                    scope_id: None,
                    meta: None,
                    raw: None,
                },
            )
            .unwrap()
            .0
        };
        let old_personal = mk("e1", "personal", "2020-01-01 10:00:00");
        let private_mid = mk("e2", "private", "2021-01-01 10:00:00");
        let newest = mk("e3", "personal", "2026-01-01 10:00:00");

        let uid = assert_fact(
            &conn,
            "person-a",
            "related_to",
            Some("person-b"),
            None,
            "Ada and Bo co-occur",
            None,
            None,
            0.5,
            "npmi",
        )
        .unwrap();
        assert_eq!(
            get_fact_by_uid(&conn, &uid).unwrap().unwrap().sensitivity,
            "personal",
            "an episode-less derived fact starts at the default tier"
        );

        attach_derivation(&conn, &uid, &[old_personal, private_mid, newest]).unwrap();
        let f = get_fact_by_uid(&conn, &uid).unwrap().unwrap();
        assert_eq!(
            f.sensitivity, "private",
            "MAX over the FULL contributing set — aggregation is a hop (V008)"
        );
        assert_eq!(
            f.episode_id,
            Some(newest),
            "clock anchored to the newest contributor, so λ-staleness is world-time"
        );
        let obs_ep: Option<i64> = conn
            .query_row(
                "SELECT episode_id FROM fact_observation WHERE fact_id = ?1 AND kind = 'asserted'",
                params![f.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            obs_ep,
            Some(newest),
            "the trail agrees with the fact about what it cites"
        );
    }

    #[test]
    fn test_display_paths_show_denials_traversal_does_not() {
        // Rejection memory can only stop the system re-asking if the
        // surfaces where you decide to ask can see it. facts_for_node is
        // a display path (kg_entity, context packs, summaries, TUI), so
        // it serves both polarities; fact_current stays positive-only
        // because a negative edge in traversal would be a bug.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("person-a", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("org-x", "org", "X Corp")).unwrap();
        assert_fact(
            &conn,
            "person-a",
            "works_on",
            Some("org-x"),
            None,
            "Ada works on the X Corp pilot",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        assert_negative_fact(
            &conn,
            "person-a",
            "colleague_of",
            Some("org-x"),
            None,
            "Ada is NOT a colleague of X Corp staff",
            None,
            0.95,
            "user",
        )
        .unwrap();

        let shown = facts_for_node(&conn, "person-a", 10).unwrap();
        assert_eq!(
            shown.len(),
            2,
            "a denial is visible where re-asking is decided"
        );
        assert!(shown.iter().any(|f| f.polarity == "negative"));

        // Traversal keeps meaning "current positive beliefs".
        let traversed: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_current WHERE subject_id = 'person-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(traversed, 1, "fact_current stays positive-only");
        let edges: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM edges WHERE from_id = 'person-a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(edges, 1, "no negative edge enters graph traversal");

        // A retracted denial disappears from display like any other belief.
        let neg_uid: String = conn
            .query_row(
                "SELECT uid FROM fact WHERE polarity = 'negative'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        supersede_fact(&conn, &neg_uid, None).unwrap();
        assert_eq!(facts_for_node(&conn, "person-a", 10).unwrap().len(), 1);
    }

    #[test]
    fn test_negative_facts_are_polarity_isolated() {
        let conn = setup();
        // A negation and a positive on the same triple coexist — the
        // positive assert must NOT corroborate the negation.
        let neg = assert_negative_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia does not work on Aim 2",
            None,
            0.8,
            "correction",
        )
        .unwrap();
        let pos = assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            None,
            None,
            0.8,
            "manual",
        )
        .unwrap();
        assert_ne!(neg, pos, "opposite polarity never corroborates");

        // fact_current (and therefore edges/graph traversal) sees only
        // the positive; the negation is queried explicitly.
        let current: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_current WHERE subject_id='nadia'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(current, 1, "fact_current is positive-only");
        assert_eq!(
            get_fact_by_uid(&conn, &neg).unwrap().unwrap().polarity,
            "negative"
        );

        // Re-asserting the negation corroborates the negation.
        let neg2 = assert_negative_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia does not work on Aim 2",
            None,
            0.8,
            "correction",
        )
        .unwrap();
        assert_eq!(neg, neg2);
    }

    #[test]
    fn test_v014_knowing_someone_through_their_work() {
        let conn = setup();
        let lam = |name: &str| -> Option<f64> {
            conn.query_row(
                "SELECT lambda FROM predicate WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
        };
        // A research programme is not a project: works_on decays in ~6mo,
        // which would have the sweep calling someone's field stale twice a
        // year. researches sits in the ~5y band.
        assert_eq!(lam("works_on"), Some(1.39));
        assert_eq!(lam("researches"), Some(0.14));
        // Having given a talk, and having encountered someone's work, are
        // events — they never stop having happened.
        assert_eq!(lam("presented"), Some(0.0));
        assert_eq!(lam("knows_of"), Some(0.0));

        assert_eq!(normalize_predicate(&conn, "studies").unwrap(), "researches");
        assert_eq!(
            normalize_predicate(&conn, "gave_talk").unwrap(),
            "presented"
        );
        assert_eq!(normalize_predicate(&conn, "heard_of").unwrap(), "knows_of");

        // knows_of is the weakest tie and stays auto-acceptable: the
        // NEVER_AUTO guard is about claims of social standing, and "I saw
        // them speak" is not one.
        assert!(!crate::precheck::NEVER_AUTO.contains(&"knows_of"));
        assert!(crate::precheck::NEVER_AUTO.contains(&"colleague_of"));

        // The extractor's vocabulary is built from this table, so the new
        // predicates are teachable without touching the prompt.
        let n: i64 = conn.query_row(
            "SELECT COUNT(*) FROM predicate WHERE name IN ('researches','presented','knows_of')",
            [], |r| r.get(0)).unwrap();
        assert_eq!(n, 3);
    }

    #[test]
    fn test_v013_predicates_and_lambda() {
        let conn = setup();
        // advises consolidates into mentors via alias.
        assert_eq!(normalize_predicate(&conn, "advises").unwrap(), "mentors");
        // λ spot checks: bands applied; evidence-anchored stays NULL.
        let lam = |name: &str| -> Option<f64> {
            conn.query_row(
                "SELECT lambda FROM predicate WHERE name = ?1",
                params![name],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(lam("attended"), Some(0.0));
        assert_eq!(lam("works_at"), Some(0.23));
        assert_eq!(lam("mentors"), Some(0.35));
        assert_eq!(
            lam("mentions"),
            None,
            "evidence-anchored predicates have no λ"
        );
        // Slot tables seeded.
        let (people, events): (i64, i64) = conn
            .query_row(
                "SELECT SUM(node_type='person'), SUM(node_type='event') FROM node_slot",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        // 15 person slots seeded by V013, plus 'research' from V014.
        assert_eq!((people, events), (16, 6));
    }

    #[test]
    fn test_predicate_alias_normalization() {
        let conn = setup();
        assert_fact(
            &conn,
            "nadia",
            "is_working_on",
            Some("aim2"),
            None,
            "Nadia is working on Aim 2",
            None,
            None,
            0.7,
            "llm",
        )
        .unwrap();
        let facts = facts_for_node(&conn, "nadia", 10).unwrap();
        assert_eq!(facts[0].predicate, "works_on", "alias must normalize");
    }

    #[test]
    fn test_bitemporal_supersession() {
        let conn = setup();
        upsert_node(&conn, &Node::new("aim3", "project", "Aim 3")).unwrap();

        let old = assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            None,
            Some("2026-01-01"),
            0.9,
            "manual",
        )
        .unwrap();
        supersede_fact(&conn, &old, Some("2026-06-01")).unwrap();
        assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim3"),
            None,
            "Nadia works on Aim 3",
            None,
            Some("2026-06-01"),
            0.9,
            "manual",
        )
        .unwrap();

        // Current: only Aim 3.
        let current = facts_for_node(&conn, "nadia", 10).unwrap();
        assert_eq!(current.len(), 1);
        assert_eq!(current[0].object_id.as_deref(), Some("aim3"));

        // As of March: Aim 2 was true.
        let march = facts_as_of(&conn, "nadia", "2026-03-15", 10).unwrap();
        assert_eq!(march.len(), 1);
        assert_eq!(march[0].object_id.as_deref(), Some("aim2"));

        // Timeline shows both.
        let tl = timeline(&conn, "nadia", None, None).unwrap();
        assert_eq!(tl.len(), 2);
    }

    #[test]
    fn test_edges_view_reflects_facts() {
        let conn = setup();
        assert_fact(
            &conn,
            "nadia",
            "works_on",
            Some("aim2"),
            None,
            "Nadia works on Aim 2",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();
        let edges = crate::graph::get_edges_from(&conn, "nadia").unwrap();
        assert_eq!(edges.len(), 1);
        assert_eq!(edges[0].predicate, "works_on");
        assert_eq!(edges[0].to_id, "aim2");
    }

    #[test]
    fn test_candidate_lifecycle() {
        let conn = setup();
        let proposed = ProposedFact {
            subject: "Nadia".into(),
            predicate: "works_on".into(),
            object: Some("Aim 2".into()),
            object_value: None,
            statement: "Nadia works on Aim 2".into(),
            valid_from: None,
            confidence: Some(0.8),
            tags: Some("recommendation".into()),
        };
        let cid = propose_fact(&conn, &proposed, "agent:hermes", None).unwrap();

        // Nothing lands in fact until accepted.
        assert!(facts_for_node(&conn, "nadia", 10).unwrap().is_empty());

        accept_candidate(&conn, cid).unwrap();
        let facts = facts_for_node(&conn, "nadia", 10).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(facts[0].object_id.as_deref(), Some("aim2"));
        // Tags flow through acceptance and are queryable.
        assert_eq!(facts[0].tags.as_deref(), Some("recommendation"));
        assert_eq!(facts_by_tag(&conn, "recommendation", 10).unwrap().len(), 1);

        // Rejection path.
        let cid2 = propose_fact(&conn, &proposed, "agent:hermes", None).unwrap();
        reject_candidate(&conn, cid2, "duplicate").unwrap();
        assert_eq!(pending_candidates(&conn, 10).unwrap().len(), 0);
    }

    #[test]
    fn test_contradiction_detection() {
        let conn = setup();
        upsert_node(&conn, &Node::new("aim3", "project", "Aim 3")).unwrap();
        assert_fact(
            &conn,
            "nadia",
            "works_at",
            Some("aim2"),
            None,
            "Nadia works at Aim 2",
            None,
            None,
            0.8,
            "m",
        )
        .unwrap();
        assert_fact(
            &conn,
            "nadia",
            "works_at",
            Some("aim3"),
            None,
            "Nadia works at Aim 3",
            None,
            None,
            0.8,
            "m",
        )
        .unwrap();
        let contradictions = live_contradictions(&conn).unwrap();
        assert_eq!(contradictions.len(), 1);
        assert_eq!(contradictions[0].0, "nadia");
    }
}
