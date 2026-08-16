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
use rusqlite::{params, Connection};
use serde::Deserialize;

// v2 (2026-08-14): owner name from the graph instead of hardcoded; durable
// vs moment typing discipline; known-entity hints; closing imperative at the
// end of the user turn. Bumping this re-queues every episode for gradual
// re-extraction, newest first — the dedup and previously-rejected guards are
// what make that safe.
pub const PROMPT_VERSION: i64 = 2;

pub struct OllamaChat {
    pub base_url: String,
    pub model: String,
}

impl OllamaChat {
    pub fn new(model: &str) -> Self {
        OllamaChat {
            base_url: std::env::var("PKG_OLLAMA_URL")
                .unwrap_or_else(|_| "http://127.0.0.1:11434".to_string()),
            model: model.to_string(),
        }
    }

    /// One JSON-mode chat completion.
    pub fn complete_json(&self, system: &str, user: &str) -> Result<serde_json::Value> {
        let resp = ureq::post(&format!("{}/api/chat", self.base_url))
            .timeout(std::time::Duration::from_secs(300))
            .send_json(serde_json::json!({
                "model": self.model,
                "messages": [
                    { "role": "system", "content": system },
                    { "role": "user", "content": user }
                ],
                "format": "json",
                "stream": false,
                "options": { "temperature": 0.1 }
            }))
            .map_err(|e| Error::Other(format!("ollama chat failed: {e}")))?;
        let body: serde_json::Value = resp
            .into_json()
            .map_err(|e| Error::Other(format!("bad ollama response: {e}")))?;
        let content = body
            .pointer("/message/content")
            .and_then(|c| c.as_str())
            .ok_or_else(|| Error::Other("no content in ollama response".into()))?;
        serde_json::from_str(content)
            .map_err(|e| Error::Parse(format!("model returned invalid JSON: {e}")))
    }
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

fn system_prompt(conn: &Connection) -> Result<String> {
    let mut stmt = conn.prepare("SELECT name FROM predicate ORDER BY name")?;
    let predicates: Vec<String> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
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
}

/// Extract over episodes not yet processed at the current prompt version.
/// `sources`: restrict to these episode sources (None = all).
pub fn extract_pending(
    conn: &Connection,
    chat: &OllamaChat,
    limit: usize,
    sources: Option<&[&str]>,
) -> Result<ExtractReport> {
    let source_clause = match sources {
        Some(s) if !s.is_empty() => format!(
            "AND e.source IN ({})",
            s.iter()
                .map(|x| format!("'{x}'"))
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => String::new(),
    };
    let sql = format!(
        "SELECT e.id, e.uid, e.body, e.occurred_at FROM episode e
         WHERE e.id NOT IN (SELECT episode_id FROM extract_state WHERE prompt_version >= ?1)
           {source_clause}
         ORDER BY e.occurred_at DESC LIMIT ?2"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows: Vec<(i64, String, String, String)> = stmt
        .query_map(params![PROMPT_VERSION, limit as i64], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;

    let system = system_prompt(conn)?;
    let mut report = ExtractReport::default();

    for (episode_id, _uid, body, occurred_at) in rows {
        extract_episode(
            conn,
            chat,
            &system,
            episode_id,
            &body,
            &occurred_at,
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
    chat: &OllamaChat,
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
    let system = system_prompt(conn)?;
    let mut report = ExtractReport::default();
    extract_episode(
        conn,
        chat,
        &system,
        episode_id,
        &body,
        &occurred_at,
        &mut report,
    )?;
    Ok(report)
}

fn extract_episode(
    conn: &Connection,
    chat: &OllamaChat,
    system: &str,
    episode_id: i64,
    body: &str,
    occurred_at: &str,
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
            .complete_json(&system, &user)
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
        for f in &parsed.facts {
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
            };
            fact::propose_fact(conn, &proposed, "llm", Some(episode_id))?;
            report.fact_candidates += 1;
            created += 1;
        }

        // Commitments → staged with kind marker; acceptance materializes a Task.
        for c in &parsed.commitments {
            if c.what.trim().is_empty() {
                continue;
            }
            if !matches!(c.direction.as_str(), "owed_by_me" | "owed_to_me") {
                continue; // unknown direction: skip, don't guess (§6)
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
    let when = p["when"].as_str();
    let direction = p["direction"].as_str().unwrap_or("owed_by_me");
    let owed_to_me = direction == "owed_to_me";

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
        "UPDATE fact_candidate SET status = 'accepted', reviewed_at = datetime('now') WHERE id = ?1",
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
        let p = system_prompt(&conn).unwrap();
        assert!(p.contains("the user"));
        assert!(
            !p.contains("Ada"),
            "a name in the prompt must come from the graph"
        );

        upsert_node(&conn, &Node::new("person-o", "person", "Ada Lovelace")).unwrap();
        crate::graph::set_owner(&conn, "person-o").unwrap();
        let p = system_prompt(&conn).unwrap();
        assert!(p.contains("Ada Lovelace"));
        // Durable-vs-moment typing discipline rides in the same prompt.
        assert!(p.contains("DURABLE"));
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
        };
        let id = fact::propose_fact(&conn, &proposed, "llm", None).unwrap();
        assert!(accept_commitment(&conn, id).is_err());
    }
}
