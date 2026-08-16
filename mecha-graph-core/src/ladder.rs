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
//!   (D3) via [`demote_class`], which drops straight to staged.
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

/// A class's human verdict record: (accepted, total).
///
/// Human only, and that asymmetry is the ladder's oldest rule — a lane must
/// not promote itself. Machine rejects carry a `precheck:%` reason, so they
/// are excluded here exactly as they are excluded from moving the streak.
fn human_record(conn: &Connection, proposer: &str, predicate: &str) -> Result<(i64, i64)> {
    Ok(conn.query_row(
        &format!(
            "SELECT
               SUM(status = 'accepted'),
               SUM(status IN ('accepted','rejected'))
             FROM fact_candidate
             WHERE COALESCE(proposed_by,'?') = ?1 AND {KEY_SQL} = ?2
               AND COALESCE(reject_reason,'') NOT LIKE 'precheck:%'"
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
const KEY_SQL: &str =
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;

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
}
