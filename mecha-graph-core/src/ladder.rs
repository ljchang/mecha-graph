//! The autonomy ladder (PLAN.md D1): staged → sampled → trusted, per
//! (proposer, predicate) class — oversight O(classes), not O(facts).
//!
//! Rules:
//! - Only HUMAN verdicts move the ladder. Auto-accepts and precheck's
//!   machine rejects are invisible here — a lane must not promote (or
//!   demote) itself.
//! - Promotion is statistical, not a streak: a class climbs when the
//!   Wilson lower bound on its human accept rate clears the rung's floor
//!   ([`PROMOTE_LB_SAMPLED`], [`PROMOTE_LB_TRUSTED`]). The old rule —
//!   twenty CONSECUTIVE accepts — was unreachable in practice and left
//!   all 724 classes staged; that history is in `PROMOTE_LB_SAMPLED`.
//! - A human reject lowers the rate and does NOT demote — one bad item
//!   in a good class is noise, and it no longer costs the class twenty
//!   more accepts either. Demotion is reserved for in-use corrections
//!   (D3) via [`demote_class`], which drops straight to staged — and,
//!   ratified 2026-08-29 (review-on-use §3), for RETRIEVAL utility: a
//!   class whose eligible facts nobody's queries ever pull demotes ONE
//!   rung via [`utility_demotions`]. That does not reopen the accept-rate
//!   refusal above: utility is a different signal with a different owner
//!   (the query stream, not the reviewer), so a human reject still never
//!   demotes anything.
//! - Commitment classes never ride the ladder (they materialize tasks).
//! - Promotions and demotions are logged to event_log — the first
//!   writer of the observability spine.
//!
//! What a rung means (wired in precheck's auto-accept lane):
//! staged = everything queues for review; sampled = auto-accept with a
//! 1-in-10 spot-check held for review; trusted = auto-accept.

use crate::error::Result;
use crate::ledger::log_event;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;

/// Consecutive human accepts required to climb one rung.
///
/// Retained only for the warm-start seed and the ledger column; promotion no
/// longer reads it. See [`promote_floor`] for why.
pub const PROMOTE_STREAK: i64 = 20;

/// Wilson lower bound a class must clear to reach a rung.
///
/// **Why the streak rule had to go.** Twenty *consecutive* human accepts is
/// unreachable by ordinary review: at 80% acceptance the run happens 1.2% of
/// the time, at 90% only 12%, and a single reject sends it back to zero.
/// Measured 2026-08-16, every one of 724 classes sat at `staged` and the best
/// streak in the graph was 7 — while `llm/uses` had been accepted 41 times out
/// of 51 and `llm/works_at` 8 out of 8. The ladder was not a slow gradient, it
/// was a closed door, and the only autonomy that ever worked was the
/// hand-maintained durable allowlist beside it.
///
/// **Why a Wilson lower bound rather than a rate plus a minimum N.** The
/// obvious replacement — "≥90% over ≥20 verdicts" — needs two numbers that
/// have to be argued separately, and it treats 18/20 and 180/200 as the same
/// evidence. The Wilson score interval's lower bound folds both into one: it
/// asks what acceptance rate the observed record would still support at 95%
/// confidence, so a small sample is penalised automatically and a long record
/// converges on its true rate. 8/8 and 41/51 both land at ≈0.675 — genuinely
/// comparable evidence, which a raw rate (100% vs 80%) badly misjudges in the
/// small sample's favour.
///
/// **Why these values.** Against the real verdict history: `uses` (41/51) and
/// `works_at` (8/8) clear 0.65; `works_on` (31/46 → 0.53) and `authored`
/// (86/146 → 0.51) do not. That is the intended line — those two are the
/// classes review has actually vindicated. `TRUSTED` at 0.85 is deliberately
/// far above anything today reaches, because `Sampled` already auto-accepts
/// while holding one in ten for review, and that spot-check is the thing worth
/// keeping until a class has a very long clean record.
pub const PROMOTE_LB_SAMPLED: f64 = 0.65;
pub const PROMOTE_LB_TRUSTED: f64 = 0.85;

/// The bar for entering a rung.
fn promote_floor(target: Rung) -> f64 {
    match target {
        Rung::Staged => 0.0,
        Rung::Sampled => PROMOTE_LB_SAMPLED,
        Rung::Trusted => PROMOTE_LB_TRUSTED,
    }
}

/// Lower bound of the Wilson score interval at 95% confidence.
///
/// The conservative end of what this record supports. Zero verdicts is zero
/// evidence, which must not read as a perfect record.
pub fn wilson_lower_bound(accepted: i64, total: i64) -> f64 {
    if total <= 0 {
        return 0.0;
    }
    const Z: f64 = 1.96;
    let n = total as f64;
    let p = accepted as f64 / n;
    let z2 = Z * Z;
    let centre = p + z2 / (2.0 * n);
    let margin = Z * ((p * (1.0 - p) / n) + z2 / (4.0 * n * n)).sqrt();
    ((centre - margin) / (1.0 + z2 / n)).max(0.0)
}

