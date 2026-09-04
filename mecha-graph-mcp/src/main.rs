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
            eprintln!("mecha-graph-mcp: cannot open {}: {e}", db_path.display());
            std::process::exit(1);
        }
    };
    let embedder = embed::Embedder::default();

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
            "description": "Resolve a name/alias/email to an entity and return its current facts, per-channel interaction recency, scope context, recent episodes, `sources` — which episode sources cover this entity, how many episodes each holds and over what span — and `tasks`, this entity's board split into `open` and `closed`. Multiple matches are returned for disambiguation. Facts carry a `polarity`: 'negative' is a recorded denial — this was already asked and answered no, so treat it as settled and do not propose it again. Each entry in a task's `about` is `{name, unreviewed}` — the title scan proposes associations at tier 'shadow', and `unreviewed: true` marks one nothing has vetted, so do not report it to a person as established without saying so.",
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
            "name": "kg_shadow_queue",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "The surfaced-verdict queue (review-on-use): live UNREVIEWED (shadow) facts that are about to matter — each with the reasons it surfaced (contradicts a reviewed fact / was served in a context pack N times / spot-check of a sampled class). Shadow facts are already retrievable, rank-discounted and labeled 'unreviewed'; this queue is what a human should look at next. Read-only, and deliberately so: show the owner what surfaced, but the verdict itself (confirm/refute) is a human act on a human surface — `pkg shadow --confirm/--refute` or the TUI.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "Max facts surfaced (default 10)" }
                }
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
            "name": "kg_notes",
            "annotations": { "readOnlyHint": true, "openWorldHint": false },
            "description": "The owner's own captured notes (source='note' episodes), newest first. A listing, not a search: what was recently written down, for surfaces that show a notebook. Each row carries `source_id`: re-upserting an episode under source='note' with that id UPDATES the note in place, which is how a notebook offers an edit rather than a second copy.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "description": "max rows (default 20)" }
                }
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
                    "valid_from": { "type": "string", "description": "When this became true, as YYYY-MM-DD (an RFC 3339 instant is accepted and keeps its date half). Anything else is refused rather than stored: this column is compared as a date, so prose sorts as one and silently answers the wrong side of every as_of query. Omit it when you do not know — absent is honest, a guess is not." },
                    "confidence": { "type": "number" },
                    "alias": { "type": "string", "description": "kind=alias: the alias text" },
                    "node_id": { "type": "string", "description": "kind=alias: the node it belongs to (an id, not a name)" },
                    "remove": { "type": "boolean", "description": "kind=alias: remove the alias instead of adding it — the repair for a name that belonged to somebody else. Only on the user's explicit instruction; removing an alias changes where every future mention of that name lands." },
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
            "description": "The GTD board: every open task, actionable statuses first (next, inbox, scheduled, waiting), then by due date. Each task carries its status, due/defer dates, parent project, who it is waiting on, the entities it is `about` (each `{name, unreviewed}`, where `unreviewed: true` means a title-scan guess nobody has vetted — say so rather than reporting it as established), and — when it was captured from something — a `captured_from` pointer at the original (the email that asked, the request, the conversation). Use it to answer 'what should Ada do next', to check whether something is already tracked before creating it, and to find overdue items (due_at earlier than today). include_closed adds done/dropped history. `entity` narrows to one person, project or topic — pair it with include_closed to answer 'everything, open and finished, involving X'.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_closed": { "type": "boolean", "description": "Also return done/dropped tasks (default false)" },
                    "entity": { "type": "string", "description": "Only tasks associated with this person, project or topic, by name or node id. Unions three associations: `about` (what the task concerns — survives completion), `waiting_on` (who currently holds the ball — cleared when the task closes) and `assigned_to`, plus tasks whose parent project IS this node. An unknown name is an error, not an empty list." }
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
                    "context": { "type": "string", "description": "GTD context tag, e.g. '@email', '@lab'" },
                    "about": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "People, projects or topics this task concerns, by name. A PERMANENT association: it survives completion, which is what makes a finished task still findable under the person it was for. Use it for 'who/what is this task about'; use waiting_on (on kg_task_update) for 'who owes me this right now', which is cleared when the task closes. Each name must already resolve to a node — an unknown one is an error, never a new node."
                    },
                    "captured_from": {
                        "type": "object",
                        "description": "What prompted this task, so a person can read the original later — `mecha tasks source <id>` follows it. A pointer, never a copy: no bodies, no quoted text, and a key that is not listed here is refused. Set it when the task comes from something with an address; omit it entirely for one somebody typed, where the absence is the honest answer.",
                        "properties": {
                            "kind": { "type": "string", "enum": ["mail", "frontdoor", "session"] },
                            "id": { "type": "string", "description": "Which one — a thread id, request id, session id" },
                            "account": { "type": "string", "description": "Required for kind 'mail': thread ids are account-scoped" },
                            "label": { "type": "string", "description": "A handle a human recognises it by, e.g. the subject line" },
                            "at": { "type": "string", "description": "When the original is dated, RFC 3339" }
                        },
                        "required": ["kind", "id"]
                    }
                },
                "required": ["name"]
            }
        },
        {
            "name": "kg_task_update",
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "openWorldHint": false },
            "description": "Move a task through its lifecycle (status: next|inbox|scheduled|waiting|done|dropped) and/or edit its scheduling. 'done'/'dropped' stamp completed_at; reopening clears it. For due/defer/context/waiting_on: omit the field to leave it untouched, pass \"\" to clear it. waiting_on names who currently has the ball and must already exist in the graph. Takes the task's node_id from kg_task_list.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "task": { "type": "string", "description": "The task's node_id, e.g. 'task-1a2b3c4d'" },
                    "status": { "type": "string", "enum": ["next", "inbox", "scheduled", "waiting", "done", "dropped"] },
                    "due": { "type": "string", "description": "New due date (YYYY-MM-DD, 'today', 'tomorrow', '+Nd'); \"\" clears" },
                    "defer": { "type": "string", "description": "Hide until this date; \"\" clears" },
                    "context": { "type": "string", "description": "New context tag; \"\" clears" },
                    "waiting_on": { "type": "string", "description": "Who has the ball — a person or agent the graph already knows, by name; '@owner' means whoever this graph is about; \"\" clears. Use with status 'waiting'. Cleared automatically when the task moves to done/dropped, because nobody owes a finished task; the task stays findable under that person through its `about` association." },
                    "about_add": { "type": "array", "items": { "type": "string" }, "description": "Also file this task under these people/projects/topics. Permanent association that survives completion — see kg_task_create's `about`. Adds; it never replaces what is already there." },
                    "about_remove": { "type": "array", "items": { "type": "string" }, "description": "Stop filing this task under these entities. A valid-time close (the association ended), not a retraction of something that was never true." },
                    "session": { "type": "string", "description": "The agent conversation working this task. Set by the harness that starts one — do not invent a value; \"\" clears." },
                    "captured_from": { "description": "What the task was captured from — same object kg_task_create takes; \"\" clears. Set it from what you actually read, never reconstructed from the task's wording." }
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
    embedder: &embed::Embedder,
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
        "kg_notes" => kg_notes(conn, &args),
        "kg_upsert" => kg_upsert(conn, &args),
        "kg_related" => kg_related(conn, &args),
        "kg_verify" => kg_verify(conn, &args),
        "kg_pending" => kg_pending(conn, &args),
        "kg_shadow_queue" => kg_shadow_queue(conn, &args),
        "kg_verdict" => kg_verdict(conn, &args),
        "kg_task_list" => kg_task_list(conn, &args),
        "kg_task_create" => kg_task_create(conn, &args),
        "kg_task_update" => kg_task_update(conn, &args),
        _ => Err(mecha_graph_core::Error::Other(format!(
            "unknown tool {name}"
        ))),
    };

    match out {
        Ok(v) => Ok(text_result(&v)),
        Err(e) => Ok(json!({
            "content": [{ "type": "text", "text": format!("error: {e}") }],
            "isError": true
        })),
    }
}

