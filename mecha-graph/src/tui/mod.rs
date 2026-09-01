//! `pkg tui` — keystroke-speed surfaces for the jobs a one-shot CLI is bad at
//!: review-queue triage, merge review, a search REPL with
//! provenance drill-down (+ episode tag/note annotation), an entity browser
//! with fact supersede, and a GTD task board.
//!
//! Keys: Tab/Shift-Tab cycle screens; 1-7 jump directly and q quits whenever
//! nothing is being typed (Esc empties the buffer, so Esc-then-digit and
//! Esc-then-q work from anywhere); Ctrl-Q quits even mid-typing.

use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{
    disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen,
};
use mecha_graph_core::rusqlite::Connection;
use mecha_graph_core::{entity_audit, episode, fact, graph, gtd, precheck, rollup, router, stats};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Tabs, Wrap};
use ratatui::{Frame, Terminal};

/// What a key did to a [`LineEdit`]. `Edited` means the text changed (search
/// uses this to trigger the debounced re-query); `Moved` means only the
/// cursor did; `Ignored` means the key wasn't an editing key — the caller's
/// own bindings should see it.
#[derive(Clone, Copy, PartialEq)]
enum EditOutcome {
    Ignored,
    Moved,
    Edited,
}

/// Single-line text input with a movable cursor and readline-style keys:
/// ←/→ · Ctrl-A/Home · Ctrl-E/End · Alt-b/f words · Backspace/Delete/Ctrl-D ·
/// Ctrl-U kill-to-start · Ctrl-K kill-to-end · Ctrl-W delete word.
/// Callers match their own keys first (Esc/Enter/↑/↓/screen bindings), then
/// hand the rest to [`LineEdit::handle`].
#[derive(Clone, Default)]
struct LineEdit {
    buf: String,
    /// Byte index into `buf`, always on a char boundary.
    cursor: usize,
}

impl LineEdit {
    fn new() -> Self {
        Self::default()
    }

    fn from(text: impl Into<String>) -> Self {
        let buf = text.into();
        let cursor = buf.len();
        LineEdit { buf, cursor }
    }

    fn text(&self) -> &str {
        &self.buf
    }

    fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    fn clear(&mut self) {
        self.buf.clear();
        self.cursor = 0;
    }

    fn at_end(&self) -> bool {
        self.cursor == self.buf.len()
    }

    fn set(&mut self, text: impl Into<String>) {
        self.buf = text.into();
        self.cursor = self.buf.len();
    }

    /// The buffer with a cursor mark at the insertion point, for rendering.
    fn display(&self) -> String {
        let mut s = String::with_capacity(self.buf.len() + 3);
        s.push_str(&self.buf[..self.cursor]);
        s.push('▌');
        s.push_str(&self.buf[self.cursor..]);
        s
    }

    fn prev_boundary(&self) -> usize {
        self.buf[..self.cursor]
            .char_indices()
            .next_back()
            .map(|(i, _)| i)
            .unwrap_or(0)
    }

    fn next_boundary(&self) -> usize {
        self.buf[self.cursor..]
            .chars()
            .next()
            .map(|c| self.cursor + c.len_utf8())
            .unwrap_or(self.cursor)
    }

    /// Start of the word before the cursor (skip spaces, then the word).
    fn prev_word(&self) -> usize {
        let before = &self.buf[..self.cursor];
        let trimmed = before.trim_end();
        let ws = trimmed
            .char_indices()
            .rev()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, c)| i + c.len_utf8())
            .unwrap_or(0);
        ws
    }

    /// End of the word after the cursor.
    fn next_word(&self) -> usize {
        let after = &self.buf[self.cursor..];
        let skipped = after.len() - after.trim_start().len();
        let rest = &after[skipped..];
        let end = rest
            .char_indices()
            .find(|(_, c)| c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(rest.len());
        self.cursor + skipped + end
    }

    fn handle(&mut self, key: KeyCode, mods: KeyModifiers) -> EditOutcome {
        let ctrl = mods.contains(KeyModifiers::CONTROL);
        let alt = mods.contains(KeyModifiers::ALT);
        match key {
            KeyCode::Left if alt => {
                self.cursor = self.prev_word();
                EditOutcome::Moved
            }
            KeyCode::Right if alt => {
                self.cursor = self.next_word();
                EditOutcome::Moved
            }
            KeyCode::Left => {
                self.cursor = self.prev_boundary();
                EditOutcome::Moved
            }
            KeyCode::Right => {
                self.cursor = self.next_boundary();
                EditOutcome::Moved
            }
            KeyCode::Home => {
                self.cursor = 0;
                EditOutcome::Moved
            }
            KeyCode::End => {
                self.cursor = self.buf.len();
                EditOutcome::Moved
            }
            KeyCode::Char('a') if ctrl => {
                self.cursor = 0;
                EditOutcome::Moved
            }
            KeyCode::Char('e') if ctrl => {
                self.cursor = self.buf.len();
                EditOutcome::Moved
            }
            KeyCode::Char('b') if alt => {
                self.cursor = self.prev_word();
                EditOutcome::Moved
            }
            KeyCode::Char('f') if alt => {
                self.cursor = self.next_word();
                EditOutcome::Moved
            }
            KeyCode::Backspace if alt || ctrl => {
                // Alt/Ctrl-Backspace: delete the word before the cursor.
                let start = self.prev_word();
                self.buf.replace_range(start..self.cursor, "");
                self.cursor = start;
                EditOutcome::Edited
            }
            KeyCode::Char('w') if ctrl => {
                let start = self.prev_word();
                self.buf.replace_range(start..self.cursor, "");
                self.cursor = start;
                EditOutcome::Edited
            }
            KeyCode::Char('u') if ctrl => {
                self.buf.replace_range(..self.cursor, "");
                self.cursor = 0;
                EditOutcome::Edited
            }
            KeyCode::Char('k') if ctrl => {
                self.buf.truncate(self.cursor);
                EditOutcome::Edited
            }
            KeyCode::Char('d') if ctrl => {
                let end = self.next_boundary();
                if end > self.cursor {
                    self.buf.replace_range(self.cursor..end, "");
                    EditOutcome::Edited
                } else {
                    EditOutcome::Moved
                }
            }
            KeyCode::Delete => {
                let end = self.next_boundary();
                if end > self.cursor {
                    self.buf.replace_range(self.cursor..end, "");
                }
                EditOutcome::Edited
            }
            KeyCode::Backspace => {
                let start = self.prev_boundary();
                if start < self.cursor {
                    self.buf.replace_range(start..self.cursor, "");
                    self.cursor = start;
                }
                EditOutcome::Edited
            }
            KeyCode::Char(c) if !ctrl && !alt => {
                self.buf.insert(self.cursor, c);
                self.cursor += c.len_utf8();
                EditOutcome::Edited
            }
            _ => EditOutcome::Ignored,
        }
    }
}

#[derive(Clone, Copy, PartialEq)]
enum Screen {
    Review,
    Merge,
    Search,
    Capture,
    Entity,
    Gtd,
    Stats,
}

#[derive(Clone, Copy, PartialEq)]
enum CaptureKind {
    Note,
    Fact,
}

struct CaptureState {
    kind: CaptureKind,
    /// Note text (Note mode).
    note: LineEdit,
    /// Fact fields (Fact mode): subject/predicate/object/statement/tags.
    fields: Vec<(&'static str, LineEdit)>,
    idx: usize,
}

struct EditState {
    /// (label, value) — subject, predicate, object, statement, tags.
    fields: Vec<(&'static str, LineEdit)>,
    idx: usize,
}

enum ReviewMode {
    Normal,
    /// Typing a rejection reason.
    Reason(LineEdit),
    /// Typing a refutation reason for the selected SHADOW fact.
    ShadowReason(LineEdit),
    /// Typing a rejection reason for the selected semantic GROUP.
    GroupReason(LineEdit),
    /// Editing the candidate's fields before acceptance.
    Edit(EditState),
}

struct ReviewState {
    items: Vec<fact::FactCandidate>,
    list: ListState,
    mode: ReviewMode,
    episode_preview: Option<(i64, String)>, // (episode_id, preview) cache
    /// Candidate ids marked for a bulk accept/reject (Space toggles).
    marked: std::collections::HashSet<i64>,
    /// Source-episode uid awaiting the confirming second `d`.
    pending_redact: Option<String>,
    /// Cluster view (`c` toggles): the queue grouped by (proposer,
    /// predicate) with verdict history — one decision per class.
    cluster_view: bool,
    clusters: Vec<precheck::ReviewCluster>,
    cluster_list: ListState,
    /// Item view drilled into one cluster (Enter from cluster view):
    /// (proposer, predicate-or-"(kind)"). Esc pops back out.
    cluster_filter: Option<(String, String)>,
    /// Proposer view (`p` toggles): the queue rolled up by proposing
    /// mechanism — gossip, the linkers, the LLM extractor, Bee, the rules.
    ///
    /// One level above clusters because that is the level the decisions are
    /// actually made at: you switch a *mechanism* on or off, and its record
    /// is invisible spread across 733 (proposer, predicate) rows.
    proposer_view: bool,
    proposers: Vec<precheck::ProposerStat>,
    proposer_list: ListState,
    /// Narrows both the cluster list and the item list to one proposer.
    /// Separate from `cluster_filter` rather than folded into it, because
    /// the two compose: pick a mechanism, then a class within it.
    proposer_filter: Option<String>,
    /// Surfaced-verdict view (`s` toggles): live shadow facts that are
    /// about to matter — review-on-use's primary human surface. The
    /// verdict verbs here act on FACTS (confirm/refute), not candidates.
    shadow_view: bool,
    shadow: Vec<mecha_graph_core::shadow::SurfacedFact>,
    shadow_list: ListState,
    /// Semantic groups within ONE class (`g` from a cluster) — the
    /// measured-sound cascade lane (same-class pairs carried the same
    /// human verdict ~89% of the time; cross-class only ~63%, which is
    /// why this view deliberately does not cross). One keystroke is one
    /// human verdict: the leader is the owner's, members cascade
    /// machine-labeled.
    group_view: bool,
    groups: Vec<mecha_graph_core::similar::SimilarGroup>,
    group_class: Option<(String, String)>,
    group_list: ListState,
}

struct MergeState {
    /// (a, b, label, proposal). `proposal` is `Some` when the row came from
    /// the audit queue, so acting on it here also decides it there — two
    /// surfaces over one decision, not two decisions.
    items: Vec<(String, String, String, Option<i64>)>,
    list: ListState,
    /// Merge direction: false = keep left(a), true = keep right(b).
    swap: bool,
}

/// An opened search result. Episodes carry their id so annotations (t/n)
/// can attach; other kinds are text-only.
struct DetailState {
    base: String,
    episode_id: Option<i64>,
    anns: Vec<episode::Annotation>,
    /// In-progress annotation: ("tag"|"note", buffer).
    annotate: Option<(&'static str, LineEdit)>,
    /// First `d` pressed; second confirms the redact.
    pending_redact: bool,
}

struct SearchState {
    input: LineEdit,
    /// Ctrl-P: include private/secret episodes in search + @browse.
    show_private: bool,
    /// Set when the input changed; a debounced live search fires ~150ms later.
    dirty_since: Option<std::time::Instant>,
    pack: Option<router::ContextPack>,
    list: ListState,
    detail: Option<DetailState>,
    /// "live" (BM25-only, instant) or "semantic" (with vectors, Ctrl-E).
    mode: &'static str,
}

#[derive(PartialEq)]
enum EntityMode {
    /// Typing a name to look up.
    Input,
    /// Choosing among multiple resolution matches.
    Pick,
    /// Browsing the entity page (facts selectable).
    View,
}

/// The three identity edits the entity page offers. Each is one line of
/// text and one call into `mecha_graph_core::graph`, which is why they share
/// a mode rather than getting a form apiece.
#[derive(Clone, Copy, PartialEq)]
enum EditKind {
    /// Change the node's name; the old one stays an alias.
    Rename,
    /// Add another way of saying this node's name.
    Alias,
    /// Mint a person who has facts and episodes but no node of their own.
    NewPerson,
}

impl EditKind {
    fn prompt(self) -> &'static str {
        match self {
            EditKind::Rename => " rename to — Enter confirms · Esc cancels ",
            EditKind::Alias => " add alias — Enter confirms · Esc cancels ",
            EditKind::NewPerson => " new person, their name — Enter confirms · Esc cancels ",
        }
    }
}

struct EntityEdit {
    kind: EditKind,
    input: LineEdit,
}

struct EntityState {
    mode: EntityMode,
    input: LineEdit,
    /// An identity edit in flight. Held beside `mode` rather than as a
    /// fourth variant of it so the lookup/pick/view logic is untouched —
    /// an edit is a thing layered *over* the page, not a fourth page.
    edit: Option<EntityEdit>,
    /// Live typeahead suggestions while typing in Input mode.
    suggestions: Vec<graph::Suggestion>,
    sug: ListState,
    candidates: Vec<graph::Node>,
    pick: ListState,
    node: Option<graph::Node>,
    interaction: Option<rollup::PersonInteraction>,
    /// Generated scope summary (node_context.summary), when one exists.
    summary: Option<String>,
    facts: Vec<fact::Fact>,
    episodes: Vec<episode::Episode>,
    /// Selection over `facts`.
    list: ListState,
    /// Which pane j/k drives: false = facts, true = timeline (h/l or ←/→).
    timeline_focus: bool,
    /// Selection over `episodes` when the timeline pane has focus.
    timeline: ListState,
}

enum GtdMode {
    List,
    /// Task form: create when `editing` is None, else schedule-edit of that
    /// task node.
    Form {
        fields: Vec<(&'static str, LineEdit)>,
        idx: usize,
        editing: Option<String>,
    },
}

struct GtdState {
    items: Vec<gtd::TaskItem>,
    list: ListState,
    show_closed: bool,
    mode: GtdMode,
}

struct App {
    conn: Connection,
    embedder: Option<mecha_graph_core::embed::Embedder>,
    screen: Screen,
    review: ReviewState,
    merge: MergeState,
    search: SearchState,
    capture: CaptureState,
    entity: EntityState,
    gtd: GtdState,
    stats_text: Option<String>,
    status: String,
    /// Set after suspending for $EDITOR — the next frame must clear.
    needs_clear: bool,
}

fn empty_fact_fields() -> Vec<(&'static str, LineEdit)> {
    vec![
        ("subject", LineEdit::new()),
        ("predicate", LineEdit::new()),
        ("object", LineEdit::new()),
        ("statement", LineEdit::new()),
        ("tags", LineEdit::new()),
    ]
}

pub fn run(conn: Connection) -> mecha_graph_core::Result<()> {
    let embedder = mecha_graph_core::embed::Embedder::default();
    let embedder = embedder.available().then_some(embedder);

    let mut app = App {
        conn,
        embedder,
        screen: Screen::Review,
        review: ReviewState {
            items: vec![],
            list: ListState::default(),
            mode: ReviewMode::Normal,
            episode_preview: None,
            marked: Default::default(),
            pending_redact: None,
            cluster_view: false,
            clusters: vec![],
            cluster_list: ListState::default(),
            cluster_filter: None,
            proposer_view: false,
            proposers: vec![],
            proposer_list: ListState::default(),
            proposer_filter: None,
            shadow_view: false,
            shadow: vec![],
            shadow_list: ListState::default(),
            group_view: false,
            groups: vec![],
            group_class: None,
            group_list: ListState::default(),
        },
        merge: MergeState {
            items: vec![],
            list: ListState::default(),
            swap: false,
        },
        search: SearchState {
            input: LineEdit::new(),
            show_private: false,
            dirty_since: None,
            pack: None,
            list: ListState::default(),
            detail: None,
            mode: "live",
        },
        capture: CaptureState {
            kind: CaptureKind::Note,
            note: LineEdit::new(),
            fields: empty_fact_fields(),
            idx: 0,
        },
        entity: EntityState {
            mode: EntityMode::Input,
            input: LineEdit::new(),
            edit: None,
            suggestions: vec![],
            sug: ListState::default(),
            candidates: vec![],
            pick: ListState::default(),
            node: None,
            interaction: None,
            summary: None,
            facts: vec![],
            episodes: vec![],
            list: ListState::default(),
            timeline_focus: false,
            timeline: ListState::default(),
        },
        gtd: GtdState {
            items: vec![],
            list: ListState::default(),
            show_closed: false,
            mode: GtdMode::List,
        },
        stats_text: None,
        needs_clear: false,
        status: "Tab/S-Tab cycle · 1-7 jump when not typing · Esc backs out · Ctrl-Q quit".into(),
    };
    app.reload_review()?;
    app.reload_merge()?;

    // Terminal setup with restore-on-panic.
    enable_raw_mode().map_err(io_err)?;
    let mut stdout = std::io::stdout();
    crossterm::execute!(stdout, EnterAlternateScreen).map_err(io_err)?;
    let default_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let _ = disable_raw_mode();
        let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
        default_hook(info);
    }));
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::new(backend).map_err(io_err)?;

