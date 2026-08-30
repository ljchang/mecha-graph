//! Hand-written typed closure rules (PLAN mechanism #9) — the plumbing
//! AMIE-style rule *mining* will later pour into.
//!
//! A rule is a Horn clause over `fact_current`, e.g.
//! `authored(A,D) ∧ authored(B,D) → collaborates_with(A,B)`. Each
//! proposal arrives in the review queue with **the rule and its
//! grounding as evidence** — exactly what a review card wants to show —
//! and each rule is its own proposer class (`rule:<name>`), so the
//! class ledger prices every rule separately: a rule that keeps being
//! rejected demotes alone, without dragging the others down (the
//! per-rule ledger PLAN.md asks for, free from V012).
//!
//! Guards, in order:
//! - **live negation blocks re-proposal** — the first real consumer of
//!   V013 rejection memory: "X does NOT collaborate with Y" stops the
//!   rule re-asking, forever, without a human seeing it twice;
//! - an existing live fact (either direction when symmetric) is not
//!   re-proposed;
//! - any prior candidate from the same rule on the same pair — proposed
//!   (dup) or rejected (asked and answered) — is not re-proposed;
//! - per-rule cap per run (D1: a new rule cannot flood the queue).
//!
//! Rules only STAGE. Nothing here writes to `fact` directly; the
//! autonomy ladder decides per class what staging means over time.

use rusqlite::{params, Connection};

use crate::error::Result;
use crate::fact::{self, ProposedFact};

/// Per-rule stage cap per run.
pub const RULE_CAP: usize = 25;

/// One typed closure rule. `sql` yields rows
/// `(subject_id, object_id, grounding)` — grounding is the human-facing
/// evidence string rendered into the candidate statement.
pub struct Rule {
    pub name: &'static str,
    /// Head predicate of proposals.
    pub predicate: &'static str,
    /// Symmetric heads dedupe on unordered pairs and check both
    /// directions for existing facts/negations.
    pub symmetric: bool,
    pub confidence: f64,
    pub sql: &'static str,
}

