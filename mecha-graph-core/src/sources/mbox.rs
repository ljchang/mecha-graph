//! Email source via mbox files (§5.1, ★★★★☆): headless-friendly email
//! ingestion — Gmail Takeout, `git format-patch`-style archives, or any
//! mbox export. (Live OAuth sync stays FlowMail's job on macOS; this path
//! needs no new credentials.)
//!
//! Segmentation: one episode per **thread** (§5.3), keyed by the root
//! Message-ID from References/In-Reply-To, falling back to normalized
//! subject. Identity: From/To/Cc addresses + display names — deterministic.
//!
//! Bulk-mail filter (§5.3): messages with List-Unsubscribe / List-Id or
//! Precedence: bulk are dropped — newsletters would swamp the graph at
//! ~zero value.

use crate::episode::Episode;
use crate::error::{Error, Result};
use crate::integrations::SourceConfig;
use crate::sources::{ProposedLink, Source};
use mailparse::MailHeaderMap;
use std::collections::BTreeMap;
use std::path::PathBuf;

pub struct MboxSource {
    pub path: PathBuf,
    pub self_emails: Vec<String>,
}

impl MboxSource {
    pub fn from_config(cfg: &SourceConfig) -> Result<Self> {
        Ok(MboxSource {
            path: PathBuf::from(
                cfg.get_str("path")
                    .ok_or_else(|| Error::Other("mbox source needs 'path'".into()))?,
            ),
            self_emails: cfg
                .get_str("self_email")
                .map(|s| s.split(',').map(|x| x.trim().to_lowercase()).collect())
                .unwrap_or_default(),
        })
    }
}

#[derive(Debug)]
struct ParsedMail {
    #[allow(dead_code)]
    message_id: String,
    thread_root: String,
    subject: String,
    date: String,                // SQLite datetime
    from: Vec<(String, String)>, // (email, name)
    to_cc: Vec<(String, String)>,
    body: String,
    /// Original message source, verbatim — the capture-retention archive.
    raw_source: String,
    is_bulk: bool,
}

/// Split an mbox file into raw messages ("From " separator lines).
fn split_mbox(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let bytes = text.as_bytes();
    let mut pos = 0;
    for line in text.lines() {
        let line_start = pos;
        pos += line.len() + 1;
        if line.starts_with("From ") {
            if let Some(s) = start {
                out.push(&text[s..line_start]);
            }
            start = Some(line_start);
        }
    }
    if let Some(s) = start {
        out.push(&text[s..]);
    }
    let _ = bytes;
    out
}

fn parse_addresses(header: &str) -> Vec<(String, String)> {
    match mailparse::addrparse(header) {
        Ok(list) => list
            .iter()
            .flat_map(|addr| match addr {
                mailparse::MailAddr::Single(s) => vec![(
                    s.addr.to_lowercase(),
                    s.display_name.clone().unwrap_or_default(),
                )],
                mailparse::MailAddr::Group(g) => g
                    .addrs
                    .iter()
                    .map(|s| {
                        (
                            s.addr.to_lowercase(),
                            s.display_name.clone().unwrap_or_default(),
                        )
                    })
                    .collect(),
            })
            .collect(),
        Err(_) => vec![],
    }
}

fn normalize_subject(s: &str) -> String {
    let mut s = s.trim().to_lowercase();
    loop {
        let stripped = s
            .trim_start_matches("re:")
            .trim_start_matches("fwd:")
            .trim_start_matches("fw:")
            .trim_start();
        if stripped == s {
            break;
        }
        s = stripped.to_string();
    }
    s
}

