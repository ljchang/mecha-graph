//! Entity maintenance proposals — the queue the fact layer has always had
//! and the entity layer never did.
//!
//! Facts are proposed by a named class, queued, reviewed, and promoted up an
//! autonomy ladder on their human accept rate. Entities got none of it:
//! creating, renaming, merging, splitting and retyping were all hand
//! surgery. The cost was measured on 2026-08-24 — a first-name alias had
//! quietly moved one person's decade of history onto another person's node,
//! a fuzzy substring match had made an event the subject of every fact about
//! a toddler, and one person existed twice. All three had been true for
//! years, and nothing in the system was capable of noticing.
//!
//! Three rules carry the design:
//!
//! - **Detectors propose; they never repair.** The repair *direction* is
//!   usually not derivable from the data. A detector can see that a node
//!   with a student's email address carries a thousand mentions from
//!   somebody's kitchen conversations; it cannot know which of the two
//!   people should keep the node. That is a question for whoever knows the
//!   family, which is what a review queue is for.
//! - **A decided proposal is never re-proposed.** The unique index plus
//!   `INSERT OR IGNORE` means the nightly can re-run every detector over the
//!   whole graph and say nothing new. A rejection is therefore durable —
//!   re-deriving a refused change every night is how a queue becomes noise
//!   nobody reads, which is the state that let 6,434 items pile up in the
//!   fact queue.
//! - **`detector` is the class**, named the way `(proposer, predicate)` is
//!   for facts, so this queue can ride the same Wilson-bound ladder later
//!   without inventing a second notion of what a class is.

use crate::error::Result;
use crate::graph::{self, canonical_collision_free_name};
use rusqlite::{params, Connection};
use serde::Serialize;

/// One proposal, as stored.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Proposal {
    pub id: i64,
    pub detector: String,
    /// merge | retype | rename | reattribute | review
    pub kind: String,
    pub subject_id: String,
    pub subject_name: String,
    /// The second node, for merge/reattribute. Empty when there is none.
    pub other_id: String,
    pub other_name: String,
    /// JSON payload: `{"to_type":"org"}`, `{"new_name":"…"}`, …
    pub payload: Option<String>,
    pub evidence: String,
    pub score: Option<f64>,
    pub status: String,
}

impl Proposal {
    /// The value a payload key holds, if the payload parses and has it.
    pub fn payload_str(&self, key: &str) -> Option<String> {
        let raw = self.payload.as_deref()?;
        let v: serde_json::Value = serde_json::from_str(raw).ok()?;
        v.get(key)?.as_str().map(str::to_string)
    }
}

/// File a proposal. Idempotent: one already on file — whatever its status —
/// wins, so a nightly re-run adds nothing and a rejection stays rejected.
#[allow(clippy::too_many_arguments)]
pub fn propose(
    conn: &Connection,
    detector: &str,
    kind: &str,
    subject_id: &str,
    other_id: &str,
    payload: Option<&str>,
    evidence: &str,
    score: f64,
) -> Result<bool> {
    let n = conn.execute(
        "INSERT OR IGNORE INTO entity_proposal
           (detector, kind, subject_id, other_id, payload, evidence, score)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![detector, kind, subject_id, other_id, payload, evidence, score],
    )?;
    Ok(n > 0)
}

