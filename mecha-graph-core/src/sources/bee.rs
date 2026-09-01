//! Bee source (§5.1): reads the `bee-sync` markdown corpus. Bee's native
//! fields (summaries, key takeaways, location) are mapped into the enrichment
//! envelope for free — no model call (§6). Names are bare/spoken, so Bee gets
//! no deterministic person links; the alias-scan linker (fed by calendar
//! attendee aliases) does that work.
//!
//! Bee audio is 'private' by default (§10): excluded from default retrieval.

use crate::enrich::Envelope;
use crate::episode::Episode;
use crate::error::{Error, Result};
use crate::sources::{ProposedLink, Source};
use rusqlite::Connection;
use std::path::{Path, PathBuf};

pub struct BeeSource {
    pub root: PathBuf,
}

impl BeeSource {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        BeeSource { root: root.into() }
    }

    pub fn default_root() -> PathBuf {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
        PathBuf::from(home).join("bee-sync")
    }
}

#[derive(Debug, Default)]
pub struct ParsedBee {
    pub id: String,
    pub start_time: Option<String>,
    pub end_time: Option<String>,
    pub short_summary: String,
    pub summary: String,
    pub key_takeaways: Vec<String>,
    pub location: Option<String>,
    pub lat: Option<f64>,
    pub lon: Option<f64>,
}

/// Parse Bee's markdown conversation/daily format:
/// `# Conversation <id>` header, `- key: value` metadata, `## Section` bodies.
pub fn parse_bee_markdown(text: &str) -> Result<ParsedBee> {
    let mut out = ParsedBee::default();
    let mut section = String::new();
    let mut section_buf: Vec<&str> = Vec::new();

    let flush = |out: &mut ParsedBee, section: &str, buf: &[&str]| {
        let body = buf.join("\n").trim().to_string();
        match section {
            "Short Summary" => out.short_summary = body,
            "Summary" => out.summary = body,
            "Primary Location" => {
                // "- 1 Sample Ln, ..., Freedonia (12.34500, -67.89000)"
                for line in buf {
                    let line = line.trim().trim_start_matches("- ");
                    if line.starts_with("created_at:") {
                        continue;
                    }
                    if line.is_empty() {
                        continue;
                    }
                    if let Some(open) = line.rfind('(') {
                        let coords = line[open + 1..].trim_end_matches(')');
                        let parts: Vec<&str> = coords.split(',').map(|s| s.trim()).collect();
                        if parts.len() == 2 {
                            out.lat = parts[0].parse().ok();
                            out.lon = parts[1].parse().ok();
                        }
                        out.location = Some(line[..open].trim().to_string());
                    } else {
                        out.location = Some(line.to_string());
                    }
                    break;
                }
            }
            _ => {}
        }
    };

    for line in text.lines() {
        if let Some(h) = line.strip_prefix("# ") {
            // "# Conversation 1234567" | "# Daily Summary — 2025-07-17"
            if let Some(id) = h.strip_prefix("Conversation ") {
                out.id = id.trim().to_string();
            }
            continue;
        }
        if let Some(h) = line.strip_prefix("## ") {
            flush(&mut out, &section, &section_buf);
            section = h.trim().to_string();
            section_buf.clear();
            continue;
        }
        if section.is_empty() {
            // Metadata block.
            let line = line.trim().trim_start_matches("- ");
            if let Some(v) = line.strip_prefix("start_time: ") {
                out.start_time = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("end_time: ") {
                out.end_time = Some(v.trim().to_string());
            } else if let Some(v) = line.strip_prefix("id: ") {
                if out.id.is_empty() {
                    out.id = v.trim().to_string();
                }
            } else if let Some(v) = line.strip_prefix("date_time: ") {
                if out.start_time.is_none() {
                    out.start_time = Some(v.trim().to_string());
                }
            }
        } else {
            section_buf.push(line);
        }
    }
    flush(&mut out, &section, &section_buf);

    // Key takeaways live inside the Summary section as bullets under
    // "### Key Takeaways" (or "# Key Takeaways" in daily files).
    let mut in_takeaways = false;
    for line in out.summary.clone().lines() {
        let t = line.trim();
        if t.trim_start_matches('#').trim() == "Key Takeaways" {
            in_takeaways = true;
            continue;
        }
        if in_takeaways {
            if t.starts_with('#') {
                in_takeaways = false;
            } else if let Some(item) = t.strip_prefix("*").or_else(|| t.strip_prefix("-")) {
                let item = item.trim().to_string();
                if !item.is_empty() {
                    out.key_takeaways.push(item);
                }
            }
        }
    }

    if out.id.is_empty() {
        return Err(Error::Parse("bee file has no id".into()));
    }
    Ok(out)
}

/// ISO8601 (`2026-01-05T09:12:04.100Z`) → SQLite datetime.
fn iso_to_sqlite(iso: &str) -> String {
    chrono::DateTime::parse_from_rfc3339(iso)
        .map(|dt| dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|_| iso.replace('T', " ").chars().take(19).collect())
}

fn parse_file(path: &Path, source: &str) -> Result<Option<(Episode, Envelope)>> {
    let text = std::fs::read_to_string(path)?;
    let parsed = match parse_bee_markdown(&text) {
        Ok(p) => p,
        Err(_) => return Ok(None),
    };

    let occurred_at = parsed
        .start_time
        .as_deref()
        .map(iso_to_sqlite)
        .unwrap_or_default();
    if occurred_at.is_empty() {
        return Ok(None);
    }

    // The graph holds distilled knowledge (§2, rule 3): summaries, not the
    // full transcription. source_ref points back at the raw file.
    let mut body = String::new();
    if !parsed.short_summary.is_empty() {
        body.push_str(&parsed.short_summary);
        body.push_str("\n\n");
    }
    body.push_str(&parsed.summary);
    if body.trim().is_empty() {
        return Ok(None);
    }

    let envelope = Envelope {
        summary: if parsed.short_summary.is_empty() {
            parsed.summary.chars().take(400).collect()
        } else {
            parsed.short_summary.clone()
        },
        key_points: parsed.key_takeaways.clone(),
        sensitivity: "private".into(),
        ..Default::default()
    };

    let episode = Episode {
        id: 0,
        uid: String::new(),
        source: source.to_string(),
        source_id: parsed.id.clone(),
        source_ref: Some(path.to_string_lossy().to_string()),
        body: body.trim().to_string(),
        occurred_at,
        occurred_end: parsed.end_time.as_deref().map(iso_to_sqlite),
        ingested_at: String::new(),
        lat: parsed.lat,
        lon: parsed.lon,
        location: parsed.location.clone(),
        sensitivity: "private".into(),
        scope_id: None,
        meta: None,
        // Full markdown incl. Transcriptions — the capture archive that makes
        // delete-after-ingest lossless.
        raw: Some(text.clone()),
    };
    Ok(Some((episode, envelope)))
}

impl BeeSource {
    fn collect(&self, subdir: &str, source: &str, since: Option<&str>) -> Result<Vec<Episode>> {
        let dir = self.root.join(subdir);
        let mut out = Vec::new();
        if !dir.exists() {
            return Ok(out);
        }
        // Layout: <root>/<subdir>/<YYYY-MM-DD>/<id>.md
        let mut day_dirs: Vec<PathBuf> = std::fs::read_dir(&dir)?
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| p.is_dir())
            .collect();
        day_dirs.sort();

        for day in day_dirs {
            // Cheap date-level cursor: skip whole days before `since`.
            if let (Some(s), Some(name)) = (since, day.file_name().and_then(|n| n.to_str())) {
                if name < &s[..10.min(s.len())] {
                    continue;
                }
            }
            let mut files: Vec<PathBuf> = std::fs::read_dir(&day)?
                .filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| p.extension().is_some_and(|x| x == "md"))
                .collect();
            files.sort();
            for f in files {
                if let Some((ep, _env)) = parse_file(&f, source)? {
                    out.push(ep);
                }
            }
        }
        Ok(out)
    }
}

