//! `pkg` CLI.

mod render;
mod tui;

use clap::{Parser, Subcommand};
use mecha_graph_core::{db, embed, episode, eval, fact, graph, gtd, rollup, router, sources, stats};
use std::io::IsTerminal;
use std::path::PathBuf;

#[derive(Parser)]
#[command(name = "mecha-graph", about = "Personal knowledge graph", version)]
struct Cli {
    /// Database path (default: ~/.mecha-graph/graph.db, or $MECHA_GRAPH_DB)
    #[arg(long, global = true)]
    db: Option<PathBuf>,

    /// Force machine-readable JSON output (default when piped)
    #[arg(long, global = true)]
    json: bool,

    /// Force human-readable output (default on a terminal)
    // Explicit id: the default id "text" collides with `Note { text }`'s
    // positional once clap propagates this global into subcommands — clap
    // panics at match time ("could not downcast to bool"), killing `mecha-graph note`.
    #[arg(long = "text", id = "text_output", global = true)]
    text: bool,

    #[command(subcommand)]
    command: Command,
}

/// Human on a TTY, JSON when piped/captured — overridable either way.
fn want_json(cli_json: bool, cli_text: bool) -> bool {
    if cli_json {
        true
    } else if cli_text {
        false
    } else {
        !std::io::stdout().is_terminal()
    }
}

fn style() -> render::Style {
    render::Style {
        enabled: std::io::stdout().is_terminal(),
    }
}

/// Resolve accept/reject targets: explicit ids verbatim, else pending
/// candidates matching the filters (capped at `limit`). `--dry-run` prints
/// the matches and resolves to nothing.
#[allow(clippy::too_many_arguments)]
fn resolve_triage_ids(
    conn: &mecha_graph_core::rusqlite::Connection,
    ids: Vec<i64>,
    proposer: &Option<String>,
    predicate: &Option<String>,
    contains: &Option<String>,
    min_confidence: Option<f64>,
    max_confidence: Option<f64>,
    limit: usize,
    dry_run: bool,
) -> mecha_graph_core::Result<Vec<i64>> {
    if !ids.is_empty() {
        return Ok(ids);
    }
    if proposer.is_none()
        && predicate.is_none()
        && contains.is_none()
        && min_confidence.is_none()
        && max_confidence.is_none()
    {
        return Err(mecha_graph_core::Error::Other(
            "give candidate ids, or at least one bulk filter \
             (--proposer / --predicate / --contains / --min-confidence / --max-confidence)"
                .into(),
        ));
    }

    let needle = contains.as_deref().map(str::to_lowercase);
    let matches: Vec<_> = fact::pending_candidates(conn, 10_000)?
        .into_iter()
        .filter(|c| {
            if let Some(p) = proposer {
                if !c.proposed_by.as_deref().unwrap_or("").contains(p.as_str()) {
                    return false;
                }
            }
            if let Some(pred) = predicate {
                // Exact match on the normalized predicate — cluster verdicts
                // must hit exactly the cluster shown, nothing adjacent.
                if c.payload.get("predicate").and_then(|v| v.as_str()) != Some(pred.as_str()) {
                    return false;
                }
            }
            if let Some(n) = &needle {
                let hay = ["statement", "subject", "object", "predicate", "what"]
                    .iter()
                    .filter_map(|k| c.payload.get(k).and_then(|v| v.as_str()))
                    .collect::<Vec<_>>()
                    .join(" ")
                    .to_lowercase();
                if !hay.contains(n.as_str()) {
                    return false;
                }
            }
            // Missing confidence never matches a bound — bulk ops stay conservative.
            if let Some(min) = min_confidence {
                if c.confidence.unwrap_or(0.0) < min {
                    return false;
                }
            }
            if let Some(max) = max_confidence {
                if c.confidence.unwrap_or(f64::MAX) > max {
                    return false;
                }
            }
            true
        })
        .take(limit)
        .collect();

    if dry_run {
        for c in &matches {
            let statement = c
                .payload
                .get("statement")
                .and_then(|s| s.as_str())
                .or_else(|| c.payload.get("what").and_then(|s| s.as_str()))
                .unwrap_or("(no statement)");
            println!(
                "would match #{} [{} · {:.2}] {}",
                c.id,
                c.proposed_by.as_deref().unwrap_or("?"),
                c.confidence.unwrap_or(0.0),
                statement
            );
        }
        println!(
            "{} candidates match (dry run — nothing changed)",
            matches.len()
        );
        return Ok(vec![]);
    }
    Ok(matches.into_iter().map(|c| c.id).collect())
}

