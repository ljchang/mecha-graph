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
}

/// All tasks, actionable statuses first then by due date. `include_closed`
/// adds done/dropped (newest completions first within their group).
pub fn list_tasks(conn: &Connection, include_closed: bool) -> Result<Vec<TaskItem>> {
    let mut stmt = conn.prepare_cached(
        "SELECT n.id, n.name, td.status, td.task_type, td.due_at, td.defer_until,
                td.context_tag, td.completed_at,
                (SELECT p.name FROM nodes p WHERE p.id = td.parent_id),
                (SELECT pn.name FROM fact_current f JOIN nodes pn ON pn.id = f.object_id
                 WHERE f.subject_id = n.id AND f.predicate = 'waiting_on' LIMIT 1)
         FROM nodes n JOIN task_detail td ON td.node_id = n.id
         WHERE ?1 OR td.status NOT IN ('done','dropped')
         ORDER BY CASE td.status WHEN 'next' THEN 0 WHEN 'inbox' THEN 1
                                 WHEN 'scheduled' THEN 2 WHEN 'waiting' THEN 3
                                 WHEN 'done' THEN 4 ELSE 5 END,
                  td.due_at IS NULL, td.due_at ASC,
                  td.completed_at DESC, n.created_at ASC",
    )?;
    let tasks = stmt
        .query_map(params![include_closed], |r| {
            Ok(TaskItem {
                node_id: r.get(0)?,
                name: r.get(1)?,
                status: r.get(2)?,
                task_type: r.get(3)?,
                due_at: r.get(4)?,
                defer_until: r.get(5)?,
                context_tag: r.get(6)?,
                completed_at: r.get(7)?,
                project: r.get(8)?,
                waiting_on: r.get(9)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(tasks)
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

/// Move a task through its lifecycle. Sets/clears `completed_at` so 'done'
/// carries a timestamp and reopening clears it.
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
}
