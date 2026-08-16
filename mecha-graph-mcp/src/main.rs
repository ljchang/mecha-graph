//! pkg-mcp: the portability layer (§9.1). Eleven tools, stdio transport —
//! one server → every harness, zero per-harness code.
//!
//! Tools return a versioned envelope ({v, items[], truncated, budget}), not
//! prose. All agent writes route through fact_candidate with
//! source='agent:<harness>' — agents hallucinate; provenance lets you undo it.

use mecha_graph_core::rusqlite::Connection;
use mecha_graph_core::{context, db, embed, episode, fact, graph, gtd, rollup, router};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2024-11-05";

fn main() {
    let db_path = db::default_db_path();
    let conn = match db::open(&db_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("pkg-mcp: cannot open {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };
    let embedder = embed::OllamaEmbedder::default();

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        if line.trim().is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let id = msg.get("id").cloned();
        let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
        let params = msg.get("params").cloned().unwrap_or(Value::Null);

        // Notifications (no id) need no response.
        let Some(id) = id else { continue };

        let result = match method {
            "initialize" => Ok(json!({
                "protocolVersion": PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "mecha-graph-mcp", "version": env!("CARGO_PKG_VERSION") }
            })),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => handle_tool_call(&conn, &embedder, &params),
            _ => Err(format!("method not found: {method}")),
        };

        let response = match result {
            Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
            Err(e) => json!({
                "jsonrpc": "2.0", "id": id,
                "error": { "code": -32603, "message": e }
            }),
        };
        let mut out = stdout.lock();
        let _ = writeln!(out, "{response}");
        let _ = out.flush();
    }
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "kg_search",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "Search the personal knowledge graph. Routes the query (entity detection → filter → rank) and returns a token-bounded context pack. `#tag` tokens in the query filter to episodes the user hand-tagged with ALL of those tags (e.g. '#recommendation restaurants'); a tag-only query lists that tag's episodes newest-first. If `ambiguous` is non-empty, ask the user which entity they meant instead of guessing. If `flags` is non-empty (contradiction | denial | staleness, ≤2), the pack itself is warning you about what it serves — weigh the flagged facts accordingly and surface the issue to the user when it changes the answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Natural-language query; may include #tag filter tokens" },
                    "k": { "type": "integer", "description": "Max results (default 10)" },
                    "as_of": { "type": "string", "description": "Bi-temporal date YYYY-MM-DD: facts true at this time" },
                    "types": { "type": "array", "items": { "type": "string" }, "description": "Restrict item kinds: episode|fact" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "Filter to episodes carrying all of these user-applied tags (same as #tag in the query)" },
                    "include_private": { "type": "boolean", "description": "Include private-tier episodes (default false)" },
                    "probe": { "type": "boolean", "description": "This read is instrumentation, not interest. It is still logged, but it does not bump the demand signal that ranks probe targets — set it when reading ABOUT an entity on the graph's own behalf (a gossip probe, a sweep), or the reader elects its own next target." },
                    "scope": { "type": "string", "enum": ["both", "facts_only", "evidence_only"], "description": "Retrieval scope (default both). facts_only serves only distilled beliefs (facts, rollups); evidence_only serves only raw episodes. The pack echoes any non-default scope." },
                    "since": { "type": "string", "description": "Only evidence on/after this date, YYYY-MM-DD. Two readers being compared must be given the SAME window, or a difference between them is the world having changed rather than the sources disagreeing." },
                    "until": { "type": "string", "description": "Only evidence before this date, YYYY-MM-DD." },
                    "sources": { "type": "array", "items": { "type": "string" }, "description": "Restrict to these EPISODE sources — note that facts are not source-filtered (a fact has no single source), so with the default scope two differently-sourced reads still share the whole distilled layer; pair `sources` with scope=evidence_only when the point is that two readers see different things. Restrict to (e.g. calendar.event, bee.conversation, slack.thread). Independent observations of the same world, so two readers given different sources can genuinely disagree — unlike the scope split, which compares a distillation with its own origin. Unknown names are an error, never a silent empty result. The pack echoes what it was restricted to." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "kg_entity",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "Resolve a name/alias/email to an entity and return its current facts, per-channel interaction recency, scope context, recent episodes, and `sources` — which episode sources cover this entity, how many episodes each holds and over what span. Multiple matches are returned for disambiguation. Facts carry a `polarity`: 'negative' is a recorded denial — this was already asked and answered no, so treat it as settled and do not propose it again.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name_or_id": { "type": "string" }
                },
                "required": ["name_or_id"]
            }
        },
        {
            "name": "kg_verify",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "Check what the graph BELIEVES against what its evidence actually says — deterministic dereference, no model in the loop. Give a `node` (name/alias/id) for every live claim about it, or a `fact` uid for one. Each verdict is one of: supported (a cited episode literally contains the claim), rederived (a computed claim recomputed from current data and still holds), refuted (a computed claim whose basis has collapsed), contradicted (two live values where one is allowed), denied (a live negation contests it), stale (past its predicate's half-life), residue (evidence exists but does not literally match — PARAPHRASE OR ERROR, needs a reader, NOT a refutation), unrooted (cites nothing), unverifiable (cited evidence is gone), missing. Use it before trusting a fact you are about to act on, and before proposing a correction. Note the division of labour: `residue` is the graph handing the question to you, not an answer.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "node": { "type": "string", "description": "Entity name, alias or id — verifies every live claim about it" },
                    "fact": { "type": "string", "description": "A single fact uid" },
                    "limit": { "type": "integer", "description": "Max claims for a node (default 20); findings are returned first" }
                }
            }
        },
        {
            "name": "kg_pending",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "List fact candidates awaiting review, narrowed to ONE class. The queue is not one problem: clustered by (proposer, predicate) it splits into classes with very different histories — some run 78% accepted, others 2% — so work a class, never the top of the undifferentiated list. Returns each candidate's id, statement, predicate, confidence and the episode it was extracted from. Oldest first: a candidate that has waited longest has had the most chance for corroborating evidence to arrive since. Read-only — deciding a candidate is a separate, human-gated act.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "proposed_by": { "type": "string", "description": "Proposer, e.g. 'llm', 'bee:suggested', 'llm:commitment'" },
                    "predicate": { "type": "string", "description": "Predicate, e.g. 'related_to', 'has_role'" },
                    "limit": { "type": "integer", "description": "Max candidates (default 20)" },
                    "entity": { "type": "string", "description": "Name/alias/id of an entity — returns pending candidates MENTIONING it, across every class. Use this when you already hold deep context on one entity (a gossip probe, a long thread) and want to spend it on the queue: adjudicating what is already pending drains the queue, where proposing new claims grows it. Mutually exclusive with the class pair; matching is over the entity's alias set, so it finds candidates staged before an alias was learned." },
                    "unjudged_by": { "type": "string", "description": "Skip candidates this mechanism already filed a verdict on (e.g. 'corroboration') — batch runs extend coverage instead of re-judging the same oldest N" },
                    "include_evidence": { "type": "boolean", "description": "Include each candidate's origin episode (source, occurred_at, body clipped to 4000 chars) — for verification mechanisms that judge the claim against what it was extracted from" }
                },
                "anyOf": [
                    { "required": ["entity"] },
                    { "required": ["proposed_by", "predicate"] }
                ]
            }
        },
        {
            "name": "kg_verdict",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false },
            "description": "Record what a dialogue concluded about a pending candidate. This is NOT a decision — the candidate stays pending and a human still decides. It is an opinion filed beside the candidate so it can be scored against that decision later, which is the only way a mechanism earns its way up the autonomy ladder. Give the candidate_id, the mechanism that produced it (corroboration|persistence|resolution), the verdict, and a one-line basis naming what was found.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "candidate_id": { "type": "integer" },
                    "mechanism": { "type": "string" },
                    "verdict": { "type": "string" },
                    "basis": { "type": "string" },
                    "model": { "type": "string", "description": "Model that produced it — a local 35B and a frontier model are not the same evidence" }
                },
                "required": ["candidate_id", "mechanism", "verdict"]
            }
        },
        {
            "name": "kg_timeline",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "Bi-temporal history for an entity, including superseded facts and episode timeline.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entity": { "type": "string" },
                    "from": { "type": "string", "description": "YYYY-MM-DD" },
                    "to": { "type": "string", "description": "YYYY-MM-DD" }
                },
                "required": ["entity"]
            }
        },
        {
            "name": "kg_upsert",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false },
            "description": "Write back something learned. kind='fact' is staged as a fact candidate (never directly into the graph) with agent provenance; kind='alias' records a user's answer to a disambiguation; kind='episode' records evidence (e.g. a distilled session summary) — it lands as a source-owned episode whose extracted beliefs still go through the review queue. Re-upserting the same source+source_id updates the episode instead of duplicating it.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "kind": { "type": "string", "enum": ["fact", "alias", "episode"], "description": "fact (default), alias, or episode" },
                    "subject": { "type": "string" },
                    "predicate": { "type": "string" },
                    "object": { "type": "string" },
                    "object_value": { "type": "string" },
                    "statement": { "type": "string", "description": "Natural-language sentence form of the fact" },
                    "valid_from": { "type": "string" },
                    "confidence": { "type": "number" },
                    "alias": { "type": "string", "description": "kind=alias: the alias text" },
                    "node_id": { "type": "string", "description": "kind=alias: the node it belongs to" },
                    "body": { "type": "string", "description": "kind=episode: the episode text" },
                    "source_id": { "type": "string", "description": "kind=episode: stable id within the source (e.g. a session id) — the idempotence key" },
                    "source_ref": { "type": "string", "description": "kind=episode: pointer back to the raw record (e.g. a transcript path)" },
                    "occurred_at": { "type": "string", "description": "kind=episode: when it happened, 'YYYY-MM-DD HH:MM:SS' (default now)" },
                    "occurred_end": { "type": "string", "description": "kind=episode: end of the span, if any" },
                    "sensitivity": { "type": "string", "enum": ["public", "personal", "private", "secret"], "description": "kind=episode: §10 tier (default personal)" },
                    "tags": { "type": "array", "items": { "type": "string" }, "description": "kind=episode: user-vocabulary tags to annotate with" },
                    "meta": { "type": "object", "description": "kind=episode: freeform provenance metadata stored on the episode row (e.g. a taint snapshot). A `corrections` array is processed immediately (D3): [{fact_uid?, wrong?, right?, about?}] — fact_uid (from the pack) is preferred; `wrong` text matches a live fact's statement, narrowed by `about`; `right` stages the replacement, its absence writes a negation. The wrong fact is superseded, its producing class demoted, and its class peers queued for re-audit." },
                    "source": { "type": "string", "description": "e.g. agent:hermes, agent:claude-code" }
                },
                "required": ["source"]
            }
        },
        {
            "name": "kg_related",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "Bounded graph neighborhood around a node: 1-2 hops over current facts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string", "description": "Node id or resolvable name" },
                    "hops": { "type": "integer", "description": "1 or 2 (default 1)" },
                    "types": { "type": "array", "items": { "type": "string" } },
                    "limit": { "type": "integer", "description": "Max nodes (default 25)" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "kg_task_list",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "The GTD board: every open task, actionable statuses first (next, inbox, scheduled, waiting), then by due date. Each task carries its status, due/defer dates, parent project, and who it is waiting on. Use it to answer 'what should Ada do next', to check whether something is already tracked before creating it, and to find overdue items (due_at earlier than today). include_closed adds done/dropped history.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_closed": { "type": "boolean", "description": "Also return done/dropped tasks (default false)" }
                }
            }
        },
        {
            "name": "kg_task_create",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false },
            "description": "Capture a task. Lands in 'inbox' status — captured, not yet committed to — mirroring manual capture in the TUI. Direct write, no review queue: a task the user asked for is an instruction, not an inference about the world (same rule that lets kind=alias land directly in kg_upsert). Check kg_task_list first so the board does not collect duplicates. `project` must name an existing graph node — an unknown name is an error, not an implicit node.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "The task, phrased as an action" },
                    "due": { "type": "string", "description": "YYYY-MM-DD, 'today', 'tomorrow', or '+Nd'" },
                    "project": { "type": "string", "description": "Parent project/topic — must resolve to an existing node" },
                    "context": { "type": "string", "description": "GTD context tag, e.g. '@email', '@lab'" }
                },
                "required": ["name"]
            }
        },
        {
            "name": "kg_task_update",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false },
            "description": "Move a task through its lifecycle (status: next|inbox|scheduled|waiting|done|dropped) and/or edit its scheduling. 'done'/'dropped' stamp completed_at; reopening clears it. For due/defer/context: omit the field to leave it untouched, pass \"\" to clear it. Takes the task's node_id from kg_task_list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The task's node_id, e.g. 'task-1a2b3c4d'" },
                    "status": { "type": "string", "enum": ["next", "inbox", "scheduled", "waiting", "done", "dropped"] },
                    "due": { "type": "string", "description": "New due date (YYYY-MM-DD, 'today', 'tomorrow', '+Nd'); \"\" clears" },
                    "defer": { "type": "string", "description": "Hide until this date; \"\" clears" },
                    "context": { "type": "string", "description": "New context tag; \"\" clears" }
                },
                "required": ["task"]
            }
        }
    ])
}

