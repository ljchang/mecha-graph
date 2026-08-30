//! GitHub source (§5.1): your own activity stream — commits, PRs, issues,
//! reviews, releases — segmented per repo per day (§5.3, same shape as
//! Slack's channel-day episodes).
//!
//! Identity flow: repos bridge to PROJECT nodes. A repo named `foo` first
//! tries to join the project node a coding-sessions source already created
//! for `~/…/foo` (unique `path` identifier suffix match) — that single join
//! is what connects "what I coded locally" with "what landed on GitHub".
//! Otherwise a fresh project node keyed by `github_repo` is created.
//!
//! Auth: settings `token`, else $GITHUB_TOKEN, else `gh auth token` (the
//! logged-in gh CLI) — so a configured gh needs zero pkg config.
//! Data source: `GET /users/{login}/events` — the API serves ~90 days /
//! 300 events, so nightly sync loses nothing.

use crate::episode::Episode;
use crate::error::{Error, Result};
use crate::integrations::SourceConfig;
use crate::sources::IngestReport;
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};

pub struct GithubSource {
    pub token: String,
    /// Login to pull events for; None → resolve via /user.
    pub username: Option<String>,
    /// Events pages per sync (100 events each; API caps at 300 total).
    pub max_pages: usize,
}

impl GithubSource {
    pub fn from_config(cfg: &SourceConfig) -> Result<Self> {
        Ok(GithubSource {
            token: resolve_token(cfg)?,
            username: cfg.get_str("username").map(|s| s.to_string()),
            max_pages: cfg
                .settings
                .get("max_pages")
                .and_then(|v| v.as_integer())
                .unwrap_or(3) as usize,
        })
    }
}