/// Pending proposals, strongest first. `detector` filters to one class.
pub fn pending(conn: &Connection, detector: Option<&str>, limit: i64) -> Result<Vec<Proposal>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.detector, p.kind, p.subject_id,
                COALESCE(s.name, p.subject_id), p.other_id, COALESCE(o.name, ''),
                p.payload, p.evidence, p.score, p.status
         FROM entity_proposal p
         LEFT JOIN nodes s ON s.id = p.subject_id
         LEFT JOIN nodes o ON o.id = p.other_id
         WHERE p.status = 'pending' AND (?1 = '' OR p.detector = ?1)
         ORDER BY p.score DESC, p.id
         LIMIT ?2",
    )?;
    let rows = stmt
        .query_map(params![detector.unwrap_or_default(), limit], |r| {
            Ok(Proposal {
                id: r.get(0)?,
                detector: r.get(1)?,
                kind: r.get(2)?,
                subject_id: r.get(3)?,
                subject_name: r.get(4)?,
                other_id: r.get(5)?,
                other_name: r.get(6)?,
                payload: r.get(7)?,
                evidence: r.get(8)?,
                score: r.get(9)?,
                status: r.get(10)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// One proposal by id, whatever its status.
pub fn get(conn: &Connection, id: i64) -> Result<Option<Proposal>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.detector, p.kind, p.subject_id,
                COALESCE(s.name, p.subject_id), p.other_id, COALESCE(o.name, ''),
                p.payload, p.evidence, p.score, p.status
         FROM entity_proposal p
         LEFT JOIN nodes s ON s.id = p.subject_id
         LEFT JOIN nodes o ON o.id = p.other_id
         WHERE p.id = ?1",
    )?;
    let mut rows = stmt.query_map(params![id], |r| {
        Ok(Proposal {
            id: r.get(0)?,
            detector: r.get(1)?,
            kind: r.get(2)?,
            subject_id: r.get(3)?,
            subject_name: r.get(4)?,
            other_id: r.get(5)?,
            other_name: r.get(6)?,
            payload: r.get(7)?,
            evidence: r.get(8)?,
            score: r.get(9)?,
            status: r.get(10)?,
        })
    })?;
    Ok(match rows.next() {
        Some(r) => Some(r?),
        None => None,
    })
}

/// Record a decision. `by` is 'user' or 'auto', on `fact_candidate`'s
/// `reviewed_by` precedent — a machine decision must never be countable as
/// the owner's, because that number is what a future ladder would promote on.
pub fn decide(conn: &Connection, id: i64, status: &str, by: &str) -> Result<()> {
    let n = conn.execute(
        "UPDATE entity_proposal
         SET status = ?2, decided_at = datetime('now'), decided_by = ?3
         WHERE id = ?1 AND status = 'pending'",
        params![id, status, by],
    )?;
    if n == 0 {
        return Err(crate::error::Error::Other(format!(
            "proposal {id} is not pending"
        )));
    }
    Ok(())
}

/// Drop the *pending* proposals of one detector. For a detector that has
/// been retuned: its old output describes a rule that no longer exists, and
/// leaving it in the queue asks a person to decide something nothing would
/// propose again.
///
/// **Decided proposals are never touched.** Those are the record of what
/// has already been asked and answered, and clearing them would let a
/// rejection be re-filed on the next run — the durability that keeps this
/// queue from becoming noise.
pub fn clear_pending(conn: &Connection, detector: &str) -> Result<usize> {
    let n = conn.execute(
        "DELETE FROM entity_proposal WHERE status = 'pending' AND detector = ?1",
        params![detector],
    )?;
    Ok(n)
}

/// Counts by detector and status, for the queue depth a reviewer sees.
/// One detector's standing: what is waiting, what has been decided, and
/// how long the oldest undecided one has been there.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct DetectorSummary {
    pub detector: String,
    pub pending: i64,
    pub decided: i64,
    /// `None` when nothing is pending — an absent age, never a zero one.
    pub oldest: Option<String>,
}

pub fn summary(conn: &Connection) -> Result<Vec<DetectorSummary>> {
    let mut stmt = conn.prepare(
        "SELECT detector,
                SUM(status = 'pending'),
                SUM(status <> 'pending'),
                MIN(CASE WHEN status = 'pending' THEN created_at END)
         FROM entity_proposal GROUP BY detector ORDER BY 2 DESC, 1",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(DetectorSummary {
                detector: r.get(0)?,
                pending: r.get(1)?,
                decided: r.get(2)?,
                oldest: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// Apply an accepted proposal. Only the kinds whose repair is fully
/// determined by the proposal itself do anything; `review` kinds are a
/// finding for a person and applying them is a no-op by design.
pub fn apply(conn: &Connection, p: &Proposal) -> Result<String> {
    match p.kind.as_str() {
        "retype" => {
            let to = p
                .payload_str("to_type")
                .ok_or_else(|| crate::error::Error::Other("no to_type in payload".into()))?;
            let (was, now) = graph::retype_node(conn, &p.subject_id, &to)?;
            Ok(format!("{}: {was} → {now}", p.subject_name))
        }
        "rename" => {
            let to = p
                .payload_str("new_name")
                .ok_or_else(|| crate::error::Error::Other("no new_name in payload".into()))?;
            let fix = graph::rename_node(conn, &p.subject_id, &to)?;
            Ok(format!("{} → {}", fix.from, fix.to))
        }
        "merge" => {
            graph::merge_nodes(conn, &p.subject_id, &p.other_id)?;
            crate::rollup::rebuild_person_interactions(conn)?;
            Ok(format!("merged {} into {}", p.other_name, p.subject_name))
        }
        "predicate_merge" => {
            let into = p
                .payload_str("into")
                .ok_or_else(|| crate::error::Error::Other("no into in payload".into()))?;
            let (moved, blocked) = crate::fact::merge_predicate(conn, &p.subject_id, &into)?;
            Ok(if blocked > 0 {
                format!(
                    "{} → {into}: {moved} fact(s) moved, {blocked} stayed (the destination \
                     already holds an identical live fact)",
                    p.subject_id
                )
            } else {
                format!("{} → {into}: {moved} fact(s) moved", p.subject_id)
            })
        }
        "predicate_bless" => {
            let desc = p
                .payload_str("description")
                .ok_or_else(|| crate::error::Error::Other("no description in payload".into()))?;
            crate::fact::bless_predicate(conn, &p.subject_id, &desc)?;
            Ok(format!("{} blessed: {desc}", p.subject_id))
        }
        // A finding, not an instruction. The repair direction is the part a
        // detector cannot know, so accepting one means "yes, this is real" —
        // the fix is whichever verb the person then reaches for.
        "review" => Ok(format!("noted: {}", p.evidence)),
        other => Err(crate::error::Error::Other(format!(
            "don't know how to apply a '{other}' proposal"
        ))),
    }
}

// ─── Detectors ───────────────────────────────────────────────────────────────

/// Run every detector. Returns (detector, newly filed) pairs.
pub fn run_all(conn: &Connection) -> Result<Vec<(&'static str, usize)>> {
    Ok(vec![
        ("email_named_person", detect_email_named_person(conn)?),
        ("type_mismatch", detect_type_mismatch(conn)?),
        ("malformed_name", detect_malformed_name(conn)?),
        ("near_duplicate_person", detect_near_duplicate_person(conn)?),
        ("absorbing_node", detect_absorbing_node(conn)?),
        ("firstname_magnet", detect_firstname_magnet(conn)?),
        ("predicate_contentless", detect_predicate_contentless(conn)?),
        ("predicate_fragment", detect_predicate_fragment(conn)?),
        ("predicate_unblessed", detect_predicate_unblessed(conn)?),
        ("missing_entity", detect_missing_entity(conn)?),
    ])
}

/// A person still named by an address, who already carries a human name as
/// an alias. `promote_human_names` does exactly this repair; the detector
/// exists so it shows up in the queue instead of only when somebody
/// remembers to run a verb whose `--dry-run` defaults to false.
fn detect_email_named_person(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name,
                (SELECT a.alias FROM node_alias a
                  WHERE a.node_id = n.id AND a.alias NOT LIKE '%@%' AND a.alias LIKE '% %'
                  ORDER BY LENGTH(a.alias) DESC LIMIT 1) AS human,
                (SELECT COUNT(*) FROM mention WHERE node_id = n.id) AS m
         FROM nodes n
         WHERE n.node_type = 'person' AND n.name LIKE '%@%' AND human IS NOT NULL",
    )?;
    let rows: Vec<(String, String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut n = 0;
    for (id, name, human, mentions) in rows {
        let proposed = crate::graph::title_case_public(&human);
        // Refuse to propose a rename into a collision — that is a merge
        // question, and a proposal that cannot be applied is worse than none.
        if !canonical_collision_free_name(conn, &proposed, &id)? {
            continue;
        }
        let payload = serde_json::json!({ "new_name": proposed }).to_string();
        if propose(
            conn,
            "email_named_person",
            "rename",
            &id,
            "",
            Some(&payload),
            &format!("named by an address; already aliased {human:?} ({mentions} mentions)"),
            mentions as f64,
        )? {
            n += 1;
        }
        let _ = name;
    }
    Ok(n)
}

/// A node filed under the wrong kind. Two shapes seen here: Google calendar
/// resources typed `person`, and institutions typed `topic`. The type is not
/// cosmetic — it decides resolution rank — so a calendar address filed as a
/// person turns up in people-shaped answers.
fn detect_type_mismatch(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, name, (SELECT COUNT(*) FROM mention WHERE node_id = nodes.id)
         FROM nodes
         WHERE node_type = 'person' AND name LIKE '%calendar.google.com'",
    )?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut n = 0;
    for (id, name, mentions) in rows {
        let payload = serde_json::json!({ "to_type": "artifact" }).to_string();
        if propose(
            conn,
            "type_mismatch",
            "retype",
            &id,
            "",
            Some(&payload),
            &format!("{name} is a Google calendar, not a person ({mentions} mentions)"),
            mentions as f64,
        )? {
            n += 1;
        }
    }
    Ok(n)
}

/// A name that is several names. Wiki-link syntax in a note — `[[A]], [[B]]`
/// — reaches the extractor as one string, so a book's co-authors become a
/// single person. Detect only: splitting one node into three is a decision
/// about which facts belong to whom, and nothing here can make it.
fn detect_malformed_name(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, name, (SELECT COUNT(*) FROM mention WHERE node_id = nodes.id)
         FROM nodes WHERE name LIKE '%]], [[%'",
    )?;
    let rows: Vec<(String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let mut n = 0;
    for (id, name, mentions) in rows {
        let people = name.split("]], [[").count();
        if propose(
            conn,
            "malformed_name",
            "review",
            &id,
            "",
            None,
            &format!("wiki-link syntax parsed as one name — this is {people} people ({mentions} mentions)"),
            mentions as f64,
        )? {
            n += 1;
        }
    }
    Ok(n)
}

/// Two person nodes that are probably one person. `dups` only ever matched
/// identical full names, which is why "Ingrid Mai Solberg" and
/// "ingrid@lawoffice-solberg.com" sat apart for years while both answered to
/// "ingrid".
///
/// Three rules, blocked on the first token so this stays linear-ish rather
/// than comparing 14,000 nodes pairwise:
/// - one name's tokens are a strict subset of the other's (`Conan Moore` ⊂
///   `Conan F Moore`)
/// - edit distance ≤ 2 on names of 8 characters or more
///   (`Jessica Andrews-Hanna` / `Jessica Andrews-Hannah`)
/// - both answer to the same **multi-word** alias — a shared bare first name
///   is ordinary and a shared full name is not
fn detect_near_duplicate_person(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT id, name, canonical_name,
                (SELECT COUNT(*) FROM mention WHERE node_id = nodes.id)
         FROM nodes WHERE node_type = 'person' AND canonical_name <> '' ",
    )?;
    let people: Vec<(String, String, String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;

    // Block by first token: two spellings of one person almost always agree
    // on the first name, and blocking is what keeps this from being 100
    // million comparisons.
    let mut blocks: std::collections::HashMap<&str, Vec<usize>> = std::collections::HashMap::new();
    for (i, p) in people.iter().enumerate() {
        if let Some(first) = p.2.split_whitespace().next() {
            blocks.entry(first).or_default().push(i);
        }
    }

    let mut n = 0;
    for idxs in blocks.values() {
        for a in 0..idxs.len() {
            for b in (a + 1)..idxs.len() {
                let (x, y) = (&people[idxs[a]], &people[idxs[b]]);
                let Some((why, score)) = duplicate_signal(&x.2, &y.2) else {
                    continue;
                };
                // Keep the better-attested node: a merge keeps the survivor's
                // name, and the one with more evidence is the one to keep.
                let (keep, dup) = if x.3 >= y.3 { (x, y) } else { (y, x) };
                if propose(
                    conn,
                    "near_duplicate_person",
                    "merge",
                    &keep.0,
                    &dup.0,
                    None,
                    &format!(
                        "{why}: {:?} ({} mentions) and {:?} ({} mentions)",
                        keep.1, keep.3, dup.1, dup.3
                    ),
                    score * (keep.3 + dup.3) as f64,
                )? {
                    n += 1;
                }
            }
        }
    }
    Ok(n)
}

/// Why two canonical names look like one person, and how strongly.
fn duplicate_signal(a: &str, b: &str) -> Option<(&'static str, f64)> {
    if a == b {
        return Some(("identical names", 1.0));
    }
    let ta: std::collections::BTreeSet<&str> = a.split_whitespace().collect();
    let tb: std::collections::BTreeSet<&str> = b.split_whitespace().collect();
    if ta.len() >= 2 && tb.len() >= 2 && (ta.is_subset(&tb) || tb.is_subset(&ta)) {
        return Some(("one name is the other plus an initial", 0.9));
    }
    if a.len() >= 8 && b.len() >= 8 && edit_distance(a, b) <= 2 {
        return Some(("names differ by a typo", 0.8));
    }
    None
}

/// Levenshtein, two rows. Four lines of arithmetic rather than a dependency:
/// nothing here needs a fast implementation over 14,000 blocked comparisons.
fn edit_distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    let mut cur = vec![0usize; b.len() + 1];
    for i in 1..=a.len() {
        cur[0] = i;
        for j in 1..=b.len() {
            let cost = usize::from(a[i - 1] != b[j - 1]);
            cur[j] = (prev[j] + 1).min(cur[j - 1] + 1).min(prev[j - 1] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[b.len()]
}

/// A node that is not a person, carrying facts that are all about one.
///
/// The shape that made an event called "SPSP Reedie Reunion" the subject of
/// twenty-one facts about a toddler: `resolve_entity_all` falls back to
/// `LIKE '%name%'`, and "Wren" is a substring of "Wrench". The tell is
/// provenance rather than content — a real event is mentioned by the
/// calendar that created it, so an event whose mentions are overwhelmingly
/// an LLM reading conversations is not being mentioned, it is being mistaken
/// for something else.
fn detect_absorbing_node(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name, n.node_type,
                SUM(m.extractor = 'llm')  AS soft,
                SUM(m.extractor <> 'llm') AS own,
                (SELECT COUNT(*) FROM fact
                  WHERE (subject_id = n.id OR object_id = n.id) AND valid_to IS NULL) AS facts
         FROM nodes n JOIN mention m ON m.node_id = n.id
         WHERE n.node_type IN ('event','topic','artifact','document','place')
         GROUP BY n.id
         HAVING soft >= 25 AND soft >= own * 5",
    )?;
    let rows: Vec<(String, String, String, i64, i64, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut n = 0;
    for (id, name, ntype, soft, own, facts) in rows {
        if propose(
            conn,
            "absorbing_node",
            "review",
            &id,
            "",
            None,
            &format!(
                "{ntype} {name:?} carries {soft} conversational mentions against {own} of its own, \
                 and {facts} live facts — a name-match may be attaching someone else's history to it"
            ),
            soft as f64,
        )? {
            n += 1;
        }
    }
    Ok(n)
}

/// A bare first name that exactly one node answers to, where that node's
/// evidence does not look like the reason.
///
/// This is the mechanism behind the worst case here, and the count is what
/// makes it dangerous rather than merely untidy. A first name held by two or
/// more nodes resolves *ambiguously*, which is visible and survivable — the
/// disambiguation envelope exists for it. A first name held by exactly one
/// node resolves **silently**, so every mention of it lands there whether or
/// not it is the person meant. Add an identity built from a single calendar
/// invitation and a thousand kitchen conversations about somebody else, and
/// the graph will merge two lives without a single ambiguous result to warn
/// anyone.
///
/// The tell is a provenance **conflict**, and `ids >= 1` is the whole of it.
/// A node with no identifier that accumulates a thousand spoken mentions is
/// simply somebody you talk about a lot — that is the system working, and
/// flagging it makes the detector cry wolf about its own best case (measured:
/// it fired on the repaired node the same evening). What is not normal is a
/// node whose *identity* comes from one channel — an address, off a single
/// calendar invitation — while its *mentions* overwhelmingly come from an
/// unrelated one. That mismatch is the conflation signature, and it is what
/// the node behind the 2026-08-24 repair looked like: one Ostrander address,
/// 1,010 kitchen-conversation alias matches, 22 mentions from anywhere else.
fn detect_firstname_magnet(conn: &Connection) -> Result<usize> {
    // The filter sits in an outer WHERE rather than a HAVING: none of these
    // columns is an aggregate as far as SQLite is concerned (they are
    // correlated subqueries), and `HAVING` on a non-aggregate query is an
    // error rather than a no-op.
    let mut stmt = conn.prepare(
        "SELECT alias, node_id, name, spoken, other, ids FROM (
           SELECT a.alias AS alias, a.node_id AS node_id, n.name AS name,
                (SELECT COUNT(*) FROM mention m JOIN episode e ON e.id = m.episode_id
                  WHERE m.node_id = n.id AND m.extractor = 'alias' AND e.source LIKE 'bee%') AS spoken,
                (SELECT COUNT(*) FROM mention m JOIN episode e ON e.id = m.episode_id
                  WHERE m.node_id = n.id AND e.source NOT LIKE 'bee%') AS other,
                (SELECT COUNT(*) FROM node_identifier i WHERE i.node_id = n.id) AS ids
           FROM node_alias a JOIN nodes n ON n.id = a.node_id
           WHERE a.source = 'firstname'
             AND (SELECT COUNT(*) FROM node_alias b WHERE b.alias = a.alias) = 1
         ) WHERE spoken >= 50 AND spoken >= other * 5 AND ids >= 1",
    )?;
    let rows: Vec<(String, String, String, i64, i64, i64)> = stmt
        .query_map([], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut n = 0;
    for (alias, id, name, spoken, other, ids) in rows {
        if propose(
            conn,
            "firstname_magnet",
            "review",
            &id,
            "",
            None,
            &format!(
                "{name:?} is the only node answering to {alias:?}, and {spoken} of its mentions are \
                 spoken-source alias matches against {other} from anywhere else ({ids} identifier(s)) \
                 — a bare first name nobody else claims resolves silently, so check this is one person"
            ),
            spoken as f64,
        )? {
            n += 1;
        }
    }
    Ok(n)
}

/// A name the graph keeps hearing and cannot place.
///
/// The other half of the corroboration gate, and the reason refusing a weak
/// match is better than guessing at one. When "Marisol" appeared in a
/// kitchen conversation and the only candidate was a student with no
/// connection to the household, the right answer was to link nothing — and
/// then to notice that a name was recurring, weekly, for years, with
/// nothing to attach it to.
///
/// That is exactly the state Wren was in: mentioned constantly, no node,
/// and nothing anywhere saying so. A detector over refused matches finds
/// her, where no amount of looking at what the graph *contains* ever could.
///
/// The floor is on **distinct episodes**, not rows: one long conversation
/// naming somebody five times is one occasion, and a name has to keep
/// coming back across separate occasions before it means anyone.
fn detect_missing_entity(conn: &Connection) -> Result<usize> {
    const FLOOR: i64 = 8;
    let mut stmt = conn.prepare(
        "SELECT u.alias, COUNT(DISTINCT u.episode_id) AS occasions,
                MIN(u.at), MAX(u.at)
         FROM unlinked_mention u
         GROUP BY u.alias HAVING occasions >= ?1
         ORDER BY occasions DESC",
    )?;
    let rows: Vec<(String, i64, String, String)> = stmt
        .query_map(params![FLOOR], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    let mut n = 0;
    for (alias, occasions, first, last) in rows {
        // Keyed on the alias rather than a node: the finding is that a name
        // has nobody behind it, so there is no subject to point at. Using
        // the name itself also makes it idempotent — the same recurring
        // name is one proposal however many times it recurs.
        if propose(
            conn,
            "missing_entity",
            "review",
            &alias,
            "",
            None,
            &format!(
                "{alias:?} appears on {occasions} separate occasions ({} to {}) and every match \
                 was refused for want of corroboration — this may be somebody the graph has no \
                 node for. Create one, or alias the name onto whoever it means",
                &first[..10.min(first.len())],
                &last[..10.min(last.len())]
            ),
            occasions as f64,
        )? {
            n += 1;
        }
    }
    Ok(n)
}

// ─── Predicate detectors ─────────────────────────────────────────────────────
//
// The vocabulary drifts the same way the entity space does, for the same
// reason: something mints a name, nothing reviews it, and the damage is
// invisible until somebody counts. 49 of the 83 predicates here were
// auto-registered, carrying about 900 live facts, and they split into three
// kinds — meaningless, fragmented, and genuinely useful but never decided
// on. One detector each.

/// A copula asserting nothing beyond "these are related". The extractor
/// mints them when a sentence has no verb worth keeping — "Wren **is** a
/// twin daughter" — and they carried 252 live facts here.
///
/// Proposed as a merge into `related_to` rather than a deletion: the facts
/// are true and their *statements* carry the meaning; only the relation is
/// empty. Deleting them would lose real sentences to a vocabulary problem.
fn detect_predicate_contentless(conn: &Connection) -> Result<usize> {
    let mut n = 0;
    for name in crate::fact::CONTENTLESS_PREDICATES {
        let live: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact f
                 WHERE f.predicate = ?1 AND f.valid_to IS NULL
                   AND EXISTS (SELECT 1 FROM predicate p WHERE p.name = ?1)",
                params![name],
                |r| r.get(0),
            )
            .unwrap_or(0);
        if live == 0 {
            continue;
        }
        let payload = serde_json::json!({ "into": "related_to" }).to_string();
        if propose(
            conn,
            "predicate_contentless",
            "predicate_merge",
            name,
            "",
            Some(&payload),
            &format!(
                "{name:?} asserts nothing beyond relatedness and carries {live} live fact(s) — \
                 fold into related_to, whose statements already say what the relation is"
            ),
            live as f64,
        )? {
            n += 1;
        }
    }
    Ok(n)
}

/// One predicate that is another wearing a different tense or a copula
/// prefix — `is_located_in` beside seeded `located_in`, `discusses` beside
/// `discussed`.
///
/// **Grouped by stem, not compared pairwise.** A family of three —
/// discussed / discusses / discussing — is one equivalence class, and
/// comparing its members in pairs proposes three merges where two are
/// needed, two of which contradict each other about which name survives.
/// Worse, applying one destroys another's target, so a reviewer accepting
/// them in order hits an error on a proposal that was correct when filed.
/// Choosing one canonical per family and proposing every other member into
/// it gives n-1 consistent merges that can be accepted in any order.
///
/// The canonical is chosen in three tiers, and the order matters:
/// **seeded** beats auto-registered however the counts fall, because the
/// predicate table is interpolated into the extraction prompt and folding a
/// chosen predicate into a minted one teaches the minted one. Then a name
/// **without a leading copula**, because `developed` is a better canonical
/// than `is_developing` whatever their counts — the copula is the noise
/// this whole detector exists to remove. Only then evidence, and finally
/// the name itself, so the choice is deterministic across runs.
fn detect_predicate_fragment(conn: &Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT p.name,
                (SELECT COUNT(*) FROM fact f WHERE f.predicate = p.name AND f.valid_to IS NULL),
                p.description = 'auto-registered'
         FROM predicate p",
    )?;
    let preds: Vec<(String, i64, bool)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let contentless: std::collections::HashSet<&str> = crate::fact::CONTENTLESS_PREDICATES
        .iter()
        .copied()
        .collect();

    let mut families: std::collections::BTreeMap<String, Vec<&(String, i64, bool)>> =
        std::collections::BTreeMap::new();
    for p in &preds {
        // Contentless predicates have their own detector and a different
        // destination; letting them into a stem family would fold `is` into
        // whatever else happens to stem to `is`.
        if contentless.contains(p.0.as_str()) {
            continue;
        }
        families
            .entry(crate::fact::stem_predicate_public(&p.0))
            .or_default()
            .push(p);
    }

    let mut n = 0;
    for members in families.values() {
        if members.len() < 2 {
            continue;
        }
        let canonical = members
            .iter()
            .min_by_key(|m| {
                (
                    m.2,                      // seeded first
                    starts_with_copula(&m.0), // then no copula prefix
                    -m.1,                     // then more evidence
                    m.0.clone(),              // then deterministic
                )
            })
            .expect("non-empty");
        for m in members {
            if m.0 == canonical.0 {
                continue;
            }
            let payload = serde_json::json!({ "into": canonical.0 }).to_string();
            if propose(
                conn,
                "predicate_fragment",
                "predicate_merge",
                &m.0,
                &canonical.0,
                Some(&payload),
                &format!(
                    "same relation, different tense or copula: {:?} ({} live) folds into {:?} ({} live)",
                    m.0, m.1, canonical.0, canonical.1
                ),
                (m.1 + canonical.1) as f64,
            )? {
                n += 1;
            }
        }
    }
    Ok(n)
}

/// Does a predicate name begin with a copula token? Used only as a
/// tiebreak, so `developed` outranks `is_developing` as a canonical name.
fn starts_with_copula(name: &str) -> bool {
    matches!(
        name.split('_').next(),
        Some("is" | "are" | "was" | "were" | "be" | "been" | "being")
    ) && name.contains('_')
}

/// A predicate nobody decided on that is nonetheless doing real work.
///
/// The half of the leak that is not junk, and the half worth getting right:
/// `is_planning` (126 live facts), `interested_in` (109), `is_developing`
/// (85) are real relations that appeared because an extractor said a word,
/// and have been carrying weight ever since with no description, no
/// half-life, and no decision behind them. Blessing one is what puts it in
/// the extraction prompt deliberately rather than by accident.
///
/// The floor is deliberately high. A predicate used twice is a one-off and
/// belongs in the fragment detector or nowhere; a predicate used fifty times
/// is load-bearing vocabulary somebody should look at.
fn detect_predicate_unblessed(conn: &Connection) -> Result<usize> {
    const FLOOR: i64 = 20;
    let mut stmt = conn.prepare(
        "SELECT p.name, (SELECT COUNT(*) FROM fact f
                          WHERE f.predicate = p.name AND f.valid_to IS NULL) AS live
         FROM predicate p WHERE p.description = 'auto-registered'
         ORDER BY live DESC",
    )?;
    let rows: Vec<(String, i64)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    let contentless: std::collections::HashSet<&str> = crate::fact::CONTENTLESS_PREDICATES
        .iter()
        .copied()
        .collect();
    let mut n = 0;
    for (name, live) in rows {
        if live < FLOOR || contentless.contains(name.as_str()) {
            continue;
        }
        let payload = serde_json::json!({
            "description": format!("{} (blessed from use)", name.replace('_', " "))
        })
        .to_string();
        if propose(
            conn,
            "predicate_unblessed",
            "predicate_bless",
            &name,
            "",
            Some(&payload),
            &format!(
                "{name:?} carries {live} live fact(s) and was auto-registered — real vocabulary \
                 nobody decided on. Bless it (edit the description first) or fold it into \
                 something seeded"
            ),
            live as f64,
        )? {
            n += 1;
        }
    }
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{add_alias, create_node, create_person, Node};

    #[test]
    fn a_proposal_is_filed_once_and_a_rejection_is_durable() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "Vera Holt", "t").unwrap();
        assert!(propose(&conn, "d", "review", &n.id, "", None, "because", 1.0).unwrap());
        // The nightly re-runs and says nothing new.
        assert!(!propose(&conn, "d", "review", &n.id, "", None, "because", 1.0).unwrap());
        assert_eq!(pending(&conn, None, 10).unwrap().len(), 1);

        let id = pending(&conn, None, 10).unwrap()[0].id;
        decide(&conn, id, "rejected", "user").unwrap();
        assert!(pending(&conn, None, 10).unwrap().is_empty());
        // Re-running the detector must not resurrect a refused proposal.
        assert!(!propose(&conn, "d", "review", &n.id, "", None, "because", 1.0).unwrap());
        assert!(pending(&conn, None, 10).unwrap().is_empty());
        // And a decision cannot be made twice.
        assert!(decide(&conn, id, "accepted", "user").is_err());
    }

    #[test]
    fn an_email_named_person_with_a_human_alias_is_proposed_and_applies() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "bob@arizona.edu", "t").unwrap();
        add_alias(&conn, &n.id, "Robert Wilson", "attendee").unwrap();
        assert_eq!(detect_email_named_person(&conn).unwrap(), 1);

        let p = &pending(&conn, Some("email_named_person"), 10).unwrap()[0];
        assert_eq!(p.kind, "rename");
        assert_eq!(p.payload_str("new_name").as_deref(), Some("Robert Wilson"));
        apply(&conn, p).unwrap();
        assert_eq!(
            crate::graph::get_node(&conn, &n.id).unwrap().unwrap().name,
            "Robert Wilson"
        );
    }

    /// A rename into a name another node already owns is a merge question,
    /// and a proposal that cannot be applied is worse than no proposal.
    #[test]
    fn a_rename_that_would_collide_is_not_proposed() {
        let conn = open_memory().unwrap();
        create_person(&conn, "Robert Wilson", "t").unwrap();
        let n = create_person(&conn, "bob@arizona.edu", "t").unwrap();
        add_alias(&conn, &n.id, "Robert Wilson", "attendee").unwrap();
        assert_eq!(detect_email_named_person(&conn).unwrap(), 0);
    }

    #[test]
    fn near_duplicates_the_old_detector_could_never_see() {
        let conn = open_memory().unwrap();
        for name in [
            "Conan Moore",
            "Conan F Moore",
            "Jessica Andrews-Hanna",
            "Jessica Andrews-Hannah",
            "Emma Calloway",
            "Emma Call",
        ] {
            create_person(&conn, name, "t").unwrap();
        }
        detect_near_duplicate_person(&conn).unwrap();
        let props = pending(&conn, Some("near_duplicate_person"), 20).unwrap();
        let seen: Vec<String> = props.iter().map(|p| p.evidence.clone()).collect();
        assert!(
            seen.iter().any(|e| e.contains("plus an initial")),
            "{seen:?}"
        );
        assert!(seen.iter().any(|e| e.contains("typo")), "{seen:?}");
        // Two different people who share a first name are not duplicates.
        assert!(
            !seen.iter().any(|e| e.contains("Emma Call")),
            "a shared first name is not a duplicate signal: {seen:?}"
        );
    }

    #[test]
    fn edit_distance_is_right_about_the_cases_it_decides() {
        assert_eq!(edit_distance("", ""), 0);
        assert_eq!(edit_distance("abc", "abc"), 0);
        assert_eq!(
            edit_distance("jessica andrews-hanna", "jessica andrews-hannah"),
            1
        );
        assert_eq!(edit_distance("kitten", "sitting"), 3);
        assert_eq!(edit_distance("", "abc"), 3);
    }

    #[test]
    fn a_calendar_resource_typed_as_a_person_is_proposed_for_retyping() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "abc123@group.calendar.google.com", "t").unwrap();
        assert_eq!(detect_type_mismatch(&conn).unwrap(), 1);
        let p = &pending(&conn, Some("type_mismatch"), 10).unwrap()[0];
        assert_eq!(p.payload_str("to_type").as_deref(), Some("artifact"));
        apply(&conn, p).unwrap();
        assert_eq!(
            crate::graph::get_node(&conn, &n.id)
                .unwrap()
                .unwrap()
                .node_type,
            "artifact"
        );
    }

    #[test]
    fn a_name_that_is_several_names_is_flagged_for_a_person_to_split() {
        let conn = open_memory().unwrap();
        create_person(&conn, "Richard S. Sutton]], [[Andrew G. Barto", "t").unwrap();
        assert_eq!(detect_malformed_name(&conn).unwrap(), 1);
        let p = &pending(&conn, Some("malformed_name"), 10).unwrap()[0];
        // Detect only: nothing here knows which facts belong to whom.
        assert_eq!(p.kind, "review");
        assert!(p.evidence.contains("2 people"), "{}", p.evidence);
    }

    /// The discriminator that keeps this detector from firing on its own
    /// best case: a bare first name held by one node is only suspicious when
    /// the node's identity evidence comes from somewhere its mentions do not.
    #[test]
    fn a_firstname_magnet_needs_an_identity_that_conflicts_with_its_mentions() {
        let conn = open_memory().unwrap();
        let mk_spoken = |node: &str, n: usize| {
            for i in 0..n {
                let ep = crate::episode::Episode {
                    id: 0,
                    uid: String::new(),
                    source: "bee.conversation".into(),
                    source_id: format!("{node}-{i}"),
                    source_ref: None,
                    body: "talk".into(),
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
                let (e, _) = crate::episode::upsert_episode(&conn, &ep).unwrap();
                crate::episode::add_mention(&conn, e, node, "alias", 1.0).unwrap();
            }
        };

        // Somebody you simply talk about: no identifier, so not a finding.
        // Distinct first names on purpose — two nodes claiming one first
        // name is the *ambiguous* case, which resolves visibly and is not
        // what this detector is for.
        let loved = create_person(&conn, "Marisol Calder", "t").unwrap();
        mk_spoken(&loved.id, 60);

        // The conflation shape: identity off an address, mentions from the
        // kitchen. `get_or_create_person` mints both the identifier and the
        // first-name alias, exactly as it did on the day.
        let student = crate::graph::get_or_create_person(
            &conn,
            Some("marguerite.b.farrow.27@ostrander.edu"),
            "Marguerite B. Farrow",
            "llm",
        )
        .unwrap();
        mk_spoken(&student.id, 60);

        assert_eq!(
            detect_firstname_magnet(&conn).unwrap(),
            1,
            "only the node whose identity and mentions disagree"
        );
        let props = pending(&conn, Some("firstname_magnet"), 5).unwrap();
        assert_eq!(props.len(), 1);
        assert_eq!(props[0].subject_id, student.id);
        assert_ne!(
            props[0].subject_id, loved.id,
            "a well-loved node is not a finding"
        );
    }

    // ── predicate detectors ───────────────────────────────────────────────

    fn mk_pred(conn: &rusqlite::Connection, name: &str, auto: bool) {
        conn.execute(
            "INSERT OR IGNORE INTO predicate (name, description) VALUES (?1, ?2)",
            params![
                name,
                if auto {
                    "auto-registered"
                } else {
                    "a real relation"
                }
            ],
        )
        .unwrap();
    }

    fn mk_fact(conn: &rusqlite::Connection, subj: &str, pred: &str, obj: Option<&str>, stmt: &str) {
        conn.execute(
            "INSERT INTO fact (uid, subject_id, predicate, object_id, statement, polarity,
                               confidence, observation_count, valid_from)
             VALUES (hex(randomblob(8)), ?1, ?2, ?3, ?4, 'positive', 1.0, 1, datetime('now'))",
            params![subj, pred, obj, stmt],
        )
        .unwrap();
    }

    /// The copula case: true sentences on an empty relation. Folded into
    /// `related_to` rather than deleted — the statements carry the meaning.
    #[test]
    fn a_contentless_predicate_is_proposed_for_folding_not_deletion() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "Wren Calder", "t").unwrap();
        mk_pred(&conn, "is", true);
        mk_fact(&conn, &n.id, "is", None, "Wren is a twin daughter.");

        assert_eq!(detect_predicate_contentless(&conn).unwrap(), 1);
        let p = &pending(&conn, Some("predicate_contentless"), 5).unwrap()[0];
        assert_eq!(p.kind, "predicate_merge");
        assert_eq!(p.subject_id, "is");
        assert_eq!(p.payload_str("into").as_deref(), Some("related_to"));

        apply(&conn, p).unwrap();
        // The fact survives, wearing a relation that means something.
        let (pred, stmt): (String, String) = conn
            .query_row(
                "SELECT predicate, statement FROM fact WHERE subject_id = ?1",
                params![n.id],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(pred, "related_to");
        assert_eq!(stmt, "Wren is a twin daughter.");
        // And the fold is learned, so the extractor stops re-minting it.
        let mapped: String = conn
            .query_row(
                "SELECT name FROM predicate_alias WHERE alias = 'is'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mapped, "related_to");
    }

    /// The fragment the stemmer could not see until it learned to strip a
    /// leading copula: `is_located_in` against seeded `located_in`.
    #[test]
    fn a_copula_prefixed_fragment_is_folded_into_the_seeded_predicate() {
        let conn = open_memory().unwrap();
        let a = create_person(&conn, "A Person", "t").unwrap();
        mk_pred(&conn, "located_in", false); // seeded
        mk_pred(&conn, "is_located_in", true); // minted
        for i in 0..5 {
            mk_fact(&conn, &a.id, "is_located_in", None, &format!("s{i}"));
        }
        mk_fact(&conn, &a.id, "located_in", None, "seeded one");

        assert!(detect_predicate_fragment(&conn).unwrap() >= 1);
        let p = pending(&conn, Some("predicate_fragment"), 10)
            .unwrap()
            .into_iter()
            .find(|p| p.subject_id == "is_located_in")
            .expect("the minted one is the one that moves");
        // Seeded wins even though the fragment holds more facts (5 vs 1):
        // the predicate table is the extraction prompt, so folding the
        // seeded one into a fragment would teach the fragment.
        assert_eq!(p.payload_str("into").as_deref(), Some("located_in"));
        apply(&conn, &p).unwrap();

        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact WHERE predicate = 'is_located_in'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
        let gone: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM predicate WHERE name = 'is_located_in'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            gone, 0,
            "an emptied predicate is removed, not left as vocabulary"
        );
    }

    /// Real vocabulary nobody decided on. Blessing is a decision, so this
    /// proposes rather than applies, and the floor keeps one-offs out.
    #[test]
    fn a_busy_auto_registered_predicate_is_proposed_for_blessing() {
        let conn = open_memory().unwrap();
        let a = create_person(&conn, "A Person", "t").unwrap();
        mk_pred(&conn, "is_planning", true);
        mk_pred(&conn, "lent", true);
        for i in 0..25 {
            mk_fact(&conn, &a.id, "is_planning", None, &format!("p{i}"));
        }
        mk_fact(&conn, &a.id, "lent", None, "a one-off");

        assert_eq!(detect_predicate_unblessed(&conn).unwrap(), 1);
        let p = &pending(&conn, Some("predicate_unblessed"), 5).unwrap()[0];
        assert_eq!(
            p.subject_id, "is_planning",
            "a predicate used once is not vocabulary"
        );
        apply(&conn, p).unwrap();
        let desc: String = conn
            .query_row(
                "SELECT description FROM predicate WHERE name = 'is_planning'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_ne!(desc, "auto-registered");
    }

    /// A stem family of three is one equivalence class, not three pairs.
    /// Pairwise comparison proposed `discussing → discusses` AND
    /// `discussing → discussed` — contradictory, and accepting one made the
    /// other's target vanish.
    #[test]
    fn a_stem_family_gets_one_canonical_not_a_pair_for_every_combination() {
        let conn = open_memory().unwrap();
        let a = create_person(&conn, "A Person", "t").unwrap();
        for (name, auto, facts) in [
            ("discussed", true, 15),
            ("discusses", true, 10),
            ("discussing", true, 2),
        ] {
            mk_pred(&conn, name, auto);
            for i in 0..facts {
                mk_fact(&conn, &a.id, name, None, &format!("{name}{i}"));
            }
        }
        let n = detect_predicate_fragment(&conn).unwrap();
        assert_eq!(n, 2, "three names, one canonical, two merges");

        let props = pending(&conn, Some("predicate_fragment"), 10).unwrap();
        let targets: std::collections::BTreeSet<String> = props
            .iter()
            .map(|p| p.payload_str("into").unwrap())
            .collect();
        assert_eq!(
            targets.len(),
            1,
            "every merge in a family agrees: {targets:?}"
        );
        assert!(targets.contains("discussed"), "{targets:?}");

        // Order-independent: whichever the reviewer accepts first, the rest
        // still apply.
        for p in &props {
            apply(&conn, p).unwrap();
        }
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact WHERE predicate IN ('discusses','discussing')",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    /// A name without a copula prefix wins the canonical slot even when the
    /// prefixed one holds more facts — the copula is the noise the whole
    /// detector exists to remove.
    #[test]
    fn the_canonical_is_the_name_without_the_copula() {
        let conn = open_memory().unwrap();
        let a = create_person(&conn, "A Person", "t").unwrap();
        mk_pred(&conn, "developed", true);
        mk_pred(&conn, "is_developing", true);
        for i in 0..3 {
            mk_fact(&conn, &a.id, "developed", None, &format!("d{i}"));
        }
        for i in 0..30 {
            mk_fact(&conn, &a.id, "is_developing", None, &format!("i{i}"));
        }
        detect_predicate_fragment(&conn).unwrap();
        let p = &pending(&conn, Some("predicate_fragment"), 5).unwrap()[0];
        assert_eq!(p.subject_id, "is_developing");
        assert_eq!(p.payload_str("into").as_deref(), Some("developed"));
    }

    /// Retuning a detector clears what it used to say — but never what a
    /// person already decided.
    #[test]
    fn clearing_a_retuned_detector_spares_the_decisions() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "A Person", "t").unwrap();
        propose(&conn, "d", "review", &n.id, "", None, "one", 1.0).unwrap();
        propose(&conn, "d", "review", &n.id, "x", None, "two", 1.0).unwrap();
        let first = pending(&conn, Some("d"), 10).unwrap()[0].id;
        decide(&conn, first, "rejected", "user").unwrap();

        assert_eq!(clear_pending(&conn, "d").unwrap(), 1);
        assert!(pending(&conn, Some("d"), 10).unwrap().is_empty());
        // The rejection survives, so it cannot be re-filed.
        assert!(!propose(&conn, "d", "review", &n.id, "", None, "one", 1.0).unwrap());
    }

    /// Families are keyed on the resolver's own stemmer, so the audit can
    /// only ever propose a merge the normalizer would itself make. Two
    /// definitions of "same relation" would let the queue ask for folds
    /// that resolution then refuses to honour.
    #[test]
    fn families_are_keyed_on_the_resolvers_stemmer() {
        let stem = crate::fact::stem_predicate_public;
        for (a, b) in [
            ("is_located_in", "located_in"),
            ("is_blocked_by", "blocked_by"),
            ("discusses", "discussed"),
            ("provided_guidance_on", "providing_guidance_on"),
            ("is_developing", "developed"),
        ] {
            assert_eq!(stem(a), stem(b), "{a} and {b} are one relation");
        }
        // Different relations that merely LOOK alike stay apart. works_at
        // and works_on are two edits apart, which is why this family gets no
        // edit-distance rung the way people do: folding them would merge
        // where somebody works into what they work on, across the whole
        // graph. An earlier version proposed exactly that.
        for (a, b) in [
            ("works_at", "works_on"),
            ("attended", "authored"),
            ("mentions", "mentors"),
        ] {
            assert_ne!(stem(a), stem(b), "{a} and {b} are different relations");
        }
    }

    #[test]
    fn every_detector_runs_over_an_empty_graph_without_complaint() {
        let conn = open_memory().unwrap();
        let out = run_all(&conn).unwrap();
        assert_eq!(out.len(), 10);
        // Nothing fires on a fresh database — which also means the *seeded*
        // vocabulary contains no pair the fragment detector would merge. An
        // earlier edit-distance rung failed here by proposing works_at into
        // works_on, and this assertion is what caught it.
        assert!(out.iter().all(|(_, n)| *n == 0), "{out:?}");
    }

    /// The absorbing shape, built the way it really happened: an event whose
    /// mentions are overwhelmingly an LLM reading conversations.
    #[test]
    fn an_event_absorbing_conversation_is_flagged() {
        let conn = open_memory().unwrap();
        let ev = create_node(&conn, "event", "SPSP Reedie Reunion", "t").unwrap();
        let mk = |i: usize, src: &str| {
            let ep = crate::episode::Episode {
                id: 0,
                uid: String::new(),
                source: src.into(),
                source_id: format!("e{i}"),
                source_ref: None,
                body: "body".into(),
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
            crate::episode::upsert_episode(&conn, &ep).unwrap().0
        };
        for i in 0..30 {
            let e = mk(i, "bee.conversation");
            crate::episode::add_mention(&conn, e, &ev.id, "llm", 1.0).unwrap();
        }
        let own = mk(999, "calendar.event");
        crate::episode::add_mention(&conn, own, &ev.id, "attendee", 1.0).unwrap();

        assert_eq!(detect_absorbing_node(&conn).unwrap(), 1);
        let p = &pending(&conn, Some("absorbing_node"), 10).unwrap()[0];
        assert!(p.evidence.contains("30 conversational"), "{}", p.evidence);
        let _ = Node::new("x", "person", "y");
    }
}