impl Source for BeeSource {
    fn id(&self) -> &'static str {
        "bee"
    }

    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>> {
        let mut eps = self.collect("conversations", "bee.conversation", since)?;
        eps.extend(self.collect("daily", "bee.daily", since)?);
        Ok(eps)
    }

    /// Bee identifies people by bare spoken names (★☆☆☆☆, §5.1) — nothing is
    /// deterministic here. Person linking happens via the alias-scan tier.
    fn deterministic_links(&self, _ep: &Episode) -> Vec<ProposedLink> {
        vec![]
    }

    /// Each conversation/daily is its own markdown file → safe to delete
    /// individually under capture_delete retention.
    fn per_episode_files(&self) -> bool {
        true
    }
}

// ─── Streaming mode: API → encrypted DB, no plaintext mirror ─────────────────

/// Streaming Bee ingest (`mode = "stream"`): pulls conversations + dailies
/// straight from the Bee CLI's JSON output into the (encrypted) DB. Plaintext
/// never touches disk; the full JSON record is archived to `episode_raw`.
/// Makes the `~/bee-sync` markdown mirror optional entirely.
pub fn fetch_stream(since: Option<&str>) -> Result<Vec<Episode>> {
    let mut out = Vec::new();
    out.extend(stream_list("conversations", since)?);
    out.extend(stream_list("daily", since)?);
    Ok(out)
}

fn epoch_ms_to_sqlite(v: &serde_json::Value) -> Option<String> {
    // Bee emits epoch-ms numbers or ISO strings depending on endpoint.
    if let Some(ms) = v.as_i64() {
        return chrono::DateTime::from_timestamp(ms / 1000, 0)
            .map(|dt| dt.naive_utc().format("%Y-%m-%d %H:%M:%S").to_string());
    }
    v.as_str().map(iso_to_sqlite)
}