#[derive(Subcommand)]
enum Command {
    /// Initialize the database
    Init,
    /// Ingest a source
    Ingest {
        #[command(subcommand)]
        source: IngestSource,
    },
    /// Embed pending episodes and facts via the local embedding server (:8081)
    Embed {
        #[arg(long, default_value_t = 100000)]
        limit: usize,
        #[arg(long, default_value_t = 16)]
        batch: usize,
    },
    /// Query the graph — returns a context pack (JSON). `#tag` tokens filter
    /// to episodes carrying all those tags; a tag alone lists them newest-first.
    Query {
        query: String,
        #[arg(short, long, default_value_t = 10)]
        k: usize,
        #[arg(long, default_value_t = 4000)]
        budget: usize,
        /// Include private-tier episodes
        #[arg(long)]
        private: bool,
        /// Bi-temporal: facts as of this date (YYYY-MM-DD)
        #[arg(long)]
        as_of: Option<String>,
        /// Retrieval scope: both | facts | evidence (gossip blind halves)
        #[arg(long, default_value = "both")]
        scope: String,
        /// Restrict to these episode sources (repeatable). Two agents
        /// given different sources are independent witnesses, which
        /// facts-vs-evidence never were. Unknown names error.
        #[arg(long = "source")]
        sources: Vec<String>,
        /// Only evidence from on/after this date (YYYY-MM-DD). Give both
        /// readers the same window or a comparison spans eras.
        #[arg(long)]
        since: Option<String>,
        /// Only evidence from before this date (YYYY-MM-DD).
        #[arg(long)]
        until: Option<String>,
    },
    /// Show everything about an entity
    Entity {
        name: String,
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Quick note capture
    Note { text: String },
    /// Re-run cheap linkers + rollups over existing episodes
    Link {
        #[arg(long)]
        auto: bool,
    },
    /// Health stats
    Stats,
    /// Review pending fact candidates
    Review {
        #[arg(long, default_value_t = 10)]
        top: i64,
        /// Cluster view: group the queue by (proposer, predicate) with
        /// verdict history and samples — one decision per class, not per fact
        #[arg(long)]
        clusters: bool,
        /// Samples shown per cluster (spread, not top-confidence)
        #[arg(long, default_value_t = 3)]
        samples: usize,
    },
    /// Accept fact candidates by id, or in bulk by filter
    Accept {
        ids: Vec<i64>,
        /// Bulk: substring match on proposed_by (e.g. llm, linker:knn)
        #[arg(long)]
        proposer: Option<String>,
        /// Bulk: exact match on the payload predicate (cluster verdicts)
        #[arg(long)]
        predicate: Option<String>,
        /// Bulk: case-insensitive substring on statement/subject/object
        #[arg(long)]
        contains: Option<String>,
        /// Bulk: only candidates at or above this confidence
        #[arg(long)]
        min_confidence: Option<f64>,
        /// Bulk: cap how many candidates the filters may match
        #[arg(long, default_value_t = 500)]
        limit: usize,
        /// List what would be accepted without accepting
        #[arg(long)]
        dry_run: bool,
    },
    /// Reject fact candidates by id, or in bulk by filter
    Reject {
        ids: Vec<i64>,
        #[arg(long, default_value = "manual rejection")]
        reason: String,
        /// Bulk: substring match on proposed_by (e.g. llm, linker:knn)
        #[arg(long)]
        proposer: Option<String>,
        /// Bulk: exact match on the payload predicate (cluster verdicts)
        #[arg(long)]
        predicate: Option<String>,
        /// Bulk: case-insensitive substring on statement/subject/object
        #[arg(long)]
        contains: Option<String>,
        /// Bulk: only candidates at or below this confidence
        #[arg(long)]
        max_confidence: Option<f64>,
        /// Bulk: cap how many candidates the filters may match
        #[arg(long, default_value_t = 500)]
        limit: usize,
        /// List what would be rejected without rejecting
        #[arg(long)]
        dry_run: bool,
    },
    /// True-delete an episode and everything derived from it
    Redact {
        /// Episode uid
        episode: String,
    },
    /// Run the gold-set eval
    Eval {
        #[arg(long, default_value = "eval/gold.jsonl")]
        gold: PathBuf,
    },
    /// Tier-7 LLM extraction over pending episodes → fact candidates
    Extract {
        #[arg(long, default_value_t = 25)]
        limit: usize,
        #[arg(long, default_value = mecha_graph_core::llm::DEFAULT_MODEL)]
        model: String,
        /// Restrict to sources, e.g. bee.conversation
        #[arg(long)]
        source: Vec<String>,
        /// Re-extract ONE episode (uid or id) regardless of prompt-version
        /// state — for a fixed prompt, a corrected episode, or an
        /// evidence-only gap. Ignores --limit/--source.
        #[arg(long)]
        episode: Option<String>,
    },
    /// Undo the most recent TUI episode delete/edit (also Ctrl-Z in the TUI)
    Undo,
    /// Deletion tombstones — what re-ingest is blocked from resurrecting
    Tombstone {
        #[command(subcommand)]
        action: TombstoneAction,
    },
    /// Promote structured Reflect notes (Type: #person/#company/#book …)
    /// to entities: identifiers, works_at/authored/role facts, mentions
    ReflectProcess,
    /// Two-way Bee facts sync: pull unconfirmed suggestions into the review
    /// queue; push accept→confirm / reject→delete verdicts back to Bee
    BeeFacts {
        /// Max suggestions pulled per run (backlog drains across runs)
        #[arg(long, default_value_t = 100)]
        pull_limit: usize,
    },
    /// Auto-triage the review queue: drop duplicates (vs the graph and
    /// within the queue), flag contradictions, optionally accept the rest
    Precheck {
        /// Also accept clean novel candidates (resolvable subject, no
        /// conflict). Conflicts and unknown subjects always stay for review.
        #[arg(long)]
        auto_accept: bool,
        /// Skip the embedding tier (deterministic dedup only)
        #[arg(long)]
        no_semantic: bool,
        /// Count outcomes without changing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// Promote a human alias to the display name for person nodes named
    /// by an email address. The address keeps resolving (identifier +
    /// alias); only what renders changes.
    FixPersonNames {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// One-shot repair: retract co-occurrence beliefs with ZERO
    /// remaining support — phantoms from pre-unique-alias over-linking.
    /// These were never true, so they take system-time invalidation
    /// rather than a valid-time close.
    InvalidatePhantoms {
        /// Report without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Decay sweep: re-derive every co-occurrence belief, close
    /// the ones whose statistic has collapsed (valid time only — decay is
    /// not error), refresh drifted numbers, alarm on input-set collapse.
    Decay {
        /// Report without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// Retrofit provenance onto co-occurrence facts written before
    /// derived-fact provenance existed: sensitivity MAX over the full
    /// contributing set, clock anchored to the newest contributor.
    BackfillDerivation {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// The Verifier's deterministic tier: dereference a claim's
    /// provenance and report what the rows actually say. A lexical miss
    /// is `residue` (hand to a model), never a refutation.
    Verify {
        /// Entity name/alias/id — verifies every live claim about it
        #[arg(long, conflicts_with = "fact")]
        node: Option<String>,
        /// A single fact uid
        #[arg(long)]
        fact: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// The gossip Selector (SQL-only): rank probe targets by
    /// demand × slot-gaps × λ-staleness. Read-only; feeds the gossip
    /// harness and fork experiments.
    ProbeTargets {
        #[arg(long, default_value_t = 10)]
        limit: usize,
        /// EXPERIMENT ONLY: also rank nodes with evidence but no demand,
        /// using mention count in place of touches. Production probing is
        /// demand-gated on purpose (accuracy is non-uniform by design);
        /// this exists so a measurement run has enough sample to be
        /// worth reading. Do not put it in the nightly.
        #[arg(long)]
        include_cold: bool,
    },
    /// Process unhandled `meta.corrections` arrays from agent episodes
    /// (D3): supersede the wrong fact, stage the replacement, demote the
    /// producing class, emit the blast-radius sweep. kg_upsert processes
    /// inline; this is the backfill/retry drain (nightly runs it).
    Corrections {
        #[arg(long, default_value_t = 100)]
        limit: i64,
    },
    /// Recompute every fact's confidence from its observation trail
    /// (Beta posterior, PLAN.md Option D) — one-shot maintenance after
    /// the V011 switch away from the MAX ratchet
    RecomputeConfidence {
        /// Count and show the shift without writing anything
        #[arg(long)]
        dry_run: bool,
    },
    /// List near-duplicate live facts (same subject, similar statements)
    DedupeFacts {
        // Reads the constant rather than repeating it. It was a literal 0.93
        // until 2026-08-20, which meant the recalibration for a new embedding
        // model moved precheck's threshold and left this one behind — two
        // copies of one fact, and only one of them corrected.
        #[arg(long, default_value_t = mecha_graph_core::precheck::SEMANTIC_DUP_THRESHOLD)]
        threshold: f64,
        /// Only exact matches after case/punctuation normalization — the
        /// tier that is safe to --apply
        #[arg(long)]
        exact: bool,
        /// Supersede the duplicate side of every pair listed
        #[arg(long)]
        apply: bool,
    },
    /// Refresh generated scope summaries (node_context.summary)
    Summarize {
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long, default_value = mecha_graph_core::llm::DEFAULT_MODEL)]
        model: String,
        /// Refresh one specific node (by id) regardless of staleness
        #[arg(long)]
        node: Option<String>,
    },
    /// Generate the boot-injection memory file (~500 tokens)
    MemoryMd {
        /// Write here instead of stdout (e.g. ~/.hermes/MEMORY.md target)
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value_t = 500)]
        budget: usize,
    },
    /// Weekly review: stalled projects, waiting-on, inbox, orphan goals
    Gtd,
    /// Manage integrations (~/.mecha-graph/config.toml)
    Source {
        #[command(subcommand)]
        action: SourceAction,
    },
    /// Encrypt the database in place with SQLCipher; key → db.key
    Encrypt {
        /// Also shred-delete the plaintext backup after verification
        #[arg(long)]
        purge_backup: bool,
    },
    /// Write a plaintext snapshot (for DuckDB analytics on an encrypted DB)
    Decrypt {
        #[arg(long)]
        out: PathBuf,
    },
    /// Fork the DB: full encrypted copy under a FRESH key — the probing/eval
    /// test bed. Must land in its own directory (db.key is directory-scoped)
    Fork {
        /// Destination, e.g. ~/.mecha-graph/forks/probe.db
        #[arg(long)]
        out: PathBuf,
    },
    /// Show the archived raw content of an episode (capture retention)
    Raw {
        /// Episode uid
        episode: String,
    },
    /// Interactive TUI: review triage, merge review, search REPL, health
    Tui,
    /// List live facts by tag (e.g. --tag recommendation)
    Facts {
        #[arg(long)]
        tag: String,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// Tag or note an episode (also available in the TUI search detail)
    Annotate {
        /// Episode uid
        episode: String,
        #[arg(long)]
        tag: Vec<String>,
        #[arg(long)]
        note: Vec<String>,
    },
    /// List the tag vocabulary (every tag in use, with episode counts)
    Tags,
    /// List episodes by annotation tag (e.g. --tag recommendation)
    Episodes {
        #[arg(long)]
        tag: String,
        #[arg(long, default_value_t = 50)]
        limit: i64,
    },
    /// List tasks (GTD board); interactive status cycling lives in `mecha-graph tui`
    Tasks {
        /// Include done/dropped
        #[arg(long)]
        all: bool,
    },
    /// List duplicate-person merge candidates (same full name)
    Dups,
    /// Show or set the graph's owner — the person whose life this is
    Owner {
        /// Name, alias, or email to mark as owner; omit to show the current one
        name: Option<String>,
    },
    /// Merge a duplicate node into the one to keep (no undo — be sure)
    Merge {
        /// Node id to keep
        keep: String,
        /// Node id to merge in and delete
        dup: String,
    },
}

#[derive(Subcommand)]
enum TombstoneAction {
    /// List tombstones (newest first)
    List,
    /// Lift a tombstone so the next sync may re-import the item
    Rm {
        /// Source as stored on the episode, e.g. reflect.note, calendar.event
        source: String,
        /// The source's item id (shown by `mecha-graph tombstone list`)
        source_id: String,
    },
}

#[derive(Subcommand)]
enum SourceAction {
    /// List configured sources with auth + sync status
    List,
    /// Add (or reconfigure) a source
    Add {
        /// Kind: bee | ics | sessions | slack | imessage | mbox
        kind: String,
        /// Name (defaults to the kind)
        #[arg(long)]
        name: Option<String>,
        /// ics: secret iCal URL (treated as a credential)
        #[arg(long)]
        url: Option<String>,
        /// ics/imessage/mbox: local file path
        #[arg(long)]
        path: Option<PathBuf>,
        /// imessage: path to a synced copy of chat.db
        #[arg(long)]
        db: Option<PathBuf>,
        /// slack: user token (xoxp-…) or bot token (xoxb-…)
        #[arg(long)]
        token: Option<String>,
        /// Your own email(s), comma-separated (ics/mbox self-exclusion)
        #[arg(long = "me")]
        self_email: Option<String>,
        /// imessage: your own handles (phones/emails), comma-separated
        #[arg(long)]
        self_handles: Option<String>,
        /// Retention: keep | capture | capture_delete (raw → encrypted DB,
        /// then optionally delete the plaintext source file)
        #[arg(long)]
        retention: Option<String>,
        /// bee: mode=stream pulls from the API directly — no plaintext mirror
        #[arg(long)]
        mode: Option<String>,
        /// Skip the connectivity test before saving
        #[arg(long)]
        no_test: bool,
    },
    /// Test auth/connectivity (no writes)
    Test {
        name: Option<String>,
    },
    /// Ingest from all enabled sources (or one)
    Sync {
        name: Option<String>,
        /// Ignore stored cursors
        #[arg(long)]
        full: bool,
    },
    Enable {
        name: String,
    },
    Disable {
        name: String,
    },
    Remove {
        name: String,
    },
}

#[derive(Subcommand)]
enum IngestSource {
    /// Bee conversations + daily summaries from ~/bee-sync
    Bee {
        #[arg(long)]
        root: Option<PathBuf>,
        /// Re-scan everything (ignore cursor)
        #[arg(long)]
        full: bool,
    },
    /// Calendar from .ics file(s) — the identity bridge
    Ics {
        paths: Vec<PathBuf>,
        /// Your own email(s), excluded from person creation
        #[arg(long = "me")]
        self_emails: Vec<String>,
        #[arg(long)]
        full: bool,
    },
    /// Reflect app markdown-export zip (read in place, capture_delete)
    Reflect {
        /// Path to the export zip
        zip: PathBuf,
    },
    /// Agent sessions: Hermes (~/.hermes/state.db) + Claude Code
    /// (~/.claude/projects). Deterministic cwd→project linking, zero AI.
    Sessions {
        #[arg(long)]
        hermes: Option<PathBuf>,
        #[arg(long)]
        claude: Option<PathBuf>,
        #[arg(long)]
        full: bool,
    },
}

fn main() {
    let cli = Cli::parse();
    if let Err(e) = run(cli) {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}

fn run(cli: Cli) -> mecha_graph_core::Result<()> {
    let cli_json = cli.json;
    let cli_text = cli.text;
    let db_path = cli.db.unwrap_or_else(db::default_db_path);

    // Encrypt/Decrypt manage the file itself — don't hold it open.
    match &cli.command {
        Command::Encrypt { purge_backup } => {
            let bak = db::encrypt_in_place(&db_path)?;
            println!(
                "encrypted {} (key: {})",
                db_path.display(),
                db::keyfile_path(&db_path).display()
            );
            if *purge_backup {
                std::fs::remove_file(&bak)?;
                println!("plaintext backup removed");
            } else {
                println!(
                    "plaintext backup kept at {} — remove it once you've verified",
                    bak.display()
                );
            }
            println!("note: restart anything holding the old DB open (mecha-graph-mcp, etc.)");
            return Ok(());
        }
        Command::Decrypt { out } => {
            db::export_plaintext(&db_path, out)?;
            println!(
                "plaintext snapshot: {} (chmod 600; treat as ephemeral)",
                out.display()
            );
            return Ok(());
        }
        Command::Fork { out } => {
            let key = db::fork_db(&db_path, out)?;
            println!("fork:  {}", out.display());
            println!("key:   {}", key.display());
            println!("use:   MECHA_GRAPH_DB={} mecha-graph …   (or --db)", out.display());
            println!("note:  a fork is a full second copy of your life — deleting it");
            println!("       (db + key) when the experiment ends is a deliberate step.");
            return Ok(());
        }
        _ => {}
    }

    let conn = db::open(&db_path)?;

    match cli.command {
        Command::Encrypt { .. } | Command::Decrypt { .. } | Command::Fork { .. } => unreachable!(),
        Command::Init => {
            println!("initialized {}", db_path.display());
        }

        Command::Ingest { source } => match source {
            IngestSource::Bee { root, full } => {
                let root = root.unwrap_or_else(sources::bee::BeeSource::default_root);
                let src = sources::bee::BeeSource::new(root.clone());
                let since = if full { None } else { sources::get_cursor(&conn, "bee")? };
                let report = sources::ingest(&conn, &src, since.as_deref())?;
                let enriched = sources::bee::enrich_from_native(&conn, &root)?;
                println!(
                    "bee: +{} inserted, {} updated, {} unchanged{}, {} mentions ({} via alias), {} enriched from native fields",
                    report.inserted, report.updated, report.unchanged, report.tombstone_note(),
                    report.mentions + report.alias_mentions, report.alias_mentions, enriched
                );
            }
            IngestSource::Ics { paths, self_emails, full } => {
                if paths.is_empty() {
                    return Err(mecha_graph_core::Error::Other(
                        "provide at least one .ics path".into(),
                    ));
                }
                let src = sources::ics::IcsSource::new(paths, self_emails);
                let since = if full { None } else { sources::get_cursor(&conn, "calendar")? };
                let report = sources::ics::ingest_ics(&conn, &src, since.as_deref())?;
                println!(
                    "calendar: +{} inserted, {} updated, {} unchanged{}, {} attendee links",
                    report.inserted, report.updated, report.unchanged,
                    report.tombstone_note(), report.mentions
                );
            }
            IngestSource::Reflect { zip } => {
                let r = mecha_graph_core::sources::reflect::ingest_zip(&conn, &zip)?;
                println!(
                    "reflect: {} inserted · {} updated · {} unchanged{} · {} backlink + {} alias mentions · {}",
                    r.inserted, r.updated, r.unchanged, r.tombstone_note(), r.mentions, r.alias_mentions,
                    if r.deleted_files > 0 { "zip archived+deleted" } else { "zip KEPT (verify failed)" }
                );
            }
            IngestSource::Sessions { hermes, claude, full } => {
                let hermes_src = sources::sessions::HermesSource::new(
                    hermes.unwrap_or_else(sources::sessions::HermesSource::default_path),
                );
                let since = if full { None } else { sources::get_cursor(&conn, "session.hermes")? };
                let r1 = sources::ingest(&conn, &hermes_src, since.as_deref())?;
                println!(
                    "hermes sessions: +{} inserted, {} unchanged, {} project links",
                    r1.inserted, r1.unchanged, r1.mentions
                );

                let claude_src = sources::sessions::ClaudeSource::new(
                    claude.unwrap_or_else(sources::sessions::ClaudeSource::default_path),
                );
                let since = if full { None } else { sources::get_cursor(&conn, "session.claude")? };
                let r2 = sources::ingest(&conn, &claude_src, since.as_deref())?;
                println!(
                    "claude sessions: +{} inserted, {} unchanged, {} project links",
                    r2.inserted, r2.unchanged, r2.mentions
                );
            }
        },

        Command::Embed { limit, batch } => {
            let embedder = embed::Embedder::default();
            if !embedder.available() {
                return Err(mecha_graph_core::Error::Embed(format!(
                    "no embedding server at {} — start one with `llama-server -m <gguf> \
                     --port 8081 --embeddings --pooling last --embd-normalize 2`",
                    embedder.base_url
                )));
            }
            // A width change means every stored vector is unusable, so the
            // tables are rebuilt and the whole corpus re-embedded. Say so
            // loudly: this is the one command that can discard an index, and
            // an "embedded 0 episodes" afterwards would look routine.
            if embed::ensure_vec_dims(&conn, embedder.dims)? {
                eprintln!(
                    "mecha-graph: vector tables rebuilt at {} dims — every vector must be \
                     re-embedded, and precheck's thresholds are calibrated to the OLD model. \
                     Do not run `precheck --auto-accept` until they are re-derived.",
                    embedder.dims
                );
            }
            let n_ep = embed::embed_pending_episodes(&conn, &embedder, limit, batch)?;
            let n_f = embed::embed_pending_facts(&conn, &embedder, limit, batch)?;
            embed::set_embed_meta(
                &conn,
                &embedder.model,
                embedder.dims,
                embed::EmbedTask::Document.tag(),
            )?;
            println!("embedded {n_ep} episodes, {n_f} facts");
        }

        Command::Query { query, k, budget, private, as_of, scope, sources, since, until } => {
            let scope = router::Scope::parse(&scope).ok_or_else(|| {
                mecha_graph_core::Error::Other(format!(
                    "bad --scope '{scope}' (both | facts | evidence)"
                ))
            })?;
            // Validate against reality: a mistyped source silently
            // returning nothing is worse than an error, because empty
            // reads as "this source knows nothing about them".
            if !sources.is_empty() {
                let known = mecha_graph_core::search::known_sources(&conn)?;
                for s in &sources {
                    if !known.contains(s) {
                        return Err(mecha_graph_core::Error::Other(format!(
                            "unknown --source '{s}'; known: {}",
                            known.join(", ")
                        )));
                    }
                }
            }
            let window = (since.is_some() || until.is_some()).then(|| router::TimeRange {
                from: since.map(|d| format!("{d} 00:00:00")),
                to: until.map(|d| format!("{d} 00:00:00")),
            });
            let lens = router::Lens { scope, sources, window };
            if let Some(as_of) = &as_of {
                // as-of queries are entity-scoped: resolve the entity, then facts_as_of.
                let (entities, _) = router::detect_entities(&conn, &query)?;
                for e in &entities {
                    let facts = fact::facts_as_of(&conn, &e.node_id, as_of, 25)?;
                    println!("# {} as of {as_of}", e.name);
                    for f in facts {
                        println!("  - {}", f.statement);
                    }
                }
                if entities.is_empty() {
                    eprintln!("--as-of needs a resolvable entity in the query");
                }
                return Ok(());
            }
            let embedder = embed::Embedder::default();
            let emb = embedder.available().then_some(&embedder);
            let pack = router::query_lens(&conn, emb, &query, k, budget, private, Some("cli.query"), lens)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&pack)?);
            } else {
                print!("{}", render::render_pack(&pack, &style()));
            }
        }

        Command::Entity { name, as_of } => {
            let matches = graph::resolve_entity_all(&conn, &name)?;
            if matches.is_empty() {
                println!("no entity matching '{name}'");
                return Ok(());
            }
            for node in matches {
                println!("# {} ({}, id={})", node.name, node.node_type, node.id);
                if !node.aliases.is_empty() {
                    println!("aliases: {}", node.aliases.join(", "));
                }
                if let Some(pi) = rollup::get_person_interaction(&conn, &node.id)? {
                    println!(
                        "interactions: {} · last seen {} via {} · last meeting {} · last spoken {}",
                        pi.interaction_count,
                        pi.last_seen_at.as_deref().unwrap_or("-"),
                        pi.last_channel.as_deref().unwrap_or("-"),
                        pi.last_meeting_at.as_deref().unwrap_or("-"),
                        pi.last_spoken_at.as_deref().unwrap_or("-"),
                    );
                }
                let facts = match &as_of {
                    Some(d) => fact::facts_as_of(&conn, &node.id, d, 20)?,
                    None => fact::facts_for_node(&conn, &node.id, 20)?,
                };
                if !facts.is_empty() {
                    println!("facts:");
                    for f in facts {
                        let dated = match &f.valid_from {
                            Some(v) => format!(" (as of {v})"),
                            None => String::new(),
                        };
                        let neg = if f.polarity == "negative" { "✗ " } else { "" };
                        println!("  - {neg}{}{dated} [{} x{}]", f.statement,
                            f.extractor.as_deref().unwrap_or("?"), f.observation_count);
                    }
                }
                let eps = episode::episodes_for_node(&conn, &node.id, 5)?;
                if !eps.is_empty() {
                    println!("recent episodes:");
                    for e in eps {
                        let first = e.body.lines().next().unwrap_or("");
                        println!("  - [{}] {} — {}", e.occurred_at, e.source,
                            first.chars().take(80).collect::<String>());
                    }
                }
                println!();
            }
        }

        Command::Note { text } => {
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
            let (id, _) = episode::upsert_episode(&conn, &ep)?;
            let n = episode::link_by_alias_scan(&conn, id, &text)?;
            println!("noted (episode {id}, {n} entities linked)");
        }

        Command::Link { auto: _ } => {
            // Remediation first: alias mentions created before the
            // unique-alias rule may credit multiple same-named people.
            // Drop and rebuild them; deterministic tiers re-derive the rest.
            conn.execute("DELETE FROM mention WHERE extractor = 'alias'", [])?;
            // Re-run alias-scan over all episodes (cheap tier)...
            let mut stmt = conn.prepare("SELECT id, body FROM episode")?;
            let rows: Vec<(i64, String)> = stmt
                .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                .collect::<std::result::Result<_, _>>()?;
            drop(stmt);
            let mut linked = 0;
            for (id, body) in rows {
                linked += episode::link_by_alias_scan(&conn, id, &body)?;
            }
            // ...then the linker cascade (temporal → NPMI → kNN → structural), then rollups.
            let cascade = mecha_graph_core::linkers::run_cascade(&conn)?;
            let people = rollup::rebuild_person_interactions(&conn)?;
            println!(
                "alias-scan: {linked} mentions · temporal: {} attributed · npmi: {} facts · knn: {} staged · structural: {} staged · rules: {} staged · rollup: {people} people",
                cascade.temporal_mentions, cascade.npmi_facts, cascade.knn_candidates,
                cascade.structural_candidates, cascade.rule_candidates
            );
            // Say WHY the temporal join attributed what it did. A small
            // number means either "the wearable was off" or "the join threw
            // the recording away", and those have opposite remedies.
            let t = &cascade.temporal;
            println!(
                "  temporal detail: bee {} usable / {} missing end · overlaps {} → \
                 below-{:.0}%-coverage {} · no-attendees {} · attributed {}",
                t.bee_with_end, t.bee_without_end, t.overlaps,
                mecha_graph_core::linkers::TEMPORAL_MIN_COVERAGE * 100.0,
                t.below_coverage, t.no_attendees, t.attributed_pairs
            );
            println!(
                "    title tier: {} attribution(s) over {} pair(s) that had no invite list",
                t.title_attributions, t.title_only_pairs
            );
            for sample in &t.no_attendee_samples {
                println!("    no-attendee event: {sample}");
            }
        }

        Command::Stats => {
            let h = stats::health(&conn)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&h)?);
            } else {
                print!("{}", render::render_stats(&h, &style()));
            }
        }