fn parse_message(raw: &str) -> Option<ParsedMail> {
    // Skip the mbox "From " envelope line.
    let content = raw.split_once('\n').map(|(_, rest)| rest).unwrap_or(raw);
    let mail = mailparse::parse_mail(content.as_bytes()).ok()?;
    let headers = &mail.headers;

    let is_bulk = headers.get_first_value("List-Unsubscribe").is_some()
        || headers.get_first_value("List-Id").is_some()
        || headers
            .get_first_value("Precedence")
            .is_some_and(|p| p.eq_ignore_ascii_case("bulk"));

    let message_id = headers
        .get_first_value("Message-ID")
        .unwrap_or_default()
        .trim()
        .to_string();
    let references = headers.get_first_value("References").unwrap_or_default();
    let in_reply_to = headers.get_first_value("In-Reply-To").unwrap_or_default();
    let subject = headers.get_first_value("Subject").unwrap_or_default();

    // Thread root: first Message-ID in References, else In-Reply-To, else the
    // message's OWN Message-ID (that's what replies will reference), else a
    // subject key as the last resort.
    let thread_root = references
        .split_whitespace()
        .next()
        .filter(|s| !s.is_empty())
        .map(|s| s.to_string())
        .or_else(|| {
            let t = in_reply_to.trim();
            (!t.is_empty()).then(|| t.split_whitespace().next().unwrap_or(t).to_string())
        })
        .or_else(|| (!message_id.is_empty()).then(|| message_id.clone()))
        .unwrap_or_else(|| format!("subject:{}", normalize_subject(&subject)));

    let date = headers
        .get_first_value("Date")
        .and_then(|d| mailparse::dateparse(&d).ok())
        .map(|epoch| {
            chrono::DateTime::from_timestamp(epoch, 0)
                .unwrap_or_default()
                .naive_utc()
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })?;

    let from = parse_addresses(&headers.get_first_value("From").unwrap_or_default());
    let mut to_cc = parse_addresses(&headers.get_first_value("To").unwrap_or_default());
    to_cc.extend(parse_addresses(
        &headers.get_first_value("Cc").unwrap_or_default(),
    ));

    // First text/plain part, or the top-level body.
    fn text_body(m: &mailparse::ParsedMail) -> Option<String> {
        if m.subparts.is_empty() {
            if m.ctype.mimetype.starts_with("text/plain") || m.ctype.mimetype.is_empty() {
                return m.get_body().ok();
            }
            return None;
        }
        m.subparts.iter().find_map(text_body)
    }
    let body = text_body(&mail)
        .or_else(|| mail.get_body().ok())
        .unwrap_or_default();

    Some(ParsedMail {
        message_id,
        thread_root,
        subject,
        date,
        from,
        to_cc,
        body,
        raw_source: raw.to_string(),
        is_bulk,
    })
}

