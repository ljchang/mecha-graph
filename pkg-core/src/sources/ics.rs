//! Calendar source via iCalendar files (§5.1 — the Rosetta Stone).
//!
//! Attendees carry `{email, displayName}` together — the only source that
//! does. Every attendee becomes: person node + email identifier + display-name
//! alias + mention + an `attended` fact on the event node. Those aliases are
//! what let the alias-scan tier resolve Bee's name-only mentions.
//!
//! Headless-friendly: point it at any exported or subscribed `.ics` (e.g. a
//! Google Calendar secret address fetched on a schedule). RRULE masters are
//! ingested as a single episode at DTSTART — recurrence is not expanded (v1).

use crate::episode::Episode;
use crate::error::Result;
use crate::sources::{ProposedLink, Source};
use std::path::PathBuf;

pub struct IcsSource {
    pub paths: Vec<PathBuf>,
    /// Emails treated as "me" — skipped for person-node creation.
    pub self_emails: Vec<String>,
}

impl IcsSource {
    pub fn new(paths: Vec<PathBuf>, self_emails: Vec<String>) -> Self {
        IcsSource { paths, self_emails }
    }
}

#[derive(Debug, Clone, Default)]
pub struct VEvent {
    pub uid: String,
    pub recurrence_id: Option<String>,
    pub summary: String,
    pub description: String,
    pub location: Option<String>,
    pub dtstart: Option<String>, // SQLite datetime (UTC where zoned)
    pub dtend: Option<String>,
    pub organizer: Option<(Option<String>, String)>, // (email, name)
    pub attendees: Vec<(Option<String>, String)>,    // (email, name)
}

/// Unfold RFC 5545 folded lines (continuations begin with space or tab).
fn unfold(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim_end_matches('\r');
        if (line.starts_with(' ') || line.starts_with('\t')) && !out.is_empty() {
            let last = out.last_mut().unwrap();
            last.push_str(&line[1..]);
        } else {
            out.push(line.to_string());
        }
    }
    out
}

/// Split "NAME;PARAM=x;PARAM=y:VALUE" into (name, params, value).
fn split_content_line(line: &str) -> Option<(String, Vec<(String, String)>, String)> {
    // The ':' separating name+params from value is the first ':' not inside
    // a double-quoted param value.
    let mut in_quotes = false;
    let mut colon = None;
    for (i, c) in line.char_indices() {
        match c {
            '"' => in_quotes = !in_quotes,
            ':' if !in_quotes => {
                colon = Some(i);
                break;
            }
            _ => {}
        }
    }
    let colon = colon?;
    let (head, value) = (&line[..colon], &line[colon + 1..]);
    let mut parts = head.split(';');
    let name = parts.next()?.to_uppercase();
    let params = parts
        .filter_map(|p| {
            let (k, v) = p.split_once('=')?;
            Some((k.to_uppercase(), v.trim_matches('"').to_string()))
        })
        .collect();
    Some((name, params, value.to_string()))
}

fn unescape(v: &str) -> String {
    v.replace("\\n", "\n")
        .replace("\\N", "\n")
        .replace("\\,", ",")
        .replace("\\;", ";")
        .replace("\\\\", "\\")
}

/// ICS datetime → SQLite datetime. Handles `...Z` (UTC), floating local, and
/// date-only values. TZID-zoned values are kept as-is (naive) — good enough
/// for interval joins at personal scale; refine if cross-TZ drift shows up.
fn ics_datetime(value: &str) -> Option<String> {
    let v = value.trim();
    if v.len() == 8 && v.chars().all(|c| c.is_ascii_digit()) {
        // VALUE=DATE (all-day)
        return Some(format!("{}-{}-{} 00:00:00", &v[..4], &v[4..6], &v[6..8]));
    }
    let (datepart, timepart, utc) = {
        let (d, t) = v.split_once('T')?;
        let utc = t.ends_with('Z');
        (d, t.trim_end_matches('Z'), utc)
    };
    if datepart.len() != 8 || timepart.len() < 6 {
        return None;
    }
    let s = format!(
        "{}-{}-{} {}:{}:{}",
        &datepart[..4],
        &datepart[4..6],
        &datepart[6..8],
        &timepart[..2],
        &timepart[2..4],
        &timepart[4..6]
    );
    let _ = utc; // naive storage; see doc comment
    Some(s)
}