/// The SQL predicate for "this verdict was the owner's": `reviewed_by` says
/// so, or the row predates the column (V017) and is not a machine reject by
/// reason. Legacy accepts keep counting as the owner's — rewriting history
/// would gut the record the ladder runs on — but every row written since the
/// column exists is exact, which is what stops a cascade or an auto-lane
/// from promoting the class it feeds.
///
/// The COALESCE is not decoration. `reviewed_by = 'user'` on a legacy NULL
/// row is SQL NULL, and NULL survives OR when the other side is false — so a
/// legacy machine reject made the whole predicate NULL, and one class whose
/// decided rows were ALL machine rejects turned `SUM(...)` NULL and errored
/// every surface that read it. In a WHERE clause that NULL merely filtered
/// (NULL is not true), which is why `human_record` looked fine while the
/// cluster view — the same predicate inside a SUM — fell over.
pub const HUMAN_VERDICT_SQL: &str = "(COALESCE(reviewed_by,'') = 'user' \
     OR (reviewed_by IS NULL AND COALESCE(reject_reason,'') NOT LIKE 'precheck:%'))";

/// A class's human verdict record: (accepted, total).
///
/// Human only, and that asymmetry is the ladder's oldest rule — a lane must
/// not promote itself. Machine rejects carry a `precheck:%` reason and
/// machine accepts carry a non-user `reviewed_by`; both are excluded here
/// exactly as they are excluded from moving the streak.
pub(crate) fn human_record(
    conn: &Connection,
    proposer: &str,
    predicate: &str,
) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        &format!(
            "SELECT
               SUM(status = 'accepted'),
               SUM(status IN ('accepted','rejected'))
             FROM fact_candidate
             WHERE COALESCE(proposed_by,'?') = ?1 AND {KEY_SQL} = ?2
               AND {HUMAN_VERDICT_SQL}"
        ),
        params![proposer, predicate],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?.unwrap_or(0),
                r.get::<_, Option<i64>>(1)?.unwrap_or(0),
            ))
        },
    )?)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Rung {
    Staged,
    Sampled,
    Trusted,
}

impl Rung {
    pub fn as_str(self) -> &'static str {
        match self {
            Rung::Staged => "staged",
            Rung::Sampled => "sampled",
            Rung::Trusted => "trusted",
        }
    }
    fn parse(s: &str) -> Rung {
        match s {
            "sampled" => Rung::Sampled,
            "trusted" => Rung::Trusted,
            _ => Rung::Staged,
        }
    }
    fn next(self) -> Rung {
        match self {
            Rung::Staged => Rung::Sampled,
            _ => Rung::Trusted,
        }
    }
}

/// The cluster-view key expression — MUST match `precheck::cluster_key`.
pub(crate) const KEY_SQL: &str =
    "COALESCE(json_extract(payload,'$.predicate'), '(' || COALESCE(json_extract(payload,'$.kind'),'none') || ')')";

/// A class's current rung; absent classes are staged.
pub fn get_rung(conn: &Connection, proposer: &str, predicate: &str) -> Result<Rung> {
    Ok(conn
        .query_row(
            "SELECT rung FROM class_ledger WHERE proposer = ?1 AND predicate = ?2",
            params![proposer, predicate],
            |r| r.get::<_, String>(0),
        )
        .optional()?
        .map(|s| Rung::parse(&s))
        .unwrap_or(Rung::Staged))
}

/// Warm start: the first touch of a class seeds its streak from verdict
/// history — consecutive accepts since the last reject on the same key.
/// The 3,7k pre-ladder verdicts count; nobody starts from zero.
fn ensure_class(conn: &Connection, proposer: &str, predicate: &str) -> Result<()> {
    let exists: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM class_ledger WHERE proposer = ?1 AND predicate = ?2",
        params![proposer, predicate],
        |r| r.get(0),
    )?;
    if exists {
        return Ok(());
    }
    let streak: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM fact_candidate
             WHERE COALESCE(proposed_by,'?') = ?1 AND {KEY_SQL} = ?2
               AND status = 'accepted'
               AND COALESCE(reviewed_at,'') > COALESCE((
                   SELECT MAX(reviewed_at) FROM fact_candidate
                   WHERE COALESCE(proposed_by,'?') = ?1 AND {KEY_SQL} = ?2
                     AND status = 'rejected'), '')"
        ),
        params![proposer, predicate],
        |r| r.get(0),
    )?;
    conn.execute(
        "INSERT OR IGNORE INTO class_ledger (proposer, predicate, streak) VALUES (?1, ?2, ?3)",
        params![proposer, predicate, streak],
    )?;
    Ok(())
}

