//! Agent-session sources (§5.1): Hermes (`~/.hermes/state.db`) and Claude Code
//! (`~/.claude/projects/*/*.jsonl`).
//!
//! Identity key is `cwd`/touched paths → **project**, not people — ★★★★★ for
//! projects, zero AI. This is why session data works with the owner's own projects.
//!
//! Distillation rule (§5.3): ingest ~2% of bytes as knowledge (title, first
//! substantive user message, touched projects, counts); the raw transcript
//! stays in its original store, pointed at by source_ref.

use crate::episode::Episode;
use crate::error::Result;
use crate::sources::{ProposedLink, Source};
use std::collections::BTreeSet;
use std::path::PathBuf;

/// Derive a project root from a file path: `.../Github/<repo>/...` → that repo
/// dir; otherwise None (home-dir scratch work is not a project).
fn project_root_of(path: &str, home: &str) -> Option<(String, String)> {
    let gh_prefix = format!("{home}/Github/");
    if let Some(rest) = path.strip_prefix(&gh_prefix) {
        let repo = rest.split('/').next()?.to_string();
        if repo.is_empty() {
            return None;
        }
        return Some((format!("{gh_prefix}{repo}"), repo));
    }
    None
}

fn home_dir() -> String {
    std::env::var("HOME").unwrap_or_else(|_| ".".to_string())
}

// ─── Hermes ──────────────────────────────────────────────────────────────────

pub struct HermesSource {
    pub state_db: PathBuf,
}

impl HermesSource {
    pub fn default_path() -> PathBuf {
        PathBuf::from(home_dir()).join(".hermes").join("state.db")
    }

    pub fn new(state_db: impl Into<PathBuf>) -> Self {
        HermesSource {
            state_db: state_db.into(),
        }
    }
}

impl Source for HermesSource {
    fn id(&self) -> &'static str {
        "session.hermes"
    }

    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>> {
        if !self.state_db.exists() {
            return Ok(vec![]);
        }
        // Read-only: never write to another app's store.
        let conn = rusqlite::Connection::open_with_flags(
            &self.state_db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;
        let home = home_dir();

        let mut stmt = conn.prepare(
            "SELECT s.id, s.source, s.cwd, s.git_repo_root, s.git_branch, s.title,
                    s.message_count, s.model,
                    datetime(s.started_at, 'unixepoch') AS started,
                    datetime(COALESCE(s.ended_at, s.started_at), 'unixepoch') AS ended,
                    (SELECT m.content FROM messages m
                     WHERE m.session_id = s.id AND m.role = 'user' AND m.content IS NOT NULL
                     ORDER BY m.timestamp ASC LIMIT 1) AS first_user
             FROM sessions s
             WHERE s.message_count > 0
             ORDER BY s.started_at ASC",
        )?;

        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>("id")?,
                r.get::<_, Option<String>>("source")?,
                r.get::<_, Option<String>>("cwd")?,
                r.get::<_, Option<String>>("git_repo_root")?,
                r.get::<_, Option<String>>("git_branch")?,
                r.get::<_, Option<String>>("title")?,
                r.get::<_, i64>("message_count")?,
                r.get::<_, Option<String>>("model")?,
                r.get::<_, String>("started")?,
                r.get::<_, String>("ended")?,
                r.get::<_, Option<String>>("first_user")?,
            ))
        })?;

        let mut out = Vec::new();
        for row in rows {
            let (
                id,
                src,
                cwd,
                git_root,
                branch,
                title,
                msg_count,
                model,
                started,
                ended,
                first_user,
            ) = row?;
            if let Some(s) = since {
                if started.as_str() <= s {
                    continue;
                }
            }

            let mut projects: BTreeSet<(String, String)> = BTreeSet::new();
            for p in [git_root.as_deref(), cwd.as_deref()].into_iter().flatten() {
                if let Some(pr) = project_root_of(p, &home) {
                    projects.insert(pr);
                }
            }

            let mut body = String::new();
            if let Some(t) = &title {
                body.push_str(t);
                body.push_str("\n\n");
            }
            if let Some(fu) = &first_user {
                let fu: String = fu.chars().take(1500).collect();
                body.push_str(&fu);
                body.push('\n');
            }
            body.push_str(&format!(
                "\n[hermes session via {} · {} messages · model {}",
                src.as_deref().unwrap_or("?"),
                msg_count,
                model.as_deref().unwrap_or("?")
            ));
            if let Some(b) = &branch {
                body.push_str(&format!(" · branch {b}"));
            }
            for (_, name) in &projects {
                body.push_str(&format!(" · project {name}"));
            }
            body.push(']');

            out.push(Episode {
                id: 0,
                uid: String::new(),
                source: "session.hermes".into(),
                source_id: id.clone(),
                source_ref: Some(format!("{}#{}", self.state_db.display(), id)),
                body,
                occurred_at: started,
                occurred_end: Some(ended),
                ingested_at: String::new(),
                lat: None,
                lon: None,
                location: None,
                sensitivity: "personal".into(),
                scope_id: None,
                meta: Some(serde_json::json!({
                    "projects": projects.iter()
                        .map(|(root, name)| serde_json::json!({"root": root, "name": name}))
                        .collect::<Vec<_>>()
                })),
                raw: None, // raw lives in state.db; sessions retention is always 'keep'
            });
        }
        Ok(out)
    }

    fn deterministic_links(&self, ep: &Episode) -> Vec<ProposedLink> {
        projects_from_meta(ep)
    }
}

