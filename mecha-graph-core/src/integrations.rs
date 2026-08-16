//! Integration registry: named, configured source instances.
//!
//! Config lives at `~/pkg/config.toml` (chmod 600), one `[sources.<name>]`
//! block per integration. Secrets (Slack token, secret ICS URL) live inline —
//! this is a single-user local box; the file is the credential boundary.
//!
//! ```toml
//! [sources.calendar]
//! kind = "ics"
//! url = "https://calendar.google.com/calendar/ical/.../basic.ics"
//! self_email = "you@example.edu"
//!
//! [sources.slack]
//! kind = "slack"
//! token = "xoxp-..."
//! ```

use crate::error::{Error, Result};
use crate::sources::{self, IngestReport, Retention};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::PathBuf;

pub const KINDS: &[&str] = &[
    "bee", "ics", "sessions", "slack", "imessage", "mbox", "github",
];

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SourceConfig {
    pub kind: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Kind-specific settings (url, token, path, self_email, ...).
    #[serde(flatten)]
    pub settings: BTreeMap<String, toml::Value>,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub sources: BTreeMap<String, SourceConfig>,
}

pub fn config_path() -> PathBuf {
    if let Ok(p) = std::env::var("PKG_CONFIG") {
        return PathBuf::from(p);
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(home).join("pkg").join("config.toml")
}

pub fn load_config() -> Result<Config> {
    let path = config_path();
    if !path.exists() {
        return Ok(Config::default());
    }
    let text = std::fs::read_to_string(&path)?;
    toml::from_str(&text).map_err(|e| Error::Parse(format!("{}: {e}", path.display())))
}

pub fn save_config(config: &Config) -> Result<()> {
    let path = config_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let text = toml::to_string_pretty(config).map_err(|e| Error::Other(e.to_string()))?;
    std::fs::write(&path, text)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

impl SourceConfig {
    pub fn get_str(&self, key: &str) -> Option<&str> {
        self.settings.get(key).and_then(|v| v.as_str())
    }

    fn require(&self, key: &str) -> Result<&str> {
        self.get_str(key)
            .ok_or_else(|| Error::Other(format!("source setting '{key}' missing")))
    }

    /// Retention policy (`retention = "keep" | "capture" | "capture_delete"`,
    /// default keep). Sessions/slack ignore it (no files of their own).
    pub fn retention(&self) -> Retention {
        self.get_str("retention")
            .and_then(Retention::parse)
            .unwrap_or_default()
    }
}

/// Defaults registered on first run: the zero-config local sources.
pub fn ensure_defaults(config: &mut Config) -> bool {
    let mut changed = false;
    for (name, kind) in [("bee", "bee"), ("sessions", "sessions")] {
        if !config.sources.contains_key(name) {
            config.sources.insert(
                name.to_string(),
                SourceConfig {
                    kind: kind.to_string(),
                    enabled: true,
                    settings: BTreeMap::new(),
                },
            );
            changed = true;
        }
    }
    changed
}

// ─── Auth / connectivity tests (no writes) ───────────────────────────────────

#[derive(Debug, Serialize)]
pub struct TestResult {
    pub ok: bool,
    pub detail: String,
}

pub fn test_source(_name: &str, cfg: &SourceConfig) -> TestResult {
    let fail = |d: String| TestResult {
        ok: false,
        detail: d,
    };
    let pass = |d: String| TestResult {
        ok: true,
        detail: d,
    };

    match cfg.kind.as_str() {
        "bee" => {
            let streaming = cfg.get_str("mode") == Some("stream");
            let root = crate::sources::bee::BeeSource::default_root();
            let cli = std::process::Command::new("bee").arg("status").output();
            match cli {
                Ok(out) if out.status.success() => {
                    let text = String::from_utf8_lossy(&out.stdout);
                    let verified = text.lines().find(|l| l.contains("Verified"));
                    let mode = if streaming {
                        "stream (API → DB, no mirror)".to_string()
                    } else {
                        format!(
                            "mirror {}",
                            if root.exists() { "present" } else { "MISSING" }
                        )
                    };
                    pass(format!(
                        "{} · {mode}",
                        verified.unwrap_or("authenticated").trim()
                    ))
                }
                Ok(_) => fail("`bee status` failed — run `bee login`".into()),
                Err(_) => {
                    if !streaming && root.exists() {
                        pass("bee CLI not found; stale mirror only".into())
                    } else {
                        fail("bee CLI not found".into())
                    }
                }
            }
        }
        "sessions" => {
            let h = crate::sources::sessions::HermesSource::default_path();
            let c = crate::sources::sessions::ClaudeSource::default_path();
            pass(format!(
                "hermes {} · claude {}",
                if h.exists() { "ok" } else { "missing" },
                if c.exists() { "ok" } else { "missing" }
            ))
        }
        "ics" => {
            if let Some(url) = cfg.get_str("url") {
                match ureq::get(url)
                    .timeout(std::time::Duration::from_secs(20))
                    .call()
                {
                    Ok(resp) => {
                        let body = resp.into_string().unwrap_or_default();
                        let n = crate::sources::ics::parse_ics(&body).len();
                        pass(format!("fetched OK, {n} events"))
                    }
                    Err(e) => fail(format!("fetch failed: {e}")),
                }
            } else if let Some(path) = cfg.get_str("path") {
                if std::path::Path::new(path).exists() {
                    pass(format!("file present: {path}"))
                } else {
                    fail(format!("file missing: {path}"))
                }
            } else {
                fail("needs 'url' or 'path'".into())
            }
        }
        "slack" => match cfg.get_str("token") {
            None => fail("no token — pkg source add slack --token xoxp-…".into()),
            Some(token) => match crate::sources::slack::auth_test(token) {
                Ok((team, user)) => pass(format!("authenticated: {user} @ {team}")),
                Err(e) => fail(format!("auth.test failed: {e}")),
            },
        },
        "imessage" => match cfg.get_str("db") {
            None => fail("no 'db' path — sync chat.db from the Mac first".into()),
            Some(db) => {
                if !std::path::Path::new(db).exists() {
                    return fail(format!("db missing: {db}"));
                }
                match crate::sources::imessage::probe(db) {
                    Ok(n) => pass(format!("{n} messages visible")),
                    Err(e) => fail(format!("cannot read chat.db: {e}")),
                }
            }
        },
        "github" => match crate::sources::github::resolve_token(cfg) {
            Err(e) => fail(e.to_string()),
            Ok(token) => match crate::sources::github::auth_user(&token) {
                Ok(login) => pass(format!("authenticated: {login}")),
                Err(e) => fail(format!("auth failed: {e}")),
            },
        },
        "mbox" => match cfg.get_str("path") {
            None => fail("no 'path' — point at an mbox export (e.g. Gmail Takeout)".into()),
            Some(p) => {
                if std::path::Path::new(p).exists() {
                    pass(format!("file present: {p}"))
                } else {
                    fail(format!("file missing: {p}"))
                }
            }
        },
        other => fail(format!("unknown kind '{other}'")),
    }
}

// ─── Sync dispatch ───────────────────────────────────────────────────────────

/// Run ingestion for one configured source. `full` ignores the stored cursor.
pub fn sync_source(
    conn: &Connection,
    _name: &str,
    cfg: &SourceConfig,
    full: bool,
) -> Result<IngestReport> {
    let cursor_for = |source_id: &str| -> Result<Option<String>> {
        if full {
            Ok(None)
        } else {
            sources::get_cursor(conn, source_id)
        }
    };

    match cfg.kind.as_str() {
        "bee" => {
            if cfg.get_str("mode") == Some("stream") {
                // Streaming: API → encrypted DB. Plaintext never touches disk;
                // raw is always archived (it's the only copy), so Keep is
                // promoted to Capture.
                let retention = match cfg.retention() {
                    Retention::Keep => Retention::Capture,
                    r => r,
                };
                let src = sources::bee::BeeStreamSource;
                let report =
                    sources::ingest_with(conn, &src, cursor_for("bee")?.as_deref(), retention)?;
                sources::bee::enrich_from_stream(conn)?;
                return Ok(report);
            }
            // Mirror mode: refresh first (incremental); ignore failure — a
            // stale mirror still ingests.
            let _ = std::process::Command::new("bee").arg("sync").status();
            let src = sources::bee::BeeSource::new(sources::bee::BeeSource::default_root());
            // capture_delete needs a full pass (not cursor-limited) so files
            // ingested before the policy flip get archived+deleted too.
            let retention = cfg.retention();
            let since = if retention == Retention::CaptureDelete {
                None
            } else {
                cursor_for("bee")?
            };
            let report = sources::ingest_with(conn, &src, since.as_deref(), retention)?;
            sources::bee::enrich_from_native(conn, &src.root)?;
            Ok(report)
        }
        "sessions" => {
            let h = sources::sessions::HermesSource::new(
                sources::sessions::HermesSource::default_path(),
            );
            let mut report = sources::ingest(conn, &h, cursor_for("session.hermes")?.as_deref())?;
            let c = sources::sessions::ClaudeSource::new(
                sources::sessions::ClaudeSource::default_path(),
            );
            let r2 = sources::ingest(conn, &c, cursor_for("session.claude")?.as_deref())?;
            report.inserted += r2.inserted;
            report.updated += r2.updated;
            report.unchanged += r2.unchanged;
            report.mentions += r2.mentions;
            Ok(report)
        }
        "ics" => {
            let self_emails: Vec<String> = cfg
                .get_str("self_email")
                .map(|s| s.split(',').map(|x| x.trim().to_string()).collect())
                .unwrap_or_default();
            // No cursor for calendars: they are not append-only in time — a
            // future event would advance the cursor past yet-unseen earlier
            // events. Re-parse is idempotent and cheap; edits get picked up.
            if let Some(url) = cfg.get_str("url") {
                // Streaming: fetch → parse in memory → encrypted DB. No
                // plaintext cache file.
                let body = ureq::get(url)
                    .timeout(std::time::Duration::from_secs(60))
                    .call()
                    .map_err(|e| Error::Other(format!("calendar fetch failed: {e}")))?
                    .into_string()
                    .map_err(|e| Error::Other(e.to_string()))?;
                sources::ics::ingest_ics_text(conn, &body, &self_emails, None)
            } else {
                let path = PathBuf::from(cfg.require("path")?);
                let src = sources::ics::IcsSource::new(vec![path], self_emails);
                sources::ics::ingest_ics(conn, &src, None)
            }
        }
        "slack" => {
            let src = sources::slack::SlackSource::from_config(cfg)?;
            sources::slack::ingest_slack(conn, &src, cursor_for("slack")?.as_deref())
        }
        "github" => {
            let src = sources::github::GithubSource::from_config(cfg)?;
            sources::github::ingest_github(conn, &src, cursor_for("github")?.as_deref())
        }
        "imessage" => {
            let src = sources::imessage::IMessageSource::from_config(cfg)?;
            let retention = cfg.retention();
            let since = if retention == Retention::CaptureDelete {
                None
            } else {
                cursor_for("sms")?
            };
            let mut report = sources::ingest_with(conn, &src, since.as_deref(), retention)?;
            // chat.db is one shared file: delete only after the whole pass
            // succeeded (every episode carried raw, so all are archived).
            // The next Mac-side rsync recreates it.
            if retention == Retention::CaptureDelete && src.db.exists() {
                std::fs::remove_file(&src.db)?;
                report.deleted_files += 1;
            }
            Ok(report)
        }
        "mbox" => {
            let src = sources::mbox::MboxSource::from_config(cfg)?;
            let retention = cfg.retention();
            let since = if retention == Retention::CaptureDelete {
                None
            } else {
                cursor_for("email.mbox")?
            };
            let mut report = sources::ingest_with(conn, &src, since.as_deref(), retention)?;
            // One shared archive file — delete after the full pass succeeded.
            if retention == Retention::CaptureDelete && src.path.exists() {
                std::fs::remove_file(&src.path)?;
                report.deleted_files += 1;
            }
            Ok(report)
        }
        other => Err(Error::Other(format!("unknown source kind '{other}'"))),
    }
}