fn text_result(v: &Value) -> Value {
    json!({
        "content": [{ "type": "text", "text": v.to_string() }],
        "isError": false
    })
}

fn handle_tool_call(
    conn: &Connection,
    embedder: &embed::OllamaEmbedder,
    params: &Value,
) -> Result<Value, String> {
    let name = params
        .get("name")
        .and_then(|n| n.as_str())
        .ok_or("missing tool name")?;
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let out = match name {
        "kg_search" => kg_search(conn, embedder, &args),
        "kg_entity" => kg_entity(conn, &args),
        "kg_timeline" => kg_timeline(conn, &args),
        "kg_upsert" => kg_upsert(conn, &args),
        "kg_related" => kg_related(conn, &args),
        "kg_verify" => kg_verify(conn, &args),
        "kg_pending" => kg_pending(conn, &args),
        "kg_verdict" => kg_verdict(conn, &args),
        "kg_task_list" => kg_task_list(conn, &args),
        "kg_task_create" => kg_task_create(conn, &args),
        "kg_task_update" => kg_task_update(conn, &args),
        _ => Err(mecha_graph_core::Error::Other(format!("unknown tool {name}"))),
    };

    match out {
        Ok(v) => Ok(text_result(&v)),
        Err(e) => Ok(json!({
            "content": [{ "type": "text", "text": format!("error: {e}") }],
            "isError": true
        })),
    }
}