/// Shared: read deterministic project links out of episode.meta.
fn projects_from_meta(ep: &Episode) -> Vec<ProposedLink> {
    let Some(meta) = &ep.meta else { return vec![] };
    let Some(projects) = meta.get("projects").and_then(|p| p.as_array()) else {
        return vec![];
    };
    projects
        .iter()
        .filter_map(|p| {
            Some(ProposedLink::Project {
                root: p.get("root")?.as_str()?.to_string(),
                name: p.get("name")?.as_str()?.to_string(),
            })
        })
        .collect()
}

// ─── Claude Code ─────────────────────────────────────────────────────────────

pub struct ClaudeSource {
    pub projects_dir: PathBuf,
}

impl ClaudeSource {
    pub fn default_path() -> PathBuf {
        PathBuf::from(home_dir()).join(".claude").join("projects")
    }

    pub fn new(projects_dir: impl Into<PathBuf>) -> Self {
        ClaudeSource {
            projects_dir: projects_dir.into(),
        }
    }
}

#[derive(Default)]
struct ClaudeSession {
    first_ts: Option<String>,
    last_ts: Option<String>,
    first_user: Option<String>,
    n_user: usize,
    n_assistant: usize,
    cwds: BTreeSet<String>,
    touched_paths: BTreeSet<String>,
    git_branch: Option<String>,
}

fn iso_to_sqlite(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|_| iso.replace('T', " ").chars().take(19).collect())
}

/// Extract every `file_path`/`path`/`notebook_path` from tool_use inputs —
/// spec §5.1: "every Read/Edit path → artifact". v1 keeps it at project
/// granularity (per-file artifact nodes would need junk control first).
fn collect_paths(value: &serde_json::Value, out: &mut BTreeSet<String>) {
    if let Some(blocks) = value.as_array() {
        for b in blocks {
            if b.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                if let Some(input) = b.get("input") {
                    for key in ["file_path", "path", "notebook_path"] {
                        if let Some(p) = input.get(key).and_then(|p| p.as_str()) {
                            out.insert(p.to_string());
                        }
                    }
                }
            }
        }
    }
}

fn parse_claude_session(path: &std::path::Path) -> Result<ClaudeSession> {
    use std::io::BufRead;
    let file = std::fs::File::open(path)?;
    let reader = std::io::BufReader::new(file);
    let mut s = ClaudeSession::default();

    for line in reader.lines() {
        let Ok(line) = line else { break };
        let Ok(d) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        let t = d.get("type").and_then(|t| t.as_str()).unwrap_or("");
        if let Some(ts) = d.get("timestamp").and_then(|t| t.as_str()) {
            let ts = iso_to_sqlite(ts);
            if s.first_ts.is_none() {
                s.first_ts = Some(ts.clone());
            }
            s.last_ts = Some(ts);
        }
        if let Some(cwd) = d.get("cwd").and_then(|c| c.as_str()) {
            s.cwds.insert(cwd.to_string());
        }
        if s.git_branch.is_none() {
            if let Some(b) = d.get("gitBranch").and_then(|b| b.as_str()) {
                if b != "HEAD" {
                    s.git_branch = Some(b.to_string());
                }
            }
        }
        match t {
            "user" => {
                s.n_user += 1;
                if s.first_user.is_none() {
                    if let Some(content) = d.pointer("/message/content").and_then(|c| c.as_str()) {
                        if !content.starts_with('<') {
                            s.first_user = Some(content.chars().take(1500).collect());
                        }
                    }
                }
            }
            "assistant" => {
                s.n_assistant += 1;
                if let Some(content) = d.pointer("/message/content") {
                    collect_paths(content, &mut s.touched_paths);
                }
            }
            _ => {}
        }
    }
    Ok(s)
}

