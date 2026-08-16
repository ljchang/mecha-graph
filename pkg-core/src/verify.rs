//! The Verifier's deterministic tier (PLAN.md gossip roles, row 4:
//! "dereferences every provenance ref; checks each claim against the
//! actual rows … deterministic checks first, model only for residue").
//!
//! The role itself lives in mecha — judging is conversational work and
//! pkg stays non-conversational (ARCHITECTURE "Boundaries"). What lives
//! here is the half that needs no model: **dereference the provenance
//! and report what the rows actually say.** That is data work, and the
//! data is pkg's.
//!
//! Why this matters more on a shared graph than anywhere else: every
//! claim's provenance ref is dereferenceable, so "episode 4471 actually
//! says that" is a mechanical check rather than an act of faith. The
//! federated profile cannot have this — there the Verifier degrades to
//! consistency checks plus the per-peer ledger.
//!
//! **What this tier will and won't claim.** A lexical hit in cited
//! evidence is real support and needs no model. A *miss* is NOT a
//! refutation — extraction paraphrases ("Sigmalab" for "Sigma Lab"),
//! and concluding "false" from a failed substring match would
//! manufacture errors at scale. So a miss returns [`Verdict::Residue`]:
//! the explicit hand-off to the model tier. Being honest about the
//! boundary is what makes the deterministic verdicts trustworthy.

use rusqlite::{params, Connection};
use serde::Serialize;

use crate::error::Result;
use crate::fact::{self, Fact};

/// Outcome of checking one claim against the rows, most severe first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Verdict {
    /// The cited fact does not exist (or is no longer live) — a stale ref.
    Missing,
    /// >1 live value on a single-valued predicate.
    Contradicted,
    /// A live negation coexists with this live positive.
    Denied,
    /// Past the predicate's half-life (λ·age > ln 2).
    Stale,
    /// Cited evidence lexically supports the claim. No model needed.
    Supported,
    /// A *computed* claim whose formula was recomputed from the current
    /// rows and still holds. Stronger than [`Verdict::Supported`]: exact
    /// rather than lexical, and it catches decay a citation never would.
    Rederived,
    /// A computed claim whose formula no longer holds — the corpus moved
    /// and the derivation is stale. Deterministic, no model needed.
    Refuted,
    /// Cited evidence exists but does not lexically support the claim —
    /// **hand to the model tier**, do not conclude anything.
    Residue,
    /// Every cited episode is gone (redacted, §10) — honestly
    /// uncheckable, not unsupported (tombstone doctrine).
    Unverifiable,
    /// The claim cites no evidence at all — nothing to dereference.
    Unrooted,
}

#[derive(Debug, Clone, Serialize)]
pub struct ClaimCheck {
    pub fact_uid: String,
    pub statement: String,
    pub predicate: String,
    pub verdict: Verdict,
    pub detail: String,
    /// The distinctive term the lexical check looked for.
    pub sought: Option<String>,
    /// Cited episodes that lexically support the claim.
    pub supported_by: Vec<i64>,
    /// Every episode cited as evidence (founding + observation trail).
    pub cited: Vec<i64>,
    /// Observation trail by kind, e.g. [("asserted",1),("corroborated",3)].
    pub observations: Vec<(String, i64)>,
    /// Peer facts implicated by a Contradicted/Denied verdict.
    pub conflicts_with: Vec<String>,
}

/// Verify one live fact by uid.
pub fn verify_fact(conn: &Connection, uid: &str) -> Result<ClaimCheck> {
    let Some(f) = fact::get_fact_by_uid(conn, uid)? else {
        return Ok(missing(uid, "no fact with this uid"));
    };
    if f.valid_to.is_some() || f.invalidated_at.is_some() {
        return Ok(missing(uid, "fact is superseded — a stale provenance ref"));
    }
    check(conn, &f)
}

