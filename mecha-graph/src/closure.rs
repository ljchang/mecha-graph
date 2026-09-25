//! Closing a board task through mecha, when the owner has opted in.
//!
//! A task moved to `done` or `dropped`, or reopened from one, is a verdict
//! mecha records and appraises (its closure record, `~/.mecha/closures`), and
//! the TUI's status keys used to write it straight to this database, where
//! nothing saw it. With `[board] close_through` set in
//! `~/.mecha-graph/config.toml`, a move across the open/closed line is handed
//! to that program as
//!
//! ```text
//! mecha tasks set <task> --status <status> --surface graph-tui [--only-open]
//! ```
//!
//! and nothing else is. Moves between open statuses (`next` → `waiting`) stay
//! direct writes: they are not verdicts, and mecha records none. `done` ↔
//! `dropped` is the other way round — a verdict change mecha records no move
//! for — so, opted in, it is refused: reopen, then close.
//!
//! **Opted in, the route never degrades.** A program that cannot be found, a
//! config that cannot be read, or a database other than the one mecha's graph
//! server opens refuses the move with nothing written — a silent fallback to
//! the direct write would be exactly the unrecorded closure the opt-in exists
//! to rule out. "The one mecha's graph server opens" is the default database
//! (`$HOME/.mecha-graph/graph.db`): the child is started with
//! `MECHA_GRAPH_DB` removed, so the server it spawns resolves the default, and
//! a TUI opened on a fork or another `--db` would close a task on a board it
//! is not showing. After the program reports success the row is read back from
//! this database, so a server that was somewhere else after all is said
//! rather than believed.
//!
//! **Not opted in, nothing changes**: the direct write, as it always was.
//! The standalone install knows nothing about mecha, and neither does
//! `mecha-graph-core` (its rule 1) — the argv above lives here, in the binary.

use mecha_graph_core::gtd;
use mecha_graph_core::rusqlite::Connection;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// The surface mecha records a closure from this TUI under.
pub const SURFACE: &str = "graph-tui";

/// The statuses that close a task — mecha's `closure::CLOSED_STATUSES`.
const CLOSED: [&str; 2] = ["done", "dropped"];

fn is_closed(status: &str) -> bool {
    CLOSED.contains(&status)
}

/// Whether a status change is a close or a reopen — the moves mecha records.
pub fn crosses_line(from: &str, to: &str) -> bool {
    is_closed(from) != is_closed(to)
}

/// How one status change is to be made.
#[derive(Debug, PartialEq, Eq)]
pub enum Route {
    /// Straight to this database: not opted in, or not a close or a reopen.
    Direct,
    /// Through the opted-in program, found at this path.
    Through(PathBuf),
    /// Opted in (or unable to tell), and the move cannot be made through the
    /// program — nothing is written, and this says why.
    Refuse(String),
}

/// Decide the route. `close_through` is the configured program — `Err` when
/// the config could not be read, since then whether the owner opted in is
/// unknown. `db` is the database this TUI has open, `served` the one mecha's
/// graph server opens, `path` the `PATH` a bare program name is looked up on.
pub fn route(
    close_through: Result<Option<&str>, &str>,
    db: &Path,
    served: Option<&Path>,
    from: &str,
    to: &str,
    path: Option<&OsStr>,
) -> Route {
    // `done` ↔ `dropped` crosses no line, so mecha records nothing for it —
    // yet it changes the verdict a recorded closure carries. Opted in, it is
    // refused below rather than written behind the record's back.
    let reclose = is_closed(from) && is_closed(to) && from != to;
    if !crosses_line(from, to) && !reclose {
        return Route::Direct;
    }
    // The status bar is one row, clipped at the terminal's width, and every
    // refusal is shown after "nothing was changed: " — so each reason leads
    // with what matters and stays short (review of #21).
    let program = match close_through {
        Ok(None) => return Route::Direct,
        Ok(Some(p)) => p,
        Err(why) => {
            return Route::Refuse(format!(
                "config unreadable, so whether closures go through mecha is unknown ({why})"
            ))
        }
    };
    if reclose {
        return Route::Refuse(format!(
            "already {from}; reopen it, then close it as {to} ({from} → {to} would be \
             recorded nowhere)"
        ));
    }
    let Some(served) = served else {
        return Route::Refuse("HOME is unset, so mecha's graph database is unknown".into());
    };
    if !same_file(db, served) {
        return Route::Refuse(format!(
            "this TUI is not on mecha's graph database ({} is not {})",
            db.display(),
            served.display()
        ));
    }
    match find_program(program, path) {
        Some(exe) => Route::Through(exe),
        None => Route::Refuse(format!("close_through {program:?} not found")),
    }
}