    let result = event_loop(&mut terminal, &mut app);

    disable_raw_mode().map_err(io_err)?;
    crossterm::execute!(terminal.backend_mut(), LeaveAlternateScreen).map_err(io_err)?;
    result
}

fn io_err(e: std::io::Error) -> mecha_graph_core::Error {
    mecha_graph_core::Error::Io(e)
}

const SEARCH_DEBOUNCE: std::time::Duration = std::time::Duration::from_millis(150);

fn event_loop(
    terminal: &mut Terminal<CrosstermBackend<std::io::Stdout>>,
    app: &mut App,
) -> mecha_graph_core::Result<()> {
    loop {
        if app.needs_clear {
            terminal.clear().map_err(io_err)?;
            app.needs_clear = false;
        }
        terminal.draw(|f| draw(f, app)).map_err(io_err)?;

        // Debounced live search: fire once typing pauses.
        if let Some(t) = app.search.dirty_since {
            if t.elapsed() >= SEARCH_DEBOUNCE {
                run_search(app, false)?;
            }
        }

        if !event::poll(std::time::Duration::from_millis(60)).map_err(io_err)? {
            continue;
        }
        if let Event::Key(key) = event::read().map_err(io_err)? {
            if key.kind != KeyEventKind::Press {
                continue;
            }
            // Ctrl-Q / Ctrl-C quit from anywhere, including mid-typing.
            if key.modifiers.contains(KeyModifiers::CONTROL)
                && matches!(key.code, KeyCode::Char('q') | KeyCode::Char('c'))
            {
                return Ok(());
            }
            // Ctrl-Z: undo the last TUI episode delete/edit, from anywhere.
            if key.modifiers.contains(KeyModifiers::CONTROL) && key.code == KeyCode::Char('z') {
                match episode::undo_last(&app.conn)? {
                    Some(msg) => {
                        app.status = format!("undo: {msg}");
                        if app.screen == Screen::Review {
                            app.reload_review()?;
                        }
                        if !app.search.input.is_empty() {
                            app.search.dirty_since = Some(std::time::Instant::now());
                        }
                    }
                    None => app.status = "nothing to undo".into(),
                }
                continue;
            }
            // A screen is "typing" only while its buffer holds text (or a
            // local mode is active). Esc always empties the buffer, so
            // "Esc, then digit" jumps screens and "Esc, then q" exits from
            // any screen. Digit-leading queries: start with a space (inputs
            // are trimmed before use).
            let typing = match app.screen {
                Screen::Search => match &app.search.detail {
                    Some(d) => d.annotate.is_some(),
                    None => !app.search.input.is_empty(),
                },
                Screen::Capture => match app.capture.kind {
                    CaptureKind::Note => !app.capture.note.is_empty(),
                    CaptureKind::Fact => app.capture.fields.iter().any(|(_, v)| !v.is_empty()),
                },
                Screen::Entity => {
                    app.entity.edit.is_some()
                        || (app.entity.mode == EntityMode::Input && !app.entity.input.is_empty())
                }
                Screen::Gtd => match &app.gtd.mode {
                    GtdMode::Form { fields, .. } => fields.iter().any(|(_, v)| !v.is_empty()),
                    GtdMode::List => false,
                },
                Screen::Review => !matches!(app.review.mode, ReviewMode::Normal),
                _ => false,
            };
            // Tab/Shift-Tab ALWAYS cycle screens — no screen may claim Tab
            // locally (forms use ↑/↓, merge keep-side uses ←/→); anything
            // else strands the user on that screen.
            if matches!(key.code, KeyCode::Tab | KeyCode::BackTab) {
                let forward = [
                    Screen::Review,
                    Screen::Merge,
                    Screen::Search,
                    Screen::Capture,
                    Screen::Entity,
                    Screen::Gtd,
                    Screen::Stats,
                ];
                let cur = forward.iter().position(|s| *s == app.screen).unwrap_or(0);
                let n = forward.len();
                app.screen = if key.code == KeyCode::Tab {
                    forward[(cur + 1) % n]
                } else {
                    forward[(cur + n - 1) % n]
                };
                match app.screen {
                    Screen::Stats => app.stats_text = None,
                    Screen::Gtd => app.reload_gtd()?,
                    _ => {}
                }
                continue;
            }
            // Free-text surfaces keep every letter — "qwen" and "Quinn" must
            // be typeable — so q-quit only fires where text can't be entered.
            let accepts_text = matches!(app.screen, Screen::Capture)
                || (app.screen == Screen::Search && app.search.detail.is_none())
                || (app.screen == Screen::Entity
                    && (app.entity.mode == EntityMode::Input || app.entity.edit.is_some()))
                || (app.screen == Screen::Gtd && matches!(app.gtd.mode, GtdMode::Form { .. }));
            if !typing {
                // Screen switches consume the key: without the `continue`, the
                // digit would fall through into the new screen's handler (and
                // e.g. type "6" into the entity lookup box).
                match key.code {
                    KeyCode::Char('q') if !accepts_text => return Ok(()),
                    KeyCode::Char('1') => {
                        app.screen = Screen::Review;
                        continue;
                    }
                    KeyCode::Char('2') => {
                        app.screen = Screen::Merge;
                        continue;
                    }
                    KeyCode::Char('3') => {
                        app.screen = Screen::Search;
                        continue;
                    }
                    KeyCode::Char('4') => {
                        app.screen = Screen::Capture;
                        continue;
                    }
                    KeyCode::Char('5') => {
                        app.screen = Screen::Entity;
                        continue;
                    }
                    KeyCode::Char('6') => {
                        app.screen = Screen::Gtd;
                        app.reload_gtd()?; // fresh data on entry
                        continue;
                    }
                    KeyCode::Char('7') => {
                        app.screen = Screen::Stats;
                        app.stats_text = None; // refresh on entry
                        continue;
                    }
                    _ => {}
                }
            }
            match app.screen {
                Screen::Review => handle_review(app, key.code, key.modifiers)?,
                Screen::Merge => handle_merge(app, key.code)?,
                Screen::Search => handle_search(app, key.code, key.modifiers)?,
                Screen::Capture => handle_capture(app, key.code, key.modifiers)?,
                Screen::Entity => handle_entity(app, key.code, key.modifiers)?,
                Screen::Gtd => handle_gtd(app, key.code, key.modifiers)?,
                Screen::Stats => {}
            }
        }
    }
}

/// Run the router for the current input. `deep` adds the vector arm (ollama
/// round-trip); the live path is BM25 + lookup/aggregate routing — instant.
fn run_search(app: &mut App, deep: bool) -> mecha_graph_core::Result<()> {
    app.search.dirty_since = None;
    let q = app.search.input.text().trim().to_string();
    if q.is_empty() {
        app.search.pack = None;
        app.search.list.select(None);
        return Ok(());
    }
    // `@source` browse mode: no ranking, just the newest episodes from a
    // source prefix ("@reflect", "@bee", bare "@" = everything). The episode
    // browser the search box was missing.
    if let Some(prefix) = q.strip_prefix('@') {
        let like = format!("{}%", prefix.trim());
        let sql = if app.search.show_private {
            "SELECT uid, source, occurred_at, body FROM episode
             WHERE source LIKE ?1 ORDER BY occurred_at DESC LIMIT 100"
        } else {
            "SELECT uid, source, occurred_at, body FROM episode
             WHERE source LIKE ?1 AND sensitivity NOT IN ('private','secret')
             ORDER BY occurred_at DESC LIMIT 100"
        };
        let mut stmt = app.conn.prepare_cached(sql)?;
        let items: Vec<router::PackItem> = stmt
            .query_map([&like], |r| {
                let body: String = r.get(3)?;
                Ok(router::PackItem {
                    kind: "episode".into(),
                    id: r.get(0)?,
                    score: 0.0,
                    occurred_at: Some(r.get::<_, String>(2)?),
                    valid_from: None,
                    source: Some(r.get(1)?),
                    tags: vec![],
                    tier: None,
                    text: body.chars().take(200).collect(),
                })
            })?
            .collect::<std::result::Result<_, _>>()?;
        app.status = format!(
            "browse @{} · {} episodes · newest first · private tiers {} (Ctrl-P toggles)",
            prefix.trim(),
            items.len(),
            if app.search.show_private {
                "SHOWN"
            } else {
                "hidden"
            }
        );
        let n = items.len();
        app.search.mode = "browse";
        app.search.pack = Some(router::ContextPack {
            v: 1,
            query: q,
            intent: router::Intent::Recall,
            entities: vec![],
            tags: vec![],
            ambiguous: vec![],
            items,
            truncated: false,
            budget_tokens: 0,
            generated_at: mecha_graph_core::ids::now(),
            scope: router::Scope::Both,
            sources: vec![],
            window: None,
            flags: vec![],
        });
        app.search.list.select(if n == 0 { None } else { Some(0) });
        return Ok(());
    }
    let embedder = if deep { app.embedder.as_ref() } else { None };
    let started = std::time::Instant::now();
    let pack = router::query(
        &app.conn,
        embedder,
        &q,
        15,
        6000,
        app.search.show_private,
        Some("tui.search"),
    )?;
    app.search.mode = if deep { "semantic" } else { "live" };
    app.status = format!(
        "{} items · intent {:?} · {} · {:.0}ms — Ctrl-E semantic search",
        pack.items.len(),
        pack.intent,
        app.search.mode,
        started.elapsed().as_millis()
    );
    let n = pack.ambiguous.len() + pack.items.len();
    app.search.list.select(if n == 0 {
        None
    } else {
        Some(pack.ambiguous.len().min(n - 1))
    });
    app.search.pack = Some(pack);
    Ok(())
}

// ─── Data plumbing ───────────────────────────────────────────────────────────

impl App {
    fn reload_review(&mut self) -> mecha_graph_core::Result<()> {
        // Two filters that compose: a proposer alone, a proposer plus a
        // predicate, or neither. Selection is by exactly the key rule the
        // clusters were built with (`precheck::cluster_key`), so drilling in
        // can never show a different set than the row that was drilled.
        let proposer = self
            .review
            .cluster_filter
            .as_ref()
            .map(|(p, _)| p.clone())
            .or_else(|| self.review.proposer_filter.clone());
        let predicate = self.review.cluster_filter.as_ref().map(|(_, p)| p.clone());
        self.review.items = match &proposer {
            Some(proposer) => fact::pending_candidates(&self.conn, 100_000)?
                .into_iter()
                .filter(|c| {
                    c.proposed_by.as_deref().unwrap_or("?") == proposer
                        && predicate
                            .as_ref()
                            .is_none_or(|p| precheck::cluster_key(&c.payload).0 == *p)
                })
                .take(500)
                .collect(),
            None => fact::pending_candidates(&self.conn, 500)?,
        };
        let live: std::collections::HashSet<i64> = self.review.items.iter().map(|c| c.id).collect();
        self.review.marked.retain(|id| live.contains(id));
        let len = self.review.items.len();
        let sel = self.review.list.selected().unwrap_or(0);
        self.review.list.select(if len == 0 {
            None
        } else {
            Some(sel.min(len - 1))
        });
        self.review.episode_preview = None;
        if self.review.cluster_view {
            self.reload_clusters()?;
        }
        if self.review.proposer_view {
            self.reload_proposers()?;
        }
        if self.review.shadow_view {
            self.reload_shadow()?;
        }
        if self.review.group_view {
            self.reload_groups()?;
        }
        Ok(())
    }

    fn reload_shadow(&mut self) -> mecha_graph_core::Result<()> {
        self.review.shadow = mecha_graph_core::shadow::surfaced(
            &self.conn,
            mecha_graph_core::shadow::DEFAULT_SURFACE_LIMIT,
        )?;
        let len = self.review.shadow.len();
        let sel = self.review.shadow_list.selected().unwrap_or(0);
        self.review.shadow_list.select(if len == 0 {
            None
        } else {
            Some(sel.min(len - 1))
        });
        Ok(())
    }

    fn selected_shadow(&self) -> Option<&mecha_graph_core::shadow::SurfacedFact> {
        self.review.shadow.get(self.review.shadow_list.selected()?)
    }

    fn reload_groups(&mut self) -> mecha_graph_core::Result<()> {
        let Some((proposer, predicate)) = self.review.group_class.clone() else {
            return Ok(());
        };
        let e = mecha_graph_core::embed::Embedder::default();
        if !e.available() {
            self.review.group_view = false;
            self.review.cluster_view = true;
            self.status = "embedding server not answering — groups need vectors".into();
            return Ok(());
        }
        self.review.groups = mecha_graph_core::similar::groups_for_class(
            &self.conn,
            &e,
            &proposer,
            &predicate,
            mecha_graph_core::similar::GROUP_THRESHOLD,
        )?;
        let len = self.review.groups.len();
        let sel = self.review.group_list.selected().unwrap_or(0);
        self.review.group_list.select(if len == 0 {
            None
        } else {
            Some(sel.min(len - 1))
        });
        Ok(())
    }

    fn selected_group(&self) -> Option<&mecha_graph_core::similar::SimilarGroup> {
        self.review.groups.get(self.review.group_list.selected()?)
    }

    fn reload_clusters(&mut self) -> mecha_graph_core::Result<()> {
        let all = precheck::review_clusters(&self.conn, 3)?;
        self.review.clusters = match &self.review.proposer_filter {
            Some(p) => all.into_iter().filter(|c| c.proposed_by == *p).collect(),
            None => all,
        };
        let len = self.review.clusters.len();
        let sel = self.review.cluster_list.selected().unwrap_or(0);
        self.review.cluster_list.select(if len == 0 {
            None
        } else {
            Some(sel.min(len - 1))
        });
        Ok(())
    }

    fn reload_proposers(&mut self) -> mecha_graph_core::Result<()> {
        self.review.proposers = precheck::proposer_stats(&self.conn)?;
        let len = self.review.proposers.len();
        let sel = self.review.proposer_list.selected().unwrap_or(0);
        self.review.proposer_list.select(if len == 0 {
            None
        } else {
            Some(sel.min(len - 1))
        });
        Ok(())
    }

    fn selected_proposer(&self) -> Option<&precheck::ProposerStat> {
        self.review
            .proposer_list
            .selected()
            .and_then(|i| self.review.proposers.get(i))
    }

    fn selected_cluster(&self) -> Option<&precheck::ReviewCluster> {
        self.review
            .clusters
            .get(self.review.cluster_list.selected()?)
    }

    /// Pending candidate ids belonging to one cluster.
    fn cluster_member_ids(
        &self,
        proposer: &str,
        predicate: &str,
    ) -> mecha_graph_core::Result<Vec<i64>> {
        Ok(fact::pending_candidates(&self.conn, 100_000)?
            .into_iter()
            .filter(|c| {
                c.proposed_by.as_deref().unwrap_or("?") == proposer
                    && precheck::cluster_key(&c.payload).0 == predicate
            })
            .map(|c| c.id)
            .collect())
    }

    fn reload_merge(&mut self) -> mecha_graph_core::Result<()> {
        // Three sources, one screen. The first two match identical names
        // and identical addresses; the third is the audit's near-duplicate
        // detector, which is the only one that sees "Conan Moore" beside
        // "Conan F Moore". Before it was wired in, this screen said "no
        // candidates 🎉" while two merges waited in the queue — a surface
        // announcing there is nothing to do is worse than one that says
        // nothing, because it is believed.
        self.merge.items = graph::duplicate_person_candidates(&self.conn)?
            .into_iter()
            .chain(graph::email_duplicate_candidates(&self.conn)?)
            .map(|(a, b, name)| (a, b, name, None))
            .collect();
        for p in entity_audit::pending(&self.conn, Some("near_duplicate_person"), 200)? {
            if p.other_id.is_empty() {
                continue;
            }
            self.merge.items.push((
                p.subject_id.clone(),
                p.other_id.clone(),
                format!("{} / {}", p.subject_name, p.other_name),
                Some(p.id),
            ));
        }
        let len = self.merge.items.len();
        let sel = self.merge.list.selected().unwrap_or(0);
        self.merge.list.select(if len == 0 {
            None
        } else {
            Some(sel.min(len - 1))
        });
        Ok(())
    }

