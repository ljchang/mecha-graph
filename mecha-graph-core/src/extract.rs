//! Tier 7 — LLM relation + commitment extraction (§6, §7). Runs LAST: it sees
//! only what tiers 1–6 left ambiguous, uses the closed predicate vocabulary,
//! and writes ONLY to `fact_candidate` — extraction proposes, promotion
//! disposes. Guardrails (§6):
//! - speaker direction is explicit ("I'll send you" vs "can you send me"
//!   inverts waiting_on, and inverting is worse than not extracting)
//! - commitments need a concrete object and a time reference
//! - dates are resolved at extraction time against episode.occurred_at
//!   ("by Friday" is unresolvable later once the anchor is gone)

use crate::error::{Error, Result};
use crate::fact::{self, ProposedFact};
use crate::graph;
use crate::llm::ChatClient;
use rusqlite::{params, Connection};
use serde::Deserialize;

// v2 (2026-08-14): owner name from the graph instead of hardcoded; durable
// vs moment typing discipline; known-entity hints; closing imperative at the
// end of the user turn. Bumping this re-queues every episode for gradual
// re-extraction, newest first — the dedup and previously-rejected guards are
// what make that safe.
pub const PROMPT_VERSION: i64 = 2;

/// Ceiling on staged facts per episode, enforced in the grammar (`maxItems`)
/// and again on the parsed list. The prompt has always said "few good facts
/// beat many weak ones", but prose is a suggestion to a small model: with no
/// cap, generation ran 15–25× the owner's review throughput and the queue
/// reached 9,395 pending — 4× every verdict ever given. When the model
/// over-produces anyway, the highest-confidence facts survive the cut.
pub const MAX_FACTS_PER_EPISODE: usize = 8;

/// Same ceiling for commitments — each accepted one materializes a task the
/// owner sees daily, so over-extraction is costlier here, not cheaper.
pub const MAX_COMMITMENTS_PER_EPISODE: usize = 4;

/// The entity types the prompt admits. Duplicated into the schema below so
/// the sampler enforces what the prose asks for — keep the two in step.
const ENTITY_TYPES: [&str; 11] = [
    "person", "place", "org", "project", "goal", "area", "task", "event", "topic", "artifact",
    "document",
];

/// The output shape, as a grammar rather than as a request.
///
/// `predicate` is the point: the closed vocabulary lives in the `predicate`
/// table, the prompt has always said "MUST be one of", and nothing downstream
/// checked — `propose_fact` stages whatever string arrives, so an out-of-vocab
/// predicate became a candidate no consumer could interpret. As an enum in the
/// schema it is unrepresentable instead.
///
/// Optional fields are spelled `["string", "null"]` and listed in `required`
/// rather than omitted: a nullable-but-present field compiles to a flat
/// grammar, where optional properties compile to a combinatorial one. serde
/// reads `null` into `Option::None` either way.
fn extraction_schema(predicates: &[String]) -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["entities", "facts", "commitments"],
        "properties": {
            "entities": { "type": "array", "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["name", "type", "identifier"],
                "properties": {
                    "name": { "type": "string" },
                    "type": { "type": "string", "enum": ENTITY_TYPES },
                    "identifier": { "type": ["string", "null"] }
                }
            }},
            "facts": { "type": "array", "maxItems": MAX_FACTS_PER_EPISODE, "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["subject", "predicate", "object", "object_value",
                             "statement", "confidence"],
                "properties": {
                    "subject": { "type": "string" },
                    "predicate": { "type": "string", "enum": predicates },
                    "object": { "type": ["string", "null"] },
                    "object_value": { "type": ["string", "null"] },
                    "statement": { "type": "string" },
                    "confidence": { "type": ["number", "null"] }
                }
            }},
            "commitments": { "type": "array", "maxItems": MAX_COMMITMENTS_PER_EPISODE, "items": {
                "type": "object",
                "additionalProperties": false,
                "required": ["who", "what", "when", "direction", "confidence"],
                "properties": {
                    "who": { "type": "string" },
                    "what": { "type": "string" },
                    "when": { "type": ["string", "null"] },
                    // Two values only. A model unsure of direction still has
                    // the out the prompt asks for — emit no commitment at all
                    // — because the *array* may be empty. Inverting is worse
                    // than not extracting (§6), and that stays true here.
                    "direction": { "type": "string",
                                   "enum": ["owed_by_me", "owed_to_me"] },
                    "confidence": { "type": ["number", "null"] }
                }
            }}
        }
    })
}