/// The surfaced-verdict queue, read-only. The verdict verbs stay off the
/// MCP surface on purpose: an agent relaying "the owner said yes" is a
/// paraphrase, and a lane must not promote itself — confirmation crosses
/// a human surface (CLI/TUI) structurally, not by convention.
fn kg_shadow_queue(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let limit = args["limit"].as_u64().unwrap_or(10).min(50) as usize;
    let (q, total) = mecha_graph_core::shadow::surfaced_counted(conn, limit)?;
    let (live, served) = mecha_graph_core::shadow::shadow_counts(conn)?;
    Ok(json!({
        "surfaced_total": total,
        "surfaced": q.iter().map(|s| json!({
            "fact_uid": s.fact.uid,
            "statement": s.fact.statement,
            "predicate": s.fact.predicate,
            "extractor": s.fact.extractor,
            "confidence": s.fact.confidence,
            "reasons": s.reasons,
            "touches": s.touches,
            "last_served": s.last_served,
        })).collect::<Vec<_>>(),
        "shadow_live": live,
        "shadow_served": served,
        "note": "verdicts are human-gated: pkg shadow --confirm <uid> / --refute <uid> --reason '…'",
    }))
}

/// The owner's notes, newest first. Deliberately source='note' only: this
/// is the notebook view, and mixing in distilled sessions or mail episodes
/// would make it a feed nobody asked for.
fn kg_notes(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let limit = args["limit"].as_u64().unwrap_or(20).min(200);
    // `source_id` rides along beside `uid`, and the pair is not redundant:
    // `uid` names the row, but the only key that can *write* to it is
    // (source, source_id) — the idempotence key `upsert_episode` matches on.
    // A notebook that lists notes without it can offer no edit at all, which
    // is what mecha's notes page discovered by having nothing to send back.
    let mut stmt = conn.prepare(
        "SELECT uid, source_id, body, occurred_at FROM episode
         WHERE source = 'note'
         ORDER BY occurred_at DESC, id DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map([limit], |r| {
            Ok(serde_json::json!({
                "uid": r.get::<_, String>(0)?,
                "source_id": r.get::<_, String>(1)?,
                "body": r.get::<_, String>(2)?,
                "occurred_at": r.get::<_, String>(3)?,
            }))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(serde_json::json!({ "notes": rows }))
}

fn kg_search(
    conn: &Connection,
    embedder: &embed::Embedder,
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
    // `facts_for_node` drops a task's `about` pointing at this node for every
    // caller, so there is nothing to opt into here — see its doc for why the
    // rule is scoped by predicate rather than by caller.
    let facts: Vec<Value> = fact::facts_for_node(conn, &node.id, 25)?
        .into_iter()
        .map(|f| {
            json!({
                "uid": f.uid, "statement": f.statement, "predicate": f.predicate,
                // 'negative' is a recorded DENIAL, not a weak belief: it
                // means this was asked and answered no. Do not re-assert it.
                "polarity": f.polarity,
                "valid_from": f.valid_from, "confidence": f.confidence,
                "observations": f.observation_count, "extractor": f.extractor,
                // review-on-use: 'shadow' facts are served but unvetted;
                // anything not 'reviewed' is unreviewed.
                "tier": f.tier
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
    // The deterministic keys — an email, a handle — that decide where future
    // ingest lands. Aliases are how the node is *spoken of*; identifiers are
    // how sources *reach* it, which is why a split that leaves one behind
    // re-merges on the next sync. Surfaced so a person can see the
    // difference before repairing a conflation.
    let identifiers: Vec<Value> = {
        let mut stmt = conn.prepare(
            "SELECT kind, value FROM node_identifier
             WHERE node_id = ?1 ORDER BY kind, value",
        )?;
        let rows = stmt
            .query_map([&node.id], |r| {
                Ok(json!({ "kind": r.get::<_, String>(0)?, "value": r.get::<_, String>(1)? }))
            })?
            .collect::<std::result::Result<Vec<_>, _>>()?;
        rows
    };
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

    // The board, filtered to this entity, split by whether it is still live.
    //
    // A block of its own rather than more `facts` rows: the association IS
    // in `facts` already (it is an `about`/`waiting_on` edge), but a fact
    // statement carries no status and no due date, and the 25-row cap means
    // a busy person's tasks fall off the bottom silently. Splitting open
    // from closed here is what makes "and what did we finish" answerable
    // without a second call.
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let all_tasks = gtd::tasks_for_entity(conn, &node.id, true)?;
    let (mut closed, open): (Vec<_>, Vec<_>) = all_tasks
        .iter()
        .partition(|t| matches!(t.status.as_str(), "done" | "dropped"));
    // The board's ordering sorts `due_at` ahead of `completed_at`, which is
    // right for work you still have to do and wrong for the first caller that
    // takes a *prefix* of the finished pile: it would keep the 15 with the
    // earliest deadlines rather than the 15 most recently finished. "What did
    // we just wrap up" is the only question this half of the block answers.
    closed.sort_by(|a, b| b.completed_at.cmp(&a.completed_at));
    // Capped, like every other block on this card (`facts` 25, `episodes` 8,
    // `context` 1500 tokens). Closed tasks only ever accumulate, and a
    // project node collects every task ever filed under it, so an uncapped
    // block grows without bound on exactly the nodes most worth asking
    // about. The objection that motivated this block was a cap that
    // truncates SILENTLY — so the count and the flag are reported, and
    // `kg_task_list` with `entity` is named as the way to see the rest.
    const TASK_CAP: usize = 15;
    let block = |rows: &[&gtd::TaskItem]| {
        json!({
            "items": rows.iter().take(TASK_CAP).map(|t| task_json(t, &today)).collect::<Vec<_>>(),
            "total": rows.len(),
            "truncated": rows.len() > TASK_CAP,
        })
    };
    let tasks = json!({
        "open": block(&open),
        "closed": block(&closed),
        "all": "kg_task_list with `entity` returns the full list, unabridged",
    });

    Ok(json!({
        "v": 1, "found": true,
        "node": { "id": node.id, "name": node.name, "type": node.node_type,
                  "aliases": node.aliases, "identifiers": identifiers,
                  "scope_id": node.scope_id },
        "interaction": pi,
        "context": ctx,
        "facts": facts,
        "tasks": tasks,
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
        if args["remove"].as_bool() == Some(true) {
            // The other repair (the CLI's `unalias`): the name belonged to
            // somebody else. Symmetric with add — both directions are the
            // owner answering a disambiguation, and removing is the safer of
            // the two (an alias is re-addable; a wrong one keeps mis-linking
            // every future mention). node_id is an id, never a name: on a
            // repair verb a name lookup could land on exactly the conflated
            // node being repaired.
            let removed = graph::remove_alias(conn, node_id, alias)?;
            return Ok(json!({
                "v": 1,
                "status": if removed { "alias_removed" } else { "alias_absent" },
                "node_id": node_id,
                "alias": alias,
                "removed": removed,
            }));
        }
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
            return Err(mecha_graph_core::Error::Other(
                "episode upsert needs body".into(),
            ));
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
        // **The second faucet.** `accept_commitment` learned to parse its
        // date; this writes the same column and did not, and it is the
        // higher-volume path — agents write beliefs constantly, commitments
        // are rare — and at `confidence >= 0.9` it auto-accepts, so the value
        // reaches `fact.valid_from` verbatim with no human in between.
        // Shipping `repair-dates` without closing this makes the repair a
        // treadmill: its idempotence holds in the unit test and fails on a
        // live graph by morning. Refused rather than dropped here, unlike the
        // commitment path — a caller staging one fact can be told about one
        // bad field, where a batch accept cannot stop for each.
        valid_from: match args["valid_from"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        {
            Some(raw) => Some(
                gtd::parse_due(raw)
                    .map_err(|e| {
                        mecha_graph_core::Error::Other(format!(
                            "`valid_from` must be YYYY-MM-DD (or an RFC 3339 instant): {e}"
                        ))
                    })?
                    .ok_or_else(|| {
                        mecha_graph_core::Error::Other("`valid_from` parsed to nothing".into())
                    })?,
            ),
            None => None,
        },
        confidence: args["confidence"].as_f64(),
        tags: args["tags"].as_str().map(|s| s.to_string()),
        ..Default::default()
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
    fn alias_upsert_removes_with_the_remove_flag() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("rowan-1", "person", "Rowan Ellery")).unwrap();

        // Add, then remove — the round trip of a conflation repair.
        let v = kg_upsert(
            &conn,
            &json!({ "kind": "alias", "node_id": "rowan-1", "alias": "rowan" }),
        )
        .unwrap();
        assert_eq!(v["status"], "alias_added");

        let v = kg_upsert(
            &conn,
            &json!({ "kind": "alias", "node_id": "rowan-1", "alias": "rowan", "remove": true }),
        )
        .unwrap();
        assert_eq!(v["status"], "alias_removed");
        assert_eq!(v["removed"], true);
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM node_alias WHERE node_id = 'daniel-1' AND alias = 'daniel'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "the alias row must actually be gone");

        // Removing what is already gone reports the truth, not an error —
        // the caller asked for it to be absent, and it is.
        let v = kg_upsert(
            &conn,
            &json!({ "kind": "alias", "node_id": "rowan-1", "alias": "rowan", "remove": true }),
        )
        .unwrap();
        assert_eq!(v["status"], "alias_absent");
        assert_eq!(v["removed"], false);
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

    /// The board can now say *who* has the ball, which is what makes
    /// "waiting" mean something a person can act on.
    #[test]
    fn waiting_on_names_who_has_the_ball_and_refuses_a_stranger() {
        let conn = open_memory().unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "get the signed copy" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // The agent node ships with the schema, so a task can be delegated on
        // a graph nobody has hand-populated.
        let v = kg_task_update(
            &conn,
            &json!({ "task": id, "status": "waiting", "waiting_on": "mecha" }),
        )
        .unwrap();
        assert_eq!(v["task"]["status"], "waiting");
        assert_eq!(
            v["task"]["waiting_on"], "mecha",
            "reported back, or a caller cannot tell a set from a no-op"
        );

        // Typo protection, exactly as `project` has: an unknown name is an
        // error rather than a new node. Without this, "waiting on Nadai"
        // quietly becomes a person.
        let err = kg_task_update(&conn, &json!({ "task": id, "waiting_on": "Nadai" }));
        assert!(err.is_err(), "an unknown name must not mint a node");
        assert!(format!("{:#}", err.unwrap_err()).contains("no node matches"));

        // The failed set left the previous answer standing rather than
        // half-clearing it.
        let board = kg_task_list(&conn, &json!({})).unwrap();
        assert_eq!(board["items"][0]["waiting_on"], "mecha");

        // "" clears, on the same tri-state the other fields speak.
        let v = kg_task_update(&conn, &json!({ "task": id, "waiting_on": "" })).unwrap();
        assert!(v["task"]["waiting_on"].is_null());
    }

    /// One live claim about who owes a task, however many times it moves.
    /// The old belief is invalidated rather than deleted — who *used* to have
    /// it is the history a bi-temporal store exists for.
    #[test]
    fn re_pointing_waiting_on_leaves_exactly_one_live_fact() {
        let conn = open_memory().unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "t" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let person = mecha_graph_core::graph::Node::new("person-x", "person", "Nadia");
        mecha_graph_core::graph::upsert_node(&conn, &person).unwrap();

        kg_task_update(&conn, &json!({ "task": id, "waiting_on": "Nadia" })).unwrap();
        kg_task_update(&conn, &json!({ "task": id, "waiting_on": "mecha" })).unwrap();

        let live: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact
                  WHERE subject_id = ?1 AND predicate = 'waiting_on'
                    AND valid_to IS NULL AND invalidated_at IS NULL",
                mecha_graph_core::rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            live, 1,
            "two live claims about who owes this is not a state"
        );

        let all: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact WHERE subject_id = ?1 AND predicate = 'waiting_on'",
                mecha_graph_core::rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(all, 2, "and the first one is history, not gone");
    }

    /// The link back to the conversation that worked a task — an attribute,
    /// because an edge to the session's episode would exist only for the runs
    /// the distiller judged worth remembering, and the runs it skips are
    /// exactly the ones a person half-remembers and wants to reopen.
    #[test]
    fn a_task_remembers_the_conversation_that_worked_it() {
        let conn = open_memory().unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "t" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        assert!(
            kg_task_list(&conn, &json!({})).unwrap()["items"][0]["session"].is_null(),
            "a task nobody has worked names no conversation"
        );

        let v = kg_task_update(
            &conn,
            &json!({ "task": id, "session": "20260826T101804-476080dd" }),
        )
        .unwrap();
        assert_eq!(v["task"]["session"], "20260826T101804-476080dd");
        // And on the board, which is what a row renders from.
        let board = kg_task_list(&conn, &json!({})).unwrap();
        assert_eq!(board["items"][0]["session"], "20260826T101804-476080dd");

        // "" clears, like every other field on this tool.
        let v = kg_task_update(&conn, &json!({ "task": id, "session": "" })).unwrap();
        assert!(v["task"]["session"].is_null());
    }

    /// The way back to what asked for the task, across the wire.
    ///
    /// Capture and update both take it, because the two capture paths differ:
    /// `mecha mail task` knows the thread as it creates the task, while a run
    /// that discovers the source later fills it in.
    #[test]
    fn a_task_remembers_what_asked_for_it() {
        let conn = open_memory().unwrap();
        let mail = json!({
            "kind": "mail", "account": "ostrander", "id": "thread-19a2f",
            "label": "SAS 2027 award nominations", "at": "2026-08-11T14:02:00Z",
        });

        // Set at capture — the mail path, where the thread is in hand.
        let id = kg_task_create(
            &conn,
            &json!({ "name": "Decide on the nominations", "captured_from": mail }),
        )
        .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let board = kg_task_list(&conn, &json!({})).unwrap();
        assert_eq!(board["items"][0]["captured_from"]["kind"], "mail");
        assert_eq!(board["items"][0]["captured_from"]["id"], "thread-19a2f");
        assert_eq!(board["items"][0]["captured_from"]["account"], "ostrander");

        // A task typed into the board carries nothing, and the row says so
        // with a null rather than a `{"kind": "manual"}` that opens nothing.
        let plain = kg_task_create(&conn, &json!({ "name": "buy milk" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let board = kg_task_list(&conn, &json!({})).unwrap();
        let plain_row = board["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["id"] == plain.as_str())
            .unwrap();
        assert!(plain_row["captured_from"].is_null());

        // Filled in later, and reported back — a caller cannot otherwise tell
        // a successful set from a silently-ignored one.
        let v = kg_task_update(
            &conn,
            &json!({ "task": plain, "captured_from": {"kind": "session", "id": "20260826T101804-476080dd"} }),
        )
        .unwrap();
        assert_eq!(v["task"]["captured_from"]["kind"], "session");

        // `""` clears, like every other field on this tool. `null` cannot:
        // an absent key deserialises to exactly that, so omitting the field
        // would wipe the pointer on every unrelated status change.
        let v = kg_task_update(&conn, &json!({ "task": id, "status": "next" })).unwrap();
        assert_eq!(
            v["task"]["captured_from"]["id"], "thread-19a2f",
            "moving a task must not forget where it came from"
        );
        let v = kg_task_update(&conn, &json!({ "task": id, "captured_from": "" })).unwrap();
        assert!(v["task"]["captured_from"].is_null());

        // A copy is refused at the tool boundary too — the graph holds a
        // pointer at other people's words, never the words.
        assert!(kg_task_create(
            &conn,
            &json!({ "name": "x", "captured_from": {"kind": "mail", "account": "a", "id": "t", "body": "Dear Ada, …"} })
        )
        .is_err());
    }

    /// `@owner` is how a harness hands work back without carrying the
    /// owner's name around — and a graph with no owner says so rather than
    /// silently naming nobody.
    #[test]
    fn owner_is_addressable_without_knowing_the_owners_name() {
        let conn = open_memory().unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "t" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        let err = kg_task_update(&conn, &json!({ "task": id, "waiting_on": "@owner" }));
        assert!(err.is_err(), "no owner set is a named failure, not a no-op");
        assert!(format!("{:#}", err.unwrap_err()).contains("no owner set"));

        let me = mecha_graph_core::graph::Node::new("person-me", "person", "Ada");
        mecha_graph_core::graph::upsert_node(&conn, &me).unwrap();
        mecha_graph_core::graph::set_owner(&conn, "person-me").unwrap();

        let v = kg_task_update(&conn, &json!({ "task": id, "waiting_on": "@owner" })).unwrap();
        assert_eq!(v["task"]["waiting_on"], "Ada");
    }

    /// The agent is not a person, and the distinction is the point: a task
    /// waits on `mecha` without `mecha` turning up in every people-shaped
    /// view, because responsibility does not transfer to it.
    #[test]
    fn the_agent_node_ships_and_is_not_a_person() {
        let conn = open_memory().unwrap();
        let kind: String = conn
            .query_row(
                "SELECT node_type FROM nodes WHERE id = 'agent-mecha'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(kind, "agent");
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

    /// An entity nobody has heard of is an ERROR, not an empty board.
    ///
    /// "No tasks for Ostrander" and "there is nobody here called Ostrander"
    /// are opposite findings, and the caller that cannot distinguish them
    /// will confidently report the first. Same shape as the unreadable store
    /// that must not read as an empty queue.
    #[test]
    fn an_unknown_entity_is_an_error_not_an_empty_board() {
        let conn = open_memory().unwrap();
        kg_task_create(&conn, &json!({ "name": "something" })).unwrap();
        let err = kg_task_list(&conn, &json!({ "entity": "Nobody At All" }));
        assert!(err.is_err(), "an unresolvable name must not answer with []");

        // And the real thing resolves, echoing who it resolved to so the
        // caller can see which of two same-named people it got.
        upsert_node(&conn, &Node::new("p-ostrander", "person", "Ostrander")).unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "write it up" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        kg_task_update(&conn, &json!({ "task": id, "about_add": ["Ostrander"] })).unwrap();
        let board = kg_task_list(&conn, &json!({ "entity": "Ostrander" })).unwrap();
        assert_eq!(board["items"].as_array().unwrap().len(), 1);
        assert_eq!(board["entity"]["name"], "Ostrander");
        assert_eq!(board["items"][0]["about"][0]["name"], "Ostrander");
        assert_eq!(
            board["items"][0]["about"][0]["unreviewed"], false,
            "a hand-set association is not a guess, and the surface says which"
        );
    }

    /// The entity's card carries its board, split by whether it is still
    /// live — the second surface, and the one a "tell me about X" lands on.
    #[test]
    fn an_entity_card_carries_its_open_and_closed_tasks() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-wren", "person", "Wren")).unwrap();
        let open = kg_task_create(&conn, &json!({ "name": "open one", "about": ["Wren"] }))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        let done = kg_task_create(&conn, &json!({ "name": "done one", "about": ["Wren"] }))
            .unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        kg_task_update(&conn, &json!({ "task": done, "status": "done" })).unwrap();

        let card = kg_entity(&conn, &json!({ "name_or_id": "Wren" })).unwrap();
        let tasks = &card["tasks"];
        assert_eq!(tasks["open"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(tasks["closed"]["items"].as_array().unwrap().len(), 1);
        assert_eq!(tasks["open"]["items"][0]["id"], open);
        assert_eq!(tasks["closed"]["items"][0]["id"], done);
        assert_eq!(tasks["open"]["truncated"], false);
        assert!(
            tasks["closed"]["items"][0]["completed_at"].is_string(),
            "a closed task carries when it closed"
        );
    }

    /// A bad `about` name creates no task at all.
    ///
    /// Resolving after the create returned an error carrying no id, for a
    /// task that existed — so the caller fixed the spelling, retried, and
    /// made a second one. The near-miss name is the common case, because the
    /// caller is guessing at spellings.
    #[test]
    fn an_unresolvable_about_name_creates_no_task() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-wren", "person", "Wren")).unwrap();
        let before = gtd::list_tasks(&conn, true).unwrap().len();

        let err = kg_task_create(
            &conn,
            &json!({ "name": "write it up", "about": ["Wren", "Wrenn"] }),
        );
        assert!(err.is_err(), "a name that does not resolve fails the call");
        assert_eq!(
            gtd::list_tasks(&conn, true).unwrap().len(),
            before,
            "and leaves no orphan behind for the retry to duplicate"
        );

        // The good spelling works and associates both nothing-left-behind.
        kg_task_create(&conn, &json!({ "name": "write it up", "about": ["Wren"] })).unwrap();
        assert_eq!(gtd::list_tasks(&conn, true).unwrap().len(), before + 1);
    }

    /// A bare string where an array is specified fails, on both tools.
    ///
    /// `as_array()` returns None for it, and a `let … else` turned that into
    /// an empty list — so the commonest shape mistake here answered
    /// `created`/`updated` having written nothing.
    #[test]
    fn a_bare_string_where_an_array_belongs_is_refused_on_both_tools() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();

        assert!(
            kg_task_create(&conn, &json!({ "name": "x", "about": "Nadia" })).is_err(),
            "create refuses the shape"
        );
        assert!(
            gtd::list_tasks(&conn, true).unwrap().is_empty(),
            "and writes no task while doing so"
        );

        let id = kg_task_create(&conn, &json!({ "name": "x" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(
            kg_task_update(&conn, &json!({ "task": id, "about_add": "Nadia" })).is_err(),
            "and update refuses it identically"
        );
        assert!(gtd::task_about(&conn, &id).unwrap().is_empty());

        // The right shape still works.
        kg_task_update(&conn, &json!({ "task": id, "about_add": ["Nadia"] })).unwrap();
        assert_eq!(gtd::task_about(&conn, &id).unwrap().len(), 1);
    }

    /// Finishing a task and naming who owed it, in ONE call, still leaves
    /// nobody owing it.
    ///
    /// `status` was applied first, so the close ran before the claim existed
    /// and then the claim was asserted onto an already-finished task — with
    /// nothing left to close it, ever. The single call most likely to hit it
    /// is the natural one: recording who owed a thing at the moment you
    /// record that it is done.
    #[test]
    fn closing_and_naming_the_owed_party_in_one_call_leaves_no_live_claim() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "the thing" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        kg_task_update(
            &conn,
            &json!({ "task": id, "status": "done", "waiting_on": "Nadia" }),
        )
        .unwrap();

        let live: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_current
                  WHERE subject_id = ?1 AND predicate = 'waiting_on'",
                mecha_graph_core::rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            live, 0,
            "nobody owes a finished task, whatever the field order"
        );

        // And she still has it on her card, through the closed claim.
        let found = gtd::tasks_for_entity(&conn, "p-nadia", true).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].previously_waiting_on, vec!["Nadia".to_string()]);
    }

    /// Reopening a task and naming who owes it, in one call, leaves a LIVE
    /// claim — the mirror of the closing case.
    ///
    /// Both directions in one test on purpose. The two cases were fixed in
    /// separate rounds and the second fix broke the first, because each was
    /// an ordering tweak and the order can only satisfy one at a time.
    /// Asserting them together is what stops a third reorder passing.
    #[test]
    fn status_and_waiting_on_in_one_call_agree_in_both_directions() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();
        let live = |id: &str| -> i64 {
            conn.query_row(
                "SELECT COUNT(*) FROM fact_current
                  WHERE subject_id = ?1 AND predicate = 'waiting_on'",
                mecha_graph_core::rusqlite::params![id],
                |r| r.get(0),
            )
            .unwrap()
        };

        // Closing direction: done + a name leaves nobody owing it.
        let a = kg_task_create(&conn, &json!({ "name": "a" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        kg_task_update(
            &conn,
            &json!({ "task": a, "status": "done", "waiting_on": "Nadia" }),
        )
        .unwrap();
        assert_eq!(live(&a), 0, "a finished task holds nobody");

        // Reopening direction, on a task that was already done: the claim
        // must survive, or you get an open `waiting` task nobody owes.
        let b = kg_task_create(&conn, &json!({ "name": "b" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        kg_task_update(&conn, &json!({ "task": b, "status": "done" })).unwrap();
        kg_task_update(
            &conn,
            &json!({ "task": b, "status": "waiting", "waiting_on": "Nadia" }),
        )
        .unwrap();
        assert_eq!(
            live(&b),
            1,
            "a reopened waiting task has somebody to wait on"
        );
        assert_eq!(
            gtd::get_task(&conn, &b)
                .unwrap()
                .unwrap()
                .waiting_on
                .as_deref(),
            Some("Nadia")
        );
    }

    /// A pre-flight check has to enforce everything the writer does, or it
    /// lets the write begin and then refuses.
    #[test]
    fn the_about_precheck_is_not_weaker_than_the_writer() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("wren-a", "person", "Wren")).unwrap();
        upsert_node(&conn, &Node::new("wren-b", "person", "Wren")).unwrap();
        let before = gtd::list_tasks(&conn, true).unwrap().len();

        // Ambiguous: resolvable, so the old resolve-only guard passed it.
        assert!(
            kg_task_create(&conn, &json!({ "name": "x", "about": ["Wren"] })).is_err(),
            "an ambiguous name must not get past the pre-check"
        );
        assert_eq!(
            gtd::list_tasks(&conn, true).unwrap().len(),
            before,
            "and must leave no task behind for the retry to duplicate"
        );

        // A task target: also resolvable, also refused by the writer.
        let t = kg_task_create(&conn, &json!({ "name": "a real task" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(kg_task_create(&conn, &json!({ "name": "y", "about": ["a real task"] })).is_err());
        assert_eq!(
            gtd::list_tasks(&conn, true).unwrap().len(),
            before + 1,
            "only the one legitimate task exists"
        );
        assert!(gtd::task_about(&conn, &t).unwrap().is_empty());
    }

    /// Re-stating the current holder changes nothing, however many times.
    #[test]
    fn re_setting_the_same_waiting_on_does_not_make_them_their_own_predecessor() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "the thing" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        for _ in 0..3 {
            kg_task_update(&conn, &json!({ "task": id, "waiting_on": "Nadia" })).unwrap();
        }
        let t = gtd::list_tasks(&conn, true).unwrap().remove(0);
        assert_eq!(t.waiting_on.as_deref(), Some("Nadia"));
        assert!(
            t.previously_waiting_on.is_empty(),
            "the current holder is not their own history, and repeats do not stack"
        );
    }

    /// The create echo uses the same association shape as every other
    /// surface, so a caller can tell a vetted link from a guess.
    #[test]
    fn create_echoes_about_in_the_shape_the_tool_descriptions_promise() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();
        let out = kg_task_create(&conn, &json!({ "name": "x", "about": ["Nadia"] })).unwrap();
        assert_eq!(out["about"][0]["name"], "Nadia");
        assert_eq!(
            out["about"][0]["unreviewed"], false,
            "a hand-set association, and the shape says so"
        );
    }

    /// The update response says what the associations now are, so a removal
    /// that removed nothing is visible.
    #[test]
    fn the_update_response_echoes_about() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("p-wren", "person", "Wren")).unwrap();
        let id = kg_task_create(&conn, &json!({ "name": "x", "about": ["Nadia"] })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();

        // Removing something that was never there resolves fine and does
        // nothing — the echo is the only way a caller finds out.
        let out = kg_task_update(&conn, &json!({ "task": id, "about_remove": ["Wren"] })).unwrap();
        let about = out["task"]["about"].as_array().unwrap();
        assert_eq!(about.len(), 1, "Nadia is still there");
        assert_eq!(about[0]["name"], "Nadia");

        let out = kg_task_update(&conn, &json!({ "task": id, "about_remove": ["Nadia"] })).unwrap();
        assert!(
            out["task"]["about"].as_array().unwrap().is_empty(),
            "and a removal that did happen shows as one"
        );
    }

    /// `kg_upsert` cannot write prose into a date column.
    #[test]
    fn valid_from_must_be_a_date() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();
        let bad = kg_upsert(
            &conn,
            &json!({
                "kind": "fact", "subject": "Nadia", "predicate": "works_on",
                "statement": "Nadia works on the pilot",
                "valid_from": "sometime in 2019", "confidence": 0.95
            }),
        );
        assert!(
            bad.is_err(),
            "prose in a date column is refused at the door"
        );

        // A real date, an RFC 3339 instant, and the space-separated form the
        // graph itself writes (`occurred_at`, `now_ts`) all land — an agent
        // copying an episode's own timestamp must not lose the fact.
        for v in ["2019-04-01", "2019-04-01T09:00:00Z", "2019-04-01 09:00:00"] {
            kg_upsert(
                &conn,
                &json!({
                    "kind": "fact", "subject": "Nadia", "predicate": "works_on",
                    "statement": format!("Nadia works on the pilot since {v}"),
                    "valid_from": v, "confidence": 0.5
                }),
            )
            .unwrap();
        }
        // And the repair pass has nothing to do afterwards, which is the
        // property that makes it a repair rather than a treadmill.
        let report = gtd::repair_unparseable_dates(&conn, false).unwrap();
        assert!(report.found.is_empty());
    }

    /// A person's `facts` block is about them, not about their to-do list.
    #[test]
    fn task_associations_do_not_crowd_the_entity_fact_block() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-nadia", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("org-lab", "org", "The Lab")).unwrap();
        mecha_graph_core::fact::assert_fact(
            &conn,
            "p-nadia",
            "member_of",
            Some("org-lab"),
            None,
            "Nadia is a member of The Lab",
            None,
            None,
            1.0,
            "manual",
        )
        .unwrap();
        for i in 0..30 {
            kg_task_create(
                &conn,
                &json!({ "name": format!("task {i}"), "about": ["Nadia"] }),
            )
            .unwrap();
        }

        let waiting = kg_task_create(&conn, &json!({ "name": "owed thing" })).unwrap()["id"]
            .as_str()
            .unwrap()
            .to_string();
        kg_task_update(&conn, &json!({ "task": waiting, "waiting_on": "Nadia" })).unwrap();

        // The SHARED reader, which the context pack, the boot digest, the TUI
        // and `mecha-graph entity` all use. Scoping the fix to one caller left
        // those four showing a block that was entirely her to-do titles.
        let shared = mecha_graph_core::fact::facts_for_node(&conn, "p-nadia", 25).unwrap();
        assert!(
            shared.iter().all(|f| f.predicate != "about"),
            "the fix reaches every caller, not just kg_entity"
        );
        assert!(
            shared.iter().any(|f| f.predicate == "waiting_on"),
            "while waiting_on stays — it was on those surfaces all along"
        );

        let card = kg_entity(&conn, &json!({ "name_or_id": "Nadia" })).unwrap();
        let facts = card["facts"].as_array().unwrap();
        assert!(
            facts.iter().all(|f| f["predicate"] != "about"),
            "30 task associations must not fill a 25-row block"
        );
        assert!(
            facts.iter().any(|f| f["predicate"] == "member_of"),
            "and the fact that says who she is survives"
        );
        // They are not lost — the tasks block is where they belong: the 30
        // `about` tasks plus the one she is waiting on.
        assert_eq!(card["tasks"]["open"]["total"], 31);
    }

    /// The card's task block is capped, and says so.
    ///
    /// The block exists because a silent 25-fact cut hid a person's tasks.
    /// Replacing it with an unbounded list would trade that for an entity
    /// card that grows forever on exactly the nodes worth asking about, so
    /// the cap is real and the truncation is reported rather than inferred.
    #[test]
    fn the_entity_cards_task_block_is_capped_and_says_so() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-wren", "person", "Wren")).unwrap();
        for i in 0..20 {
            kg_task_create(
                &conn,
                &json!({ "name": format!("task {i}"), "about": ["Wren"] }),
            )
            .unwrap();
        }
        let card = kg_entity(&conn, &json!({ "name_or_id": "Wren" })).unwrap();
        let open = &card["tasks"]["open"];
        assert_eq!(open["items"].as_array().unwrap().len(), 15, "capped");
        assert_eq!(open["total"], 20, "and the real count is still reported");
        assert_eq!(open["truncated"], true, "a cut the reader can see");

        // The uncapped route is the one the card points at.
        let full = kg_task_list(&conn, &json!({ "entity": "Wren" })).unwrap();
        assert_eq!(full["items"].as_array().unwrap().len(), 20);
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

/// A list-of-names argument, refused rather than coerced.
///
/// Absent is an empty list; an array of strings is itself; **anything else is
/// an error**, including the bare string that an array was specified for.
/// `as_array()` returning `None` for `"about": "Nadia"` made the commonest
/// shape mistake here answer `created`/`updated` having written nothing —
/// once on each of the two tools, because the guard was written at one call
/// site instead of in one function.
fn name_array(args: &Value, key: &str) -> mecha_graph_core::Result<Vec<String>> {
    match &args[key] {
        Value::Null => Ok(Vec::new()),
        Value::Array(a) => a
            .iter()
            .map(|n| {
                n.as_str().map(str::to_string).ok_or_else(|| {
                    mecha_graph_core::Error::Other(format!(
                        "`{key}` takes an array of names, got {n} inside it"
                    ))
                })
            })
            .collect(),
        other => Err(mecha_graph_core::Error::Other(format!(
            "`{key}` takes an array of names, got {other} — nothing was changed"
        ))),
    }
}

/// One task as the board renders it. Shared by `kg_task_list` and the
/// `tasks` block on `kg_entity`, so the two surfaces cannot disagree about
/// what a task looks like or when one is overdue.
fn task_json(t: &gtd::TaskItem, today: &str) -> Value {
    let overdue = t
        .due_at
        .as_deref()
        .is_some_and(|d| d < today && t.completed_at.is_none());
    json!({
        "id": t.node_id, "name": t.name, "status": t.status,
        "due_at": t.due_at, "defer_until": t.defer_until,
        "context": t.context_tag, "project": t.project,
        "waiting_on": t.waiting_on, "about": t.about,
        // Why a task with no live association is on this entity's card.
        "previously_waiting_on": t.previously_waiting_on,
        // Present only when the extractor's date could not be read, which is
        // why this task has no due date.
        "unreadable_when": t.unreadable_when,
        "session": t.session, "completed_at": t.completed_at,
        "captured_from": t.captured_from,
        "overdue": overdue
    })
}

fn kg_task_list(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let include_closed = args["include_closed"].as_bool().unwrap_or(false);
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    // An unresolvable entity is an ERROR, never an empty board. "No tasks
    // for Nadia" and "there is nobody here called Nadia" are opposite
    // findings, and a caller that cannot tell them apart will report the
    // first when the truth is the second.
    let entity = match args["entity"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        // `gtd::resolve_about`, not `resolve_entity`: the sentinel writes but
        // did not read, so `entity: "@owner"` failed with "no node matches"
        // on a graph that has one.
        Some(name) => Some(gtd::resolve_about(conn, name)?.ok_or_else(|| {
            mecha_graph_core::Error::Other(format!(
                "no node matches '{name}' — kg_entity resolves names, and \
                 an unknown one is not an empty task list"
            ))
        })?),
        None => None,
    };
    let tasks = match &entity {
        Some(node) => gtd::tasks_for_entity(conn, &node.id, include_closed)?,
        None => gtd::list_tasks(conn, include_closed)?,
    };
    let items: Vec<Value> = tasks.iter().map(|t| task_json(t, &today)).collect();
    let mut out = json!({ "v": 1, "items": items, "today": today, "truncated": false });
    if let Some(node) = entity {
        out["entity"] = json!({ "id": node.id, "name": node.name });
    }
    Ok(out)
}

fn kg_task_create(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let name = args["name"].as_str().unwrap_or_default();
    let due = match args["due"].as_str() {
        Some(raw) => gtd::parse_due(raw)?,
        None => None,
    };
    // **Resolve every `about` name before creating anything.** The same rule
    // `set_task_waiting_on` states as "resolve before retiring anything",
    // for the same reason one step earlier: resolving afterwards makes a
    // near-miss name — the common failure, since the caller is guessing at
    // spellings — return an error carrying no task id, for a task that now
    // exists. The caller corrects the name, retries, and there are two.
    // Checking first makes the call all-or-nothing.
    // Shape and names both checked before anything is written — see
    // `name_array`, which is shared with `kg_task_update` so the two tools
    // cannot disagree about what a list of names is.
    let about_names = name_array(args, "about")?;
    for name in &about_names {
        // The FULL rule the writer applies, not a subset of it — see
        // `validate_about_target`. A guard weaker than the thing it guards
        // lets the create run and then refuses, which is the half-write this
        // pre-check exists to prevent.
        gtd::validate_about_target(conn, name)
            .map_err(|e| mecha_graph_core::Error::Other(format!("{e} — no task was created")))?;
    }
    let task_id = gtd::create_task(
        conn,
        name,
        due.as_deref(),
        args["project"].as_str(),
        args["context"].as_str(),
    )?;
    // A second write rather than a sixth positional argument, on the
    // `set_task_session` shape: the property has its own validating setter,
    // and `create_task` has a TUI caller that has nothing to say about
    // provenance. A refused pointer fails the *call*, so the caller learns
    // its pointer was junk rather than getting a task with the provenance
    // quietly missing — which is the absence this whole field exists to fix.
    if !args["captured_from"].is_null() {
        gtd::set_task_captured_from(conn, &task_id, Some(&args["captured_from"]))?;
    }
    // Every name here already resolved above, so these cannot fail on a
    // lookup and the task cannot be left half-associated.
    for name in &about_names {
        gtd::add_task_about(conn, &task_id, name)?;
    }
    // Echoed in the SAME shape every other surface uses — `{name, unreviewed}`,
    // read back from the store rather than from the names that went in. Bare
    // strings here made this the one response where a caller could not tell a
    // vetted association from a guess, contradicting the tool descriptions
    // this PR wrote; and reading it back means the echo reflects what was
    // actually recorded, including a pre-existing shadow row upgraded to
    // reviewed by this very call.
    let about = gtd::get_task(conn, &task_id)?
        .map(|t| t.about)
        .unwrap_or_default();
    Ok(json!({
        "v": 1, "status": "created", "id": task_id, "due_at": due, "about": about
    }))
}

fn kg_task_update(conn: &Connection, args: &Value) -> mecha_graph_core::Result<Value> {
    let task = args["task"]
        .as_str()
        .ok_or_else(|| mecha_graph_core::Error::Other("kg_task_update needs `task`".into()))?;

    // **Validate the `about` names before the first write, not beside their
    // own.** This function applies its fields in sequence, so a name checked
    // where it is used would refuse the association *after* the status change
    // and the reschedule had already landed — and the error would say nothing
    // about which of them stuck. Checked here, "nothing was changed" is true.
    //
    // A non-string entry is refused rather than skipped: dropping it silently
    // reports success for a name nobody applied.
    let names = |key: &str| -> mecha_graph_core::Result<Vec<String>> { name_array(args, key) };
    let to_add = names("about_add")?;
    let to_remove = names("about_remove")?;
    for name in to_add.iter().chain(to_remove.iter()) {
        gtd::validate_about_target(conn, name)
            .map_err(|e| mecha_graph_core::Error::Other(format!("{e} — nothing was changed")))?;
    }

    // **Status goes FIRST, so every field after it sees the status the
    // caller is actually setting.**
    //
    // It briefly went last, to stop `{status: "done", waiting_on: "Nadia"}`
    // asserting a live obligation onto a finished task. That was the right
    // bug and the wrong layer: the guard now lives on `set_task_waiting_on`,
    // which closes the claim itself when the task is done, so no call shape
    // can route around it — and the reorder became not merely redundant but
    // harmful. `set_task_waiting_on` reads the status from the row, so with
    // status applied last it read the PRE-call value: `{status: "waiting",
    // waiting_on: "Nadia"}` on a done task asserted the claim, immediately
    // closed it because the row still said done, and only then reopened the
    // task — an open `waiting` task that nobody owes. Two ordering fixes for
    // the same field cancelled each other; the invariant on the writer is
    // what actually holds, and this order is what lets it see the truth.
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

    // After the schedule, because both can arrive in one call and a caller
    // moving a task to `waiting` almost always names who in the same breath.
    if let Some(who) = args["waiting_on"].as_str() {
        gtd::set_task_waiting_on(conn, task, who)?;
    }
    if let Some(session) = args["session"].as_str() {
        gtd::set_task_session(conn, task, session)?;
    }
    // Add and remove rather than set, because `about` is multi-valued: a
    // `set` would make "also file this under Nadia" silently drop whoever
    // was already there.
    //
    // Both lists were resolved at the top of this function, so neither loop
    // can fail on a lookup and leave the edit half applied.
    for name in &to_add {
        gtd::add_task_about(conn, task, name)?;
    }
    for name in &to_remove {
        gtd::remove_task_about(conn, task, name)?;
    }
    // An object sets it; `""` clears it, which is the tri-state every other
    // field here speaks. `null` cannot mean "clear" — an absent key
    // deserialises to exactly that, so the two would be the same call and
    // omitting the field would wipe the pointer.
    match &args["captured_from"] {
        Value::Null => {}
        Value::String(s) if s.trim().is_empty() => {
            gtd::set_task_captured_from(conn, task, None)?;
        }
        value => gtd::set_task_captured_from(conn, task, Some(value))?,
    }

    // `task_json`, not a second literal. The reason this response echoes
    // `waiting_on` at all — a caller cannot otherwise tell a successful set
    // from a silently ignored one, because the field is a fact rather than a
    // column — is exactly as true of `about`, and the hand-written copy
    // omitted it. `about_remove` on a task that was never filed there updates
    // nothing and reports `updated`, so the echo is the only way a caller
    // learns the unfiling did not happen. One renderer means the two
    // responses cannot drift again.
    let today = chrono::Utc::now()
        .date_naive()
        .format("%Y-%m-%d")
        .to_string();
    let updated = gtd::get_task(conn, task)?.map(|t| task_json(&t, &today));
    Ok(json!({ "v": 1, "status": "updated", "task": updated }))
}