    fn selected_candidate(&self) -> Option<&fact::FactCandidate> {
        self.review.items.get(self.review.list.selected()?)
    }

    fn reload_gtd(&mut self) -> mecha_graph_core::Result<()> {
        self.gtd.items = gtd::list_tasks(&self.conn, self.gtd.show_closed)?;
        let len = self.gtd.items.len();
        let sel = self.gtd.list.selected().unwrap_or(0);
        self.gtd.list.select(if len == 0 {
            None
        } else {
            Some(sel.min(len - 1))
        });
        Ok(())
    }

    /// Load a node's page and switch to the entity screen. Reachable from the
    /// lookup box, search results (Enter on a person/fact hit), and GTD tasks.
    fn open_entity(&mut self, node_id: &str) -> mecha_graph_core::Result<()> {
        let Some(node) = graph::get_node(&self.conn, node_id)? else {
            self.status = format!("node {node_id} not found");
            return Ok(());
        };
        graph::increment_node_access(&self.conn, node_id)?;
        self.entity.interaction = rollup::get_person_interaction(&self.conn, node_id)?;
        self.entity.summary = mecha_graph_core::context::get_node_context(&self.conn, node_id)?
            .map(|c| c.summary)
            .filter(|s| !s.is_empty());
        self.entity.facts = fact::facts_for_node(&self.conn, node_id, 200)?;
        self.entity.episodes = episode::episodes_for_node(&self.conn, node_id, 30)?;
        self.entity.list.select(if self.entity.facts.is_empty() {
            None
        } else {
            Some(0)
        });
        self.entity.timeline_focus = false;
        self.entity
            .timeline
            .select(if self.entity.episodes.is_empty() {
                None
            } else {
                Some(0)
            });
        self.entity.node = Some(node);
        self.entity.mode = EntityMode::View;
        self.screen = Screen::Entity;
        Ok(())
    }

    /// Reload the open entity page in place (after a supersede).
    fn refresh_entity(&mut self) -> mecha_graph_core::Result<()> {
        if let Some(node) = &self.entity.node {
            let id = node.id.clone();
            self.entity.facts = fact::facts_for_node(&self.conn, &id, 200)?;
            let len = self.entity.facts.len();
            let sel = self.entity.list.selected().unwrap_or(0);
            self.entity.list.select(if len == 0 {
                None
            } else {
                Some(sel.min(len - 1))
            });
        }
        Ok(())
    }

    fn episode_preview(&mut self, episode_id: i64) -> String {
        if let Some((id, text)) = &self.review.episode_preview {
            if *id == episode_id {
                return text.clone();
            }
        }
        let text = episode::get_episode(&self.conn, episode_id)
            .ok()
            .flatten()
            .map(|e| {
                format!(
                    "[{} · {}]\n{}",
                    e.occurred_at,
                    e.source,
                    e.body.chars().take(1200).collect::<String>()
                )
            })
            .unwrap_or_else(|| "(no source episode)".into());
        self.review.episode_preview = Some((episode_id, text.clone()));
        text
    }

    fn node_summary(&self, id: &str) -> String {
        let Ok(Some(node)) = graph::get_node(&self.conn, id) else {
            return format!("{id}: (missing)");
        };
        let idents: Vec<String> = self
            .conn
            .prepare("SELECT kind || ':' || value FROM node_identifier WHERE node_id = ?1")
            .and_then(|mut s| {
                s.query_map([id], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
            })
            .unwrap_or_default();
        let pi = rollup::get_person_interaction(&self.conn, id)
            .ok()
            .flatten();
        let eps = episode::episodes_for_node(&self.conn, id, 4).unwrap_or_default();
        let mut out = format!("{}\n{}\n", node.name, id);
        out.push_str(&format!("identifiers:\n"));
        for i in &idents {
            out.push_str(&format!("  {i}\n"));
        }
        if let Some(pi) = pi {
            out.push_str(&format!(
                "interactions: {} · last seen {}\n",
                pi.interaction_count,
                pi.last_seen_at.as_deref().unwrap_or("-")
            ));
        }
        out.push_str("recent:\n");
        for e in eps {
            out.push_str(&format!(
                "  [{}] {}\n",
                &e.occurred_at[..10.min(e.occurred_at.len())],
                e.body
                    .lines()
                    .next()
                    .unwrap_or("")
                    .chars()
                    .take(48)
                    .collect::<String>()
            ));
        }
        out
    }
}

// ─── Key handling ────────────────────────────────────────────────────────────

fn move_sel(list: &mut ListState, len: usize, delta: i64) {
    if len == 0 {
        return;
    }
    let cur = list.selected().unwrap_or(0) as i64;
    list.select(Some((cur + delta).clamp(0, len as i64 - 1) as usize));
}

/// Entity-shaped form fields get ghost-text completion.
fn is_entity_field(label: &str) -> bool {
    matches!(label, "subject" | "object" | "project")
}

/// Top typeahead completion for a partial entity name. Returns the full
/// node name plus the display string rendered after the cursor: the dim
/// remainder when the name extends what was typed, else " → Name".
fn ghost_for(conn: &Connection, partial: &str) -> Option<(String, String)> {
    let typed = partial.trim();
    if typed.len() < 2 {
        return None;
    }
    let sug = graph::suggest_entities(conn, typed, 1)
        .ok()?
        .into_iter()
        .next()?;
    let name = sug.node.name;
    if name.eq_ignore_ascii_case(typed) {
        return None; // already complete
    }
    let display = if name.to_lowercase().starts_with(&typed.to_lowercase()) {
        name[typed.len()..].to_string()
    } else {
        format!(" → {name}")
    };
    Some((name, display))
}

/// Top tag-vocabulary completion for a partial tag (same contract as
/// [`ghost_for`]).
fn ghost_tag(conn: &Connection, partial: &str) -> Option<(String, String)> {
    let t = partial.trim().to_lowercase();
    if t.is_empty() {
        return None;
    }
    let tags = episode::list_tags(conn).ok()?;
    let hit = tags
        .iter()
        .map(|(n, _)| n)
        .find(|n| n.starts_with(&t) && **n != t)
        .or_else(|| {
            tags.iter()
                .map(|(n, _)| n)
                .find(|n| n.contains(&t) && **n != t)
        })?
        .clone();
    let display = if hit.starts_with(&t) {
        hit[t.len()..].to_string()
    } else {
        format!(" → {hit}")
    };
    Some((hit, display))
}

/// Suspend the TUI, open `$EDITOR` on the text, return the edited content
/// (None on abort or no change). The temp file lives under ~/.mecha-graph (0600) and
/// is removed before returning.
fn spawn_editor(initial: &str) -> mecha_graph_core::Result<Option<String>> {
    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".into());
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    let path = std::path::PathBuf::from(home).join("pkg").join(".edit.md");
    std::fs::write(&path, initial).map_err(io_err)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    disable_raw_mode().map_err(io_err)?;
    let _ = crossterm::execute!(std::io::stdout(), LeaveAlternateScreen);
    let status = std::process::Command::new(&editor).arg(&path).status();
    let _ = crossterm::execute!(std::io::stdout(), EnterAlternateScreen);
    enable_raw_mode().map_err(io_err)?;
    let edited = match status {
        Ok(st) if st.success() => std::fs::read_to_string(&path).ok(),
        _ => None,
    };
    let _ = std::fs::remove_file(&path);
    Ok(edited.filter(|t| t.trim() != initial.trim() && !t.trim().is_empty()))
}

/// Strings too generic to become aliases — learning "they" → a person would
/// poison resolution for every future "They …" candidate.
// alias_worthy moved to core (fact::alias_worthy) when the CLI `bind` verb
// arrived — two copies of that list is two lists that drift.
use mecha_graph_core::fact::alias_worthy;

/// Ids a review action applies to: in cluster view, every member of the
/// selected cluster; otherwise the marked set (in list order) when
/// non-empty, else just the selection.
fn review_targets(app: &App) -> mecha_graph_core::Result<Vec<i64>> {
    if app.review.cluster_view {
        return match app.selected_cluster() {
            Some(cl) => app.cluster_member_ids(&cl.proposed_by, &cl.predicate),
            None => Ok(vec![]),
        };
    }
    Ok(if app.review.marked.is_empty() {
        app.selected_candidate().map(|c| c.id).into_iter().collect()
    } else {
        app.review
            .items
            .iter()
            .map(|c| c.id)
            .filter(|id| app.review.marked.contains(id))
            .collect()
    })
}

/// Reject every target with one shared reason. `r` calls this directly
/// (no prompt — the owner almost never has prose to add, and an empty
/// prompt was pure decision tax); `R` routes through the reason prompt
/// first for the rare case where the why matters.
fn reject_targets(app: &mut App, reason: &str) -> mecha_graph_core::Result<()> {
    let ids = review_targets(app)?;
    for id in &ids {
        fact::reject_candidate(&app.conn, *id, reason)?;
    }
    app.status = match ids.len() {
        0 => String::new(),
        1 => format!("#{} rejected", ids[0]),
        n => format!("{n} rejected"),
    };
    app.reload_review()?;
    Ok(())
}

fn accept_selected(app: &mut App, create_missing: bool) -> mecha_graph_core::Result<()> {
    let ids = review_targets(app)?;
    let (mut ok, mut failed) = (0usize, 0usize);
    let mut last_err = String::new();
    for id in &ids {
        match mecha_graph_core::extract::accept_commitment(&app.conn, *id) {
            Ok(_) => ok += 1,
            Err(_) => match fact::accept_candidate_opts(&app.conn, *id, create_missing, true) {
                Ok(_) => ok += 1,
                Err(e) => {
                    failed += 1;
                    last_err = format!("#{id}: {e}");
                }
            },
        }
    }
    app.status = match (ids.len(), failed) {
        (0, _) => return Ok(()),
        (1, 0) => format!("#{} accepted", ids[0]),
        (_, 0) => format!("{ok} accepted"),
        _ => format!("{ok} accepted, {failed} failed — last: {last_err} (e edit · A new topic)"),
    };
    app.reload_review()?;
    Ok(())
}

/// Refute the selected shadow fact: never true, retracted, candidate
/// rejected under the human label. One selection, one verdict.
fn refute_selected_shadow(app: &mut App, reason: &str) -> mecha_graph_core::Result<()> {
    if let Some(sf) = app.selected_shadow() {
        let uid = sf.fact.uid.clone();
        fact::refute_shadow_fact(&app.conn, &uid, reason)?;
        app.status = format!(
            "refuted {} — retracted as never true",
            &uid[..8.min(uid.len())]
        );
        app.reload_shadow()?;
    }
    Ok(())
}

/// One human verdict on a semantic group: the leader is the owner's,
/// every member goes through the cascade paths (machine-labeled,
/// invisible to the ladder). If the leader's verdict cannot land, nothing
/// cascades — a fan-out from a failed verdict is a fan-out from nothing.
fn accept_selected_group(app: &mut App) -> mecha_graph_core::Result<()> {
    let Some(g) = app.selected_group() else {
        return Ok(());
    };
    let (leader, members) = (g.leader_id, g.members.clone());
    match fact::accept_candidate_opts(&app.conn, leader, false, true) {
        Ok(_) => {}
        Err(e) => {
            app.status = format!("group leader #{leader} failed: {e} — nothing cascaded");
            return Ok(());
        }
    }
    let (mut done, mut failed) = (0usize, 0usize);
    for (id, _) in &members {
        match fact::accept_candidate_cascade(&app.conn, *id, leader) {
            Ok(_) => done += 1,
            Err(_) => failed += 1,
        }
    }
    app.status = match failed {
        0 => format!("group accepted: you + {done} cascaded (one human verdict)"),
        f => format!("group accepted: you + {done} cascaded, {f} left pending"),
    };
    // `reload_review` re-groups on its own when `group_view` is set, and this
    // key is only reachable while it is — so the explicit call that used to
    // sit here ran the class's grouping a second time, on the UI thread,
    // after every single group verdict. Both passes embedded the class before
    // V023 and both read the cache after it; either way the terminal was
    // frozen for two of them where one was needed.
    app.reload_review()
}

fn reject_selected_group(app: &mut App, reason: &str) -> mecha_graph_core::Result<()> {
    let Some(g) = app.selected_group() else {
        return Ok(());
    };
    let (leader, members) = (g.leader_id, g.members.clone());
    match fact::reject_candidate(&app.conn, leader, reason) {
        Ok(()) => {}
        Err(e) => {
            app.status = format!("group leader #{leader} failed: {e} — nothing cascaded");
            return Ok(());
        }
    }
    let mut done = 0usize;
    for (id, sim) in &members {
        if fact::reject_candidate_cascade(&app.conn, *id, leader, Some(*sim)).is_ok() {
            done += 1;
        }
    }
    app.status = format!("group rejected: you + {done} cascaded (one human verdict)");
    // Same double pass as the accept path above, and removed for the same
    // reason: `reload_review` owns the re-group whenever the group view is up.
    app.reload_review()
}

fn handle_review(app: &mut App, key: KeyCode, mods: KeyModifiers) -> mecha_graph_core::Result<()> {
    match &mut app.review.mode {
        ReviewMode::Reason(buf) => {
            match key {
                KeyCode::Esc => app.review.mode = ReviewMode::Normal,
                KeyCode::Enter => {
                    let reason = if buf.is_empty() {
                        "rejected in review".to_string()
                    } else {
                        buf.text().to_string()
                    };
                    app.review.mode = ReviewMode::Normal;
                    reject_targets(app, &reason)?;
                }
                k => {
                    buf.handle(k, mods);
                }
            }
            return Ok(());
        }
        ReviewMode::ShadowReason(buf) => {
            match key {
                KeyCode::Esc => app.review.mode = ReviewMode::Normal,
                KeyCode::Enter => {
                    let reason = if buf.is_empty() {
                        "refuted at review".to_string()
                    } else {
                        buf.text().to_string()
                    };
                    app.review.mode = ReviewMode::Normal;
                    refute_selected_shadow(app, &reason)?;
                }
                k => {
                    buf.handle(k, mods);
                }
            }
            return Ok(());
        }
        ReviewMode::GroupReason(buf) => {
            match key {
                KeyCode::Esc => app.review.mode = ReviewMode::Normal,
                KeyCode::Enter => {
                    let reason = if buf.is_empty() {
                        "rejected in review".to_string()
                    } else {
                        buf.text().to_string()
                    };
                    app.review.mode = ReviewMode::Normal;
                    reject_selected_group(app, &reason)?;
                }
                k => {
                    buf.handle(k, mods);
                }
            }
            return Ok(());
        }
        ReviewMode::Edit(edit) => {
            match key {
                KeyCode::Esc => app.review.mode = ReviewMode::Normal,
                KeyCode::Down => edit.idx = (edit.idx + 1) % edit.fields.len(),
                KeyCode::Up => edit.idx = (edit.idx + edit.fields.len() - 1) % edit.fields.len(),
                KeyCode::Enter => {
                    // Save edited fields back into the candidate payload.
                    let fields = edit.fields.clone();
                    if let Some(c) = app.selected_candidate() {
                        let id = c.id;
                        let mut payload = c.payload.clone();
                        if let serde_json::Value::Object(map) = &mut payload {
                            for (label, value) in &fields {
                                let v = value.text().trim();
                                map.insert(
                                    label.to_string(),
                                    if v.is_empty() {
                                        serde_json::Value::Null
                                    } else {
                                        serde_json::Value::String(v.to_string())
                                    },
                                );
                            }
                        }
                        fact::update_candidate_payload(&app.conn, id, &payload)?;
                        app.status = format!("#{id} edited — a to accept");
                    }
                    app.review.mode = ReviewMode::Normal;
                    app.reload_review()?;
                }
                k => {
                    let field = &mut edit.fields[edit.idx];
                    let accept_ghost = matches!(k, KeyCode::Right if field.1.at_end())
                        || (k == KeyCode::Char('f') && mods.contains(KeyModifiers::CONTROL));
                    if accept_ghost && is_entity_field(field.0) {
                        if let Some((name, _)) = ghost_for(&app.conn, field.1.text()) {
                            field.1.set(name);
                            return Ok(());
                        }
                    }
                    field.1.handle(k, mods);
                }
            }
            return Ok(());
        }
        ReviewMode::Normal => {}
    }

    // Semantic-group view: one class's near-repeats, one verdict per
    // group. Within-class only — the measured lane (~89% agreement);
    // crossing classes measured ~63% and stays off this surface.
    if app.review.group_view {
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                move_sel(&mut app.review.group_list, app.review.groups.len(), 1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_sel(&mut app.review.group_list, app.review.groups.len(), -1)
            }
            KeyCode::Char('G') => {
                let len = app.review.groups.len();
                if len > 0 {
                    app.review.group_list.select(Some(len - 1));
                }
            }
            KeyCode::Char('g') | KeyCode::Esc => {
                app.review.group_view = false;
                app.review.cluster_view = true;
                app.reload_review()?;
            }
            KeyCode::Char('a') => accept_selected_group(app)?,
            KeyCode::Char('r') => {
                if let Some((p, k)) = app.review.group_class.clone() {
                    reject_selected_group(app, &format!("cluster verdict: {p} · {k}"))?;
                }
            }
            KeyCode::Char('R') if app.selected_group().is_some() => {
                app.review.mode = ReviewMode::GroupReason(LineEdit::new());
            }
            _ => {}
        }
        return Ok(());
    }