/// The hand-written seed set (3–5 per PLAN; mining discovers more
/// later, staged for review like facts). All grounded in the settled
/// predicate semantics: collaborates_with = "co-authorship or active
/// not-yet-published projects" is definitional, the closures are
/// plausible-not-certain and priced accordingly.
///
/// Every body atom carries `extractor NOT LIKE 'rule:%'` — the
/// feedback-loop guard RESEARCH_LOOP.md §rule-mining specifies: support
/// must root in raw evidence, never in another rule's output. Without it
/// the set contains a closed cycle — `works_on ∧ works_on →
/// collaborates_with` (rule 2) and `collaborates_with ∧ works_on →
/// works_on` (rule 4) — so each accepted proposal widened the body set
/// for the next night's run: a transitive-closure engine feeding itself,
/// which is what took `collaborators-share-projects` to 2 accepts
/// against 43 rejects. Excluding all rule-derived facts (not just the
/// same rule's) is deliberate: the doc's per-rule guard misses exactly
/// this two-rule cycle.
pub const RULES: &[Rule] = &[
    Rule {
        name: "coauthors-collaborate",
        predicate: "collaborates_with",
        symmetric: true,
        confidence: 0.7,
        // authored(A,D) ∧ authored(B,D) → collaborates_with(A,B)
        sql: "SELECT a.subject_id, b.subject_id,
                     'both authored ' || COALESCE(d.name, a.object_id)
              FROM fact_current a
              JOIN fact_current b
                ON b.predicate = 'authored' AND b.object_id = a.object_id
               AND b.subject_id > a.subject_id
              JOIN nodes pa ON pa.id = a.subject_id AND pa.node_type = 'person'
              JOIN nodes pb ON pb.id = b.subject_id AND pb.node_type = 'person'
              LEFT JOIN nodes d ON d.id = a.object_id
              WHERE a.predicate = 'authored' AND a.object_id IS NOT NULL
                AND a.extractor NOT LIKE 'rule:%' AND b.extractor NOT LIKE 'rule:%'",
    },
    Rule {
        name: "project-mates-collaborate",
        predicate: "collaborates_with",
        symmetric: true,
        confidence: 0.6,
        // works_on(A,P) ∧ works_on(B,P) → collaborates_with(A,B)
        sql: "SELECT a.subject_id, b.subject_id,
                     'both work on ' || COALESCE(p.name, a.object_id)
              FROM fact_current a
              JOIN fact_current b
                ON b.predicate = 'works_on' AND b.object_id = a.object_id
               AND b.subject_id > a.subject_id
              JOIN nodes pa ON pa.id = a.subject_id AND pa.node_type = 'person'
              JOIN nodes pb ON pb.id = b.subject_id AND pb.node_type = 'person'
              JOIN nodes p ON p.id = a.object_id AND p.node_type = 'project'
              WHERE a.predicate = 'works_on' AND a.object_id IS NOT NULL
                AND a.extractor NOT LIKE 'rule:%' AND b.extractor NOT LIKE 'rule:%'",
    },
    Rule {
        name: "mentees-join-the-lab",
        predicate: "member_of",
        symmetric: false,
        confidence: 0.55,
        // mentors(A,X) ∧ member_of(A,L) → member_of(X,L)
        //
        // L must be an ORG. The untyped form read "L is anything A is a
        // member of" — the owner's societies, committees and Slack
        // workspaces all qualified, so every mentee was proposed into all
        // of them (0 accepts over 9 asks). An inner join on node_type
        // narrows L to the reading the rule's name promises.
        sql: "SELECT m.object_id, l.object_id,
                     COALESCE(na.name, m.subject_id) || ' mentors them and belongs to '
                     || nl.name
              FROM fact_current m
              JOIN fact_current l
                ON l.subject_id = m.subject_id AND l.predicate = 'member_of'
               AND l.object_id IS NOT NULL
              JOIN nodes px ON px.id = m.object_id AND px.node_type = 'person'
              LEFT JOIN nodes na ON na.id = m.subject_id
              JOIN nodes nl ON nl.id = l.object_id AND nl.node_type = 'org'
              WHERE m.predicate = 'mentors' AND m.object_id IS NOT NULL
                AND m.extractor NOT LIKE 'rule:%' AND l.extractor NOT LIKE 'rule:%'",
    },
    Rule {
        name: "collaborators-share-projects",
        predicate: "works_on",
        symmetric: false,
        confidence: 0.5,
        // collaborates_with(A,B) ∧ works_on(A,P) → works_on(B,P)
        // (collaborates_with is stored one direction; match both.)
        sql: "SELECT CASE WHEN w.subject_id = c.subject_id
                          THEN c.object_id ELSE c.subject_id END,
                     w.object_id,
                     'collaborates with ' || COALESCE(nw.name, w.subject_id)
                     || ', who works on ' || COALESCE(np.name, w.object_id)
              FROM fact_current c
              JOIN fact_current w
                ON w.predicate = 'works_on' AND w.object_id IS NOT NULL
               AND w.subject_id IN (c.subject_id, c.object_id)
              JOIN nodes p ON p.id = w.object_id AND p.node_type = 'project'
              LEFT JOIN nodes nw ON nw.id = w.subject_id
              LEFT JOIN nodes np ON np.id = w.object_id
              WHERE c.predicate = 'collaborates_with' AND c.object_id IS NOT NULL
                AND c.extractor NOT LIKE 'rule:%' AND w.extractor NOT LIKE 'rule:%'",
    },
];

/// Run every rule; returns candidates staged per rule (by name), for the
/// link report. Deterministic, CPU-only — runs in the nightly cascade.
pub fn run_rules(conn: &Connection) -> Result<Vec<(&'static str, usize)>> {
    let mut out = vec![];
    for rule in RULES {
        out.push((rule.name, run_rule(conn, rule)?));
    }
    Ok(out)
}

