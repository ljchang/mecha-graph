//! Slack source (§5.1, ★★★★☆): user ID + email from the API + display name.
//!
//! Identity flow: `users.list` maps every workspace member to a person node
//! with `slack_uid` AND `email` identifiers — so Slack people merge
//! deterministically with calendar/email people. Messages are segmented per
//! channel per day (§5.3: semantic units, not individual messages).
//!
//! Auth: a user token (`xoxp-…`) sees your DMs; a bot token (`xoxb-…`) only
//! sees channels it's invited to. Scopes needed: `channels:history`,
//! `groups:history`, `im:history`, `mpim:history`, `users:read`,
//! `users:read.email`, `channels:read`, `groups:read`, `im:read`, `mpim:read`.
//! DMs land as sensitivity 'private' (§10); channels as 'personal'.

use crate::episode::Episode;
use crate::error::{Error, Result};
use crate::integrations::SourceConfig;
use crate::sources::IngestReport;
use rusqlite::Connection;
use std::collections::{BTreeMap, HashMap};

pub struct SlackSource {
    pub token: String,
    /// Max channels touched per sync (rate-limit friendliness).
    pub max_channels: usize,
    /// Max history pages per channel per sync.
    pub max_pages: usize,
}

impl SlackSource {
    pub fn from_config(cfg: &SourceConfig) -> Result<Self> {
        Ok(SlackSource {
            token: cfg
                .get_str("token")
                .ok_or_else(|| Error::Other("slack source needs 'token'".into()))?
                .to_string(),
            max_channels: cfg
                .settings
                .get("max_channels")
                .and_then(|v| v.as_integer())
                .unwrap_or(50) as usize,
            max_pages: cfg
                .settings
                .get("max_pages")
                .and_then(|v| v.as_integer())
                .unwrap_or(5) as usize,
        })
    }
}

fn slack_get(token: &str, method: &str, params: &[(&str, &str)]) -> Result<serde_json::Value> {
    let mut req = ureq::get(&format!("https://slack.com/api/{method}"))
        .set("Authorization", &format!("Bearer {token}"))
        .timeout(std::time::Duration::from_secs(30));
    for (k, v) in params {
        req = req.query(k, v);
    }
    let body: serde_json::Value = req
        .call()
        .map_err(|e| Error::Other(format!("slack {method}: {e}")))?
        .into_json()
        .map_err(|e| Error::Other(format!("slack {method}: bad json: {e}")))?;
    if body["ok"].as_bool() != Some(true) {
        return Err(Error::Other(format!(
            "slack {method}: {}",
            body["error"].as_str().unwrap_or("unknown error")
        )));
    }
    Ok(body)
}

/// Validate a token; returns (team, user).
pub fn auth_test(token: &str) -> Result<(String, String)> {
    let body = slack_get(token, "auth.test", &[])?;
    Ok((
        body["team"].as_str().unwrap_or("?").to_string(),
        body["user"].as_str().unwrap_or("?").to_string(),
    ))
}

#[derive(Debug, Clone)]
pub struct SlackUser {
    pub id: String,
    pub real_name: String,
    pub email: Option<String>,
    pub is_bot: bool,
}

fn list_users(token: &str) -> Result<Vec<SlackUser>> {
    let mut users = Vec::new();
    let mut cursor = String::new();
    loop {
        let mut params = vec![("limit", "200")];
        if !cursor.is_empty() {
            params.push(("cursor", &cursor));
        }
        let body = slack_get(token, "users.list", &params)?;
        for m in body["members"].as_array().unwrap_or(&vec![]) {
            users.push(SlackUser {
                id: m["id"].as_str().unwrap_or_default().to_string(),
                real_name: m["profile"]["real_name"]
                    .as_str()
                    .or(m["real_name"].as_str())
                    .unwrap_or_default()
                    .to_string(),
                email: m["profile"]["email"].as_str().map(|s| s.to_string()),
                is_bot: m["is_bot"].as_bool().unwrap_or(false)
                    || m["id"].as_str() == Some("USLACKBOT"),
            });
        }
        cursor = body["response_metadata"]["next_cursor"]
            .as_str()
            .unwrap_or("")
            .to_string();
        if cursor.is_empty() {
            break;
        }
    }
    Ok(users)
}