fn kg_search(
    conn: &Connection,
    embedder: &embed::OllamaEmbedder,
    args: &Value,
) -> mecha_graph_core::Result<Value> {
    let mut query = args["query"].as_str().unwrap_or_default().to_string();
    let k = args["k"].as_u64().unwrap_or(10) as usize;
    let include_private = args["include_private"].as_bool().unwrap_or(false);

    // The `tags` param is sugar for `#tag` tokens — fold it into the query so
    // the router stays the single place that understands tags.
    if let Some(tags) = args["tags"].as_array() {
        for t in tags.iter().filter_map(|t| t.as_str()) {
            let t = t.trim().trim_start_matches('#');
            if !t.is_empty() {
                query.push_str(&format!(" #{t}"));
            }
        }
    }
    let query = query.as_str();

    if let Some(as_of) = args["as_of"].as_str() {
        let (entities, ambiguous) = router::detect_entities(conn, query)?;
        let mut items = vec![];
        for e in &entities {
            for f in fact::facts_as_of(conn, &e.node_id, as_of, 25)? {
                items.push(json!({
                    "kind": "fact", "id": f.uid, "text": f.statement,
                    "valid_from": f.valid_from, "valid_to": f.valid_to
                }));
            }
        }
        return Ok(json!({
            "v": 1, "as_of": as_of, "items": items,
            "ambiguous": ambiguous, "truncated": false
        }));
    }

    // A blind gossip Answerer must never silently widen to Both — junk
    // scope strings are an error, not a default.
    let scope = match args["scope"].as_str() {
        None => router::Scope::Both,
        Some(s) => router::Scope::parse(s).ok_or_else(|| {
            mecha_graph_core::Error::Other(format!(
                "bad scope '{s}' (both | facts_only | evidence_only)"
            ))
        })?,
    };

    // Sources are validated, never silently empty: an agent told "Bee
    // knows nothing about her" because of a typo would record a gap that
    // does not exist.
    let mut sources: Vec<String> = vec![];
    if let Some(arr) = args["sources"].as_array() {
        let known = mecha_graph_core::search::known_sources(conn)?;
        for s in arr.iter().filter_map(|s| s.as_str()) {
            if !known.contains(&s.to_string()) {
                return Err(mecha_graph_core::Error::Other(format!(
                    "unknown source '{s}'; known: {}",
                    known.join(", ")
                )));
            }
            sources.push(s.to_string());
        }
    }

    let emb = embedder.available().then_some(embedder);
    // Same window for both readers, or the comparison spans eras.
    let window = match (args["since"].as_str(), args["until"].as_str()) {
        (None, None) => None,
        (from, to) => Some(router::TimeRange {
            from: from.map(|d| format!("{d} 00:00:00")),
            to: to.map(|d| format!("{d} 00:00:00")),
        }),
    };
    let lens = router::Lens {
        scope,
        sources,
        window,
    };
    let pack = router::query_lens(
        conn,
        emb,
        query,
        k,
        4000,
        include_private,
        // `.probe` suffix marks an instrumentation read: it is logged but
        // does not bump demand. Gossip sets it, because a probe that raised
        // its own target's demand would elect that target again next night.
        if args["probe"].as_bool().unwrap_or(false) {
            Some("mcp.kg_search.probe")
        } else {
            Some("mcp.kg_search")
        },
        lens,
    )?;
    let mut v = serde_json::to_value(&pack)?;
    if let Some(types) = args["types"].as_array() {
        let allowed: Vec<&str> = types.iter().filter_map(|t| t.as_str()).collect();
        if let Some(items) = v.get_mut("items").and_then(|i| i.as_array_mut()) {
            items.retain(|i| i["kind"].as_str().is_some_and(|k| allowed.contains(&k)));
        }
    }
    Ok(v)
}

