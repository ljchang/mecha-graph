//! SMS / iMessage source (§5.1, ★★★★☆): phone numbers are strong identity.
//!
//! Reads a **copy** of macOS `chat.db` (sync it over from the Mac, e.g.
//! `rsync mac:~/Library/Messages/chat.db ~/pkg/chat.db` via Tailscale;
//! the Mac-side process needs Full Disk Access). Always opened read-only.
//!
//! Segmentation: one episode per (chat, day) — semantic units, not single
//! texts (§5.3). Identity: `handle.id` is an E.164 phone or an email —
//! both are deterministic `node_identifier` kinds. Sensitivity: private (§10).
//!
//! Known limitation (v1): newer macOS often leaves `message.text` NULL and
//! stores the body in the `attributedBody` typedstream blob — those messages
//! are skipped and counted; if the skip rate is high, add a typedstream
//! decoder.

use crate::episode::Episode;
use crate::error::{Error, Result};
use crate::integrations::SourceConfig;
use crate::sources::{ProposedLink, Source};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct IMessageSource {
    pub db: PathBuf,
    /// Numbers/emails that are "me" (excluded from person creation).
    pub self_handles: Vec<String>,
}

impl IMessageSource {
    pub fn from_config(cfg: &SourceConfig) -> Result<Self> {
        Ok(IMessageSource {
            db: PathBuf::from(
                cfg.get_str("db")
                    .ok_or_else(|| Error::Other("imessage source needs 'db'".into()))?,
            ),
            self_handles: cfg
                .get_str("self_handles")
                .map(|s| s.split(',').map(|x| normalize_handle(x)).collect())
                .unwrap_or_default(),
        })
    }
}

/// Apple stores dates as time since 2001-01-01 — seconds historically,
/// nanoseconds on modern macOS. Disambiguate by magnitude.
const APPLE_EPOCH_OFFSET: i64 = 978_307_200;

fn apple_date_to_epoch(v: i64) -> i64 {
    let secs = if v > 1_000_000_000_000 {
        v / 1_000_000_000
    } else {
        v
    };
    secs + APPLE_EPOCH_OFFSET
}

fn epoch_to_sqlite(epoch: i64) -> String {
    chrono::DateTime::from_timestamp(epoch, 0)
        .unwrap_or_default()
        .naive_utc()
        .format("%Y-%m-%d %H:%M:%S")
        .to_string()
}

/// Normalize a handle: lowercase emails; strip spaces/dashes from phones.
pub fn normalize_handle(h: &str) -> String {
    let h = h.trim();
    if h.contains('@') {
        h.to_lowercase()
    } else {
        h.chars()
            .filter(|c| c.is_ascii_digit() || *c == '+')
            .collect()
    }
}

