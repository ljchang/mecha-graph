//! A per-episode hold on a shared model router, in the file protocol mecha's
//! `hold.rs` speaks — so a model switch there waits for the episode in
//! flight, and an episode does not start while a switch is pending.
//!
//! **Why.** Behind a llama-server router the request's `model` field selects,
//! and mecha switches the owner's model by loading another. A switch waits
//! for every run that holds the router (mecha's D13); a client that holds
//! nothing is cut off mid-request and, retrying, loads its model back. On
//! 2026-09-28 a nightly extraction did exactly that to the owner's switch.
//! Following the router per episode (`ChatClient::follow`) makes the next
//! episode use the new model; the hold is what keeps a switch from landing
//! mid-episode, so one episode is answered by one model.
//!
//! **Off unless asked.** The directory comes from `MECHA_GRAPH_HOLDS_DIR`
//! (`scripts/nightly.sh` points it at `~/.mecha/holds`) and must already
//! exist — this never creates one, and knows nothing else about mecha. The
//! core crate knows nothing of it either (lib.rs rule 1): it takes a gate.
//!
//! **The wire format is mecha's, and pinned by a test on each side**
//! (`the_files_are_the_ones_mecha_reads` here). A hold is
//! `<pid>-<uuid>.hold`, JSON `{pid, base_url, what, taken_at}`, written whole
//! through a temp sibling; a pending switch is `switch-<slug>.json`, where the
//! slug is the router's base URL with every non-alphanumeric byte as `_`.
//! The handshake is mecha's: write the hold, *then* look for a switch, and
//! yield — drop the hold, wait for the switch to clear, try again.
//!
//! **Only the batch holds.** `extract --episode` (one interactive re-run)
//! takes no hold: a switch mid-run costs one re-runnable episode, and it
//! still follows the router on every request.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// How often a waiting episode looks for the switch to have cleared.
const POLL: Duration = Duration::from_millis(500);

/// How long a switch file nobody can read is waited on before extraction
/// stops and says so. A live switch is waited on without limit — it ends, or
/// its switcher dies and stops counting — but an unreadable file names no
/// switcher to outlive, so without a bound it would hang the whole nightly
/// run, which then reads as neither clean nor charged (found on review).
/// Stopping is still the fail-closed direction: nothing runs under a switch
/// that might be real, and the run is retried the next night.
const UNREADABLE_LIMIT: Duration = Duration::from_secs(600);

/// What the switch file says.
#[derive(Debug, PartialEq, Eq)]
enum Pending {
    None,
    /// A switch by a live process, to this model.
    Live(String),
    /// A file that exists and cannot be read as a switch — pending, the
    /// fail-closed way, but only for [`UNREADABLE_LIMIT`].
    Unreadable,
}

pub struct Holds {
    dir: PathBuf,
    base: String,
}

/// An episode's hold, released on drop — with the cancel file a "switch now"
/// may have written beside it, so it cannot reach whatever holds next.
pub struct Held {
    path: PathBuf,
}

impl Drop for Held {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
        let _ = std::fs::remove_file(self.path.with_extension("cancel"));
    }
}

/// mecha's `router::base`: one router, however its URL is spelled.
fn base(url: &str) -> String {
    url.trim_end_matches('/')
        .trim_end_matches("/v1")
        .trim_end_matches('/')
        .to_string()
}

/// Is a process with this pid running? `None` when this host has no
/// `/proc` to ask — a check that cannot run, which must not read as "gone"
/// and so as "no switch" (found on review).
fn alive(pid: u64) -> Option<bool> {
    Path::new("/proc/self")
        .exists()
        .then(|| pid > 0 && Path::new(&format!("/proc/{pid}")).exists())
}

impl Holds {
    /// From `MECHA_GRAPH_HOLDS_DIR`, when it names a directory that exists.
    pub fn from_env(base_url: &str) -> Option<Holds> {
        Holds::from_dir(std::env::var_os("MECHA_GRAPH_HOLDS_DIR"), base_url)
    }

    /// The pure half of [`from_env`](Self::from_env): off when unset, empty,
    /// or naming no existing directory — which is what `nightly.sh`'s default
    /// gives a box where mecha never made `~/.mecha/holds`.
    fn from_dir(value: Option<std::ffi::OsString>, base_url: &str) -> Option<Holds> {
        let dir = PathBuf::from(value.filter(|v| !v.is_empty())?);
        dir.is_dir().then(|| Holds::new(dir, base_url))
    }

    fn new(dir: PathBuf, base_url: &str) -> Holds {
        Holds {
            dir,
            base: base(base_url),
        }
    }