/// The database mecha's graph server opens: the default path, resolved
/// without `MECHA_GRAPH_DB` — which is removed from the child's environment
/// for exactly that reason. `None` without a `HOME`: `db::default_db_path`
/// would fall back to the working directory there, and a guess is not a
/// database anyone can vouch for.
pub fn served_db(home: Option<&OsStr>) -> Option<PathBuf> {
    Some(PathBuf::from(home?).join(".mecha-graph").join("graph.db"))
}

/// Two paths name the same file. A path that cannot be resolved names
/// nothing that can be proved the same — unknown is never a match.
fn same_file(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(a), Ok(b)) => a == b,
        _ => false,
    }
}

/// A program named with a slash is that file (a leading `~/` is the home
/// directory, as a shell would read it); a bare name is looked up on `path`.
fn find_program(program: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let program = program.trim();
    if program.is_empty() {
        return None;
    }
    if program.contains('/') {
        let p = expand_home(program, std::env::var_os("HOME").as_deref())?;
        return is_executable(&p).then_some(p);
    }
    std::env::split_paths(path?)
        .map(|dir| dir.join(program))
        .find(|p| is_executable(p))
}

/// `~/x` under `home`; any other path as written. `~/` with no home names
/// nothing.
fn expand_home(program: &str, home: Option<&OsStr>) -> Option<PathBuf> {
    match program.strip_prefix("~/") {
        Some(rest) => Some(PathBuf::from(home?).join(rest)),
        None => Some(PathBuf::from(program)),
    }
}