fn stream_list(endpoint: &str, since: Option<&str>) -> Result<Vec<Episode>> {
    let mut out = Vec::new();
    let mut cursor: Option<String> = None;
    // Pages are newest-first; stop once a whole page is older than `since`.
    'pages: for _page in 0..200 {
        let mut cmd = std::process::Command::new("bee");
        cmd.args([endpoint, "list", "--limit", "100", "--json"]);
        if let Some(c) = &cursor {
            cmd.args(["--cursor", c]);
        }
        let output = cmd
            .output()
            .map_err(|e| Error::Other(format!("bee CLI not runnable: {e}")))?;
        if !output.status.success() {
            return Err(Error::Other(format!(
                "bee {endpoint} list failed: {}",
                String::from_utf8_lossy(&output.stderr)
            )));
        }
        let body: serde_json::Value = serde_json::from_slice(&output.stdout)
            .map_err(|e| Error::Parse(format!("bee {endpoint} json: {e}")))?;
        let items = body
            .get("conversations")
            .or_else(|| body.get("daily_summaries"))
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            break;
        }

        let mut all_old = true;
        for item in &items {
            let (source, occurred_at, occurred_end, body_text, lat, lon, location) =
                if endpoint == "conversations" {
                    let occurred = item.get("start_time").and_then(epoch_ms_to_sqlite);
                    let end = item.get("end_time").and_then(epoch_ms_to_sqlite);
                    let short = item["short_summary"].as_str().unwrap_or_default();
                    let summary = item["summary"].as_str().unwrap_or_default();
                    let loc = item.get("primary_location");
                    (
                        "bee.conversation",
                        occurred,
                        end,
                        format!("{short}\n\n{summary}").trim().to_string(),
                        loc.and_then(|l| l["latitude"].as_f64()),
                        loc.and_then(|l| l["longitude"].as_f64()),
                        loc.and_then(|l| l["address"].as_str())
                            .map(|s| s.to_string()),
                    )
                } else {
                    let occurred = item.get("date_time").and_then(epoch_ms_to_sqlite);
                    let short = item["short_summary"].as_str().unwrap_or_default();
                    let summary = item["summary"].as_str().unwrap_or_default();
                    (
                        "bee.daily",
                        occurred,
                        None,
                        format!("{short}\n\n{summary}").trim().to_string(),
                        None,
                        None,
                        None,
                    )
                };

            let Some(occurred_at) = occurred_at else {
                continue;
            };
            let id = item["id"]
                .as_i64()
                .map(|i| i.to_string())
                .or_else(|| item["id"].as_str().map(|s| s.to_string()));
            let Some(id) = id else { continue };
            if body_text.is_empty() {
                continue;
            }

            if let Some(s) = since {
                if occurred_at.as_str() <= s {
                    continue;
                }
            }
            all_old = false;

            out.push(Episode {
                id: 0,
                uid: String::new(),
                source: source.into(),
                source_id: id,
                source_ref: None, // no file — the archive IS the raw
                body: body_text,
                occurred_at,
                occurred_end,
                ingested_at: String::new(),
                lat,
                lon,
                location,
                sensitivity: "private".into(),
                scope_id: None,
                meta: None,
                // Full API record, verbatim, into the encrypted archive.
                raw: Some(item.to_string()),
            });
        }

        if all_old && since.is_some() {
            break 'pages; // everything on this page predates the cursor
        }
        cursor = body["next_cursor"].as_str().map(|s| s.to_string());
        if cursor.is_none() {
            break;
        }
    }
    Ok(out)
}

/// Pull "Key Takeaways" bullets out of a Bee summary (markdown headings).
fn extract_takeaways(summary: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut in_takeaways = false;
    for line in summary.lines() {
        let t = line.trim();
        if t.trim_start_matches('#').trim() == "Key Takeaways" {
            in_takeaways = true;
            continue;
        }
        if in_takeaways {
            if t.starts_with('#') {
                in_takeaways = false;
            } else if let Some(item) = t.strip_prefix('*').or_else(|| t.strip_prefix('-')) {
                let item = item.trim().to_string();
                if !item.is_empty() {
                    out.push(item);
                }
            }
        }
    }
    out
}

/// Enrichment for stream-mode episodes: map the archived JSON record into the
/// envelope. (Stream episodes have no source_ref; their raw is the API JSON.)
pub fn enrich_from_stream(conn: &rusqlite::Connection) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT e.id FROM episode e
         WHERE e.source IN ('bee.conversation','bee.daily')
           AND e.source_ref IS NULL
           AND e.id NOT IN (SELECT episode_id FROM episode_enrichment)",
    )?;
    let ids: Vec<i64> = stmt
        .query_map([], |r| r.get(0))?
        .collect::<std::result::Result<_, _>>()?;

    let mut n = 0;
    for episode_id in ids {
        let Some(raw) = crate::episode::get_raw(conn, episode_id)? else {
            continue;
        };
        let Ok(item) = serde_json::from_str::<serde_json::Value>(&raw) else {
            continue;
        };
        let short = item["short_summary"].as_str().unwrap_or_default();
        let summary = item["summary"].as_str().unwrap_or_default();
        let envelope = Envelope {
            summary: if short.is_empty() {
                summary.chars().take(400).collect()
            } else {
                short.to_string()
            },
            key_points: extract_takeaways(summary),
            sensitivity: "private".into(),
            ..Default::default()
        };
        crate::enrich::store_enrichment(conn, episode_id, &envelope, "bee-native")?;
        n += 1;
    }
    Ok(n)
}