    // Surfaced-verdict view: shadow facts that are about to matter.
    // The verbs act on FACTS — y confirms (tier → reviewed), r/R refutes
    // (never true) — and each keystroke settles the staging candidate and
    // moves the ladder exactly once, in core.
    if app.review.shadow_view {
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                move_sel(&mut app.review.shadow_list, app.review.shadow.len(), 1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_sel(&mut app.review.shadow_list, app.review.shadow.len(), -1)
            }
            KeyCode::Char('G') => {
                let len = app.review.shadow.len();
                if len > 0 {
                    app.review.shadow_list.select(Some(len - 1));
                }
            }
            KeyCode::Char('s') | KeyCode::Esc => {
                app.review.shadow_view = false;
                app.reload_review()?;
            }
            KeyCode::Char('y') | KeyCode::Char('a') => {
                if let Some(sf) = app.selected_shadow() {
                    let uid = sf.fact.uid.clone();
                    fact::confirm_shadow_fact(&app.conn, &uid)?;
                    app.status = format!("confirmed {} — now reviewed", &uid[..8.min(uid.len())]);
                    app.reload_shadow()?;
                }
            }
            // r: instant refute — the surfacing reason IS the context; R
            // prompts for prose when the why matters (it feeds rejection
            // memory).
            KeyCode::Char('r') => refute_selected_shadow(app, "refuted at review")?,
            KeyCode::Char('R') if app.selected_shadow().is_some() => {
                app.review.mode = ReviewMode::ShadowReason(LineEdit::new());
            }
            _ => {}
        }
        return Ok(());
    }

    // Proposer view: which mechanism is proposing well. Read-only on
    // purpose — no bulk verdict spans a whole mechanism, because a proposer
    // is not a claim about anything and "reject everything the extractor
    // ever said" is a decision no single keystroke should carry.
    if app.review.proposer_view {
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                move_sel(&mut app.review.proposer_list, app.review.proposers.len(), 1)
            }
            KeyCode::Char('k') | KeyCode::Up => move_sel(
                &mut app.review.proposer_list,
                app.review.proposers.len(),
                -1,
            ),
            KeyCode::Char('G') => {
                let len = app.review.proposers.len();
                if len > 0 {
                    app.review.proposer_list.select(Some(len - 1));
                }
            }
            KeyCode::Char('p') | KeyCode::Esc => {
                app.review.proposer_view = false;
                app.review.proposer_filter = None;
                app.reload_review()?;
            }
            // Enter narrows to this mechanism and drops into its clusters.
            KeyCode::Enter => {
                if let Some(p) = app.selected_proposer() {
                    let name = p.proposer.clone();
                    let pend = p.pending;
                    app.review.proposer_view = false;
                    app.review.cluster_view = true;
                    app.review.cluster_filter = None;
                    app.review.proposer_filter = Some(name.clone());
                    app.review.cluster_list.select(Some(0));
                    app.reload_review()?;
                    app.status = format!("{name} — {pend} pending · Esc back to proposers");
                }
            }
            // c: skip the cluster level and see this mechanism's items.
            KeyCode::Char('c') => {
                if let Some(p) = app.selected_proposer() {
                    let name = p.proposer.clone();
                    app.review.proposer_view = false;
                    app.review.cluster_view = false;
                    app.review.cluster_filter = None;
                    app.review.proposer_filter = Some(name.clone());
                    app.review.list.select(Some(0));
                    app.reload_review()?;
                    app.status = format!("{name} — every pending item · Esc clears the filter");
                }
            }
            _ => {}
        }
        return Ok(());
    }

    // Cluster view: one decision per (proposer, predicate) class.
    if app.review.cluster_view {
        match key {
            KeyCode::Char('j') | KeyCode::Down => {
                move_sel(&mut app.review.cluster_list, app.review.clusters.len(), 1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_sel(&mut app.review.cluster_list, app.review.clusters.len(), -1)
            }
            KeyCode::Char('G') => {
                let len = app.review.clusters.len();
                if len > 0 {
                    app.review.cluster_list.select(Some(len - 1));
                }
            }
            // c drops to items keeping whatever narrowing is in force; Esc
            // peels one layer, so it climbs to the proposers when we got
            // here from one and only leaves the level otherwise.
            KeyCode::Char('c') => {
                app.review.cluster_view = false;
                app.reload_review()?;
            }
            KeyCode::Esc => {
                app.review.cluster_view = false;
                if app.review.proposer_filter.is_some() {
                    app.review.proposer_filter = None;
                    app.review.proposer_view = true;
                }
                app.reload_review()?;
            }
            KeyCode::Char('p') => {
                app.review.cluster_view = false;
                app.review.proposer_filter = None;
                app.review.proposer_view = true;
                app.reload_review()?;
            }
            // Drill in: item view filtered to this cluster (Esc pops back).
            KeyCode::Enter => {
                if let Some(cl) = app.selected_cluster() {
                    let (pb, pred) = (cl.proposed_by.clone(), cl.predicate.clone());
                    app.review.cluster_view = false;
                    app.review.cluster_filter = Some((pb.clone(), pred.clone()));
                    app.review.list.select(Some(0));
                    app.reload_review()?;
                    app.status = format!(
                        "cluster {pb} · {pred} — a/r/Space per item · Esc back to clusters"
                    );
                }
            }
            // g: semantic groups within this class — one verdict per group.
            KeyCode::Char('g') => match app.selected_cluster() {
                Some(cl) if cl.commitment => {
                    app.status =
                        "commitments do not cascade — Enter to review them individually".into();
                }
                Some(cl) => {
                    app.review.group_class = Some((cl.proposed_by.clone(), cl.predicate.clone()));
                    app.review.group_list.select(Some(0));
                    app.review.cluster_view = false;
                    app.review.group_view = true;
                    app.status = "grouping (embedding the class)…".into();
                    app.reload_groups()?;
                }
                None => {}
            },
            KeyCode::Char('a') | KeyCode::Char('A') => match app.selected_cluster() {
                Some(cl) if cl.commitment => {
                    app.status =
                        "commitments materialize tasks — Enter to review them individually".into();
                }
                Some(_) => accept_selected(app, key == KeyCode::Char('A'))?,
                None => {}
            },
            // r: instant cluster reject — the cluster key IS the reason
            // (better mining signal than prose nobody types). R prompts.
            KeyCode::Char('r') => match app.selected_cluster() {
                Some(cl) if cl.commitment => {
                    app.status =
                        "commitments materialize tasks — Enter to review them individually".into();
                }
                Some(cl) => {
                    let reason = format!("cluster verdict: {} · {}", cl.proposed_by, cl.predicate);
                    reject_targets(app, &reason)?;
                }
                None => {}
            },
            KeyCode::Char('R') => match app.selected_cluster() {
                Some(cl) if cl.commitment => {
                    app.status =
                        "commitments materialize tasks — Enter to review them individually".into();
                }
                Some(_) => app.review.mode = ReviewMode::Reason(LineEdit::new()),
                None => {}
            },
            _ => {}
        }
        return Ok(());
    }

    if !matches!(key, KeyCode::Char('d')) {
        app.review.pending_redact = None;
    }
    match key {
        KeyCode::Char('j') | KeyCode::Down => {
            move_sel(&mut app.review.list, app.review.items.len(), 1)
        }
        KeyCode::Char('k') | KeyCode::Up => {
            move_sel(&mut app.review.list, app.review.items.len(), -1)
        }
        KeyCode::Char('G') => {
            let len = app.review.items.len();
            if len > 0 {
                app.review.list.select(Some(len - 1));
            }
        }
        // Space marks for bulk ops and advances; * toggles all; Esc clears.
        KeyCode::Char(' ') => {
            if let Some(id) = app.selected_candidate().map(|c| c.id) {
                if !app.review.marked.remove(&id) {
                    app.review.marked.insert(id);
                }
                move_sel(&mut app.review.list, app.review.items.len(), 1);
            }
        }
        KeyCode::Char('*') => {
            if app.review.marked.len() == app.review.items.len() {
                app.review.marked.clear();
            } else {
                app.review.marked = app.review.items.iter().map(|c| c.id).collect();
            }
        }
        // Esc peels one layer at a time: marks, then the cluster
        // drill-down, then the proposer filter. Never more than one per
        // press — a key that could drop you two levels is a key you stop
        // trusting to be reversible.
        KeyCode::Esc => {
            if !app.review.marked.is_empty() {
                app.review.marked.clear();
            } else if app.review.cluster_filter.is_some() {
                app.review.cluster_filter = None;
                app.review.cluster_view = true;
                app.reload_review()?;
            } else if app.review.proposer_filter.is_some() {
                app.review.proposer_filter = None;
                app.review.proposer_view = true;
                app.reload_review()?;
            }
        }
        // c: cluster view — the queue grouped by (proposer, predicate),
        // narrowed to the current proposer if one is selected.
        KeyCode::Char('c') => {
            app.review.cluster_filter = None;
            app.review.cluster_view = true;
            app.reload_review()?;
        }
        // p: proposer view — the queue rolled up by proposing mechanism.
        KeyCode::Char('p') => {
            app.review.cluster_filter = None;
            app.review.proposer_filter = None;
            app.review.proposer_view = true;
            app.reload_review()?;
        }
        // s: the surfaced-verdict queue — shadow facts about to matter.
        KeyCode::Char('s') => {
            app.review.shadow_view = true;
            app.reload_review()?;
        }
        // d twice: redact the selected candidate's SOURCE episode — kills the
        // note and every candidate extracted from it (junk reference notes).
        KeyCode::Char('d') => {
            let ep_id = app.selected_candidate().and_then(|c| c.episode_id);
            if let Some(ep_id) = ep_id {
                if let Some(ep) = episode::get_episode(&app.conn, ep_id)? {
                    if app.review.pending_redact.as_deref() == Some(ep.uid.as_str()) {
                        let n_before = app.review.items.len();
                        episode::redact_episode_undoable(&app.conn, &ep.uid)?;
                        app.review.pending_redact = None;
                        app.reload_review()?;
                        app.status = format!(
                            "source episode deleted — {} candidates went with it (Ctrl-Z restores)",
                            n_before - app.review.items.len()
                        );
                    } else {
                        app.review.pending_redact = Some(ep.uid.clone());
                        app.status = format!(
                            "d again to PERMANENTLY delete source episode [{} {}] + ALL its candidates",
                            &ep.occurred_at[..10.min(ep.occurred_at.len())],
                            ep.source
                        );
                    }
                }
            } else {
                app.status = "candidate has no source episode".into();
            }
        }
        // b: rebind an unresolvable subject to the top did-you-mean match
        // (shown in the detail pane) and learn the original as an alias.
        KeyCode::Char('b') => {
            if let Some(c) = app.selected_candidate().cloned() {
                let subject = c
                    .payload
                    .get("subject")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if !subject.is_empty() {
                    let already = graph::resolve_entity_all(&app.conn, &subject)?.len() == 1;
                    if already {
                        app.status = format!("'{subject}' already resolves");
                    } else if let Some(sug) = graph::suggest_entities(&app.conn, &subject, 1)?
                        .into_iter()
                        .next()
                    {
                        let mut payload = c.payload.clone();
                        if let serde_json::Value::Object(map) = &mut payload {
                            map.insert(
                                "subject".into(),
                                serde_json::Value::String(sug.node.name.clone()),
                            );
                        }
                        fact::update_candidate_payload(&app.conn, c.id, &payload)?;
                        if alias_worthy(&subject) {
                            graph::add_alias(&app.conn, &sug.node.id, &subject, "review")?;
                        }
                        app.status = format!("subject '{subject}' → {} — a accepts", sug.node.name);
                        app.reload_review()?;
                    } else {
                        app.status = format!("no suggestion for '{subject}' — e to edit");
                    }
                }
            }
        }
        KeyCode::Char('a') => accept_selected(app, false)?,
        // Shift-A: accept even when the subject is new — creates a topic node.
        KeyCode::Char('A') => accept_selected(app, true)?,
        // r: instant reject, no prompt. R: reject with a typed reason.
        KeyCode::Char('r') => {
            if app.selected_candidate().is_some() || !app.review.marked.is_empty() {
                reject_targets(app, "rejected in review")?;
            }
        }
        KeyCode::Char('R') => {
            if app.selected_candidate().is_some() || !app.review.marked.is_empty() {
                app.review.mode = ReviewMode::Reason(LineEdit::new());
            }
        }
        // Edit subject/predicate/object/statement/tags before accepting —
        // e.g. tag a software mention as "recommendation" for later revisit.
        KeyCode::Char('e') => {
            if let Some(c) = app.selected_candidate() {
                let get = |k: &str| {
                    c.payload
                        .get(k)
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                app.review.mode = ReviewMode::Edit(EditState {
                    fields: vec![
                        ("subject", LineEdit::from(get("subject"))),
                        ("predicate", LineEdit::from(get("predicate"))),
                        ("object", LineEdit::from(get("object"))),
                        ("statement", LineEdit::from(get("statement"))),
                        ("tags", LineEdit::from(get("tags"))),
                    ],
                    idx: 0,
                });
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_merge(app: &mut App, key: KeyCode) -> mecha_graph_core::Result<()> {
    match key {
        KeyCode::Char('j') | KeyCode::Down => {
            move_sel(&mut app.merge.list, app.merge.items.len(), 1)
        }
        KeyCode::Char('k') | KeyCode::Up => {
            move_sel(&mut app.merge.list, app.merge.items.len(), -1)
        }
        KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
            app.merge.swap = !app.merge.swap
        }
        KeyCode::Char('m') => {
            if let Some(i) = app.merge.list.selected() {
                if let Some((a, b, name, proposal)) = app.merge.items.get(i).cloned() {
                    let (keep, dup) = if app.merge.swap { (b, a) } else { (a, b) };
                    match graph::merge_nodes(&app.conn, &keep, &dup) {
                        Ok(()) => {
                            rollup::rebuild_person_interactions(&app.conn)?;
                            // Decide AFTER the merge succeeded: a proposal
                            // marked accepted whose repair then failed is a
                            // lie the queue keeps telling.
                            if let Some(id) = proposal {
                                let _ = entity_audit::decide(&app.conn, id, "accepted", "user");
                            }
                            app.status =
                                format!("merged '{name}' → kept {}", &keep[..24.min(keep.len())]);
                        }
                        Err(e) => app.status = format!("merge failed: {e}"),
                    }
                    app.reload_merge()?;
                }
            }
        }
        KeyCode::Char('s') => move_sel(&mut app.merge.list, app.merge.items.len(), 1),
        _ => {}
    }
    Ok(())
}

fn handle_search(app: &mut App, key: KeyCode, mods: KeyModifiers) -> mecha_graph_core::Result<()> {
    if let Some(detail) = &mut app.search.detail {
        // Annotation entry line (t tag / n note on an episode).
        if let Some((kind, buf)) = &mut detail.annotate {
            match key {
                KeyCode::Esc => detail.annotate = None,
                KeyCode::Enter => {
                    let (kind, body) = (*kind, buf.text().trim().to_string());
                    let ep_id = detail.episode_id.unwrap_or_default();
                    if kind == "entity" {
                        // Link a mention: exact resolution first, else the
                        // top typeahead match the ghost was showing.
                        let node = {
                            let mut m = graph::resolve_entity_all(&app.conn, &body)?;
                            if m.len() == 1 {
                                Some(m.remove(0))
                            } else {
                                graph::suggest_entities(&app.conn, &body, 1)?
                                    .into_iter()
                                    .next()
                                    .map(|sg| sg.node)
                            }
                        };
                        match node {
                            Some(n) => {
                                episode::add_mention(&app.conn, ep_id, &n.id, "manual", 1.0)?;
                                app.status = format!("linked {} ({})", n.name, n.node_type);
                            }
                            None => app.status = format!("no entity matches '{body}'"),
                        }
                    } else {
                        match episode::annotate_episode(&app.conn, ep_id, kind, &body) {
                            Ok(true) => app.status = format!("{kind} saved"),
                            Ok(false) => app.status = format!("{kind} already present"),
                            Err(e) => app.status = format!("{kind} failed: {e}"),
                        }
                        detail.anns = episode::annotations_for(&app.conn, ep_id)?;
                    }
                    detail.annotate = None;
                }
                k => {
                    let accept_ghost = matches!(k, KeyCode::Right if buf.at_end())
                        || (k == KeyCode::Char('f') && mods.contains(KeyModifiers::CONTROL));
                    if accept_ghost {
                        let ghost = match *kind {
                            "tag" => ghost_tag(&app.conn, buf.text()),
                            "entity" => ghost_for(&app.conn, buf.text()),
                            _ => None,
                        };
                        if let Some((full, _)) = ghost {
                            buf.set(full);
                            return Ok(());
                        }
                    }
                    buf.handle(k, mods);
                }
            }
            return Ok(());
        }
        if !matches!(key, KeyCode::Char('d')) {
            detail.pending_redact = false;
        }
        match key {
            KeyCode::Esc | KeyCode::Enter => app.search.detail = None,
            KeyCode::Char('t') if detail.episode_id.is_some() => {
                detail.annotate = Some(("tag", LineEdit::new()))
            }
            KeyCode::Char('n') if detail.episode_id.is_some() => {
                detail.annotate = Some(("note", LineEdit::new()))
            }
            KeyCode::Char('m') if detail.episode_id.is_some() => {
                detail.annotate = Some(("entity", LineEdit::new()))
            }
            // Cycle the sensitivity tier.
            KeyCode::Char('p') => {
                if let Some(id) = detail.episode_id {
                    if let Some(ep) = episode::get_episode(&app.conn, id)? {
                        let tiers = episode::SENSITIVITY_TIERS;
                        let cur = tiers.iter().position(|t| *t == ep.sensitivity).unwrap_or(1);
                        let next = tiers[(cur + 1) % tiers.len()];
                        episode::set_sensitivity(&app.conn, id, next)?;
                        detail.base = format!(
                            "{} · {} · sensitivity {}\n\n{}",
                            ep.occurred_at, ep.source, next, ep.body
                        );
                        app.status = format!("sensitivity → {next}");
                    }
                }
            }
            // Redact (true delete): d, then d again to confirm.
            KeyCode::Char('d') => {
                if let Some(id) = detail.episode_id {
                    if let Some(ep) = episode::get_episode(&app.conn, id)? {
                        if detail.pending_redact {
                            episode::redact_episode_undoable(&app.conn, &ep.uid)?;
                            app.search.detail = None;
                            app.search.dirty_since = Some(std::time::Instant::now());
                            app.status = format!(
                                "episode {} deleted — Ctrl-Z (or `pkg undo`) restores",
                                &ep.uid[..8]
                            );
                        } else {
                            detail.pending_redact = true;
                            app.status = format!(
                                "d again to PERMANENTLY delete this {} episode + everything derived",
                                ep.source
                            );
                        }
                    }
                }
            }
            // Edit in $EDITOR — user-authored sources only: evidence from
            // other systems must not be rewritten here.
            KeyCode::Char('e') => {
                if let Some(id) = detail.episode_id {
                    if let Some(ep) = episode::get_episode(&app.conn, id)? {
                        let editable = ep.source == "note" || ep.source.starts_with("reflect.");
                        if !editable {
                            app.status =
                                format!("{} episodes are source-owned — not editable", ep.source);
                        } else if let Some(new_body) = spawn_editor(&ep.body)? {
                            episode::snapshot_edit(&app.conn, id)?;
                            let mut updated = ep.clone();
                            updated.body = new_body.clone();
                            episode::upsert_episode(&app.conn, &updated)?;
                            episode::store_raw(&app.conn, id, &new_body)?;
                            episode::link_by_alias_scan(&app.conn, id, &new_body)?;
                            detail.base = format!(
                                "{} · {} · sensitivity {}\n\n{}",
                                ep.occurred_at, ep.source, ep.sensitivity, new_body
                            );
                            app.status = "edited — re-embeds in tonight's pipeline".into();
                        } else {
                            app.status = "edit aborted (no change)".into();
                        }
                        app.needs_clear = true;
                    }
                }
            }
            _ => {}
        }
        return Ok(());
    }
    // fzf model: the input is always live; arrows browse; Enter opens.
    let (n_amb, n_items) = app
        .search
        .pack
        .as_ref()
        .map(|p| (p.ambiguous.len(), p.items.len()))
        .unwrap_or((0, 0));
    let n = n_amb + n_items;
    match key {
        KeyCode::Char('e') if mods.contains(KeyModifiers::CONTROL) => run_search(app, true)?,
        // Ctrl-P: reveal/hide private+secret tiers, re-running the query.
        KeyCode::Char('p') if mods.contains(KeyModifiers::CONTROL) => {
            app.search.show_private = !app.search.show_private;
            app.status = format!(
                "private tiers {} — re-searching",
                if app.search.show_private {
                    "SHOWN"
                } else {
                    "hidden"
                }
            );
            run_search(app, false)?;
        }
        KeyCode::Char('u') if mods.contains(KeyModifiers::CONTROL) => {
            app.search.input.clear();
            app.search.pack = None;
            app.search.list.select(None);
        }
        KeyCode::Esc => {
            app.search.input.clear();
            app.search.pack = None;
            app.search.list.select(None);
        }
        KeyCode::Down => move_sel(&mut app.search.list, n, 1),
        KeyCode::Up => move_sel(&mut app.search.list, n, -1),
        KeyCode::Enter => {
            let item = app
                .search
                .list
                .selected()
                .filter(|i| *i >= n_amb)
                .and_then(|i| {
                    app.search
                        .pack
                        .as_ref()
                        .and_then(|p| p.items.get(i - n_amb))
                })
                .cloned();
            if let Some(item) = item {
                match item.kind.as_str() {
                    "episode" => match episode::get_episode_by_uid(&app.conn, &item.id)? {
                        Some(ep) => {
                            let raw_note = if episode::has_raw(&app.conn, ep.id)? {
                                format!("\n\n[raw archived — pkg raw {}]", ep.uid)
                            } else {
                                String::new()
                            };
                            app.search.detail = Some(DetailState {
                                base: format!(
                                    "{} · {} · sensitivity {}\n\n{}{}",
                                    ep.occurred_at, ep.source, ep.sensitivity, ep.body, raw_note
                                ),
                                anns: episode::annotations_for(&app.conn, ep.id)?,
                                episode_id: Some(ep.id),
                                annotate: None,
                                pending_redact: false,
                            });
                        }
                        None => app.status = "(episode not found)".into(),
                    },
                    // Entity-shaped hits open the entity page directly.
                    "person_interaction" | "node" => app.open_entity(&item.id)?,
                    "fact" => match fact::get_fact_by_uid(&app.conn, &item.id)? {
                        Some(f) => app.open_entity(&f.subject_id)?,
                        None => app.status = "(fact not found)".into(),
                    },
                    _ => {
                        app.search.detail = Some(DetailState {
                            base: format!("{}\n\nkind: {} · id: {}", item.text, item.kind, item.id),
                            episode_id: None,
                            anns: vec![],
                            annotate: None,
                            pending_redact: false,
                        });
                    }
                }
            }
        }
        k => {
            if app.search.input.handle(k, mods) == EditOutcome::Edited {
                app.search.dirty_since = Some(std::time::Instant::now());
            }
        }
    }
    Ok(())
}

fn handle_capture(app: &mut App, key: KeyCode, mods: KeyModifiers) -> mecha_graph_core::Result<()> {
    // Ctrl-T toggles note ↔ fact.
    if key == KeyCode::Char('t') && mods.contains(KeyModifiers::CONTROL) {
        app.capture.kind = match app.capture.kind {
            CaptureKind::Note => CaptureKind::Fact,
            CaptureKind::Fact => CaptureKind::Note,
        };
        return Ok(());
    }
    match app.capture.kind {
        CaptureKind::Note => match key {
            KeyCode::Enter => {
                let text = app.capture.note.text().trim().to_string();
                if text.is_empty() {
                    return Ok(());
                }
                let ep = episode::Episode {
                    id: 0,
                    uid: String::new(),
                    source: "note".into(),
                    source_id: mecha_graph_core::ids::new_uid(),
                    source_ref: None,
                    body: text.clone(),
                    occurred_at: mecha_graph_core::ids::now(),
                    occurred_end: None,
                    ingested_at: String::new(),
                    lat: None,
                    lon: None,
                    location: None,
                    sensitivity: "personal".into(),
                    scope_id: None,
                    meta: None,
                    raw: None,
                };
                let (id, _) = episode::upsert_episode(&app.conn, &ep)?;
                let linked = episode::link_by_alias_scan(&app.conn, id, &text)?;
                app.status = format!("noted (episode {id}, {linked} entities linked)");
                app.capture.note.clear();
            }
            KeyCode::Esc => app.capture.note.clear(),
            k => {
                app.capture.note.handle(k, mods);
            }
        },
        CaptureKind::Fact => match key {
            KeyCode::Down => app.capture.idx = (app.capture.idx + 1) % app.capture.fields.len(),
            KeyCode::Up => {
                app.capture.idx =
                    (app.capture.idx + app.capture.fields.len() - 1) % app.capture.fields.len()
            }
            KeyCode::Esc => {
                app.capture.fields = empty_fact_fields();
                app.capture.idx = 0;
            }
            KeyCode::Enter => {
                let get = |k: &str| {
                    app.capture
                        .fields
                        .iter()
                        .find(|(l, _)| *l == k)
                        .map(|(_, v)| v.text().trim().to_string())
                        .unwrap_or_default()
                };
                let (subject, statement) = (get("subject"), get("statement"));
                if subject.is_empty() || statement.is_empty() {
                    app.status = "fact needs at least subject + statement".into();
                    return Ok(());
                }
                // Human-authored: stage then immediately accept, creating the
                // subject as a topic if it's new (the human IS the review).
                let proposed = fact::ProposedFact {
                    subject,
                    predicate: {
                        let p = get("predicate");
                        if p.is_empty() {
                            "related_to".into()
                        } else {
                            p
                        }
                    },
                    object: Some(get("object")).filter(|s| !s.is_empty()),
                    object_value: None,
                    statement,
                    valid_from: Some(mecha_graph_core::ids::now()),
                    confidence: Some(0.95),
                    tags: Some(get("tags")).filter(|s| !s.is_empty()),
                    ..Default::default()
                };
                let id = fact::propose_fact(&app.conn, &proposed, "manual:tui", None)?;
                match fact::accept_candidate_opts(&app.conn, id, true, true) {
                    Ok(uid) => {
                        app.status = format!("fact saved ({})", &uid[..8]);
                        app.capture.fields = empty_fact_fields();
                        app.capture.idx = 0;
                    }
                    Err(e) => app.status = format!("save failed: {e}"),
                }
            }
            k => {
                let field = &mut app.capture.fields[app.capture.idx];
                let accept_ghost = matches!(k, KeyCode::Right if field.1.at_end())
                    || (k == KeyCode::Char('f') && mods.contains(KeyModifiers::CONTROL));
                if accept_ghost && is_entity_field(field.0) {
                    if let Some((name, _)) = ghost_for(&app.conn, field.1.text()) {
                        field.1.set(name);
                        return Ok(());
                    }
                }
                field.1.handle(k, mods);
            }
        },
    }
    Ok(())
}

/// One line of text, then one call into core. Every outcome — including
/// every refusal — lands in the status line and leaves the page open, on
/// the same reasoning as the CLI: a collision is a *question* (merge? or
/// did you mean this other node?), and answering it needs the page you were
/// already reading.
fn handle_entity_edit(
    app: &mut App,
    key: KeyCode,
    mods: KeyModifiers,
) -> mecha_graph_core::Result<()> {
    match key {
        KeyCode::Esc => {
            app.entity.edit = None;
        }
        KeyCode::Enter => {
            let Some(edit) = app.entity.edit.take() else {
                return Ok(());
            };
            let text = edit.input.text().trim().to_string();
            if text.is_empty() {
                return Ok(());
            }
            let here = app.entity.node.as_ref().map(|n| n.id.clone());
            match edit.kind {
                EditKind::Rename => {
                    let Some(id) = here else { return Ok(()) };
                    match graph::rename_node(&app.conn, &id, &text) {
                        Ok(fix) => {
                            app.status = format!(
                                "renamed {} → {} ({:?} kept as an alias)",
                                fix.from, fix.to, fix.from
                            );
                            app.refresh_entity()?;
                        }
                        Err(e) => app.status = format!("rename refused: {e}"),
                    }
                }
                EditKind::Alias => {
                    let Some(id) = here else { return Ok(()) };
                    match graph::add_alias(&app.conn, &id, &text, "manual") {
                        Ok(()) => {
                            app.status = format!("alias added: {text:?}");
                            app.refresh_entity()?;
                        }
                        Err(e) => app.status = format!("alias failed: {e}"),
                    }
                }
                EditKind::NewPerson => match graph::create_person(&app.conn, &text, "manual") {
                    Ok(node) => {
                        let id = node.id.clone();
                        app.status = format!("created {} ({})", node.name, node.id);
                        // Open it: the next thing anyone wants is the page of
                        // the person they just made.
                        app.open_entity(&id)?;
                    }
                    Err(e) => app.status = format!("create refused: {e}"),
                },
            }
        }
        k => {
            if let Some(edit) = app.entity.edit.as_mut() {
                edit.input.handle(k, mods);
            }
        }
    }
    Ok(())
}

fn handle_entity(app: &mut App, key: KeyCode, mods: KeyModifiers) -> mecha_graph_core::Result<()> {
    // An identity edit is layered over whatever page is behind it, so it
    // takes every key first and returns. Nothing below sees them.
    if app.entity.edit.is_some() {
        return handle_entity_edit(app, key, mods);
    }
    match app.entity.mode {
        EntityMode::Input => match key {
            // Ctrl-N, not `n`: this page accepts text, so a bare letter is a
            // letter. Prefilled with whatever was typed, because the moment
            // you want this is the moment a lookup came back empty.
            KeyCode::Char('n') if mods.contains(KeyModifiers::CONTROL) => {
                let mut input = LineEdit::new();
                for c in app.entity.input.text().trim().chars() {
                    input.handle(KeyCode::Char(c), KeyModifiers::NONE);
                }
                app.entity.edit = Some(EntityEdit {
                    kind: EditKind::NewPerson,
                    input,
                });
            }
            KeyCode::Esc => {
                app.entity.input.clear();
                app.entity.suggestions.clear();
                app.entity.sug.select(None);
            }
            KeyCode::Down => {
                let n = app.entity.suggestions.len();
                if n > 0 {
                    move_sel(&mut app.entity.sug, n, 1);
                }
            }
            KeyCode::Up => {
                let n = app.entity.suggestions.len();
                if n > 0 {
                    move_sel(&mut app.entity.sug, n, -1);
                }
            }
            KeyCode::Enter => {
                // A highlighted suggestion wins; else the exact-resolve path.
                if let Some(sug) = app
                    .entity
                    .sug
                    .selected()
                    .and_then(|i| app.entity.suggestions.get(i))
                {
                    let id = sug.node.id.clone();
                    app.entity.suggestions.clear();
                    app.entity.sug.select(None);
                    app.open_entity(&id)?;
                    return Ok(());
                }
                let q = app.entity.input.text().trim().to_string();
                if q.is_empty() {
                    return Ok(());
                }
                let matches = graph::resolve_entity_all(&app.conn, &q)?;
                match matches.len() {
                    0 => app.status = format!("no entity matches '{q}'"),
                    1 => app.open_entity(&matches[0].id)?,
                    _ => {
                        app.status = format!("{} matches for '{q}'", matches.len());
                        app.entity.candidates = matches;
                        app.entity.pick.select(Some(0));
                        app.entity.mode = EntityMode::Pick;
                    }
                }
            }
            k => {
                if app.entity.input.handle(k, mods) == EditOutcome::Edited {
                    app.entity.suggestions =
                        graph::suggest_entities(&app.conn, app.entity.input.text(), 8)?;
                    app.entity.sug.select(if app.entity.suggestions.is_empty() {
                        None
                    } else {
                        Some(0)
                    });
                }
            }
        },
        EntityMode::Pick => match key {
            KeyCode::Esc => app.entity.mode = EntityMode::Input,
            KeyCode::Char('j') | KeyCode::Down => {
                move_sel(&mut app.entity.pick, app.entity.candidates.len(), 1)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                move_sel(&mut app.entity.pick, app.entity.candidates.len(), -1)
            }
            KeyCode::Enter => {
                if let Some(node) = app
                    .entity
                    .pick
                    .selected()
                    .and_then(|i| app.entity.candidates.get(i))
                {
                    let id = node.id.clone();
                    app.open_entity(&id)?;
                }
            }
            _ => {}
        },
        EntityMode::View => match key {
            KeyCode::Esc | KeyCode::Char('/') => {
                app.entity.mode = EntityMode::Input;
                app.entity.input.clear();
            }
            KeyCode::Left | KeyCode::Right | KeyCode::Char('h') | KeyCode::Char('l') => {
                app.entity.timeline_focus = !app.entity.timeline_focus;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if app.entity.timeline_focus {
                    move_sel(&mut app.entity.timeline, app.entity.episodes.len(), 1)
                } else {
                    move_sel(&mut app.entity.list, app.entity.facts.len(), 1)
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if app.entity.timeline_focus {
                    move_sel(&mut app.entity.timeline, app.entity.episodes.len(), -1)
                } else {
                    move_sel(&mut app.entity.list, app.entity.facts.len(), -1)
                }
            }
            // The three identity edits. `n` is on the View page as well as
            // the lookup page because "there is no node for this person" is
            // a thing you discover *while reading* the wrong one.
            KeyCode::Char('r') => {
                if app.entity.node.is_some() {
                    app.entity.edit = Some(EntityEdit {
                        kind: EditKind::Rename,
                        input: LineEdit::new(),
                    });
                }
            }
            KeyCode::Char('a') => {
                if app.entity.node.is_some() {
                    app.entity.edit = Some(EntityEdit {
                        kind: EditKind::Alias,
                        input: LineEdit::new(),
                    });
                }
            }
            KeyCode::Char('n') => {
                app.entity.edit = Some(EntityEdit {
                    kind: EditKind::NewPerson,
                    input: LineEdit::new(),
                });
            }
            // Supersede the selected fact: ends both timelines now;
            // history stays queryable via timeline/as-of.
            KeyCode::Char('s') => {
                let fact = app
                    .entity
                    .list
                    .selected()
                    .and_then(|i| app.entity.facts.get(i))
                    .map(|f| (f.uid.clone(), f.statement.clone()));
                if let Some((uid, statement)) = fact {
                    match fact::supersede_fact(&app.conn, &uid, None) {
                        Ok(()) => {
                            app.status = format!(
                                "superseded: {}",
                                statement.chars().take(60).collect::<String>()
                            );
                            app.refresh_entity()?;
                        }
                        Err(e) => app.status = format!("supersede failed: {e}"),
                    }
                }
            }
            // Verdicts on unreviewed (shadow) facts, in place: the entity
            // view is a review trigger, and the reviewable object is the
            // fact beside everything else known about its subject.
            KeyCode::Char('y') => {
                let fact = app
                    .entity
                    .list
                    .selected()
                    .and_then(|i| app.entity.facts.get(i))
                    .map(|f| (f.uid.clone(), f.is_shadow()));
                if let Some((uid, is_shadow)) = fact {
                    if is_shadow {
                        match fact::confirm_shadow_fact(&app.conn, &uid) {
                            Ok(()) => {
                                app.status = "confirmed — now reviewed".into();
                                app.refresh_entity()?;
                            }
                            Err(e) => app.status = format!("confirm failed: {e}"),
                        }
                    } else {
                        app.status = "already reviewed — y is for ◌ facts".into();
                    }
                }
            }
            KeyCode::Char('u') => {
                let fact = app
                    .entity
                    .list
                    .selected()
                    .and_then(|i| app.entity.facts.get(i))
                    .map(|f| (f.uid.clone(), f.is_shadow()));
                if let Some((uid, is_shadow)) = fact {
                    if is_shadow {
                        match fact::refute_shadow_fact(
                            &app.conn,
                            &uid,
                            "refuted on the entity page",
                        ) {
                            Ok(()) => {
                                app.status = "refuted — retracted as never true".into();
                                app.refresh_entity()?;
                            }
                            Err(e) => app.status = format!("refute failed: {e}"),
                        }
                    } else {
                        app.status =
                            "already reviewed — s supersedes what stopped being true".into();
                    }
                }
            }
            // Jump across the graph: Enter follows the selected fact to its
            // other endpoint — or, when the timeline pane has focus, opens
            // the selected episode in the search detail view (t/n annotate).
            KeyCode::Enter if app.entity.timeline_focus => {
                let ep = app
                    .entity
                    .timeline
                    .selected()
                    .and_then(|i| app.entity.episodes.get(i))
                    .cloned();
                if let Some(ep) = ep {
                    let raw_note = if episode::has_raw(&app.conn, ep.id)? {
                        format!("\n\n[raw archived — pkg raw {}]", ep.uid)
                    } else {
                        String::new()
                    };
                    app.search.detail = Some(DetailState {
                        base: format!(
                            "{} · {} · sensitivity {}\n\n{}{}",
                            ep.occurred_at, ep.source, ep.sensitivity, ep.body, raw_note
                        ),
                        anns: episode::annotations_for(&app.conn, ep.id)?,
                        episode_id: Some(ep.id),
                        annotate: None,
                        pending_redact: false,
                    });
                    app.screen = Screen::Search;
                }
            }
            KeyCode::Enter => {
                let here = app
                    .entity
                    .node
                    .as_ref()
                    .map(|n| n.id.clone())
                    .unwrap_or_default();
                let other = app
                    .entity
                    .list
                    .selected()
                    .and_then(|i| app.entity.facts.get(i))
                    .and_then(|f| {
                        if f.subject_id != here {
                            Some(f.subject_id.clone())
                        } else {
                            f.object_id.clone()
                        }
                    });
                if let Some(id) = other {
                    app.open_entity(&id)?;
                }
            }
            _ => {}
        },
    }
    Ok(())
}

fn handle_gtd(app: &mut App, key: KeyCode, mods: KeyModifiers) -> mecha_graph_core::Result<()> {
    if let GtdMode::Form {
        fields,
        idx,
        editing,
    } = &mut app.gtd.mode
    {
        match key {
            KeyCode::Esc => app.gtd.mode = GtdMode::List,
            KeyCode::Down => *idx = (*idx + 1) % fields.len(),
            KeyCode::Up => *idx = (*idx + fields.len() - 1) % fields.len(),
            KeyCode::Enter => {
                let get = |k: &str| {
                    fields
                        .iter()
                        .find(|(l, _)| *l == k)
                        .map(|(_, v)| v.text().trim().to_string())
                        .unwrap_or_default()
                };
                // Dates bounce the form with an error instead of saving junk.
                let due = match gtd::parse_due(&get("due")) {
                    Ok(d) => d,
                    Err(e) => {
                        app.status = e.to_string();
                        return Ok(());
                    }
                };
                let result = match editing.clone() {
                    None => gtd::create_task(
                        &app.conn,
                        &get("name"),
                        due.as_deref(),
                        Some(get("project")).filter(|s| !s.is_empty()).as_deref(),
                        Some(get("context")).filter(|s| !s.is_empty()).as_deref(),
                    )
                    .map(|_| "task created".to_string()),
                    Some(node_id) => match gtd::parse_due(&get("defer")) {
                        Err(e) => {
                            app.status = e.to_string();
                            return Ok(());
                        }
                        Ok(defer) => gtd::update_task_schedule(
                            &app.conn,
                            &node_id,
                            Some(due.as_deref()),
                            Some(defer.as_deref()),
                            Some(Some(get("context")).filter(|s| !s.is_empty()).as_deref()),
                        )
                        .map(|_| "task updated".to_string()),
                    },
                };
                match result {
                    Ok(msg) => {
                        app.status = msg;
                        app.gtd.mode = GtdMode::List;
                        app.reload_gtd()?;
                    }
                    Err(e) => app.status = e.to_string(),
                }
            }
            k => {
                let field = &mut fields[*idx];
                let accept_ghost = matches!(k, KeyCode::Right if field.1.at_end())
                    || (k == KeyCode::Char('f') && mods.contains(KeyModifiers::CONTROL));
                if accept_ghost && is_entity_field(field.0) {
                    if let Some((name, _)) = ghost_for(&app.conn, field.1.text()) {
                        field.1.set(name);
                        return Ok(());
                    }
                }
                field.1.handle(k, mods);
            }
        }
        return Ok(());
    }

    let set_status = |app: &mut App, status: &str| -> mecha_graph_core::Result<()> {
        let task = app
            .gtd
            .list
            .selected()
            .and_then(|i| app.gtd.items.get(i))
            .map(|t| (t.node_id.clone(), t.name.clone()));
        if let Some((id, name)) = task {
            match gtd::set_task_status(&app.conn, &id, status) {
                Ok(()) => {
                    app.status = format!("{status}: {}", name.chars().take(50).collect::<String>())
                }
                Err(e) => app.status = format!("status change failed: {e}"),
            }
            app.reload_gtd()?;
        }
        Ok(())
    };
    match key {
        KeyCode::Char('j') | KeyCode::Down => move_sel(&mut app.gtd.list, app.gtd.items.len(), 1),
        KeyCode::Char('k') | KeyCode::Up => move_sel(&mut app.gtd.list, app.gtd.items.len(), -1),
        KeyCode::Char('n') => set_status(app, "next")?,
        KeyCode::Char('i') => set_status(app, "inbox")?,
        KeyCode::Char('w') => set_status(app, "waiting")?,
        KeyCode::Char('s') => set_status(app, "scheduled")?,
        KeyCode::Char('d') => set_status(app, "done")?,
        KeyCode::Char('x') => set_status(app, "dropped")?,
        // Space walks the active statuses (next → inbox → scheduled → waiting).
        KeyCode::Char(' ') => {
            let next = app
                .gtd
                .list
                .selected()
                .and_then(|i| app.gtd.items.get(i))
                .map(|t| {
                    let active = &gtd::TASK_STATUSES[..4];
                    let cur = active.iter().position(|s| *s == t.status).unwrap_or(0);
                    active[(cur + 1) % active.len()]
                });
            if let Some(status) = next {
                set_status(app, status)?;
            }
        }
        KeyCode::Char('z') => {
            app.gtd.show_closed = !app.gtd.show_closed;
            app.reload_gtd()?;
        }
        KeyCode::Char('a') => {
            app.gtd.mode = GtdMode::Form {
                fields: vec![
                    ("name", LineEdit::new()),
                    ("due", LineEdit::new()),
                    ("project", LineEdit::new()),
                    ("context", LineEdit::new()),
                ],
                idx: 0,
                editing: None,
            };
        }
        KeyCode::Char('e') => {
            if let Some(t) = app.gtd.list.selected().and_then(|i| app.gtd.items.get(i)) {
                app.gtd.mode = GtdMode::Form {
                    fields: vec![
                        ("due", LineEdit::from(t.due_at.clone().unwrap_or_default())),
                        (
                            "defer",
                            LineEdit::from(t.defer_until.clone().unwrap_or_default()),
                        ),
                        (
                            "context",
                            LineEdit::from(t.context_tag.clone().unwrap_or_default()),
                        ),
                    ],
                    idx: 0,
                    editing: Some(t.node_id.clone()),
                };
            }
        }
        // A task is a node: its page shows waiting_on/about facts + provenance.
        KeyCode::Enter => {
            let id = app
                .gtd
                .list
                .selected()
                .and_then(|i| app.gtd.items.get(i))
                .map(|t| t.node_id.clone());
            if let Some(id) = id {
                app.open_entity(&id)?;
            }
        }
        _ => {}
    }
    Ok(())
}

// ─── Drawing ─────────────────────────────────────────────────────────────────

fn draw(f: &mut Frame, app: &mut App) {
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Min(3),
            Constraint::Length(1),
        ])
        .split(f.area());

    let titles = [
        "1 review",
        "2 merge",
        "3 search",
        "4 capture",
        "5 entity",
        "6 gtd",
        "7 stats",
    ];
    let idx = match app.screen {
        Screen::Review => 0,
        Screen::Merge => 1,
        Screen::Search => 2,
        Screen::Capture => 3,
        Screen::Entity => 4,
        Screen::Gtd => 5,
        Screen::Stats => 6,
    };
    let tabs = Tabs::new(titles.iter().map(|t| Line::from(*t)).collect::<Vec<_>>())
        .select(idx)
        .highlight_style(
            Style::default()
                .add_modifier(Modifier::BOLD)
                .fg(Color::Cyan),
        );
    f.render_widget(tabs, chunks[0]);

    match app.screen {
        Screen::Review => draw_review(f, app, chunks[1]),
        Screen::Merge => draw_merge(f, app, chunks[1]),
        Screen::Search => draw_search(f, app, chunks[1]),
        Screen::Capture => draw_capture(f, app, chunks[1]),
        Screen::Entity => draw_entity(f, app, chunks[1]),
        Screen::Gtd => draw_gtd(f, app, chunks[1]),
        Screen::Stats => draw_stats(f, app, chunks[1]),
    }

    let status = Paragraph::new(app.status.as_str()).style(Style::default().fg(Color::DarkGray));
    f.render_widget(status, chunks[2]);
}

/// Cluster view: the queue grouped by (proposer, predicate), largest
/// first, with the verdict-history prior — one decision per class.
/// How much human evidence a rate rests on, as a word.
///
/// The point of the column: a bare percentage reads the same at n=2 and
/// n=200, and the whole queue was misread once already because a rate with
/// almost nothing behind it was taken as a verdict on the mechanism.
fn evidence_word(n: i64) -> (&'static str, Color) {
    match n {
        0 => ("unjudged", Color::DarkGray),
        1..=9 => ("thin", Color::Yellow),
        10..=29 => ("some", Color::White),
        _ => ("solid", Color::Green),
    }
}

fn draw_review_proposers(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(52), Constraint::Percentage(48)])
        .split(area);

    let total: usize = app.review.proposers.iter().map(|p| p.pending).sum();
    let items: Vec<ListItem> = app
        .review
        .proposers
        .iter()
        .map(|p| {
            let (word, colour) = evidence_word(p.judged());
            // An unknown rate renders as a dash, never as 0% — the two mean
            // opposite things and printing them alike is what made a
            // never-reviewed mechanism look like a rejected one.
            let rate = match p.accept_rate() {
                Some(r) => format!("{:>4.0}%", r * 100.0),
                None => "   —".into(),
            };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>5} ", p.pending),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(format!(
                    "{:<24}",
                    p.proposer.chars().take(24).collect::<String>()
                )),
                Span::styled(rate, Style::default().fg(colour)),
                // No "/0" behind a dash: a denominator of nothing is not a
                // sample size, and printing one invites reading the dash as
                // a measured zero.
                Span::styled(
                    match p.judged() {
                        0 => "      ".to_string(),
                        n => format!(" /{n:<4} "),
                    },
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(format!("{word:<8}"), Style::default().fg(colour)),
            ]))
        })
        .collect();
    let title = format!(
        " proposers ({total} pending from {}) — j/k · Enter classes · c items · p/Esc back ",
        app.review.proposers.len()
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, cols[0], &mut app.review.proposer_list);

    let detail = match app.selected_proposer() {
        Some(p) => {
            let mut t = format!(
                "proposer:   {}\npending:    {} across {} class(es)\n\n",
                p.proposer, p.pending, p.classes
            );
            match p.accept_rate() {
                Some(r) => {
                    t.push_str(&format!(
                        "you accepted: {}✓ / {}✗  ({:.0}% of {} decisions)\n",
                        p.accepted_hist,
                        p.rejected_hist,
                        r * 100.0,
                        p.judged()
                    ));
                    if let Some(lb) = p.accept_lb {
                        t.push_str(&format!(
                            "confident at: {:.0}% or better (Wilson 95% lower bound)\n",
                            lb * 100.0
                        ));
                    }
                }
                None => t.push_str(
                    "you accepted: nothing yet — no human verdict on this\n              mechanism, so it has no rate to read.\n",
                ),
            }
            t.push_str(&format!(
                "auto-dropped: {} (duplicates and ephemerals — the\n              pipeline's own, never counted above)\n",
                p.machine_rejected
            ));
            let (word, _) = evidence_word(p.judged());
            t.push_str(&format!("\nevidence:   {word}\n"));
            t.push_str(match p.judged() {
                0 => "\nNo basis to judge this mechanism yet. Sample a dozen of\nits items at random — random being what makes the rate\nmean anything — before concluding it proposes badly.\n",
                1..=9 => "\nToo few decisions to separate a bad mechanism from an\nunlucky handful. A dozen would settle it.\n",
                10..=29 => "\nEnough to be suggestive, not enough to act on for a\nmechanism this size.\n",
                _ => "\nEnough decisions to trust this rate.\n",
            });
            t.push_str("\nEnter  the classes this mechanism proposes\nc      every pending item from it, unfiltered");
            t
        }
        None => "queue is empty 🎉".into(),
    };
    f.render_widget(
        Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" mechanism ")),
        cols[1],
    );
}