impl Source for MboxSource {
    fn id(&self) -> &'static str {
        "email.mbox"
    }

    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>> {
        let text = std::fs::read_to_string(&self.path)?;
        let mut threads: BTreeMap<String, Vec<ParsedMail>> = BTreeMap::new();
        for raw in split_mbox(&text) {
            let Some(m) = parse_message(raw) else {
                continue;
            };
            if m.is_bulk {
                continue; // §5.3 aggressive filter
            }
            threads.entry(m.thread_root.clone()).or_default().push(m);
        }

        let mut out = Vec::new();
        for (root, mut msgs) in threads {
            msgs.sort_by(|a, b| a.date.cmp(&b.date));
            let first = msgs.first().unwrap();
            let last = msgs.last().unwrap();
            let occurred_at = first.date.clone();
            if let Some(s) = since {
                // Thread-level cursor keyed on latest activity.
                if last.date.as_str() <= s {
                    continue;
                }
            }

            let subject = msgs
                .iter()
                .map(|m| m.subject.as_str())
                .find(|s| !s.is_empty())
                .unwrap_or("(no subject)");

            let mut people: BTreeMap<String, String> = BTreeMap::new(); // email → name
            let mut lines = Vec::new();
            for m in &msgs {
                for (email, name) in m.from.iter().chain(m.to_cc.iter()) {
                    if self.self_emails.contains(email) {
                        continue;
                    }
                    let entry = people.entry(email.clone()).or_default();
                    if entry.is_empty() && !name.is_empty() {
                        *entry = name.clone();
                    }
                }
                let sender = m
                    .from
                    .first()
                    .map(|(e, n)| if n.is_empty() { e.clone() } else { n.clone() })
                    .unwrap_or_default();
                // Drop quoted reply tails: keep lines until the quote block.
                let body: String = m
                    .body
                    .lines()
                    .take_while(|l| !l.starts_with('>') && !l.starts_with("On "))
                    .collect::<Vec<_>>()
                    .join("\n");
                let body: String = body.trim().chars().take(1500).collect();
                lines.push(format!("--- {} · {} ---\n{}", m.date, sender, body));
            }

            let body = format!(
                "Email thread: {subject}\n\n{}",
                lines.join("\n\n").chars().take(10000).collect::<String>()
            );

            out.push(Episode {
                id: 0,
                uid: String::new(),
                source: "email.thread".into(),
                source_id: root,
                source_ref: Some(self.path.to_string_lossy().to_string()),
                body,
                occurred_at,
                occurred_end: Some(last.date.clone()),
                ingested_at: String::new(),
                lat: None,
                lon: None,
                location: None,
                sensitivity: "personal".into(),
                scope_id: None,
                meta: Some(serde_json::json!({
                    "people": people
                        .iter()
                        .map(|(e, n)| serde_json::json!({"email": e, "name": n}))
                        .collect::<Vec<_>>(),
                    "messages": msgs.len(),
                })),
                raw: Some(
                    msgs.iter()
                        .map(|m| m.raw_source.as_str())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ),
            });
        }
        out.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at));
        Ok(out)
    }

    /// From/To/Cc are the classic deterministic identity keys (§5.1).
    fn deterministic_links(&self, ep: &Episode) -> Vec<ProposedLink> {
        let Some(meta) = &ep.meta else { return vec![] };
        let Some(people) = meta.get("people").and_then(|p| p.as_array()) else {
            return vec![];
        };
        people
            .iter()
            .filter_map(|p| {
                Some(ProposedLink::Person {
                    email: Some(p.get("email")?.as_str()?.to_string()),
                    phone: None,
                    display_name: p
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or_default()
                        .to_string(),
                    fact: None,
                })
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = "From ada@example.com Thu Jul 30 10:00:00 2026\n\
Message-ID: <a1@mail>\nDate: Thu, 30 Jul 2026 10:00:00 +0000\n\
From: Nadia Petrova <nadia@example.edu>\nTo: Ada Lovelace <ada.lovelace@example.edu>\n\
Subject: pilot data\n\nAttached the pilot data you asked about.\n\
From ada@example.com Thu Jul 30 11:00:00 2026\n\
Message-ID: <a2@mail>\nIn-Reply-To: <a1@mail>\nReferences: <a1@mail>\n\
Date: Thu, 30 Jul 2026 11:00:00 +0000\n\
From: Ada Lovelace <ada.lovelace@example.edu>\nTo: Nadia Petrova <nadia@example.edu>\n\
Subject: Re: pilot data\n\nThanks! Will review by Friday.\n\
From news@example.com Thu Jul 30 12:00:00 2026\n\
Message-ID: <n1@mail>\nDate: Thu, 30 Jul 2026 12:00:00 +0000\n\
From: Newsletter <news@spam.io>\nTo: ada.lovelace@example.edu\n\
List-Unsubscribe: <http://spam.io/u>\nSubject: WEEKLY DEALS\n\nBuy things.\n";

    fn write_sample(dir: &std::path::Path) -> PathBuf {
        let p = dir.join("mail.mbox");
        std::fs::write(&p, SAMPLE).unwrap();
        p
    }

    #[test]
    fn test_mbox_threads_and_bulk_filter() {
        let dir = tempfile::tempdir().unwrap();
        let src = MboxSource {
            path: write_sample(dir.path()),
            self_emails: vec!["ada.lovelace@example.edu".into()],
        };
        let eps = src.fetch(None).unwrap();
        assert_eq!(eps.len(), 1, "2 msgs → 1 thread; newsletter filtered");
        let ep = &eps[0];
        assert_eq!(ep.source, "email.thread");
        assert_eq!(ep.source_id, "<a1@mail>");
        assert!(ep.body.contains("pilot data"));
        assert!(ep.body.contains("Will review by Friday"));

        let links = src.deterministic_links(ep);
        assert_eq!(links.len(), 1, "self excluded, Nadia linked");
    }

    #[test]
    fn test_mbox_ingest_creates_email_identity() {
        let dir = tempfile::tempdir().unwrap();
        let conn = crate::db::open_memory().unwrap();
        let src = MboxSource {
            path: write_sample(dir.path()),
            self_emails: vec!["ada.lovelace@example.edu".into()],
        };
        let report = crate::sources::ingest(&conn, &src, None).unwrap();
        assert_eq!(report.inserted, 1);

        let nadia = crate::graph::get_node_by_identifier(&conn, "email", "nadia@example.edu")
            .unwrap()
            .expect("person from email headers");
        assert!(nadia.aliases.contains(&"nadia petrova".to_string()));

        // Rollup got last_email_at.
        let pi = crate::rollup::get_person_interaction(&conn, &nadia.id)
            .unwrap()
            .unwrap();
        assert!(pi.last_email_at.is_some());
    }
}
