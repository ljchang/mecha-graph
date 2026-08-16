//! Reflect app export → episodes. Reads the markdown zip DIRECTLY (never
//! extracts: Reflect uses note titles as filenames, which routinely exceed
//! the filesystem's 255-byte component limit).
//!
//! Layout observed (export v1, 2026-08):
//! - `daily-notes/YYYY-MM-DD.md` → `reflect.daily`, one episode per day,
//!   occurred_at at that date.
//! - `<Title>-<id>.md` → `reflect.note`, keyed by the trailing id (32-hex
//!   or slug) — titles change, ids don't. The export carries no per-note
//!   timestamps, so occurred_at is the export moment (topic notes are
//!   reference material; they surface via search and mentions, not the
//!   timeline).
//!
//! `[[Backlinks]]` become mentions when they resolve to exactly one node
//! (ambiguity never auto-links, §7); the alias scanner covers plain-text
//! name references. Raw markdown is archived per episode (`episode_raw`),
//! and the zip is deleted after every archive verifies — file-based
//! sources are capture_delete under the at-rest design (§10).

use crate::episode::{self, Episode};
use crate::error::{Error, Result};
use crate::sources::IngestReport;
use rusqlite::Connection;
use std::io::Read;
use std::path::Path;

fn slugify(s: &str) -> String {
    let mut out = String::new();
    for c in s.to_lowercase().chars() {
        if c.is_ascii_alphanumeric() {
            out.push(c);
        } else if !out.is_empty() && !out.ends_with('-') {
            out.push('-');
        }
    }
    out.trim_end_matches('-').to_string()
}

/// Split "<Title>-<stable id>.md" into (id, title). Reflect uses either a
/// 32-hex id or a slugified copy of the title; the slug contains hyphens,
/// so it is found by testing each split point against slugify(title).
fn note_key(name: &str) -> Option<(String, String)> {
    let stem = name.strip_suffix(".md")?;
    if let Some((title, id)) = stem.rsplit_once('-') {
        if id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit()) && !title.is_empty() {
            return Some((id.to_string(), title.to_string()));
        }
    }
    for (idx, _) in stem.match_indices('-') {
        let (title, rest) = (&stem[..idx], &stem[idx + 1..]);
        if !title.is_empty() && rest == slugify(title) {
            return Some((rest.to_string(), title.to_string()));
        }
    }
    // Unknown suffix shape: the whole stem is at least stable.
    Some((stem.to_string(), stem.to_string()))
}

fn ep(source: &str, source_id: &str, occurred_at: &str, body: String) -> Episode {
    Episode {
        id: 0,
        uid: String::new(),
        source: source.into(),
        source_id: source_id.into(),
        source_ref: None,
        body,
        occurred_at: occurred_at.into(),
        occurred_end: None,
        ingested_at: String::new(),
        lat: None,
        lon: None,
        location: None,
        sensitivity: "personal".into(),
        scope_id: None,
        meta: None,
        raw: None,
    }
}

/// `[[Name]]` targets in a note body.
fn backlinks(body: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = body;
    while let Some(start) = rest.find("[[") {
        rest = &rest[start + 2..];
        let Some(end) = rest.find("]]") else { break };
        let name = rest[..end].trim();
        if !name.is_empty() && name.len() < 120 {
            out.push(name.to_string());
        }
        rest = &rest[end + 2..];
    }
    out
}