/// Adapter so the streaming path runs through the standard ingest driver.
pub struct BeeStreamSource;

impl Source for BeeStreamSource {
    fn id(&self) -> &'static str {
        "bee"
    }
    fn fetch(&self, since: Option<&str>) -> Result<Vec<Episode>> {
        fetch_stream(since)
    }
    fn deterministic_links(&self, _ep: &Episode) -> Vec<ProposedLink> {
        vec![]
    }
}

/// Free enrichment for Bee episodes already in the DB: map native fields into
/// the envelope without any model call (§6).
///
/// Reads the source file when present, and falls back to the `episode_raw`
/// capture archive when it has been deleted (capture_delete retention) — so
/// re-enrichment keeps working after the plaintext files are gone.
pub fn enrich_from_native(conn: &rusqlite::Connection, root: &Path) -> Result<usize> {
    let mut stmt = conn.prepare(
        "SELECT e.id, e.source_ref FROM episode e
         WHERE e.source IN ('bee.conversation','bee.daily')
           AND e.source_ref IS NOT NULL
           AND e.id NOT IN (SELECT episode_id FROM episode_enrichment)",
    )?;
    let rows: Vec<(i64, String)> = stmt
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<std::result::Result<_, _>>()?;

    let mut n = 0;
    for (episode_id, source_ref) in rows {
        let path = PathBuf::from(&source_ref);
        let path = if path.is_absolute() {
            path
        } else {
            root.join(path)
        };
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => Some(t),
            Err(_) => crate::episode::get_raw(conn, episode_id)?, // archive fallback
        };
        let Some(text) = text else { continue };
        if let Ok(parsed) = parse_bee_markdown(&text) {
            let envelope = Envelope {
                summary: if parsed.short_summary.is_empty() {
                    parsed.summary.chars().take(400).collect()
                } else {
                    parsed.short_summary.clone()
                },
                key_points: parsed.key_takeaways.clone(),
                sensitivity: "private".into(),
                ..Default::default()
            };
            crate::enrich::store_enrichment(conn, episode_id, &envelope, "bee-native")?;
            n += 1;
        }
    }
    Ok(n)
}

// ─── Bee facts write-back (two-way review, #12 partial) ─────────────────────
//
// Bee's app extracts its own "suggested facts" from the same conversations
// pkg ingests. Reviewing them twice (once in Bee's app, once here) is
// wasted attention — so pull the unconfirmed ones into pkg's review queue,
// let the pkg tooling (precheck, bulk triage, ghost-text edit) decide, and
// push the verdicts back: accept → `bee facts confirm`, reject →
// `bee facts delete`. The Bee fact id rides in the candidate payload
// (`bee_fact_id`); pushed verdicts are marked `bee_pushed` so the sync is
// idempotent.

#[derive(Debug, Default, serde::Serialize)]
pub struct BeeFactsReport {
    pub staged: usize,
    pub confirmed: usize,
    pub deleted: usize,
    pub push_errors: usize,
    /// One entry per verdict that failed to push, with the reason and how
    /// long it has been failing.
    ///
    /// **A count alone cannot tell a first failure from a permanent one.**
    /// `Err(_) => report.push_errors += 1` discarded the reason and wrote
    /// nothing to the row, so the candidate came back on the next sync
    /// looking exactly like a fresh failure — and one verdict retried every
    /// night from 2026-08-24 to 2026-09-01 under the label "will retry",
    /// with no record anywhere of what was wrong. "Will retry" is a label,
    /// not a mechanism: the retry needs somewhere to accumulate.
    pub push_failures: Vec<BeePushFailure>,
}

/// A verdict that could not be pushed back to Bee, and its history.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BeePushFailure {
    pub candidate_id: i64,
    pub bee_fact_id: i64,
    /// Whether the verdict was accept (`confirm`) or reject (`delete`).
    pub accepted: bool,
    /// Consecutive failed attempts, this one included.
    pub attempts: u32,
    /// RFC 3339, when this verdict first failed to push.
    pub first_failed_at: String,
    /// The error from the last attempt, as `bee_json` reported it.
    pub error: String,
}

/// Attempts after which a push is reported as stuck rather than retrying.
///
/// A permanent failure and a transient one are indistinguishable on the
/// first night and obvious by the eighth; this is where the report stops
/// saying "will retry" and starts naming it as something to look at.
pub const BEE_PUSH_STUCK_ATTEMPTS: u32 = 3;