/// Verify every live positive fact about a node — the per-target report
/// the gossip harness runs before it trusts anything, ordered by
/// severity so the findings come first.
pub fn verify_node(conn: &Connection, node_id: &str, limit: usize) -> Result<Vec<ClaimCheck>> {
    let facts: Vec<Fact> = {
        let mut stmt = conn.prepare(
            "SELECT * FROM fact_current WHERE subject_id = ?1
             ORDER BY observation_count ASC, ingested_at DESC",
        )?;
        let rows = stmt
            .query_map(params![node_id], fact::row_to_fact)?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
    let mut out: Vec<ClaimCheck> = facts
        .iter()
        .map(|f| check(conn, f))
        .collect::<Result<_>>()?;
    out.sort_by_key(|c| severity(c.verdict));
    out.truncate(limit);
    Ok(out)
}

fn severity(v: Verdict) -> u8 {
    match v {
        Verdict::Missing => 0,
        Verdict::Refuted => 1,
        Verdict::Contradicted => 2,
        Verdict::Denied => 3,
        Verdict::Stale => 4,
        Verdict::Residue => 5,
        Verdict::Unrooted => 6,
        Verdict::Unverifiable => 7,
        Verdict::Rederived => 8,
        Verdict::Supported => 9,
    }
}

fn missing(uid: &str, detail: &str) -> ClaimCheck {
    ClaimCheck {
        fact_uid: uid.into(),
        statement: String::new(),
        predicate: String::new(),
        verdict: Verdict::Missing,
        detail: detail.into(),
        sought: None,
        supported_by: vec![],
        cited: vec![],
        observations: vec![],
        conflicts_with: vec![],
    }
}

fn check(conn: &Connection, f: &Fact) -> Result<ClaimCheck> {
    // ── Provenance dereference: every episode this claim cites.
    let mut cited: Vec<i64> = f.episode_id.into_iter().collect();
    {
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT episode_id FROM fact_observation
             WHERE fact_id = ?1 AND episode_id IS NOT NULL
               AND kind IN ('asserted','corroborated','verified')",
        )?;
        for id in stmt.query_map(params![f.id], |r| r.get::<_, i64>(0))? {
            let id = id?;
            if !cited.contains(&id) {
                cited.push(id);
            }
        }
    }

    // ── The lexical check: does the cited evidence contain the claim's
    // distinctive term? Object node name, else the literal value.
    let sought = match (&f.object_id, &f.object_value) {
        (Some(oid), _) => conn
            .query_row("SELECT name FROM nodes WHERE id = ?1", params![oid], |r| {
                r.get::<_, String>(0)
            })
            .ok(),
        (None, Some(v)) => Some(v.clone()),
        _ => None,
    };
    let mut supported_by = vec![];
    let mut alive = 0;
    for ep_id in &cited {
        let body: Option<String> = conn
            .query_row(
                "SELECT body FROM episode WHERE id = ?1",
                params![ep_id],
                |r| r.get(0),
            )
            .ok();
        let Some(body) = body else { continue }; // redacted
        alive += 1;
        if let Some(term) = &sought {
            if lexically_supports(&body, term) {
                supported_by.push(*ep_id);
            }
        }
    }

    // ── Observation trail (the how-known/how-verified story, V010).
    let observations: Vec<(String, i64)> = {
        let mut stmt = conn.prepare_cached(
            "SELECT kind, COUNT(*) FROM fact_observation WHERE fact_id = ?1
             GROUP BY kind ORDER BY kind",
        )?;
        let rows = stmt
            .query_map(params![f.id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };

    // ── Deterministic conflict checks (definitive — these are the ones
    // the tier is allowed to assert outright).
    let mut conflicts_with = vec![];
    if crate::precheck::SINGLE_VALUED.contains(&f.predicate.as_str()) && f.object_id.is_some() {
        let mut stmt = conn.prepare_cached(
            "SELECT uid FROM fact_current
             WHERE subject_id = ?1 AND predicate = ?2 AND object_id IS NOT NULL
               AND uid != ?3",
        )?;
        let peers: Vec<String> = stmt
            .query_map(params![f.subject_id, f.predicate, f.uid], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        if !peers.is_empty() {
            conflicts_with = peers;
            return Ok(ClaimCheck {
                verdict: Verdict::Contradicted,
                detail: format!(
                    "{} other live value(s) on single-valued '{}'",
                    conflicts_with.len(),
                    f.predicate
                ),
                ..assembled(
                    f,
                    sought,
                    supported_by,
                    cited,
                    observations,
                    conflicts_with.clone(),
                )
            });
        }
    }
    {
        let mut stmt = conn.prepare_cached(
            "SELECT uid FROM fact
             WHERE subject_id = ?1 AND predicate = ?2 AND polarity = 'negative'
               AND valid_to IS NULL AND invalidated_at IS NULL",
        )?;
        let negs: Vec<String> = stmt
            .query_map(params![f.subject_id, f.predicate], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        if !negs.is_empty() {
            return Ok(ClaimCheck {
                verdict: Verdict::Denied,
                detail: "a live negation coexists with this claim".into(),
                ..assembled(f, sought, supported_by, cited, observations, negs)
            });
        }
    }

    // ── Staleness (λ from V013, world-time clock — same rule as
    // flags::detect_staleness and the Selector).
    let stale: Option<f64> = conn
        .query_row(
            "SELECT p.lambda *
                    ((julianday('now') -
                      julianday(COALESCE(MAX(COALESCE(e.occurred_at, o.observed_at)),
                                         f.valid_from, f.ingested_at))) / 365.25)
             FROM fact f
             JOIN predicate p ON p.name = f.predicate
             LEFT JOIN fact_observation o
                    ON o.fact_id = f.id AND o.kind IN ('asserted','corroborated','verified')
             LEFT JOIN episode e ON e.id = o.episode_id
             WHERE f.id = ?1 AND p.lambda IS NOT NULL AND p.lambda > 0
             GROUP BY f.id",
            params![f.id],
            |r| r.get(0),
        )
        .ok();
    if stale.is_some_and(|s| s > std::f64::consts::LN_2) {
        return Ok(ClaimCheck {
            verdict: Verdict::Stale,
            detail: "past its predicate's half-life — re-verify before relying on it".into(),
            ..assembled(f, sought, supported_by, cited, observations, vec![])
        });
    }

    // ── Computed claims are re-derived, not read. For a fact produced by
    // a formula the strongest check is recomputation: exact instead of
    // lexical, and it catches a co-occurrence that has DECAYED below
    // threshold as the corpus grew — which no citation could reveal.
    if f.extractor.as_deref() == Some("npmi") {
        if let Some(oid) = &f.object_id {
            let (verdict, detail) = match rederive_npmi(conn, &f.subject_id, oid)? {
                Some((npmi, co)) if npmi >= crate::linkers::NPMI_THRESHOLD => (
                    Verdict::Rederived,
                    format!(
                        "recomputed from current mentions: NPMI {npmi:.2} over {co} \
                             shared episodes, still ≥ {:.2}",
                        crate::linkers::NPMI_THRESHOLD
                    ),
                ),
                Some((npmi, co)) => (
                    Verdict::Refuted,
                    format!(
                        "recomputed NPMI {npmi:.2} over {co} shared episodes has fallen \
                             below {:.2} — the derivation no longer holds",
                        crate::linkers::NPMI_THRESHOLD
                    ),
                ),
                None => (
                    Verdict::Refuted,
                    "the pair no longer co-occurs above the floor in the mention table".into(),
                ),
            };
            return Ok(ClaimCheck {
                verdict,
                detail,
                ..assembled(f, sought, supported_by, cited, observations, vec![])
            });
        }
    }

    // ── Evidence verdicts, in honesty order.
    let (verdict, detail) = if cited.is_empty() {
        (
            Verdict::Unrooted,
            "cites no evidence — nothing to dereference".to_string(),
        )
    } else if alive == 0 {
        (
            Verdict::Unverifiable,
            format!(
                "all {} cited episode(s) are gone (redacted) — uncheckable, not unsupported",
                cited.len()
            ),
        )
    } else if !supported_by.is_empty() {
        (
            Verdict::Supported,
            format!(
                "{}/{} live cited episode(s) contain \"{}\"",
                supported_by.len(),
                alive,
                sought.clone().unwrap_or_default()
            ),
        )
    } else if sought.is_none() {
        (
            Verdict::Residue,
            "no distinctive term to match — a model must read the evidence".to_string(),
        )
    } else {
        (
            Verdict::Residue,
            format!(
                "no cited episode literally contains \"{}\" — paraphrase or error, \
                  a model must read it (a miss is NOT a refutation)",
                sought.clone().unwrap_or_default()
            ),
        )
    };
    Ok(ClaimCheck {
        verdict,
        detail,
        ..assembled(f, sought, supported_by, cited, observations, vec![])
    })
}

#[allow(clippy::too_many_arguments)]
fn assembled(
    f: &Fact,
    sought: Option<String>,
    supported_by: Vec<i64>,
    cited: Vec<i64>,
    observations: Vec<(String, i64)>,
    conflicts_with: Vec<String>,
) -> ClaimCheck {
    ClaimCheck {
        fact_uid: f.uid.clone(),
        statement: f.statement.clone(),
        predicate: f.predicate.clone(),
        verdict: Verdict::Residue, // overwritten by every caller
        detail: String::new(),
        sought,
        supported_by,
        cited,
        observations,
        conflicts_with,
    }
}

/// Recompute NPMI for a pair from the CURRENT mention table, using the
/// same formula and universe as `linkers::link_npmi` (N = episodes with
/// at least one mention). Returns `(npmi, co_occurrences)`, or None when
/// the pair no longer clears `NPMI_MIN_COOCCUR`.
pub fn rederive_npmi(conn: &Connection, a: &str, b: &str) -> Result<Option<(f64, i64)>> {
    let n_total: i64 =
        conn.query_row("SELECT COUNT(DISTINCT episode_id) FROM mention", [], |r| {
            r.get(0)
        })?;
    if n_total < 10 {
        return Ok(None);
    }
    let co = crate::linkers::shared_episodes(conn, a, b)?.len() as i64;
    if co < crate::linkers::NPMI_MIN_COOCCUR {
        return Ok(None);
    }
    let marginal = |id: &str| -> Result<i64> {
        Ok(conn.query_row(
            "SELECT COUNT(DISTINCT episode_id) FROM mention WHERE node_id = ?1",
            params![id],
            |r| r.get(0),
        )?)
    };
    let (ca, cb) = (marginal(a)?, marginal(b)?);
    if ca == 0 || cb == 0 {
        return Ok(None);
    }
    let n = n_total as f64;
    let p_ab = co as f64 / n;
    let (p_a, p_b) = (ca as f64 / n, cb as f64 / n);
    let pmi = (p_ab / (p_a * p_b)).ln();
    Ok(Some((pmi / -(p_ab.ln()), co)))
}

/// Lexical support: the term's tokens must appear as a **contiguous
/// whole-word run** in the body. Case- and punctuation-insensitive, so
/// "Sigma Lab" matches "SIGMA-LAB retreat" and "Westfield" matches
/// "Company: Westfield".
///
/// Word boundaries, not substrings, and that asymmetry is deliberate:
/// a false `Supported` makes the Verifier vouch for something it hasn't
/// checked, which *suppresses a real finding* — the expensive error. A
/// false `Residue` only costs one model call. So "sigmalab" does not
/// vouch for "Sigma Lab"; it hands off. (Caught by the test below,
/// which the first substring implementation passed for the wrong
/// reason.)
fn lexically_supports(body: &str, term: &str) -> bool {
    let toks = |s: &str| -> Vec<String> {
        s.chars()
            .map(|c| {
                if c.is_alphanumeric() {
                    c.to_ascii_lowercase()
                } else {
                    ' '
                }
            })
            .collect::<String>()
            .split_whitespace()
            .map(str::to_string)
            .collect()
    };
    let (b, t) = (toks(body), toks(term));
    if t.is_empty() || b.len() < t.len() {
        return false;
    }
    b.windows(t.len()).any(|w| w == t.as_slice())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{upsert_episode, Episode};
    use crate::fact::{assert_fact, assert_negative_fact};
    use crate::graph::{upsert_node, Node};

    fn ep(conn: &Connection, sid: &str, body: &str, at: &str) -> i64 {
        upsert_episode(
            conn,
            &Episode {
                id: 0,
                uid: String::new(),
                source: "note".into(),
                source_id: sid.into(),
                source_ref: None,
                body: body.into(),
                occurred_at: at.into(),
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
        .unwrap()
        .0
    }

    fn nodes(conn: &Connection) {
        for (id, ty, name) in [
            ("person-a", "person", "Ada"),
            ("org-u", "org", "Westfield"),
            ("org-c", "org", "Sigma Lab"),
        ] {
            upsert_node(conn, &Node::new(id, ty, name)).unwrap();
        }
    }

    #[test]
    fn test_provenance_deref_supports_a_grounded_claim() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // The real shape from the live graph: a reflect note whose body
        // carries "Company: Westfield".
        let e = ep(
            &conn,
            "n1",
            "# Iris Calder - Company: Westfield - Type: #person",
            &crate::ids::now(),
        );
        let uid = assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("org-u"),
            None,
            "Ada works at Westfield",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();

        let c = verify_fact(&conn, &uid).unwrap();
        assert_eq!(c.verdict, Verdict::Supported);
        assert_eq!(c.supported_by, vec![e]);
        assert_eq!(c.sought.as_deref(), Some("Westfield"));
        assert_eq!(c.observations, vec![("asserted".to_string(), 1)]);
    }

    #[test]
    fn test_paraphrase_is_residue_never_refutation() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // Evidence says "Sigmalab"; the object node is "Sigma Lab".
        let e = ep(
            &conn,
            "n1",
            "the sigmalab meeting ran long",
            &crate::ids::now(),
        );
        let uid = assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("org-c"),
            None,
            "Ada is a member of Sigma Lab",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();

        let c = verify_fact(&conn, &uid).unwrap();
        assert_eq!(
            c.verdict,
            Verdict::Residue,
            "a lexical miss hands off, never refutes"
        );
        assert!(c.detail.contains("NOT a refutation"));
        assert_eq!(c.cited, vec![e], "the ref still dereferenced");
    }

    #[test]
    fn test_token_match_bridges_casing_and_punctuation() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let e = ep(
            &conn,
            "n1",
            "met at the SIGMA-LAB retreat",
            &crate::ids::now(),
        );
        let uid = assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("org-c"),
            None,
            "Ada is a member of Sigma Lab",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_eq!(
            verify_fact(&conn, &uid).unwrap().verdict,
            Verdict::Supported
        );
    }

    #[test]
    fn test_redaction_takes_the_derived_claim_with_it() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let e = ep(&conn, "n1", "Ada joined Westfield", &crate::ids::now());
        let uid = assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("org-u"),
            None,
            "Ada works at Westfield",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        let ep_uid: String = conn
            .query_row("SELECT uid FROM episode WHERE id = ?1", params![e], |r| {
                r.get(0)
            })
            .unwrap();
        crate::episode::redact_episode(&conn, &ep_uid).unwrap();

        // §10 redaction cascades to derived facts, so the claim is gone
        // rather than dangling — a privacy delete that left the belief
        // behind would not be a delete. Verified here so the Verifier's
        // contract with redaction is pinned.
        assert_eq!(verify_fact(&conn, &uid).unwrap().verdict, Verdict::Missing);
    }

    #[test]
    fn test_dangling_evidence_ref_is_unverifiable_not_unsupported() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let e = ep(&conn, "n1", "Ada joined Westfield", &crate::ids::now());
        let uid = assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("org-u"),
            None,
            "Ada works at Westfield",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        // A live claim citing evidence that is no longer there. FK
        // constraints prevent this today, so it is a defensive branch for
        // legacy/corrupt rows — constructed here with FKs off, since the
        // whole point is that the Verifier must not call it "unsupported".
        conn.execute_batch("PRAGMA foreign_keys=off;").unwrap();
        conn.execute("DELETE FROM episode WHERE id = ?1", params![e])
            .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=on;").unwrap();

        let c = verify_fact(&conn, &uid).unwrap();
        assert_eq!(c.verdict, Verdict::Unverifiable);
        assert!(c.detail.contains("not unsupported"));
    }

    #[test]
    fn test_conflicts_outrank_evidence_verdicts() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let e = ep(&conn, "n1", "Ada works at Westfield", &crate::ids::now());
        let uid = assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-u"),
            None,
            "Ada works at Westfield",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        // Well-supported AND contradicted: the contradiction is the finding.
        assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-c"),
            None,
            "Ada works at Sigma Lab",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();

        let c = verify_fact(&conn, &uid).unwrap();
        assert_eq!(c.verdict, Verdict::Contradicted);
        assert_eq!(c.conflicts_with.len(), 1);

        // A live negation reports Denied on a predicate with no
        // single-valued conflict.
        let uid2 = assert_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("org-u"),
            None,
            "Ada collaborates with Westfield",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_negative_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("org-u"),
            None,
            "Ada does NOT collaborate with Westfield",
            Some(e),
            0.9,
            "user",
        )
        .unwrap();
        assert_eq!(verify_fact(&conn, &uid2).unwrap().verdict, Verdict::Denied);
    }

    /// n episodes mentioning both a and b, plus padding so N ≥ 10. The
    /// padding uses a THIRD node deliberately: padding with `a` would make
    /// it appear in every episode, and NPMI is built to punish exactly
    /// that hub shape — it scores such a pair 0.0, correctly.
    fn cooccur(conn: &Connection, a: &str, b: &str, shared: usize) {
        upsert_node(conn, &Node::new("person-pad", "person", "Pad")).unwrap();
        for i in 0..shared {
            let e = ep(conn, &format!("s{i}"), "shared", "2026-01-01 10:00:00");
            crate::episode::add_mention(conn, e, a, "manual", 1.0).unwrap();
            crate::episode::add_mention(conn, e, b, "manual", 1.0).unwrap();
        }
        for i in 0..12 {
            let e = ep(conn, &format!("p{i}"), "pad", "2026-01-02 10:00:00");
            crate::episode::add_mention(conn, e, "person-pad", "manual", 1.0).unwrap();
        }
    }

    #[test]
    fn test_computed_claim_is_rederived_not_read() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        upsert_node(&conn, &Node::new("person-b", "person", "Bo")).unwrap();
        cooccur(&conn, "person-a", "person-b", 5);
        let uid = assert_fact(
            &conn,
            "person-a",
            "related_to",
            Some("person-b"),
            None,
            "Ada and Bo frequently co-occur (5 shared episodes, NPMI 0.42)",
            None,
            None,
            0.42,
            "npmi",
        )
        .unwrap();

        let c = verify_fact(&conn, &uid).unwrap();
        assert_eq!(
            c.verdict,
            Verdict::Rederived,
            "a formula-derived claim is recomputed, not lexically read"
        );
        assert!(c.detail.contains("recomputed from current mentions"));
    }

    #[test]
    fn test_decayed_cooccurrence_is_refuted_deterministically() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        upsert_node(&conn, &Node::new("person-b", "person", "Bo")).unwrap();
        cooccur(&conn, "person-a", "person-b", 3);
        let uid = assert_fact(
            &conn,
            "person-a",
            "related_to",
            Some("person-b"),
            None,
            "Ada and Bo frequently co-occur (3 shared episodes, NPMI 0.55)",
            None,
            None,
            0.55,
            "npmi",
        )
        .unwrap();
        assert_eq!(
            verify_fact(&conn, &uid).unwrap().verdict,
            Verdict::Rederived
        );

        // The corpus grows: Bo now appears everywhere, so the pair's
        // association is no longer distinctive. Citation could never show
        // this — only recomputation can.
        for i in 0..40 {
            let e = ep(&conn, &format!("g{i}"), "growth", "2026-02-01 10:00:00");
            crate::episode::add_mention(&conn, e, "person-b", "manual", 1.0).unwrap();
        }
        let c = verify_fact(&conn, &uid).unwrap();
        assert_eq!(c.verdict, Verdict::Refuted);
        assert!(c.detail.contains("no longer holds"));
    }

    #[test]
    fn test_superseded_ref_is_missing_and_node_report_sorts_by_severity() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        let e = ep(&conn, "n1", "Ada works at Westfield", &crate::ids::now());
        let gone = assert_fact(
            &conn,
            "person-a",
            "uses",
            None,
            Some("vim"),
            "Ada uses vim",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        fact::supersede_fact(&conn, &gone, None).unwrap();
        assert_eq!(verify_fact(&conn, &gone).unwrap().verdict, Verdict::Missing);

        // Two live claims: one contradicted, one supported.
        assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-u"),
            None,
            "Ada works at Westfield",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "works_at",
            Some("org-c"),
            None,
            "Ada works at Sigma Lab",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("org-u"),
            None,
            "Ada is a member of Westfield",
            Some(e),
            None,
            0.9,
            "test",
        )
        .unwrap();

        let report = verify_node(&conn, "person-a", 10).unwrap();
        assert_eq!(report[0].verdict, Verdict::Contradicted, "findings first");
        assert!(
            report.iter().all(|c| c.fact_uid != gone),
            "superseded facts aren't live claims"
        );
    }
}