/// Record a HUMAN verdict on a class. Machine verdicts (auto-accept,
/// precheck auto-reject) must not call this. `commitment` classes are
/// ignored. Returns the rung after the verdict.
pub fn note_verdict(
    conn: &Connection,
    proposer: &str,
    predicate: &str,
    accepted: bool,
    commitment: bool,
) -> Result<Rung> {
    if commitment {
        return Ok(Rung::Staged);
    }
    ensure_class(conn, proposer, predicate)?;
    let rung = get_rung(conn, proposer, predicate)?;
    if !accepted {
        conn.execute(
            "UPDATE class_ledger SET streak = 0, updated_at = datetime('now')
             WHERE proposer = ?1 AND predicate = ?2",
            params![proposer, predicate],
        )?;
        return Ok(rung);
    }
    // The streak is still kept — it is the warm-start seed and it reads well
    // in the cluster view — but it no longer decides anything.
    conn.execute(
        "UPDATE class_ledger SET streak = streak + 1, updated_at = datetime('now')
         WHERE proposer = ?1 AND predicate = ?2",
        params![proposer, predicate],
    )?;

    if rung == Rung::Trusted {
        return Ok(rung);
    }
    let up = rung.next();
    let (accepted, total) = human_record(conn, proposer, predicate)?;
    let lb = wilson_lower_bound(accepted, total);
    if lb < promote_floor(up) {
        return Ok(rung);
    }

    conn.execute(
        "UPDATE class_ledger SET rung = ?3, streak = 0,
                promoted_at = datetime('now'), updated_at = datetime('now')
         WHERE proposer = ?1 AND predicate = ?2",
        params![proposer, predicate, up.as_str()],
    )?;
    log_event(
        conn,
        "class_promoted",
        Some(&format!("{proposer}·{predicate}")),
        Some(&format!(
            "{{\"from\":\"{}\",\"to\":\"{}\",\"accepted\":{accepted},\"judged\":{total},\
              \"wilson_lb\":{lb:.3}}}",
            rung.as_str(),
            up.as_str()
        )),
    )?;
    Ok(up)
}

/// Demote a class straight to staged (D3: any in-use correction).
pub fn demote_class(
    conn: &Connection,
    proposer: &str,
    predicate: &str,
    reason: &str,
) -> Result<()> {
    ensure_class(conn, proposer, predicate)?;
    let from = get_rung(conn, proposer, predicate)?;
    conn.execute(
        "UPDATE class_ledger SET rung = 'staged', streak = 0,
                demoted_at = datetime('now'), updated_at = datetime('now')
         WHERE proposer = ?1 AND predicate = ?2",
        params![proposer, predicate],
    )?;
    log_event(
        conn,
        "class_demoted",
        Some(&format!("{proposer}·{predicate}")),
        Some(&format!(
            "{{\"from\":\"{}\",\"reason\":{}}}",
            from.as_str(),
            serde_json::Value::String(reason.into())
        )),
    )?;
    Ok(())
}