fn bee_json(args: &[&str]) -> Result<serde_json::Value> {
    let output = std::process::Command::new("bee")
        .args(args)
        .arg("--json")
        .output()
        .map_err(|e| Error::Other(format!("bee CLI not runnable: {e}")))?;
    if !output.status.success() {
        return Err(Error::Other(format!(
            "bee {} failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr)
        )));
    }
    serde_json::from_slice(&output.stdout)
        .map_err(|e| Error::Parse(format!("bee {} json: {e}", args.join(" "))))
}

/// Bee fact ids already present in any candidate payload (any status).
fn staged_bee_ids(conn: &Connection) -> Result<std::collections::HashSet<i64>> {
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload, '$.bee_fact_id') FROM fact_candidate
         WHERE json_extract(payload, '$.bee_fact_id') IS NOT NULL",
    )?;
    let ids = stmt
        .query_map([], |r| r.get::<_, i64>(0))?
        .collect::<std::result::Result<_, _>>()?;
    Ok(ids)
}

/// Stage one Bee suggested fact as a review candidate. Subject is pre-filled
/// by deterministic entity detection over the text when exactly one
/// unambiguous entity is present (people first). An ambiguous match keeps its
/// surface string (the payload subject is a name, not an id) — resolution
/// happens live at review/accept time, and an ambiguous name must not be
/// laundered into an empty subject. Only a text with no entity at all stays
/// empty for the reviewer's ghost-text/`b`-bind flow. Public for tests.
pub fn stage_bee_fact(conn: &Connection, id: i64, text: &str, tags: &[String]) -> Result<i64> {
    let (detected, ambiguous) = crate::router::detect_entities(conn, text)?;
    let subject = detected
        .iter()
        .find(|d| d.node_type == "person")
        .map(|d| d.name.clone())
        .or_else(|| {
            ambiguous
                .iter()
                .find(|a| {
                    a.candidates
                        .iter()
                        .any(|c| c.node_id.starts_with("person-"))
                })
                .map(|a| a.matched.clone())
        })
        .or_else(|| detected.first().map(|d| d.name.clone()))
        .or_else(|| ambiguous.first().map(|a| a.matched.clone()))
        // A claim naming nothing the graph knows is usually about a THING
        // rather than a person ("The gutter cleaning is scheduled…")
        // — its own noun phrase is the subject, headed for a topic node on
        // accept. Never defaulted to the owner: owner-default attribution
        // is the wearable's failure mode, not a repair.
        .or_else(|| crate::router::subject_phrase(text))
        .unwrap_or_default();
    let proposed = crate::fact::ProposedFact {
        subject,
        predicate: "related_to".into(),
        object: None,
        object_value: None,
        statement: text.to_string(),
        valid_from: None,
        confidence: Some(0.5), // Bee's own extraction: unvetted
        tags: (!tags.is_empty()).then(|| tags.join(",")),
        ..Default::default()
    };
    let cid = crate::fact::propose_fact(conn, &proposed, "bee:suggested", None)?;
    // Ride the Bee id in the payload so verdicts can be pushed back.
    let payload: serde_json::Value = conn.query_row(
        "SELECT payload FROM fact_candidate WHERE id = ?1",
        rusqlite::params![cid],
        |r| {
            let s: String = r.get(0)?;
            Ok(serde_json::from_str(&s).unwrap_or(serde_json::Value::Null))
        },
    )?;
    if let serde_json::Value::Object(mut map) = payload {
        map.insert("bee_fact_id".into(), serde_json::json!(id));
        map.insert("kind".into(), serde_json::json!("bee_fact"));
        crate::fact::update_candidate_payload(conn, cid, &serde_json::Value::Object(map))?;
    }
    Ok(cid)
}

/// One pending verdict: the candidate, its Bee id, the verdict, and what
/// the previous attempts did.
///
/// The last two fields are why this is a struct rather than the tuple it
/// was: a retry that cannot see its own history reports every attempt as
/// the first one.
#[derive(Debug, Clone)]
pub struct PendingBeePush {
    pub candidate_id: i64,
    pub bee_fact_id: i64,
    pub accepted: bool,
    /// Failed attempts so far. Zero on a verdict that has never been tried.
    pub attempts: u32,
    /// RFC 3339 of the first failure, if there has been one.
    pub first_failed_at: Option<String>,
}

/// Reviewed-but-unpushed Bee candidates, with whatever their earlier push
/// attempts recorded. Public for tests.
pub fn pending_bee_pushes(conn: &Connection) -> Result<Vec<PendingBeePush>> {
    let mut stmt = conn.prepare(
        "SELECT id,
                json_extract(payload, '$.bee_fact_id'),
                status,
                json_extract(payload, '$.bee_push_attempts'),
                json_extract(payload, '$.bee_push_first_failed_at')
         FROM fact_candidate
         WHERE json_extract(payload, '$.bee_fact_id') IS NOT NULL
           AND json_extract(payload, '$.bee_pushed') IS NULL
           AND status IN ('accepted', 'rejected')",
    )?;
    let rows = stmt
        .query_map([], |r| {
            Ok(PendingBeePush {
                candidate_id: r.get::<_, i64>(0)?,
                bee_fact_id: r.get::<_, i64>(1)?,
                accepted: r.get::<_, String>(2)? == "accepted",
                // A payload written before this column existed reads as
                // zero attempts, which is the honest answer: nothing was
                // recorded, so nothing is known to have failed.
                attempts: r.get::<_, Option<i64>>(3)?.unwrap_or(0).max(0) as u32,
                first_failed_at: r.get::<_, Option<String>>(4)?,
            })
        })?
        .collect::<std::result::Result<_, _>>()?;
    Ok(rows)
}