pub fn ingest_zip(conn: &Connection, zip_path: &Path) -> Result<IngestReport> {
    let file = std::fs::File::open(zip_path)
        .map_err(|e| Error::Other(format!("cannot open {}: {e}", zip_path.display())))?;
    let mut archive =
        zip::ZipArchive::new(file).map_err(|e| Error::Other(format!("not a zip: {e}")))?;

    let export_stamp = crate::ids::now();
    let mut report = IngestReport::default();
    let mut episode_ids = Vec::new();

    for i in 0..archive.len() {
        let mut entry = archive
            .by_index(i)
            .map_err(|e| Error::Other(format!("zip entry {i}: {e}")))?;
        let name = entry.name().to_string();
        if !name.ends_with(".md") {
            continue;
        }
        let mut body = String::new();
        entry
            .read_to_string(&mut body)
            .map_err(|e| Error::Other(format!("read {name}: {e}")))?;
        if body.trim().is_empty() {
            continue;
        }

        let episode = if let Some(date) = name
            .strip_prefix("daily-notes/")
            .and_then(|f| f.strip_suffix(".md"))
        {
            if date.len() != 10 {
                continue; // not YYYY-MM-DD — unknown layout, skip
            }
            ep(
                "reflect.daily",
                &format!("daily:{date}"),
                &format!("{date} 00:00:00"),
                body.clone(),
            )
        } else {
            let Some((id, title)) = note_key(&name) else {
                continue;
            };
            let body_with_title = if body.trim_start().starts_with('#') {
                body.clone()
            } else {
                format!("# {title}\n\n{body}")
            };
            ep("reflect.note", &id, &export_stamp, body_with_title)
        };

        let (eid, status) = episode::upsert_episode(conn, &episode)?;
        match status {
            episode::IngestOutcome::Inserted => report.inserted += 1,
            episode::IngestOutcome::Updated => report.updated += 1,
            episode::IngestOutcome::Unchanged => {
                report.unchanged += 1;
                continue; // mentions + raw already in place
            }
            episode::IngestOutcome::Tombstoned => {
                report.tombstoned += 1;
                continue; // user deleted this note's episode — stay deleted
            }
        }
        episode_ids.push(eid);

        // Raw markdown into the encrypted archive (capture tier).
        episode::store_raw(conn, eid, &episode.body)?;
        report.captured += 1;

        // Backlinks: unambiguous targets only.
        for target in backlinks(&episode.body) {
            let matches = crate::graph::resolve_entity_all(conn, &target)?;
            if matches.len() == 1 {
                episode::add_mention(conn, eid, &matches[0].id, "backlink", 0.9)?;
                report.mentions += 1;
            }
        }
        // Plain-text alias scan (same tier note capture uses).
        report.alias_mentions += episode::link_by_alias_scan(conn, eid, &episode.body)?;
    }

    // capture_delete: every new/updated episode has its raw verified before
    // the source file is removed (§10).
    let mut verified = true;
    for eid in &episode_ids {
        if !episode::has_raw(conn, *eid)? {
            verified = false;
            break;
        }
    }
    if verified && !episode_ids.is_empty() {
        if std::fs::remove_file(zip_path).is_ok() {
            report.deleted_files = 1;
        }
    }
    Ok(report)
}

// ─── Note processor: promote structured notes to entities ───────────────────
//
// Reflect notes carry deliberate structure — `Type: #person/#author/
// #company/#book` tags and attribute bullets (`Company:`, `Email:`,
// `Authors: [[X]]`) the user hand-curated. Plain episodes waste that:
// searching a project shows nothing because notes aren't linked, and
// people-notes duplicate what the graph knows. This pass promotes ONLY
// evidence-bearing notes (an explicit Type tag, or a title that resolves
// to exactly one existing entity) — plain prose notes stay episodes, so
// there is no junk-node explosion. Everything is idempotent: person
// creation dedupes by email identifier, assert_fact corroborates
// existing facts, identifiers/mentions upsert.

#[derive(Debug, Default, serde::Serialize)]
pub struct ProcessReport {
    pub scanned: usize,
    pub promoted: usize,
    pub attached: usize,
    pub facts: usize,
    pub identifiers: usize,
    pub skipped: usize,
}

fn unescape(s: &str) -> String {
    s.replace('\\', "")
}

fn strip_brackets(s: &str) -> &str {
    s.trim()
        .trim_start_matches("[[")
        .trim_end_matches("]]")
        .trim()
}

/// Leading `# Title` plus attribute bullets. `- Key: value` collects one
/// value; a bare `- Key` or `- Keys` header collects its indented children
/// (Reflect's `- Emails\n  - a@b.c` shape).
fn parse_note(body: &str) -> (String, Vec<(String, Vec<String>)>) {
    let mut title = String::new();
    let mut attrs: Vec<(String, Vec<String>)> = Vec::new();
    let mut open_key: Option<usize> = None;
    for line in body.lines() {
        if title.is_empty() {
            if let Some(t) = line.strip_prefix("# ") {
                title = t.trim().to_string();
            }
            continue;
        }
        if let Some(rest) = line.strip_prefix("- ") {
            open_key = None;
            let Some((key, value)) = rest.split_once(':') else {
                // Bare header bullet ("- Emails") opens a sublist.
                let k = rest.trim().trim_end_matches('s').to_lowercase();
                if !k.is_empty() && k.chars().all(|c| c.is_ascii_alphabetic()) {
                    attrs.push((k, vec![]));
                    open_key = Some(attrs.len() - 1);
                }
                continue;
            };
            let key_norm = key.trim().trim_end_matches('s').to_lowercase();
            if key_norm.is_empty() || !key_norm.chars().all(|c| c.is_ascii_alphabetic()) {
                continue;
            }
            let value = unescape(value.trim());
            if value.is_empty() {
                attrs.push((key_norm, vec![]));
                open_key = Some(attrs.len() - 1);
            } else {
                attrs.push((key_norm, vec![value]));
            }
        } else if let Some(item) = line.strip_prefix("  - ") {
            if let Some(k) = open_key {
                let v = unescape(item.trim());
                if !v.is_empty() {
                    attrs[k].1.push(v);
                }
            }
        } else if !line.trim().is_empty() {
            open_key = None;
        }
    }
    (title, attrs)
}