fn kg_entity(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let name = args["name_or_id"].as_str().unwrap_or_default();

    let mut matches = graph::resolve_entity_all(conn, name)?;
    if matches.is_empty() {
        if let Some(n) = graph::get_node(conn, name)? {
            matches.push(n);
        }
    }
    if matches.is_empty() {
        return Ok(json!({ "v": 1, "found": false, "query": name }));
    }
    if matches.len() > 1 {
        // Contradicting-entity envelope: let the agent ask, answer becomes an alias (§11.2).
        let candidates: Vec<Value> = matches
            .iter()
            .map(|n| {
                let pi = rollup::get_person_interaction(conn, &n.id).ok().flatten();
                json!({
                    "id": n.id, "name": n.name, "type": n.node_type,
                    "last_seen": pi.as_ref().and_then(|p| p.last_seen_at.clone()),
                    "interaction_count": pi.map(|p| p.interaction_count).unwrap_or(0)
                })
            })
            .collect();
        return Ok(json!({ "v": 1, "found": true, "ambiguous": candidates }));
    }

    let node = matches.remove(0);
    graph::increment_node_access(conn, &node.id)?;
    let facts: Vec<Value> = fact::facts_for_node(conn, &node.id, 25)?
        .into_iter()
        .map(|f| {
            json!({
                "uid": f.uid, "statement": f.statement, "predicate": f.predicate,
                // 'negative' is a recorded DENIAL, not a weak belief: it
                // means this was asked and answered no. Do not re-assert it.
                "polarity": f.polarity,
                "valid_from": f.valid_from, "confidence": f.confidence,
                "observations": f.observation_count, "extractor": f.extractor
            })
        })
        .collect();
    let episodes: Vec<Value> = episode::episodes_for_node(conn, &node.id, 8)?
        .into_iter()
        .map(|e| {
            json!({
                "uid": e.uid, "source": e.source, "occurred_at": e.occurred_at,
                "preview": e.body.chars().take(200).collect::<String>()
            })
        })
        .collect();
    let pi = rollup::get_person_interaction(conn, &node.id)?;
    let ctx = context::assemble_context(conn, &node.id, 1500)?;
    // Which sources actually cover this entity, and over what span. The
    // rollup already answers this coarsely as per-channel recency; this is
    // the same question asked precisely, and it is what a caller needs to
    // give two readers genuinely different evidence — a source with no
    // coverage yields a confident "I don't know" that reads like a finding.
    let coverage: Vec<Value> = episode::source_coverage(conn, &node.id)?
        .into_iter()
        .map(|(source, episodes, first, last)| {
            json!({ "source": source, "episodes": episodes,
                    "first": first, "last": last })
        })
        .collect();

    Ok(json!({
        "v": 1, "found": true,
        "node": { "id": node.id, "name": node.name, "type": node.node_type,
                  "aliases": node.aliases, "scope_id": node.scope_id },
        "interaction": pi,
        "context": ctx,
        "facts": facts,
        "sources": coverage,
        "episodes": episodes
    }))
}

fn kg_timeline(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let entity = args["entity"].as_str().unwrap_or_default();
    let node = graph::resolve_entity(conn, entity)?
        .ok_or_else(|| mecha_graph_core::Error::Other(format!("no entity '{entity}'")))?;

    let facts: Vec<Value> =
        fact::timeline(conn, &node.id, args["from"].as_str(), args["to"].as_str())?
            .into_iter()
            .map(|f| {
                json!({
                    "uid": f.uid, "statement": f.statement,
                    "valid_from": f.valid_from, "valid_to": f.valid_to,
                    "ingested_at": f.ingested_at, "invalidated_at": f.invalidated_at,
                    "superseded": f.invalidated_at.is_some()
                })
            })
            .collect();

    let episodes: Vec<Value> = episode::episodes_for_node(conn, &node.id, 50)?
        .into_iter()
        .filter(|e| {
            args["from"]
                .as_str()
                .map_or(true, |f| e.occurred_at.as_str() >= f)
                && args["to"]
                    .as_str()
                    .map_or(true, |t| e.occurred_at.as_str() <= t)
        })
        .map(|e| {
            json!({
                "uid": e.uid, "source": e.source, "occurred_at": e.occurred_at,
                "preview": e.body.chars().take(120).collect::<String>()
            })
        })
        .collect();

    Ok(json!({
        "v": 1,
        "entity": { "id": node.id, "name": node.name },
        "facts": facts,
        "episodes": episodes
    }))
}