/// Record a failed push on the candidate so the next sync can see it.
///
/// Increments the attempt count and stamps the first failure once. The
/// error text is stored too: it is the thing that was missing when one
/// verdict failed silently for eight consecutive nights.
fn record_bee_push_error(
    conn: &Connection,
    candidate_id: i64,
    attempts: u32,
    first_failed_at: &str,
    message: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE fact_candidate
         SET payload = json_set(
                 payload,
                 '$.bee_push_attempts', ?2,
                 '$.bee_push_first_failed_at', ?3,
                 '$.bee_push_error', ?4)
         WHERE id = ?1",
        rusqlite::params![candidate_id, attempts, first_failed_at, message],
    )?;
    Ok(())
}

fn mark_bee_pushed(conn: &Connection, candidate_id: i64) -> Result<()> {
    conn.execute(
        "UPDATE fact_candidate
         SET payload = json_set(payload, '$.bee_pushed', 1)
         WHERE id = ?1",
        rusqlite::params![candidate_id],
    )?;
    Ok(())
}

/// Full two-way pass: pull unconfirmed Bee facts into the queue (capped at
/// `pull_limit` per sync so a 1000-fact backlog drains gradually instead of
/// swamping the review queue), push reviewed verdicts back. Pages continue
/// past already-staged items, so repeated runs reach deeper into the
/// backlog. CLI failures on push are recorded, not fatal — the candidate
/// stays unpushed and retries next sync, carrying its attempt count, the
/// time it first failed and the last error, so a push that will never
/// succeed stops looking like one that just started failing.
pub fn sync_bee_facts(conn: &Connection, pull_limit: usize) -> Result<BeeFactsReport> {
    let mut report = BeeFactsReport::default();
    let already = staged_bee_ids(conn)?;

    // Pull (newest-first pages).
    let mut cursor: Option<String> = None;
    'pages: for _ in 0..20 {
        let mut args: Vec<String> = vec![
            "facts".into(),
            "list".into(),
            "--unconfirmed".into(),
            "--limit".into(),
            "100".into(),
        ];
        if let Some(c) = &cursor {
            args.push("--cursor".into());
            args.push(c.clone());
        }
        let body = bee_json(&args.iter().map(|s| s.as_str()).collect::<Vec<_>>())?;
        let items = body
            .get("facts")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if items.is_empty() {
            break;
        }
        for item in &items {
            if report.staged >= pull_limit {
                break 'pages;
            }
            let Some(id) = item.get("id").and_then(|v| v.as_i64()) else {
                continue;
            };
            if already.contains(&id) {
                continue;
            }
            let text = item
                .get("text")
                .and_then(|v| v.as_str())
                .unwrap_or_default();
            if text.is_empty() {
                continue;
            }
            let tags: Vec<String> = item
                .get("tags")
                .and_then(|v| v.as_array())
                .map(|a| {
                    a.iter()
                        .filter_map(|t| t.as_str().map(String::from))
                        .collect()
                })
                .unwrap_or_default();
            stage_bee_fact(conn, id, text, &tags)?;
            report.staged += 1;
        }
        cursor = body
            .get("next_cursor")
            .or_else(|| body.get("cursor"))
            .and_then(|v| v.as_str())
            .map(String::from);
        if cursor.is_none() {
            break;
        }
    }

    // Push verdicts.
    for pending in pending_bee_pushes(conn)? {
        let sub = if pending.accepted {
            "confirm"
        } else {
            "delete"
        };
        match bee_json(&["facts", sub, &pending.bee_fact_id.to_string()]) {
            Ok(_) => {
                mark_bee_pushed(conn, pending.candidate_id)?;
                if pending.accepted {
                    report.confirmed += 1;
                } else {
                    report.deleted += 1;
                }
            }
            // **Bind the error.** It was `Err(_)`, which threw away the one
            // thing a reader needed: the row was never marked, so it came
            // back every night looking new, and the reason it failed existed
            // nowhere — not in the log, not on the candidate, not in the
            // report. Eight nights of "1 push errors (will retry)" and no
            // way to learn what was wrong without changing the code first.
            Err(e) => {
                let message = format!("{e}");
                let attempts = pending.attempts.saturating_add(1);
                let first_failed_at = pending
                    .first_failed_at
                    .unwrap_or_else(|| chrono::Utc::now().to_rfc3339());
                // Best-effort, like every other write in this sweep: failing
                // the whole sync because the *bookkeeping* about a failure
                // could not be written would turn one stuck verdict into a
                // stuck pipeline.
                if let Err(write_err) = record_bee_push_error(
                    conn,
                    pending.candidate_id,
                    attempts,
                    &first_failed_at,
                    &message,
                ) {
                    eprintln!(
                        "bee: could not record push failure for candidate {}: {write_err}",
                        pending.candidate_id
                    );
                }
                report.push_errors += 1;
                report.push_failures.push(BeePushFailure {
                    candidate_id: pending.candidate_id,
                    bee_fact_id: pending.bee_fact_id,
                    accepted: pending.accepted,
                    attempts,
                    first_failed_at,
                    error: message,
                });
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"# Conversation 1234567

- start_time: 2026-01-05T09:12:04.100Z
- end_time: 2026-01-05T09:20:11.900Z
- device_type: apple_watch
- state: COMPLETED

## Short Summary

Steady progress in the vegetable garden

## Summary

Here's a summary:

### Key Takeaways
*   The tomato seedlings are hardening off well.
*   Raised beds are outperforming the border plot.

### Detail
You spent time planning the spring planting.

## Primary Location

- 1 Sample Ln, Port Exemplar, Cascadia County, 00001, Freedonia (12.34500, -67.89000)
- created_at: 2026-01-05T09:20:12.500Z
"#;

    #[test]
    fn test_parse_bee_markdown() {
        let p = parse_bee_markdown(SAMPLE).unwrap();
        assert_eq!(p.id, "1234567");
        assert_eq!(p.start_time.as_deref(), Some("2026-01-05T09:12:04.100Z"));
        assert_eq!(p.short_summary, "Steady progress in the vegetable garden");
        assert_eq!(p.key_takeaways.len(), 2);
        assert!((p.lat.unwrap() - 12.34500).abs() < 1e-6);
        assert!((p.lon.unwrap() - -67.89000).abs() < 1e-6);
        assert!(p.location.unwrap().starts_with("1 Sample Ln"));
    }

    #[test]
    fn staging_keeps_an_ambiguous_subjects_surface_string() {
        // Regression: two nodes sharing the alias "ada" used to launder the
        // subject into "" — 189 of 200 bee:suggested candidates arrived with
        // an empty subject while the graph held a duplicate Ada.
        let conn = crate::db::open_memory().unwrap();
        let mut a = crate::graph::Node::new("person-a", "person", "Ada B Lovelace");
        a.aliases = vec!["ada".into()];
        crate::graph::upsert_node(&conn, &a).unwrap();
        let mut b = crate::graph::Node::new("person-b", "person", "Ada B. Lovelace");
        b.aliases = vec!["ada".into()];
        crate::graph::upsert_node(&conn, &b).unwrap();

        let cid = stage_bee_fact(&conn, 1, "Ada is prototyping a new eye tracker.", &[]).unwrap();
        let payload: String = conn
            .query_row(
                "SELECT payload FROM fact_candidate WHERE id = ?1",
                rusqlite::params![cid],
                |r| r.get(0),
            )
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&payload).unwrap();
        assert_eq!(
            v["subject"], "ada",
            "ambiguous person match keeps its matched string for live resolution"
        );
    }

    #[test]
    fn test_iso_to_sqlite() {
        assert_eq!(
            iso_to_sqlite("2026-01-05T09:12:04.100Z"),
            "2026-01-05 09:12:04"
        );
    }

    #[test]
    fn test_capture_delete_archives_then_deletes_and_reenriches() {
        use crate::sources::{ingest_with, Retention};
        let conn = crate::db::open_memory().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let day = dir.path().join("conversations").join("2025-07-23");
        std::fs::create_dir_all(&day).unwrap();
        let file = day.join("1234567.md");
        std::fs::write(&file, SAMPLE).unwrap();

        let src = BeeSource::new(dir.path());
        let report = ingest_with(&conn, &src, None, Retention::CaptureDelete).unwrap();
        assert_eq!(report.inserted, 1);
        assert_eq!(report.captured, 1);
        assert_eq!(report.deleted_files, 1);
        assert!(!file.exists(), "plaintext source file must be gone");

        // The archive holds the FULL original file, inside the DB.
        let raw = crate::episode::get_raw(&conn, 1)
            .unwrap()
            .expect("raw archived");
        assert_eq!(raw, SAMPLE);

        // Re-enrichment still works with the file deleted (archive fallback) —
        // the user's requirement: we can update after ingesting.
        conn.execute("DELETE FROM episode_enrichment", []).unwrap();
        let n = enrich_from_native(&conn, dir.path()).unwrap();
        assert_eq!(n, 1, "re-enrichment from episode_raw after file deletion");
        let env = crate::enrich::get_enrichment(&conn, 1).unwrap().unwrap();
        assert_eq!(env.summary, "Steady progress in the vegetable garden");
        assert_eq!(env.key_points.len(), 2);
    }

    #[test]
    fn test_bee_fact_staging_dedup_and_push_lifecycle() {
        let conn = crate::db::open_memory().unwrap();
        crate::graph::upsert_node(&conn, &crate::graph::Node::new("ada", "person", "Ada")).unwrap();

        // Staging pre-fills the subject via entity detection and rides the id.
        let cid = stage_bee_fact(
            &conn,
            42,
            "Ada prefers bullet-point emails",
            &["style".into()],
        )
        .unwrap();
        let staged = staged_bee_ids(&conn).unwrap();
        assert!(staged.contains(&42));
        let c = crate::fact::pending_candidates(&conn, 10).unwrap();
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].payload["subject"], "Ada");
        assert_eq!(c[0].payload["bee_fact_id"], 42);

        // Nothing to push while pending.
        assert!(pending_bee_pushes(&conn).unwrap().is_empty());

        // Reject → queued as a delete push; marking clears it.
        crate::fact::reject_candidate(&conn, cid, "noise").unwrap();
        let pushes = pending_bee_pushes(&conn).unwrap();
        assert_eq!(pushes.len(), 1);
        assert_eq!(pushes[0].candidate_id, cid);
        assert_eq!(pushes[0].bee_fact_id, 42);
        assert!(!pushes[0].accepted);
        // Never tried, so no failure history — the state the old tuple
        // could not express and the retry therefore could not see.
        assert_eq!(pushes[0].attempts, 0);
        assert!(pushes[0].first_failed_at.is_none());
        mark_bee_pushed(&conn, cid).unwrap();
        assert!(pending_bee_pushes(&conn).unwrap().is_empty());
    }
}