fn attr<'a>(attrs: &'a [(String, Vec<String>)], key: &str) -> Vec<&'a str> {
    attrs
        .iter()
        .filter(|(k, _)| k == key)
        .flat_map(|(_, vs)| vs.iter().map(|s| s.as_str()))
        .collect()
}

fn resolve_unique_nonanchor(conn: &Connection, name: &str) -> Result<Option<crate::graph::Node>> {
    let mut m: Vec<_> = crate::graph::resolve_entity_all(conn, name)?
        .into_iter()
        .filter(|n| {
            !matches!(
                n.node_type.as_str(),
                "event" | "event_series" | "document" | "artifact"
            )
        })
        .collect();
    Ok(if m.len() == 1 {
        Some(m.remove(0))
    } else {
        None
    })
}

fn get_or_create_typed(
    conn: &Connection,
    name: &str,
    node_type: &str,
) -> Result<crate::graph::Node> {
    // Exact-type match first — the anchor-last filter below would hide a
    // document node from its own re-run and mint duplicates.
    let mut same_type: Vec<_> = crate::graph::resolve_entity_all(conn, name)?
        .into_iter()
        .filter(|n| n.node_type == node_type)
        .collect();
    if same_type.len() == 1 {
        return Ok(same_type.remove(0));
    }
    if let Some(n) = resolve_unique_nonanchor(conn, name)? {
        return Ok(n);
    }
    let id = format!("{node_type}-{}", uuid::Uuid::new_v4());
    let mut node = crate::graph::Node::new(&id, node_type, name);
    node.source = "reflect".into();
    crate::graph::upsert_node(conn, &node)?;
    Ok(node)
}