/// All ladder rows, for surfaces (rung column in cluster views).
pub fn ladder_rows(conn: &Connection) -> Result<Vec<(String, String, Rung, i64)>> {
    let mut stmt = conn.prepare(
        "SELECT proposer, predicate, rung, streak FROM class_ledger ORDER BY proposer, predicate",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get(1)?,
                r.get::<_, String>(2)?,
                r.get(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows
        .into_iter()
        .map(|(p, pr, ru, s)| (p, pr, Rung::parse(&ru), s))
        .collect())
}

/// One class as the ladder sees it: where it sits, what its human record
/// is, and the rung that record would support.
#[derive(Debug, Clone, Serialize)]
pub struct LadderView {
    pub proposer: String,
    pub predicate: String,
    pub rung: Rung,
    /// The rung one recompute pass would leave it at — at most one rung above
    /// `rung`, never below it.
    pub earned: Rung,
    pub accepted: i64,
    pub judged: i64,
    pub wilson_lb: f64,
    /// Candidates of this class waiting in the queue right now.
    pub pending: i64,
}

/// Every class with either a ledger row or a human verdict on record.
///
/// The union matters: `ensure_class` runs only inside `note_verdict`, so a
/// class whose verdicts all predate the ladder has no ledger row at all —
/// and those are exactly the classes a recompute exists to reach.
pub fn ladder_view(conn: &Connection) -> Result<Vec<LadderView>> {
    let mut classes: Vec<(String, String)> = conn
        .prepare(&format!(
            "SELECT DISTINCT COALESCE(proposed_by,'?'), {KEY_SQL}
             FROM fact_candidate
             WHERE status IN ('accepted','rejected')
               AND COALESCE(reject_reason,'') NOT LIKE 'precheck:%'
             UNION
             SELECT proposer, predicate FROM class_ledger"
        ))?
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    classes.sort();

    let mut out = Vec::new();
    for (proposer, predicate) in classes {
        // Kind-keyed classes (`(commitment)` and kin) never auto-accept and
        // `note_verdict` refuses to move them; the recompute mirrors that.
        if predicate.starts_with('(') {
            continue;
        }
        let rung = get_rung(conn, &proposer, &predicate)?;
        let (accepted, judged) = human_record(conn, &proposer, &predicate)?;
        let lb = wilson_lower_bound(accepted, judged);
        // One rung per pass, exactly as `note_verdict` promotes one rung per
        // verdict. Trusted means no spot-check ever again, so a class walks
        // through Sampled — where 1-in-10 still reaches a human — first.
        let earned = if rung == Rung::Trusted {
            Rung::Trusted
        } else {
            let up = rung.next();
            if lb >= promote_floor(up) {
                up
            } else {
                rung
            }
        };
        let pending: i64 = conn.query_row(
            &format!(
                "SELECT COUNT(*) FROM fact_candidate
                 WHERE status = 'proposed'
                   AND COALESCE(proposed_by,'?') = ?1 AND {KEY_SQL} = ?2"
            ),
            params![proposer, predicate],
            |r| r.get(0),
        )?;
        out.push(LadderView {
            proposer,
            predicate,
            rung,
            earned,
            accepted,
            judged,
            wilson_lb: lb,
            pending,
        });
    }
    Ok(out)
}

/// Re-derive rungs from the human verdict record: promote every class whose
/// record already clears the next floor, one rung per pass, never demoting.
///
/// One-shot maintenance on the `recompute-confidence` precedent. The Wilson
/// rule replaced the streak on 2026-08-16, but promotion still fires only
/// inside [`note_verdict`] — so a class whose verdicts all landed before the
/// switch sits at `staged` however strong its record, and nothing ever
/// re-reads it. This is still the owner's own verdict history doing the
/// promoting; the lane has not promoted itself. Demotion stays where it was:
/// correction-driven ([`demote_class`]), never statistical.
///
/// Returns the classes a pass would move; with `apply` false nothing is
/// written.
pub fn recompute_rungs(conn: &Connection, apply: bool) -> Result<Vec<LadderView>> {
    let moves: Vec<LadderView> = ladder_view(conn)?
        .into_iter()
        .filter(|v| v.earned != v.rung)
        .collect();
    if !apply {
        return Ok(moves);
    }
    for v in &moves {
        ensure_class(conn, &v.proposer, &v.predicate)?;
        conn.execute(
            "UPDATE class_ledger SET rung = ?3, streak = 0,
                    promoted_at = datetime('now'), updated_at = datetime('now')
             WHERE proposer = ?1 AND predicate = ?2",
            params![v.proposer, v.predicate, v.earned.as_str()],
        )?;
        log_event(
            conn,
            "class_promoted",
            Some(&format!("{}\u{b7}{}", v.proposer, v.predicate)),
            Some(&format!(
                "{{\"from\":\"{}\",\"to\":\"{}\",\"accepted\":{},\"judged\":{},\
                  \"wilson_lb\":{:.3},\"recompute\":true}}",
                v.rung.as_str(),
                v.earned.as_str(),
                v.accepted,
                v.judged,
                v.wilson_lb
            )),
        )?;
    }
    Ok(moves)
}

// ─── Utility: retrieval is the ground truth of usefulness ───────────────
// (review-on-use §3 — the half of the loop `accept_lb` never had.)

/// One (proposer, predicate) class's retrieval record. `rate` is `None`
/// over an empty denominator — a class whose facts are all younger than
/// the opportunity window has not been measured, and a dash is never zero.
#[derive(Debug, Serialize)]
pub struct ClassUtility {
    pub proposer: String,
    pub predicate: String,
    /// Live facts in the class (any tier — utility asks whether the class
    /// is worth having at all, not whether review got to it).
    pub live: i64,
    /// Live facts old enough to have had retrieval opportunity.
    pub eligible: i64,
    /// Eligible facts a context pack has ever served.
    pub retrieved: i64,
    pub rate: Option<f64>,
}

/// Floors for the utility half of the loop. Deliberately not consts: the
/// right numbers need weeks of `fact_usage` data at the new generation
/// rate (open decision 3), so callers pass them — the nightly from env,
/// report-only until the owner sets a floor.
#[derive(Debug, Clone, Copy)]
pub struct UtilityFloors {
    /// Retrieval rate below which a class demotes/gates.
    pub floor: f64,
    /// Classes with fewer eligible facts than this are not measured.
    pub min_eligible: i64,
    /// Days a fact must have been live to count as an opportunity.
    pub opportunity_days: i64,
}

/// Per-class retrieval record over facts older than the opportunity
/// window. Class identity is (extractor, predicate) — the extractor on a
/// minted fact IS the proposer that staged it.
pub fn class_utility(conn: &Connection, opportunity_days: i64) -> Result<Vec<ClassUtility>> {
    let cutoff = format!("-{opportunity_days} days");
    let mut stmt = conn.prepare(
        "SELECT COALESCE(f.extractor,'?') AS proposer, f.predicate,
                COUNT(*) AS live,
                SUM(f.ingested_at <= datetime('now', ?1)) AS eligible,
                SUM(f.ingested_at <= datetime('now', ?1) AND t.ref_id IS NOT NULL) AS retrieved
         FROM fact f
         LEFT JOIN retrieval_touch t ON t.kind = 'fact' AND t.ref_id = f.uid
         WHERE f.valid_to IS NULL AND f.invalidated_at IS NULL
         GROUP BY proposer, f.predicate
         ORDER BY proposer, f.predicate",
    )?;
    let rows = stmt
        .query_map(params![cutoff], |r| {
            let eligible: i64 = r.get::<_, Option<i64>>("eligible")?.unwrap_or(0);
            let retrieved: i64 = r.get::<_, Option<i64>>("retrieved")?.unwrap_or(0);
            Ok(ClassUtility {
                proposer: r.get("proposer")?,
                predicate: r.get("predicate")?,
                live: r.get("live")?,
                eligible,
                retrieved,
                rate: (eligible > 0).then(|| retrieved as f64 / eligible as f64),
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Demote classes the query stream has voted against: rung above staged,
/// enough eligible facts to mean something, retrieval rate under the
/// floor. ONE rung per run — trusted falls to sampled, sampled to staged
/// — beside the human-verdict promotion, never replacing it. `apply:
/// false` reports what would demote; either way the caller owns the
/// loud nightly line, and applied demotions also land in `event_log`
/// (`class_demoted_utility`) — a guard that acts silently is the failure
/// mode this repo keeps finding.
pub fn utility_demotions(
    conn: &Connection,
    floors: &UtilityFloors,
    apply: bool,
) -> Result<Vec<(ClassUtility, Rung, Rung)>> {
    let mut out = Vec::new();
    for cu in class_utility(conn, floors.opportunity_days)? {
        if cu.eligible < floors.min_eligible {
            continue;
        }
        let Some(rate) = cu.rate else { continue };
        if rate >= floors.floor {
            continue;
        }
        let from = get_rung(conn, &cu.proposer, &cu.predicate)?;
        let to = match from {
            Rung::Trusted => Rung::Sampled,
            Rung::Sampled => Rung::Staged,
            Rung::Staged => continue, // nowhere lower to go
        };
        if apply {
            conn.execute(
                "UPDATE class_ledger SET rung = ?3, streak = 0,
                        demoted_at = datetime('now'), updated_at = datetime('now')
                 WHERE proposer = ?1 AND predicate = ?2",
                params![cu.proposer, cu.predicate, to.as_str()],
            )?;
            log_event(
                conn,
                "class_demoted_utility",
                Some(&format!("{}·{}", cu.proposer, cu.predicate)),
                Some(&format!(
                    "{{\"from\":\"{}\",\"to\":\"{}\",\"rate\":{:.3},\"eligible\":{}}}",
                    from.as_str(),
                    to.as_str(),
                    rate,
                    cu.eligible
                )),
            )?;
        }
        out.push((cu, from, to));
    }
    Ok(out)
}

/// How many human verdicts a class needs before its precision can gate
/// generation. Below this, `accept_lb` is noise about a class nobody has
/// really judged.
pub const GATE_MIN_JUDGED: i64 = 20;

/// The precision floor: a class judged at least [`GATE_MIN_JUDGED`] times
/// whose Wilson lower bound sits under this stops being extracted — even
/// optimistically, fewer than ~1 in 7 of its claims survive review.
/// Calibrated against the measured tail: kNN/structural/rules ran 4–14%
/// accept and were hand-retired; `llm·has_role` ran 2% while its class
/// kept flooding the queue. `accept_lb` was computed on every render
/// since 08-16 and consumed by nothing — this is its consumer.
pub const GATE_ACCEPT_LB_FLOOR: f64 = 0.15;

/// A class generation should stop producing, and why — the self-limiting
/// half of the system. Consumed by extraction (the predicate drops out of
/// the prompt enum for that proposer) and printed by the nightly.
#[derive(Debug, Serialize)]
pub struct GatedClass {
    pub proposer: String,
    pub predicate: String,
    pub why: String,
}

/// Classes below the precision floor (always measured — the human record
/// is real evidence today) or, when `utility` floors are supplied, below
/// the retrieval-utility floor (opt-in until the usage data has tenure).
pub fn gated_classes(
    conn: &Connection,
    utility: Option<&UtilityFloors>,
) -> Result<Vec<GatedClass>> {
    let mut out: Vec<GatedClass> = Vec::new();
    let mut stmt = conn.prepare(&format!(
        "SELECT COALESCE(proposed_by,'?') AS proposer, {KEY_SQL} AS k,
                SUM(status = 'accepted') AS acc,
                COUNT(*) AS judged
         FROM fact_candidate
         WHERE status IN ('accepted','rejected') AND {HUMAN_VERDICT_SQL}
         GROUP BY proposer, k
         HAVING judged >= ?1"
    ))?;
    let rows = stmt.query_map(params![GATE_MIN_JUDGED], |r| {
        Ok((
            r.get::<_, String>("proposer")?,
            r.get::<_, String>("k")?,
            r.get::<_, Option<i64>>("acc")?.unwrap_or(0),
            r.get::<_, i64>("judged")?,
        ))
    })?;
    for row in rows {
        let (proposer, predicate, acc, judged) = row?;
        if predicate.starts_with('(') {
            continue; // kind-keyed classes (commitments) never gate
        }
        let lb = wilson_lower_bound(acc, judged);
        if lb < GATE_ACCEPT_LB_FLOOR {
            out.push(GatedClass {
                proposer,
                predicate,
                why: format!("accept_lb {lb:.2} over {judged} human verdicts"),
            });
        }
    }
    if let Some(floors) = utility {
        for cu in class_utility(conn, floors.opportunity_days)? {
            if cu.eligible < floors.min_eligible {
                continue;
            }
            let Some(rate) = cu.rate else { continue };
            if rate < floors.floor
                && !out
                    .iter()
                    .any(|g| g.proposer == cu.proposer && g.predicate == cu.predicate)
            {
                out.push(GatedClass {
                    proposer: cu.proposer,
                    predicate: cu.predicate,
                    why: format!("retrieval {rate:.2} over {} eligible facts", cu.eligible),
                });
            }
        }
    }
    out.sort_by(|a, b| (&a.proposer, &a.predicate).cmp(&(&b.proposer, &b.predicate)));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;

    fn seed_fact(
        conn: &Connection,
        extractor: &str,
        predicate: &str,
        age_days: i64,
        touched: bool,
    ) {
        let node = crate::graph::Node::new("ada", "person", "Ada");
        crate::graph::upsert_node(conn, &node).unwrap();
        let uid = crate::ids::new_uid();
        conn.execute(
            "INSERT INTO fact (uid, subject_id, predicate, statement, extractor, sensitivity,
                               polarity, tier, ingested_at, confidence)
             VALUES (?1, 'ada', ?2, ?1 || ' statement', ?3, 'personal', 'positive', 'shadow',
                     datetime('now', '-' || ?4 || ' days'), 0.8)",
            params![uid, predicate, extractor, age_days],
        )
        .unwrap();
        if touched {
            conn.execute(
                "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
                 VALUES ('fact', ?1, 1, datetime('now'), datetime('now'))",
                params![uid],
            )
            .unwrap();
        }
    }

    /// A class is measured only over facts old enough to have had a
    /// chance; a class of only-young facts reports None, never 0%.
    #[test]
    fn utility_rate_needs_opportunity_and_a_dash_is_never_zero() {
        let conn = open_memory().unwrap();
        for _ in 0..3 {
            seed_fact(&conn, "llm", "works_on", 30, false);
        }
        seed_fact(&conn, "llm", "works_on", 30, true);
        for _ in 0..4 {
            seed_fact(&conn, "llm", "about", 1, false); // too young to judge
        }
        let cu = class_utility(&conn, 21).unwrap();
        let works_on = cu.iter().find(|c| c.predicate == "works_on").unwrap();
        assert_eq!(works_on.eligible, 4);
        assert_eq!(works_on.retrieved, 1);
        assert_eq!(works_on.rate, Some(0.25));
        let about = cu.iter().find(|c| c.predicate == "about").unwrap();
        assert_eq!(about.eligible, 0);
        assert_eq!(
            about.rate, None,
            "no opportunity is not the same as useless"
        );
    }

    /// Utility demotion drops ONE rung, only above staged, only over the
    /// floors — and an applied demotion is loud (event_log), while a dry
    /// run writes nothing.
    #[test]
    fn utility_demotion_is_one_rung_floored_and_loud() {
        let conn = open_memory().unwrap();
        for _ in 0..25 {
            seed_fact(&conn, "llm", "works_on", 30, false); // never retrieved
        }
        conn.execute(
            "INSERT INTO class_ledger (proposer, predicate, rung) VALUES ('llm','works_on','trusted')",
            [],
        )
        .unwrap();
        let floors = UtilityFloors {
            floor: 0.05,
            min_eligible: 20,
            opportunity_days: 21,
        };
        // Dry run: reported, nothing moves, nothing logged.
        let would = utility_demotions(&conn, &floors, false).unwrap();
        assert_eq!(would.len(), 1);
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Trusted);
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_log WHERE kind = 'class_demoted_utility'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 0, "a dry run must not write the ledger");
        // Applied: trusted falls exactly one rung, and says so.
        let done = utility_demotions(&conn, &floors, true).unwrap();
        assert_eq!(done.len(), 1);
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Sampled);
        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_log WHERE kind = 'class_demoted_utility'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1);
        // Second application: sampled → staged; third finds nowhere lower.
        utility_demotions(&conn, &floors, true).unwrap();
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Staged);
        assert!(utility_demotions(&conn, &floors, true).unwrap().is_empty());
    }

    /// The precision gate: a class the owner has judged often and almost
    /// always rejected stops being extracted; a vindicated class and a
    /// barely-judged class do not gate.
    #[test]
    fn the_precision_gate_needs_a_real_and_bad_record() {
        let conn = open_memory().unwrap();
        let seed = |pred: &str, status: &str, n: usize| {
            for _ in 0..n {
                conn.execute(
                    "INSERT INTO fact_candidate (payload, proposed_by, status, reviewed_by, reviewed_at)
                     VALUES (json_object('predicate', ?1, 'subject', 'x', 'statement', 's'),
                             'llm', ?2, 'user', datetime('now'))",
                    params![pred, status],
                )
                .unwrap();
            }
        };
        seed("has_role", "rejected", 24);
        seed("has_role", "accepted", 1); // 1/25 — hopeless
        seed("works_at", "accepted", 20);
        seed("works_at", "rejected", 2); // vindicated
        seed("mentors", "rejected", 5); // bad but barely judged

        let gated = gated_classes(&conn, None).unwrap();
        assert_eq!(gated.len(), 1);
        assert_eq!(gated[0].predicate, "has_role");
        assert!(gated[0].why.contains("25 human verdicts"));
    }

    fn seed_verdict(conn: &Connection, status: &str, ts: &str) {
        conn.execute(
            "INSERT INTO fact_candidate (payload, proposed_by, status, reviewed_at)
             VALUES ('{\"predicate\":\"works_on\",\"subject\":\"x\",\"statement\":\"s\"}',
                     'llm', ?1, ?2)",
            params![status, ts],
        )
        .unwrap();
    }

    /// Seed a record of `acc` accepts and `rej` rejects, interleaved so no
    /// long consecutive run exists — the old rule would promote none of
    /// these no matter how good the ratio.
    fn seed_record(conn: &Connection, acc: i64, rej: i64) {
        let mut t = 0;
        for i in 0..(acc + rej) {
            let status = if i % (acc + rej) < rej.min(acc + rej) && i % 3 == 0 && t < rej {
                t += 1;
                "rejected"
            } else {
                "accepted"
            };
            seed_verdict(
                conn,
                status,
                &format!("2026-08-02 00:{:02}:{:02}", i / 60, i % 60),
            );
        }
        // Top up whichever side the interleave shortchanged.
        let (mut a, mut r) = (0, 0);
        let counted: Vec<String> = conn
            .prepare("SELECT status FROM fact_candidate")
            .unwrap()
            .query_map([], |x| x.get(0))
            .unwrap()
            .collect::<std::result::Result<_, _>>()
            .unwrap();
        for s in counted {
            if s == "accepted" {
                a += 1
            } else if s == "rejected" {
                r += 1
            }
        }
        for i in a..acc {
            seed_verdict(conn, "accepted", &format!("2026-08-03 00:00:{:02}", i % 60));
        }
        for i in r..rej {
            seed_verdict(conn, "rejected", &format!("2026-08-04 00:00:{:02}", i % 60));
        }
    }

    #[test]
    fn wilson_bound_penalises_a_short_record() {
        // A perfect 3/3 is weaker evidence than 41/51 at 80%, and the bound
        // says so — which is the whole reason for using it over a raw rate
        // plus a minimum-N rule argued separately.
        let tiny = wilson_lower_bound(3, 3);
        let long = wilson_lower_bound(41, 51);
        assert!(
            tiny < long,
            "3/3 ({tiny:.3}) must not outrank 41/51 ({long:.3})"
        );
        assert!((0.43..0.45).contains(&tiny), "3/3 ≈ 0.44, got {tiny:.3}");
        assert!((0.66..0.69).contains(&long), "41/51 ≈ 0.675, got {long:.3}");
        // 8/8 and 41/51 are comparable evidence; a raw rate would call the
        // first perfect and the second an 80% class.
        let eight = wilson_lower_bound(8, 8);
        assert!(
            (eight - long).abs() < 0.02,
            "8/8 {eight:.3} ≈ 41/51 {long:.3}"
        );
        assert_eq!(
            wilson_lower_bound(0, 0),
            0.0,
            "no evidence is not a perfect record"
        );
    }

    /// The measured shapes from the live graph, on the rule that replaced the
    /// streak. `uses` (41/51) and `works_at` (8/8) are the classes review has
    /// actually vindicated; `works_on` (31/46) and `authored` (86/146) are
    /// not, and the floor is set to separate exactly those.
    #[test]
    fn promotion_follows_the_record_not_a_lucky_run() {
        for (acc, rej, expect, label) in [
            (41, 10, Rung::Sampled, "uses 41/51"),
            (8, 0, Rung::Sampled, "works_at 8/8"),
            (31, 15, Rung::Staged, "works_on 31/46"),
            (86, 60, Rung::Staged, "authored 86/146"),
            (3, 0, Rung::Staged, "a perfect but tiny record"),
        ] {
            let conn = open_memory().unwrap();
            seed_record(&conn, acc, rej);
            let rung = note_verdict(&conn, "llm", "works_on", true, false).unwrap();
            assert_eq!(rung, expect, "{label}");
        }
    }

    /// A single reject used to send a strong class back to zero and cost it
    /// twenty more accepts. Now it moves the rate slightly and nothing else —
    /// which is what made the old ladder unreachable in practice.
    #[test]
    fn one_reject_no_longer_undoes_a_strong_record() {
        let conn = open_memory().unwrap();
        seed_record(&conn, 41, 10);
        assert_eq!(
            note_verdict(&conn, "llm", "works_on", true, false).unwrap(),
            Rung::Sampled
        );
        // A reject holds the rung (demotion is reserved for D3 corrections)…
        assert_eq!(
            note_verdict(&conn, "llm", "works_on", false, false).unwrap(),
            Rung::Sampled
        );
        // …and the next accept does not have to re-earn twenty in a row.
        assert_eq!(
            note_verdict(&conn, "llm", "works_on", true, false).unwrap(),
            Rung::Sampled
        );
    }

    #[test]
    fn test_warm_start_and_promotion_with_event() {
        let conn = open_memory().unwrap();
        // History: a reject, then 19 consecutive accepts.
        seed_verdict(&conn, "rejected", "2026-08-01 00:00:00");
        for i in 0..19 {
            seed_verdict(&conn, "accepted", &format!("2026-08-02 00:00:{i:02}"));
        }

        // First live human accept: streak warm-starts at 19 → 20 → promote.
        let rung = note_verdict(&conn, "llm", "works_on", true, false).unwrap();
        assert_eq!(rung, Rung::Sampled, "warm-started streak promotes");

        let events: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_log WHERE kind = 'class_promoted'
                 AND ref = 'llm·works_on'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(events, 1, "promotion writes the event_log");

        // A reject resets the streak but keeps the rung.
        assert_eq!(
            note_verdict(&conn, "llm", "works_on", false, false).unwrap(),
            Rung::Sampled
        );
        let streak: i64 = conn
            .query_row(
                "SELECT streak FROM class_ledger WHERE proposer='llm'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(streak, 0);

        // Demotion (in-use correction) drops to staged + event.
        demote_class(&conn, "llm", "works_on", "wrong org in pack").unwrap();
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Staged);
        let demos: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM event_log WHERE kind='class_demoted'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(demos, 1);
    }

    #[test]
    fn test_commitments_never_ride() {
        let conn = open_memory().unwrap();
        for _ in 0..25 {
            note_verdict(&conn, "llm:commitment", "(commitment)", true, true).unwrap();
        }
        let rows: i64 = conn
            .query_row("SELECT COUNT(*) FROM class_ledger", [], |r| r.get(0))
            .unwrap();
        assert_eq!(rows, 0, "commitment verdicts must not create ladder state");
    }

    /// The recompute exists for exactly this shape: a strong record whose
    /// verdicts all predate the ladder, so `ensure_class` never ran and the
    /// class has no ledger row at all. `get_rung` reads it as staged forever;
    /// one recompute pass reads the record instead.
    #[test]
    fn recompute_promotes_a_pre_ladder_record() {
        let conn = open_memory().unwrap();
        seed_record(&conn, 41, 10); // uses-shaped: LB ~0.675, clears 0.65
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Staged);

        let moves = recompute_rungs(&conn, true).unwrap();
        assert_eq!(moves.len(), 1);
        assert_eq!(moves[0].earned, Rung::Sampled);
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Sampled);

        // One rung per pass: the same record does not clear the Trusted
        // floor, so a second pass moves nothing — the spot-check rung is
        // where a class waits for more evidence.
        let again = recompute_rungs(&conn, true).unwrap();
        assert!(again.is_empty(), "0.675 must not clear the 0.85 floor");
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Sampled);
    }

    /// `apply: false` is a preview: it names the moves and writes nothing.
    #[test]
    fn recompute_dry_run_writes_nothing() {
        let conn = open_memory().unwrap();
        seed_record(&conn, 41, 10);
        let moves = recompute_rungs(&conn, false).unwrap();
        assert_eq!(moves.len(), 1);
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Staged);
    }

    /// Never demotes: a class review promoted keeps its rung however the
    /// record has drifted since. Demotion stays correction-driven (D3).
    #[test]
    fn recompute_never_demotes() {
        let conn = open_memory().unwrap();
        seed_record(&conn, 5, 20); // LB far below every floor
        ensure_class(&conn, "llm", "works_on").unwrap();
        conn.execute(
            "UPDATE class_ledger SET rung = 'sampled' WHERE proposer = 'llm'",
            [],
        )
        .unwrap();
        let moves = recompute_rungs(&conn, true).unwrap();
        assert!(moves.is_empty());
        assert_eq!(get_rung(&conn, "llm", "works_on").unwrap(), Rung::Sampled);
    }

    /// Machine rejects must not starve a promotion, and machine accepts must
    /// not fuel one — the human record is the only evidence, here exactly as
    /// in `note_verdict`. A pile of precheck rejects alone is no class at all.
    #[test]
    fn recompute_sees_only_human_verdicts() {
        let conn = open_memory().unwrap();
        for i in 0..30 {
            conn.execute(
                "INSERT INTO fact_candidate (payload, proposed_by, status, reviewed_at, reject_reason)
                 VALUES ('{\"predicate\":\"works_on\",\"subject\":\"x\",\"statement\":\"s\"}',
                         'llm', 'rejected', ?1, 'precheck: duplicate')",
                params![format!("2026-08-02 01:00:{:02}", i % 60)],
            )
            .unwrap();
        }
        assert!(recompute_rungs(&conn, true).unwrap().is_empty());
        // And with a real human record beside them, the machine rows change
        // nothing about the outcome.
        seed_record(&conn, 41, 10);
        let moves = recompute_rungs(&conn, true).unwrap();
        assert_eq!(moves.len(), 1);
        assert_eq!(
            moves[0].judged, 51,
            "machine rejects out of the denominator"
        );
    }
}
