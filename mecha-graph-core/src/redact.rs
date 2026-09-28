//! True redaction (§10): an episode, and every row anywhere in the store that
//! holds its id, its uid, its text, or text derived from it.
//!
//! Two doors, one purge. `mecha-graph redact` is the privacy path — it takes
//! the undo snapshots with it, because an undo copy would defeat a true
//! delete. The TUI's delete ([`crate::episode::redact_episode_undoable`])
//! writes a snapshot first and then runs the same purge, keeping the
//! snapshot so Ctrl-Z works.
//!
//! **What a redaction reaches**, table by table, so the next table a
//! migration adds has somewhere obvious to be listed:
//!
//! - the episode row, its FTS rows (trigger), its vector, and the tables
//!   keyed on it: `mention`, `episode_enrichment`, `episode_raw`,
//!   `episode_annotation`, `extract_state`, `unlinked_mention`. Deleted
//!   explicitly rather than trusted to `ON DELETE CASCADE`, because a
//!   connection opened with `foreign_keys` off would otherwise leave them.
//! - the facts it founded (`fact.episode_id`), with their vectors, FTS rows,
//!   observation trails and co-occurrence alarms.
//! - its sightings of facts it did **not** found (`fact_observation`): the
//!   row goes, the fact stays, and the fact's counter and confidence are
//!   re-derived without it. This replaces the old `ON DELETE SET NULL`
//!   behaviour, which kept an identifier-free sighting — and which
//!   `fact::recompute_confidence` then counted as *episodeless* support, so a
//!   redacted `agent:*` corroboration that never counted became one that did.
//! - the candidates extracted from it, with their agent verdicts, cached
//!   review-queue vectors (`candidate_embedding`) and rejection-memory
//!   vectors (`vec_rejected`).
//! - telemetry naming it or its facts: `retrieval_touch` rows, and
//!   `event_log` rows whose ref or payload names the episode or one of its
//!   facts (corrections carry the corrected text in their payload).
//! - derived views over it: the `person_interaction` rollup of every person
//!   it mentioned is rebuilt, and the generated `node_context.summary` of
//!   every node it touched is cleared for the nightly to regenerate (a
//!   summary quotes episode snippets; the hand-authored `instruction` is
//!   never touched).
//! - a `nodes.source_ref` pointing at its uid (calendar event nodes) is
//!   cleared; the node stays.
//! - privacy path only: every `undo_log` snapshot of it — by uid, and by
//!   (source, source_id) so a snapshot left by an earlier TUI delete of the
//!   same item is found too — and the telemetry naming what that snapshot
//!   held.
//!
//! **Derived beliefs are re-derived, not deleted** (owner's ruling,
//! 2026-09-28): a co-occurrence fact (`extractor = 'npmi'`) cites its newest
//! contributor, so this episode may be its anchor while thirty others
//! support it. It is re-anchored on the contributors that remain, and
//! deleted only if none do — it quotes node names and counts, never episode
//! text. Reported as `rederived`. The privacy path only: the TUI's undoable
//! delete takes the belief like any fact the episode founded, so undo
//! restores it exactly, and the nightly `link` re-mints it from what
//! survives if the delete stands. A belief already closed keeps its verdict
//! and is only re-anchored. `entity_proposal` is in the same
//! class: its evidence is an alias and dates mined across many episodes
//! (a floor of eight), names and counts rather than any one episode's text.
//!
//! **What it deliberately leaves**: the `episode_tombstone` row
//! (source, source_id — identifiers only, so re-ingest cannot resurrect it);
//! nodes the episode created or named, which carry no episode provenance —
//! the ones left with no mention and no fact are *reported* as
//! `orphaned_nodes`, never deleted, because a node can be the owner's own
//! (an accepted task) whatever minted it; belief changes the episode caused
//! in *other* facts (a correction's supersede, a class demotion), which are
//! the owner's rulings and are not reverted; `query_log`, which holds
//! query text with no link to any episode; and **every copy of the database
//! outside this file** — the `graph.db.pre-*.bak` backups and `backups/`, a
//! `fork` (a full copy under its own key), and a `decrypt --out` plaintext
//! snapshot. Nothing here can reach them, [`scrub`] included, and the report
//! cannot know they exist; removing them is the operator's job.
//!
//! The logical purge is not the physical one. Deleted rows leave bytes in
//! free pages and the WAL, and an FTS5 delete only appends a tombstone to
//! the index — the tokens stay in the old segment until a merge. The
//! privacy path therefore runs an FTS `optimize`; [`scrub`] adds a WAL
//! checkpoint and `VACUUM`; `secure_delete` is set by the CLI's redact before
//! the purge (`secure_delete_on`), so a library caller sets it itself.

use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use std::collections::BTreeSet;

/// What one redaction removed. Serialized as the CLI's `--json` output.
#[derive(Debug, Default, Serialize)]
pub struct RedactReport {
    /// Live episodes deleted.
    pub redacted: usize,
    /// Their uids.
    pub uids: Vec<String>,
    /// Facts the episodes founded, deleted with them.
    pub facts: usize,
    /// Candidates extracted from them.
    pub candidates: usize,
    /// Their sightings of facts they did not found — removed, the facts kept.
    pub observations: usize,
    /// Undo snapshots of them (privacy path only).
    pub undo_snapshots: usize,
    /// `event_log` rows naming them or their facts.
    pub events: usize,
    /// `retrieval_touch` rows for them or their facts.
    pub touches: usize,
    /// Generated node summaries cleared for regeneration.
    pub summaries_cleared: usize,
    /// Whether the FTS indexes were merged so deleted tokens are gone.
    pub fts_optimized: bool,
    /// Nodes the episodes touched that now have no mention and no fact —
    /// kept, listed so the owner can decide.
    pub orphaned_nodes: Vec<String>,
    /// Derived beliefs this episode was the newest contributor to, kept and
    /// re-anchored on the contributors that remain (deleted, and counted in
    /// `facts`, when none did).
    pub rederived: usize,
    /// Of those, the beliefs that fell below the floor the linker needs to
    /// mint one (`NPMI_MIN_COOCCUR` shared episodes) and were closed in valid
    /// time — logged as `belief_decayed`, as the nightly decay logs it.
    pub derived_closed: usize,
    /// Whether a tombstone was written for an identity that matched no
    /// episode (`--tombstone-absent`): the redaction arrived before the
    /// ingest, and the tombstone is what makes that ingest a no-op.
    pub tombstoned_absent: bool,
}

/// Which door a redaction came through.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    /// `mecha-graph redact`: no copy survives, undo snapshots included.
    Privacy,
    /// The TUI's delete: a snapshot was just written and must survive.
    Undoable,
}

struct Target {
    id: i64,
    uid: String,
    source: String,
    source_id: String,
}

/// Redact the episode with this uid (privacy path).
///
/// An unknown uid is not an error: nothing is deleted and `redacted` is 0 —
/// but an undo snapshot left under that uid by an earlier TUI delete is
/// still purged.
pub fn redact_uid(conn: &Connection, uid: &str) -> Result<RedactReport> {
    let targets = targets(
        conn,
        "SELECT id, uid, source, source_id FROM episode WHERE uid = ?1",
        params![uid],
    )?;
    let identities = targets
        .iter()
        .map(|t| (t.source.clone(), t.source_id.clone()))
        .collect();
    in_savepoint(conn, "redact_uid", || {
        redact_targets(
            conn,
            &targets,
            &[uid.to_string()],
            identities,
            Mode::Privacy,
        )
    })
}

/// Redact every episode with this (source, source_id) (privacy path).
///
/// `UNIQUE(source, source_id)` makes that zero or one today; the loop does
/// not depend on it. Zero is success with `redacted: 0` — the item may never
/// have been ingested — and still purges any undo snapshot of it.
///
/// **Zero is also the one case where the caller's intent is knowable and the
/// store's state is not**: a redaction can arrive before the ingest it is
/// meant to prevent (a conversation deleted while its distill is in flight,
/// a session file not yet swept by tonight's cursor). `tombstone_absent`
/// writes the tombstone anyway, so that ingest lands as `Tombstoned`. It is
/// the caller's to ask for, never the default: a mistyped id would
/// otherwise block a legitimate future item forever — which is a risk a
/// caller holding an exact id (mecha, naming its own session) does not run.
pub fn redact_source(
    conn: &Connection,
    source: &str,
    source_id: &str,
    tombstone_absent: bool,
) -> Result<RedactReport> {
    let targets = targets(
        conn,
        "SELECT id, uid, source, source_id FROM episode WHERE source = ?1 AND source_id = ?2",
        params![source, source_id],
    )?;
    in_savepoint(conn, "redact_source", || {
        let mut rep = redact_targets(
            conn,
            &targets,
            &[],
            vec![(source.to_string(), source_id.to_string())],
            Mode::Privacy,
        )?;
        if targets.is_empty() && tombstone_absent {
            conn.execute(
                "INSERT OR IGNORE INTO episode_tombstone (source, source_id) VALUES (?1, ?2)",
                params![source, source_id],
            )?;
            rep.tombstoned_absent = true;
        }
        Ok(rep)
    })
}

/// Purge one undo snapshot of a deleted episode as the privacy path would:
/// the snapshot, and the telemetry, pointers and alarms the TUI's delete
/// left for it — they are reachable only through what the snapshot holds.
/// `undo --discard`'s door: dropping the row alone would strand them where no
/// later redact can find them.
pub(crate) fn purge_snapshot(
    conn: &Connection,
    uid: &str,
    identity: (String, String),
) -> Result<RedactReport> {
    in_savepoint(conn, "purge_snapshot", || {
        redact_targets(conn, &[], &[uid.to_string()], vec![identity], Mode::Privacy)
    })
}