fn is_executable(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    p.metadata()
        .is_ok_and(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
}

/// The command line handed to the program. `--only-open` on a close: the
/// row may have been closed elsewhere since this screen read it, and a close
/// of a closed task would turn `done` into `dropped` with no record.
pub fn argv(task: &str, to: &str) -> Vec<String> {
    let mut a: Vec<String> = ["tasks", "set", task, "--status", to, "--surface", SURFACE]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if is_closed(to) {
        a.push("--only-open".into());
    }
    a
}

/// How long a close through mecha may take before it is stopped. It starts a
/// graph server and appraises the closure — seconds, normally; the bound is
/// for a child that never answers, since the TUI waits on it.
pub const TIMEOUT: Duration = Duration::from_secs(120);

/// Make one status change the way [`route`] says, and answer with the line
/// the status bar shows — `Err` when nothing changed or when what changed
/// cannot be confirmed.
pub fn set_status(conn: &Connection, route: Route, task: &str, to: &str) -> Result<String, String> {
    set_status_within(conn, route, task, to, TIMEOUT)
}

/// How the program ended.
enum Ended {
    Exited(std::process::ExitStatus),
    /// Still running at the deadline, and stopped.
    TimedOut,
    /// Started, and then the wait on it failed.
    Lost(String),
}

/// [`set_status`], with the wait bounded by `timeout`.
///
/// **Every ending is checked against this board**, not only success: mecha
/// can move the board and then fail (its "outcome unknown" path), and a
/// failure taken on its word would tell the owner nothing changed while a
/// retry — now `done` to `done`, which routes direct — reported a success
/// mecha never saw.
///
/// The child's output goes to files, never pipes: a pipe is only at its end
/// when *every* holder closes it, and a graph server the child started could
/// hold it past the child's own exit, freezing the TUI on the alternate
/// screen. The wait is on the child process itself, with a deadline.
pub fn set_status_within(
    conn: &Connection,
    route: Route,
    task: &str,
    to: &str,
    timeout: Duration,
) -> Result<String, String> {
    let exe = match route {
        Route::Direct => {
            return gtd::set_task_status(conn, task, to)
                .map(|()| String::new())
                .map_err(|e| e.to_string())
        }
        // Every message below leads with its verdict: the status bar is one
        // row clipped at the terminal's width (review of #21).
        Route::Refuse(why) => return Err(format!("nothing was changed: {why}")),
        Route::Through(exe) => exe,
    };
    let (ended, stderr) = match run_bounded(&exe, &argv(task, to), timeout) {
        Ok(r) => r,
        // Refused before the child existed: nothing ran, nothing moved.
        Err(Spawn::NotStarted(e)) => {
            return Err(format!(
                "nothing was changed: {} could not be run ({e})",
                exe.display()
            ))
        }
        // Started, then lost track of: the board decides, like any ending.
        Err(Spawn::Lost(e)) => (Ended::Lost(e.to_string()), String::new()),
    };
    // The reason is the last line that is not the appraisal: a failure after
    // the appraisal printed must not be reported as the appraisal.
    let appraisal = stderr
        .lines()
        .find_map(|l| l.strip_prefix("mecha's appraisal of "))
        .and_then(|l| l.split_once(": ").map(|(_, r)| r.trim().to_string()));
    let last = stderr
        .lines()
        .map(str::trim)
        .rfind(|l| !l.is_empty() && !l.starts_with("mecha's appraisal of "))
        .unwrap_or("it exited without saying why")
        .to_string();
    // mecha's word is not this board's: read the row back from the database
    // this screen shows, however the child ended.
    let now = gtd::get_task(conn, task)
        .map_err(|e| {
            format!("unknown whether it moved: mecha ran, and {task} could not be re-read ({e})")
        })?
        .map(|t| t.status);
    let landed = now.as_deref() == Some(to);
    let reads = now.as_deref().unwrap_or("no such task");
    let secs = timeout.as_secs();
    match ended {
        Ended::Exited(status) if status.success() && landed => Ok(match appraisal {
            Some(a) => format!("recorded by mecha — {a}"),
            None => "recorded by mecha".to_string(),
        }),
        Ended::Exited(status) if status.success() => Err(format!(
            "not on this board: mecha reported the move, but this board reads {reads} — its \
             graph server may be on another database"
        )),
        Ended::Exited(_) if landed => Ok(format!(
            "landed, record uncertain: mecha ended with an error ({last})"
        )),
        Ended::Exited(_) => Err(format!("nothing was changed: mecha refused — {last}")),
        Ended::TimedOut if landed => Ok(format!(
            "landed, appraisal may not have run: mecha was stopped after {secs}s"
        )),
        Ended::TimedOut => Err(format!(
            "nothing was changed: mecha was stopped after {secs}s; the board reads {reads}"
        )),
        Ended::Lost(e) if landed => Ok(format!("landed, record uncertain: mecha was lost ({e})")),
        Ended::Lost(e) => Err(format!(
            "nothing was changed here: mecha was lost ({e}); the board reads {reads}"
        )),
    }
}

/// Why [`run_bounded`] could not report an ending.
enum Spawn {
    /// Before the child existed — its output files, or the spawn itself.
    NotStarted(std::io::Error),
    /// After: the wait itself failed.
    Lost(std::io::Error),
}

/// Run `exe` with its output in owner-only scratch files, wait for the
/// process (not its output) up to `timeout`, and hand back how it ended and
/// what it wrote to stderr.
fn run_bounded(exe: &Path, args: &[String], timeout: Duration) -> Result<(Ended, String), Spawn> {
    use std::os::unix::fs::OpenOptionsExt;
    let stem = std::env::temp_dir().join(format!(
        "mecha-graph-close-{}-{}",
        std::process::id(),
        mecha_graph_core::ids::new_uid()
    ));
    let (out_path, err_path) = (stem.with_extension("out"), stem.with_extension("err"));
    let open = |p: &Path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(p)
    };
    let result = (|| {
        let mut child = std::process::Command::new(exe)
            .args(args)
            .env_remove("MECHA_GRAPH_DB")
            .stdin(std::process::Stdio::null())
            .stdout(open(&out_path).map_err(Spawn::NotStarted)?)
            .stderr(open(&err_path).map_err(Spawn::NotStarted)?)
            .spawn()
            .map_err(Spawn::NotStarted)?;
        let deadline = Instant::now() + timeout;
        let ended = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ended::Exited(status),
                Ok(None) => {}
                Err(e) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(Spawn::Lost(e));
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                break Ended::TimedOut;
            }
            std::thread::sleep(Duration::from_millis(50));
        };
        let stderr = std::fs::read_to_string(&err_path).unwrap_or_default();
        Ok((ended, stderr))
    })();
    let _ = std::fs::remove_file(&out_path);
    let _ = std::fs::remove_file(&err_path);
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use mecha_graph_core::db;

    /// A scratch directory holding a real database file, removed on drop.
    struct Scratch(PathBuf);

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn scratch(tag: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!(
            "mecha-graph-closure-{tag}-{}-{}",
            std::process::id(),
            mecha_graph_core::ids::new_uid()
        ));
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::create_dir_all(dir.join(".mecha-graph")).unwrap();
        Scratch(dir)
    }

    /// The database at the default location under `home`, one open task.
    fn board(home: &Path) -> (Connection, PathBuf, String) {
        let path = served_db(Some(home.as_os_str())).unwrap();
        let conn = db::open(&path).unwrap();
        let id = gtd::create_task(&conn, "Book the vendor walkthrough", None, None, None).unwrap();
        gtd::set_task_status(&conn, &id, "next").unwrap();
        (conn, path, id)
    }

    /// A stand-in for mecha: records its argv, and — like mecha's graph
    /// server — writes the status to the default database under `home`,
    /// unless `apply` is false.
    fn fake_mecha(dir: &Path, home: &Path, exit: i32, apply: bool) -> PathBuf {
        fake_mecha_with(dir, home, exit, apply, "")
    }

    /// [`fake_mecha`], running the Python statement `extra` (one line, or
    /// empty) after the write and before it exits.
    fn fake_mecha_with(dir: &Path, home: &Path, exit: i32, apply: bool, extra: &str) -> PathBuf {
        let exe = dir.join("bin").join("mecha");
        let db = served_db(Some(home.as_os_str())).unwrap();
        let log = dir.join("argv.json");
        let script = format!(
            "#!/usr/bin/env python3\n\
             import json, os, sqlite3, subprocess, sys, time\n\
             a = sys.argv[1:]\n\
             json.dump({{'argv': a, 'db_env': os.environ.get('MECHA_GRAPH_DB')}}, open({log:?}, 'w'))\n\
             if {apply}:\n\
             \x20   c = sqlite3.connect({db:?})\n\
             \x20   c.execute('UPDATE task_detail SET status = ? WHERE node_id = ?', (a[a.index('--status') + 1], a[2]))\n\
             \x20   c.commit()\n\
             if {apply} and {exit} == 0:\n\
             \x20   sys.stderr.write(\"mecha's appraisal of %s: pride · +0.5\\n\" % a[2])\n\
             {extra}\n\
             if {exit} != 0:\n\
             \x20   sys.stderr.write('mecha: task-x is already done — nothing was changed\\n')\n\
             sys.exit({exit})\n",
            log = log.display().to_string(),
            db = db.display().to_string(),
            apply = if apply { "True" } else { "False" },
        );
        std::fs::write(&exe, script).unwrap();
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        exe
    }

    fn logged(dir: &Path) -> Option<serde_json::Value> {
        let text = std::fs::read_to_string(dir.join("argv.json")).ok()?;
        serde_json::from_str(&text).ok()
    }

    fn status(conn: &Connection, id: &str) -> String {
        gtd::get_task(conn, id).unwrap().unwrap().status
    }

    fn python() -> bool {
        std::process::Command::new("python3")
            .arg("--version")
            .output()
            .is_ok()
    }

    #[test]
    fn only_a_close_or_a_reopen_crosses_the_line() {
        assert!(crosses_line("next", "done"));
        assert!(crosses_line("waiting", "dropped"));
        assert!(crosses_line("done", "next"));
        assert!(!crosses_line("done", "dropped"));
        assert!(!crosses_line("next", "waiting"));
    }

    /// Not opted in, every move is the direct write it always was — the
    /// standalone install sees no difference, and no program is looked for.
    #[test]
    fn not_opted_in_the_direct_write_is_unchanged() {
        let s = scratch("direct");
        let (conn, db, id) = board(&s.0);
        let path = s.0.join("bin");
        let r = route(
            Ok(None),
            &db,
            Some(&db),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        assert_eq!(r, Route::Direct);
        assert_eq!(set_status(&conn, r, &id, "done"), Ok(String::new()));
        assert_eq!(status(&conn, &id), "done");
        assert!(logged(&s.0).is_none(), "no program ran");
    }

    /// Opted in, a close runs mecha with the graph-tui surface and the
    /// open-only guard, the TUI writes nothing itself, and the row mecha's
    /// server moved is read back from this database.
    #[test]
    fn opted_in_a_close_runs_mecha_on_the_graph_tui_surface() {
        if !python() {
            eprintln!("skipping: no python3 for the stand-in mecha");
            return;
        }
        let s = scratch("through");
        let (conn, db, id) = board(&s.0);
        fake_mecha(&s.0, &s.0, 0, true);
        let path = s.0.join("bin");
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        assert_eq!(r, Route::Through(s.0.join("bin/mecha")));
        let line = set_status(&conn, r, &id, "done").unwrap();
        assert!(line.contains("pride · +0.5"), "{line}");
        assert_eq!(status(&conn, &id), "done");
        let log = logged(&s.0).expect("mecha ran");
        assert_eq!(
            log["argv"],
            serde_json::json!([
                "tasks",
                "set",
                id,
                "--status",
                "done",
                "--surface",
                "graph-tui",
                "--only-open"
            ])
        );
        assert!(log["db_env"].is_null(), "MECHA_GRAPH_DB is removed: {log}");

        // A reopen goes the same way, without the open-only guard.
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "done",
            "next",
            Some(path.as_os_str()),
        );
        set_status(&conn, r, &id, "next").unwrap();
        let log = logged(&s.0).unwrap();
        assert_eq!(log["argv"][4], "next");
        assert!(!log["argv"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a == "--only-open"));

        // A move between open statuses is not a verdict: still direct.
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "next",
            "waiting",
            Some(path.as_os_str()),
        );
        assert_eq!(r, Route::Direct);
    }

    /// Opted in with mecha absent: refused, nothing written.
    #[test]
    fn opted_in_with_mecha_absent_refuses_and_writes_nothing() {
        let s = scratch("absent");
        let (conn, db, id) = board(&s.0);
        let path = s.0.join("bin");
        for program in ["mecha", "/nowhere/mecha"] {
            let r = route(
                Ok(Some(program)),
                &db,
                Some(&db),
                "next",
                "done",
                Some(path.as_os_str()),
            );
            assert!(
                matches!(r, Route::Refuse(ref why) if why.contains("not found")),
                "{r:?}"
            );
            let e = set_status(&conn, r, &id, "done").unwrap_err();
            assert!(e.contains("nothing was changed"), "{e}");
            assert_eq!(status(&conn, &id), "next");
        }
    }

    /// Opted in on a database that is not the one mecha's graph server
    /// opens — a fork, another `--db` — refused before mecha is looked for,
    /// and nothing written.
    #[test]
    fn opted_in_on_another_database_refuses_and_writes_nothing() {
        let s = scratch("fork");
        let (_, served, _) = board(&s.0);
        let fork = s.0.join("fork.db");
        let conn = db::open(&fork).unwrap();
        let id = gtd::create_task(&conn, "Book the vendor walkthrough", None, None, None).unwrap();
        gtd::set_task_status(&conn, &id, "next").unwrap();
        fake_mecha(&s.0, &s.0, 0, true);
        let path = s.0.join("bin");
        let r = route(
            Ok(Some("mecha")),
            &fork,
            Some(&served),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        assert!(
            matches!(r, Route::Refuse(ref why) if why.contains("fork.db")),
            "{r:?}"
        );
        assert!(set_status(&conn, r, &id, "done").is_err());
        assert_eq!(status(&conn, &id), "next");
        assert!(logged(&s.0).is_none(), "mecha never ran");
    }

    /// `done` → `dropped` crosses no line, so mecha would record nothing, yet
    /// it changes a recorded closure's verdict: opted in, it is refused and
    /// nothing is written; not opted in, it stays the direct write.
    #[test]
    fn opted_in_a_closed_task_is_not_reclosed_behind_the_record() {
        let s = scratch("reclose");
        let (conn, db, id) = board(&s.0);
        gtd::set_task_status(&conn, &id, "done").unwrap();
        fake_mecha(&s.0, &s.0, 0, true);
        let path = s.0.join("bin");
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "done",
            "dropped",
            Some(path.as_os_str()),
        );
        assert!(
            matches!(r, Route::Refuse(ref why) if why.contains("reopen it")),
            "{r:?}"
        );
        assert!(set_status(&conn, r, &id, "dropped").is_err());
        assert_eq!(status(&conn, &id), "done");
        assert!(logged(&s.0).is_none(), "mecha never ran");
        assert_eq!(
            route(Ok(None), &db, Some(&db), "done", "dropped", None),
            Route::Direct
        );
        // Re-asserting the same closed status changes no verdict.
        assert_eq!(
            route(Ok(Some("mecha")), &db, Some(&db), "done", "done", None),
            Route::Direct
        );
    }

    #[test]
    fn a_home_relative_program_is_read_as_a_shell_would() {
        let home = OsStr::new("/home/someone");
        assert_eq!(
            expand_home("~/.cargo/bin/mecha", Some(home)),
            Some(PathBuf::from("/home/someone/.cargo/bin/mecha"))
        );
        assert_eq!(expand_home("~/.cargo/bin/mecha", None), None);
        assert_eq!(
            expand_home("/usr/local/bin/mecha", None),
            Some(PathBuf::from("/usr/local/bin/mecha"))
        );
    }

    /// mecha can move the board and then fail (its unknown-outcome path): the
    /// board, re-read, is the answer — "landed", not "refused".
    #[test]
    fn a_failure_after_the_move_landed_is_reported_as_landed() {
        if !python() {
            eprintln!("skipping: no python3 for the stand-in mecha");
            return;
        }
        let s = scratch("landed");
        let (conn, db, id) = board(&s.0);
        fake_mecha(&s.0, &s.0, 1, true);
        let path = s.0.join("bin");
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        let line = set_status(&conn, r, &id, "done").unwrap();
        assert!(
            line.contains("landed") && line.contains("ended with an error"),
            "{line}"
        );
        assert_eq!(status(&conn, &id), "done");
    }

    /// A process mecha started that outlives it — its graph server — holds
    /// whatever mecha's stderr was. On a pipe that is an end that never
    /// comes; the wait is on the child, so the TUI is not frozen.
    #[test]
    fn a_process_left_holding_the_output_does_not_hold_the_tui() {
        if !python() {
            eprintln!("skipping: no python3 for the stand-in mecha");
            return;
        }
        let s = scratch("linger");
        let (conn, db, id) = board(&s.0);
        fake_mecha_with(&s.0, &s.0, 0, true, "subprocess.Popen(['sleep', '15'])");
        let path = s.0.join("bin");
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        let started = Instant::now();
        let line = set_status(&conn, r, &id, "done").unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(8),
            "waited {:?} on a lingering holder",
            started.elapsed()
        );
        assert!(line.contains("pride"), "{line}");
    }

    /// A child that never finishes is stopped at the deadline, and the board
    /// says what happened.
    #[test]
    fn a_child_that_never_finishes_is_stopped_at_the_deadline() {
        if !python() {
            eprintln!("skipping: no python3 for the stand-in mecha");
            return;
        }
        let s = scratch("hang");
        let (conn, db, id) = board(&s.0);
        fake_mecha_with(&s.0, &s.0, 0, false, "time.sleep(30)");
        let path = s.0.join("bin");
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        let started = Instant::now();
        let e = set_status_within(&conn, r, &id, "done", Duration::from_secs(1)).unwrap_err();
        assert!(started.elapsed() < Duration::from_secs(8));
        assert!(
            e.contains("stopped after") && e.contains("reads next"),
            "{e}"
        );
        assert_eq!(status(&conn, &id), "next");
    }

    /// With no `HOME`, the database mecha's server opens is a guess about
    /// the working directory — unknown, so never a match.
    #[test]
    fn with_no_home_the_served_database_is_unknown_and_a_close_refuses() {
        assert_eq!(served_db(None), None);
        let s = scratch("nohome");
        let (_, db, _) = board(&s.0);
        let r = route(Ok(Some("mecha")), &db, None, "next", "done", None);
        assert!(
            matches!(r, Route::Refuse(ref why) if why.contains("HOME is unset")),
            "{r:?}"
        );
    }

    /// Every refusal leads with its verdict: the status bar is one row,
    /// clipped at the terminal's width.
    #[test]
    fn a_refusal_leads_with_its_verdict() {
        let s = scratch("verdict");
        let (conn, db, id) = board(&s.0);
        let e = set_status(
            &conn,
            route(Ok(Some("mecha")), &db, None, "next", "done", None),
            &id,
            "done",
        )
        .unwrap_err();
        assert!(e.starts_with("nothing was changed: "), "{e}");
    }

    /// A config that cannot be read cannot say whether the owner opted in:
    /// a close refuses rather than guessing "not".
    #[test]
    fn an_unreadable_config_refuses_a_close() {
        let s = scratch("config");
        let (conn, db, id) = board(&s.0);
        let r = route(Err("bad toml"), &db, Some(&db), "next", "done", None);
        assert!(
            matches!(r, Route::Refuse(ref why) if why.contains("bad toml")),
            "{r:?}"
        );
        assert!(set_status(&conn, r, &id, "done").is_err());
        assert_eq!(status(&conn, &id), "next");
        // An open-to-open move needs no answer to that question.
        assert_eq!(
            route(Err("bad toml"), &db, Some(&db), "next", "waiting", None),
            Route::Direct
        );
    }

    /// mecha's refusal is shown and nothing is written here; mecha's success
    /// that did not reach this board is caught by the read-back.
    #[test]
    fn a_refusal_is_shown_and_a_success_elsewhere_is_caught() {
        if !python() {
            eprintln!("skipping: no python3 for the stand-in mecha");
            return;
        }
        let s = scratch("refused");
        let (conn, db, id) = board(&s.0);
        let path = s.0.join("bin");
        fake_mecha(&s.0, &s.0, 1, false);
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        let e = set_status(&conn, r, &id, "done").unwrap_err();
        assert!(e.contains("already done"), "{e}");
        assert_eq!(status(&conn, &id), "next");

        fake_mecha(&s.0, &s.0, 0, false);
        let r = route(
            Ok(Some("mecha")),
            &db,
            Some(&db),
            "next",
            "done",
            Some(path.as_os_str()),
        );
        let e = set_status(&conn, r, &id, "done").unwrap_err();
        assert!(e.contains("reads next"), "{e}");
    }
}