fn draw_review_clusters(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let total: usize = app.review.clusters.iter().map(|c| c.pending).sum();
    let items: Vec<ListItem> = app
        .review
        .clusters
        .iter()
        .map(|cl| {
            let judged = cl.accepted_hist + cl.rejected_hist;
            let hist = if judged > 0 {
                format!(
                    " {:.0}%✓/{judged}",
                    100.0 * cl.accepted_hist as f64 / judged as f64
                )
            } else {
                " unjudged".into()
            };
            let tag = if cl.commitment { " ⏰" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>4} ", cl.pending),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(cl.predicate.clone()),
                Span::styled(
                    format!(" · {}", cl.proposed_by),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::styled(hist, Style::default().fg(Color::DarkGray)),
                Span::styled(tag, Style::default().fg(Color::Yellow)),
            ]))
        })
        .collect();
    // The narrowing has to be in the title: a filtered list that does not
    // say so is a list that looks like the whole queue got smaller.
    let title = match &app.review.proposer_filter {
        Some(p) => format!(
            " clusters · {p} ({total} pending in {}) — Enter drill in · c items · Esc proposers ",
            app.review.clusters.len()
        ),
        None => format!(
            " clusters ({total} pending in {}) — a/r verdict cluster · g groups · Enter drill in · p proposers · c items ",
            app.review.clusters.len()
        ),
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, cols[0], &mut app.review.cluster_list);

    let para = match &app.review.mode {
        ReviewMode::Reason(buf) => Paragraph::new(format!(
            "reject reason for the WHOLE cluster (Enter to confirm, Esc to cancel):\n> {}",
            buf.display()
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" reject cluster "),
        ),
        _ => {
            let detail = match app.selected_cluster() {
                Some(cl) => {
                    let mut text = format!(
                        "proposer:   {}\npredicate:  {}\npending:    {} · confidence {:.2}–{:.2}\n",
                        cl.proposed_by, cl.predicate, cl.pending, cl.conf_min, cl.conf_max
                    );
                    let (a, r) = (cl.accepted_hist, cl.rejected_hist);
                    if a + r > 0 {
                        text.push_str(&format!(
                            "you said:   {a}✓ / {r}✗ ({:.0}% over {} decision(s))\n",
                            100.0 * a as f64 / (a + r) as f64,
                            a + r
                        ));
                    } else {
                        text.push_str("you said:   nothing yet — no human verdict here\n");
                    }
                    // Kept out of the rate above: a class that mostly makes
                    // duplicates is an argument about the extractor, not
                    // about whether the predicate is wanted.
                    text.push_str(&format!(
                        "auto-dropped: {} (dups / ephemerals)\n",
                        cl.machine_rejected
                    ));
                    text.push_str(&format!(
                        "ladder:     {} · streak {}/{}\n",
                        cl.rung,
                        cl.streak,
                        mecha_graph_core::ladder::PROMOTE_STREAK
                    ));
                    text.push_str("\n─ typical samples (spread over the cluster) ─\n");
                    for s in &cl.samples {
                        text.push_str(&format!("• {}\n", s.chars().take(160).collect::<String>()));
                    }
                    if cl.commitment {
                        text.push_str(
                            "\n⏰ commitments materialize tasks — no bulk verdicts.\nEnter reviews them individually.",
                        );
                    } else {
                        text.push_str(
                            "\na accept cluster · A accept creating topics\nr reject cluster (R adds a reason) · Enter inspects the items first",
                        );
                    }
                    text
                }
                None => "queue is empty 🎉".into(),
            };
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" cluster "))
        }
    };
    f.render_widget(para, cols[1]);
}