#[cfg(test)]
mod push_failure_tests {
    use super::*;

    fn pending_candidate(conn: &Connection, bee_fact_id: i64) -> i64 {
        conn.execute(
            "INSERT INTO fact_candidate (payload, status) VALUES (?1, 'accepted')",
            rusqlite::params![format!(r#"{{"bee_fact_id":{bee_fact_id}}}"#)],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    /// **The regression, stated as a test.** The push arm was
    /// `Err(_) => report.push_errors += 1`: nothing was written to the row,
    /// so the next sync re-read the candidate with no history and every
    /// attempt looked like the first one. One verdict retried nightly from
    /// 2026-08-24 to 2026-09-01 under "will retry", and the reason it failed
    /// was recorded nowhere.
    ///
    /// Fails on the old behaviour at the second assertion: `attempts` stayed
    /// 0 forever, so nothing could ever cross [`BEE_PUSH_STUCK_ATTEMPTS`].
    #[test]
    fn a_failed_push_accumulates_attempts_across_syncs() {
        let conn = crate::db::open_memory().unwrap();
        let cid = pending_candidate(&conn, 4242);

        // First sync: never tried, so no history.
        let first = pending_bee_pushes(&conn).unwrap();
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].attempts, 0);
        assert!(first[0].first_failed_at.is_none());

        record_bee_push_error(
            &conn,
            cid,
            1,
            "2026-08-24T01:38:00Z",
            "bee facts confirm failed",
        )
        .unwrap();

        // Second sync sees the first failure rather than a clean slate.
        let second = pending_bee_pushes(&conn).unwrap();
        assert_eq!(second[0].attempts, 1, "the attempt must survive the sync");
        assert_eq!(
            second[0].first_failed_at.as_deref(),
            Some("2026-08-24T01:38:00Z")
        );

        // And the first failure's timestamp is what ages it: later attempts
        // increment the count and leave the origin alone, so "failing since"
        // means since the first failure, not since the last one.
        record_bee_push_error(&conn, cid, 2, "2026-08-24T01:38:00Z", "still failing").unwrap();
        let third = pending_bee_pushes(&conn).unwrap();
        assert_eq!(third[0].attempts, 2);
        assert_eq!(
            third[0].first_failed_at.as_deref(),
            Some("2026-08-24T01:38:00Z"),
            "first_failed_at is stamped once, not refreshed"
        );
    }

    /// A payload written before this branch has no attempt fields, and must
    /// read as "nothing known to have failed" rather than failing the query.
    /// The append-only-store rule: an unknown field degrades to a default.
    #[test]
    fn a_payload_predating_the_attempt_fields_reads_as_zero() {
        let conn = crate::db::open_memory().unwrap();
        pending_candidate(&conn, 7);
        let rows = pending_bee_pushes(&conn).unwrap();
        assert_eq!(rows[0].attempts, 0);
        assert!(rows[0].first_failed_at.is_none());
    }

    /// A pushed verdict leaves the queue — the idempotence the sync relies
    /// on, and the negative that makes the test above non-vacuous.
    #[test]
    fn a_pushed_verdict_stops_being_pending() {
        let conn = crate::db::open_memory().unwrap();
        let cid = pending_candidate(&conn, 99);
        assert_eq!(pending_bee_pushes(&conn).unwrap().len(), 1);
        mark_bee_pushed(&conn, cid).unwrap();
        assert!(pending_bee_pushes(&conn).unwrap().is_empty());
    }
}
