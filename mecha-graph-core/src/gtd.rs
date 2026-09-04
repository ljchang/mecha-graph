//! Weekly review as a query, not a ritual (§8.4), plus boot-injection
//! MEMORY.md generation (§8.3).

use crate::error::{Error, Result};
use rusqlite::{params, Connection};
use serde::Serialize;

/// Task lifecycle (task_detail.status). Order is the display/cycle order:
/// actionable first, terminal last.
pub const TASK_STATUSES: &[&str] = &["next", "inbox", "scheduled", "waiting", "done", "dropped"];

#[derive(Debug, Clone, Serialize)]
pub struct TaskItem {
    pub node_id: String,
    pub name: String,
    pub status: String,
    pub task_type: String,
    pub due_at: Option<String>,
    pub defer_until: Option<String>,
    pub context_tag: Option<String>,
    pub completed_at: Option<String>,
    /// Parent project name (task_detail.parent_id), if any.
    pub project: Option<String>,
    /// Who this waits on (live `waiting_on` fact), if anyone.
    pub waiting_on: Option<String>,
    /// The agent conversation that worked this task, if one has.
    ///
    /// An **attribute, not an edge**, and the distinction was argued rather
    /// than defaulted to. The obvious alternative is a fact pointing at the
    /// session's episode — but an episode is evidence of what happened, so it
    /// exists only after the run *and* only if the distiller judged the run
    /// worth remembering, which it deliberately does not for "smoke tests,
    /// one-line lookups, greetings, aborted or purely mechanical runs". Those
    /// are exactly the runs a person most wants to click back into, so an
    /// edge-based link would be missing precisely where it is needed.
    ///
    /// It is also the more general of the two: the episode's idempotence key
    /// *is* the session id, so holding this finds the episode too, whenever
    /// one appears. The edge remains available later as an addition — it
    /// answers a different question (traversal and provenance) and so cannot
    /// disagree with this one.
    pub session: Option<String>,
    /// The entities this task is *about* — a permanent association, and what
    /// makes a *completed* task still findable under the person it concerned.
    ///
    /// Deliberately a different predicate from [`TaskItem::waiting_on`],
    /// because the two have different lifetimes and one field cannot carry
    /// both. `waiting_on` is a live claim about who holds the ball *now*, so
    /// closing the task closes the fact — it stopped being true in the world.
    /// `about` is what the task concerned, which finishing it does not
    /// change. Collapsing them into one predicate forces a choice between a
    /// person's card carrying stale "waiting on" claims forever and their
    /// finished work becoming unfindable. Keeping them apart is what lets
    /// both be right.
    pub about: Vec<String>,
    /// What the task was captured *from* — the email that asked, the
    /// stranger's request, the conversation it fell out of. See
    /// [`set_task_captured_from`] for the shape and why it is a pointer.
    ///
    /// Absent when there is none, and that is the honest answer rather than a
    /// gap: a task typed into the board was captured *here*, so a
    /// `{"kind": "manual"}` placeholder would be a link that goes nowhere. A
    /// surface shows the way back only when there is one. Same rule as
    /// [`TaskItem::session`], one field over.
    pub captured_from: Option<serde_json::Value>,
}

/// The predicates that associate a task with an entity, for the purposes of
/// "show me this person's tasks".
///
/// All three are real edges (`object_id` set), so all three traverse. They
/// differ in what they claim, and the entity filter deliberately unions them
/// rather than making a caller pick: someone asking "what is open and closed
/// for Grace" does not mean "…but only the ones where she holds the ball".
pub const TASK_ENTITY_PREDICATES: &[&str] = &["about", "waiting_on", "assigned_to"];

/// Names inside a `group_concat` are joined on ASCII unit separator rather
/// than a comma, because a node name may contain a comma and splitting on
/// one would quietly saw a person in half.
const NAME_SEP: char = '\u{1f}';

/// All tasks, actionable statuses first then by due date. `include_closed`
/// adds done/dropped (newest completions first within their group).
pub fn list_tasks(conn: &Connection, include_closed: bool) -> Result<Vec<TaskItem>> {
    list_tasks_filtered(conn, include_closed, None)
}

/// Every task associated with one entity, open and closed, newest completion
/// first within each status group. `entity` is a node **id** — resolve a name
/// through [`crate::graph::resolve_entity`] first, so an unknown name is an
/// error the caller can report rather than an empty board that reads like
/// "this person has no tasks".
pub fn tasks_for_entity(
    conn: &Connection,
    entity_id: &str,
    include_closed: bool,
) -> Result<Vec<TaskItem>> {
    list_tasks_filtered(conn, include_closed, Some(entity_id))
}