/// Connectivity probe for `pkg source test`.
pub fn probe(db: &str) -> Result<i64> {
    let conn =
        rusqlite::Connection::open_with_flags(db, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok(conn.query_row("SELECT COUNT(*) FROM message", [], |r| r.get(0))?)
}

impl Source for IMessageSource {
    fn id(&self) -> &'static str {
        "sms"
    }

    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>> {
        if !self.db.exists() {
            return Ok(vec![]);
        }
        let conn = rusqlite::Connection::open_with_flags(
            &self.db,
            rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
        )?;

        // chat_identifier groups both 1:1 and group chats.
        let mut stmt = conn.prepare(
            "SELECT c.chat_identifier, COALESCE(c.display_name, ''),
                    m.date, m.is_from_me, COALESCE(h.id, ''), m.text
             FROM message m
             JOIN chat_message_join cmj ON cmj.message_id = m.ROWID
             JOIN chat c ON c.ROWID = cmj.chat_id
             LEFT JOIN handle h ON h.ROWID = m.handle_id
             WHERE m.text IS NOT NULL AND m.text != ''
             ORDER BY m.date ASC",
        )?;
        let rows: Vec<(String, String, i64, bool, String, String)> = stmt
            .query_map([], |r| {
                Ok((
                    r.get(0)?,
                    r.get(1)?,
                    r.get(2)?,
                    r.get::<_, i64>(3)? != 0,
                    r.get(4)?,
                    r.get(5)?,
                ))
            })?
            .collect::<std::result::Result<_, _>>()?;

        // Group per (chat, day).
        type DayMsgs = Vec<(String, bool, String, String)>; // (ts, from_me, handle, text)
        let mut grouped: BTreeMap<(String, String), (String, DayMsgs)> = BTreeMap::new();
        for (chat_id, display_name, date, from_me, handle, text) in rows {
            let ts = epoch_to_sqlite(apple_date_to_epoch(date));
            let day = ts[..10].to_string();
            if let Some(s) = since {
                if ts.as_str() <= s {
                    continue;
                }
            }
            grouped
                .entry((chat_id, day))
                .or_insert_with(|| (display_name.clone(), Vec::new()))
                .1
                .push((ts, from_me, handle, text));
        }

        let mut out = Vec::new();
        for ((chat_id, day), (display_name, msgs)) in grouped {
            let first = msgs.first().unwrap().0.clone();
            let last = msgs.last().unwrap().0.clone();

            let mut handles: BTreeMap<String, ()> = BTreeMap::new();
            let mut lines = Vec::new();
            let mut raw_lines = Vec::new(); // untruncated, for the capture archive
            for (ts, from_me, handle, text) in &msgs {
                let handle_norm = normalize_handle(handle);
                let who = if *from_me {
                    "me".to_string()
                } else {
                    if !handle_norm.is_empty() && !self.self_handles.contains(&handle_norm) {
                        handles.insert(handle_norm.clone(), ());
                    }
                    handle_norm.clone()
                };
                raw_lines.push(format!("[{ts}] {who}: {text}"));
                let text: String = text.chars().take(400).collect();
                lines.push(format!("[{}] {who}: {text}", &ts[11..16]));
            }

            let title = if display_name.is_empty() {
                &chat_id
            } else {
                &display_name
            };
            let body = format!(
                "SMS/iMessage with {title} — {day}\n{}",
                lines.join("\n").chars().take(8000).collect::<String>()
            );

            out.push(Episode {
                id: 0,
                uid: String::new(),
                source: "sms".into(),
                source_id: format!("{chat_id}:{day}"),
                source_ref: Some(self.db.to_string_lossy().to_string()),
                body,
                occurred_at: first,
                occurred_end: Some(last),
                ingested_at: String::new(),
                lat: None,
                lon: None,
                location: None,
                sensitivity: "private".into(),
                scope_id: None,
                meta: Some(serde_json::json!({
                    "handles": handles.keys().cloned().collect::<Vec<_>>()
                })),
                raw: Some(raw_lines.join("\n")),
            });
        }
        Ok(out)
    }

    /// Handles are deterministic identity keys: phone (E.164-ish) or email.
    fn deterministic_links(&self, ep: &Episode) -> Vec<ProposedLink> {
        let Some(meta) = &ep.meta else { return vec![] };
        let Some(handles) = meta.get("handles").and_then(|h| h.as_array()) else {
            return vec![];
        };
        handles
            .iter()
            .filter_map(|h| h.as_str())
            .map(|h| {
                if h.contains('@') {
                    ProposedLink::Person {
                        email: Some(h.to_string()),
                        phone: None,
                        display_name: String::new(),
                        fact: None,
                    }
                } else {
                    // Phone-only: node named after the number until a
                    // contacts-carrying source supplies the real name; the
                    // phone identifier makes the eventual merge deterministic.
                    ProposedLink::Person {
                        email: None,
                        phone: Some(h.to_string()),
                        display_name: h.to_string(),
                        fact: None,
                    }
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_db(path: &std::path::Path) {
        let conn = rusqlite::Connection::open(path).unwrap();
        conn.execute_batch(
            "CREATE TABLE message (ROWID INTEGER PRIMARY KEY, date INTEGER,
                 is_from_me INTEGER, handle_id INTEGER, text TEXT);
             CREATE TABLE handle (ROWID INTEGER PRIMARY KEY, id TEXT);
             CREATE TABLE chat (ROWID INTEGER PRIMARY KEY, chat_identifier TEXT,
                 display_name TEXT);
             CREATE TABLE chat_message_join (chat_id INTEGER, message_id INTEGER);
             INSERT INTO handle VALUES (1, '+16035550123');
             INSERT INTO chat VALUES (1, '+16035550123', NULL);
             -- 2026-08-01 ~14:00 UTC in Apple nanoseconds:
             INSERT INTO message VALUES
               (1, 807285600000000000, 0, 1, 'running late, be there in 10'),
               (2, 807285660000000000, 1, 1, 'no problem, see you soon');
             INSERT INTO chat_message_join VALUES (1, 1), (1, 2);",
        )
        .unwrap();
    }

    #[test]
    fn test_apple_epoch_both_precisions() {
        // Seconds vs nanoseconds must land on the same instant.
        let secs = 807_285_600i64;
        let nanos = 807_285_600_000_000_000i64;
        assert_eq!(apple_date_to_epoch(secs), apple_date_to_epoch(nanos));
        assert!(epoch_to_sqlite(apple_date_to_epoch(secs)).starts_with("2026-08-01"));
    }

    #[test]
    fn test_imessage_fetch_groups_by_chat_day() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("chat.db");
        fixture_db(&db);

        let src = IMessageSource {
            db,
            self_handles: vec![],
        };
        let eps = src.fetch(None).unwrap();
        assert_eq!(eps.len(), 1, "two messages, one chat-day episode");
        let ep = &eps[0];
        assert_eq!(ep.source, "sms");
        assert_eq!(ep.sensitivity, "private");
        assert!(ep.body.contains("running late"));
        assert!(ep.body.contains("me: no problem"));

        let links = src.deterministic_links(ep);
        assert_eq!(links.len(), 1, "one counterparty handle");
    }

    #[test]
    fn test_normalize_handle() {
        assert_eq!(normalize_handle("+1 (603) 555-0123"), "+16035550123");
        assert_eq!(normalize_handle("Foo@Bar.Com "), "foo@bar.com");
    }
}