/// Parse mailto + CN from ATTENDEE/ORGANIZER.
fn parse_person(params: &[(String, String)], value: &str) -> (Option<String>, String) {
    let email = value
        .strip_prefix("mailto:")
        .or_else(|| value.strip_prefix("MAILTO:"))
        .map(|e| e.trim().to_lowercase());
    let name = params
        .iter()
        .find(|(k, _)| k == "CN")
        .map(|(_, v)| v.clone())
        .filter(|cn| !cn.contains('@')) // CN often repeats the email; useless as alias
        .unwrap_or_default();
    (email, name)
}

pub fn parse_ics(text: &str) -> Vec<VEvent> {
    let mut events = Vec::new();
    let mut current: Option<VEvent> = None;

    for line in unfold(text) {
        let Some((name, params, value)) = split_content_line(&line) else {
            continue;
        };
        match name.as_str() {
            "BEGIN" if value == "VEVENT" => current = Some(VEvent::default()),
            "END" if value == "VEVENT" => {
                if let Some(ev) = current.take() {
                    if !ev.uid.is_empty() && ev.dtstart.is_some() {
                        events.push(ev);
                    }
                }
            }
            _ => {
                let Some(ev) = current.as_mut() else { continue };
                match name.as_str() {
                    "UID" => ev.uid = value,
                    "RECURRENCE-ID" => ev.recurrence_id = ics_datetime(&value),
                    "SUMMARY" => ev.summary = unescape(&value),
                    "DESCRIPTION" => ev.description = unescape(&value),
                    "LOCATION" => {
                        let loc = unescape(&value);
                        if !loc.is_empty() {
                            ev.location = Some(loc);
                        }
                    }
                    "DTSTART" => ev.dtstart = ics_datetime(&value),
                    "DTEND" => ev.dtend = ics_datetime(&value),
                    "ORGANIZER" => ev.organizer = Some(parse_person(&params, &value)),
                    "ATTENDEE" => ev.attendees.push(parse_person(&params, &value)),
                    _ => {}
                }
            }
        }
    }
    events
}

fn event_to_episode(ev: &VEvent) -> Episode {
    let mut body = ev.summary.clone();
    if let Some(loc) = &ev.location {
        body.push_str(&format!("\nLocation: {loc}"));
    }
    if !ev.attendees.is_empty() {
        let names: Vec<String> = ev
            .attendees
            .iter()
            .map(|(e, n)| {
                if n.is_empty() {
                    e.clone().unwrap_or_default()
                } else {
                    n.clone()
                }
            })
            .filter(|s| !s.is_empty())
            .collect();
        body.push_str(&format!("\nAttendees: {}", names.join(", ")));
    }
    if !ev.description.is_empty() {
        // Descriptions of recurring invites often carry huge boilerplate
        // (zoom dial-ins). Keep a bounded slice.
        let desc: String = ev.description.chars().take(2000).collect();
        body.push_str(&format!("\n\n{desc}"));
    }

    let source_id = match &ev.recurrence_id {
        Some(r) => format!("{}#{}", ev.uid, r),
        None => ev.uid.clone(),
    };

    Episode {
        id: 0,
        uid: String::new(),
        source: "calendar.event".into(),
        source_id,
        source_ref: None,
        body,
        occurred_at: ev.dtstart.clone().unwrap_or_default(),
        occurred_end: ev.dtend.clone(),
        ingested_at: String::new(),
        lat: None,
        lon: None,
        location: ev.location.clone(),
        sensitivity: "personal".into(),
        scope_id: None,
        meta: Some(serde_json::json!({ "summary": ev.summary })),
        raw: None, // regenerable from the ICS URL — nothing to capture
    }
}

impl Source for IcsSource {
    fn id(&self) -> &'static str {
        "calendar"
    }

    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>> {
        let mut out = Vec::new();
        for path in &self.paths {
            let text = std::fs::read_to_string(path)?;
            for ev in parse_ics(&text) {
                let ep = event_to_episode(&ev);
                if let Some(s) = since {
                    if ep.occurred_at.as_str() <= s {
                        continue;
                    }
                }
                out.push(ep);
            }
        }
        Ok(out)
    }

    /// Calendar attendees are the identity bridge: {email, displayName}
    /// pairs, asserted deterministically (Tier 1).
    fn deterministic_links(&self, ep: &Episode) -> Vec<ProposedLink> {
        // Re-parse attendees out of the body's "Attendees:" line is lossy;
        // instead the ingest driver calls fetch() → we stored attendees in
        // meta? Simpler: parse from body meta is fragile, so we re-derive from
        // the raw file at fetch time. To keep the trait signature, attendees
        // are re-parsed here from the stored body line.
        let mut links = Vec::new();
        for line in ep.body.lines() {
            if let Some(rest) = line.strip_prefix("Attendees: ") {
                for name in rest.split(", ") {
                    let name = name.trim();
                    if name.is_empty() {
                        continue;
                    }
                    let looks_email = name.contains('@');
                    let email = looks_email.then(|| name.to_lowercase());
                    if let Some(ref e) = email {
                        if self.self_emails.iter().any(|s| s.eq_ignore_ascii_case(e)) {
                            continue;
                        }
                    }
                    links.push(ProposedLink::Person {
                        email,
                        phone: None,
                        display_name: if looks_email {
                            String::new()
                        } else {
                            name.to_string()
                        },
                        fact: None,
                    });
                }
            }
        }
        links
    }
}