fn draw_review_shadow(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let items: Vec<ListItem> = app
        .review
        .shadow
        .iter()
        .map(|sf| {
            let contra = sf.reasons.iter().any(|r| r.starts_with("contradicts"));
            let tag = if contra { " ⚡" } else { "" };
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:.2} ", sf.fact.confidence),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(sf.fact.statement.chars().take(70).collect::<String>()),
                Span::styled(tag, Style::default().fg(Color::Yellow)),
            ]))
        })
        .collect();
    let title = format!(
        " surfaced for verdict ({}) — y confirm · r/R refute · s/Esc back ",
        app.review.shadow.len()
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, cols[0], &mut app.review.shadow_list);

    let para = match &app.review.mode {
        ReviewMode::ShadowReason(buf) => Paragraph::new(format!(
            "refute reason — it feeds rejection memory (Enter to confirm, Esc to cancel):
> {}",
            buf.display()
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" refute fact "),
        ),
        _ => {
            let detail = match app.selected_shadow() {
                Some(sf) => {
                    let mut text = format!(
                        "{}

class:      {} · {}
confidence: {:.2}
since:      {}
",
                        sf.fact.statement,
                        sf.fact.extractor.as_deref().unwrap_or("?"),
                        sf.fact.predicate,
                        sf.fact.confidence,
                        &sf.fact.ingested_at[..10.min(sf.fact.ingested_at.len())],
                    );
                    text.push_str(
                        "
─ why it surfaced ─
",
                    );
                    for r in &sf.reasons {
                        text.push_str(&format!(
                            "• {r}
"
                        ));
                    }
                    text.push_str(
                        "
y: confirm — a human stands behind it (tier → reviewed)
                         r: refute — it was never true (retracted; class learns)
",
                    );
                    text
                }
                None => "nothing surfaced — no shadow fact is about to matter.

                     Shadow facts earn review when a query serves them, when they
                     contradict a reviewed fact, or when a sampled class spot-checks
                     them. An idle shadow fact costs nothing and waits."
                    .into(),
            };
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" fact "))
        }
    };
    f.render_widget(para, cols[1]);
}