fn kg_upsert(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let source = args["source"].as_str().unwrap_or("agent:unknown");
    let kind = args["kind"].as_str().unwrap_or("fact");

    if kind == "alias" {
        // Inline disambiguation answers become permanent aliases (§11.2) —
        // deterministic user-confirmed input, so it lands directly.
        let node_id = args["node_id"]
            .as_str()
            .ok_or_else(|| mecha_graph_core::Error::Other("alias upsert needs node_id".into()))?;
        let alias = args["alias"]
            .as_str()
            .ok_or_else(|| mecha_graph_core::Error::Other("alias upsert needs alias".into()))?;
        graph::add_alias(conn, node_id, alias, source)?;
        return Ok(json!({ "v": 1, "status": "alias_added", "node_id": node_id, "alias": alias }));
    }

    if kind == "episode" {
        // An episode is evidence, not a belief, so it does not route through
        // fact_candidate: it lands as a source-owned episode row — browsable
        // via @source, redactable, sensitivity-tiered — and the nightly
        // extractor turns it into candidates that wait in the review queue.
        // The staging guardrail applies to the beliefs derived from the
        // evidence; provenance (`source`) is what lets you undo the evidence.
        let body = args["body"].as_str().unwrap_or_default();
        if body.is_empty() {
            return Err(mecha_graph_core::Error::Other("episode upsert needs body".into()));
        }
        let source_id = args["source_id"].as_str().unwrap_or_default();
        if source_id.is_empty() {
            return Err(mecha_graph_core::Error::Other(
                "episode upsert needs source_id (stable within the source; \
                 re-upserting the same source_id updates instead of duplicating)"
                    .into(),
            ));
        }
        let sensitivity = args["sensitivity"].as_str().unwrap_or("personal");
        if !episode::SENSITIVITY_TIERS.contains(&sensitivity) {
            return Err(mecha_graph_core::Error::Other(format!(
                "sensitivity '{sensitivity}' not in {:?}",
                episode::SENSITIVITY_TIERS
            )));
        }
        let ep = episode::Episode {
            id: 0,
            uid: String::new(),
            source: source.to_string(),
            source_id: source_id.to_string(),
            source_ref: args["source_ref"].as_str().map(|s| s.to_string()),
            body: body.to_string(),
            occurred_at: args["occurred_at"]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(mecha_graph_core::ids::now),
            occurred_end: args["occurred_end"].as_str().map(|s| s.to_string()),
            ingested_at: String::new(),
            lat: None,
            lon: None,
            location: None,
            sensitivity: sensitivity.to_string(),
            scope_id: None,
            meta: args.get("meta").filter(|m| m.is_object()).cloned(),
            raw: None,
        };
        let (id, outcome) = episode::upsert_episode(conn, &ep)?;
        if outcome == episode::IngestOutcome::Tombstoned {
            return Ok(json!({
                "v": 1, "status": "tombstoned", "episode_id": 0, "uid": "",
                "entities_linked": 0,
                "note": "a deleted episode with this source id blocks re-capture; `mecha-graph tombstone rm` lifts it"
            }));
        }
        let linked = match outcome {
            episode::IngestOutcome::Unchanged => 0,
            _ => episode::link_by_alias_scan(conn, id, body)?,
        };
        if let Some(tags) = args["tags"].as_array() {
            for t in tags.iter().filter_map(|t| t.as_str()) {
                episode::annotate_episode(conn, id, "tag", t)?;
            }
        }
        let uid = episode::get_episode(conn, id)?
            .map(|e| e.uid)
            .unwrap_or_default();
        let status = match outcome {
            episode::IngestOutcome::Inserted => "inserted",
            episode::IngestOutcome::Updated => "updated",
            episode::IngestOutcome::Unchanged => "unchanged",
            episode::IngestOutcome::Tombstoned => unreachable!("early return above"),
        };
        // D3: a meta.corrections array is processed inline — a known-wrong
        // fact must not keep being served until the nightly gets to it.
        // (Idempotent per fact, so a retry upsert cannot double-repair.)
        let corrections = if outcome != episode::IngestOutcome::Unchanged {
            let s = mecha_graph_core::corrections::process_episode(conn, id)?;
            (s.processed > 0).then(|| serde_json::to_value(&s).unwrap_or_default())
        } else {
            None
        };
        let mut resp = json!({
            "v": 1, "status": status, "episode_id": id, "uid": uid,
            "entities_linked": linked
        });
        if let Some(c) = corrections {
            resp["corrections"] = c;
        }
        return Ok(resp);
    }

    // Facts from agents are staged, never direct (§9.1).
    let proposed = fact::ProposedFact {
        subject: args["subject"].as_str().unwrap_or_default().to_string(),
        predicate: args["predicate"]
            .as_str()
            .unwrap_or("related_to")
            .to_string(),
        object: args["object"].as_str().map(|s| s.to_string()),
        object_value: args["object_value"].as_str().map(|s| s.to_string()),
        statement: args["statement"].as_str().unwrap_or_default().to_string(),
        valid_from: args["valid_from"].as_str().map(|s| s.to_string()),
        confidence: args["confidence"].as_f64(),
        tags: args["tags"].as_str().map(|s| s.to_string()),
    };
    if proposed.subject.is_empty() || proposed.statement.is_empty() {
        return Err(mecha_graph_core::Error::Other(
            "fact upsert needs subject and statement".into(),
        ));
    }
    let id = fact::propose_fact(conn, &proposed, source, None)?;

    // High-confidence proposals from deterministic contexts auto-accept if the
    // subject resolves; otherwise they wait in the review queue (§11.1).
    let auto = proposed.confidence.unwrap_or(0.7) >= 0.9;
    if auto {
        // Agent-originated auto-accept — not a human verification.
        match fact::accept_candidate_opts(conn, id, false, false) {
            Ok(uid) => {
                return Ok(
                    json!({ "v": 1, "status": "accepted", "candidate_id": id, "fact_uid": uid }),
                )
            }
            Err(_) => {
                // Leave staged; subject didn't resolve.
            }
        }
    }
    Ok(json!({ "v": 1, "status": "staged", "candidate_id": id }))
}