/// Full-fidelity ingest for ICS: unlike the generic driver, this variant keeps
/// the parsed attendee (email AND name together) — use this instead of
/// `sources::ingest` for calendars.
pub fn ingest_ics(
    conn: &rusqlite::Connection,
    src: &IcsSource,
    since: Option<&str>,
) -> Result<crate::sources::IngestReport> {
    let mut texts = Vec::new();
    for path in &src.paths {
        texts.push(std::fs::read_to_string(path)?);
    }
    ingest_ics_texts(conn, &texts, &src.self_emails, since)
}

/// Streaming variant: ingest ICS content already in memory (fetched from a
/// secret URL) — no plaintext cache file ever touches disk.
pub fn ingest_ics_text(
    conn: &rusqlite::Connection,
    text: &str,
    self_emails: &[String],
    since: Option<&str>,
) -> Result<crate::sources::IngestReport> {
    ingest_ics_texts(
        conn,
        std::slice::from_ref(&text.to_string()),
        self_emails,
        since,
    )
}

fn ingest_ics_texts(
    conn: &rusqlite::Connection,
    texts: &[String],
    self_emails: &[String],
    since: Option<&str>,
) -> Result<crate::sources::IngestReport> {
    use crate::episode::{add_mention, upsert_episode, IngestOutcome};
    use crate::graph;
    use crate::rollup;

    let started = crate::ids::now();
    let mut report = crate::sources::IngestReport::default();
    let mut max_occurred: Option<String> = since.map(|s| s.to_string());

    for text in texts {
        for ev in parse_ics(text) {
            let ep = event_to_episode(&ev);
            if let Some(s) = since {
                if ep.occurred_at.as_str() <= s {
                    continue;
                }
            }
            let (episode_id, outcome) = upsert_episode(conn, &ep)?;
            match outcome {
                IngestOutcome::Inserted => report.inserted += 1,
                IngestOutcome::Updated => report.updated += 1,
                IngestOutcome::Unchanged => {
                    report.unchanged += 1;
                    continue;
                }
                IngestOutcome::Tombstoned => {
                    report.tombstoned += 1;
                    continue;
                }
            }
            let ep_uid: String = conn.query_row(
                "SELECT uid FROM episode WHERE id = ?1",
                rusqlite::params![episode_id],
                |r| r.get(0),
            )?;

            // Event node, so tasks/facts can attach (discussed_at → Event).
            let event_node_id = format!(
                "event-{}",
                crate::ids::content_hash(&ep.source_id)[..16].to_string()
            );
            let mut event_node = graph::Node::new(
                &event_node_id,
                "event",
                if ev.summary.is_empty() {
                    "(untitled event)"
                } else {
                    &ev.summary
                },
            );
            event_node.source = "calendar".into();
            event_node.source_ref = Some(ep_uid.clone());
            graph::upsert_node(conn, &event_node)?;
            add_mention(conn, episode_id, &event_node_id, "attendee", 1.0)?;

            // Attendees: the {email, displayName} Rosetta Stone (§5.1).
            let mut people = ev.attendees.clone();
            if let Some(org) = &ev.organizer {
                people.push(org.clone());
            }
            for (email, name) in people {
                if let Some(ref e) = email {
                    if self_emails.iter().any(|s| s.eq_ignore_ascii_case(e)) {
                        continue;
                    }
                }
                if email.is_none() && name.is_empty() {
                    continue;
                }
                let person =
                    graph::get_or_create_person(conn, email.as_deref(), &name, "calendar")?;
                add_mention(conn, episode_id, &person.id, "attendee", 1.0)?;
                rollup::touch_person(conn, &person.id, &ep_uid, "calendar.event", &ep.occurred_at)?;
                crate::fact::assert_fact(
                    conn,
                    &person.id,
                    "attended",
                    Some(&event_node_id),
                    None,
                    &format!(
                        "{} attended \"{}\" on {}",
                        person.name, ev.summary, ep.occurred_at
                    ),
                    Some(episode_id),
                    Some(&ep.occurred_at),
                    0.9,
                    "attendee",
                )?;
                report.mentions += 1;
            }

            if max_occurred
                .as_deref()
                .map_or(true, |m| ep.occurred_at.as_str() > m)
            {
                max_occurred = Some(ep.occurred_at.clone());
            }
        }
    }

    conn.execute(
        "INSERT INTO ingest_state (source, cursor, last_run_at, last_ok_at, items_seen, last_error)
         VALUES ('calendar', ?1, ?2, ?2, ?3, NULL)
         ON CONFLICT(source) DO UPDATE SET
             cursor = COALESCE(excluded.cursor, cursor), last_run_at = excluded.last_run_at,
             last_ok_at = excluded.last_ok_at,
             items_seen = items_seen + excluded.items_seen, last_error = NULL",
        rusqlite::params![
            max_occurred,
            started,
            (report.inserted + report.updated) as i64
        ],
    )?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;

    const SAMPLE_ICS: &str = "BEGIN:VCALENDAR\r\nVERSION:2.0\r\nBEGIN:VEVENT\r\nUID:evt-123@google.com\r\nDTSTART:20260730T140000Z\r\nDTEND:20260730T150000Z\r\nSUMMARY:Meeting with Nadia\r\nLOCATION:Alder Hall 254\r\nORGANIZER;CN=Ada Lovelace:mailto:ada.lovelace@example.edu\r\nATTENDEE;CN=Nadia Petrova;PARTSTAT=ACCEPTED:mailto:nadia@example.edu\r\nATTENDEE;CN=June Chen;PARTSTAT=ACC\r\n EPTED:mailto:june.chen@example.edu\r\nDESCRIPTION:Pilot data review\\, then Aim 2 planning.\r\nEND:VEVENT\r\nEND:VCALENDAR\r\n";

    #[test]
    fn test_parse_ics_folded_lines_and_attendees() {
        let events = parse_ics(SAMPLE_ICS);
        assert_eq!(events.len(), 1);
        let ev = &events[0];
        assert_eq!(ev.uid, "evt-123@google.com");
        assert_eq!(ev.summary, "Meeting with Nadia");
        assert_eq!(ev.dtstart.as_deref(), Some("2026-07-30 14:00:00"));
        assert_eq!(ev.attendees.len(), 2);
        // Folded ATTENDEE line must still parse.
        assert_eq!(
            ev.attendees[1].0.as_deref(),
            Some("june.chen@example.edu")
        );
        assert_eq!(ev.attendees[1].1, "June Chen");
        assert_eq!(ev.description, "Pilot data review, then Aim 2 planning.");
    }

    #[test]
    fn test_ingest_ics_seeds_identity_bridge() {
        let conn = open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let ics_path = dir.path().join("cal.ics");
        std::fs::write(&ics_path, SAMPLE_ICS).unwrap();

        let src = IcsSource::new(vec![ics_path], vec!["ada.lovelace@example.edu".into()]);
        let report = ingest_ics(&conn, &src, None).unwrap();
        assert_eq!(report.inserted, 1);
        assert_eq!(report.mentions, 2, "self is excluded");

        // Identity bridge: email-keyed person with display-name alias.
        let nadia = crate::graph::get_node_by_identifier(&conn, "email", "nadia@example.edu")
            .unwrap()
            .expect("nadia exists");
        assert!(nadia.aliases.contains(&"nadia petrova".to_string()));

        // attended fact landed.
        let facts = crate::fact::facts_for_node(&conn, &nadia.id, 10).unwrap();
        assert!(facts.iter().any(|f| f.predicate == "attended"));

        // Rollup has last_meeting_at.
        let pi = crate::rollup::get_person_interaction(&conn, &nadia.id)
            .unwrap()
            .unwrap();
        assert_eq!(pi.last_meeting_at.as_deref(), Some("2026-07-30 14:00:00"));

        // Re-ingest is a no-op.
        let again = ingest_ics(&conn, &src, None).unwrap();
        assert_eq!(again.inserted, 0);
        assert_eq!(again.unchanged, 1);
    }
}