fn run_rule(conn: &Connection, rule: &Rule) -> Result<usize> {
    let derivations: Vec<(String, String, String)> = {
        let mut stmt = conn.prepare(rule.sql)?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };

    let mut staged = 0;
    let mut seen: std::collections::HashSet<(String, String)> = Default::default();
    for (subj, obj, grounding) in derivations {
        if subj == obj {
            continue;
        }
        // One proposal per pair even when several groundings derive it
        // (two co-authored papers → one collaborates_with candidate).
        let pair = if rule.symmetric && obj < subj {
            (obj.clone(), subj.clone())
        } else {
            (subj.clone(), obj.clone())
        };
        if !seen.insert(pair) {
            continue;
        }

        let name = |id: &str| -> Result<String> {
            Ok(conn
                .query_row("SELECT name FROM nodes WHERE id = ?1", params![id], |r| {
                    r.get(0)
                })
                .unwrap_or_else(|_| id.to_string()))
        };
        let (ns, no) = (name(&subj)?, name(&obj)?);

        if fact_or_negation_exists(conn, rule, &subj, &obj)?
            || already_asked(conn, rule, &subj, &obj, &ns, &no)?
        {
            continue;
        }

        // Names in subject/object, ids only in the SQL above — the same bug
        // commit 24fa9be fixed for linker:knn was never fixed here:
        // `ProposedFact.subject` is what accept hands to `resolve_entity`,
        // which has no tier that reads `nodes.id`, so every rule candidate
        // failed on accept with "cannot resolve subject 'person-<uuid>'".
        // Unacceptable-by-construction — some fraction of the rules' reject
        // record is plausibly the owner clearing items that could not be
        // accepted at all.
        fact::propose_fact(
            conn,
            &ProposedFact {
                subject: ns.clone(),
                predicate: rule.predicate.into(),
                object: Some(no.clone()),
                object_value: None,
                statement: format!(
                    "{ns} {} {no} — rule {}: {grounding}",
                    rule.predicate.replace('_', " "),
                    rule.name
                ),
                valid_from: None,
                confidence: Some(rule.confidence),
                tags: None,
                subject_node: Some(subj.clone()),
                object_node: Some(obj.clone()),
            },
            &format!("rule:{}", rule.name),
            None,
        )?;
        staged += 1;
        if staged >= RULE_CAP {
            break;
        }
    }
    Ok(staged)
}

/// A live fact satisfies the head (nothing to propose), or a live
/// negation denies it (rejection memory: never re-ask). Symmetric heads
/// check both directions.
fn fact_or_negation_exists(conn: &Connection, rule: &Rule, subj: &str, obj: &str) -> Result<bool> {
    let sql = if rule.symmetric {
        "SELECT COUNT(*) > 0 FROM fact
         WHERE predicate = ?1 AND valid_to IS NULL AND invalidated_at IS NULL
           AND ((subject_id = ?2 AND object_id = ?3)
             OR (subject_id = ?3 AND object_id = ?2))"
    } else {
        "SELECT COUNT(*) > 0 FROM fact
         WHERE predicate = ?1 AND valid_to IS NULL AND invalidated_at IS NULL
           AND subject_id = ?2 AND object_id = ?3"
    };
    Ok(conn.query_row(sql, params![rule.predicate, subj, obj], |r| r.get(0))?)
}