impl Source for ClaudeSource {
    fn id(&self) -> &'static str {
        "session.claude"
    }

    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>> {
        let mut out = Vec::new();
        if !self.projects_dir.exists() {
            return Ok(out);
        }
        let home = home_dir();

        for proj_entry in std::fs::read_dir(&self.projects_dir)? {
            let proj_dir = proj_entry?.path();
            if !proj_dir.is_dir() {
                continue;
            }
            for f in std::fs::read_dir(&proj_dir)? {
                let f = f?.path();
                if f.extension().is_none_or(|e| e != "jsonl") {
                    continue;
                }
                let session_id = f
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default()
                    .to_string();
                let s = parse_claude_session(&f)?;

                // Skip empty shells (mode-only files, no conversation).
                if s.n_user == 0 || s.first_ts.is_none() {
                    continue;
                }
                let occurred_at = s.first_ts.clone().unwrap();
                if let Some(cursor) = since {
                    if occurred_at.as_str() <= cursor {
                        continue;
                    }
                }

                let mut projects: BTreeSet<(String, String)> = BTreeSet::new();
                for p in s.cwds.iter().chain(s.touched_paths.iter()) {
                    if let Some(pr) = project_root_of(p, &home) {
                        projects.insert(pr);
                    }
                }

                let mut body = String::new();
                if let Some(fu) = &s.first_user {
                    body.push_str(fu);
                    body.push('\n');
                }
                body.push_str(&format!(
                    "\n[claude code session · {} user / {} assistant messages",
                    s.n_user, s.n_assistant
                ));
                if let Some(b) = &s.git_branch {
                    body.push_str(&format!(" · branch {b}"));
                }
                for (_, name) in &projects {
                    body.push_str(&format!(" · project {name}"));
                }
                body.push(']');

                out.push(Episode {
                    id: 0,
                    uid: String::new(),
                    source: "session.claude".into(),
                    source_id: session_id,
                    source_ref: Some(f.to_string_lossy().to_string()),
                    body,
                    occurred_at,
                    occurred_end: s.last_ts.clone(),
                    ingested_at: String::new(),
                    lat: None,
                    lon: None,
                    location: None,
                    sensitivity: "personal".into(),
                    scope_id: None,
                    meta: Some(serde_json::json!({
                        "projects": projects.iter()
                            .map(|(root, name)| serde_json::json!({"root": root, "name": name}))
                            .collect::<Vec<_>>(),
                        "touched_files": s.touched_paths.len(),
                    })),
                    raw: None, // raw lives in the jsonl; sessions retention is always 'keep'
                });
            }
        }
        out.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at));
        Ok(out)
    }

    fn deterministic_links(&self, ep: &Episode) -> Vec<ProposedLink> {
        projects_from_meta(ep)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;

    #[test]
    fn test_project_root_extraction() {
        let home_dir = std::env::temp_dir().join("pkg-test-home").join("user");
        let home = home_dir.to_str().unwrap();
        assert_eq!(
            project_root_of(
                &format!("{home}/Github/flowmail/src-tauri/src/db/graph.rs"),
                home
            ),
            Some((format!("{home}/Github/flowmail"), "flowmail".into()))
        );
        assert_eq!(project_root_of(&format!("{home}/notes.md"), home), None);
        assert_eq!(project_root_of("/nonexistent/x", home), None);
    }

    #[test]
    fn test_claude_session_parse_and_ingest() {
        let dir = tempfile::tempdir().unwrap();
        let proj = dir.path().join("-home-user");
        std::fs::create_dir_all(&proj).unwrap();
        let home_dir = std::env::temp_dir().join("pkg-test-home").join("user");
        let home = home_dir.to_str().unwrap();
        let jsonl = concat!(
            r#"{"type":"mode","mode":"normal","sessionId":"abc"}"#,
            "\n",
            r#"{"type":"user","cwd":"__HOME__","timestamp":"2026-08-01T10:00:00.000Z","message":{"role":"user","content":"help me fix the flowmail search bug"}}"#,
            "\n",
            r#"{"type":"assistant","timestamp":"2026-08-01T10:01:00.000Z","message":{"role":"assistant","content":[{"type":"tool_use","name":"Edit","input":{"file_path":"__HOME__/Github/flowmail/src-tauri/src/db/search.rs"}}]}}"#,
            "\n",
        )
        .replace("__HOME__", home);
        std::fs::write(proj.join("sess-1.jsonl"), jsonl).unwrap();

        std::env::set_var("HOME", home);
        let src = ClaudeSource::new(dir.path());
        let eps = src.fetch(None).unwrap();
        assert_eq!(eps.len(), 1);
        let ep = &eps[0];
        assert_eq!(ep.source_id, "sess-1");
        assert_eq!(ep.occurred_at, "2026-08-01 10:00:00");
        assert!(ep.body.contains("flowmail search bug"));

        let links = src.deterministic_links(ep);
        assert_eq!(links.len(), 1, "Edit path → flowmail project link");

        // Through the driver: project node + identifier + mention.
        let conn = open_memory().unwrap();
        let report = crate::sources::ingest(&conn, &src, None).unwrap();
        assert_eq!(report.inserted, 1);
        let project =
            crate::graph::get_node_by_identifier(&conn, "path", &format!("{home}/Github/flowmail"))
                .unwrap()
                .expect("project node created");
        assert_eq!(project.node_type, "project");
        assert_eq!(project.name, "flowmail");
        let eps = crate::episode::episodes_for_node(&conn, &project.id, 10).unwrap();
        assert_eq!(eps.len(), 1);
    }
}