#[derive(Debug, Deserialize)]
struct Extraction {
    #[serde(default)]
    entities: Vec<ExtractedEntity>,
    #[serde(default)]
    facts: Vec<ExtractedFact>,
    #[serde(default)]
    commitments: Vec<ExtractedCommitment>,
}

#[derive(Debug, Deserialize)]
struct ExtractedEntity {
    name: String,
    #[serde(rename = "type")]
    entity_type: String,
    #[serde(default)]
    identifier: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ExtractedFact {
    subject: String,
    predicate: String,
    #[serde(default)]
    object: Option<String>,
    #[serde(default)]
    object_value: Option<String>,
    statement: String,
    #[serde(default)]
    confidence: Option<f64>,
}

#[derive(Debug, Deserialize)]
struct ExtractedCommitment {
    who: String,
    what: String,
    #[serde(default)]
    when: Option<String>,
    direction: String, // owed_by_me | owed_to_me
    #[serde(default)]
    confidence: Option<f64>,
}

/// The closed vocabulary, read once per run. Feeds both the prose and the
/// grammar, so the two cannot drift apart.
fn predicates(conn: &Connection) -> Result<Vec<String>> {
    let mut stmt = conn.prepare("SELECT name FROM predicate ORDER BY name")?;
    let names: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(names)
}

/// The vocabulary this run may extract: the closed set minus what the
/// ladder has gated for the `llm` proposer (review-on-use §3 — a class
/// below the precision floor stops being extracted AT ALL; the predicate
/// leaves the grammar, so the waste never happens). Returns the gate
/// report beside the survivors because a guard that acts silently is the
/// failure mode this repo keeps finding.
fn extraction_predicates(conn: &Connection) -> Result<(Vec<String>, Vec<String>)> {
    let mut names = predicates(conn)?;
    let gated: Vec<crate::ladder::GatedClass> = crate::ladder::gated_classes(conn, None)?
        .into_iter()
        .filter(|g| g.proposer == "llm")
        .collect();
    let mut report = Vec::new();
    for g in &gated {
        if let Some(i) = names.iter().position(|n| n == &g.predicate) {
            names.remove(i);
            report.push(format!("{} ({})", g.predicate, g.why));
        }
    }
    Ok((names, report))
}

fn system_prompt(conn: &Connection, predicates: &[String]) -> Result<String> {
    let predicates = predicates.to_vec();
    // The owner's name comes from the graph, never from this source file: a
    // hardcoded name is wrong for every other deployment and personal data
    // in a repo headed for the public.
    let narrator = match crate::graph::owner_node(conn)? {
        Some(n) => format!(
            "The narrator/\"you\" is {name} (the graph's owner) — when they are a \
             fact's subject, name them \"{name}\" exactly, never a pronoun.",
            name = n.name
        ),
        None => "The narrator/\"you\" is the user.".to_string(),
    };

    Ok(format!(
        r#"You extract structured knowledge from personal episode summaries (conversations, meetings, notes).

Return STRICT JSON: {{"entities": [...], "facts": [...], "commitments": [...]}}.

entities: [{{"name": str, "type": one of person|place|org|project|goal|area|task|event|topic|artifact|document, "identifier": email-or-null}}]
  Only entities you are CONFIDENT about. Skip generic references ("the child", "a colleague", "the team"). If unsure of the type, omit the entity entirely.

facts: [{{"subject": str, "predicate": str, "object": str-or-null, "object_value": str-or-null, "statement": one natural-language sentence, "confidence": 0..1}}]
  predicate MUST be one of: {preds}
  subject: a named person or concrete thing — never a pronoun, never "the team".
  A fact is DURABLE: still true next month, not just in this moment. A sentence
  anchored to one moment ("was doing X", "that day", "this morning") belongs to
  the episode record, not to a fact — skip it. Property predicates (has_role,
  is, has) name lasting properties only; one-time events take an event
  predicate (attended, presented, demonstrated) or are skipped.
  Only facts worth remembering (roles, relationships, preferences, decisions) — not play-by-play. Few good facts beat many weak ones. Return an empty list if nothing qualifies.

commitments: [{{"who": str, "what": str, "when": "YYYY-MM-DD"-or-null, "direction": "owed_by_me"|"owed_to_me", "confidence": 0..1}}]
  RULES:
  - "I'll send you X" (speaker=user) => owed_by_me. "Can you send me X" / "she'll send" => owed_to_me. Getting direction wrong is worse than not extracting — if unsure, SKIP.
  - Require a concrete deliverable AND a time reference. "We should grab lunch sometime" is NOT a commitment.
  - Resolve relative dates ("by Friday") against the episode date given in the input.

{narrator} Be conservative: precision beats recall."#,
        preds = predicates.join(", ")
    ))
}

/// The closing imperative for the user turn. With a long transcript a local
/// model keeps the instruction it read most recently, so the binding
/// output-shape command goes at the END of the input, not (only) in the
/// system prompt — the harness lesson that cost the most reruns.
const CLOSING_IMPERATIVE: &str = "\
Now return STRICT JSON exactly as specified: {\"entities\": [...], \
\"facts\": [...], \"commitments\": [...]}. Predicates from the allowed \
list only. Durable facts only — skip anything anchored to a single moment.";

#[derive(Debug, Default, serde::Serialize)]
pub struct ExtractReport {
    pub episodes: usize,
    pub mentions: usize,
    pub fact_candidates: usize,
    pub commitment_candidates: usize,
    pub errors: usize,
    /// Predicates this run refused to extract, with why — the ladder's
    /// generation gate (review-on-use §3). Reported, never silent.
    pub gated: Vec<String>,
}

/// The episodes the next extraction pass would take, newest first.
///
/// Split out of `extract_pending` so the eligibility rules are testable
/// without a model. Two clauses beyond the prompt-version bookkeeping:
///
/// - `occurred_at <= now`: a future calendar invite is not evidence of
///   anything yet — `rollup.rs` has excluded future episodes since day one
///   for the same reason, but extraction didn't, and `ORDER BY occurred_at
///   DESC` put *unheld meetings first in line* for the GPU every night.
///   Known caveat, shared with rollup's identical comparison: ics.rs keeps
///   TZID-zoned calendar times as naive local strings while
///   `datetime('now')` is UTC, so a calendar event crosses this boundary
///   hours early or late by the zone offset. The real fix is normalizing
///   to UTC at ingest; until then calendar is excluded from extraction by
///   default anyway, and an off-by-hours boundary on an *excluded* source
///   costs nothing.
/// - `exclude_sources`: some sources cannot contain a durable fact the
///   deterministic tiers didn't already take. A calendar body is a title
///   plus an attendee list `ics.rs` has already turned into facts; 65% of
///   the corpus is calendar events, and LLM-extracting them re-derives
///   tier-1 output as prose and queues it for human review.
fn pending_episodes(
    conn: &Connection,
    limit: usize,
    sources: Option<&[&str]>,
    exclude_sources: Option<&[&str]>,
) -> Result<Vec<(i64, String, String, String)>> {
    let quote_list = |s: &[&str]| {
        s.iter()
            .map(|x| format!("'{}'", x.replace('\'', "''")))
            .collect::<Vec<_>>()
            .join(",")
    };
    let source_clause = match sources {
        Some(s) if !s.is_empty() => format!("AND e.source IN ({})", quote_list(s)),
        _ => String::new(),
    };
    let exclude_clause = match exclude_sources {
        Some(s) if !s.is_empty() => format!("AND e.source NOT IN ({})", quote_list(s)),
        _ => String::new(),
    };
    let sql = format!(
        "SELECT e.id, e.uid, e.body, e.occurred_at FROM episode e
         WHERE e.id NOT IN (SELECT episode_id FROM extract_state WHERE prompt_version >= ?1)
           AND e.occurred_at <= datetime('now')
           {source_clause}
           {exclude_clause}
         ORDER BY e.occurred_at DESC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![PROMPT_VERSION, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// The commitment deliverables that block a re-file, normalized the same
/// way precheck normalizes statements — ONE identity function, because the
/// two tiers previously disagreed (`lower(trim())` here vs `normalize()`
/// there): "Send the pilot data." with a trailing period was a duplicate
/// to precheck but not to extract, so extract re-filed what precheck then
/// machine-rejected, every night.
///
/// A machine (`precheck:%`) reject does NOT block: precheck stale-rejects
/// a commitment from an old episode minutes after extraction, and if that
/// row then blocked forever, one machine decision would silently retire a
/// recurring obligation for life with no human verdict anywhere in the
/// chain. Pending, accepted, and human-rejected rows block; the machine's
/// own rejects are its business to re-make.
///
/// Built once per run (the per-commitment form was an unindexed full-table
/// scan inside the per-episode loop), and updated as the run stages.
pub(crate) fn commitment_block_set(conn: &Connection) -> Result<std::collections::HashSet<String>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(json_extract(payload, '$.what'), '') FROM fact_candidate
         WHERE json_extract(payload, '$.kind') = 'commitment'
           AND (status != 'rejected' OR COALESCE(reject_reason, '') NOT LIKE 'precheck:%')",
    )?;
    let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
    Ok(rows
        .filter_map(|w| w.ok())
        .map(|w| crate::precheck::normalize(&w))
        .filter(|w| !w.is_empty())
        .collect())
}

/// Extract over episodes not yet processed at the current prompt version.
/// `sources`: restrict to these episode sources (None = all).
/// `exclude_sources`: skip these sources (None = none) — see
/// [`pending_episodes`] for why calendar is the intended tenant.
pub fn extract_pending(
    conn: &Connection,
    chat: &ChatClient,
    limit: usize,
    sources: Option<&[&str]>,
    exclude_sources: Option<&[&str]>,
) -> Result<ExtractReport> {
    let rows = pending_episodes(conn, limit, sources, exclude_sources)?;
    let mut committed = commitment_block_set(conn)?;

    let (vocab, gated) = extraction_predicates(conn)?;
    let system = system_prompt(conn, &vocab)?;
    let schema = extraction_schema(&vocab);
    let mut report = ExtractReport {
        gated,
        ..Default::default()
    };

    for (episode_id, _uid, body, occurred_at) in rows {
        extract_episode(
            conn,
            chat,
            &system,
            &schema,
            episode_id,
            &body,
            &occurred_at,
            &mut committed,
            &mut report,
        )?;
    }

    Ok(report)
}

/// Re-extract ONE episode regardless of prompt-version state — the targeted
/// re-run for a fixed prompt, a corrected episode, or an evidence-only gap
/// probing surfaced. Safe to repeat: the precheck dedup tiers absorb
/// candidates duplicating live facts or the queue, and the
/// previously-rejected guard stops a re-extraction from resurrecting a
/// claim the owner already said no to.
pub fn reextract_episode(
    conn: &Connection,
    chat: &ChatClient,
    episode: &str,
) -> Result<ExtractReport> {
    use rusqlite::OptionalExtension;
    let row: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, body, occurred_at FROM episode WHERE uid = ?1 OR CAST(id AS TEXT) = ?1",
            params![episode],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((episode_id, body, occurred_at)) = row else {
        return Err(Error::Other(format!("no episode matching '{episode}'")));
    };
    conn.execute(
        "DELETE FROM extract_state WHERE episode_id = ?1",
        params![episode_id],
    )?;
    let (vocab, gated) = extraction_predicates(conn)?;
    let system = system_prompt(conn, &vocab)?;
    let schema = extraction_schema(&vocab);
    let mut report = ExtractReport {
        gated,
        ..Default::default()
    };
    let mut committed = commitment_block_set(conn)?;
    extract_episode(
        conn,
        chat,
        &system,
        &schema,
        episode_id,
        &body,
        &occurred_at,
        &mut committed,
        &mut report,
    )?;
    Ok(report)
}

#[allow(clippy::too_many_arguments)]
fn extract_episode(
    conn: &Connection,
    chat: &ChatClient,
    system: &str,
    schema: &serde_json::Value,
    episode_id: i64,
    body: &str,
    occurred_at: &str,
    committed: &mut std::collections::HashSet<String>,
    report: &mut ExtractReport,
) -> Result<()> {
    {
        report.episodes += 1;
        let body_trunc: String = body.chars().take(6000).collect();
        // Entities the deterministic alias scan already linked: anchoring
        // the model to canonical names is what keeps subjects resolvable —
        // the queue's unresolvable-subject majority came from spellings the
        // graph didn't know.
        let mut stmt = conn.prepare_cached(
            "SELECT DISTINCT n.name FROM mention m JOIN nodes n ON n.id = m.node_id
             WHERE m.episode_id = ?1 ORDER BY n.name LIMIT 12",
        )?;
        let known: Vec<String> = stmt
            .query_map(params![episode_id], |r| r.get(0))?
            .collect::<std::result::Result<_, _>>()?;
        let hints = if known.is_empty() {
            String::new()
        } else {
            format!(
                "Known entities in this episode (use these exact names): {}\n",
                known.join(", ")
            )
        };
        let user =
            format!("Episode date: {occurred_at}\n{hints}\n{body_trunc}\n\n{CLOSING_IMPERATIVE}");

        let parsed: Extraction = match chat
            .complete_schema(system, &user, "extraction", schema.clone())
            .and_then(|v| serde_json::from_value(v).map_err(|e| Error::Parse(e.to_string())))
        {
            Ok(p) => p,
            Err(e) => {
                report.errors += 1;
                eprintln!("extract: episode {episode_id}: {e}");
                // Mark attempted so one poison episode doesn't wedge the batch
                // forever; bump PROMPT_VERSION to force retries.
                conn.execute(
                    "INSERT OR REPLACE INTO extract_state (episode_id, model, prompt_version, candidates_created)
                     VALUES (?1, ?2, ?3, 0)",
                    params![episode_id, chat.model, PROMPT_VERSION],
                )?;
                return Ok(());
            }
        };

        let mut created = 0i64;

        // Entities: mention when they resolve to an existing node; create only
        // when a deterministic identifier (email) is present. LLMs must not
        // invent nodes (§4.2).
        for ent in &parsed.entities {
            let resolved = graph::resolve_entity(conn, &ent.name)?;
            match resolved {
                Some(node) => {
                    crate::episode::add_mention(conn, episode_id, &node.id, "llm", 0.7)?;
                    report.mentions += 1;
                }
                None => {
                    if ent.entity_type == "person" {
                        if let Some(email) = ent.identifier.as_deref().filter(|i| i.contains('@')) {
                            let node =
                                graph::get_or_create_person(conn, Some(email), &ent.name, "llm")?;
                            crate::episode::add_mention(conn, episode_id, &node.id, "llm", 0.9)?;
                            report.mentions += 1;
                        }
                    }
                }
            }
        }

        // Facts → staged candidates (§4.3: the sole non-deterministic write path).
        // The grammar caps the list, but a schema is a request to a sampler,
        // not a proof about it — enforce the cap on what actually parsed,
        // keeping the highest-confidence facts when the model overran.
        let mut facts: Vec<&ExtractedFact> = parsed.facts.iter().collect();
        if facts.len() > MAX_FACTS_PER_EPISODE {
            facts.sort_by(|a, b| {
                b.confidence
                    .unwrap_or(0.5)
                    .partial_cmp(&a.confidence.unwrap_or(0.5))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            facts.truncate(MAX_FACTS_PER_EPISODE);
        }
        for f in facts {
            if f.subject.trim().is_empty() || f.statement.trim().is_empty() {
                continue;
            }
            let proposed = ProposedFact {
                subject: f.subject.clone(),
                predicate: f.predicate.clone(),
                object: f.object.clone(),
                object_value: f.object_value.clone(),
                statement: f.statement.clone(),
                valid_from: Some(occurred_at.to_string()),
                confidence: f.confidence,
                tags: None,
                ..Default::default()
            };
            fact::propose_fact(conn, &proposed, "llm", Some(episode_id))?;
            report.fact_candidates += 1;
            created += 1;
        }

        // Commitments → staged with kind marker; acceptance materializes a
        // Task. Validity is checked BEFORE the cap and the cap is
        // confidence-ranked, mirroring the facts path above — a `take(N)`
        // over the raw list let N junk entries consume the cap and drop a
        // real commitment listed fifth.
        let mut commitments: Vec<&ExtractedCommitment> = parsed
            .commitments
            .iter()
            .filter(|c| {
                !c.what.trim().is_empty()
                    // unknown direction: skip, don't guess (§6)
                    && matches!(c.direction.as_str(), "owed_by_me" | "owed_to_me")
            })
            .collect();
        if commitments.len() > MAX_COMMITMENTS_PER_EPISODE {
            commitments.sort_by(|a, b| {
                b.confidence
                    .unwrap_or(0.5)
                    .partial_cmp(&a.confidence.unwrap_or(0.5))
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            commitments.truncate(MAX_COMMITMENTS_PER_EPISODE);
        }
        for c in commitments {
            // Commitments used to skip every dedup tier (precheck `continue`s
            // past them), so a PROMPT_VERSION bump re-proposed every old
            // commitment and the owner re-judged each one. Any prior
            // candidate with the same deliverable — pending, accepted, or
            // HUMAN-rejected — blocks a re-file: asked and answered. See
            // `commitment_block_set` for why machine rejects don't block.
            let what_norm = crate::precheck::normalize(&c.what);
            if !what_norm.is_empty() && !committed.insert(what_norm) {
                continue;
            }
            let payload = serde_json::json!({
                "kind": "commitment",
                "who": c.who,
                "what": c.what,
                "when": c.when,
                "direction": c.direction,
                "confidence": c.confidence.unwrap_or(0.6),
            });
            conn.execute(
                "INSERT INTO fact_candidate (payload, proposed_by, episode_id, confidence)
                 VALUES (?1, 'llm:commitment', ?2, ?3)",
                params![payload.to_string(), episode_id, c.confidence.unwrap_or(0.6)],
            )?;
            report.commitment_candidates += 1;
            created += 1;
        }

        conn.execute(
            "INSERT OR REPLACE INTO extract_state (episode_id, model, prompt_version, candidates_created)
             VALUES (?1, ?2, ?3, ?4)",
            params![episode_id, chat.model, PROMPT_VERSION, created],
        )?;
    }
    Ok(())
}

/// Accept a commitment candidate: materialize Task node + task_detail +
/// waiting_on/originated_in facts (§6's payoff graph shape).
pub fn accept_commitment(conn: &Connection, candidate_id: i64) -> Result<String> {
    use rusqlite::OptionalExtension;
    let row: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT payload, episode_id FROM fact_candidate
             WHERE id = ?1 AND status = 'proposed'",
            params![candidate_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    let Some((payload, episode_id)) = row else {
        return Err(Error::Other(format!("no pending candidate {candidate_id}")));
    };
    let p: serde_json::Value = serde_json::from_str(&payload)?;
    if p.get("kind").and_then(|k| k.as_str()) != Some("commitment") {
        return Err(Error::Other(
            "not a commitment candidate — use pkg accept".into(),
        ));
    }

    let what = p["what"].as_str().unwrap_or("(unnamed)");
    let who = p["who"].as_str().unwrap_or("");
    let direction = p["direction"].as_str().unwrap_or("owed_by_me");
    let owed_to_me = direction == "owed_to_me";

    // **A model's `when` is text until it parses.** This went in raw, and it
    // reaches three date columns — `task_detail.due_at` below, and the
    // `valid_from` of both facts asserted further down. A model that answered
    // the literal string "null" put that in all three: it sorts as a date,
    // never comes out overdue, and silently joins the wrong side of every
    // bi-temporal `--as-of` query. One row on this graph, which is exactly
    // how long a bug like this stays invisible.
    //
    // Unparseable degrades to None rather than failing the accept: the
    // commitment is real even when its date is noise, and refusing to accept
    // it would leave a genuine obligation stuck in the queue over a word.
    // Nothing is lost — the candidate payload keeps the raw `when` verbatim.
    //
    // Caveat worth knowing: `parse_due` resolves 'tomorrow' and '+3d'
    // against *now*, not against the episode the commitment came from, so a
    // relative date accepted late lands late. Still strictly better than
    // storing the word, and it keeps one date parser rather than two.
    // Reported, not merely swallowed. `.ok().flatten()` alone made "the model
    // gave no date" and "the model gave something unreadable" identical to
    // every caller, and the accept returns only a task id, so the drop was
    // invisible at the surface as well as in the row.
    let when_raw = p["when"].as_str().map(str::trim).filter(|s| !s.is_empty());
    let when_owned = match when_raw {
        Some(raw) => match crate::gtd::parse_due(raw) {
            Ok(parsed) => parsed,
            Err(e) => {
                eprintln!(
                    "accept_commitment: candidate {candidate_id}: unreadable `when` \
                     {raw:?} dropped ({e}); the task is created without a due date \
                     and the candidate payload keeps the original"
                );
                None
            }
        },
        None => None,
    };
    let when = when_owned.as_deref();

    let task_id = format!("task-{}", uuid_suffix());
    let mut task = graph::Node::new(&task_id, "task", what);
    task.source = "llm:commitment".into();
    graph::upsert_node(conn, &task)?;
    conn.execute(
        "INSERT INTO task_detail (node_id, status, task_type, due_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![
            task_id,
            if owed_to_me { "waiting" } else { "next" },
            if owed_to_me { "waiting" } else { "action" },
            when
        ],
    )?;

    // waiting_on → Person is what makes this a graph, not a list (§4.4).
    if owed_to_me && !who.is_empty() && who.to_lowercase() != "me" {
        if let Some(person) = graph::resolve_entity(conn, who)? {
            fact::assert_fact(
                conn,
                &task_id,
                "waiting_on",
                Some(&person.id),
                None,
                &format!("\"{what}\" is waiting on {}", person.name),
                episode_id,
                when,
                0.8,
                "llm:commitment",
            )?;
        }
    }
    if let Some(ep_id) = episode_id {
        let ep_uid: String = conn.query_row(
            "SELECT uid FROM episode WHERE id = ?1",
            params![ep_id],
            |r| r.get(0),
        )?;
        fact::assert_fact(
            conn,
            &task_id,
            "originated_in",
            None,
            Some(&ep_uid),
            &format!("Task \"{what}\" originated in episode {ep_uid}"),
            Some(ep_id),
            when,
            0.9,
            "llm:commitment",
        )?;
    }

    conn.execute(
        "UPDATE fact_candidate SET status = 'accepted', reviewed_at = datetime('now'),
                reviewed_by = 'user'
         WHERE id = ?1",
        params![candidate_id],
    )?;
    Ok(task_id)
}

fn uuid_suffix() -> String {
    uuid::Uuid::new_v4().to_string()[..8].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{get_or_create_person, upsert_node, Node};

    #[test]
    fn the_prompt_names_the_owner_from_the_graph_not_the_source() {
        let conn = open_memory().unwrap();
        // No owner set: neutral narrator, and no personal name baked in.
        let vocab = predicates(&conn).unwrap();
        let p = system_prompt(&conn, &vocab).unwrap();
        assert!(p.contains("the user"));
        assert!(
            !p.contains("Ada"),
            "a name in the prompt must come from the graph"
        );

        upsert_node(&conn, &Node::new("person-o", "person", "Ada Lovelace")).unwrap();
        crate::graph::set_owner(&conn, "person-o").unwrap();
        let p = system_prompt(&conn, &vocab).unwrap();
        assert!(p.contains("Ada Lovelace"));
        // Durable-vs-moment typing discipline rides in the same prompt.
        assert!(p.contains("DURABLE"));
    }

    /// A gated class's predicate leaves both the vocabulary and the run's
    /// report says so — the gate is structural (out of the grammar), not
    /// advisory prose.
    #[test]
    fn a_gated_predicate_leaves_the_extraction_vocabulary() {
        let conn = open_memory().unwrap();
        for _ in 0..24 {
            conn.execute(
                "INSERT INTO fact_candidate (payload, proposed_by, status, reviewed_by, reviewed_at)
                 VALUES (json_object('predicate','has_role','subject','x','statement','s'),
                         'llm', 'rejected', 'user', datetime('now'))",
                [],
            )
            .unwrap();
        }
        let (vocab, gated) = extraction_predicates(&conn).unwrap();
        assert!(!vocab.iter().any(|p| p == "has_role"));
        assert_eq!(gated.len(), 1);
        assert!(gated[0].starts_with("has_role ("));
        // The rest of the vocabulary is untouched.
        assert!(vocab.iter().any(|p| p == "works_at"));
    }

    #[test]
    fn test_accept_commitment_materializes_task() {
        let conn = open_memory().unwrap();
        get_or_create_person(&conn, Some("nadia@example.edu"), "Nadia", "t").unwrap();
        let (ep_id, _) = crate::episode::upsert_episode(
            &conn,
            &crate::episode::Episode {
                id: 0,
                uid: String::new(),
                source: "bee.conversation".into(),
                source_id: "c1".into(),
                source_ref: None,
                body: "Nadia said she'll send the pilot data by Friday".into(),
                occurred_at: "2026-08-01 10:00:00".into(),
                occurred_end: None,
                ingested_at: String::new(),
                lat: None,
                lon: None,
                location: None,
                sensitivity: "private".into(),
                scope_id: None,
                meta: None,
                raw: None,
            },
        )
        .unwrap();

        conn.execute(
            "INSERT INTO fact_candidate (payload, proposed_by, episode_id, confidence)
             VALUES (?1, 'llm:commitment', ?2, 0.8)",
            params![
                serde_json::json!({
                    "kind": "commitment", "who": "Nadia", "what": "send pilot data",
                    "when": "2026-08-07", "direction": "owed_to_me", "confidence": 0.8
                })
                .to_string(),
                ep_id
            ],
        )
        .unwrap();

        let task_id = accept_commitment(&conn, 1).unwrap();

        // Task detail: waiting, due Friday.
        let (status, due): (String, Option<String>) = conn
            .query_row(
                "SELECT status, due_at FROM task_detail WHERE node_id = ?1",
                params![task_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "waiting");
        assert_eq!(due.as_deref(), Some("2026-08-07"));

        // waiting_on Nadia + originated_in episode.
        let facts = fact::facts_for_node(&conn, &task_id, 10).unwrap();
        assert!(facts.iter().any(|f| f.predicate == "waiting_on"));
        assert!(facts.iter().any(|f| f.predicate == "originated_in"));
    }

    fn plain_episode(source: &str, sid: &str, at: &str) -> crate::episode::Episode {
        crate::episode::Episode {
            id: 0,
            uid: String::new(),
            source: source.into(),
            source_id: sid.into(),
            source_ref: None,
            body: format!("episode {sid}"),
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
        }
    }

    #[test]
    fn extraction_skips_future_episodes_and_excluded_sources() {
        let conn = open_memory().unwrap();
        crate::episode::upsert_episode(
            &conn,
            &plain_episode("slack.thread", "past", "2026-01-05 10:00:00"),
        )
        .unwrap();
        crate::episode::upsert_episode(
            &conn,
            &plain_episode("calendar.event", "cal", "2026-01-06 10:00:00"),
        )
        .unwrap();
        // A meeting that hasn't happened is not evidence of anything yet —
        // and DESC ordering used to put it FIRST in line.
        crate::episode::upsert_episode(
            &conn,
            &plain_episode("calendar.event", "future", "2999-01-01 10:00:00"),
        )
        .unwrap();

        let all = pending_episodes(&conn, 10, None, None).unwrap();
        assert_eq!(all.len(), 2, "the future episode must not be eligible");
        assert!(all.iter().all(|r| r.3.as_str() < "2999"));

        let filtered = pending_episodes(&conn, 10, None, Some(&["calendar.event"])).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].3, "2026-01-05 10:00:00");
    }

    fn insert_commitment(conn: &Connection, what: &str, status: &str, reason: Option<&str>) {
        conn.execute(
            "INSERT INTO fact_candidate (payload, proposed_by, status, reject_reason, confidence)
             VALUES (?1, 'llm:commitment', ?2, ?3, 0.8)",
            params![
                serde_json::json!({
                    "kind": "commitment", "who": "Nadia", "what": what,
                    "when": null, "direction": "owed_to_me", "confidence": 0.8
                })
                .to_string(),
                status,
                reason
            ],
        )
        .unwrap();
    }

    #[test]
    fn the_commitment_guard_blocks_judged_but_not_machine_rejected() {
        let conn = open_memory().unwrap();
        insert_commitment(&conn, "Send the pilot data.", "rejected", Some("not mine"));
        insert_commitment(
            &conn,
            "Book the scanner",
            "rejected",
            Some("precheck: stale commitment"),
        );
        insert_commitment(&conn, "Draft the memo", "proposed", None);

        let set = commitment_block_set(&conn).unwrap();
        // One normalization with precheck: the trailing period and casing
        // must not make a different identity — the two tiers previously
        // disagreed and extract re-filed what precheck then machine-rejected.
        assert!(
            set.contains(&crate::precheck::normalize("send the pilot data")),
            "a human-rejected deliverable blocks, punctuation-insensitively"
        );
        assert!(set.contains(&crate::precheck::normalize("Draft the memo")));
        // A machine reject must NOT hold a lifetime block: one precheck
        // staleness decision would otherwise silently retire a recurring
        // obligation forever with no human verdict anywhere in the chain.
        assert!(
            !set.contains(&crate::precheck::normalize("Book the scanner")),
            "the machine's own reject is not asked-and-answered"
        );
    }

    #[test]
    fn test_accept_commitment_rejects_plain_facts() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("w", "person", "W")).unwrap();
        let proposed = ProposedFact {
            subject: "W".into(),
            predicate: "works_on".into(),
            object: None,
            object_value: Some("X".into()),
            statement: "W works on X".into(),
            valid_from: None,
            confidence: Some(0.8),
            tags: None,
            ..Default::default()
        };
        let id = fact::propose_fact(&conn, &proposed, "llm", None).unwrap();
        assert!(accept_commitment(&conn, id).is_err());
    }
}