    fn switch_path(&self) -> PathBuf {
        let slug: String = self
            .base
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect();
        self.dir.join(format!("switch-{slug}.json"))
    }

    /// Is a switch pending on this router? One whose switcher is gone is not
    /// (mecha sweeps it). A file this cannot read as a switch — not JSON, or
    /// a `pid` that is not a number — is [`Pending::Unreadable`]: never "no
    /// switch", which would run straight through one if mecha ever renamed or
    /// re-typed the field (found on review).
    fn pending(&self) -> Pending {
        let path = self.switch_path();
        let text = match std::fs::read_to_string(&path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Pending::None,
            Err(_) => return Pending::Unreadable,
        };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) else {
            return Pending::Unreadable;
        };
        let Some(pid) = v.get("pid").and_then(|p| p.as_u64()) else {
            return Pending::Unreadable;
        };
        match alive(pid) {
            Some(false) => return Pending::None,
            None => return Pending::Unreadable,
            Some(true) => {}
        }
        Pending::Live(
            v.get("to")
                .and_then(|t| t.as_str())
                .unwrap_or("another model")
                .to_string(),
        )
    }

    fn write_hold(&self, what: &str) -> std::io::Result<Held> {
        let path = self.dir.join(format!(
            "{}-{}.hold",
            std::process::id(),
            uuid::Uuid::new_v4().simple()
        ));
        let hold = serde_json::json!({
            "pid": std::process::id(),
            "base_url": self.base,
            "what": what,
            "taken_at": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Nanos, true),
        });
        let tmp = path.with_extension("hold.tmp");
        std::fs::write(&tmp, hold.to_string())?;
        std::fs::rename(&tmp, &path)?;
        Ok(Held { path })
    }

    /// Hold the router for one episode, first waiting out any switch that is
    /// pending — without limit, as every mecha run does: the switch is the
    /// owner's, and it ends, or its switcher dies and it stops counting.
    pub fn enter(&self, what: &str) -> std::io::Result<Held> {
        self.enter_within(what, UNREADABLE_LIMIT)
    }

    fn enter_within(&self, what: &str, unreadable_limit: Duration) -> std::io::Result<Held> {
        let mut said = false;
        let mut unreadable_since: Option<std::time::Instant> = None;
        loop {
            let held = self.write_hold(what)?;
            let mut state = self.pending();
            if state == Pending::None {
                return Ok(held);
            }
            drop(held);
            if !std::mem::replace(&mut said, true) {
                match &state {
                    Pending::Live(to) => {
                        eprintln!("mecha-graph: the router is switching to {to}; waiting for it")
                    }
                    _ => eprintln!(
                        "mecha-graph: {} cannot be read; waiting for it to clear",
                        self.switch_path().display()
                    ),
                }
            }
            while state != Pending::None {
                if state == Pending::Unreadable {
                    let since = *unreadable_since.get_or_insert_with(std::time::Instant::now);
                    if since.elapsed() >= unreadable_limit {
                        return Err(std::io::Error::other(format!(
                            "{} has been unreadable for {}s, so whether a model switch is \
                             pending is unknown — stopping rather than extract under one. \
                             `mecha model cancel-switch` withdraws it.",
                            self.switch_path().display(),
                            unreadable_limit.as_secs()
                        )));
                    }
                } else {
                    unreadable_since = None;
                }
                std::thread::sleep(POLL);
                state = self.pending();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!(
            "mecha-graph-holds-{name}-{}",
            uuid::Uuid::new_v4().simple()
        ));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// The files are the ones mecha's `hold.rs` reads and writes: the hold's
    /// name and fields, and the switch file's name for a router spelled with
    /// a trailing `/v1/`. mecha pins the same shapes from its side; change
    /// one and the other's test is the one that fails.
    #[test]
    fn the_files_are_the_ones_mecha_reads() {
        let d = dir("wire");
        let h = Holds::new(d.clone(), "http://127.0.0.1:8080/v1/");
        assert_eq!(h.switch_path(), d.join("switch-http___127_0_0_1_8080.json"));
        let held = h.enter("mecha-graph extract").unwrap();
        let name = held.path.file_name().unwrap().to_string_lossy().to_string();
        let (pid, rest) = name.split_once('-').unwrap();
        assert_eq!(pid, std::process::id().to_string());
        assert!(rest.ends_with(".hold") && rest.len() == 32 + 5, "{name}");
        let v: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&held.path).unwrap()).unwrap();
        assert_eq!(v["pid"], std::process::id());
        assert_eq!(v["base_url"], "http://127.0.0.1:8080");
        assert_eq!(v["what"], "mecha-graph extract");
        assert!(
            chrono::DateTime::parse_from_rfc3339(v["taken_at"].as_str().unwrap()).is_ok(),
            "{v}"
        );
        let path = held.path.clone();
        std::fs::write(path.with_extension("cancel"), b"switch now\n").unwrap();
        drop(held);
        assert!(!path.exists() && !path.with_extension("cancel").exists());
    }

    /// An episode does not start under a pending switch: it waits until the
    /// switch clears, holding nothing meanwhile — so the switch, which waits
    /// for holds, is not waiting on it.
    #[test]
    fn an_episode_waits_out_a_pending_switch_holding_nothing() {
        let d = dir("wait");
        let h = Holds::new(d.clone(), "http://127.0.0.1:8080");
        // A switch by a live process: this one.
        let switch = serde_json::json!({
            "pid": std::process::id(), "base_url": "http://127.0.0.1:8080",
            "from": "a", "to": "b", "started_at": "2026-09-28T03:21:40Z",
        });
        std::fs::write(h.switch_path(), switch.to_string()).unwrap();
        let entering = {
            let d = d.clone();
            std::thread::spawn(move || {
                Holds::new(d, "http://127.0.0.1:8080").enter("mecha-graph extract")
            })
        };
        std::thread::sleep(Duration::from_millis(1200));
        let holds = || {
            std::fs::read_dir(&d)
                .unwrap()
                .flatten()
                .filter(|e| e.file_name().to_string_lossy().ends_with(".hold"))
                .count()
        };
        assert_eq!(holds(), 0, "held the router under a pending switch");
        assert!(!entering.is_finished(), "started under a pending switch");
        std::fs::remove_file(h.switch_path()).unwrap();
        let held = entering.join().unwrap().unwrap();
        assert!(held.path.exists(), "did not hold once the switch cleared");
    }

    /// A dead switcher's file is not waited on, and an unreadable one is
    /// (mecha's fail-closed direction).
    #[test]
    fn a_dead_switch_is_ignored_and_an_unreadable_one_is_not() {
        let d = dir("dead");
        let h = Holds::new(d, "http://127.0.0.1:8080");
        let dead = serde_json::json!({ "pid": u32::MAX - 1, "to": "b" });
        std::fs::write(h.switch_path(), dead.to_string()).unwrap();
        assert_eq!(h.pending(), Pending::None);
        std::fs::write(h.switch_path(), b"not json").unwrap();
        assert_eq!(h.pending(), Pending::Unreadable);
        // The two keys read out of mecha's switch file, pinned: a pid that is
        // not a number is unreadable, never "no switch".
        let mecha_shaped = serde_json::json!({
            "pid": std::process::id(), "base_url": "http://127.0.0.1:8080",
            "from": "a", "to": "b", "started_at": "2026-09-28T03:21:40.1Z",
        });
        std::fs::write(h.switch_path(), mecha_shaped.to_string()).unwrap();
        assert_eq!(h.pending(), Pending::Live("b".into()));
        let retyped = serde_json::json!({ "pid": std::process::id().to_string(), "to": "b" });
        std::fs::write(h.switch_path(), retyped.to_string()).unwrap();
        assert_eq!(h.pending(), Pending::Unreadable);
    }

    /// An unreadable switch file is waited on, then stops extraction with an
    /// error naming it — never a hang (found on review).
    #[test]
    fn an_unreadable_switch_file_stops_extraction_rather_than_hangs() {
        let d = dir("unreadable");
        let h = Holds::new(d, "http://127.0.0.1:8080");
        std::fs::write(h.switch_path(), b"not json").unwrap();
        let err = h
            .enter_within("mecha-graph extract", Duration::from_millis(700))
            .err()
            .expect("held under an unreadable switch file");
        assert!(err.to_string().contains("cancel-switch"), "{err}");
    }

    /// Off unless the directory is named, non-empty and exists — the
    /// set-but-missing case is what nightly.sh's default gives a box without
    /// mecha. Pure, so no test touches the process environment.
    #[test]
    fn holds_are_off_without_an_existing_directory() {
        let base = "http://127.0.0.1:8080";
        assert!(Holds::from_dir(None, base).is_none());
        assert!(Holds::from_dir(Some("".into()), base).is_none());
        assert!(Holds::from_dir(Some("/nonexistent/mecha/holds".into()), base).is_none());
        assert!(Holds::from_dir(Some(dir("on").into_os_string()), base).is_some());
    }
}