/// Seed/refresh person nodes from the workspace directory. Deterministic
/// Tier-1: slack_uid + email identifiers on the same node.
pub fn sync_identities(conn: &Connection, token: &str, self_uid: &str) -> Result<usize> {
    let mut n = 0;
    for u in list_users(token)? {
        if u.is_bot || u.id == self_uid || u.real_name.is_empty() {
            continue;
        }
        let person =
            crate::graph::get_or_create_person(conn, u.email.as_deref(), &u.real_name, "slack")?;
        crate::graph::upsert_identifier(conn, "slack_uid", &u.id, &person.id, "slack")?;
        n += 1;
    }
    Ok(n)
}

/// Slack-specific ingest (needs multi-call API access, so it doesn't fit the
/// simple `Source::fetch` shape — same pattern as `ingest_ics`).
pub fn ingest_slack(
    conn: &Connection,
    src: &SlackSource,
    since: Option<&str>,
) -> Result<IngestReport> {
    let started = crate::ids::now();
    let mut report = IngestReport::default();

    let auth = slack_get(&src.token, "auth.test", &[])?;
    let self_uid = auth["user_id"].as_str().unwrap_or_default().to_string();

    // 1. Identity directory (the valuable part even with zero messages).
    sync_identities(conn, &src.token, &self_uid)?;

    // uid → node lookup for message attribution.
    let mut uid_to_node: HashMap<String, (String, String)> = HashMap::new(); // uid → (node_id, name)
    {
        let mut stmt = conn.prepare(
            "SELECT ni.value, n.id, n.name FROM node_identifier ni
             JOIN nodes n ON n.id = ni.node_id WHERE ni.kind = 'slack_uid'",
        )?;
        let rows: Vec<(String, String, String)> = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for (uid, node_id, name) in rows {
            uid_to_node.insert(uid, (node_id, name));
        }
    }

    // 2. Channels the token can see.
    let body = slack_get(
        &src.token,
        "conversations.list",
        &[
            ("types", "public_channel,private_channel,im,mpim"),
            ("exclude_archived", "true"),
            ("limit", "200"),
        ],
    )?;
    let channels: Vec<(String, String, bool)> = body["channels"]
        .as_array()
        .unwrap_or(&vec![])
        .iter()
        .filter(|c| {
            c["is_member"].as_bool().unwrap_or(false) || c["is_im"].as_bool().unwrap_or(false)
        })
        .map(|c| {
            let is_im =
                c["is_im"].as_bool().unwrap_or(false) || c["is_mpim"].as_bool().unwrap_or(false);
            let name = if is_im {
                c["user"].as_str().unwrap_or("dm").to_string()
            } else {
                c["name"].as_str().unwrap_or("?").to_string()
            };
            (
                c["id"].as_str().unwrap_or_default().to_string(),
                name,
                is_im,
            )
        })
        .take(src.max_channels)
        .collect();

    // since (SQLite datetime) → Slack ts.
    let oldest = since
        .and_then(|s| {
            chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S")
                .ok()
                .map(|dt| format!("{}", dt.and_utc().timestamp()))
        })
        .unwrap_or_else(|| "0".to_string());

    for (channel_id, channel_name, is_im) in channels {
        // 3. History, grouped per day (§5.3 semantic segmentation).
        let mut by_day: BTreeMap<String, Vec<(String, String, String)>> = BTreeMap::new(); // day → (ts, uid, text)
        let mut cursor = String::new();
        for _page in 0..src.max_pages {
            let mut params = vec![
                ("channel", channel_id.as_str()),
                ("oldest", oldest.as_str()),
                ("limit", "200"),
            ];
            if !cursor.is_empty() {
                params.push(("cursor", &cursor));
            }
            let hist = match slack_get(&src.token, "conversations.history", &params) {
                Ok(h) => h,
                Err(e) => {
                    eprintln!("slack: {channel_name}: {e}");
                    break;
                }
            };
            for m in hist["messages"].as_array().unwrap_or(&vec![]) {
                if m["subtype"].is_string() {
                    continue; // joins, topic changes, bot noise
                }
                let (Some(ts), Some(user), Some(text)) =
                    (m["ts"].as_str(), m["user"].as_str(), m["text"].as_str())
                else {
                    continue;
                };
                if text.trim().is_empty() {
                    continue;
                }
                let epoch: f64 = ts.parse().unwrap_or(0.0);
                let dt = chrono::DateTime::from_timestamp(epoch as i64, 0)
                    .unwrap_or_default()
                    .naive_utc();
                by_day
                    .entry(dt.format("%Y-%m-%d").to_string())
                    .or_default()
                    .push((
                        dt.format("%Y-%m-%d %H:%M:%S").to_string(),
                        user.to_string(),
                        text.to_string(),
                    ));
            }
            cursor = hist["response_metadata"]["next_cursor"]
                .as_str()
                .unwrap_or("")
                .to_string();
            if cursor.is_empty() {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(1200)); // tier-3 rate limit
        }

        for (day, mut msgs) in by_day {
            msgs.sort();
            let first = msgs.first().unwrap().0.clone();
            let last = msgs.last().unwrap().0.clone();
            let mut participants: BTreeMap<String, String> = BTreeMap::new(); // node_id → name
            let mut lines = Vec::new();
            for (ts, uid, text) in &msgs {
                let who = match uid_to_node.get(uid) {
                    Some((node_id, name)) => {
                        if *uid != self_uid {
                            participants.insert(node_id.clone(), name.clone());
                        }
                        name.clone()
                    }
                    None => uid.clone(),
                };
                let text: String = text.chars().take(500).collect();
                lines.push(format!("[{}] {who}: {text}", &ts[11..16]));
            }
            let body = format!(
                "#{channel_name} — {day}\n{}",
                lines.join("\n").chars().take(8000).collect::<String>()
            );

            let ep = Episode {
                id: 0,
                uid: String::new(),
                source: "slack.thread".into(),
                source_id: format!("{channel_id}:{day}"),
                source_ref: Some(format!("slack://channel/{channel_id}")),
                body,
                occurred_at: first,
                occurred_end: Some(last),
                ingested_at: String::new(),
                lat: None,
                lon: None,
                location: None,
                sensitivity: if is_im { "private" } else { "personal" }.into(),
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
            let ep_uid: String = conn.query_row(
                "SELECT uid FROM episode WHERE id = ?1",
                rusqlite::params![episode_id],
                |r| r.get(0),
            )?;
            for (node_id, _name) in participants {
                crate::episode::add_mention(conn, episode_id, &node_id, "attendee", 1.0)?;
                crate::rollup::touch_person(
                    conn,
                    &node_id,
                    &ep_uid,
                    "slack.thread",
                    &ep.occurred_at,
                )?;
                report.mentions += 1;
            }
        }
    }

    conn.execute(
        "INSERT INTO ingest_state (source, cursor, last_run_at, last_ok_at, items_seen, last_error)
         VALUES ('slack', ?1, ?2, ?2, ?3, NULL)
         ON CONFLICT(source) DO UPDATE SET
             cursor = COALESCE(excluded.cursor, cursor), last_run_at = excluded.last_run_at,
             last_ok_at = excluded.last_ok_at,
             items_seen = items_seen + excluded.items_seen, last_error = NULL",
        rusqlite::params![started, started, (report.inserted + report.updated) as i64],
    )?;
    Ok(report)
}