/// Any prior candidate proposing this HEAD on this pair — pending (dup) or
/// already judged (asked and answered) — blocks a re-proposal.
///
/// Keyed on (head predicate, pair) across ALL `rule:%` proposers, never on
/// the rule's own name: `coauthors-collaborate` and
/// `project-mates-collaborate` both emit `collaborates_with`, so a
/// per-rule scope let a pair the owner rejected under one rule re-enter
/// verbatim under its sibling the next night — the same question, asked
/// again because a different mechanism asked it. Rejection writes no
/// negation fact, so nothing else stands in the way.
///
/// Three matching arms, one per payload generation (same scheme as
/// `linkers::linker_pair_asked`): node ids (exact, collision-free — names
/// are not unique), names only for the id-less cohort, and `LIKE` over
/// uuids for the id-era rows.
fn already_asked(
    conn: &Connection,
    rule: &Rule,
    subj: &str,
    obj: &str,
    subj_name: &str,
    obj_name: &str,
) -> Result<bool> {
    Ok(conn.query_row(
        "SELECT COUNT(*) > 0 FROM fact_candidate
         WHERE proposed_by LIKE 'rule:%'
           AND json_extract(payload, '$.predicate') = ?1
           AND ((json_extract(payload, '$.subject_node') IN (?2, ?3)
                 AND json_extract(payload, '$.object_node') IN (?2, ?3))
             OR (json_extract(payload, '$.subject_node') IS NULL
                 AND json_extract(payload, '$.subject') IN (?4, ?5)
                 AND json_extract(payload, '$.object') IN (?4, ?5))
             OR (payload LIKE '%' || ?2 || '%' AND payload LIKE '%' || ?3 || '%'))",
        params![rule.predicate, subj, obj, subj_name, obj_name],
        |r| r.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::fact::{assert_fact, assert_negative_fact, pending_candidates};
    use crate::graph::{upsert_node, Node};

    fn nodes(conn: &Connection) {
        for (id, ty, name) in [
            ("person-a", "person", "Ada"),
            ("person-b", "person", "Bo"),
            ("person-c", "person", "Cy"),
            ("doc-1", "document", "Neural Paper"),
            ("project-p", "project", "Hypercourse"),
            ("org-lab", "org", "Sigma Lab"),
        ] {
            upsert_node(conn, &Node::new(id, ty, name)).unwrap();
        }
    }

    #[test]
    fn test_coauthors_propose_collaboration_with_grounding() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        assert_fact(
            &conn,
            "person-a",
            "authored",
            Some("doc-1"),
            None,
            "Ada authored Neural Paper",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-b",
            "authored",
            Some("doc-1"),
            None,
            "Bo authored Neural Paper",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        let report = run_rules(&conn).unwrap();
        let n = report
            .iter()
            .find(|(r, _)| *r == "coauthors-collaborate")
            .unwrap()
            .1;
        assert_eq!(n, 1);
        let c = &pending_candidates(&conn, 10).unwrap()[0];
        assert_eq!(c.proposed_by.as_deref(), Some("rule:coauthors-collaborate"));
        assert_eq!(c.payload["predicate"], "collaborates_with");
        let stmt = c.payload["statement"].as_str().unwrap();
        assert!(
            stmt.contains("rule coauthors-collaborate"),
            "rule named in evidence"
        );
        assert!(stmt.contains("Neural Paper"), "grounding named in evidence");

        // Idempotent: the same derivation does not re-stage.
        let report = run_rules(&conn).unwrap();
        assert_eq!(
            report
                .iter()
                .find(|(r, _)| *r == "coauthors-collaborate")
                .unwrap()
                .1,
            0
        );
    }

    #[test]
    fn test_negation_is_rejection_memory() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        assert_fact(
            &conn,
            "person-a",
            "works_on",
            Some("project-p"),
            None,
            "Ada works on Hypercourse",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-b",
            "works_on",
            Some("project-p"),
            None,
            "Bo works on Hypercourse",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        // The owner already said no — note the REVERSED direction: the
        // symmetric check must still see it.
        assert_negative_fact(
            &conn,
            "person-b",
            "collaborates_with",
            Some("person-a"),
            None,
            "Bo does NOT collaborate with Ada",
            None,
            0.9,
            "user",
        )
        .unwrap();

        let report = run_rules(&conn).unwrap();
        assert_eq!(
            report
                .iter()
                .find(|(r, _)| *r == "project-mates-collaborate")
                .unwrap()
                .1,
            0,
            "a live negation blocks the rule from re-asking"
        );
    }

    #[test]
    fn test_existing_fact_not_reproposed_and_rejection_remembered() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        assert_fact(
            &conn,
            "person-a",
            "mentors",
            Some("person-c"),
            None,
            "Ada mentors Cy",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("org-lab"),
            None,
            "Ada is a member of Sigma Lab",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        let report = run_rules(&conn).unwrap();
        assert_eq!(
            report
                .iter()
                .find(|(r, _)| *r == "mentees-join-the-lab")
                .unwrap()
                .1,
            1
        );

        // Reject it — the rule must not ask again next run.
        let id = pending_candidates(&conn, 10).unwrap()[0].id;
        fact::reject_candidate(&conn, id, "not in the lab").unwrap();
        let report = run_rules(&conn).unwrap();
        assert_eq!(
            report
                .iter()
                .find(|(r, _)| *r == "mentees-join-the-lab")
                .unwrap()
                .1,
            0,
            "a rejected candidate is asked-and-answered"
        );
    }

    #[test]
    fn test_collaborator_project_closure_both_directions() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // Stored direction: A collaborates_with B; only B works on P.
        assert_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("person-b"),
            None,
            "Ada collaborates with Bo",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-b",
            "works_on",
            Some("project-p"),
            None,
            "Bo works on Hypercourse",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        run_rules(&conn).unwrap();
        let cands = pending_candidates(&conn, 10).unwrap();
        let closure: Vec<_> = cands
            .iter()
            .filter(|c| c.proposed_by.as_deref() == Some("rule:collaborators-share-projects"))
            .collect();
        assert_eq!(closure.len(), 1);
        // NAMES, not node ids: the payload fields are what accept hands to
        // resolve_entity, which cannot read an id.
        assert_eq!(
            closure[0].payload["subject"], "Ada",
            "the closure lands on the collaborator who lacks the edge"
        );
        assert_eq!(closure[0].payload["object"], "Hypercourse");
    }

    #[test]
    fn a_pair_rejected_under_one_rule_is_asked_for_all_rules_with_that_head() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // Ada and Bo co-author a document → coauthors-collaborate proposes
        // collaborates_with(Ada, Bo); the owner rejects it.
        assert_fact(
            &conn,
            "person-a",
            "authored",
            Some("doc-1"),
            None,
            "Ada authored the Neural Paper",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-b",
            "authored",
            Some("doc-1"),
            None,
            "Bo authored the Neural Paper",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        run_rules(&conn).unwrap();
        let cand = pending_candidates(&conn, 10)
            .unwrap()
            .into_iter()
            .find(|c| c.proposed_by.as_deref() == Some("rule:coauthors-collaborate"))
            .expect("coauthors proposes the pair");
        fact::reject_candidate(&conn, cand.id, "they are not collaborators").unwrap();

        // Now the sibling rule derives the SAME head on the SAME pair from
        // different evidence. Per-rule scoping used to let it re-ask the
        // question the owner just answered.
        assert_fact(
            &conn,
            "person-a",
            "works_on",
            Some("project-p"),
            None,
            "Ada works on Hypercourse",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-b",
            "works_on",
            Some("project-p"),
            None,
            "Bo works on Hypercourse",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        run_rules(&conn).unwrap();
        assert!(
            !pending_candidates(&conn, 10)
                .unwrap()
                .iter()
                .any(|c| c.proposed_by.as_deref() == Some("rule:project-mates-collaborate")),
            "the same head on the same pair is asked-and-answered, whoever asks"
        );
    }

    #[test]
    fn rule_output_never_feeds_a_rule_body() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // A collaborates_with B *derived by a rule*, and B works on P.
        // Without the extractor guard, collaborators-share-projects reads
        // its sibling's output as support and the closure cascades — each
        // accepted proposal widening the body set for the next night.
        assert_fact(
            &conn,
            "person-a",
            "collaborates_with",
            Some("person-b"),
            None,
            "Ada collaborates with Bo",
            None,
            None,
            0.9,
            "rule:coauthors-collaborate",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-b",
            "works_on",
            Some("project-p"),
            None,
            "Bo works on Hypercourse",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();

        run_rules(&conn).unwrap();
        let cands = pending_candidates(&conn, 10).unwrap();
        assert!(
            !cands
                .iter()
                .any(|c| c.proposed_by.as_deref() == Some("rule:collaborators-share-projects")),
            "support must root in raw evidence, never in another rule's output"
        );
    }

    #[test]
    fn mentee_closure_needs_an_org_shaped_lab() {
        let conn = open_memory().unwrap();
        nodes(&conn);
        // Ada mentors Cy; Ada is a member of a PROJECT — not a lab. The
        // untyped form proposed Cy into anything Ada belonged to.
        assert_fact(
            &conn,
            "person-a",
            "mentors",
            Some("person-c"),
            None,
            "Ada mentors Cy",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-a",
            "member_of",
            Some("project-p"),
            None,
            "Ada is on Hypercourse",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        run_rules(&conn).unwrap();
        assert!(
            !pending_candidates(&conn, 10)
                .unwrap()
                .iter()
                .any(|c| c.proposed_by.as_deref() == Some("rule:mentees-join-the-lab")),
            "membership in a non-org confers nothing"
        );

        // The same shape against an actual org DOES fire.
        assert_fact(
            &conn,
            "person-b",
            "mentors",
            Some("person-c"),
            None,
            "Bo mentors Cy",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        assert_fact(
            &conn,
            "person-b",
            "member_of",
            Some("org-lab"),
            None,
            "Bo is in Sigma Lab",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        run_rules(&conn).unwrap();
        let hits: Vec<_> = pending_candidates(&conn, 10)
            .unwrap()
            .into_iter()
            .filter(|c| c.proposed_by.as_deref() == Some("rule:mentees-join-the-lab"))
            .collect();
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].payload["subject"], "Cy");
        assert_eq!(hits[0].payload["object"], "Sigma Lab");
    }
}