/// The one task query, with the entity filter as a parameter rather than a
/// second SQL string.
///
/// Written as `?2 IS NULL OR …` so the filtered and unfiltered boards are
/// *literally the same statement*. Two queries would drift: the board grew a
/// `defer_until` column and an ordering rule the hard way, and a parallel
/// entity query would have had to grow them again, from memory, correctly.
fn list_tasks_filtered(
    conn: &Connection,
    include_closed: bool,
    entity_id: Option<&str>,
) -> Result<Vec<TaskItem>> {
    // The predicate list is interpolated rather than bound because SQLite has
    // no array parameter. It is a private `const` of string literals, never
    // caller input, so there is nothing here for a caller to inject.
    let predicates = TASK_ENTITY_PREDICATES
        .iter()
        .map(|p| format!("'{p}'"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT n.id, n.name, td.status, td.task_type, td.due_at, td.defer_until,
                td.context_tag, td.completed_at,
                json_extract(n.properties, '$.session'),
                json_extract(n.properties, '$.captured_from'),
                (SELECT p.name FROM nodes p WHERE p.id = td.parent_id),
                (SELECT pn.name FROM fact_current f JOIN nodes pn ON pn.id = f.object_id
                 WHERE f.subject_id = n.id AND f.predicate = 'waiting_on' LIMIT 1),
                (SELECT group_concat(an.name, char(31)) FROM fact_current af
                 JOIN nodes an ON an.id = af.object_id
                 WHERE af.subject_id = n.id AND af.predicate = 'about')
         FROM nodes n JOIN task_detail td ON td.node_id = n.id
         WHERE (?1 OR td.status NOT IN ('done','dropped'))
           AND (?2 IS NULL
                OR td.parent_id = ?2
                OR EXISTS (SELECT 1 FROM fact_current ef
                           WHERE ef.subject_id = n.id AND ef.object_id = ?2
                             AND ef.predicate IN ({predicates})))
         ORDER BY CASE td.status WHEN 'next' THEN 0 WHEN 'inbox' THEN 1
                                 WHEN 'scheduled' THEN 2 WHEN 'waiting' THEN 3
                                 WHEN 'done' THEN 4 ELSE 5 END,
                  td.due_at IS NULL, td.due_at ASC,
                  td.completed_at DESC, n.created_at ASC"
    );
    let mut stmt = conn.prepare_cached(&sql)?;
    let tasks = stmt
        .query_map(params![include_closed, entity_id], |r| {
            Ok(TaskItem {
                node_id: r.get(0)?,
                name: r.get(1)?,
                status: r.get(2)?,
                task_type: r.get(3)?,
                due_at: r.get(4)?,
                defer_until: r.get(5)?,
                context_tag: r.get(6)?,
                completed_at: r.get(7)?,
                session: r.get(8)?,
                // `json_extract` hands back an object as a TEXT of JSON, so
                // it is re-parsed here rather than passed on as a string —
                // a caller handed `"{\"kind\":\"mail\"}"` would have to
                // parse it itself, and one of them would forget.
                captured_from: r
                    .get::<_, Option<String>>(9)?
                    .and_then(|raw| serde_json::from_str(&raw).ok()),
                project: r.get(10)?,
                waiting_on: r.get(11)?,
                about: r
                    .get::<_, Option<String>>(12)?
                    .map(|joined| joined.split(NAME_SEP).map(str::to_string).collect())
                    .unwrap_or_default(),
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(tasks)
}

/// One date column holding something that is not a date.
#[derive(Debug, Clone, Serialize)]
pub struct BadDate {
    /// `task_detail.due_at`, `task_detail.defer_until`, or `fact.valid_from`.
    pub column: String,
    /// The task node id, or the fact uid.
    pub row: String,
    /// What is actually stored — the point of the report.
    pub value: String,
    /// The task name or fact statement, so a reader can tell what it is.
    pub label: String,
}

#[derive(Debug, Default, Serialize)]
pub struct DateRepairReport {
    pub found: Vec<BadDate>,
    /// Rows actually nulled. Zero on a dry run, however many were found.
    pub repaired: usize,
}

/// Find (and optionally clear) date columns holding text that is not a date.
///
/// The damage `accept_commitment` did before it learned to parse: a model's
/// `when` went verbatim into `task_detail.due_at` and into the `valid_from`
/// of the two facts the acceptance asserts. A literal `"null"` there sorts
/// as a date, never reads as overdue, and quietly answers the wrong side of
/// every `--as-of` query.
///
/// **Reporting is the default and `apply` is opt-in**, the same way
/// `relink_alias_mentions` is: this nulls columns, and a pass that can erase
/// dates should not be able to do it to somebody who ran it expecting a
/// survey.
///
/// A date here is anything starting `YYYY-` — every writer in this codebase
/// produces either `YYYY-MM-DD` or SQLite's `datetime('now')`, so the prefix
/// separates real values from prose without having to parse the tail. NULL is
/// not malformed; it is the honest "no date", and is left alone.
pub fn repair_unparseable_dates(conn: &Connection, apply: bool) -> Result<DateRepairReport> {
    let mut report = DateRepairReport::default();
    // GLOB rather than LIKE: '[0-9]' is a character class in GLOB, and
    // matching four literal digits is the whole test.
    const IS_DATEISH: &str = "GLOB '[0-9][0-9][0-9][0-9]-*'";

    for column in ["due_at", "defer_until"] {
        let sql = format!(
            "SELECT td.node_id, td.{column}, n.name FROM task_detail td
             JOIN nodes n ON n.id = td.node_id
             WHERE td.{column} IS NOT NULL AND NOT (td.{column} {IS_DATEISH})"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows: Vec<BadDate> = stmt
            .query_map([], |r| {
                Ok(BadDate {
                    column: format!("task_detail.{column}"),
                    row: r.get(0)?,
                    value: r.get(1)?,
                    label: r.get(2)?,
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        drop(stmt);
        if apply && !rows.is_empty() {
            report.repaired += conn.execute(
                &format!(
                    "UPDATE task_detail SET {column} = NULL
                      WHERE {column} IS NOT NULL AND NOT ({column} {IS_DATEISH})"
                ),
                [],
            )?;
        }
        report.found.extend(rows);
    }

    let mut stmt = conn.prepare(&format!(
        "SELECT uid, valid_from, statement FROM fact
          WHERE valid_from IS NOT NULL AND NOT (valid_from {IS_DATEISH})"
    ))?;
    let facts: Vec<BadDate> = stmt
        .query_map([], |r| {
            Ok(BadDate {
                column: "fact.valid_from".into(),
                row: r.get(0)?,
                value: r.get(1)?,
                label: r.get(2)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    drop(stmt);
    if apply && !facts.is_empty() {
        report.repaired += conn.execute(
            &format!(
                "UPDATE fact SET valid_from = NULL
                  WHERE valid_from IS NOT NULL AND NOT (valid_from {IS_DATEISH})"
            ),
            [],
        )?;
    }
    report.found.extend(facts);
    Ok(report)
}

/// Parse a human due-date: `YYYY-MM-DD`, `today`, `tomorrow`, or `+Nd`.
/// Returns None for empty input; Err for anything unparseable (better to
/// bounce the form than silently store garbage in task_detail.due_at).
pub fn parse_due(input: &str) -> Result<Option<String>> {
    let s = input.trim().to_lowercase();
    if s.is_empty() {
        return Ok(None);
    }
    let today = chrono::Utc::now().date_naive();
    let date = if s == "today" {
        today
    } else if s == "tomorrow" {
        today + chrono::Days::new(1)
    } else if let Some(days) = s.strip_prefix('+').and_then(|r| r.strip_suffix('d')) {
        let n: u64 = days
            .parse()
            .map_err(|_| Error::Other(format!("bad relative date '{input}' — use +Nd")))?;
        today + chrono::Days::new(n)
    } else {
        chrono::NaiveDate::parse_from_str(&s, "%Y-%m-%d").map_err(|_| {
            Error::Other(format!(
                "bad date '{input}' — YYYY-MM-DD, today, tomorrow, or +Nd"
            ))
        })?
    };
    Ok(Some(date.format("%Y-%m-%d").to_string()))
}

/// Create a task by hand (TUI `a` / manual capture). `project` resolves
/// against the graph (project/topic node); unknown names are an error rather
/// than an implicit node — typo protection.
pub fn create_task(
    conn: &Connection,
    name: &str,
    due: Option<&str>,
    project_name: Option<&str>,
    context_tag: Option<&str>,
) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Other("task needs a name".into()));
    }
    let parent_id = match project_name.map(str::trim).filter(|s| !s.is_empty()) {
        Some(p) => match crate::graph::resolve_entity(conn, p)? {
            Some(node) => Some(node.id),
            None => return Err(Error::Other(format!("no node matches project '{p}'"))),
        },
        None => None,
    };
    let task_id = format!("task-{}", &crate::ids::new_uid()[..8]);
    let mut task = crate::graph::Node::new(&task_id, "task", name);
    task.source = "manual".into();
    crate::graph::upsert_node(conn, &task)?;
    conn.execute(
        "INSERT INTO task_detail (node_id, status, task_type, due_at, context_tag, parent_id)
         VALUES (?1, 'inbox', 'action', ?2, ?3, ?4)",
        params![
            task_id,
            due,
            context_tag.filter(|s| !s.trim().is_empty()),
            parent_id
        ],
    )?;
    Ok(task_id)
}

/// Edit scheduling fields on an existing task (TUI `e`). `Some("")` clears a
/// field; `None` leaves it untouched.
pub fn update_task_schedule(
    conn: &Connection,
    node_id: &str,
    due: Option<Option<&str>>,
    defer: Option<Option<&str>>,
    context_tag: Option<Option<&str>>,
) -> Result<()> {
    let n = conn.execute(
        "UPDATE task_detail SET
            due_at      = CASE WHEN ?2 THEN ?3 ELSE due_at END,
            defer_until = CASE WHEN ?4 THEN ?5 ELSE defer_until END,
            context_tag = CASE WHEN ?6 THEN ?7 ELSE context_tag END
         WHERE node_id = ?1",
        params![
            node_id,
            due.is_some(),
            due.flatten(),
            defer.is_some(),
            defer.flatten(),
            context_tag.is_some(),
            context_tag.flatten(),
        ],
    )?;
    if n == 0 {
        return Err(Error::Other(format!("{node_id} is not a task")));
    }
    Ok(())
}

/// Record which agent conversation worked a task, or clear it with `""`.
///
/// Written by the harness, never derived: mecha knows its own session id at
/// run start and this is where it puts it, so "open the conversation" is one
/// read rather than a traversal through a node that may not exist. See
/// [`TaskItem::session`] for why this is an attribute and not an edge.
pub fn set_task_session(conn: &Connection, node_id: &str, session: &str) -> Result<()> {
    let mut node = match crate::graph::get_node(conn, node_id)? {
        Some(n) => n,
        None => return Err(Error::Other(format!("{node_id} is not a node"))),
    };
    if node.node_type != "task" {
        return Err(Error::Other(format!("{node_id} is not a task")));
    }
    let session = session.trim();
    if session.is_empty() {
        if let serde_json::Value::Object(ref mut map) = node.properties {
            map.remove("session");
        }
    } else {
        node.set_property("session", serde_json::json!(session));
    }
    crate::graph::upsert_node(conn, &node)?;
    Ok(())
}

/// The kinds of thing a task can be captured from, and the whole set of them.
///
/// **Closed on purpose**, the same rule that makes the mail surface's triage
/// actions an enum: a free-form kind is one a surface cannot open, so the
/// board would grow rows offering a way back that dead-ends. Adding a kind is
/// a line here *plus* a reader that can follow it, and the two have to arrive
/// together — which is why `slack` is absent despite being an obvious source.
/// Nothing on this side can render a Slack thread, so accepting the kind would
/// buy a row with a button that opens nothing, which is worse than the plain
/// absence this whole field exists to fix.
pub const CAPTURE_KINDS: &[&str] = &["mail", "frontdoor", "session"];

/// The keys a capture pointer may carry. Everything else is refused.
const CAPTURE_KEYS: &[&str] = &["kind", "id", "account", "label", "at"];

/// A label is a subject line somebody else wrote. Capped at the door rather
/// than refused, on the image rule: the caller with a pathological one still
/// has a real task to capture, and losing it over a long subject is the worse
/// failure. Whole graphemes are not worth the dependency here — this is a
/// recognisable handle in a list, not a document.
const LABEL_MAX: usize = 200;

/// Record what a task was captured from, or clear it with `None`.
///
/// The shape is a small typed pointer and deliberately nothing more:
///
/// ```json
/// {"kind": "mail", "account": "ostrander", "id": "<thread_id>",
///  "label": "SAS 2027 award nominations", "at": "2026-08-11T14:02:00Z"}
/// ```
///
/// **A pointer, never a copy, and the key list is what enforces it.** The
/// obvious convenience is to store the email's body alongside — one read, no
/// provider round-trip, works offline. It is the wrong trade twice over: the
/// graph would become a store of other people's words that everything reading
/// it treats as belief, and the copy would go stale against the thread it
/// names, so a person clicking through to "the original" would be shown
/// something the original no longer says. Refusing unknown keys makes that a
/// property of the store instead of a convention somebody remembers — a
/// caller that tries to put a `body` here is told no.
///
/// `label` is **prose somebody else chose** and is stored as a handle for a
/// human reading a list, exactly as the mail triage record's `subject` is. It
/// is not evidence about anything and must not be reasoned about.
///
/// Written by whatever captured the task, never derived — the harness that
/// read the mail knows the thread id at the moment it creates the task, and
/// reconstructing it afterwards by matching subject lines is a guess. Same
/// argument as [`set_task_session`], one field over.
pub fn set_task_captured_from(
    conn: &Connection,
    node_id: &str,
    captured_from: Option<&serde_json::Value>,
) -> Result<()> {
    let mut node = match crate::graph::get_node(conn, node_id)? {
        Some(n) => n,
        None => return Err(Error::Other(format!("{node_id} is not a node"))),
    };
    if node.node_type != "task" {
        return Err(Error::Other(format!("{node_id} is not a task")));
    }
    match captured_from {
        None => {
            if let serde_json::Value::Object(ref mut map) = node.properties {
                map.remove("captured_from");
            }
        }
        Some(value) => {
            node.set_property("captured_from", validate_captured_from(value)?);
        }
    }
    crate::graph::upsert_node(conn, &node)?;
    Ok(())
}

/// Bounce a malformed pointer rather than storing it — `parse_due`'s rule,
/// and it matters more here: a stored pointer nothing can follow looks
/// exactly like provenance right up until somebody clicks it.
fn validate_captured_from(value: &serde_json::Value) -> Result<serde_json::Value> {
    let object = value
        .as_object()
        .ok_or_else(|| Error::Other("captured_from must be an object".into()))?;
    if let Some(key) = object.keys().find(|k| !CAPTURE_KEYS.contains(&k.as_str())) {
        return Err(Error::Other(format!(
            "captured_from has no '{key}' field — it holds a pointer ({}), never a copy of \
             what it points at",
            CAPTURE_KEYS.join(", ")
        )));
    }
    let mut out = serde_json::Map::new();
    for key in CAPTURE_KEYS {
        let Some(raw) = object.get(*key) else {
            continue;
        };
        let text = raw
            .as_str()
            .ok_or_else(|| Error::Other(format!("captured_from.{key} must be a string")))?
            .trim();
        if text.is_empty() {
            continue;
        }
        let text = if *key == "label" && text.chars().count() > LABEL_MAX {
            let cut: String = text.chars().take(LABEL_MAX - 1).collect();
            format!("{cut}…")
        } else {
            text.to_string()
        };
        out.insert((*key).to_string(), serde_json::json!(text));
    }
    let kind = out
        .get("kind")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| Error::Other("captured_from needs a `kind`".into()))?;
    if !CAPTURE_KINDS.contains(&kind) {
        return Err(Error::Other(format!(
            "captured_from.kind '{kind}' is not one of: {}",
            CAPTURE_KINDS.join(", ")
        )));
    }
    // Without an id there is nothing to open, and a kind alone would render a
    // "read the original" affordance over no original at all.
    if !out.contains_key("id") {
        return Err(Error::Other(format!(
            "captured_from needs an `id` — which {kind} it came from"
        )));
    }
    // Thread ids are account-scoped, so a mail pointer without its account
    // names a thread in whichever mailbox the reader happens to ask first.
    if kind == "mail" && !out.contains_key("account") {
        return Err(Error::Other(
            "captured_from.kind 'mail' needs an `account` — thread ids are account-scoped".into(),
        ));
    }
    Ok(serde_json::Value::Object(out))
}

/// The stand-in for whoever this graph is about, usable wherever a
/// `waiting_on` name is taken.
///
/// A literal rather than a lookup at the call site, because the callers that
/// need it are harnesses handing work back to a person, and a person's name is
/// exactly the thing a harness should not be carrying around.
pub const OWNER: &str = "@owner";

/// Point a task's `waiting_on` at a node, or clear it with `""`.
///
/// `@owner` ([`OWNER`]) resolves to whoever the graph is about.
///
/// **The delegation half of the board.** OmniFocus's insight is that
/// delegation is a status which suppresses an item from "what can I do now"
/// while keeping it reviewable, and `waiting` + `waiting_on` already is that —
/// this gives it an object the harness can set, so a task handed to the agent
/// is visibly held by the agent rather than merely "waiting" for reasons the
/// board cannot state.
///
/// Two rules carried from elsewhere in this file. The name must **resolve to
/// a node that already exists**, exactly as `create_task` requires of a
/// project: an implicit node on a typo is how a graph fills with junk, and
/// here it would also mean "waiting on Nadai" silently becoming a real
/// person. And the previous belief is **invalidated, never deleted** — one
/// live fact per (subject, predicate, object) is the schema's rule, and who
/// used to have the ball is exactly the history a bi-temporal store is for.
///
/// Returns the resolved name, or `None` when the field was cleared.
pub fn set_task_waiting_on(conn: &Connection, node_id: &str, who: &str) -> Result<Option<String>> {
    require_task(conn, node_id)?;

    // **Resolve before retiring anything.** Retiring first and resolving after
    // means a typo does not merely fail — it *clears* who actually had the
    // ball, so `waiting_on: "Nadai"` silently turns "Nadia owes me this" into
    // "nobody owes me this" and the error message says nothing about what was
    // lost. Found by the test written for the typo protection, which is a
    // fair description of how that protection was incomplete.
    let who = who.trim();
    let target = if who.is_empty() {
        None
    } else if who == OWNER {
        // **The one name a caller cannot be expected to know.** A harness
        // handing a task back says "this is yours now", and making it look up
        // the owner's actual name first would mean shipping that name into
        // config on every machine — and getting it wrong the day it changes.
        // The graph already records who it is about (`owner_node`, an explicit
        // mark rather than a heuristic), so this asks it.
        match crate::graph::owner_node(conn)? {
            Some(n) => Some(n),
            None => {
                return Err(Error::Other(
                    "this graph has no owner set, so `@owner` names nobody — \
                     `mecha-graph owner <node>` marks one"
                        .into(),
                ))
            }
        }
    } else {
        match crate::graph::resolve_entity(conn, who)? {
            Some(n) => Some(n),
            None => {
                return Err(Error::Other(format!(
                    "no node matches '{who}' — waiting_on must name someone the graph already knows"
                )))
            }
        }
    };

    // Now that the answer is known, retire the old belief. Clearing and
    // re-pointing are the same operation from here, so neither can leave two
    // live claims about who owes this.
    conn.execute(
        "UPDATE fact SET invalidated_at = datetime('now')
          WHERE subject_id = ?1 AND predicate = 'waiting_on'
            AND valid_to IS NULL AND invalidated_at IS NULL",
        params![node_id],
    )?;

    let Some(target) = target else {
        return Ok(None);
    };
    let task_name: String = conn.query_row(
        "SELECT name FROM nodes WHERE id = ?1",
        params![node_id],
        |r| r.get(0),
    )?;
    crate::fact::assert_fact(
        conn,
        node_id,
        "waiting_on",
        Some(&target.id),
        None,
        &format!("{task_name} is waiting on {}", target.name),
        None,
        None,
        1.0,
        "manual",
    )?;
    Ok(Some(target.name))
}

/// Associate a task with an entity it concerns, as a live `about` fact.
///
/// Multi-valued, unlike [`set_task_waiting_on`]: a task can be about several
/// people and topics at once, and there is no single answer to retire when a
/// second one arrives. So this *adds*; [`remove_task_about`] is the inverse.
///
/// Same resolution rule as `waiting_on`, for the same reason: the name must
/// already be a node. Minting one here would let a typo create a plausible
/// looking person nobody ever meant, and a task filed under them is worse
/// than a task filed under nobody — it reads as an answer.
///
/// Idempotent. The live-fact unique index already forbids a duplicate
/// (subject, predicate, object), so re-adding the same association returns
/// the existing name rather than failing; a caller re-running a scan should
/// not have to care.
pub fn add_task_about(conn: &Connection, node_id: &str, what: &str) -> Result<String> {
    require_task(conn, node_id)?;
    let what = what.trim();
    if what.is_empty() {
        return Err(Error::Other("about needs a name".into()));
    }
    let target = match what {
        OWNER => crate::graph::owner_node(conn)?.ok_or_else(|| {
            Error::Other(
                "this graph has no owner set, so `@owner` names nobody — \
                 `mecha-graph owner <node>` marks one"
                    .into(),
            )
        })?,
        _ => crate::graph::resolve_entity(conn, what)?.ok_or_else(|| {
            Error::Other(format!(
                "no node matches '{what}' — about must name something the graph already knows"
            ))
        })?,
    };
    let task_name: String = conn.query_row(
        "SELECT name FROM nodes WHERE id = ?1",
        params![node_id],
        |r| r.get(0),
    )?;
    crate::fact::assert_fact(
        conn,
        node_id,
        "about",
        Some(&target.id),
        None,
        &format!("{task_name} is about {}", target.name),
        None,
        None,
        1.0,
        "manual",
    )?;
    Ok(target.name)
}

/// Drop a task's association with an entity. Returns whether one was there.
///
/// A **valid-time close**, not an invalidation: "this task is no longer
/// filed under Grace" says the association ended, not that it was never
/// true, and the history behind it stays readable. `invalidated_at` is for
/// beliefs that were wrong — see `fact::invalidate_never_true`.
pub fn remove_task_about(conn: &Connection, node_id: &str, what: &str) -> Result<bool> {
    require_task(conn, node_id)?;
    let Some(target) = crate::graph::resolve_entity(conn, what.trim())? else {
        return Ok(false);
    };
    let n = conn.execute(
        "UPDATE fact SET valid_to = datetime('now')
          WHERE subject_id = ?1 AND predicate = 'about' AND object_id = ?2
            AND valid_to IS NULL AND invalidated_at IS NULL",
        params![node_id, target.id],
    )?;
    Ok(n > 0)
}

/// What one pass of [`propose_task_entities`] did.
#[derive(Debug, Default, Serialize)]
pub struct TaskScanReport {
    /// Tasks examined — those with no `about` association yet.
    pub scanned: usize,
    /// Associations proposed and minted live at tier `shadow`.
    pub minted: usize,
    /// Strong matches already asserted; the scan is re-runnable.
    pub already: usize,
    /// Weak matches (a bare first name) refused for want of corroboration.
    pub refused_weak: usize,
}

/// Scan task titles for entities the graph already knows, and mint the
/// matches as **shadow** `about` facts (review-on-use).
///
/// Why shadow and not direct: a task the owner typed is an instruction, but
/// "this title contains a word that is also a person's name" is an
/// inference, and inference does not get to land as settled fact. Shadow is
/// the tier that already means exactly this — live and retrievable, labeled
/// unreviewed, rank-discounted, earning a human verdict the first time a
/// query actually serves it. So the board is useful today without anything
/// pretending to be more certain than it is.
///
/// **Strong matches only.** `alias_verdict` lets a weak match (a bare first
/// name) through when other strong matches in the same episode corroborate
/// it. A task title is one line — there is no corroborating context to be
/// had, and the 2026-08-24 repair is what happens when a bare first name is
/// trusted without it. Here a weak match is always refused, and counted so
/// the refusal is visible rather than silent.
///
/// Tasks are excluded as *targets*: their names live in `nodes` like any
/// other, so without this every task links to every task sharing a word.
pub fn propose_task_entities(conn: &Connection) -> Result<TaskScanReport> {
    let mut report = TaskScanReport::default();
    let pairs = crate::episode::alias_pairs(conn)?;

    // Tasks with no association yet — re-running must not re-propose what a
    // previous pass already minted or a human already asserted.
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name FROM nodes n JOIN task_detail td ON td.node_id = n.id
          WHERE NOT EXISTS (SELECT 1 FROM fact_current f
                            WHERE f.subject_id = n.id AND f.predicate = 'about')",
    )?;
    let tasks: Vec<(String, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    drop(stmt);

    for (task_id, task_name) in tasks {
        report.scanned += 1;
        let lower = task_name.to_lowercase();
        for (name, node_id, weak) in &pairs {
            if node_id == &task_id || !crate::episode::appears_in(&lower, name) {
                continue;
            }
            if *weak {
                report.refused_weak += 1;
                continue;
            }
            let node_type: String = conn.query_row(
                "SELECT node_type FROM nodes WHERE id = ?1",
                params![node_id],
                |r| r.get(0),
            )?;
            if node_type == "task" {
                continue;
            }
            let target_name: String = conn.query_row(
                "SELECT name FROM nodes WHERE id = ?1",
                params![node_id],
                |r| r.get(0),
            )?;
            // Already live from an earlier pass or a human? Nothing to do.
            let live: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM fact_current
                  WHERE subject_id = ?1 AND predicate = 'about' AND object_id = ?2",
                params![task_id, node_id],
                |r| r.get(0),
            )?;
            if live {
                report.already += 1;
                continue;
            }
            let proposed = crate::fact::ProposedFact {
                subject: task_name.clone(),
                predicate: "about".into(),
                object: Some(target_name.clone()),
                object_value: None,
                statement: format!("{task_name} is about {target_name}"),
                valid_from: None,
                confidence: Some(0.6),
                tags: None,
                // Set, because this producer derived the pair FROM nodes —
                // it never guessed at a name. That is what keeps two
                // genuinely distinct same-named people from folding into
                // one another at accept time.
                subject_node: Some(task_id.clone()),
                object_node: Some(node_id.clone()),
            };
            let cid = crate::fact::propose_fact(conn, &proposed, "linker:task-title", None)?;
            crate::fact::mint_shadow_candidate(conn, cid)?;
            report.minted += 1;
        }
    }
    Ok(report)
}

/// The entities a task is currently about, as `(node_id, name)`.
pub fn task_about(conn: &Connection, node_id: &str) -> Result<Vec<(String, String)>> {
    let mut stmt = conn.prepare_cached(
        "SELECT an.id, an.name FROM fact_current af
         JOIN nodes an ON an.id = af.object_id
         WHERE af.subject_id = ?1 AND af.predicate = 'about'
         ORDER BY an.name",
    )?;
    let rows = stmt
        .query_map(params![node_id], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Guard shared by every task mutator: the id must actually be a task.
///
/// Factored out of [`set_task_waiting_on`], which had it inline. Two copies
/// of "is this a task" is one copy away from a mutator that forgets to ask
/// and writes a `waiting_on` onto a person node.
fn require_task(conn: &Connection, node_id: &str) -> Result<()> {
    let is_task: bool = conn.query_row(
        "SELECT COUNT(*) > 0 FROM task_detail WHERE node_id = ?1",
        params![node_id],
        |r| r.get(0),
    )?;
    if !is_task {
        return Err(Error::Other(format!("{node_id} is not a task")));
    }
    Ok(())
}

/// Move a task through its lifecycle. Sets/clears `completed_at` so 'done'
/// carries a timestamp and reopening clears it.
///
/// **Closing a task closes its `waiting_on`**, and that is a valid-time
/// close, not an invalidation: nobody owes a finished task, so the claim
/// stopped being true — it was not wrong. Without this the fact stays live
/// forever and every future read of that person says they still hold a ball
/// they handed back months ago; `facts_for_node` is bidirectional, so it is
/// their card that carries it, not the task's.
///
/// The task stays findable under that person afterwards, because the entity
/// filter unions `about` with `waiting_on` and reads `fact` history rather
/// than only what is live. That union is the whole reason this close is
/// safe: retiring the live claim does not retire the association.
///
/// Reopening deliberately does **not** resurrect it. Who owes a reopened
/// task is a new question with a new answer, and guessing the old one is how
/// a person silently re-acquires an obligation nobody gave them.
pub fn set_task_status(conn: &Connection, node_id: &str, status: &str) -> Result<()> {
    if !TASK_STATUSES.contains(&status) {
        return Err(Error::Other(format!(
            "unknown task status '{status}' (one of {})",
            TASK_STATUSES.join("|")
        )));
    }
    let n = conn.execute(
        "UPDATE task_detail SET status = ?2,
                completed_at = CASE WHEN ?2 IN ('done','dropped')
                                    THEN COALESCE(completed_at, datetime('now'))
                                    ELSE NULL END
         WHERE node_id = ?1",
        params![node_id, status],
    )?;
    if n == 0 {
        return Err(Error::Other(format!("{node_id} is not a task")));
    }
    if matches!(status, "done" | "dropped") {
        conn.execute(
            "UPDATE fact SET valid_to = datetime('now')
              WHERE subject_id = ?1 AND predicate = 'waiting_on'
                AND valid_to IS NULL AND invalidated_at IS NULL",
            params![node_id],
        )?;
    }
    Ok(())
}

#[derive(Debug, Serialize)]
pub struct GtdReview {
    /// Active projects with no episode activity in 14 days.
    pub stalled_projects: Vec<(String, String, Option<String>)>, // (id, name, last_activity)
    /// Waiting-on items with who and how long.
    pub waiting_on: Vec<(String, String, Option<String>)>, // (task, person, due)
    /// Tasks sitting in inbox (no next action decided).
    pub inbox_tasks: Vec<(String, String)>, // (id, name)
    /// Goals with no active project pursuing them.
    pub goals_without_project: Vec<(String, String)>,
}

pub fn weekly_review(conn: &Connection) -> Result<GtdReview> {
    // Stalled: active project whose most recent mention-episode is old/absent.
    let mut stmt = conn.prepare(
        "SELECT n.id, n.name,
                (SELECT MAX(e.occurred_at) FROM episode e
                 JOIN mention m ON m.episode_id = e.id WHERE m.node_id = n.id) AS last_act
         FROM nodes n
         LEFT JOIN project_detail pd ON pd.node_id = n.id
         WHERE n.node_type = 'project'
           AND COALESCE(pd.status, 'active') = 'active'
           AND COALESCE(
                 (SELECT MAX(e.occurred_at) FROM episode e
                  JOIN mention m ON m.episode_id = e.id WHERE m.node_id = n.id),
                 '') < datetime('now', '-14 days')",
    )?;
    let stalled_projects = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    // Waiting-on across every channel at once — the query no single app can
    // answer (§8.4).
    let mut stmt = conn.prepare(
        "SELECT tn.name, pn.name, td.due_at
         FROM fact_current f
         JOIN nodes tn ON tn.id = f.subject_id
         JOIN nodes pn ON pn.id = f.object_id
         LEFT JOIN task_detail td ON td.node_id = tn.id
         WHERE f.predicate = 'waiting_on'
           AND COALESCE(td.status, 'waiting') NOT IN ('done','dropped')
         ORDER BY td.due_at ASC NULLS LAST",
    )?;
    let waiting_on = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT n.id, n.name FROM nodes n
         JOIN task_detail td ON td.node_id = n.id
         WHERE td.status = 'inbox' ORDER BY n.created_at ASC",
    )?;
    let inbox_tasks = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut stmt = conn.prepare(
        "SELECT n.id, n.name FROM nodes n
         LEFT JOIN goal_detail gd ON gd.node_id = n.id
         WHERE n.node_type = 'goal' AND COALESCE(gd.status,'active') = 'active'
           AND NOT EXISTS (
             SELECT 1 FROM fact_current f
             JOIN nodes p ON p.id = f.object_id
             LEFT JOIN project_detail pd ON pd.node_id = p.id
             WHERE f.subject_id = n.id AND f.predicate = 'pursued_via'
               AND COALESCE(pd.status,'active') = 'active')",
    )?;
    let goals_without_project = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    Ok(GtdReview {
        stalled_projects,
        waiting_on,
        inbox_tasks,
        goals_without_project,
    })
}

/// Generate the boot-injection memory file (§8.3): ~20 facts / ~500 tokens,
/// always in context. Top-salience current facts + most-interacted people.
/// valid_from is always rendered — undated personal facts age badly.
pub fn generate_memory_md(conn: &Connection, max_tokens: usize) -> Result<String> {
    let mut out = String::from(
        "# Knowledge graph — boot context\n\
         <!-- generated by `pkg memory-md`; do not hand-edit (regenerated nightly) -->\n\n",
    );
    let mut spent = out.len() / 4;

    // Most-salient current facts: weight × confidence × log(observations),
    // human-curated (manual) and deterministic extractors first.
    let mut stmt = conn.prepare(
        "SELECT statement, valid_from, extractor FROM fact_current
         ORDER BY (weight * confidence * (1.0 + MIN(observation_count, 10) * 0.1)) DESC,
                  CASE WHEN extractor IN ('manual','attendee') THEN 0 ELSE 1 END,
                  ingested_at DESC
         LIMIT 40",
    )?;
    let facts: Vec<(String, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
        .collect::<std::result::Result<_, _>>()?;

    if !facts.is_empty() {
        out.push_str("## Facts\n");
        let mut n = 0;
        for (statement, valid_from, _extractor) in facts {
            if n >= 20 {
                break;
            }
            let line = match valid_from {
                Some(v) => format!("- as of {}: {}\n", &v[..10.min(v.len())], statement),
                None => format!("- {}\n", statement),
            };
            let t = line.len() / 4;
            if spent + t > max_tokens {
                break;
            }
            spent += t;
            out.push_str(&line);
            n += 1;
        }
        out.push('\n');
    }

    // People: top interaction counts with per-channel recency.
    let mut stmt = conn.prepare(
        "SELECT n.name, pi.interaction_count, pi.last_seen_at, pi.last_channel
         FROM person_interaction pi JOIN nodes n ON n.id = pi.node_id
         ORDER BY pi.interaction_count DESC LIMIT 8",
    )?;
    let people: Vec<(String, i64, Option<String>, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if !people.is_empty() && spent < max_tokens {
        out.push_str("## People (by interaction volume)\n");
        for (name, count, last_seen, channel) in people {
            let line = format!(
                "- {name}: {count} interactions, last {} via {}\n",
                last_seen
                    .as_deref()
                    .map(|s| &s[..10.min(s.len())])
                    .unwrap_or("-"),
                channel.as_deref().unwrap_or("-")
            );
            let t = line.len() / 4;
            if spent + t > max_tokens {
                break;
            }
            spent += t;
            out.push_str(&line);
        }
        out.push('\n');
    }

    // Active projects with recent activity.
    let mut stmt = conn.prepare(
        "SELECT n.name,
                (SELECT MAX(e.occurred_at) FROM episode e
                 JOIN mention m ON m.episode_id = e.id WHERE m.node_id = n.id) AS last_act
         FROM nodes n WHERE n.node_type = 'project'
         ORDER BY last_act DESC NULLS LAST LIMIT 6",
    )?;
    let projects: Vec<(String, Option<String>)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;
    if !projects.is_empty() && spent < max_tokens {
        out.push_str("## Active projects\n");
        for (name, last) in projects {
            let line = format!(
                "- {name} (last activity {})\n",
                last.as_deref()
                    .map(|s| &s[..10.min(s.len())])
                    .unwrap_or("unknown")
            );
            if spent + line.len() / 4 > max_tokens {
                break;
            }
            spent += line.len() / 4;
            out.push_str(&line);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::fact::assert_fact;
    use crate::graph::{upsert_node, Node};

    #[test]
    fn test_memory_md_bounded_and_dated() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("w", "person", "Nadia")).unwrap();
        upsert_node(&conn, &Node::new("aim2", "project", "Aim 2")).unwrap();
        assert_fact(
            &conn,
            "w",
            "works_on",
            Some("aim2"),
            None,
            "Nadia is lead on Aim 2",
            None,
            Some("2026-07-31"),
            0.9,
            "manual",
        )
        .unwrap();

        let md = generate_memory_md(&conn, 500).unwrap();
        assert!(md.contains("as of 2026-07-31: Nadia is lead on Aim 2"));
        assert!(md.len() / 4 <= 600, "must stay near the token budget");
    }

    #[test]
    fn test_weekly_review_queries() {
        let conn = open_memory().unwrap();
        // A goal with no project, a waiting task.
        upsert_node(&conn, &Node::new("g1", "goal", "Land R01")).unwrap();
        upsert_node(&conn, &Node::new("t1", "task", "Get pilot data")).unwrap();
        upsert_node(&conn, &Node::new("w", "person", "Nadia")).unwrap();
        conn.execute(
            "INSERT INTO task_detail (node_id, status, task_type) VALUES ('t1','waiting','waiting')",
            [],
        ).unwrap();
        assert_fact(
            &conn,
            "t1",
            "waiting_on",
            Some("w"),
            None,
            "Get pilot data waiting on Nadia",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();

        let review = weekly_review(&conn).unwrap();
        assert_eq!(review.goals_without_project.len(), 1);
        assert_eq!(review.waiting_on.len(), 1);
        assert_eq!(review.waiting_on[0].1, "Nadia");
    }

    #[test]
    fn test_list_tasks_ordering_and_joins() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p1", "project", "R01 renewal")).unwrap();
        upsert_node(&conn, &Node::new("w", "person", "Nadia")).unwrap();
        for (id, name, status, due) in [
            ("t-in", "Triage inbox thing", "inbox", None),
            ("t-next2", "Next, no due date", "next", None),
            ("t-next1", "Next, due soon", "next", Some("2026-08-05")),
            ("t-wait", "Get pilot data", "waiting", None),
            ("t-done", "Shipped already", "done", None),
        ] {
            upsert_node(&conn, &Node::new(id, "task", name)).unwrap();
            conn.execute(
                "INSERT INTO task_detail (node_id, status, due_at, parent_id)
                 VALUES (?1, ?2, ?3, 'p1')",
                params![id, status, due],
            )
            .unwrap();
        }
        assert_fact(
            &conn,
            "t-wait",
            "waiting_on",
            Some("w"),
            None,
            "Get pilot data waiting on Nadia",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();

        // Open only: done excluded; next-with-due before next-without, then inbox.
        let open = list_tasks(&conn, false).unwrap();
        let ids: Vec<&str> = open.iter().map(|t| t.node_id.as_str()).collect();
        assert_eq!(ids, vec!["t-next1", "t-next2", "t-in", "t-wait"]);
        assert_eq!(open[0].project.as_deref(), Some("R01 renewal"));
        assert_eq!(open[3].waiting_on.as_deref(), Some("Nadia"));

        let all = list_tasks(&conn, true).unwrap();
        assert_eq!(all.len(), 5);
        assert_eq!(all.last().unwrap().node_id, "t-done");
    }

    #[test]
    fn test_set_task_status_lifecycle() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("t1", "task", "Write methods section")).unwrap();
        conn.execute("INSERT INTO task_detail (node_id) VALUES ('t1')", [])
            .unwrap();

        set_task_status(&conn, "t1", "done").unwrap();
        let done_at: Option<String> = conn
            .query_row(
                "SELECT completed_at FROM task_detail WHERE node_id = 't1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(done_at.is_some(), "done stamps completed_at");

        // Reopening clears the stamp.
        set_task_status(&conn, "t1", "next").unwrap();
        let reopened: Option<String> = conn
            .query_row(
                "SELECT completed_at FROM task_detail WHERE node_id = 't1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(reopened.is_none());

        assert!(set_task_status(&conn, "t1", "someday").is_err());
        assert!(set_task_status(&conn, "nope", "done").is_err());
    }

    /// What a task was captured from, and the four ways a pointer is refused.
    ///
    /// The refusals are the test: a stored pointer nothing can follow renders
    /// a "read the original" affordance over nothing, which is worse than the
    /// absence this feature exists to fix.
    #[test]
    fn a_task_remembers_what_it_was_captured_from() {
        let conn = open_memory().unwrap();
        let id = create_task(&conn, "Decide on the award nominations", None, None, None).unwrap();

        // A task nobody captured from anywhere names nothing — the honest
        // answer for one typed into the board, and no button on the card.
        assert!(
            list_tasks(&conn, false).unwrap()[0].captured_from.is_none(),
            "a hand-captured task has no provenance to show"
        );

        let mail = serde_json::json!({
            "kind": "mail",
            "account": "ostrander",
            "id": "thread-19a2f",
            "label": "SAS 2027 award nominations",
            "at": "2026-08-11T14:02:00Z",
        });
        set_task_captured_from(&conn, &id, Some(&mail)).unwrap();
        let got = list_tasks(&conn, false).unwrap()[0]
            .captured_from
            .clone()
            .expect("the pointer survives the round trip");
        assert_eq!(got["kind"], "mail");
        assert_eq!(got["account"], "ostrander");
        assert_eq!(got["id"], "thread-19a2f");
        assert_eq!(got["label"], "SAS 2027 award nominations");

        // A copy is refused, which is what keeps this a pointer. The graph is
        // not where other people's words live.
        let with_body = serde_json::json!({
            "kind": "mail", "account": "ostrander", "id": "t", "body": "Dear Ada, …",
        });
        assert!(set_task_captured_from(&conn, &id, Some(&with_body)).is_err());

        // A kind no surface can open, and a kind with nothing to open.
        assert!(set_task_captured_from(
            &conn,
            &id,
            Some(&serde_json::json!({"kind": "fax", "id": "1"}))
        )
        .is_err());
        assert!(
            set_task_captured_from(&conn, &id, Some(&serde_json::json!({"kind": "mail"}))).is_err()
        );
        // Thread ids are account-scoped; without one this names a thread in
        // whichever mailbox the reader asks first.
        assert!(set_task_captured_from(
            &conn,
            &id,
            Some(&serde_json::json!({"kind": "mail", "id": "t"}))
        )
        .is_err());

        // A refused pointer leaves the good one standing rather than half-
        // writing over it.
        assert_eq!(
            list_tasks(&conn, false).unwrap()[0]
                .captured_from
                .as_ref()
                .unwrap()["id"],
            "thread-19a2f"
        );

        // A subject line is somebody else's prose: capped at the door, like an
        // image, because the caller still has a real task to capture.
        let long = "x".repeat(500);
        set_task_captured_from(
            &conn,
            &id,
            Some(&serde_json::json!({"kind": "frontdoor", "id": "41", "label": long})),
        )
        .unwrap();
        let label = list_tasks(&conn, false).unwrap()[0]
            .captured_from
            .clone()
            .unwrap()["label"]
            .as_str()
            .unwrap()
            .to_string();
        assert_eq!(label.chars().count(), LABEL_MAX);
        assert!(label.ends_with('…'));

        // Cleared, and the session it may sit beside is untouched.
        set_task_session(&conn, &id, "20260826T101804-476080dd").unwrap();
        set_task_captured_from(&conn, &id, None).unwrap();
        let row = &list_tasks(&conn, false).unwrap()[0];
        assert!(row.captured_from.is_none());
        assert_eq!(row.session.as_deref(), Some("20260826T101804-476080dd"));

        assert!(set_task_captured_from(&conn, "task-nope", Some(&mail)).is_err());
    }

    #[test]
    fn test_parse_due_forms() {
        let today = chrono::Utc::now().date_naive();
        assert_eq!(parse_due("").unwrap(), None);
        assert_eq!(parse_due("  ").unwrap(), None);
        assert_eq!(
            parse_due("2026-09-15").unwrap().as_deref(),
            Some("2026-09-15")
        );
        assert_eq!(
            parse_due("today").unwrap().unwrap(),
            today.format("%Y-%m-%d").to_string()
        );
        assert_eq!(
            parse_due("+7d").unwrap().unwrap(),
            (today + chrono::Days::new(7))
                .format("%Y-%m-%d")
                .to_string()
        );
        assert!(parse_due("next tuesday").is_err());
        assert!(parse_due("+xd").is_err());
    }

    #[test]
    fn test_create_and_edit_task() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p1", "project", "R01 renewal")).unwrap();

        let id = create_task(
            &conn,
            "Draft specific aims",
            Some("2026-08-20"),
            Some("R01 renewal"),
            Some("writing"),
        )
        .unwrap();
        let t = &list_tasks(&conn, false).unwrap()[0];
        assert_eq!(t.node_id, id);
        assert_eq!(t.status, "inbox");
        assert_eq!(t.due_at.as_deref(), Some("2026-08-20"));
        assert_eq!(t.project.as_deref(), Some("R01 renewal"));
        assert_eq!(t.context_tag.as_deref(), Some("writing"));

        // Unknown project bounces instead of creating a mystery node.
        assert!(create_task(&conn, "x", None, Some("No Such Project"), None).is_err());

        // Edit: change due, clear context, leave defer untouched.
        update_task_schedule(&conn, &id, Some(Some("2026-08-25")), None, Some(None)).unwrap();
        let t = &list_tasks(&conn, false).unwrap()[0];
        assert_eq!(t.due_at.as_deref(), Some("2026-08-25"));
        assert_eq!(t.context_tag, None);
        assert!(update_task_schedule(&conn, "nope", Some(None), None, None).is_err());
    }

    /// The whole point of the `about` predicate, in one assertion.
    ///
    /// Fails on the old behaviour twice over: there was no `about` at all, so
    /// there was nothing to survive; and `waiting_on` — the only association
    /// that existed — was never closed, so the *other* half of this test
    /// (that a finished task stops claiming somebody owes it) also failed.
    #[test]
    fn a_finished_task_is_still_findable_under_the_person_it_was_about() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-ostrander", "person", "Ostrander")).unwrap();
        let id = create_task(&conn, "Draft the SAS award citation", None, None, None).unwrap();
        add_task_about(&conn, &id, "Ostrander").unwrap();
        set_task_waiting_on(&conn, &id, "Ostrander").unwrap();

        // Open: found either way.
        assert_eq!(
            tasks_for_entity(&conn, "p-ostrander", false).unwrap().len(),
            1
        );

        // The negative below is only worth something if the fact was there to
        // begin with. Without this line a `waiting_on` that never got written
        // would make the "it was closed" assertion pass vacuously.
        let before: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_current
                  WHERE subject_id = ?1 AND predicate = 'waiting_on'",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(before, 1, "the obligation is live while the task is open");

        set_task_status(&conn, &id, "done").unwrap();

        // Closed and excluded from the live board...
        assert!(
            tasks_for_entity(&conn, "p-ostrander", false)
                .unwrap()
                .is_empty(),
            "a done task is not open work"
        );
        // ...but still findable, with its completion stamp, which is the
        // capability this whole change exists for.
        let closed = tasks_for_entity(&conn, "p-ostrander", true).unwrap();
        assert_eq!(closed.len(), 1, "a finished task stays findable under them");
        assert_eq!(closed[0].status, "done");
        assert!(closed[0].completed_at.is_some());
        assert_eq!(closed[0].about, vec!["Ostrander".to_string()]);

        // And the live claim is gone: nobody owes a finished task. This is
        // the assertion that fails loudest on the old code.
        let live_waiting: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_current
                  WHERE subject_id = ?1 AND predicate = 'waiting_on'",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(live_waiting, 0, "closing a task closes who owes it");
        // Closed in valid time, not invalidated: it was true, and stopped.
        let closed_not_wrong: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact
                  WHERE subject_id = ?1 AND predicate = 'waiting_on'
                    AND valid_to IS NOT NULL AND invalidated_at IS NULL",
                params![id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            closed_not_wrong, 1,
            "a finished obligation ended, it was not wrong"
        );
    }

    /// The filter unions every way a task can be tied to an entity, and ties
    /// nothing else in. The negative half matters most: a filter that returns
    /// everything is indistinguishable from one that works, on a board this
    /// small.
    #[test]
    fn the_entity_filter_unions_the_three_associations_and_the_project() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-wren", "person", "Wren")).unwrap();
        upsert_node(&conn, &Node::new("proj-tide", "project", "Tidelab")).unwrap();

        let about = create_task(&conn, "Write the intro", None, None, None).unwrap();
        add_task_about(&conn, &about, "Wren").unwrap();

        let waiting = create_task(&conn, "Collect the consent forms", None, None, None).unwrap();
        set_task_waiting_on(&conn, &waiting, "Wren").unwrap();

        let assigned = create_task(&conn, "Book the room", None, None, None).unwrap();
        assert_fact(
            &conn,
            &assigned,
            "assigned_to",
            Some("p-wren"),
            None,
            "Book the room is assigned to Wren",
            None,
            None,
            1.0,
            "manual",
        )
        .unwrap();

        let child = create_task(&conn, "Ship the pilot", None, Some("Tidelab"), None).unwrap();
        let unrelated = create_task(&conn, "Renew the parking permit", None, None, None).unwrap();

        let mut found: Vec<String> = tasks_for_entity(&conn, "p-wren", true)
            .unwrap()
            .into_iter()
            .map(|t| t.node_id)
            .collect();
        found.sort();
        let mut want = vec![about, waiting, assigned];
        want.sort();
        assert_eq!(
            found, want,
            "about + waiting_on + assigned_to, and nothing else"
        );

        // The project is a foreign key, not a fact — so it needs its own arm
        // of the filter, and this is what catches its removal.
        let by_project: Vec<String> = tasks_for_entity(&conn, "proj-tide", true)
            .unwrap()
            .into_iter()
            .map(|t| t.node_id)
            .collect();
        assert_eq!(by_project, vec![child], "a task's parent project finds it");
        assert!(
            !by_project.contains(&unrelated),
            "an unrelated task is not everyone's"
        );
    }

    /// `about` is multi-valued and additive. A `set`-shaped API would make
    /// "also file this under Wren" silently drop Ostrander.
    #[test]
    fn about_accumulates_is_idempotent_and_removes_one_at_a_time() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-wren", "person", "Wren")).unwrap();
        upsert_node(&conn, &Node::new("p-ostrander", "person", "Ostrander")).unwrap();
        let id = create_task(&conn, "Schedule the committee", None, None, None).unwrap();

        add_task_about(&conn, &id, "Ostrander").unwrap();
        add_task_about(&conn, &id, "Wren").unwrap();
        assert_eq!(
            task_about(&conn, &id).unwrap().len(),
            2,
            "adding never replaces"
        );

        // Re-adding is a no-op, so a re-run of the scan is safe.
        add_task_about(&conn, &id, "Wren").unwrap();
        assert_eq!(task_about(&conn, &id).unwrap().len(), 2, "idempotent");

        assert!(remove_task_about(&conn, &id, "Wren").unwrap());
        let left = task_about(&conn, &id).unwrap();
        assert_eq!(left.len(), 1);
        assert_eq!(left[0].1, "Ostrander", "removing one leaves the other");
        assert!(
            !remove_task_about(&conn, &id, "Wren").unwrap(),
            "removing what is not there reports false rather than erroring"
        );

        // A name the graph does not know is an error, never a new node.
        assert!(add_task_about(&conn, &id, "Nobody At All").is_err());
        let minted: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE name = 'Nobody At All'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(minted, 0, "a typo must not mint a plausible person");

        assert!(
            add_task_about(&conn, "p-wren", "Ostrander").is_err(),
            "not a task"
        );
    }

    /// The title scan files what it finds as *shadow*, refuses a bare first
    /// name, and never links one task to another.
    #[test]
    fn the_title_scan_mints_shadow_and_refuses_a_bare_first_name() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("proj-tide", "project", "Tidelab")).unwrap();
        upsert_node(&conn, &Node::new("p-ostrander", "person", "Ostrander")).unwrap();
        conn.execute(
            "INSERT INTO node_alias (alias, node_id, source) VALUES ('wren', 'p-ostrander', 'firstname')",
            [],
        )
        .unwrap();

        let hit = create_task(&conn, "Ship the Tidelab pilot", None, None, None).unwrap();
        let weak = create_task(&conn, "Call wren about the forms", None, None, None).unwrap();
        create_task(&conn, "Renew the parking permit", None, None, None).unwrap();

        let report = propose_task_entities(&conn).unwrap();
        assert_eq!(report.scanned, 3);
        assert_eq!(
            report.minted, 1,
            "the canonical project name is a strong match"
        );
        assert!(
            report.refused_weak >= 1,
            "a bare first name has no corroboration here"
        );

        // Filed, and findable...
        let found = tasks_for_entity(&conn, "proj-tide", true).unwrap();
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].node_id, hit);

        // ...but labelled unreviewed, which is the whole bargain.
        let tier: String = conn
            .query_row(
                "SELECT tier FROM fact_current WHERE subject_id = ?1 AND predicate = 'about'",
                params![hit],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(tier, "shadow", "inference is served, never asserted");

        assert!(
            task_about(&conn, &weak).unwrap().is_empty(),
            "the weak match was refused, not quietly filed"
        );

        // Re-running proposes nothing new — the scan is safe on a cron.
        let again = propose_task_entities(&conn).unwrap();
        assert_eq!(again.minted, 0, "already-associated tasks are skipped");
    }

    /// A model's `when` is text until it parses, and the repair finds what
    /// went in before that was true.
    ///
    /// Fails on the old behaviour at the first assertion: `"null"` used to
    /// land in `due_at` verbatim.
    #[test]
    fn a_models_when_is_parsed_and_the_old_damage_is_repairable() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p-wren", "person", "Wren")).unwrap();

        // Straight through the accept path, which is where the bug lived.
        let propose = |when: &str| {
            let payload = serde_json::json!({
                "kind": "commitment", "what": format!("thing due {when}"),
                "who": "Wren", "when": when, "direction": "owed_to_me",
            });
            conn.execute(
                "INSERT INTO fact_candidate (payload, status, proposed_by)
                 VALUES (?1, 'proposed', 'llm')",
                params![payload.to_string()],
            )
            .unwrap();
            conn.last_insert_rowid()
        };

        let junk = crate::extract::accept_commitment(&conn, propose("null")).unwrap();
        let real = crate::extract::accept_commitment(&conn, propose("2026-09-15")).unwrap();
        let prose = crate::extract::accept_commitment(&conn, propose("sometime soon")).unwrap();

        let due = |id: &str| -> Option<String> {
            conn.query_row(
                "SELECT due_at FROM task_detail WHERE node_id = ?1",
                params![id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(
            due(&junk),
            None,
            "'null' is not a date and must not be stored"
        );
        assert_eq!(due(&prose), None, "neither is prose");
        assert_eq!(
            due(&real).as_deref(),
            Some("2026-09-15"),
            "a real date survives"
        );

        // And the fact's valid_from — the half that is easy to forget,
        // because nothing about the board shows it.
        let vf: Option<String> = conn
            .query_row(
                "SELECT valid_from FROM fact WHERE subject_id = ?1 AND predicate = 'waiting_on'",
                params![junk],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            vf, None,
            "garbage must not reach bi-temporal valid time either"
        );

        // The repair, for rows written before the parse existed. Hand-write
        // the damage the old code would have made.
        conn.execute(
            "UPDATE task_detail SET due_at = 'null' WHERE node_id = ?1",
            params![junk],
        )
        .unwrap();
        conn.execute(
            "UPDATE fact SET valid_from = 'whenever' WHERE subject_id = ?1",
            params![junk],
        )
        .unwrap();

        let dry = repair_unparseable_dates(&conn, false).unwrap();
        assert!(dry.found.len() >= 2, "the survey sees both columns");
        assert_eq!(dry.repaired, 0, "a survey must not write");
        assert_eq!(
            due(&junk).as_deref(),
            Some("null"),
            "and must not have written"
        );

        let applied = repair_unparseable_dates(&conn, true).unwrap();
        assert!(applied.repaired >= 2);
        assert_eq!(due(&junk), None);

        // The good row is untouched — a repair that also erases valid data
        // is not a repair.
        assert_eq!(due(&real).as_deref(), Some("2026-09-15"));
        assert!(
            repair_unparseable_dates(&conn, false)
                .unwrap()
                .found
                .is_empty(),
            "and it is idempotent"
        );
    }

    /// Task nodes are in `nodes` like anything else, so the scan has to
    /// exclude them explicitly or every task links to every task sharing a
    /// word. This is the test for that one `continue`.
    #[test]
    fn the_title_scan_never_links_one_task_to_another() {
        let conn = open_memory().unwrap();
        create_task(&conn, "Review the Tidelab budget", None, None, None).unwrap();
        create_task(&conn, "Review the Tidelab budget again", None, None, None).unwrap();
        let report = propose_task_entities(&conn).unwrap();
        assert_eq!(
            report.minted, 0,
            "tasks are not entities to file each other under"
        );
    }
}