fn kg_related(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let id_or_name = args["id"].as_str().unwrap_or_default();
    let node = match graph::get_node(conn, id_or_name)? {
        Some(n) => n,
        None => graph::resolve_entity(conn, id_or_name)?
            .ok_or_else(|| mecha_graph_core::Error::Other(format!("no node '{id_or_name}'")))?,
    };
    let hops = args["hops"].as_i64().unwrap_or(1).clamp(1, 2) as i32;
    let limit = args["limit"].as_u64().unwrap_or(25) as usize;

    let neighborhood = graph::get_neighborhood(conn, &[&node.id], hops, None, Some(limit))?;
    let type_filter: Option<Vec<&str>> = args["types"]
        .as_array()
        .map(|a| a.iter().filter_map(|t| t.as_str()).collect());

    let items: Vec<Value> = neighborhood
        .into_iter()
        .filter(|e| {
            type_filter
                .as_ref()
                .map_or(true, |tf| tf.contains(&e.node.node_type.as_str()))
        })
        .map(|e| {
            json!({
                "id": e.node.id, "name": e.node.name, "type": e.node.node_type,
                "depth": e.depth,
                "via": e.edge.map(|ed| json!({
                    "predicate": ed.predicate, "from": ed.from_id, "to": ed.to_id
                }))
            })
        })
        .collect();

    Ok(json!({
        "v": 1,
        "root": { "id": node.id, "name": node.name },
        "items": items,
        "truncated": false
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_graph_core::db::open_memory;
    use mecha_graph_core::graph::{upsert_node, Node};

    #[test]
    fn episode_upsert_is_idempotent_on_source_id() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();

        let args = json!({
            "kind": "episode", "source": "agent:mecha", "source_id": "sess-1",
            "body": "Worked with Nadia on the pilot analysis.",
            "occurred_at": "2026-08-05 12:00:00",
            "tags": ["mecha-session"]
        });
        let v = kg_upsert(&conn, &args).unwrap();
        assert_eq!(v["status"], "inserted");
        assert_eq!(v["entities_linked"], 1, "alias scan links Nadia");
        let id = v["episode_id"].as_i64().unwrap();
        let tags = episode::tags_for(&conn, id).unwrap();
        assert_eq!(tags, vec!["mecha-session"]);

        // Same source_id, same body: a no-op, not a duplicate.
        let v2 = kg_upsert(&conn, &args).unwrap();
        assert_eq!(v2["status"], "unchanged");
        assert_eq!(v2["episode_id"].as_i64().unwrap(), id);

        // Same source_id, new body: an update in place.
        let mut args3 = args.clone();
        args3["body"] = json!("Worked with Nadia on the pilot analysis. Decided on mixed models.");
        let v3 = kg_upsert(&conn, &args3).unwrap();
        assert_eq!(v3["status"], "updated");
        assert_eq!(v3["episode_id"].as_i64().unwrap(), id);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1, "re-upserts must never duplicate the episode");
    }

    #[test]
    fn episode_upsert_validates_its_inputs() {
        let conn = open_memory().unwrap();
        // No body.
        let e = kg_upsert(
            &conn,
            &json!({
                "kind": "episode", "source": "agent:mecha", "source_id": "s"
            }),
        );
        assert!(e.is_err());
        // No source_id: without the idempotence key every retry would duplicate.
        let e = kg_upsert(
            &conn,
            &json!({
                "kind": "episode", "source": "agent:mecha", "body": "text"
            }),
        );
        assert!(e.is_err());
        // Invalid sensitivity tier.
        let e = kg_upsert(
            &conn,
            &json!({
                "kind": "episode", "source": "agent:mecha", "source_id": "s",
                "body": "text", "sensitivity": "nonsense"
            }),
        );
        assert!(e.is_err());
        // Nothing landed from any of the failures.
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM episode", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn episode_lands_unextracted_so_the_review_queue_gets_its_beliefs() {
        let conn = open_memory().unwrap();
        let v = kg_upsert(
            &conn,
            &json!({
                "kind": "episode", "source": "agent:mecha", "source_id": "sess-2",
                "body": "Decided the eval judge stays on gemma26."
            }),
        )
        .unwrap();
        let id = v["episode_id"].as_i64().unwrap();
        // The staging guardrail for kind=episode is the extraction pipeline:
        // the episode must be visible to the extractor (no extract_state row),
        // so its beliefs become candidates that wait for review.
        let extracted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM extract_state WHERE episode_id = ?1",
                mecha_graph_core::rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(extracted, 0, "a fresh agent episode awaits extraction");
    }

    #[test]
    fn a_task_crosses_the_wire_and_comes_back_on_the_board() {
        let conn = open_memory().unwrap();
        let v = kg_task_create(
            &conn,
            &json!({ "name": "Renew the domain", "due": "2026-08-17", "context": "@email" }),
        )
        .unwrap();
        assert_eq!(v["status"], "created");
        assert_eq!(v["due_at"], "2026-08-17");
        let id = v["id"].as_str().unwrap().to_string();

        let board = kg_task_list(&conn, &json!({})).unwrap();
        let items = board["items"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["id"], id.as_str());
        assert_eq!(items[0]["status"], "inbox", "capture lands uncommitted");
        assert_eq!(items[0]["context"], "@email");

        // done stamps completed_at; reopening clears it.
        let v = kg_task_update(&conn, &json!({ "task": id, "status": "done" })).unwrap();
        assert!(v["task"]["completed_at"].as_str().is_some());
        let v = kg_task_update(&conn, &json!({ "task": id, "status": "next" })).unwrap();
        assert!(v["task"]["completed_at"].is_null());

        // A closed task leaves the default board and returns with include_closed.
        kg_task_update(&conn, &json!({ "task": id, "status": "dropped" })).unwrap();
        let open = kg_task_list(&conn, &json!({})).unwrap();
        assert_eq!(open["items"].as_array().unwrap().len(), 0);
        let all = kg_task_list(&conn, &json!({ "include_closed": true })).unwrap();
        assert_eq!(all["items"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn schedule_edits_are_tri_state_and_dates_bounce_garbage() {
        let conn = open_memory().unwrap();
        let v = kg_task_create(&conn, &json!({ "name": "t", "due": "2026-09-01" })).unwrap();
        let id = v["id"].as_str().unwrap().to_string();

        // Absent field: untouched. Empty string: cleared.
        let v = kg_task_update(&conn, &json!({ "task": id, "defer": "2026-08-20" })).unwrap();
        assert_eq!(
            v["task"]["due_at"], "2026-09-01",
            "due survives a defer-only edit"
        );
        assert_eq!(v["task"]["defer_until"], "2026-08-20");
        let v = kg_task_update(&conn, &json!({ "task": id, "due": "" })).unwrap();
        assert!(v["task"]["due_at"].is_null(), "\"\" clears");
        assert_eq!(v["task"]["defer_until"], "2026-08-20", "defer untouched");

        // parse_due guards both entry points: bounce, don't store garbage.
        assert!(kg_task_create(&conn, &json!({ "name": "x", "due": "someday" })).is_err());
        assert!(kg_task_update(&conn, &json!({ "task": id, "due": "someday" })).is_err());
        // And a bounced create leaves nothing behind.
        let board = kg_task_list(&conn, &json!({})).unwrap();
        assert_eq!(board["items"].as_array().unwrap().len(), 1);

        // Lifecycle guards: unknown status, unknown task.
        assert!(kg_task_update(&conn, &json!({ "task": id, "status": "finished" })).is_err());
        assert!(kg_task_update(&conn, &json!({ "task": "task-nope", "status": "done" })).is_err());
    }

    #[test]
    fn overdue_is_computed_against_today_not_stored() {
        let conn = open_memory().unwrap();
        kg_task_create(&conn, &json!({ "name": "late", "due": "2020-01-01" })).unwrap();
        kg_task_create(&conn, &json!({ "name": "future", "due": "2999-01-01" })).unwrap();
        let board = kg_task_list(&conn, &json!({})).unwrap();
        let items = board["items"].as_array().unwrap();
        let by_name = |n: &str| items.iter().find(|i| i["name"] == n).unwrap().clone();
        assert_eq!(by_name("late")["overdue"], true);
        assert_eq!(by_name("future")["overdue"], false);
    }
}

/// The Verifier's deterministic tier, over MCP.
///
/// The role lives in mecha — judging is conversational work and pkg stays
/// non-conversational — but the checkable half is data work and the data is
/// pkg's. Exposing it is what lets the mecha-side Judge run deterministic
/// checks first and spend a model only on the residue, which is the ordering
/// PLAN specifies and the 2026-08-13 measurement earned: these checks found
/// 589 real problems on the day blind model probing found none.
/// Pending candidates in one review class, with the evidence they came from.
/// File an agent's opinion beside a candidate. Never decides it.
fn kg_verdict(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let (Some(cid), Some(mech), Some(verdict)) = (
        args["candidate_id"].as_i64(),
        args["mechanism"].as_str(),
        args["verdict"].as_str(),
    ) else {
        return Ok(json!({
            "v": 1,
            "error": "candidate_id, mechanism and verdict are all required"
        }));
    };
    let id = mecha_graph_core::fact::record_verdict(
        conn,
        cid,
        mech,
        verdict,
        args["basis"].as_str().unwrap_or_default(),
        args["model"].as_str(),
    )?;
    Ok(json!({
        "v": 1, "recorded": id, "candidate_id": cid,
        "note": "opinion filed; the candidate is still pending and still needs a human"
    }))
}

/// A candidate's subject, resolved against the graph as it is now.
///
/// Falls back to live entity detection when the staged subject is empty.
/// Staging-time resolution is a snapshot; aliases, merges and name fixes
/// all land afterwards, and nothing re-runs over the queue.
fn subject_now(conn: &Connection, payload: &Value) -> Value {
    if let Some(s) = payload["subject"].as_str() {
        if !s.trim().is_empty() {
            return json!(s);
        }
    }
    let Some(text) = payload["statement"].as_str() else {
        return Value::Null;
    };
    match mecha_graph_core::router::detect_entities(conn, text) {
        Ok((detected, ambiguous)) => {
            if let Some(d) = detected
                .iter()
                .find(|d| d.node_type == "person")
                .or_else(|| detected.first())
            {
                return json!(d.name);
            }
            // Ambiguity is a feature (§8.1) and staging treats it as
            // silence: bee.rs drops the ambiguous arm, so every claim
            // naming a person the graph holds twice loses its subject —
            // when the owner's own name resolves to two nodes that are
            // one person, nearly every suggested candidate arrives
            // subjectless. Report the dominant candidate so downstream
            // work is not blocked, and say that it was a guess. See
            // `subject_ambiguous`.
            ambiguous
                .first()
                .and_then(|a| {
                    a.candidates
                        .iter()
                        .max_by_key(|c| c.interaction_count)
                        .map(|c| json!(c.name))
                })
                .unwrap_or(Value::Null)
        }
        Err(_) => Value::Null,
    }
}

/// Whether the subject above had to be guessed from an ambiguous match.
fn subject_is_guessed(conn: &Connection, payload: &Value) -> bool {
    if payload["subject"]
        .as_str()
        .is_some_and(|s| !s.trim().is_empty())
    {
        return false;
    }
    let Some(text) = payload["statement"].as_str() else {
        return false;
    };
    matches!(
        mecha_graph_core::router::detect_entities(conn, text),
        Ok((detected, ambiguous)) if detected.is_empty() && !ambiguous.is_empty()
    )
}

fn kg_pending(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let proposer = args["proposed_by"].as_str().unwrap_or_default();
    let predicate = args["predicate"].as_str().unwrap_or_default();
    // `entity` is the OTHER axis, not an extra filter on the class one. A
    // reader that has just studied one person should spend that context on
    // every pending claim about them, whatever class it sits in.
    let entity = args["entity"].as_str().unwrap_or_default();
    if entity.is_empty() && (proposer.is_empty() || predicate.is_empty()) {
        return Ok(json!({
            "v": 1,
            "error": "give either `entity`, or both `proposed_by` and `predicate` — \
                      the queue is worked one class at a time, or one entity at a time"
        }));
    }
    let limit = args["limit"].as_i64().unwrap_or(20).clamp(1, 200);
    // A mechanism that names itself gets candidates it has not yet judged,
    // instead of the same oldest N on every run.
    let unjudged_by = args["unjudged_by"].as_str().filter(|s| !s.is_empty());
    // Verification reads the claim against the evidence it was extracted
    // FROM — hand the origin episode over rather than making a reader
    // search for what the row already cites.
    let include_evidence = args["include_evidence"].as_bool().unwrap_or(false);
    let items = if entity.is_empty() {
        mecha_graph_core::fact::pending_in_class(conn, proposer, predicate, limit, unjudged_by)?
    } else {
        // Resolve to the node so the alias set does the matching — a
        // candidate staged before an alias was learned names the surface
        // form, not the canonical one.
        let node = graph::resolve_entity(conn, entity)?
            .ok_or_else(|| mecha_graph_core::Error::Other(format!("no entity '{entity}'")))?;
        let mut surfaces = vec![node.name.clone()];
        surfaces.extend(node.aliases.iter().cloned());
        mecha_graph_core::fact::pending_about_entity(conn, &surfaces, limit, unjudged_by)?
    };
    let items: Vec<Value> = items
        .iter()
        .map(|c| {
            json!({
                "candidate_id": c.id,
                "origin_source": c.episode_id.and_then(|_| {
                    mecha_graph_core::fact::candidate_origin_source(conn, c.id).ok().flatten()
                }),
                "statement": c.payload["statement"],
                // Resolved NOW, not trusted from staging. A candidate's
                // subject is a snapshot of what the graph could resolve on
                // the day it was staged, and the graph improves underneath
                // it: 189 of 200 bee:suggested candidates carry an empty
                // subject while their statements name someone the graph
                // resolves cleanly today, because the aliases arrived after
                // the candidates did. Staged state that nothing revisits
                // goes stale silently.
                "subject": subject_now(conn, &c.payload),
                "subject_ambiguous": subject_is_guessed(conn, &c.payload),
                "object": c.payload["object"],
                "predicate": c.payload["predicate"],
                "confidence": c.confidence,
                "episode_id": c.episode_id,
                "created_at": c.created_at,
                "evidence": include_evidence
                    .then(|| {
                        c.episode_id.and_then(|eid| {
                            mecha_graph_core::episode::get_episode(conn, eid).ok().flatten().map(|e| {
                                json!({
                                    "source": e.source,
                                    "occurred_at": e.occurred_at,
                                    "body": e.body.chars().take(4000).collect::<String>(),
                                })
                            })
                        })
                    })
                    .flatten(),
            })
        })
        .collect();
    Ok(
        json!({ "v": 1, "proposed_by": proposer, "predicate": predicate, "count": items.len(), "items": items }),
    )
}

fn kg_verify(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let checks = match (args["node"].as_str(), args["fact"].as_str()) {
        (_, Some(uid)) => vec![mecha_graph_core::verify::verify_fact(conn, uid)?],
        (Some(name), None) => {
            let mut matches = graph::resolve_entity_all(conn, name)?;
            if matches.is_empty() {
                if let Some(n) = graph::get_node(conn, name)? {
                    matches.push(n);
                }
            }
            match matches.len() {
                0 => return Ok(json!({ "v": 1, "found": false, "query": name })),
                // Ambiguity is a feature (§8.1): say so rather than verifying
                // the wrong person's beliefs.
                n if n > 1 => {
                    return Ok(json!({
                        "v": 1, "found": true,
                        "ambiguous": matches.iter().map(|m| json!({
                            "id": m.id, "name": m.name, "type": m.node_type
                        })).collect::<Vec<_>>()
                    }))
                }
                _ => mecha_graph_core::verify::verify_node(
                    conn,
                    &matches[0].id,
                    args["limit"].as_u64().unwrap_or(20) as usize,
                )?,
            }
        }
        (None, None) => {
            return Err(mecha_graph_core::Error::Other(
                "kg_verify needs `node` or `fact`".into(),
            ))
        }
    };

    // Findings first — verify_node already sorts by severity, and a caller
    // that reads only the head should read the problems.
    let items: Vec<Value> = checks
        .iter()
        .map(|c| {
            json!({
                "fact_uid": c.fact_uid,
                "statement": c.statement,
                "predicate": c.predicate,
                "verdict": format!("{:?}", c.verdict).to_lowercase(),
                "detail": c.detail,
                "cited_episodes": c.cited,
                "supported_by": c.supported_by,
                "observations": c.observations,
                "conflicts_with": c.conflicts_with,
            })
        })
        .collect();
    let findings = items
        .iter()
        .filter(|i| {
            matches!(
                i["verdict"].as_str(),
                Some("missing" | "refuted" | "contradicted" | "denied" | "stale")
            )
        })
        .count();
    Ok(json!({ "v": 1, "items": items, "findings": findings, "truncated": false }))
}

fn kg_task_list(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let include_closed = args["include_closed"].as_bool().unwrap_or(false);
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let items: Vec<Value> = gtd::list_tasks(conn, include_closed)?
        .into_iter()
        .map(|t| {
            let overdue = t
                .due_at
                .as_deref()
                .is_some_and(|d| d < today.as_str() && t.completed_at.is_none());
            json!({
                "id": t.node_id, "name": t.name, "status": t.status,
                "due_at": t.due_at, "defer_until": t.defer_until,
                "context": t.context_tag, "project": t.project,
                "waiting_on": t.waiting_on, "completed_at": t.completed_at,
                "overdue": overdue
            })
        })
        .collect();
    Ok(json!({ "v": 1, "items": items, "today": today, "truncated": false }))
}

fn kg_task_create(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let name = args["name"].as_str().unwrap_or_default();
    let due = match args["due"].as_str() {
        Some(raw) => gtd::parse_due(raw)?,
        None => None,
    };
    let task_id = gtd::create_task(
        conn,
        name,
        due.as_deref(),
        args["project"].as_str(),
        args["context"].as_str(),
    )?;
    Ok(json!({ "v": 1, "status": "created", "id": task_id, "due_at": due }))
}

fn kg_task_update(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let task = args["task"]
        .as_str()
        .ok_or_else(|| mecha_graph_core::Error::Other("kg_task_update needs `task`".into()))?;

    if let Some(status) = args["status"].as_str() {
        gtd::set_task_status(conn, task, status)?;
    }

    // Absent field → untouched; "" → cleared — the same tri-state
    // update_task_schedule speaks, with dates going through parse_due so
    // 'tomorrow' and '+3d' work here too.
    let sched = |v: &Value| -> mecha_graph_core::Result<Option<Option<String>>> {
        match v.as_str() {
            None => Ok(None),
            Some(raw) => Ok(Some(gtd::parse_due(raw)?)),
        }
    };
    let due = sched(&args["due"])?;
    let defer = sched(&args["defer"])?;
    let context = args["context"]
        .as_str()
        .map(|c| Some(c.to_string()).filter(|s| !s.trim().is_empty()));
    if due.is_some() || defer.is_some() || context.is_some() {
        gtd::update_task_schedule(
            conn,
            task,
            due.as_ref().map(|o| o.as_deref()),
            defer.as_ref().map(|o| o.as_deref()),
            context.as_ref().map(|o| o.as_deref()),
        )?;
    }

    let updated = gtd::list_tasks(conn, true)?
        .into_iter()
        .find(|t| t.node_id == task)
        .map(|t| {
            json!({
                "id": t.node_id, "name": t.name, "status": t.status,
                "due_at": t.due_at, "defer_until": t.defer_until,
                "context": t.context_tag, "completed_at": t.completed_at
            })
        });
    Ok(json!({ "v": 1, "status": "updated", "task": updated }))
}