/// settings.token → $GITHUB_TOKEN → `gh auth token`.
pub fn resolve_token(cfg: &SourceConfig) -> Result<String> {
    if let Some(t) = cfg.get_str("token") {
        return Ok(t.to_string());
    }
    if let Ok(t) = std::env::var("GITHUB_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    let out = std::process::Command::new("gh")
        .args(["auth", "token"])
        .output()
        .map_err(|_| Error::Other("no token, no $GITHUB_TOKEN, and gh CLI not found".into()))?;
    if !out.status.success() {
        return Err(Error::Other(
            "gh auth token failed — run `gh auth login`".into(),
        ));
    }
    let t = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if t.is_empty() {
        return Err(Error::Other("gh auth token returned nothing".into()));
    }
    Ok(t)
}

fn api_get(token: &str, path: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
    let mut req = ureq::get(&format!("https://api.github.com{path}"))
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .set("X-GitHub-Api-Version", "2022-11-28")
        .set("User-Agent", "pkg-knowledge-graph")
        .timeout(std::time::Duration::from_secs(30));
    for (k, v) in params {
        req = req.query(k, v);
    }
    req.call()
        .map_err(|e| Error::Other(format!("github {path}: {e}")))?
        .into_json()
        .map_err(|e| Error::Other(format!("github {path}: bad json: {e}")))
}

/// Validate the token; returns the authenticated login.
pub fn auth_user(token: &str) -> Result<String> {
    let body = api_get(token, "/user", &[])?;
    body["login"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| Error::Other("github /user: no login in response".into()))
}

/// Metadata the slim events feed omits, fetched separately.
#[derive(Default)]
struct Enrichment {
    /// (repo, PR number) → (title, merged).
    pr: HashMap<(String, i64), (String, bool)>,
    /// (repo, head sha) → (commit count, first-line messages ≤5) via the
    /// compare API — private-repo PushEvents carry no commits at all.
    push: HashMap<(String, String), (i64, Vec<String>)>,
}

/// One event → (repo "owner/name", "YYYY-MM-DD HH:MM:SS", summary line).
/// None for noise (watches, forks by others, unknown types).
///
/// The user-events feed serves SLIM payloads: PushEvent may omit `commits`
/// entirely (observed on private repos) and PullRequestEvent carries only
/// the PR number — hence the fallbacks and the `pr_info` side-channel.
fn summarize_event(
    ev: &serde_json::Value,
    enrich: &Enrichment,
) -> Option<(String, String, String)> {
    let repo = ev["repo"]["name"].as_str()?.to_string();
    // "2026-08-02T12:34:56Z" → SQLite datetime.
    let ts = ev["created_at"]
        .as_str()?
        .replace('T', " ")
        .replace('Z', "");
    let p = &ev["payload"];
    let pr_line = |action: &str, number: i64| -> String {
        match enrich.pr.get(&(repo.clone(), number)) {
            Some((title, merged)) => {
                let action = if *merged && action == "closed" {
                    "merged"
                } else {
                    action
                };
                format!("{action} PR #{number}: {title}")
            }
            None => format!("{action} PR #{number}"),
        }
    };
    let line = match ev["type"].as_str()? {
        "PushEvent" => {
            let branch = p["ref"]
                .as_str()
                .map(|r| r.rsplit('/').next().unwrap_or(r).to_string())
                .unwrap_or_else(|| "?".into());
            let inline: Vec<String> = p["commits"]
                .as_array()
                .map(|commits| {
                    commits
                        .iter()
                        .filter_map(|c| c["message"].as_str())
                        .map(|m| m.lines().next().unwrap_or("").chars().take(80).collect())
                        .take(5)
                        .collect()
                })
                .unwrap_or_default();
            let fetched = p["head"]
                .as_str()
                .and_then(|h| enrich.push.get(&(repo.clone(), h.to_string())));
            match (inline.is_empty(), fetched) {
                (false, _) => format!(
                    "pushed {} commit(s): {}",
                    p["size"].as_i64().unwrap_or(inline.len() as i64),
                    inline.join(" · ")
                ),
                (true, Some((0, _))) => return None, // empty force-push noise
                (true, Some((n, msgs))) if !msgs.is_empty() => {
                    format!("pushed {n} commit(s): {}", msgs.join(" · "))
                }
                (true, Some((n, _))) => format!("pushed {n} commit(s) to {branch}"),
                (true, None) => match p["size"].as_i64() {
                    Some(0) => return None,
                    Some(n) => format!("pushed {n} commit(s) to {branch}"),
                    None => format!("pushed to {branch}"),
                },
            }
        }
        "PullRequestEvent" => {
            let number = p["number"]
                .as_i64()
                .or_else(|| p["pull_request"]["number"].as_i64())
                .unwrap_or(0);
            let action = if p["pull_request"]["merged"].as_bool() == Some(true) {
                "merged"
            } else {
                p["action"].as_str().unwrap_or("touched")
            };
            if !matches!(action, "opened" | "closed" | "reopened" | "merged") {
                return None; // labeled/assigned/synchronize churn
            }
            pr_line(action, number)
        }
        "PullRequestReviewEvent" => {
            let state = p["review"]["state"]
                .as_str()
                .unwrap_or("commented")
                .to_string();
            format!(
                "{} ({state})",
                pr_line(
                    "reviewed",
                    p["pull_request"]["number"].as_i64().unwrap_or(0)
                )
            )
        }
        "PullRequestReviewCommentEvent" => format!(
            "commented on PR #{}",
            p["pull_request"]["number"].as_i64().unwrap_or(0)
        ),
        "IssuesEvent" => {
            let action = p["action"].as_str().unwrap_or("touched");
            // labeled/assigned/milestoned fire once per label — pure noise.
            if !matches!(action, "opened" | "closed" | "reopened") {
                return None;
            }
            format!(
                "{action} issue #{}: {}",
                p["issue"]["number"].as_i64().unwrap_or(0),
                p["issue"]["title"].as_str().unwrap_or("")
            )
        }
        "IssueCommentEvent" => format!(
            "commented on #{} {}: {}",
            p["issue"]["number"].as_i64().unwrap_or(0),
            p["issue"]["title"].as_str().unwrap_or(""),
            p["comment"]["body"]
                .as_str()
                .unwrap_or("")
                .lines()
                .next()
                .unwrap_or("")
                .chars()
                .take(120)
                .collect::<String>()
        ),
        "CreateEvent" => match p["ref_type"].as_str() {
            Some("repository") => "created repository".to_string(),
            Some("tag") => format!("tagged {}", p["ref"].as_str().unwrap_or("?")),
            Some("branch") => format!("created branch {}", p["ref"].as_str().unwrap_or("?")),
            _ => return None,
        },
        "ReleaseEvent" => format!(
            "released {}",
            p["release"]["tag_name"].as_str().unwrap_or("?")
        ),
        _ => return None, // WatchEvent, ForkEvent, DeleteEvent, bot noise
    };
    Some((repo, ts, line))
}

/// Group summarized events into (repo, day) buckets. `since` (SQLite
/// datetime) drops strictly-older events; pass the START of the cursor's day
/// so a partially-synced day re-aggregates completely (the day episode is
/// one unit — a mid-day cursor would otherwise truncate its body on update).
fn aggregate_events(
    events: &[serde_json::Value],
    since: Option<&str>,
    enrich: &Enrichment,
) -> BTreeMap<(String, String), Vec<(String, String)>> {
    let mut by_repo_day: BTreeMap<(String, String), Vec<(String, String)>> = BTreeMap::new();
    for ev in events {
        let Some((repo, ts, line)) = summarize_event(ev, enrich) else {
            continue;
        };
        if since.is_some_and(|s| ts.as_str() < s) {
            continue;
        }
        let day = ts[..10].to_string();
        by_repo_day.entry((repo, day)).or_default().push((ts, line));
    }
    by_repo_day
}

/// Repo → project node, creating or bridging as needed (see module doc).
fn resolve_repo_project(conn: &Connection, full_repo: &str) -> Result<String> {
    if let Some(node) = crate::graph::get_node_by_identifier(conn, "github_repo", full_repo)? {
        return Ok(node.id);
    }
    let short = full_repo.rsplit('/').next().unwrap_or(full_repo);

    // Bridge to an existing sessions-created project: unique path suffix
    // match only — two candidate paths means ambiguity, and ambiguity never
    // auto-links (same rule as person aliases).
    let candidates: Vec<String> = conn
        .prepare_cached(
            "SELECT DISTINCT ni.node_id FROM node_identifier ni
             JOIN nodes n ON n.id = ni.node_id
             WHERE ni.kind = 'path' AND n.node_type = 'project' AND ni.value LIKE ?1",
        )?
        .query_map(rusqlite::params![format!("%/{short}")], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;
    if candidates.len() == 1 {
        crate::graph::upsert_identifier(conn, "github_repo", full_repo, &candidates[0], "github")?;
        return Ok(candidates[0].clone());
    }

    let id = format!(
        "project-{}",
        &crate::ids::content_hash(&format!("github:{full_repo}"))[..12]
    );
    let mut node = crate::graph::Node::new(&id, "project", short);
    node.source = "github".into();
    node.source_ref = Some(format!("https://github.com/{full_repo}"));
    crate::graph::upsert_node(conn, &node)?;
    crate::graph::upsert_identifier(conn, "github_repo", full_repo, &id, "github")?;
    Ok(id)
}

/// GitHub-specific ingest (multi-call API — same pattern as `ingest_slack`).
pub fn ingest_github(
    conn: &Connection,
    src: &GithubSource,
    since: Option<&str>,
) -> Result<IngestReport> {
    let started = crate::ids::now();
    let mut report = IngestReport::default();

    let login = match &src.username {
        Some(u) => u.clone(),
        None => auth_user(&src.token)?,
    };

    // Rewind the cursor to the start of its day (see aggregate_events).
    let since_day_start = since.map(|s| format!("{} 00:00:00", &s[..10.min(s.len())]));

    let mut events: Vec<serde_json::Value> = Vec::new();
    for page in 1..=src.max_pages {
        let page_s = page.to_string();
        let body = api_get(
            &src.token,
            &format!("/users/{login}/events"),
            &[("per_page", "100"), ("page", &page_s)],
        )?;
        let Some(arr) = body.as_array() else { break };
        if arr.is_empty() {
            break;
        }
        // Events arrive newest-first: once a page ends before the cursor,
        // later pages are entirely older — stop.
        let page_exhausted = arr.iter().all(|ev| {
            ev["created_at"]
                .as_str()
                .map(|t| t.replace('T', " ").replace('Z', ""))
                .is_some_and(|ts| since_day_start.as_deref().is_some_and(|s| ts.as_str() < s))
        });
        events.extend(arr.iter().cloned());
        if page_exhausted {
            break;
        }
    }

    // The user-events feed is SLIM: PR events carry only a number; private
    // PushEvents carry only ref/head/before. Enrich both with one cached call
    // per unique PR / push inside the sync window. Failures degrade to
    // number-only or branch-only lines, never abort the sync.
    let mut enrich = Enrichment::default();
    for ev in &events {
        let (Some(kind), Some(repo), Some(ts)) = (
            ev["type"].as_str(),
            ev["repo"]["name"].as_str(),
            ev["created_at"]
                .as_str()
                .map(|t| t.replace('T', " ").replace('Z', "")),
        ) else {
            continue;
        };
        if since_day_start.as_deref().is_some_and(|s| ts.as_str() < s) {
            continue;
        }
        let p = &ev["payload"];
        match kind {
            "PullRequestEvent" | "PullRequestReviewEvent" => {
                let number = p["number"]
                    .as_i64()
                    .or_else(|| p["pull_request"]["number"].as_i64())
                    .unwrap_or(0);
                let key = (repo.to_string(), number);
                if number == 0 || enrich.pr.contains_key(&key) {
                    continue;
                }
                if let Ok(pr) = api_get(&src.token, &format!("/repos/{repo}/pulls/{number}"), &[]) {
                    if let Some(title) = pr["title"].as_str() {
                        enrich.pr.insert(
                            key,
                            (title.to_string(), pr["merged"].as_bool().unwrap_or(false)),
                        );
                    }
                }
            }
            "PushEvent" if p["commits"].as_array().map_or(true, |c| c.is_empty()) => {
                let (Some(head), Some(before)) = (p["head"].as_str(), p["before"].as_str()) else {
                    continue;
                };
                let key = (repo.to_string(), head.to_string());
                if enrich.push.contains_key(&key) {
                    continue;
                }
                if let Ok(cmp) = api_get(
                    &src.token,
                    &format!("/repos/{repo}/compare/{before}...{head}"),
                    &[],
                ) {
                    let msgs: Vec<String> = cmp["commits"]
                        .as_array()
                        .map(|commits| {
                            commits
                                .iter()
                                .filter_map(|c| c["commit"]["message"].as_str())
                                .map(|m| m.lines().next().unwrap_or("").chars().take(80).collect())
                                .take(5)
                                .collect()
                        })
                        .unwrap_or_default();
                    let total = cmp["total_commits"].as_i64().unwrap_or(msgs.len() as i64);
                    enrich.push.insert(key, (total, msgs));
                }
            }
            _ => {}
        }
    }

    let mut repo_nodes: HashMap<String, String> = HashMap::new();
    for ((repo, day), mut items) in aggregate_events(&events, since_day_start.as_deref(), &enrich) {
        items.sort();
        let first = items.first().unwrap().0.clone();
        let last = items.last().unwrap().0.clone();
        let lines: Vec<String> = items
            .iter()
            .map(|(ts, line)| format!("[{}] {line}", &ts[11..16.min(ts.len())]))
            .collect();
        let body = format!(
            "{repo} — {day}\n{}",
            lines.join("\n").chars().take(8000).collect::<String>()
        );

        let ep = Episode {
            id: 0,
            uid: String::new(),
            source: "github.activity".into(),
            source_id: format!("{repo}:{day}"),
            source_ref: Some(format!("https://github.com/{repo}")),
            body: body.clone(),
            occurred_at: first,
            occurred_end: Some(last),
            ingested_at: String::new(),
            lat: None,
            lon: None,
            location: None,
            sensitivity: "personal".into(),
            scope_id: None,
            meta: None,
            raw: None,
        };
        let (episode_id, outcome) = crate::episode::upsert_episode(conn, &ep)?;
        match outcome {
            crate::episode::IngestOutcome::Inserted => report.inserted += 1,
            crate::episode::IngestOutcome::Updated => report.updated += 1,
            crate::episode::IngestOutcome::Unchanged => {
                report.unchanged += 1;
                continue;
            }
            crate::episode::IngestOutcome::Tombstoned => {
                report.tombstoned += 1;
                continue;
            }
        }

        let project_id = match repo_nodes.get(&repo) {
            Some(id) => id.clone(),
            None => {
                let id = resolve_repo_project(conn, &repo)?;
                repo_nodes.insert(repo.clone(), id.clone());
                id
            }
        };
        crate::episode::add_mention(conn, episode_id, &project_id, "attendee", 1.0)?;
        report.mentions += 1;
        // Commit messages and issue titles carry names — same alias payoff
        // as Bee (§5.1).
        report.alias_mentions += crate::episode::link_by_alias_scan(conn, episode_id, &body)?;
    }

    conn.execute(
        "INSERT INTO ingest_state (source, cursor, last_run_at, last_ok_at, items_seen, last_error)
         VALUES ('github', ?1, ?2, ?2, ?3, NULL)
         ON CONFLICT(source) DO UPDATE SET
             cursor = COALESCE(excluded.cursor, cursor), last_run_at = excluded.last_run_at,
             last_ok_at = excluded.last_ok_at,
             items_seen = items_seen + excluded.items_seen, last_error = NULL",
        rusqlite::params![started, started, (report.inserted + report.updated) as i64],
    )?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{upsert_identifier, upsert_node, Node};

    fn ev(kind: &str, repo: &str, at: &str, payload: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "type": kind,
            "repo": {"name": repo},
            "created_at": at,
            "payload": payload,
        })
    }

    #[test]
    fn test_summarize_and_aggregate() {
        // Payload shapes as OBSERVED from /users/{u}/events (slimmer than the
        // documented webhook shapes): PR events carry only the number; pushes
        // on private repos omit `commits`.
        let events = vec![
            ev(
                "PushEvent",
                "adalovelace/pkg",
                "2026-08-01T10:00:00Z",
                serde_json::json!({
                    "commits": [{"message": "Fix the thing\n\nlong body"}, {"message": "Add tests"}],
                    "size": 2, "ref": "refs/heads/main"
                }),
            ),
            ev(
                "PushEvent",
                "adalovelace/private",
                "2026-08-01T11:00:00Z",
                serde_json::json!({
                    "ref": "refs/heads/main", "head": "abc123", "before": "def456"
                }),
            ),
            ev(
                "PushEvent",
                "adalovelace/private",
                "2026-08-01T12:00:00Z",
                serde_json::json!({
                    "ref": "refs/heads/main", "head": "eee999", "before": "abc123"
                }),
            ),
            ev(
                "PullRequestEvent",
                "adalovelace/pkg",
                "2026-08-01T15:30:00Z",
                serde_json::json!({
                    "action": "closed", "number": 7,
                    "pull_request": {"url": "…", "id": 1, "number": 7}
                }),
            ),
            ev(
                "IssuesEvent",
                "sigmalab/sigtools",
                "2026-08-02T09:00:00Z",
                serde_json::json!({
                    "action": "opened", "issue": {"number": 42, "title": "NaN handling"}
                }),
            ),
            ev(
                "WatchEvent",
                "someone/repo",
                "2026-08-01T11:00:00Z",
                serde_json::json!({}),
            ),
        ];
        let mut enrich = Enrichment::default();
        enrich.pr.insert(
            ("adalovelace/pkg".into(), 7),
            ("Entity browser".into(), true),
        );
        enrich.push.insert(
            ("adalovelace/private".into(), "abc123".into()),
            (2, vec!["Wire the GTD form".into(), "Fix tests".into()]),
        );

        let agg = aggregate_events(&events, None, &enrich);
        assert_eq!(agg.len(), 3);
        let pkg_day = &agg[&("adalovelace/pkg".to_string(), "2026-08-01".to_string())];
        assert_eq!(pkg_day.len(), 2);
        assert!(pkg_day[0]
            .1
            .contains("pushed 2 commit(s): Fix the thing · Add tests"));
        // closed + merged (from enrichment) reads as merged, with title.
        assert!(pkg_day[1].1.contains("merged PR #7: Entity browser"));
        // Commit-less private pushes: compare-API enrichment supplies the
        // messages; unenriched ones degrade to branch-only.
        let priv_day = &agg[&("adalovelace/private".to_string(), "2026-08-01".to_string())];
        assert!(priv_day[0]
            .1
            .contains("pushed 2 commit(s): Wire the GTD form · Fix tests"));
        assert!(priv_day[1].1.contains("pushed to main"));
        // Without enrichment the PR line degrades to number-only.
        let bare = aggregate_events(&events, None, &Enrichment::default());
        let pkg_day = &bare[&("adalovelace/pkg".to_string(), "2026-08-01".to_string())];
        assert!(pkg_day[1].1.contains("closed PR #7"));

        // since drops strictly-older events (WatchEvent already noise-filtered).
        let agg = aggregate_events(&events, Some("2026-08-02 00:00:00"), &enrich);
        assert_eq!(agg.len(), 1);
        assert!(agg.contains_key(&("sigmalab/sigtools".to_string(), "2026-08-02".to_string())));
    }

    #[test]
    fn test_repo_bridges_to_sessions_project_only_when_unique() {
        let conn = open_memory().unwrap();
        // Sessions source already created a project for the local clone.
        upsert_node(&conn, &Node::new("project-abc", "project", "pkg")).unwrap();
        let clone = std::env::temp_dir()
            .join("pkg-test-home")
            .join("user")
            .join("Github")
            .join("pkg");
        upsert_identifier(
            &conn,
            "path",
            clone.to_str().unwrap(),
            "project-abc",
            "session",
        )
        .unwrap();

        let id = resolve_repo_project(&conn, "adalovelace/pkg").unwrap();
        assert_eq!(
            id, "project-abc",
            "unique path suffix joins the repo to the local project"
        );
        // Idempotent via the new github_repo identifier.
        assert_eq!(
            resolve_repo_project(&conn, "adalovelace/pkg").unwrap(),
            "project-abc"
        );

        // Ambiguous suffix (two 'demo' clones) → fresh node, no auto-join.
        upsert_node(&conn, &Node::new("project-d1", "project", "demo")).unwrap();
        upsert_identifier(&conn, "path", "/home/a/demo", "project-d1", "session").unwrap();
        upsert_node(&conn, &Node::new("project-d2", "project", "demo")).unwrap();
        upsert_identifier(&conn, "path", "/home/b/demo", "project-d2", "session").unwrap();
        let id = resolve_repo_project(&conn, "adalovelace/demo").unwrap();
        assert!(id.starts_with("project-") && id != "project-d1" && id != "project-d2");

        // No local project at all → fresh node named after the repo.
        let id = resolve_repo_project(&conn, "sigmalab/sigtools").unwrap();
        let node = crate::graph::get_node(&conn, &id).unwrap().unwrap();
        assert_eq!(node.name, "sigtools");
        assert_eq!(node.node_type, "project");
    }
}