/// Promote structured reflect.note episodes already in the DB. Safe to
/// re-run any time (e.g. after each export ingest).
pub fn process_notes(conn: &Connection) -> Result<ProcessReport> {
    // The role predicate isn't in the seeded vocabulary; add idempotently.
    conn.execute(
        "INSERT OR IGNORE INTO predicate (name, inverse, description)
         VALUES ('has_role', 'role_of', 'Person holds role/title (from Reflect attributes)')",
        [],
    )?;

    let mut stmt = conn.prepare("SELECT id, body FROM episode WHERE source = 'reflect.note'")?;
    let notes: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut report = ProcessReport::default();
    report.scanned = notes.len();

    for (eid, body) in notes {
        let (title, attrs) = parse_note(&body);
        if title.is_empty() {
            report.skipped += 1;
            continue;
        }
        let ty = attr(&attrs, "type")
            .first()
            .map(|v| v.trim_start_matches('#').to_lowercase());

        let node = match ty.as_deref() {
            Some("person") | Some("author") => {
                let emails: Vec<String> = attr(&attrs, "email")
                    .iter()
                    .map(|v| v.to_lowercase())
                    .filter(|v| v.contains('@'))
                    .collect();
                let node = crate::graph::get_or_create_person(
                    conn,
                    emails.first().map(|s| s.as_str()),
                    &title,
                    "reflect",
                )?;
                for extra in emails.iter().skip(1) {
                    crate::graph::upsert_identifier(conn, "email", extra, &node.id, "reflect")?;
                    report.identifiers += 1;
                }
                for phone in attr(&attrs, "phone") {
                    let digits: String = phone
                        .chars()
                        .filter(|c| c.is_ascii_digit() || *c == '+')
                        .collect();
                    if digits.len() >= 7 {
                        crate::graph::upsert_identifier(
                            conn, "phone", &digits, &node.id, "reflect",
                        )?;
                        report.identifiers += 1;
                    }
                }
                for company in attr(&attrs, "company") {
                    let cname = strip_brackets(company);
                    if cname.is_empty() {
                        continue;
                    }
                    let org = get_or_create_typed(conn, cname, "org")?;
                    crate::fact::assert_fact(
                        conn,
                        &node.id,
                        "works_at",
                        Some(&org.id),
                        None,
                        &format!("{title} works at {cname}."),
                        Some(eid),
                        None,
                        0.9,
                        "reflect",
                    )?;
                    report.facts += 1;
                }
                for role in attr(&attrs, "title") {
                    crate::fact::assert_fact(
                        conn,
                        &node.id,
                        "has_role",
                        None,
                        Some(role),
                        &format!("{title} is {role}."),
                        Some(eid),
                        None,
                        0.9,
                        "reflect",
                    )?;
                    report.facts += 1;
                }
                for loc in attr(&attrs, "location") {
                    crate::fact::assert_fact(
                        conn,
                        &node.id,
                        "located_in",
                        None,
                        Some(loc),
                        &format!("{title} is located in {loc}."),
                        Some(eid),
                        None,
                        0.8,
                        "reflect",
                    )?;
                    report.facts += 1;
                }
                Some(node)
            }
            Some("company") => {
                let node = get_or_create_typed(conn, &title, "org")?;
                for domain in attr(&attrs, "domain") {
                    let d = strip_brackets(domain).to_lowercase();
                    if d.contains('.') {
                        crate::graph::upsert_identifier(conn, "url", &d, &node.id, "reflect")?;
                        report.identifiers += 1;
                    }
                }
                Some(node)
            }
            Some("book") => {
                let node = get_or_create_typed(conn, &title, "document")?;
                for author in attr(&attrs, "author") {
                    let aname = strip_brackets(author);
                    if aname.is_empty() {
                        continue;
                    }
                    let person = crate::graph::get_or_create_person(conn, None, aname, "reflect")?;
                    crate::fact::assert_fact(
                        conn,
                        &person.id,
                        "authored",
                        Some(&node.id),
                        None,
                        &format!("{aname} authored {title}."),
                        Some(eid),
                        None,
                        0.9,
                        "reflect",
                    )?;
                    report.facts += 1;
                }
                Some(node)
            }
            _ => {
                // No type evidence: attach only when the title already IS a
                // known entity — never create from prose.
                resolve_unique_nonanchor(conn, &title)?
            }
        };

        match node {
            Some(n) => {
                episode::add_mention(conn, eid, &n.id, "reflect", 1.0)?;
                if ty.is_some() {
                    report.promoted += 1;
                } else {
                    report.attached += 1;
                }
            }
            None => report.skipped += 1,
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{upsert_node, Node};
    use std::io::Write;

    fn make_zip(dir: &Path) -> std::path::PathBuf {
        let path = dir.join("reflect.zip");
        let f = std::fs::File::create(&path).unwrap();
        let mut z = zip::ZipWriter::new(f);
        let opts = zip::write::SimpleFileOptions::default();
        z.start_file("daily-notes/2026-02-06.md", opts).unwrap();
        z.write_all(
            "# Fri, February 6th, 2026\n\n- met with [[Nadia]] about the pilot\n".as_bytes(),
        )
        .unwrap();
        z.start_file("Sam's Thesis-0123456789abcdef0123456789abcdef.md", opts)
            .unwrap();
        z.write_all("# Sam's Thesis\n\n- no trigger needed\n".as_bytes())
            .unwrap();
        z.start_file("Tips and tricks-tips-and-tricks.md", opts)
            .unwrap();
        z.write_all("tips body\n".as_bytes()).unwrap();
        z.finish().unwrap();
        path
    }

    #[test]
    fn test_process_notes_promotes_typed_attaches_known_skips_prose() {
        let conn = open_memory().unwrap();
        // Existing person the Iris note must ATTACH to, not duplicate.
        let existing = crate::graph::get_or_create_person(
            &conn,
            Some("iris.calder@example.com"),
            "Iris Calder",
            "cal",
        )
        .unwrap();
        // Existing project a plain note's title resolves to.
        upsert_node(&conn, &Node::new("proj-fm", "project", "flowmail")).unwrap();

        for (sid, body) in [
            ("n1", "# Iris Calder\n\n- Company: Westfield\n- Type: #person\n- Email: iris.calder\\@example.com\n- Phone:\n"),
            ("n2", "# Omar Reyes\n\n- Title: CEO\n- Company: [[Notely]]\n- Type: #person\n- Emails\n  - omar\\@notely.example.com\n"),
            ("n3", "# Notely\n\n- type: #company\n- domain: notely.example.com\n"),
            ("n4", "# The Way to Wealth\n\n- Type: #book\n- Authors: [[Benjamin Franklin]]\n"),
            ("n5", "# flowmail\n\nnotes about the app\n"),
            ("n6", "# Random musing\n\njust prose\n"),
        ] {
            let (id, _) = crate::episode::upsert_episode(&conn, &super::ep(
                "reflect.note", sid, "2026-08-03 00:00:00", body.replace("\\n", "\n"))).unwrap();
            let _ = id;
        }

        let r = process_notes(&conn).unwrap();
        assert_eq!(r.scanned, 6);
        assert_eq!(r.promoted, 4);
        assert_eq!(
            r.attached, 1,
            "flowmail note attaches to the existing project"
        );
        assert_eq!(r.skipped, 1, "prose note stays an episode");

        // Iris attached to the EXISTING node (email identifier dedupe).
        let persons: i64 = conn.query_row(
            "SELECT COUNT(*) FROM nodes WHERE node_type='person' AND canonical_name='iris calder'",
            [], |q| q.get(0)).unwrap();
        assert_eq!(persons, 1, "no duplicate Iris");
        let iris_facts = crate::fact::facts_for_node(&conn, &existing.id, 10).unwrap();
        assert!(
            iris_facts.iter().any(|f| f.predicate == "works_at"),
            "Westfield works_at fact"
        );

        // Omar → works_at the Notely org (company note + [[Company]] attr converge).
        let orgs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM nodes WHERE node_type='org' AND canonical_name='notely'",
                [],
                |q| q.get(0),
            )
            .unwrap();
        assert_eq!(orgs, 1, "Notely org created once, not per reference");

        // Book + authored edge.
        let authored: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact_current WHERE predicate='authored'",
                [],
                |q| q.get(0),
            )
            .unwrap();
        assert_eq!(authored, 1);

        // Idempotent re-run: no new nodes or facts.
        let nodes_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |q| q.get(0))
            .unwrap();
        let facts_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM fact", [], |q| q.get(0))
            .unwrap();
        process_notes(&conn).unwrap();
        let nodes_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |q| q.get(0))
            .unwrap();
        let facts_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM fact", [], |q| q.get(0))
            .unwrap();
        assert_eq!(nodes_before, nodes_after);
        assert_eq!(facts_before, facts_after);
    }

    #[test]
    fn test_reflect_zip_ingest_backlinks_and_capture_delete() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("nadia", "person", "Nadia")).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let zip_path = make_zip(dir.path());

        let r = ingest_zip(&conn, &zip_path).unwrap();
        assert_eq!(r.inserted, 3);
        assert_eq!(r.captured, 3);
        assert_eq!(r.deleted_files, 1);
        assert!(
            !zip_path.exists(),
            "capture_delete: zip removed after archive verified"
        );
        assert!(r.mentions >= 1, "[[Nadia]] backlink must link");

        // Daily note keyed by date; topic note by trailing id; slug id works.
        let daily: i64 = conn.query_row(
            "SELECT COUNT(*) FROM episode WHERE source='reflect.daily' AND source_id='daily:2026-02-06'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(daily, 1);
        let by_id: String = conn.query_row(
            "SELECT body FROM episode WHERE source='reflect.note' AND source_id='0123456789abcdef0123456789abcdef'",
            [], |r| r.get(0)).unwrap();
        assert!(by_id.starts_with("# Sam's Thesis"));
        let slug: i64 = conn.query_row(
            "SELECT COUNT(*) FROM episode WHERE source='reflect.note' AND source_id='tips-and-tricks'",
            [], |r| r.get(0)).unwrap();
        assert_eq!(slug, 1, "slug-suffixed notes keyed by the full slug");

        // Re-ingest of identical content is a no-op (fresh zip, same notes).
        let zip_path = make_zip(dir.path());
        let r2 = ingest_zip(&conn, &zip_path).unwrap();
        assert_eq!(r2.inserted, 0);
        assert_eq!(r2.unchanged, 3);
    }
}