fn draw_review_groups(f: &mut Frame, app: &mut App, area: Rect) {
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let items: Vec<ListItem> = app
        .review
        .groups
        .iter()
        .map(|g| {
            ListItem::new(Line::from(vec![
                Span::styled(
                    format!("{:>3}× ", g.size()),
                    Style::default().fg(Color::Yellow),
                ),
                Span::raw(g.leader_statement.chars().take(66).collect::<String>()),
            ]))
        })
        .collect();
    let class = app
        .review
        .group_class
        .as_ref()
        .map(|(p, k)| format!("{p} · {k}"))
        .unwrap_or_default();
    let title = format!(
        " groups {class} ({}) — a/r verdict the group · R with reason · g/Esc back ",
        app.review.groups.len()
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, cols[0], &mut app.review.group_list);

    let para = match &app.review.mode {
        ReviewMode::GroupReason(buf) => Paragraph::new(format!(
            "reject reason for the WHOLE group (Enter to confirm, Esc to cancel):\n> {}",
            buf.display()
        ))
        .wrap(Wrap { trim: false })
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" reject group "),
        ),
        _ => {
            let detail = match app.selected_group() {
                Some(g) => {
                    let mut text = format!("{}\n", g.leader_statement);
                    if !g.members.is_empty() {
                        text.push_str(&format!(
                            "\n─ {} member(s) this verdict also covers ─\n",
                            g.members.len()
                        ));
                        for (s, (_, sim)) in g.sample.iter().zip(g.members.iter()) {
                            text.push_str(&format!(
                                "• {} (cosine {sim:.2})\n",
                                s.chars().take(70).collect::<String>()
                            ));
                        }
                        if g.members.len() > g.sample.len() {
                            text.push_str(&format!(
                                "  … and {} more\n",
                                g.members.len() - g.sample.len()
                            ));
                        }
                    }
                    // What a group verdict costs, in the pane where it is
                    // spent: same-class pairs matched the owner's own record
                    // ~89% of the time (measured 2026-08-29) — cascaded
                    // members are machine-labeled and revisable.
                    text.push_str(
                        "\none keystroke = one human verdict (yours, on the leader);\n\
                         members cascade machine-labeled — measured ~89% same-class\n\
                         agreement, so expect ~1 in 10 to deserve a second look\n",
                    );
                    text
                }
                None => "no groups — this class's pending items share nothing at the floor".into(),
            };
            Paragraph::new(detail)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" group "))
        }
    };
    f.render_widget(para, cols[1]);
}

fn draw_review(f: &mut Frame, app: &mut App, area: Rect) {
    if app.review.group_view {
        draw_review_groups(f, app, area);
        return;
    }
    if app.review.shadow_view {
        draw_review_shadow(f, app, area);
        return;
    }
    if app.review.proposer_view {
        draw_review_proposers(f, app, area);
        return;
    }
    if app.review.cluster_view {
        draw_review_clusters(f, app, area);
        return;
    }
    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(45), Constraint::Percentage(55)])
        .split(area);

    let items: Vec<ListItem> = app
        .review
        .items
        .iter()
        .map(|c| {
            let statement = c
                .payload
                .get("statement")
                .and_then(|s| s.as_str())
                .or_else(|| c.payload.get("what").and_then(|s| s.as_str()))
                .unwrap_or("(no statement)");
            let is_commit = c.payload.get("kind").and_then(|k| k.as_str()) == Some("commitment");
            let tag = if is_commit { " ⏰" } else { "" };
            let mark = if app.review.marked.contains(&c.id) {
                "●"
            } else {
                " "
            };
            ListItem::new(Line::from(vec![
                Span::styled(format!("{mark} "), Style::default().fg(Color::Yellow)),
                Span::styled(
                    format!("{:.2} ", c.confidence.unwrap_or(0.0)),
                    Style::default().fg(Color::DarkGray),
                ),
                Span::raw(statement.chars().take(70).collect::<String>()),
                Span::styled(tag, Style::default().fg(Color::Yellow)),
            ]))
        })
        .collect();
    let title = if !app.review.marked.is_empty() {
        format!(
            " review queue ({}) — {} marked · a/r apply to marked · Esc clears ",
            app.review.items.len(),
            app.review.marked.len()
        )
    } else if let Some((_, pred)) = &app.review.cluster_filter {
        format!(
            " cluster {pred} ({}) — a accept · e edit · r reject · Esc back to clusters ",
            app.review.items.len()
        )
    } else if let Some(p) = &app.review.proposer_filter {
        format!(
            " from {p} ({}) — a accept · e edit · r reject · Esc back to proposers ",
            app.review.items.len()
        )
    } else {
        format!(
            " review queue ({}) — Space mark · a accept · e edit · r/R reject · c clusters · p proposers ",
            app.review.items.len()
        )
    };
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, cols[0], &mut app.review.list);

    // Detail pane: payload + resolution help + source episode.
    let detail = match app.selected_candidate().cloned() {
        Some(c) => {
            let mut text = serde_json::to_string_pretty(&c.payload).unwrap_or_default();
            text.push_str(&format!(
                "\n\nproposed by {} · {}\n",
                c.proposed_by.as_deref().unwrap_or("?"),
                c.created_at
            ));
            // Unresolvable subject: show typeahead matches; b binds the top
            // one (and learns the original string as an alias).
            if let Some(subject) = c.payload.get("subject").and_then(|v| v.as_str()) {
                let n = graph::resolve_entity_all(&app.conn, subject)
                    .map(|m| m.len())
                    .unwrap_or(0);
                if n != 1 && !subject.is_empty() {
                    let sugs = graph::suggest_entities(&app.conn, subject, 3).unwrap_or_default();
                    if sugs.is_empty() {
                        text.push_str(&format!(
                            "\n─ subject '{subject}' doesn't resolve — e to edit ─\n"
                        ));
                    } else {
                        text.push_str(&format!("\n─ subject '{subject}' — did you mean ─\n"));
                        for (i, sg) in sugs.iter().enumerate() {
                            let marker = if i == 0 { "b → " } else { "    " };
                            text.push_str(&format!(
                                "{marker}{} ({})\n",
                                sg.node.name, sg.node.node_type
                            ));
                        }
                    }
                }
            }
            if let Some(ep_id) = c.episode_id {
                text.push_str("\n─ source episode ─\n");
                text.push_str(&app.episode_preview(ep_id));
            }
            text
        }
        None => "queue is empty 🎉".into(),
    };
    // Ghost completion for the active entity-shaped edit field, computed
    // before `mode` is borrowed for rendering.
    let edit_ghost: Option<String> = match &app.review.mode {
        ReviewMode::Edit(edit) => {
            let (label, value) = &edit.fields[edit.idx];
            if is_entity_field(label) {
                ghost_for(&app.conn, value.text()).map(|(_, d)| d)
            } else {
                None
            }
        }
        _ => None,
    };
    let para = match &app.review.mode {
        ReviewMode::Reason(buf) | ReviewMode::ShadowReason(buf) | ReviewMode::GroupReason(buf) => {
            Paragraph::new(format!(
                "reject reason (Enter to confirm, Esc to cancel):\n> {}",
                buf.display()
            ))
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" reject "))
        }
        ReviewMode::Edit(edit) => {
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                "↑/↓ move field · Enter save · Esc cancel · →/Ctrl-F complete entity",
                Style::default().fg(Color::DarkGray),
            ))];
            for (i, (label, value)) in edit.fields.iter().enumerate() {
                let active = i == edit.idx;
                let mut spans = vec![
                    Span::styled(
                        format!("{label:>10}: "),
                        if active {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    ),
                    Span::raw(if active {
                        value.display()
                    } else {
                        value.text().to_string()
                    }),
                ];
                if active {
                    if let Some(g) = &edit_ghost {
                        spans.push(Span::styled(
                            g.clone(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ));
                    }
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "tip: tags like \"recommendation,software\" make this revisitable via pkg facts --tag",
                Style::default().fg(Color::DarkGray),
            )));
            Paragraph::new(lines).wrap(Wrap { trim: false }).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" edit candidate "),
            )
        }
        ReviewMode::Normal => Paragraph::new(detail)
            .wrap(Wrap { trim: false })
            .block(Block::default().borders(Borders::ALL).title(" provenance ")),
    };
    f.render_widget(para, cols[1]);
}

fn draw_merge(f: &mut Frame, app: &mut App, area: Rect) {
    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(6), Constraint::Min(3)])
        .split(area);

    let items: Vec<ListItem> = app
        .merge
        .items
        .iter()
        .map(|(_, _, name, _)| ListItem::new(name.as_str()))
        .collect();
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(format!(
            " merge candidates ({}) — j/k · ←/→ swap keep side · m merge · s skip ",
            app.merge.items.len()
        )))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, rows[0], &mut app.merge.list);

    let cols = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([Constraint::Percentage(50), Constraint::Percentage(50)])
        .split(rows[1]);

    let (left, right) = match app
        .merge
        .list
        .selected()
        .and_then(|i| app.merge.items.get(i))
    {
        Some((a, b, _, _)) => (app.node_summary(a), app.node_summary(b)),
        None => ("no candidates 🎉".into(), String::new()),
    };
    let (keep_left, keep_right) = if app.merge.swap {
        (false, true)
    } else {
        (true, false)
    };
    let style_for = |keep: bool| {
        if keep {
            Style::default().fg(Color::Green)
        } else {
            Style::default().fg(Color::Red)
        }
    };
    let ltitle = if keep_left { " KEEP " } else { " merge away " };
    let rtitle = if keep_right { " KEEP " } else { " merge away " };
    f.render_widget(
        Paragraph::new(left).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(ltitle)
                .border_style(style_for(keep_left)),
        ),
        cols[0],
    );
    f.render_widget(
        Paragraph::new(right).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(rtitle)
                .border_style(style_for(keep_right)),
        ),
        cols[1],
    );
}