        Command::Review { top, clusters, samples } => {
            if clusters {
                let rows = mecha_graph_core::precheck::review_clusters(&conn, samples)?;
                if want_json(cli_json, cli_text) {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                    return Ok(());
                }
                let total: usize = rows.iter().map(|c| c.pending).sum();
                println!("{} pending in {} clusters\n", total, rows.len());
                for c in &rows {
                    let hist = if c.accepted_hist + c.rejected_hist > 0 {
                        format!(
                            "  history {}✓/{}✗ ({:.0}% accepted)",
                            c.accepted_hist,
                            c.rejected_hist,
                            100.0 * c.accepted_hist as f64
                                / (c.accepted_hist + c.rejected_hist) as f64
                        )
                    } else {
                        "  history none".into()
                    };
                    let rung = if c.rung == "staged" {
                        String::new()
                    } else {
                        format!("  [{} · streak {}/20]", c.rung, c.streak)
                    };
                    println!(
                        "{:5}  {} · {}  conf {:.2}-{:.2}{}{}",
                        c.pending, c.proposed_by, c.predicate, c.conf_min, c.conf_max, hist, rung
                    );
                    for s in &c.samples {
                        let s: String = s.chars().take(96).collect();
                        println!("         · {s}");
                    }
                    if c.commitment {
                        println!("         → commitments materialize tasks; review individually (mecha-graph tui)");
                    } else {
                        println!(
                            "         → mecha-graph accept|reject --proposer '{}' --predicate '{}'",
                            c.proposed_by,
                            c.predicate.trim_matches(|ch| ch == '(' || ch == ')')
                        );
                    }
                    println!();
                }
                return Ok(());
            }
            let pending = fact::pending_candidates(&conn, top)?;
            if pending.is_empty() {
                println!("no pending candidates");
            }
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&pending)?);
            } else {
                let st = style();
                for c in &pending {
                    let statement = c
                        .payload
                        .get("statement")
                        .and_then(|s| s.as_str())
                        .or_else(|| c.payload.get("what").and_then(|s| s.as_str()))
                        .unwrap_or("(no statement)");
                    let kind = if c.payload.get("kind").and_then(|k| k.as_str()) == Some("commitment") {
                        " [commitment]"
                    } else {
                        ""
                    };
                    let pred = c
                        .payload
                        .get("predicate")
                        .and_then(|p| p.as_str())
                        .unwrap_or("-");
                    println!(
                        "{:>5}  {}{}\n       {}",
                        st.bold(&format!("#{}", c.id)),
                        statement,
                        st.accent(kind),
                        st.dim(&format!(
                            "{} · conf {:.2} · by {}",
                            pred,
                            c.confidence.unwrap_or(0.0),
                            c.proposed_by.as_deref().unwrap_or("?")
                        ))
                    );
                }
                println!("\naccept: mecha-graph accept <id...>   reject: mecha-graph reject <id...> --reason \"…\"");
            }
        }

        Command::Accept { ids, proposer, predicate, contains, min_confidence, limit, dry_run } => {
            let ids = resolve_triage_ids(
                &conn, ids, &proposer, &predicate, &contains, min_confidence, None, limit, dry_run,
            )?;
            for id in ids {
                // Commitment candidates materialize a Task; plain ones a fact.
                match mecha_graph_core::extract::accept_commitment(&conn, id) {
                    Ok(task_id) => println!("#{id} accepted → task {task_id}"),
                    Err(_) => match fact::accept_candidate(&conn, id) {
                        Ok(uid) => println!("#{id} accepted → fact {uid}"),
                        Err(e) => println!("#{id} FAILED: {e}"),
                    },
                }
            }
        }

        Command::Reject { ids, reason, proposer, predicate, contains, max_confidence, limit, dry_run } => {
            let ids = resolve_triage_ids(
                &conn, ids, &proposer, &predicate, &contains, None, max_confidence, limit, dry_run,
            )?;
            for id in ids {
                match fact::reject_candidate(&conn, id, &reason) {
                    Ok(()) => println!("#{id} rejected"),
                    Err(e) => println!("#{id} FAILED: {e}"),
                }
            }
        }

        Command::Raw { episode: uid } => {
            match episode::get_episode_by_uid(&conn, &uid)? {
                Some(ep) => match episode::get_raw(&conn, ep.id)? {
                    Some(raw) => print!("{raw}"),
                    None => println!(
                        "no raw archived for {uid} (retention 'keep', or source file still holds it: {})",
                        ep.source_ref.as_deref().unwrap_or("-")
                    ),
                },
                None => println!("no episode with uid {uid}"),
            }
        }

        Command::Tui => {
            tui::run(conn)?;
        }

        Command::Facts { tag, limit } => {
            let facts = fact::facts_by_tag(&conn, &tag, limit)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&facts)?);
            } else if facts.is_empty() {
                println!("no live facts tagged '{tag}'");
            } else {
                let st = style();
                for f in facts {
                    let dated = f
                        .valid_from
                        .as_deref()
                        .map(|v| format!("as of {} · ", &v[..10.min(v.len())]))
                        .unwrap_or_default();
                    println!(
                        "• {}\n  {}",
                        f.statement,
                        st.dim(&format!("{dated}tags: {}", f.tags.as_deref().unwrap_or("-")))
                    );
                }
            }
        }

        Command::Annotate { episode: uid, tag, note } => {
            match episode::get_episode_by_uid(&conn, &uid)? {
                Some(ep) => {
                    for t in &tag {
                        let fresh = episode::annotate_episode(&conn, ep.id, "tag", t)?;
                        println!("tag '{t}'{}", if fresh { "" } else { " (already present)" });
                    }
                    for n in &note {
                        episode::annotate_episode(&conn, ep.id, "note", n)?;
                        println!("note added");
                    }
                    if tag.is_empty() && note.is_empty() {
                        let anns = episode::annotations_for(&conn, ep.id)?;
                        if want_json(cli_json, cli_text) {
                            println!("{}", serde_json::to_string_pretty(&anns)?);
                        } else if anns.is_empty() {
                            println!("no annotations — add with --tag/--note");
                        } else {
                            let st = style();
                            for a in anns {
                                println!(
                                    "{} {}  {}",
                                    a.kind,
                                    a.body,
                                    st.dim(&a.created_at)
                                );
                            }
                        }
                    }
                }
                None => println!("no episode with uid {uid}"),
            }
        }
        Command::Tags => {
            let tags = episode::list_tags(&conn)?;
            if want_json(cli_json, cli_text) {
                let v: Vec<_> = tags
                    .iter()
                    .map(|(t, n)| serde_json::json!({ "tag": t, "episodes": n }))
                    .collect();
                println!("{}", serde_json::to_string_pretty(&v)?);
            } else if tags.is_empty() {
                println!("no tags yet — tag episodes with `mecha-graph annotate` or `t` in the TUI");
            } else {
                for (t, n) in tags {
                    println!("#{t}  ({n})");
                }
            }
        }
        Command::Episodes { tag, limit } => {
            let eps = episode::episodes_by_tag(&conn, &tag, limit)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&eps)?);
            } else if eps.is_empty() {
                println!("no episodes tagged '{tag}'");
            } else {
                let st = style();
                for e in eps {
                    println!(
                        "• [{}] {}\n  {}",
                        &e.occurred_at[..10.min(e.occurred_at.len())],
                        e.body.lines().next().unwrap_or("").chars().take(90).collect::<String>(),
                        st.dim(&format!("{} · {}", e.source, e.uid))
                    );
                }
            }
        }
        Command::Tasks { all } => {
            let tasks = gtd::list_tasks(&conn, all)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&tasks)?);
            } else if tasks.is_empty() {
                println!("no tasks — accept a commitment in review, or add via kg_upsert");
            } else {
                let st = style();
                for t in tasks {
                    let mut extra = Vec::new();
                    if let Some(d) = &t.due_at {
                        extra.push(format!("due {}", &d[..10.min(d.len())]));
                    }
                    if let Some(w) = &t.waiting_on {
                        extra.push(format!("waiting on {w}"));
                    }
                    if let Some(p) = &t.project {
                        extra.push(format!("[{p}]"));
                    }
                    println!(
                        "{:<9} {}{}",
                        t.status,
                        t.name,
                        st.dim(&if extra.is_empty() {
                            String::new()
                        } else {
                            format!("  {}", extra.join(" · "))
                        })
                    );
                }
            }
        }
        Command::Dups => {
            let mut dups = graph::duplicate_person_candidates(&conn)?;
            dups.extend(graph::email_duplicate_candidates(&conn)?);
            if dups.is_empty() {
                println!("no duplicate-person candidates");
            }
            for (a, b, name) in dups {
                let detail = |id: &str| -> mecha_graph_core::Result<String> {
                    let ids: Vec<String> = {
                        let mut stmt = conn.prepare(
                            "SELECT kind || ':' || value FROM node_identifier WHERE node_id = ?1",
                        )?;
                        let v: Vec<String> = stmt
                            .query_map([id], |r| r.get(0))?
                            .collect::<std::result::Result<_, _>>()?;
                        v
                    };
                    let pi = rollup::get_person_interaction(&conn, id)?;
                    Ok(format!(
                        "{id} [{}] ({} interactions)",
                        ids.join(", "),
                        pi.map(|p| p.interaction_count).unwrap_or(0)
                    ))
                };
                println!("{name}:");
                println!("  keep? {}", detail(&a)?);
                println!("  dup?  {}", detail(&b)?);
                println!("  → mecha-graph merge <keep-id> <dup-id>");
            }
        }

        Command::Merge { keep, dup } => {
            graph::merge_nodes(&conn, &keep, &dup)?;
            rollup::rebuild_person_interactions(&conn)?;
            println!("merged {dup} into {keep} (aliases/identifiers/mentions/facts moved, rollup rebuilt)");
        }

        Command::Owner { name } => match name {
            Some(n) => {
                let matches = graph::resolve_entity_all(&conn, &n)?;
                match matches.len() {
                    0 => println!("no entity matching '{n}'"),
                    1 => {
                        graph::set_owner(&conn, &matches[0].id)?;
                        println!("owner: {} ({})", matches[0].name, matches[0].id);
                    }
                    _ => {
                        println!("'{n}' is ambiguous — name it by email or id:");
                        for m in matches {
                            println!("  {} ({})", m.name, m.id);
                        }
                    }
                }
            }
            None => match graph::owner_node(&conn)? {
                Some(nd) => println!("owner: {} ({})", nd.name, nd.id),
                None => println!("no owner set — `mecha-graph owner <name|email>`"),
            },
        },

        Command::Redact { episode: uid } => {
            if episode::redact_episode(&conn, &uid)? {
                println!("redacted episode {uid} and all derived data (tombstoned — re-ingest will not resurrect it)");
            } else {
                println!("no episode with uid {uid}");
            }
        }

        Command::Undo => {
            match mecha_graph_core::episode::undo_last(&conn)? {
                Some(msg) => println!("{msg}"),
                None => println!("nothing to undo"),
            }
        }

        Command::Tombstone { action } => match action {
            TombstoneAction::List => {
                let mut stmt = conn.prepare(
                    "SELECT source, source_id, created_at FROM episode_tombstone
                     ORDER BY created_at DESC",
                )?;
                let rows: Vec<(String, String, String)> = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
                    .collect::<std::result::Result<_, _>>()?;
                if rows.is_empty() {
                    println!("no tombstones — nothing is blocked from re-import");
                } else {
                    for (source, source_id, at) in &rows {
                        println!("{at}  {source}  {source_id}");
                    }
                    println!("{} tombstone(s); `mecha-graph tombstone rm <source> <source_id>` lifts one", rows.len());
                }
            }
            TombstoneAction::Rm { source, source_id } => {
                let n = conn.execute(
                    "DELETE FROM episode_tombstone WHERE source = ?1 AND source_id = ?2",
                    mecha_graph_core::rusqlite::params![source, source_id],
                )?;
                if n > 0 {
                    println!("lifted — the next sync may re-import {source} {source_id}");
                } else {
                    println!("no tombstone for {source} {source_id}");
                }
            }
        },

        Command::ReflectProcess => {
            let r = mecha_graph_core::sources::reflect::process_notes(&conn)?;
            println!(
                "reflect notes: {} scanned · {} promoted · {} attached · {} facts · {} identifiers · {} skipped (plain prose)",
                r.scanned, r.promoted, r.attached, r.facts, r.identifiers, r.skipped
            );
        }

        Command::BeeFacts { pull_limit } => {
            let r = mecha_graph_core::sources::bee::sync_bee_facts(&conn, pull_limit)?;
            println!(
                "bee facts: {} staged for review · {} confirmed in Bee · {} deleted in Bee{}",
                r.staged, r.confirmed, r.deleted,
                if r.push_errors > 0 { format!(" · {} push errors (will retry)", r.push_errors) } else { String::new() }
            );
        }

        Command::Precheck { auto_accept, no_semantic, dry_run } => {
            let embedder = if no_semantic {
                None
            } else {
                let e = embed::Embedder::default();
                e.available().then_some(e)
            };
            let r = mecha_graph_core::precheck::precheck_pending_opts(
                &conn, embedder.as_ref(), auto_accept, dry_run,
            )?;
            if dry_run {
                println!("(dry run — nothing changed)");
            }
            println!(
                "scanned {} · dup-of-fact {} · dup-in-queue {} · semantic-dup {} · \
                 ephemeral {} · contradictions {} · similar-flagged {} · auto-accepted {} · \
                 left {} · subjects-backfilled {} · subjects-phrased {} · \
                 subjects-implied {} · subjects-minted {} · predicates-canonicalized {} · \
                 eventive {} · rejected-dup {}",
                r.scanned, r.dup_of_fact, r.dup_in_queue, r.semantic_dup, r.ephemeral_rejected,
                r.contradiction_flagged, r.similar_flagged, r.auto_accepted, r.left_for_review,
                r.subject_backfilled, r.subject_phrased, r.subject_implied, r.subjects_minted,
                r.predicate_canonicalized, r.eventive_rejected, r.rejected_dup
            );
            if embedder.is_none() && !no_semantic {
                println!("(embedding server unreachable — semantic tier skipped)");
            }
        }

        Command::FixPersonNames { dry_run } => {
            let (fixes, skipped) = graph::promote_human_names(&conn, dry_run)?;
            if dry_run {
                println!("(dry run — nothing changed)\n");
            }
            for f in &fixes {
                println!("  {}  →  {}", f.from, f.to);
            }
            println!(
                "\n{} person node(s) {}renamed · {} skipped",
                fixes.len(),
                if dry_run { "would be " } else { "" },
                skipped.len()
            );
            for s in skipped.iter().take(10) {
                println!("  skip: {s}");
            }
        }

        Command::InvalidatePhantoms { dry_run } => {
            let r = mecha_graph_core::decay::invalidate_phantoms(&conn, dry_run)?;
            if dry_run {
                println!("(dry run — nothing changed)\n");
            }
            println!(
                "scanned {} co-occurrence beliefs\n  \
                 zero support (the repair set): {}\n  \
                 held (human-verified):         {}\n  \
                 partial collapse (NOT repaired, ambiguous): {}\n  \
                 distinct nodes implicated:     {}\n  \
                 {} {}",
                r.scanned, r.zero_support, r.held_user, r.partial_collapse,
                r.affected_nodes,
                if dry_run { "would invalidate:" } else { "invalidated:" },
                r.zero_support - r.held_user
            );
            if !r.samples.is_empty() {
                println!("\nsample of the repair set (claimed shared episodes → 0):");
                for (stmt, co) in &r.samples {
                    println!("  · [{co:>4} → 0] {stmt}");
                }
            }
        }

        Command::Decay { dry_run } => {
            let r = mecha_graph_core::decay::sweep_npmi(&conn, dry_run)?;
            if dry_run {
                println!("(dry run — nothing changed)");
            }
            println!(
                "scanned {} · closed {}/{} eligible (cap {}) · statements refreshed {} · \
                 held: band {}, user-verified {} · unparsed {}",
                r.scanned, r.closed, r.eligible, mecha_graph_core::decay::DECAY_CAP,
                r.refreshed, r.held_band, r.held_user, r.unparsed
            );
            if r.eligible > r.closed && !dry_run {
                println!(
                    "{} more eligible than the per-run cap allowed — drains next run",
                    r.eligible - r.closed
                );
            }
            if !r.integrity_alarms.is_empty() {
                println!(
                    "\n⚑ {} data-integrity alarm(s) — mentions lost, NOT decay; \
                     beliefs left untouched:",
                    r.integrity_alarms.len()
                );
                for (stmt, detail) in r.integrity_alarms.iter().take(10) {
                    println!("  · {stmt}\n    {detail}");
                }
            }
        }

        Command::BackfillDerivation { dry_run } => {
            let pending: i64 = conn.query_row(
                "SELECT COUNT(*) FROM fact WHERE extractor = 'npmi'
                   AND object_id IS NOT NULL AND episode_id IS NULL
                   AND valid_to IS NULL AND invalidated_at IS NULL",
                [], |r| r.get(0))?;
            if dry_run {
                println!("{pending} unrooted co-occurrence fact(s) would be re-provenanced");
            } else {
                let (n, retiered) = mecha_graph_core::linkers::backfill_npmi_derivation(&conn)?;
                println!(
                    "re-provenanced {n}/{pending} co-occurrence facts · {retiered} re-tiered \
                     (sensitivity MAX over full contributing set)"
                );
            }
        }

        Command::Verify { node, fact: fact_uid, limit } => {
            let checks = match (node, fact_uid) {
                (_, Some(uid)) => vec![mecha_graph_core::verify::verify_fact(&conn, &uid)?],
                (Some(name), None) => {
                    let matches = graph::resolve_entity_all(&conn, &name)?;
                    let node_id = match matches.len() {
                        0 => match graph::get_node(&conn, &name)? {
                            Some(n) => n.id,
                            None => return Err(mecha_graph_core::Error::Other(
                                format!("no entity matching '{name}'"))),
                        },
                        1 => matches[0].id.clone(),
                        _ => {
                            eprintln!("'{name}' is ambiguous — verify by id:");
                            for m in &matches {
                                eprintln!("  {} ({})", m.id, m.name);
                            }
                            return Ok(());
                        }
                    };
                    mecha_graph_core::verify::verify_node(&conn, &node_id, limit)?
                }
                (None, None) => return Err(mecha_graph_core::Error::Other(
                    "verify needs --node or --fact".into())),
            };
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&checks)?);
            } else {
                for c in &checks {
                    let v = format!("{:?}", c.verdict).to_lowercase();
                    println!("{:>13}  {}", v, c.statement);
                    println!("               {} · cited {:?}", c.detail, c.cited);
                }
            }
        }

        Command::ProbeTargets { limit, include_cold } => {
            let targets = mecha_graph_core::probe::probe_targets_opts(&conn, limit, include_cold)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&targets)?);
            } else {
                for t in &targets {
                    println!(
                        "{:6.1}  {} ({}) · {} touches · missing: [{}] · stale: [{}]",
                        t.score, t.name, t.node_type, t.touches,
                        t.missing_slots.join(", "),
                        t.stale_facts.iter().map(|(p, _)| p.as_str())
                            .collect::<Vec<_>>().join(", ")
                    );
                }
            }
        }

        Command::Corrections { limit } => {
            let s = mecha_graph_core::corrections::process_pending(&conn, limit)?;
            println!(
                "corrections {} · superseded {} · staged {} · negated {} · \
                 classes demoted {} · sweep targets {} · unresolved→review {}",
                s.processed, s.superseded, s.staged, s.negated,
                s.demoted, s.sweep_targets, s.unresolved
            );
        }

        Command::RecomputeConfidence { dry_run } => {
            let facts: Vec<(i64, f64)> = {
                let mut stmt = conn.prepare("SELECT id, confidence FROM fact")?;
                let rows = stmt
                    .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<std::result::Result<Vec<_>, _>>()?;
                rows
            };
            let (mut rose, mut fell, mut sum_delta) = (0usize, 0usize, 0f64);
            for (id, old) in &facts {
                let new = if dry_run {
                    // Compute without persisting: recompute then restore.
                    let n = fact::recompute_confidence(&conn, *id)?;
                    conn.execute(
                        "UPDATE fact SET confidence = ?2 WHERE id = ?1",
                        mecha_graph_core::rusqlite::params![id, old],
                    )?;
                    n
                } else {
                    fact::recompute_confidence(&conn, *id)?
                };
                let d = new - old;
                sum_delta += d;
                if d > 1e-9 {
                    rose += 1;
                } else if d < -1e-9 {
                    fell += 1;
                }
            }
            println!(
                "{} facts: {} fell, {} rose, {} unchanged · mean shift {:+.3}{}",
                facts.len(),
                fell,
                rose,
                facts.len() - fell - rose,
                if facts.is_empty() { 0.0 } else { sum_delta / facts.len() as f64 },
                if dry_run { " (dry run — nothing written)" } else { "" }
            );
        }

        Command::DedupeFacts { threshold, exact, apply } => {
            let pairs = mecha_graph_core::precheck::live_fact_dups(&conn, threshold, exact)?;
            if pairs.is_empty() {
                println!("no near-duplicate live facts at threshold {threshold}");
            }
            for p in &pairs {
                println!("{:.2}  keep {}  «{}»", p.similarity, &p.keep_uid[..8], p.keep_statement);
                println!("      drop {}  «{}»", &p.drop_uid[..8], p.drop_statement);
                if apply {
                    fact::supersede_fact(&conn, &p.drop_uid, None)?;
                }
            }
            if apply {
                println!("{} duplicates superseded", pairs.len());
            } else if !pairs.is_empty() {
                println!("{} pairs (dry run — pass --apply to supersede the drop side)", pairs.len());
            }
        }

        Command::Summarize { limit, model, node } => {
            let chat = mecha_graph_core::llm::ChatClient::connect(&model)?;
            match node {
                Some(id) => {
                    let done = mecha_graph_core::summarize::summarize_node(&conn, &chat, &id)?;
                    if done {
                        let ctx = mecha_graph_core::context::get_node_context(&conn, &id)?
                            .unwrap_or_default();
                        println!("{id}: {}", ctx.summary);
                    } else {
                        println!("{id}: nothing to summarize");
                    }
                }
                None => {
                    let report = mecha_graph_core::summarize::refresh_summaries(&conn, &chat, limit)?;
                    println!("summaries refreshed: {}", report.refreshed);
                    for e in &report.errors {
                        eprintln!("error: {e}");
                    }
                }
            }
        }

        Command::Extract { limit, model, source, episode } => {
            let chat = mecha_graph_core::llm::ChatClient::connect(&model)?;
            let report = if let Some(ep) = episode {
                mecha_graph_core::extract::reextract_episode(&conn, &chat, &ep)?
            } else {
                let sources: Vec<&str> = source.iter().map(|s| s.as_str()).collect();
                mecha_graph_core::extract::extract_pending(
                    &conn,
                    &chat,
                    limit,
                    (!sources.is_empty()).then_some(&sources[..]),
                )?
            };
            println!(
                "extracted {} episodes → {} mentions, {} fact candidates, {} commitments ({} errors)",
                report.episodes, report.mentions, report.fact_candidates,
                report.commitment_candidates, report.errors
            );
            if report.fact_candidates + report.commitment_candidates > 0 {
                println!("review with: mecha-graph review");
            }
        }

        Command::MemoryMd { out, budget } => {
            let md = mecha_graph_core::gtd::generate_memory_md(&conn, budget)?;
            match out {
                Some(path) => {
                    std::fs::write(&path, &md)?;
                    println!("wrote {} ({} chars)", path.display(), md.len());
                }
                None => print!("{md}"),
            }
        }

        Command::Gtd => {
            let r = mecha_graph_core::gtd::weekly_review(&conn)?;
            println!("# Weekly review\n");
            println!("## Stalled projects (active, no activity 14d)");
            for (_, name, last) in &r.stalled_projects {
                println!("- {name} (last: {})", last.as_deref().unwrap_or("never"));
            }
            println!("\n## Waiting on");
            for (task, person, due) in &r.waiting_on {
                println!("- {task} ← {person}{}", due.as_deref().map(|d| format!(" (due {d})")).unwrap_or_default());
            }
            println!("\n## Inbox (needs a next action)");
            for (_, name) in &r.inbox_tasks {
                println!("- {name}");
            }
            println!("\n## Goals with no active project");
            for (_, name) in &r.goals_without_project {
                println!("- {name}");
            }
        }

        Command::Source { action } => {
            use mecha_graph_core::integrations::{self, SourceConfig};
            let mut config = integrations::load_config()?;
            if integrations::ensure_defaults(&mut config) {
                integrations::save_config(&config)?;
            }

            match action {
                SourceAction::List => {
                    println!(
                        "{:<12} {:<10} {:<8} {:<40} {:<20} {}",
                        "NAME", "KIND", "ENABLED", "STATUS", "LAST OK", "ITEMS"
                    );
                    for (name, cfg) in &config.sources {
                        let test = integrations::test_source(name, cfg);
                        let status = if test.ok {
                            format!("ok: {}", test.detail)
                        } else {
                            format!("✗ {}", test.detail)
                        };
                        // ingest_state may track multiple ids per source kind.
                        let ids: &[&str] = match cfg.kind.as_str() {
                            "bee" => &["bee"],
                            "sessions" => &["session.hermes", "session.claude"],
                            "ics" => &["calendar"],
                            "slack" => &["slack"],
                            "github" => &["github"],
                            "imessage" => &["sms"],
                            "mbox" => &["email.mbox"],
                            _ => &[],
                        };
                        let mut last_ok: Option<String> = None;
                        let mut items = 0i64;
                        for id in ids {
                            if let Ok(row) = conn.query_row(
                                "SELECT last_ok_at, items_seen FROM ingest_state WHERE source = ?1",
                                [id],
                                |r| Ok((r.get::<_, Option<String>>(0)?, r.get::<_, i64>(1)?)),
                            ) {
                                if row.0 > last_ok {
                                    last_ok = row.0;
                                }
                                items += row.1;
                            }
                        }
                        println!(
                            "{:<12} {:<10} {:<8} {:<40} {:<20} {}",
                            name,
                            cfg.kind,
                            if cfg.enabled { "yes" } else { "no" },
                            status.chars().take(40).collect::<String>(),
                            last_ok.as_deref().unwrap_or("never"),
                            items
                        );
                    }
                    println!("\nconfig: {}", integrations::config_path().display());
                }

                SourceAction::Add {
                    kind, name, url, path, db, token, self_email, self_handles,
                    retention, mode, no_test,
                } => {
                    if let Some(r) = &retention {
                        if mecha_graph_core::sources::Retention::parse(r).is_none() {
                            return Err(mecha_graph_core::Error::Other(format!(
                                "invalid retention '{r}' — keep | capture | capture_delete"
                            )));
                        }
                    }
                    if !integrations::KINDS.contains(&kind.as_str()) {
                        return Err(mecha_graph_core::Error::Other(format!(
                            "unknown kind '{kind}' — one of {:?}",
                            integrations::KINDS
                        )));
                    }
                    let name = name.unwrap_or_else(|| kind.clone());
                    let mut settings = std::collections::BTreeMap::new();
                    let mut set = |k: &str, v: Option<String>| {
                        if let Some(v) = v {
                            settings.insert(k.to_string(), mecha_graph_core::toml::Value::String(v));
                        }
                    };
                    set("url", url);
                    set("path", path.map(|p| p.to_string_lossy().to_string()));
                    set("db", db.map(|p| p.to_string_lossy().to_string()));
                    set("token", token);
                    set("self_email", self_email);
                    set("self_handles", self_handles);
                    set("retention", retention);
                    set("mode", mode);

                    let cfg = SourceConfig { kind: kind.clone(), enabled: true, settings };
                    if !no_test {
                        let test = integrations::test_source(&name, &cfg);
                        if !test.ok {
                            return Err(mecha_graph_core::Error::Other(format!(
                                "test failed ({}) — fix it or pass --no-test: {}",
                                name, test.detail
                            )));
                        }
                        println!("test ok: {}", test.detail);
                    }
                    config.sources.insert(name.clone(), cfg);
                    integrations::save_config(&config)?;
                    println!("saved '{name}' to {}", integrations::config_path().display());
                    println!("next: mecha-graph source sync {name}");
                }

                SourceAction::Test { name } => {
                    let targets: Vec<(&String, &SourceConfig)> = match &name {
                        Some(n) => config
                            .sources
                            .get(n)
                            .map(|c| vec![(n, c)])
                            .ok_or_else(|| mecha_graph_core::Error::Other(format!("no source '{n}'")))?,
                        None => config.sources.iter().collect(),
                    };
                    for (n, cfg) in targets {
                        let t = integrations::test_source(n, cfg);
                        println!("{:<12} {}", n, if t.ok { format!("ok: {}", t.detail) } else { format!("FAIL: {}", t.detail) });
                    }
                }

                SourceAction::Sync { name, full } => {
                    let targets: Vec<(String, SourceConfig)> = match &name {
                        Some(n) => config
                            .sources
                            .get(n)
                            .map(|c| vec![(n.clone(), c.clone())])
                            .ok_or_else(|| mecha_graph_core::Error::Other(format!("no source '{n}'")))?,
                        None => config
                            .sources
                            .iter()
                            .filter(|(_, c)| c.enabled)
                            .map(|(n, c)| (n.clone(), c.clone()))
                            .collect(),
                    };
                    for (n, cfg) in targets {
                        match integrations::sync_source(&conn, &n, &cfg, full) {
                            Ok(r) => println!(
                                "{n}: +{} inserted, {} updated, {} unchanged{}, {} links",
                                r.inserted, r.updated, r.unchanged, r.tombstone_note(),
                                r.mentions + r.alias_mentions
                            ),
                            Err(e) => println!("{n}: FAILED — {e}"),
                        }
                    }
                    let people = rollup::rebuild_person_interactions(&conn)?;
                    println!("rollup: {people} people");
                }

                SourceAction::Enable { name } | SourceAction::Disable { name }
                    if !config.sources.contains_key(&name) =>
                {
                    return Err(mecha_graph_core::Error::Other(format!("no source '{name}'")));
                }
                SourceAction::Enable { name } => {
                    config.sources.get_mut(&name).unwrap().enabled = true;
                    integrations::save_config(&config)?;
                    println!("enabled {name}");
                }
                SourceAction::Disable { name } => {
                    config.sources.get_mut(&name).unwrap().enabled = false;
                    integrations::save_config(&config)?;
                    println!("disabled {name}");
                }
                SourceAction::Remove { name } => {
                    if config.sources.remove(&name).is_none() {
                        return Err(mecha_graph_core::Error::Other(format!("no source '{name}'")));
                    }
                    integrations::save_config(&config)?;
                    println!("removed {name} (already-ingested episodes are kept; use mecha-graph redact to purge)");
                }
            }
        }

        Command::Eval { gold } => {
            let queries = eval::load_gold(&gold)?;
            let embedder = embed::Embedder::default();
            let emb = embedder.available().then_some(&embedder);
            let report = eval::run(&conn, emb, &queries)?;
            for j in &report.per_job {
                println!(
                    "{:<8} n={:<3} recall@10={:.2} MRR={:.2}",
                    j.job, j.n, j.recall_at_10, j.mrr
                );
            }
            for r in &report.results {
                if !r.recall_at_10 {
                    println!("  MISS [{}] {}", r.job, r.query);
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_text_flag_does_not_collide_with_note_positional() {
        // Regression: the global --text bool used clap id "text", colliding
        // with `Note { text }`'s positional after global-arg propagation —
        // `mecha-graph note <msg>` panicked at argument-match time.
        let cli = Cli::try_parse_from(["mecha-graph", "note", "call Victor back"]).unwrap();
        assert!(!cli.text);
        match cli.command {
            Command::Note { text } => assert_eq!(text, "call Victor back"),
            _ => panic!("expected note subcommand"),
        }
        let cli = Cli::try_parse_from(["mecha-graph", "--text", "stats"]).unwrap();
        assert!(cli.text);
        // The full CLI definition stays internally consistent.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