/// The TUI's purge of one live episode — no undo purge, no FTS optimize
/// (a merge of the whole index per keystroke is the wrong price for an
/// undoable delete; the privacy path pays it).
pub(crate) fn redact_uid_undoable(conn: &Connection, uid: &str) -> Result<RedactReport> {
    let targets = targets(
        conn,
        "SELECT id, uid, source, source_id FROM episode WHERE uid = ?1",
        params![uid],
    )?;
    redact_targets(conn, &targets, &[], vec![], Mode::Undoable)
}

fn targets(conn: &Connection, sql: &str, p: impl rusqlite::Params) -> Result<Vec<Target>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(p, |r| {
            Ok(Target {
                id: r.get(0)?,
                uid: r.get(1)?,
                source: r.get(2)?,
                source_id: r.get(3)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Run `f` inside a savepoint: all of it lands or none of it does. A
/// half-applied redaction is the worst outcome available — the episode
/// gone from search while its facts, or its text, survive.
///
/// A savepoint rather than a transaction because it nests: the TUI's delete
/// wraps its snapshot and this purge in one.
pub(crate) fn in_savepoint<T>(
    conn: &Connection,
    name: &str,
    f: impl FnOnce() -> Result<T>,
) -> Result<T> {
    conn.execute_batch(&format!("SAVEPOINT {name}"))?;
    match f() {
        Ok(v) => {
            conn.execute_batch(&format!("RELEASE {name}"))?;
            Ok(v)
        }
        Err(e) => {
            let _ = conn.execute_batch(&format!("ROLLBACK TO {name}; RELEASE {name}"));
            Err(e)
        }
    }
}

fn redact_targets(
    conn: &Connection,
    targets: &[Target],
    extra_uids: &[String],
    identities: Vec<(String, String)>,
    mode: Mode,
) -> Result<RedactReport> {
    let mut rep = RedactReport::default();
    let mut touched: BTreeSet<String> = BTreeSet::new();
    for t in targets {
        purge_one(conn, t, mode, &mut rep, &mut touched)?;
    }

    if mode == Mode::Privacy {
        let mut uids: Vec<String> = targets.iter().map(|t| t.uid.clone()).collect();
        uids.extend(extra_uids.iter().cloned());
        purge_undo(conn, &uids, &identities, &mut rep, &mut touched)?;
    }

    let touched: Vec<String> = touched.into_iter().collect();
    // Derived state is re-derived only on the privacy path. The TUI's delete
    // is undoable, and `undo_last` restores rows, not rollups or generated
    // summaries — so rebuilding them here made Ctrl-Z answer "when did I
    // last talk to P?" as if P had never been seen. There, the nightly
    // rebuild and the summariser catch up, as they did before.
    if mode == Mode::Privacy {
        crate::rollup::rebuild_person_interactions_for(conn, &touched)?;
        rep.summaries_cleared = conn.execute(
            "UPDATE node_context SET summary = '', summary_updated_at = NULL
             WHERE node_id IN (SELECT value FROM json_each(?1)) AND summary <> ''",
            params![serde_json::to_string(&touched)?],
        )?;
    }

    for n in &touched {
        let still_there: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM nodes WHERE id = ?1)
                AND NOT EXISTS (SELECT 1 FROM mention WHERE node_id = ?1)
                AND NOT EXISTS (SELECT 1 FROM fact WHERE subject_id = ?1 OR object_id = ?1)",
            params![n],
            |r| r.get(0),
        )?;
        if still_there {
            rep.orphaned_nodes.push(n.clone());
        }
    }

    if mode == Mode::Privacy && (rep.redacted > 0 || rep.undo_snapshots > 0) {
        optimize_fts(conn)?;
        rep.fts_optimized = true;
    }
    Ok(rep)
}

/// Every row that holds one live episode, in dependency order.
fn purge_one(
    conn: &Connection,
    t: &Target,
    mode: Mode,
    rep: &mut RedactReport,
    touched: &mut BTreeSet<String>,
) -> Result<()> {
    let id = t.id;

    // Tombstone first: whole-file sources (ICS, reflect, mbox) re-present
    // every item on every sync and would otherwise resurrect this episode.
    conn.execute(
        "INSERT OR IGNORE INTO episode_tombstone (source, source_id) VALUES (?1, ?2)",
        params![t.source, t.source_id],
    )?;

    // What it touched, read before anything is deleted.
    for n in column::<String>(
        conn,
        "SELECT node_id FROM mention WHERE episode_id = ?1
         UNION SELECT node_id FROM unlinked_mention WHERE episode_id = ?1
         UNION SELECT node_id FROM person_interaction WHERE last_episode_id = ?2",
        params![id, t.uid],
    )? {
        touched.insert(n);
    }
    let founded: Vec<(i64, String, String, Option<String>)> = {
        let mut stmt =
            conn.prepare("SELECT id, uid, subject_id, object_id FROM fact WHERE episode_id = ?1")?;
        let rows = stmt
            .query_map(params![id], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    // **A derived belief is re-derived, not deleted** (owner's ruling,
    // 2026-09-28). A co-occurrence fact is anchored on its *newest*
    // contributor (`fact::attach_derivation`), so `episode_id = this` means
    // "cited here", not "founded here": thirty episodes may support it. It
    // is re-derived from what survives once this episode's mentions are
    // gone, and deleted only if nothing does. It quotes node names and
    // counts, never episode text, so keeping it keeps nothing of this one.
    //
    // **The privacy path only.** The TUI's delete is undoable, and undo
    // restores rows, not re-derivations: a belief re-anchored, re-rendered or
    // closed there would stay so after Ctrl-Z. There the belief goes like any
    // fact the episode founded — captured in the snapshot, restored exactly
    // by undo — and the nightly `link` re-mints it from what survives if the
    // delete stands.
    let derived: BTreeSet<i64> = if mode == Mode::Privacy {
        column::<i64>(
            conn,
            "SELECT id FROM fact WHERE episode_id = ?1 AND extractor IS 'npmi' AND object_id IS NOT NULL",
            params![id],
        )?
        .into_iter()
        .collect()
    } else {
        BTreeSet::new()
    };
    for fid in &derived {
        // Its founding observation leaves this episode now, so the sightings
        // purge below leaves it alone and the re-derivation re-points it.
        conn.execute(
            "UPDATE fact_observation SET episode_id = NULL
             WHERE fact_id = ?1 AND episode_id = ?2 AND kind = 'asserted'",
            params![fid, id],
        )?;
    }
    for (_, _, subject, object) in &founded {
        touched.insert(subject.clone());
        if let Some(o) = object {
            touched.insert(o.clone());
        }
    }
    // Its sightings of facts founded elsewhere, and whether any of them
    // moved the counter. `fact::assert_fact` counts a corroboration once
    // per distinct episode and never for probe/agent sources; undoing it
    // follows the same rule.
    let sighted: Vec<(i64, bool)> = {
        let mut stmt = conn.prepare(
            "SELECT fact_id, MAX(kind = 'corroborated') FROM fact_observation
             WHERE episode_id = ?1
               AND fact_id NOT IN (SELECT id FROM fact WHERE episode_id = ?1
                                   AND NOT (extractor IS 'npmi' AND object_id IS NOT NULL))
             GROUP BY fact_id",
        )?;
        let rows = stmt
            .query_map(params![id], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    let counted = source_counts(&t.source);

    // Facts it founded, with everything keyed on them — less the derived
    // beliefs, which are settled once the mentions are gone.
    let mut fact_uids: Vec<String> = founded
        .iter()
        .filter(|f| !derived.contains(&f.0))
        .map(|f| f.1.clone())
        .collect();
    for (fid, fuid, _, _) in founded.iter().filter(|f| !derived.contains(&f.0)) {
        conn.execute("DELETE FROM vec_fact WHERE fact_id = ?1", params![fid])?;
        // The alarm's first sighting is deliberately never overwritten, and
        // undo does not restore it — so the TUI's delete leaves it, or Ctrl-Z
        // would re-raise a long-standing collapse as new. The privacy path
        // takes it here, or with the snapshot in `purge_undo`.
        if mode == Mode::Privacy {
            conn.execute(
                "DELETE FROM cooccurrence_alarm WHERE fact_uid = ?1",
                params![fuid],
            )?;
        }
        conn.execute(
            "DELETE FROM fact_observation WHERE fact_id = ?1",
            params![fid],
        )?;
    }
    for (fid, _, _, _) in founded.iter().filter(|f| !derived.contains(&f.0)) {
        rep.facts += conn.execute("DELETE FROM fact WHERE id = ?1", params![fid])?;
    }

    // Its sightings of other facts: the row goes, the belief re-derives.
    rep.observations += conn.execute(
        "DELETE FROM fact_observation WHERE episode_id = ?1",
        params![id],
    )?;
    for (fid, corroborated) in &sighted {
        // Only a fact still here: `sighted` and the founded-fact delete are
        // two predicates over one table, and a divergence between them (a
        // NULL `extractor` once made one) must not sink the redaction on a
        // recompute of a row it just deleted.
        let live: bool = conn.query_row(
            "SELECT EXISTS (SELECT 1 FROM fact WHERE id = ?1)",
            params![fid],
            |r| r.get(0),
        )?;
        if !live {
            continue;
        }
        if *corroborated && counted {
            conn.execute(
                "UPDATE fact SET observation_count = MAX(observation_count - 1, 1) WHERE id = ?1",
                params![fid],
            )?;
        }
        crate::fact::recompute_confidence(conn, *fid)?;
    }

    // Candidates extracted FROM this episode carry its content in their
    // payloads — true delete takes them too, reviewed or not, with the
    // verdicts and the vectors computed from their statements.
    let candidates = column::<i64>(
        conn,
        "SELECT id FROM fact_candidate WHERE episode_id = ?1",
        params![id],
    )?;
    for c in &candidates {
        conn.execute(
            "DELETE FROM agent_verdict WHERE candidate_id = ?1",
            params![c],
        )?;
        conn.execute(
            "DELETE FROM candidate_embedding WHERE candidate_id = ?1",
            params![c],
        )?;
        conn.execute(
            "DELETE FROM vec_rejected WHERE candidate_id = ?1",
            params![c],
        )?;
    }
    rep.candidates += conn.execute(
        "DELETE FROM fact_candidate WHERE episode_id = ?1",
        params![id],
    )?;

    conn.execute("DELETE FROM vec_episode WHERE episode_id = ?1", params![id])?;
    for table in [
        "mention",
        "episode_enrichment",
        "episode_raw",
        "episode_annotation",
        "extract_state",
        "unlinked_mention",
    ] {
        conn.execute(
            &format!("DELETE FROM {table} WHERE episode_id = ?1"),
            params![id],
        )?;
    }
    // The derived beliefs, now that this episode's mentions are gone: the
    // contributors that remain re-anchor it, or there are none and it goes
    // like any fact this episode founded.
    for (fid, fuid, subject, object) in founded.iter().filter(|f| derived.contains(&f.0)) {
        let Some(object) = object else { continue };
        let survivors = crate::linkers::shared_episodes(conn, subject, object)?;
        if survivors.is_empty() {
            conn.execute("DELETE FROM vec_fact WHERE fact_id = ?1", params![fid])?;
            if mode == Mode::Privacy {
                conn.execute(
                    "DELETE FROM cooccurrence_alarm WHERE fact_uid = ?1",
                    params![fuid],
                )?;
            }
            conn.execute(
                "DELETE FROM fact_observation WHERE fact_id = ?1",
                params![fid],
            )?;
            rep.facts += conn.execute("DELETE FROM fact WHERE id = ?1", params![fid])?;
            fact_uids.push(fuid.clone());
        } else {
            crate::fact::attach_derivation(conn, fuid, &survivors)?;
            // A belief already closed or retracted (the nightly decay closes
            // one and leaves its anchor) keeps that verdict: it is only
            // re-anchored, never re-rendered or closed a second time — which
            // `close_valid_time` refuses, and a refusal here would roll back
            // the whole redaction and leave the episode un-redactable.
            let open: bool = conn.query_row(
                "SELECT valid_to IS NULL AND invalidated_at IS NULL FROM fact WHERE id = ?1",
                params![fid],
                |r| r.get(0),
            )?;
            if !open {
                rep.rederived += 1;
                continue;
            }
            // Re-derived means the statistic too, not only the anchor: the
            // statement *is* the derivation ("3 shared episodes, NPMI .."),
            // and one that still counted the redacted episode would assert
            // a count it no longer has. The nightly decay's own rules: the
            // numbers re-rendered (and the stale vector dropped for re-embed),
            // or — below the floor the linker needs to mint it at all — a
            // valid-time close, true then and not now, unless the owner
            // verified it.
            match crate::verify::rederive_npmi(conn, subject, object)? {
                Some((npmi, co)) => {
                    let statement: String = conn.query_row(
                        "SELECT statement FROM fact WHERE id = ?1",
                        params![fid],
                        |r| r.get(0),
                    )?;
                    if let Some(fresh) = crate::decay::render_statement(&statement, co, npmi) {
                        if fresh != statement {
                            conn.execute(
                                "UPDATE fact SET statement = ?2 WHERE id = ?1",
                                params![fid, fresh],
                            )?;
                            conn.execute("DELETE FROM vec_fact WHERE fact_id = ?1", params![fid])?;
                        }
                    }
                }
                // `None` is two answers, kept apart as the decay sweep keeps
                // them: below the co-occurrence floor, the belief no longer
                // holds and is closed (and logged); a corpus too small to
                // compute NPMI over says nothing about the pair, so only the
                // count is restated and the belief stays open.
                None => {
                    let co = survivors.len() as i64;
                    let closes = co < crate::linkers::NPMI_MIN_COOCCUR
                        && !crate::fact::is_user_verified(conn, *fid)?;
                    if closes {
                        crate::fact::close_valid_time(conn, fuid, None)?;
                        let payload = serde_json::json!({
                            "class": "npmi", "reason": "redacted_contributor",
                            "shared_now": co,
                            "npmi_min_cooccur": crate::linkers::NPMI_MIN_COOCCUR,
                        });
                        crate::ledger::log_event(
                            conn,
                            "belief_decayed",
                            Some(fuid),
                            Some(&payload.to_string()),
                        )?;
                        rep.derived_closed += 1;
                    } else {
                        // Open — the corpus too small to compute over, or the
                        // owner verified it and the hold keeps it open under
                        // the floor — so it states the count it has now,
                        // never one only the redacted episode made true.
                        let statement: String = conn.query_row(
                            "SELECT statement FROM fact WHERE id = ?1",
                            params![fid],
                            |r| r.get(0),
                        )?;
                        if let Some((_, npmi)) = crate::decay::parse_cooccurrence(&statement) {
                            if let Some(fresh) =
                                crate::decay::render_statement(&statement, co, npmi)
                            {
                                if fresh != statement {
                                    conn.execute(
                                        "UPDATE fact SET statement = ?2 WHERE id = ?1",
                                        params![fid, fresh],
                                    )?;
                                    conn.execute(
                                        "DELETE FROM vec_fact WHERE fact_id = ?1",
                                        params![fid],
                                    )?;
                                }
                            }
                        }
                    }
                }
            }
            rep.rederived += 1;
        }
    }
    // Pointers and telemetry only on the privacy path: `undo_last` restores
    // rows the snapshot holds, and neither of these is in it — so the TUI's
    // delete must leave an event node's `source_ref`, the `event_log` rows
    // naming its facts, and their demand counters for Ctrl-Z to find. The
    // snapshot keeps the body verbatim anyway; when the privacy path later
    // takes the snapshot, `purge_undo` takes these with it.
    if mode == Mode::Privacy {
        conn.execute(
            "UPDATE nodes SET source_ref = NULL WHERE source_ref = ?1",
            params![t.uid],
        )?;
        purge_telemetry(conn, Some(id), None, &t.uid, &fact_uids, rep)?;
    }

    // FTS row goes with it (trg_episode_ad).
    conn.execute("DELETE FROM episode WHERE id = ?1", params![id])?;
    rep.redacted += 1;
    rep.uids.push(t.uid.clone());
    Ok(())
}

/// Does a sighting from this source move a fact's corroboration counter?
/// The rule in `fact::assert_fact`: probe and agent evidence never does.
pub(crate) fn source_counts(source: &str) -> bool {
    !(source.starts_with("probe") || source.starts_with("agent:"))
}

/// `retrieval_touch` and `event_log` rows naming an episode or its facts.
/// `ep_id` is the episode's integer id — for an episode known only from an
/// undo snapshot, the id the snapshot recorded, which is how a correction's
/// payload (`episode_id`, with its `right`/`wrong` text) is reached at all.
/// Rowids are reused once freed, so for a snapshot `id_until` bounds those
/// id matches to rows logged no later than the snapshot was taken: nothing
/// the deleted episode caused can postdate it, and a live episode that
/// inherited the id keeps the corrections the ladder reads.
fn purge_telemetry(
    conn: &Connection,
    ep_id: Option<i64>,
    id_until: Option<&str>,
    ep_uid: &str,
    fact_uids: &[String],
    rep: &mut RedactReport,
) -> Result<()> {
    let facts = serde_json::to_string(fact_uids)?;
    rep.touches += conn.execute(
        "DELETE FROM retrieval_touch
         WHERE (kind = 'episode' AND ref_id = ?1)
            OR (kind = 'fact' AND ref_id IN (SELECT value FROM json_each(?2)))",
        params![ep_uid, facts],
    )?;
    // One pass over the log. CASE rather than AND, because SQLite does not
    // promise to short-circuit AND and json_extract raises on bad JSON.
    // Payload keys are the ones the writers use: corrections
    // (`episode_id`, `trigger_episode`, `trigger_fact`) and flag_shown
    // (`fact_uids`); every other kind names its fact in `ref`.
    rep.events += conn.execute(
        "DELETE FROM event_log
         WHERE ref = ?1
            OR ref IN (SELECT value FROM json_each(?2))
            OR CASE WHEN json_valid(payload) THEN
                   ((json_extract(payload, '$.episode_id') = ?3
                     OR json_extract(payload, '$.trigger_episode') = ?3)
                    AND (?4 IS NULL OR ts <= ?4))
                OR json_extract(payload, '$.trigger_fact') IN (SELECT value FROM json_each(?2))
                OR EXISTS (SELECT 1 FROM json_each(payload, '$.fact_uids') j
                           WHERE j.value IN (SELECT value FROM json_each(?2)))
               ELSE 0 END",
        params![ep_uid, facts, ep_id, id_until],
    )?;
    Ok(())
}

/// Undo snapshots of the redacted items — by uid, and by (source,
/// source_id) so one left by an earlier TUI delete is found too — plus the
/// telemetry naming the episode and facts such a snapshot held.
fn purge_undo(
    conn: &Connection,
    uids: &[String],
    identities: &[(String, String)],
    rep: &mut RedactReport,
    touched: &mut BTreeSet<String>,
) -> Result<()> {
    let mut ids: BTreeSet<i64> = BTreeSet::new();
    for uid in uids {
        ids.extend(column::<i64>(
            conn,
            "SELECT id FROM undo_log WHERE ref_uid = ?1",
            params![uid],
        )?);
    }
    for (source, source_id) in identities {
        ids.extend(column::<i64>(
            conn,
            "SELECT id FROM undo_log
             WHERE CASE WHEN json_valid(snapshot) THEN
                       json_extract(snapshot, '$.episode[0][2]') = ?1
                   AND json_extract(snapshot, '$.episode[0][3]') = ?2
                   ELSE 0 END",
            params![source, source_id],
        )?);
    }
    for log_id in ids {
        let row: Option<(String, String)> = conn
            .query_row(
                "SELECT snapshot, created_at FROM undo_log WHERE id = ?1",
                params![log_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let (snapshot, taken_at) = match row {
            Some((s, at)) => (Some(s), Some(at)),
            None => (None, None),
        };
        // A snapshot that cannot be read cannot be purged *of* anything: the
        // id that reaches its corrections and the nodes to re-derive are
        // inside it. An error, so the savepoint rolls back and the report
        // never counts it as taken.
        let parsed =
            match snapshot {
                Some(s) => Some(serde_json::from_str::<serde_json::Value>(&s).map_err(|e| {
                    crate::error::Error::Parse(format!("undo snapshot {log_id}: {e}"))
                })?),
                None => None,
            };
        if let Some(v) = parsed {
            // Snapshot rows are column arrays in EPISODE_COLS / FACT_COLS
            // order: uid is column 1 of both.
            let ep_uid = v["episode"][0][1].as_str().unwrap_or_default().to_string();
            let fact_uids: Vec<String> = v["facts"]
                .as_array()
                .map(|fs| {
                    fs.iter()
                        .filter_map(|f| f[1].as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default();
            // Less the facts still live: a derived belief the snapshot's
            // episode once anchored was re-derived and survives, and its
            // alarm and telemetry are the surviving belief's, not the
            // snapshot's.
            // A failed read is an error, never "not live": the savepoint rolls
            // the redaction back rather than purge a live belief's history.
            let mut fact_uids_dead = Vec::with_capacity(fact_uids.len());
            for u in fact_uids {
                let live: bool = conn.query_row(
                    "SELECT EXISTS (SELECT 1 FROM fact WHERE uid = ?1)",
                    params![u],
                    |r| r.get(0),
                )?;
                if !live {
                    fact_uids_dead.push(u);
                }
            }
            let fact_uids = fact_uids_dead;
            conn.execute(
                "DELETE FROM cooccurrence_alarm WHERE fact_uid IN (SELECT value FROM json_each(?1))",
                params![serde_json::to_string(&fact_uids)?],
            )?;
            if !ep_uid.is_empty() {
                // EPISODE_COLS starts with `id`, as `undo_last` reads it.
                let ep_id = v["episode"][0][0].as_i64();
                // Bounded by when the snapshot was taken: rowids are reused
                // once freed, so after it the id may name a *live* episode
                // whose corrections the ladder still reads. Anything the
                // deleted episode caused was logged before its snapshot.
                purge_telemetry(conn, ep_id, taken_at.as_deref(), &ep_uid, &fact_uids, rep)?;
                // The TUI's delete left the pointer for undo; with the
                // snapshot going, nothing can restore what it points at.
                conn.execute(
                    "UPDATE nodes SET source_ref = NULL WHERE source_ref = ?1",
                    params![ep_uid],
                )?;
            }
            // The nodes the deleted copy mentioned (MENTION_COLS: node_id is
            // column 1). Its TUI delete left their rollup and summary for undo
            // to find; with the snapshot gone, this path owns re-deriving them.
            if let Some(ms) = v["mentions"].as_array() {
                touched.extend(ms.iter().filter_map(|m| m[1].as_str().map(str::to_string)));
            }
        }
        rep.undo_snapshots +=
            conn.execute("DELETE FROM undo_log WHERE id = ?1", params![log_id])?;
    }
    Ok(())
}

fn column<T: rusqlite::types::FromSql>(
    conn: &Connection,
    sql: &str,
    p: impl rusqlite::Params,
) -> Result<Vec<T>> {
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(p, |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Merge each FTS5 index into one segment, dropping deleted entries.
///
/// An FTS5 delete appends a delete marker; the deleted document's tokens
/// and positions stay in the older segment — enough to rebuild most of the
/// text — until an automerge happens to reach it. `optimize` is the only
/// way to say "now". It also clears tokens left by every earlier edit of an
/// episode's body, which the delete trigger never saw.
pub fn optimize_fts(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "INSERT INTO fts_episode(fts_episode) VALUES('optimize');
         INSERT INTO fts_fact(fts_fact) VALUES('optimize');",
    )?;
    Ok(())
}

/// Turn on `PRAGMA secure_delete` for this connection and report whether
/// it took: freed content is overwritten with zeros instead of lingering
/// in free pages. Read back rather than assumed, because a build compiled
/// with `SQLITE_SECURE_DELETE` off-and-locked would silently ignore it.
pub fn secure_delete_on(conn: &Connection) -> Result<bool> {
    conn.pragma_update(None, "secure_delete", "ON")?;
    let v: i64 = conn.pragma_query_value(None, "secure_delete", |r| r.get(0))?;
    Ok(v == 1)
}

/// What the physical scrub did.
#[derive(Debug, Default, Serialize)]
pub struct ScrubReport {
    pub vacuumed: bool,
    /// `PRAGMA wal_checkpoint(TRUNCATE)` after the vacuum: `busy` is 1 when
    /// another connection's read kept the WAL from being reset, in which
    /// case the old frames survive until it is (`log`/`checkpointed` are
    /// -1 on a database not in WAL mode).
    pub wal_busy: i64,
    pub wal_log: i64,
    pub wal_checkpointed: i64,
}

/// Checkpoint, `VACUUM`, checkpoint again with TRUNCATE.
///
/// The first checkpoint moves the redaction into the main file; `VACUUM`
/// rebuilds that file so no free page carries the deleted text; the last
/// checkpoint writes the vacuumed image back and truncates the WAL, whose
/// frames would otherwise still hold old page images. Works under SQLCipher:
/// the rebuilt file is encrypted under the same key.
pub fn scrub(conn: &Connection) -> Result<ScrubReport> {
    let _: (i64, i64, i64) = checkpoint(conn)?;
    conn.execute_batch("VACUUM")?;
    let (busy, log, done) = checkpoint(conn)?;
    Ok(ScrubReport {
        vacuumed: true,
        wal_busy: busy,
        wal_log: log,
        wal_checkpointed: done,
    })
}

fn checkpoint(conn: &Connection) -> Result<(i64, i64, i64)> {
    Ok(conn.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
        Ok((r.get(0)?, r.get(1)?, r.get(2)?))
    })?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{
        add_mention, annotate_episode, redact_episode, redact_episode_undoable, snapshot_edit,
        store_raw, undo_last, upsert_episode, Episode, IngestOutcome,
    };
    use crate::fact::{assert_fact, propose_fact, record_verdict, ProposedFact};
    use crate::graph::{upsert_node, Node};

    /// A word nothing else in a fresh store contains, and that porter
    /// stemming leaves alone — so it can be looked for in raw FTS segments.
    const NEEDLE: &str = "zanzibarquux";

    fn ep(source: &str, source_id: &str, body: &str) -> Episode {
        Episode {
            id: 0,
            uid: String::new(),
            source: source.into(),
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

    fn uid_of(conn: &Connection, id: i64) -> String {
        conn.query_row("SELECT uid FROM episode WHERE id = ?1", params![id], |r| {
            r.get(0)
        })
        .unwrap()
    }

    fn vec768(x: f32) -> String {
        serde_json::to_string(&vec![x; 768]).unwrap()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    struct Fixture {
        id: i64,
        uid: String,
        /// A fact another episode founded and this one only corroborated.
        other_fact: i64,
        /// A fact this episode founded.
        own_fact: i64,
        candidate: i64,
    }

    /// One episode written into every table the store has that can hold
    /// its id, its uid, its text, or text derived from it.
    fn fixture(conn: &Connection, source: &str, source_id: &str) -> Fixture {
        upsert_node(conn, &Node::new("ada", "person", "Ada")).unwrap();
        upsert_node(conn, &Node::new("org-w", "org", "Westfield")).unwrap();
        upsert_node(conn, &Node::new("ev-1", "event", "Standup")).unwrap();

        // Another episode founds a belief the redacted one only corroborates.
        let (o, _) = upsert_episode(conn, &ep("note", "other", "Ada works at Westfield")).unwrap();
        let other_uid = assert_fact(
            conn,
            "ada",
            "works_at",
            Some("org-w"),
            None,
            "Ada works at Westfield",
            Some(o),
            None,
            0.8,
            "test",
        )
        .unwrap();

        let body = format!("A {NEEDLE} conversation with Ada, who works at Westfield");
        let (id, _) = upsert_episode(conn, &ep(source, source_id, &body)).unwrap();
        let uid = uid_of(conn, id);
        add_mention(conn, id, "ada", "manual", 1.0).unwrap();
        store_raw(conn, id, &format!("raw {NEEDLE} transcript")).unwrap();
        annotate_episode(conn, id, "note", &format!("{NEEDLE} note")).unwrap();
        conn.execute(
            "INSERT INTO episode_enrichment (episode_id, payload) VALUES (?1, ?2)",
            params![id, format!("{{\"summary\":\"{NEEDLE}\"}}")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO extract_state (episode_id, model) VALUES (?1, 'm')",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO unlinked_mention (alias, node_id, episode_id) VALUES ('ada', 'ada', ?1)",
            params![id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vec_episode (episode_id, embedding) VALUES (?1, ?2)",
            params![id, vec768(0.25)],
        )
        .unwrap();

        // Its sighting of the other belief, and a belief of its own.
        assert_fact(
            conn,
            "ada",
            "works_at",
            Some("org-w"),
            None,
            "Ada works at Westfield",
            Some(id),
            None,
            0.8,
            "test",
        )
        .unwrap();
        let own_uid = assert_fact(
            conn,
            "ada",
            "about",
            None,
            Some(NEEDLE),
            &format!("Ada discussed {NEEDLE}"),
            Some(id),
            None,
            0.8,
            "llm",
        )
        .unwrap();
        let own_fact: i64 = conn
            .query_row(
                "SELECT id FROM fact WHERE uid = ?1",
                params![own_uid],
                |r| r.get(0),
            )
            .unwrap();
        let other_fact: i64 = conn
            .query_row(
                "SELECT id FROM fact WHERE uid = ?1",
                params![other_uid],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO vec_fact (fact_id, embedding) VALUES (?1, ?2)",
            params![own_fact, vec768(0.5)],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO cooccurrence_alarm (fact_uid, stated_co, observed_co) VALUES (?1, 3, 1)",
            params![own_uid],
        )
        .unwrap();

        // A candidate extracted from it, with everything keyed on that.
        let candidate = propose_fact(
            conn,
            &ProposedFact {
                subject: "Ada".into(),
                predicate: "about".into(),
                statement: format!("Ada mentioned {NEEDLE}"),
                ..Default::default()
            },
            "llm",
            Some(id),
        )
        .unwrap();
        record_verdict(
            conn,
            candidate,
            "corroboration",
            "accept",
            &format!("{NEEDLE} in text"),
            None,
        )
        .unwrap();
        conn.execute(
            "INSERT INTO candidate_embedding (candidate_id, text_hash, dims, embedding)
             VALUES (?1, 'h', 1, x'0000803f')",
            params![candidate],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vec_rejected (candidate_id, embedding) VALUES (?1, ?2)",
            params![candidate, vec768(0.75)],
        )
        .unwrap();

        // Telemetry naming it and its facts; one unrelated row that stays.
        crate::ledger::log_event(
            conn,
            "correction",
            Some(&other_uid),
            Some(&serde_json::json!({"episode_id": id, "right": NEEDLE}).to_string()),
        )
        .unwrap();
        crate::ledger::log_event(
            conn,
            "flag_shown",
            Some("ada"),
            Some(&serde_json::json!({"fact_uids": [own_uid]}).to_string()),
        )
        .unwrap();
        crate::ledger::log_event(
            conn,
            "sweep_target",
            Some(&other_uid),
            Some(&serde_json::json!({"trigger_episode": id}).to_string()),
        )
        .unwrap();
        crate::ledger::log_event(conn, "class_promoted", Some("llm·about"), Some("{}")).unwrap();
        for (kind, r) in [("episode", &uid), ("fact", &own_uid)] {
            conn.execute(
                "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
                 VALUES (?1, ?2, 1, datetime('now'), datetime('now'))",
                params![kind, r],
            )
            .unwrap();
        }

        // Views derived from it.
        crate::rollup::touch_person(conn, "ada", &uid, source, "2026-08-01 12:00:00").unwrap();
        crate::context::set_summary(conn, "ada", &format!("Ada talked about {NEEDLE}.")).unwrap();
        conn.execute(
            "UPDATE nodes SET source_ref = ?1 WHERE id = 'ev-1'",
            params![uid],
        )
        .unwrap();

        // The TUI's edit snapshot of it.
        snapshot_edit(conn, id).unwrap();

        Fixture {
            id,
            uid,
            other_fact,
            own_fact,
            candidate,
        }
    }

    /// Every cell of every table (virtual, FTS shadow and vec0 shadow
    /// included) that still holds one of `needles`, or an `episode_id`
    /// equal to `ep_id`. Schema-driven, so a table a later migration adds
    /// is covered without anyone remembering to list it here.
    fn traces(conn: &Connection, needles: &[&str], ep_id: i64) -> Vec<String> {
        let tables: Vec<String> = column(
            conn,
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name <> '_migrations'
             ORDER BY name",
            [],
        )
        .unwrap();
        let mut found = vec![];
        for t in tables {
            let mut stmt = conn.prepare(&format!("SELECT * FROM \"{t}\"")).unwrap();
            let cols: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let mut rows = stmt.query([]).unwrap();
            while let Some(row) = rows.next().unwrap() {
                for (i, col) in cols.iter().enumerate() {
                    use rusqlite::types::ValueRef::*;
                    match row.get_ref(i).unwrap() {
                        Text(b) | Blob(b) => {
                            for n in needles {
                                if b.windows(n.len()).any(|w| w == n.as_bytes()) {
                                    found.push(format!("{t}.{col} holds {n:?}"));
                                }
                            }
                        }
                        Integer(v) if col == "episode_id" && v == ep_id => {
                            found.push(format!("{t}.episode_id = {ep_id}"));
                        }
                        _ => {}
                    }
                }
            }
        }
        found.sort();
        found.dedup();
        found
    }

    #[test]
    fn the_fixture_reaches_every_table_it_claims_to() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "agent:mecha", "sess-1");
        let t = traces(&conn, &[NEEDLE, &f.uid], f.id);
        for table in [
            "episode.body",
            "episode_raw.content",
            "episode_annotation.body",
            "episode_enrichment.payload",
            "fact.statement",
            "fact_candidate.payload",
            "agent_verdict.basis",
            "event_log.payload",
            "retrieval_touch.ref_id",
            "person_interaction.last_episode_id",
            "node_context.summary",
            "nodes.source_ref",
            "undo_log.snapshot",
            "fts_episode_data.block",
            "fts_fact_data.block",
        ] {
            assert!(
                t.iter().any(|x| x.starts_with(table)),
                "fixture misses {table}: {t:#?}"
            );
        }
    }

    /// The uid form, which is what `mecha-graph redact <uid>` has always
    /// been. Before this, it left the candidate vectors, the fact's vector
    /// and alarm, every telemetry row, the rollup's pointer, the summary,
    /// the edit snapshot, a sighting turned into anonymous support, and the
    /// deleted text's tokens in both FTS indexes.
    #[test]
    fn redacting_by_uid_leaves_no_trace_anywhere() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "agent:mecha", "sess-1");

        assert!(redact_episode(&conn, &f.uid).unwrap());

        assert_eq!(traces(&conn, &[NEEDLE, &f.uid], f.id), Vec::<String>::new());
        for (table, key, id) in [
            ("vec_fact", "fact_id", f.own_fact),
            ("fact_observation", "fact_id", f.own_fact),
            ("agent_verdict", "candidate_id", f.candidate),
            ("candidate_embedding", "candidate_id", f.candidate),
            ("vec_rejected", "candidate_id", f.candidate),
            ("vec_episode", "episode_id", f.id),
        ] {
            let n: i64 = conn
                .query_row(
                    &format!("SELECT COUNT(*) FROM {table} WHERE {key} = ?1"),
                    params![id],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(n, 0, "{table} still keyed on the redacted episode's rows");
        }
        // Telemetry about other things stays.
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM event_log"), 1);
        // The other episode's belief survives with only its own sighting —
        // not an anonymous row the confidence arithmetic would count.
        assert_eq!(
            count(
                &conn,
                &format!(
                    "SELECT COUNT(*) FROM fact_observation WHERE fact_id = {}",
                    f.other_fact
                )
            ),
            1
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM fact_observation WHERE episode_id IS NULL"
            ),
            0
        );
    }

    #[test]
    fn redacting_by_source_reports_and_tombstones() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "agent:mecha", "sess-1");
        // Same source id under another source is a different item.
        let (keep, _) = upsert_episode(&conn, &ep("note", "sess-1", "unrelated note")).unwrap();

        let rep = redact_source(&conn, "agent:mecha", "sess-1", false).unwrap();
        assert_eq!(rep.redacted, 1);
        assert_eq!(rep.uids, vec![f.uid.clone()]);
        assert_eq!((rep.facts, rep.candidates, rep.observations), (1, 1, 1));
        assert_eq!(rep.undo_snapshots, 1, "the edit snapshot");
        assert_eq!(rep.events, 3);
        assert_eq!(rep.touches, 2);
        assert_eq!(rep.summaries_cleared, 1);
        assert!(rep.fts_optimized);
        assert!(
            rep.orphaned_nodes.is_empty(),
            "ada keeps the other episode's belief"
        );
        assert_eq!(traces(&conn, &[NEEDLE, &f.uid], f.id), Vec::<String>::new());
        assert!(crate::episode::get_episode(&conn, keep).unwrap().is_some());

        // Tombstoned: the nightly re-presenting it changes nothing.
        let (_, o) = upsert_episode(&conn, &ep("agent:mecha", "sess-1", "again")).unwrap();
        assert_eq!(o, IngestOutcome::Tombstoned);
    }

    /// A redaction that beats its ingest: the conversation was deleted while
    /// its distill was in flight. Asked for, the tombstone is written anyway
    /// and the late ingest is refused; not asked for, nothing is written.
    #[test]
    fn a_redaction_that_arrives_before_its_ingest_can_still_forbid_it() {
        let conn = open_memory().unwrap();
        let rep = redact_source(&conn, "agent:mecha", "in-flight", true).unwrap();
        assert_eq!(rep.redacted, 0);
        assert!(rep.tombstoned_absent);
        let (_, outcome) = upsert_episode(
            &conn,
            &ep("agent:mecha", "in-flight", "said after the delete"),
        )
        .unwrap();
        assert!(matches!(outcome, crate::episode::IngestOutcome::Tombstoned));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM episode"), 0);
    }

    /// Ctrl-Z must round-trip: the TUI's delete leaves the rollup and the
    /// generated summary alone, because `undo_last` restores neither.
    #[test]
    fn an_undoable_delete_leaves_derived_state_for_undo_to_find() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("wren", "person", "Wren")).unwrap();
        let (e, _) = upsert_episode(&conn, &ep("bee", "b1", "Wren said hi")).unwrap();
        add_mention(&conn, e, "wren", "alias", 1.0).unwrap();
        crate::rollup::rebuild_person_interactions_for(&conn, &["wren".to_string()]).unwrap();
        let before = count(&conn, "SELECT COUNT(*) FROM person_interaction");
        assert_eq!(before, 1, "the fixture has a rollup row to lose");
        let uid: String = conn
            .query_row("SELECT uid FROM episode WHERE id = ?1", params![e], |r| {
                r.get(0)
            })
            .unwrap();
        // An event node pointing at the episode and a demand counter for it:
        // neither is in the undo snapshot, so the undoable delete must leave both.
        conn.execute(
            "UPDATE nodes SET source_ref = ?1 WHERE id = 'wren'",
            params![uid],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO retrieval_touch (kind, ref_id, first_at, last_at)
             VALUES ('episode', ?1, '2026-09-01', '2026-09-01')",
            params![uid],
        )
        .unwrap();
        assert!(redact_episode_undoable(&conn, &uid).unwrap());
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM person_interaction"),
            before
        );
        crate::episode::undo_last(&conn).unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM person_interaction"),
            before
        );
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM retrieval_touch"), 1);
        let pointer: Option<String> = conn
            .query_row("SELECT source_ref FROM nodes WHERE id = 'wren'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(pointer.as_deref(), Some(uid.as_str()));
    }

    #[test]
    fn a_source_id_that_was_never_ingested_is_success_with_nothing_done() {
        let conn = open_memory().unwrap();
        let rep = redact_source(&conn, "agent:mecha", "never-distilled", false).unwrap();
        assert_eq!(rep.redacted, 0);
        assert!(rep.uids.is_empty());
        assert!(!rep.fts_optimized);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM episode_tombstone"), 0);
    }

    /// `UNIQUE(source, source_id)` makes "every episode with that source
    /// id" at most one live row. The other copy there can be is a snapshot:
    /// deleted in the TUI, tombstone lifted, re-captured — and the privacy
    /// path must take the old snapshot as well as the live episode.
    /// Deleted in the TUI first (telemetry left for undo), redacted for
    /// privacy later: a correction logged against it is reachable only by
    /// the episode's integer id, which the snapshot recorded — and its
    /// payload carries the corrected sentence verbatim.
    #[test]
    fn a_privacy_redaction_after_a_tui_delete_takes_the_corrections_text() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "c-1");
        let payload = serde_json::json!({
            "episode_id": f.id, "wrong": "a sentence to forget", "about": "x",
        })
        .to_string();
        crate::ledger::log_event(&conn, "correction_unresolved", None, Some(&payload)).unwrap();
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM event_log WHERE payload LIKE '%a sentence to forget%'"
            ),
            1,
            "the undoable delete leaves telemetry for Ctrl-Z"
        );
        let rep = redact_source(&conn, "bee.conversation", "c-1", false).unwrap();
        assert!(rep.undo_snapshots >= 1);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM event_log WHERE payload LIKE '%a sentence to forget%'"
            ),
            0
        );
    }

    /// Undo restores a sighting of a fact founded elsewhere. If that fact was
    /// redacted with its own episode in between, the sighting is skipped —
    /// never a foreign-key failure that leaves the episode half back and the
    /// undo entry failing forever.
    #[test]
    fn undo_after_the_sighted_fact_was_redacted_restores_the_rest_and_clears_the_entry() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-1");
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        let founder: String = conn
            .query_row(
                "SELECT e.uid FROM fact x JOIN episode e ON e.id = x.episode_id WHERE x.id = ?1",
                params![f.other_fact],
                |r| r.get(0),
            )
            .unwrap();
        redact_uid(&conn, &founder).unwrap();
        crate::episode::undo_last(&conn).expect("undo must not fail on a fact that is gone");
        assert!(crate::episode::get_episode(&conn, f.id).unwrap().is_some());
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM undo_log WHERE action = 'delete'"
            ),
            0
        );
    }

    /// A freed rowid is taken by the next observation; the restored sighting
    /// must still land, and the counter move only for what landed.
    #[test]
    fn undo_restores_a_sighting_whose_old_rowid_was_reused() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-2");
        let sightings = |conn: &Connection| {
            count(
                conn,
                &format!(
                    "SELECT COUNT(*) FROM fact_observation WHERE fact_id = {} AND episode_id = {}",
                    f.other_fact, f.id
                ),
            )
        };
        assert_eq!(sightings(&conn), 1, "the fixture corroborates once");
        let before: i64 = conn
            .query_row(
                "SELECT observation_count FROM fact WHERE id = ?1",
                params![f.other_fact],
                |r| r.get(0),
            )
            .unwrap();
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        // Something else takes the freed rowid before the undo.
        conn.execute(
            "INSERT INTO fact_observation (fact_id, episode_id, observed_at, kind, method, confidence)
             VALUES (?1, NULL, '2026-09-28', 'asserted', 'probe', 0.5)",
            params![f.other_fact],
        )
        .unwrap();
        crate::episode::undo_last(&conn).unwrap();
        assert_eq!(
            sightings(&conn),
            1,
            "the sighting was dropped on a rowid clash"
        );
        let after: i64 = conn
            .query_row(
                "SELECT observation_count FROM fact WHERE id = ?1",
                params![f.other_fact],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, before);
    }

    /// The snapshot's integer id reaches its corrections, but only those
    /// logged before the snapshot: a later row under a reused id belongs to
    /// whatever episode holds the id now.
    #[test]
    fn a_snapshot_id_does_not_reach_corrections_logged_after_it() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-3");
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        conn.execute(
            "INSERT INTO event_log (ts, kind, ref, payload) VALUES ('2999-01-01 00:00:00', 'correction_unresolved', NULL, ?1)",
            params![serde_json::json!({"episode_id": f.id, "wrong": "a later episode's"}).to_string()],
        )
        .unwrap();
        redact_source(&conn, "bee.conversation", "b-3", false).unwrap();
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM event_log WHERE payload LIKE '%a later episode%'"
            ),
            1
        );
    }

    /// The deleted episode's rowid taken by a newer episode: undo refuses,
    /// rather than restore its rows onto the one that holds the id now.
    #[test]
    fn undo_refuses_when_the_episode_id_was_taken_and_restores_nothing() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-4");
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        conn.execute(
            "INSERT INTO episode (id, uid, source, source_id, body, occurred_at, content_hash)
             VALUES (?1, 'newcomer', 'note', 'n-1', 'an unrelated note', '2026-09-28', 'h')",
            params![f.id],
        )
        .unwrap();
        let raw_before = count(&conn, "SELECT COUNT(*) FROM episode_raw");
        assert!(crate::episode::undo_last(&conn).is_err());
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM episode_raw"), raw_before);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM episode_tombstone WHERE source_id = 'b-4'"
            ),
            1,
            "the tombstone lift was rolled back"
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM undo_log WHERE action = 'delete'"
            ),
            1
        );
    }

    /// A candidate's freed rowid taken by a stranger: undo must not attach
    /// the restored verdicts to it — a `supported` verdict on a claim
    /// nothing verified widens autonomy.
    #[test]
    fn undo_never_attaches_verdicts_to_a_stranger_candidate() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-9");
        let cid: i64 = conn
            .query_row("SELECT candidate_id FROM agent_verdict LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        conn.execute(
            "INSERT INTO fact_candidate (id, payload, proposed_by) VALUES (?1, '{}', 'stranger')",
            params![cid],
        )
        .unwrap();
        crate::episode::undo_last(&conn).unwrap();
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM agent_verdict WHERE candidate_id = {cid}")
            ),
            0,
            "a restored verdict landed on a stranger candidate"
        );
    }

    /// A founded fact's id taken by another fact since: undo refuses, rather
    /// than restore the episode with that fact silently missing and its
    /// sightings attached to the stranger.
    #[test]
    fn undo_refuses_when_a_founded_facts_id_was_taken() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-10");
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        conn.execute(
            "INSERT INTO fact (id, uid, subject_id, predicate, statement, confidence, extractor)
             VALUES (?1, 'stranger-fact', 'ada', 'related_to', 'a stranger', 0.5, 'test')",
            params![f.own_fact],
        )
        .unwrap();
        let err = crate::episode::undo_last(&conn).unwrap_err().to_string();
        assert!(err.contains("another fact"), "{err}");
        assert!(
            crate::episode::get_episode(&conn, f.id).unwrap().is_none(),
            "half-restored"
        );
    }

    /// An unreadable snapshot is an error, never a purge that reports it taken.
    #[test]
    fn an_unreadable_undo_snapshot_stops_the_redaction() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-5");
        conn.execute(
            "INSERT INTO undo_log (action, ref_uid, snapshot) VALUES ('delete', ?1, '{not json')",
            params![f.uid],
        )
        .unwrap();
        assert!(redact_uid(&conn, &f.uid).is_err());
        assert!(crate::episode::get_episode(&conn, f.id).unwrap().is_some());
    }

    /// Ctrl-Z round-trips what undo cannot rebuild: a candidate's verdicts
    /// (write-once, the precision figure's trials) come back with it, and a
    /// collapse alarm is left alone rather than re-raised as new.
    #[test]
    fn an_undone_delete_keeps_verdicts_and_alarms() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-6");
        let verdicts = count(&conn, "SELECT COUNT(*) FROM agent_verdict");
        let alarms = count(&conn, "SELECT COUNT(*) FROM cooccurrence_alarm");
        assert!(verdicts > 0 && alarms > 0, "the fixture has both to lose");
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM cooccurrence_alarm"),
            alarms
        );
        crate::episode::undo_last(&conn).unwrap();
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM agent_verdict"), verdicts);
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM cooccurrence_alarm"),
            alarms
        );
    }

    /// A co-occurrence belief across `n` episodes, anchored — as
    /// `link_npmi` anchors it — on the newest, which is the one returned.
    fn derived_belief(conn: &Connection, n: usize) -> (String, Vec<i64>) {
        upsert_node(conn, &Node::new("ada", "person", "Ada")).unwrap();
        upsert_node(conn, &Node::new("wren", "person", "Wren")).unwrap();
        let mut eps = Vec::new();
        for i in 0..n {
            let mut e = ep(
                "agent:mecha",
                &format!("sess-{i}"),
                &format!("Ada and Wren, talk {i}"),
            );
            e.occurred_at = format!("2026-08-0{} 12:00:00", i + 1);
            let (id, _) = upsert_episode(conn, &e).unwrap();
            add_mention(conn, id, "ada", "alias", 1.0).unwrap();
            add_mention(conn, id, "wren", "alias", 1.0).unwrap();
            eps.push(id);
        }
        let uid = crate::fact::assert_fact(
            conn,
            "ada",
            "related_to",
            Some("wren"),
            None,
            &format!("Ada and Wren frequently co-occur ({n} shared episodes, NPMI 0.50)"),
            None,
            None,
            0.5,
            "npmi",
        )
        .unwrap();
        crate::fact::attach_derivation(conn, &uid, &eps).unwrap();
        (uid, eps)
    }

    /// Owner's ruling: a derived belief whose newest contributor is redacted
    /// is re-derived from the contributors that remain, never deleted with it.
    #[test]
    fn a_derived_belief_is_rederived_from_what_survives() {
        let conn = open_memory().unwrap();
        let (uid, eps) = derived_belief(&conn, 3);
        let anchored: i64 = conn
            .query_row(
                "SELECT episode_id FROM fact WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(anchored, eps[2], "anchored on the newest");

        let rep = redact_source(&conn, "agent:mecha", "sess-2", false).unwrap();
        assert_eq!((rep.redacted, rep.rederived, rep.facts), (1, 1, 0));
        let (anchor, obs): (i64, i64) = conn
            .query_row(
                "SELECT f.episode_id,
                        (SELECT episode_id FROM fact_observation WHERE fact_id = f.id AND kind = 'asserted')
                 FROM fact f WHERE f.uid = ?1",
                params![uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("the belief survives");
        assert_eq!(anchor, eps[1], "re-anchored on the newest survivor");
        assert_eq!(obs, eps[1], "its founding observation follows the anchor");
        // Two shared episodes is under the floor the linker needs to mint a
        // co-occurrence belief at all: re-derived, it no longer holds, and
        // is closed in valid time — as the nightly decay would close it.
        let closed: Option<String> = conn
            .query_row(
                "SELECT valid_to FROM fact WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            closed.is_some(),
            "a belief under the minting floor stayed open"
        );
        assert_eq!(
            rep.derived_closed, 1,
            "the close is counted apart from the re-anchor"
        );
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM event_log WHERE kind = 'belief_decayed' AND ref = '{uid}'")
            ),
            1,
            "a close the sweep would log must be logged here too"
        );
    }

    /// A belief the nightly decay already closed, anchored on the redacted
    /// episode, keeps its verdict and is only re-anchored — closing it a
    /// second time is refused, and that refusal once rolled the whole
    /// redaction back, leaving the episode un-redactable.
    #[test]
    fn redacting_the_anchor_of_a_closed_belief_succeeds() {
        let conn = open_memory().unwrap();
        let (uid, eps) = derived_belief(&conn, 3);
        crate::fact::close_valid_time(&conn, &uid, None).unwrap();
        let rep = redact_source(&conn, "agent:mecha", "sess-2", false)
            .expect("a closed derived belief sank the redaction");
        assert_eq!((rep.redacted, rep.rederived), (1, 1));
        let anchor: i64 = conn
            .query_row(
                "SELECT episode_id FROM fact WHERE uid = ?1",
                params![uid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(anchor, eps[1]);
    }

    /// The TUI's delete is exactly reversible for a derived belief: taken
    /// with the episode, and back — same anchor, same statement — on undo.
    #[test]
    fn an_undone_delete_restores_a_derived_belief_exactly() {
        let conn = open_memory().unwrap();
        let (uid, eps) = derived_belief(&conn, 3);
        let before: (i64, String) = conn
            .query_row(
                "SELECT episode_id, statement FROM fact WHERE uid = ?1",
                params![uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let ep_uid: String = conn
            .query_row(
                "SELECT uid FROM episode WHERE id = ?1",
                params![eps[2]],
                |r| r.get(0),
            )
            .unwrap();
        assert!(redact_episode_undoable(&conn, &ep_uid).unwrap());
        crate::episode::undo_last(&conn).unwrap();
        let after: (i64, String) = conn
            .query_row(
                "SELECT episode_id, statement FROM fact WHERE uid = ?1",
                params![uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(after, before);
    }

    /// A belief the owner verified stays open under the floor (the hold),
    /// but still states the count it has now.
    #[test]
    fn a_verified_belief_under_the_floor_stays_open_and_restates_its_count() {
        let conn = open_memory().unwrap();
        let (uid, _) = derived_belief(&conn, 3);
        conn.execute(
            "INSERT INTO fact_observation (fact_id, episode_id, observed_at, kind, method, confidence)
             SELECT id, NULL, '2026-09-01', 'verified', 'user', 1.0 FROM fact WHERE uid = ?1",
            params![uid],
        )
        .unwrap();
        let rep = redact_source(&conn, "agent:mecha", "sess-2", false).unwrap();
        assert_eq!((rep.rederived, rep.derived_closed), (1, 0));
        let (statement, closed): (String, Option<String>) = conn
            .query_row(
                "SELECT statement, valid_to FROM fact WHERE uid = ?1",
                params![uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(closed.is_none(), "the owner's hold was overridden");
        assert!(statement.contains("(2 shared episodes"), "{statement}");
    }

    /// Re-derived means the statistic too: a belief that still clears the
    /// floor states the count it has now, not the one the redacted episode
    /// was part of.
    #[test]
    fn a_rederived_belief_states_its_new_count() {
        let conn = open_memory().unwrap();
        let (uid, _) = derived_belief(&conn, 4);
        // A corpus big enough to compute NPMI over once the redacted episode
        // is gone: ten episodes with mentions remain (three shared + seven).
        upsert_node(&conn, &Node::new("filler", "person", "Filler")).unwrap();
        for i in 0..7 {
            let (id, _) = upsert_episode(&conn, &ep("note", &format!("f-{i}"), "filler")).unwrap();
            add_mention(&conn, id, "filler", "alias", 1.0).unwrap();
        }
        let rep = redact_source(&conn, "agent:mecha", "sess-3", false).unwrap();
        assert_eq!(rep.rederived, 1);
        let (statement, closed): (String, Option<String>) = conn
            .query_row(
                "SELECT statement, valid_to FROM fact WHERE uid = ?1",
                params![uid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(statement.contains("(3 shared episodes"), "{statement}");
        assert!(closed.is_none());
    }

    /// A sighting the anchor episode made of its own derived belief goes
    /// with it — never stranded as anonymous support that confidence counts.
    #[test]
    fn a_rederived_belief_keeps_no_sighting_of_the_redacted_anchor() {
        let conn = open_memory().unwrap();
        let (uid, eps) = derived_belief(&conn, 3);
        let fid: i64 = conn
            .query_row("SELECT id FROM fact WHERE uid = ?1", params![uid], |r| {
                r.get(0)
            })
            .unwrap();
        conn.execute(
            "INSERT INTO fact_observation (fact_id, episode_id, observed_at, kind, method, confidence)
             VALUES (?1, ?2, '2026-08-03 12:00:00', 'corroborated', 'llm', 0.6)",
            params![fid, eps[2]],
        )
        .unwrap();
        let rep = redact_source(&conn, "agent:mecha", "sess-2", false).unwrap();
        assert_eq!(rep.rederived, 1);
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM fact_observation WHERE fact_id = {fid} AND episode_id IS NULL")
            ),
            0,
            "a sighting from the redacted episode survived as anonymous support"
        );
    }

    /// An undo that cannot be applied is refused, and `--discard` is the way
    /// past it: the entry goes, the episode stays deleted.
    #[test]
    fn a_refused_undo_can_be_discarded() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-7");
        // A correction carrying the episode's own sentence: the TUI's delete
        // leaves it for the snapshot's purge, so a discard must take it.
        let payload =
            serde_json::json!({"episode_id": f.id, "wrong": "a discarded sentence"}).to_string();
        crate::ledger::log_event(&conn, "correction_unresolved", None, Some(&payload)).unwrap();
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        conn.execute(
            "INSERT INTO episode (id, uid, source, source_id, body, occurred_at, content_hash)
             VALUES (?1, 'newcomer', 'note', 'n-7', 'an unrelated note', '2026-09-28', 'h')",
            params![f.id],
        )
        .unwrap();
        let err = crate::episode::undo_last(&conn).unwrap_err().to_string();
        assert!(err.contains("undo --discard"), "{err}");
        let deletes = count(
            &conn,
            "SELECT COUNT(*) FROM undo_log WHERE action = 'delete'",
        );
        assert!(crate::episode::discard_last_undo(&conn).unwrap().is_some());
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM event_log WHERE payload LIKE '%a discarded sentence%'"
            ),
            0,
            "a discard stranded what only the snapshot could reach"
        );
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM undo_log WHERE action = 'delete'"
            ),
            deletes - 1
        );
    }

    /// A legacy fact with a NULL extractor and a node object is founded, not
    /// derived — SQL's NULL must not make the two predicates disagree and
    /// leave the episode impossible to redact.
    #[test]
    fn a_fact_with_a_null_extractor_does_not_block_redaction() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "bee.conversation", "b-8");
        conn.execute(
            "UPDATE fact SET extractor = NULL, object_id = 'org-w' WHERE id = ?1",
            params![f.own_fact],
        )
        .unwrap();
        let rep = redact_uid(&conn, &f.uid).expect("a NULL extractor sank the redaction");
        assert_eq!(rep.redacted, 1);
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM fact WHERE id = {}", f.own_fact)
            ),
            0
        );
    }

    /// A belief whose only contributor is redacted has nothing left to
    /// derive from, and goes like any fact the episode founded.
    #[test]
    fn a_derived_belief_with_no_survivor_is_deleted() {
        let conn = open_memory().unwrap();
        let (uid, _) = derived_belief(&conn, 1);
        let rep = redact_source(&conn, "agent:mecha", "sess-0", false).unwrap();
        assert_eq!((rep.rederived, rep.facts), (0, 1));
        assert_eq!(
            count(
                &conn,
                &format!("SELECT COUNT(*) FROM fact WHERE uid = '{uid}'")
            ),
            0
        );
    }

    #[test]
    fn redacting_by_source_takes_the_live_episode_and_an_older_deleted_copy() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "agent:mecha", "sess-1");
        assert!(redact_episode_undoable(&conn, &f.uid).unwrap());
        conn.execute("DELETE FROM episode_tombstone", []).unwrap();
        let (again, o) = upsert_episode(
            &conn,
            &ep("agent:mecha", "sess-1", &format!("{NEEDLE} recaptured")),
        )
        .unwrap();
        assert_eq!(o, IngestOutcome::Inserted);
        // Re-distilled: the body changes in place, and the earlier body's
        // tokens are left in the FTS index as a delete marker.
        let (_, o) = upsert_episode(&conn, &ep("agent:mecha", "sess-1", "re-distilled")).unwrap();
        assert_eq!(o, IngestOutcome::Updated);
        assert!(
            conn.execute(
                "INSERT INTO episode (uid, source, source_id, body, occurred_at, content_hash)
                 VALUES ('dup', 'agent:mecha', 'sess-1', 'b', '2026-01-01', 'h')",
                [],
            )
            .is_err(),
            "the schema keeps one live episode per (source, source_id)"
        );

        let rep = redact_source(&conn, "agent:mecha", "sess-1", false).unwrap();
        assert_eq!(rep.redacted, 1);
        assert_eq!(
            rep.undo_snapshots, 2,
            "the TUI delete's snapshot and the edit's"
        );
        let again_uid = rep.uids[0].clone();
        assert_eq!(
            traces(&conn, &[NEEDLE, &f.uid, &again_uid], again),
            Vec::<String>::new()
        );
    }

    /// A corroboration from a counted source moved the fact's counter;
    /// redacting it moves it back and re-derives the confidence, and the
    /// TUI's undo restores both.
    #[test]
    fn a_sighting_leaves_the_belief_and_takes_its_contribution() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("ada", "person", "Ada")).unwrap();
        upsert_node(&conn, &Node::new("org-w", "org", "Westfield")).unwrap();
        let (o, _) = upsert_episode(&conn, &ep("note", "o", "Ada works at Westfield")).unwrap();
        let fuid = assert_fact(
            &conn,
            "ada",
            "works_at",
            Some("org-w"),
            None,
            "Ada works at Westfield",
            Some(o),
            None,
            0.7,
            "test",
        )
        .unwrap();
        let before: (i64, f64) = conn
            .query_row(
                "SELECT observation_count, confidence FROM fact WHERE uid = ?1",
                params![fuid],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        let (b, _) = upsert_episode(
            &conn,
            &ep("bee.conversation", "b", "Ada at Westfield again"),
        )
        .unwrap();
        assert_fact(
            &conn,
            "ada",
            "works_at",
            Some("org-w"),
            None,
            "Ada works at Westfield",
            Some(b),
            None,
            0.7,
            "test",
        )
        .unwrap();
        let state = |c: &Connection| -> (i64, f64, i64) {
            c.query_row(
                "SELECT f.observation_count, f.confidence,
                        (SELECT COUNT(*) FROM fact_observation o WHERE o.fact_id = f.id)
                 FROM fact f WHERE uid = ?1",
                params![fuid],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap()
        };
        let corroborated = state(&conn);
        assert_eq!(corroborated.0, 2);
        assert!(corroborated.1 > before.1);

        let b_uid = uid_of(&conn, b);
        assert!(redact_episode_undoable(&conn, &b_uid).unwrap());
        let redacted = state(&conn);
        assert_eq!((redacted.0, redacted.2), (1, 1));
        assert!(
            (redacted.1 - before.1).abs() < 1e-9,
            "confidence back to one sighting"
        );

        undo_last(&conn).unwrap().expect("undo entry");
        let restored = state(&conn);
        assert_eq!((restored.0, restored.2), (2, 2));
        assert!((restored.1 - corroborated.1).abs() < 1e-9);

        // An agent's sighting never moved the counter, so taking it back
        // does not either.
        let (a, _) = upsert_episode(&conn, &ep("agent:mecha", "s", "Ada, Westfield")).unwrap();
        assert_fact(
            &conn,
            "ada",
            "works_at",
            Some("org-w"),
            None,
            "Ada works at Westfield",
            Some(a),
            None,
            0.7,
            "test",
        )
        .unwrap();
        assert_eq!(state(&conn).0, 2);
        redact_source(&conn, "agent:mecha", "s", false).unwrap();
        assert_eq!(state(&conn).0, 2);
    }

    #[test]
    fn a_node_left_with_nothing_is_reported_not_deleted() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("wren", "person", "Wren")).unwrap();
        let (e, _) = upsert_episode(&conn, &ep("agent:mecha", "s", "Wren said hi")).unwrap();
        add_mention(&conn, e, "wren", "alias", 1.0).unwrap();
        let rep = redact_source(&conn, "agent:mecha", "s", false).unwrap();
        assert_eq!(rep.orphaned_nodes, vec!["wren".to_string()]);
        assert!(crate::graph::get_node(&conn, "wren").unwrap().is_some());
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM person_interaction"), 0);
    }

    /// A failure part-way must leave the store as it was.
    #[test]
    fn a_redaction_that_fails_changes_nothing() {
        let conn = open_memory().unwrap();
        let f = fixture(&conn, "agent:mecha", "sess-1");
        conn.execute_batch(
            "CREATE TRIGGER boom BEFORE DELETE ON episode BEGIN SELECT RAISE(ABORT, 'boom'); END;",
        )
        .unwrap();
        assert!(redact_source(&conn, "agent:mecha", "sess-1", false).is_err());
        assert!(crate::episode::get_episode(&conn, f.id).unwrap().is_some());
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM episode_tombstone"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM fact_candidate"), 1);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM event_log"), 4);
    }

    fn file_holds(path: &std::path::Path, needle: &str) -> bool {
        std::fs::read(path)
            .map(|b| b.windows(needle.len()).any(|w| w == needle.as_bytes()))
            .unwrap_or(false)
    }

    /// On a plaintext file the text is readable with `strings`, so the
    /// physical claim can be checked directly: after redact + scrub, neither
    /// the database file nor its WAL holds the deleted text.
    #[test]
    fn scrub_leaves_the_text_in_neither_the_file_nor_the_wal() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.db");
        let wal = dir.path().join("graph.db-wal");
        let conn = crate::db::open(&db).unwrap();
        let f = fixture(&conn, "agent:mecha", "sess-1");
        checkpoint(&conn).unwrap();
        assert!(file_holds(&db, NEEDLE), "sanity: the text reached the file");

        assert!(secure_delete_on(&conn).unwrap());
        let rep = redact_source(&conn, "agent:mecha", "sess-1", false).unwrap();
        assert_eq!(rep.uids, vec![f.uid]);
        let s = scrub(&conn).unwrap();
        assert!(s.vacuumed);
        assert_eq!(s.wal_busy, 0);
        assert!(
            !file_holds(&db, NEEDLE),
            "database file still holds the text"
        );
        assert!(!file_holds(&wal, NEEDLE), "WAL still holds the text");
    }

    /// Under SQLCipher the file is ciphertext either way, so what needs
    /// showing is that the pragmas and VACUUM work there at all, and the
    /// store reopens under its key afterwards.
    #[test]
    fn secure_delete_and_scrub_work_under_sqlcipher() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("graph.db");
        std::fs::write(crate::db::keyfile_path(&db), "ab".repeat(32)).unwrap();
        {
            let conn = crate::db::open(&db).unwrap();
            let cipher: String = conn
                .query_row("PRAGMA cipher_version", [], |r| r.get(0))
                .unwrap();
            assert!(!cipher.is_empty(), "this build is SQLCipher");
            fixture(&conn, "agent:mecha", "sess-1");
            assert!(secure_delete_on(&conn).unwrap());
            assert_eq!(
                redact_source(&conn, "agent:mecha", "sess-1", false)
                    .unwrap()
                    .redacted,
                1
            );
            let s = scrub(&conn).unwrap();
            assert_eq!(s.wal_busy, 0);
        }
        assert!(!file_holds(&db, NEEDLE));
        let raw = Connection::open(&db).unwrap();
        assert!(
            raw.query_row("SELECT COUNT(*) FROM sqlite_master", [], |r| r
                .get::<_, i64>(0))
                .is_err(),
            "still encrypted after VACUUM"
        );
        let conn = crate::db::open(&db).unwrap();
        assert_eq!(
            count(&conn, "SELECT COUNT(*) FROM episode"),
            1,
            "the other episode"
        );
    }
}