fn draw_search(f: &mut Frame, app: &mut App, area: Rect) {
    if let Some(detail) = &app.search.detail {
        let mut text = detail.base.clone();
        if !detail.anns.is_empty() {
            let tags: Vec<&str> = detail
                .anns
                .iter()
                .filter(|a| a.kind == "tag")
                .map(|a| a.body.as_str())
                .collect();
            text.push_str("\n\n─ annotations ─\n");
            if !tags.is_empty() {
                text.push_str(&format!("tags: {}\n", tags.join(", ")));
            }
            for a in detail.anns.iter().filter(|a| a.kind == "note") {
                text.push_str(&format!(
                    "note [{}]: {}\n",
                    &a.created_at[..10.min(a.created_at.len())],
                    a.body
                ));
            }
        }
        let title = if detail.episode_id.is_some() {
            " episode — t tag · n note · m link entity · p tier · e edit · d delete · Esc "
        } else {
            " detail — Esc back "
        };
        if let Some((kind, buf)) = &detail.annotate {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Min(3), Constraint::Length(3)])
                .split(area);
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title(title)),
                rows[0],
            );
            f.render_widget(
                Paragraph::new(buf.display()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(format!(
                            " new {kind} — Enter save · →/Ctrl-F complete · Esc cancel "
                        ))
                        .border_style(Style::default().fg(Color::Cyan)),
                ),
                rows[1],
            );
        } else {
            f.render_widget(
                Paragraph::new(text)
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title(title)),
                area,
            );
        }
        return;
    }

    let rows = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Length(3), Constraint::Min(3)])
        .split(area);

    let input = Paragraph::new(app.search.input.display()).block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(
                " type to search · #tag filter · @source browse · Enter open · Ctrl-E semantic · Ctrl-P private:{} ",
                if app.search.show_private { "on" } else { "off" }
            )),
    );
    f.render_widget(input, rows[0]);

    let mut lines: Vec<ListItem> = Vec::new();
    if let Some(pack) = &app.search.pack {
        for amb in &pack.ambiguous {
            let cands: Vec<String> = amb
                .candidates
                .iter()
                .map(|c| format!("{} ({})", c.name, c.interaction_count))
                .collect();
            lines.push(ListItem::new(Line::from(Span::styled(
                format!("\"{}\" is ambiguous: {}", amb.matched, cands.join(" · ")),
                Style::default().fg(Color::Yellow),
            ))));
        }
        for item in &pack.items {
            let when = item
                .occurred_at
                .as_deref()
                .map(|s| s[..10.min(s.len())].to_string())
                .unwrap_or_else(|| "          ".into());
            let src = item.source.clone().unwrap_or_default();
            lines.push(ListItem::new(Line::from(vec![
                Span::styled(format!("{when} "), Style::default().fg(Color::DarkGray)),
                Span::raw(
                    item.text
                        .lines()
                        .next()
                        .unwrap_or("")
                        .chars()
                        .take(80)
                        .collect::<String>(),
                ),
                Span::styled(format!("  [{src}]"), Style::default().fg(Color::DarkGray)),
            ])));
        }
    }
    let list = List::new(lines)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(" results — Enter opens "),
        )
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    // Ambiguity rows offset the selection; account by selecting into items only
    // when there are no ambiguity rows (simplification: selection includes them).
    f.render_stateful_widget(list, rows[1], &mut app.search.list);
}

fn draw_capture(f: &mut Frame, app: &mut App, area: Rect) {
    match app.capture.kind {
        CaptureKind::Note => {
            let para = Paragraph::new(app.capture.note.display())
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(
                    " quick note — Enter saves as episode (entities auto-linked) · Ctrl-T switch to fact ",
                ));
            f.render_widget(para, area);
        }
        CaptureKind::Fact => {
            let mut lines: Vec<Line> = vec![Line::from(Span::styled(
                "↑/↓ move field · Enter save · Esc clear · Ctrl-T switch to note",
                Style::default().fg(Color::DarkGray),
            ))];
            let cap_ghost: Option<String> = {
                let (label, value) = &app.capture.fields[app.capture.idx];
                if is_entity_field(label) {
                    ghost_for(&app.conn, value.text()).map(|(_, d)| d)
                } else {
                    None
                }
            };
            for (i, (label, value)) in app.capture.fields.iter().enumerate() {
                let active = i == app.capture.idx;
                let mut spans = vec![
                    Span::styled(
                        format!("{label:>10}: "),
                        if active {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default().fg(Color::DarkGray)
                        },
                    ),
                    Span::raw(if active {
                        value.display()
                    } else {
                        value.text().to_string()
                    }),
                ];
                if active {
                    if let Some(g) = &cap_ghost {
                        spans.push(Span::styled(
                            g.clone(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ));
                    }
                }
                lines.push(Line::from(spans));
            }
            lines.push(Line::from(""));
            lines.push(Line::from(Span::styled(
                "subject resolves against the graph (new names become topics); \
                 object optional; tags e.g. \"recommendation\"",
                Style::default().fg(Color::DarkGray),
            )));
            let para = Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(" new fact "));
            f.render_widget(para, area);
        }
    }
}

fn draw_entity(f: &mut Frame, app: &mut App, area: Rect) {
    // An edit in flight takes the top three rows and the page keeps drawing
    // beneath it — you are renaming *this* node, and hiding the page you are
    // renaming behind a popup is how the wrong node gets renamed.
    let area = match &app.entity.edit {
        Some(edit) => {
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(3), Constraint::Min(1)])
                .split(area);
            let widget = Paragraph::new(edit.input.display()).block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_style(Style::default().fg(Color::Yellow))
                    .title(edit.kind.prompt()),
            );
            f.render_widget(widget, rows[0]);
            rows[1]
        }
        None => area,
    };
    match app.entity.mode {
        EntityMode::Input => {
            let sug_rows = (app.entity.suggestions.len() as u16).min(8);
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([
                    Constraint::Length(3),
                    Constraint::Length(if sug_rows > 0 { sug_rows + 2 } else { 0 }),
                    Constraint::Min(1),
                ])
                .split(area);
            f.render_widget(
                Paragraph::new(app.entity.input.display()).block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" entity lookup — type to suggest · ↑/↓ pick · Enter opens "),
                ),
                rows[0],
            );
            if sug_rows > 0 {
                let items: Vec<ListItem> = app
                    .entity
                    .suggestions
                    .iter()
                    .map(|sg| {
                        let mut spans = vec![
                            Span::raw(sg.node.name.clone()),
                            Span::styled(
                                format!("  {} ", sg.node.node_type),
                                Style::default().fg(Color::DarkGray),
                            ),
                        ];
                        if sg.via != "name" {
                            spans.push(Span::styled(
                                format!("via {}: {}", sg.via, sg.matched),
                                Style::default()
                                    .fg(Color::DarkGray)
                                    .add_modifier(Modifier::ITALIC),
                            ));
                        }
                        ListItem::new(Line::from(spans))
                    })
                    .collect();
                let list = List::new(items)
                    .block(Block::default().borders(Borders::ALL).title(" matches "))
                    .highlight_style(
                        Style::default()
                            .bg(Color::DarkGray)
                            .add_modifier(Modifier::BOLD),
                    );
                f.render_stateful_widget(list, rows[1], &mut app.entity.sug);
            }
            f.render_widget(
                Paragraph::new(
                    "Person or project page: current facts (s supersedes the stale one),\n\
                     interaction rollup, and recent episode timeline.\n\n\
                     Identity: r renames (the old name stays an alias), a adds an alias,\n\
                     n creates a person who has facts but no node. Ctrl-N does the same\n\
                     from the lookup box, prefilled with what you typed.\n\n\
                     Also reachable via Enter on a person/fact search result or a GTD task.",
                )
                .style(Style::default().fg(Color::DarkGray))
                .wrap(Wrap { trim: false }),
                rows[2],
            );
        }
        EntityMode::Pick => {
            let items: Vec<ListItem> = app
                .entity
                .candidates
                .iter()
                .map(|n| {
                    ListItem::new(Line::from(vec![
                        Span::raw(n.name.clone()),
                        Span::styled(
                            format!("  ({} · {})", n.node_type, n.id),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" multiple matches — j/k · Enter opens · Esc back "),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            f.render_stateful_widget(list, area, &mut app.entity.pick);
        }
        EntityMode::View => {
            let Some(node) = &app.entity.node else { return };

            let mut header = vec![Line::from(vec![
                Span::styled(
                    node.name.clone(),
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::styled(
                    format!("  {} · {}", node.node_type, node.id),
                    Style::default().fg(Color::DarkGray),
                ),
            ])];
            if !node.aliases.is_empty() {
                header.push(Line::from(Span::styled(
                    format!("aka: {}", node.aliases.join(", ")),
                    Style::default().fg(Color::DarkGray),
                )));
            }
            if let Some(pi) = &app.entity.interaction {
                header.push(Line::from(format!(
                    "{} interactions · last seen {} via {}",
                    pi.interaction_count,
                    pi.last_seen_at
                        .as_deref()
                        .map(|s| &s[..10.min(s.len())])
                        .unwrap_or("-"),
                    pi.last_channel.as_deref().unwrap_or("-"),
                )));
            }
            if let Some(summary) = &app.entity.summary {
                let mut s: String = summary.chars().take(220).collect();
                if s.len() < summary.len() {
                    s.push('…');
                }
                header.push(Line::from(Span::styled(
                    s,
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::ITALIC),
                )));
            }

            // Header grows with its content (aka / interactions / summary all
            // optional); the summary line may wrap onto a second row.
            let header_rows =
                2 + header.len() as u16 + if app.entity.summary.is_some() { 1 } else { 0 };
            let rows = Layout::default()
                .direction(Direction::Vertical)
                .constraints([Constraint::Length(header_rows), Constraint::Min(3)])
                .split(area);
            f.render_widget(
                Paragraph::new(header)
                    .wrap(Wrap { trim: false })
                    .block(Block::default().borders(Borders::ALL).title(" entity ")),
                rows[0],
            );

            let cols = Layout::default()
                .direction(Direction::Horizontal)
                .constraints([Constraint::Percentage(58), Constraint::Percentage(42)])
                .split(rows[1]);

            let here = &node.id;
            let items: Vec<ListItem> = app
                .entity
                .facts
                .iter()
                .map(|fact| {
                    let when = fact
                        .valid_from
                        .as_deref()
                        .map(|v| format!("{} ", &v[..10.min(v.len())]))
                        .unwrap_or_else(|| "           ".into());
                    let inbound = fact.subject_id != *here;
                    let arrow = if inbound { "← " } else { "" };
                    // A recorded denial reads as settled, not as a weak
                    // positive — red ✗ and dimmed text.
                    let negative = fact.polarity == "negative";
                    let (mark, body) = if negative {
                        ("✗ ", Style::default().fg(Color::DarkGray))
                    } else {
                        ("", Style::default())
                    };
                    // ◌: unreviewed (shadow) — opening the entity IS one of
                    // the review triggers, so the page must say which facts
                    // still need a verdict (y confirms, u refutes).
                    let unrev = if fact.is_shadow() { "◌ " } else { "" };
                    ListItem::new(Line::from(vec![
                        Span::styled(when, Style::default().fg(Color::DarkGray)),
                        Span::styled(arrow, Style::default().fg(Color::Yellow)),
                        Span::styled(mark, Style::default().fg(Color::Red)),
                        Span::styled(unrev, Style::default().fg(Color::Yellow)),
                        Span::styled(fact.statement.chars().take(70).collect::<String>(), body),
                    ]))
                })
                .collect();
            let focused = |on: bool| {
                if on {
                    Style::default().fg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                }
            };
            let title = format!(
                " facts ({}) — j/k · s supersede · Enter follow · h/l pane · / lookup ",
                app.entity.facts.len()
            );
            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(focused(!app.entity.timeline_focus))
                        .title(title),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            f.render_stateful_widget(list, cols[0], &mut app.entity.list);

            let tl_items: Vec<ListItem> = app
                .entity
                .episodes
                .iter()
                .map(|e| {
                    ListItem::new(Line::from(vec![
                        Span::styled(
                            format!("[{}] ", &e.occurred_at[..10.min(e.occurred_at.len())]),
                            Style::default().fg(Color::DarkGray),
                        ),
                        Span::raw(format!(
                            "{} · {}",
                            e.source,
                            e.body
                                .lines()
                                .next()
                                .unwrap_or("")
                                .chars()
                                .take(44)
                                .collect::<String>()
                        )),
                    ]))
                })
                .collect();
            let tl = List::new(tl_items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .border_style(focused(app.entity.timeline_focus))
                        .title(format!(
                            " timeline ({}) — Enter opens ",
                            app.entity.episodes.len()
                        )),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                );
            f.render_stateful_widget(tl, cols[1], &mut app.entity.timeline);
        }
    }
}

fn draw_gtd(f: &mut Frame, app: &mut App, area: Rect) {
    if let GtdMode::Form {
        fields,
        idx,
        editing,
    } = &app.gtd.mode
    {
        let title = match editing {
            None => " new task — ↑/↓ move field · Enter save · Esc cancel ",
            Some(_) => " edit schedule — ↑/↓ move field · Enter save · Esc cancel ",
        };
        let mut lines: Vec<Line> = Vec::new();
        let gtd_ghost: Option<String> = {
            let (label, value) = &fields[*idx];
            if is_entity_field(label) {
                ghost_for(&app.conn, value.text()).map(|(_, d)| d)
            } else {
                None
            }
        };
        for (i, (label, value)) in fields.iter().enumerate() {
            let active = i == *idx;
            let mut spans = vec![
                Span::styled(
                    format!("{label:>8}: "),
                    if active {
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ),
                Span::raw(if active {
                    value.display()
                } else {
                    value.text().to_string()
                }),
            ];
            if active {
                if let Some(g) = &gtd_ghost {
                    spans.push(Span::styled(
                        g.clone(),
                        Style::default()
                            .fg(Color::DarkGray)
                            .add_modifier(Modifier::ITALIC),
                    ));
                }
            }
            lines.push(Line::from(spans));
        }
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "dates: YYYY-MM-DD, today, tomorrow, +Nd (empty clears) · \
             project resolves against the graph",
            Style::default().fg(Color::DarkGray),
        )));
        f.render_widget(
            Paragraph::new(lines)
                .wrap(Wrap { trim: false })
                .block(Block::default().borders(Borders::ALL).title(title)),
            area,
        );
        return;
    }

    let today = mecha_graph_core::ids::now();
    let items: Vec<ListItem> = app
        .gtd
        .items
        .iter()
        .map(|t| {
            let status_style = match t.status.as_str() {
                "next" => Style::default().fg(Color::Green),
                "inbox" => Style::default().fg(Color::Yellow),
                "waiting" => Style::default().fg(Color::Magenta),
                "scheduled" => Style::default().fg(Color::Blue),
                _ => Style::default().fg(Color::DarkGray),
            };
            let mut spans = vec![
                Span::styled(format!("{:<9} ", t.status), status_style),
                Span::raw(t.name.chars().take(56).collect::<String>()),
            ];
            if let Some(due) = &t.due_at {
                let due_short = &due[..10.min(due.len())];
                let overdue = t.status != "done" && *due < today;
                spans.push(Span::styled(
                    format!("  due {due_short}"),
                    if overdue {
                        Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
                    } else {
                        Style::default().fg(Color::DarkGray)
                    },
                ));
            }
            if let Some(who) = &t.waiting_on {
                spans.push(Span::styled(
                    format!("  ⧗ {who}"),
                    Style::default().fg(Color::Magenta),
                ));
            }
            if let Some(project) = &t.project {
                spans.push(Span::styled(
                    format!("  [{project}]"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();
    let open = app
        .gtd
        .items
        .iter()
        .filter(|t| t.completed_at.is_none())
        .count();
    let title = format!(
        " tasks ({open} open{}) — a add · e schedule · n/i/w/s/d/x status · Space cycle · z closed · Enter page ",
        if app.gtd.show_closed {
            format!(", {} closed", app.gtd.items.len() - open)
        } else {
            String::new()
        }
    );
    let list = List::new(items)
        .block(Block::default().borders(Borders::ALL).title(title))
        .highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        );
    f.render_stateful_widget(list, area, &mut app.gtd.list);
}

fn draw_stats(f: &mut Frame, app: &mut App, area: Rect) {
    if app.stats_text.is_none() {
        let style = crate::render::Style { enabled: false };
        app.stats_text = stats::health(&app.conn)
            .map(|h| crate::render::render_stats(&h, &style))
            .ok();
    }
    let para = Paragraph::new(
        app.stats_text
            .clone()
            .unwrap_or_else(|| "unavailable".into()),
    )
    .wrap(Wrap { trim: false })
    .block(Block::default().borders(Borders::ALL).title(" health "));
    f.render_widget(para, area);
}
