//! `pkg` CLI.

mod render;
mod tui;

use clap::{Parser, Subcommand};
use mecha_graph_core::{
    db, embed, entity_audit, episode, eval, fact, graph, gtd, rollup, router, sources, stats,
};
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

/// A seed nobody chose, from the clock.
///
/// Only ever used when `--seed` was omitted, and the value is printed the
/// moment it is drawn — an unreproducible sample cannot be checked, and the
/// whole reason to sample is to produce a number somebody will act on.
fn fresh_seed() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// splitmix64 — a whole PRNG in four lines, rather than a dependency.
///
/// Nothing here needs cryptographic randomness or a long period; it needs a
/// draw uncorrelated with the queue's ordering, and it needs to be
/// reproducible from a seed. Pulling `rand` in for that would add a
/// dependency to the one binary that reads the owner's encrypted graph.
fn next_u64(state: &mut u64) -> u64 {
    *state = state.wrapping_add(0x9E37_79B9_7F4A_7C15);
    let mut z = *state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Truncate `items` to `k` drawn uniformly at random, in draw order.
///
/// A partial Fisher-Yates: each of the first `k` positions gets a uniform
/// pick from the remainder, which is an unbiased sample and touches `k`
/// elements rather than the whole vector. Draw order is kept rather than
/// re-sorted — the order a reviewer sees is then also random, so fatigue
/// part-way through a sitting does not land preferentially on old ids.
fn draw_sample<T>(items: &mut Vec<T>, k: usize, seed: u64) {
    let n = items.len();
    if k >= n {
        return;
    }
    let mut state = seed;
    for i in 0..k {
        // Modulo bias is negligible at these magnitudes and irrelevant to
        // what this is for; a rejection loop here would be ceremony.
        let j = i + (next_u64(&mut state) % (n - i) as u64) as usize;
        items.swap(i, j);
    }
    items.truncate(k);
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
    // Explicit ids skip the filters but NOT the dry run: returning them
    // before the dry_run check made `accept <id> --dry-run` a real accept —
    // the flag read as a preview while the store changed underneath it. A
    // named id falls through to the same rendering as a bulk match, so the
    // preview shows the statement it would act on (and a named id that is
    // not pending honestly shows as no match).
    if !ids.is_empty() {
        if !dry_run {
            return Ok(ids);
        }
    } else if proposer.is_none()
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
            if !ids.is_empty() {
                return ids.contains(&c.id);
            }
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
    Note {
        text: String,
    },
    /// Re-run cheap linkers + rollups over existing episodes
    Link {
        /// Deprecated: accepted and ignored (the nightly passed it for a
        /// month while the handler discarded it; removing it would break
        /// an older script against a newer binary).
        #[arg(long, hide = true)]
        auto: bool,
        /// Also run the speculative tiers that STAGE review candidates
        /// (kNN, structural, rules). Off by default: they ran at 4–14%
        /// human accept for a month with nothing consuming that rate, so
        /// proposing is opt-in until a precision gate exists. The
        /// deterministic tiers (alias, temporal, NPMI) always run.
        #[arg(long)]
        propose: bool,
    },
    /// Health stats
    /// The autonomy ladder: every class with a ledger row or a human
    /// verdict record, its rung, and the rung that record would support.
    /// `--promote` applies the one-rung recompute (never demotes) — the
    /// one-shot for classes whose verdicts predate the 2026-08-16 Wilson
    /// switch, which promotion (firing only on a live verdict) never re-reads
    Ladder {
        /// Apply the promotions instead of previewing them
        #[arg(long)]
        promote: bool,
    },
    Stats,
    /// Review pending fact candidates
    Review {
        #[arg(long, default_value_t = 10)]
        top: i64,
        /// Cluster view: group the queue by (proposer, predicate) with
        /// verdict history and samples — one decision per class, not per fact
        #[arg(long)]
        clusters: bool,
        /// Proposer view: roll the queue up by proposing mechanism (the LLM
        /// extractor, the linkers, Bee, the rules) with each one's HUMAN
        /// accept rate — is this mechanism worth running?
        #[arg(long)]
        proposers: bool,
        /// Samples shown per cluster (spread, not top-confidence)
        #[arg(long, default_value_t = 3)]
        samples: usize,
        /// Only this proposer, e.g. `llm`, `bee:suggested`.
        #[arg(long)]
        proposer: Option<String>,
        /// Only this predicate — the cluster key, so `(commitment)` too.
        #[arg(long)]
        predicate: Option<String>,
        /// Draw this many candidates UNIFORMLY AT RANDOM from what the
        /// filters left, instead of taking the first `--top`.
        ///
        /// The queue is ordered, and every order it could have is correlated
        /// with something — age, id, confidence. Judging the first N and
        /// reading the result as the class's accept rate measures the order,
        /// which is how a class ends up with a rate nobody should trust. A
        /// random draw is the only selection that makes the rate an estimate
        /// of the class rather than of its head.
        #[arg(long)]
        sample: Option<usize>,
        /// Seed for `--sample`. Omit and one is drawn and printed — a sample
        /// nobody can redraw is a sample nobody can check.
        #[arg(long)]
        seed: Option<u64>,
        /// Group one class's pending candidates by semantic similarity
        /// (largest first), so one verdict can cover a group via
        /// `accept|reject <leader> --like`. Requires --proposer and
        /// --predicate — a group never crosses a class uninvited; the
        /// invitation is --across-classes.
        #[arg(long)]
        groups: bool,
        /// With --groups: the top layer — group the WHOLE pending queue
        /// (optionally one --proposer's) regardless of class, at the
        /// stricter global floor, largest groups first. Every group names
        /// the classes it spans; verdict one with
        /// `accept|reject <leader> --cascade <ids> --across-classes`.
        #[arg(long)]
        across_classes: bool,
        /// Cosine floor for --groups. Defaults per mode — precheck's
        /// similar-flag line within a class, the stricter global floor with
        /// --across-classes — so a model swap recalibrates every consumer
        /// together.
        #[arg(long)]
        threshold: Option<f64>,
        /// Only these candidate ids, comma-separated, returned in the order
        /// given — the fetch behind a similarity group's member listing.
        #[arg(long)]
        ids: Option<String>,
    },
    /// Rebind a pending candidate's unresolvable subject to a real entity —
    /// the way through `cannot resolve subject 'X'` without leaving the
    /// review surface that reported it. Takes the top suggestion, or `--to`
    /// names the target; the old spelling is learned as an alias so the next
    /// candidate carrying it resolves on its own.
    Bind {
        id: i64,
        /// Exact display name of the entity to bind to (else: top suggestion).
        #[arg(long)]
        to: Option<String>,
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
        /// A subject the graph does not know becomes a new topic node
        /// instead of a failure — the CLI spelling of the TUI's Shift-A.
        #[arg(long)]
        create_subjects: bool,
        /// Cascade: accept the one named id as YOUR verdict, then accept
        /// every pending candidate of the same class within --threshold of
        /// it as a machine cascade. One keystroke stays one human verdict —
        /// the members are labeled `cascade:<seed>` and never move the
        /// autonomy ladder.
        #[arg(long, conflicts_with_all = ["proposer","predicate","contains","min_confidence"])]
        like: bool,
        /// Cosine floor for --like (default: precheck's similar-flag line).
        #[arg(long, default_value_t = mecha_graph_core::similar::GROUP_THRESHOLD)]
        threshold: f64,
        /// Cascade over an EXPLICIT member list (comma-separated ids from a
        /// groups listing) instead of re-deriving similarity: the listing
        /// someone read is what their verdict is about, and no embedder
        /// runs. Same rules as --like: one seed, one human verdict, members
        /// labeled `cascade:<seed>`, never across a class unless
        /// --across-classes says so.
        #[arg(long, conflicts_with_all = ["like","proposer","predicate","contains","min_confidence"])]
        cascade: Option<String>,
        /// With --cascade: the listed ids may come from other classes —
        /// pair with a listing from `review --groups --across-classes`.
        /// Refused without --cascade: --like re-derives similarity, and the
        /// only cross-class set a verdict may ride is one a person READ.
        #[arg(long, requires = "cascade")]
        across_classes: bool,
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
        /// Cascade: reject the one named id as YOUR verdict, then reject
        /// every pending candidate of the same class within --threshold of
        /// it as a machine cascade — labeled, and invisible to the ladder.
        #[arg(long, conflicts_with_all = ["proposer","predicate","contains","max_confidence"])]
        like: bool,
        /// Cosine floor for --like (default: precheck's similar-flag line).
        #[arg(long, default_value_t = mecha_graph_core::similar::GROUP_THRESHOLD)]
        threshold: f64,
        /// Cascade over an EXPLICIT member list — see `accept --cascade`.
        #[arg(long, conflicts_with_all = ["like","proposer","predicate","contains","max_confidence"])]
        cascade: Option<String>,
        /// With --cascade: the listed ids may come from other classes —
        /// see `accept --across-classes`.
        #[arg(long, requires = "cascade")]
        across_classes: bool,
    },
    /// True-delete an episode and everything derived from it
    Redact {
        /// Episode uid
        episode: String,
    },
    /// Run the gold-set eval
    Eval {
        /// Gold queries. Defaults to `~/.mecha-graph/eval/gold.jsonl` —
        /// outside the repo, because the set is mined from real episodes.
        /// Override with `--gold` or `MECHA_GRAPH_GOLD`.
        #[arg(long)]
        gold: Option<PathBuf>,
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
        /// Skip these sources. The nightly passes calendar.event: a calendar
        /// body is a title plus an attendee list the deterministic tiers
        /// already extracted — LLM-extracting it re-derives tier-1 output
        /// as prose and queues it for human review.
        #[arg(long)]
        exclude_source: Vec<String>,
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
    /// Measure the global cluster threshold against the recorded human
    /// verdicts (review-on-use §4): at each cosine floor, how often two
    /// decided statements that close carried the SAME verdict — i.e. how
    /// often a cascade at that floor would have matched the owner's own
    /// history. Runs the Document and Dedup embedding spaces side by side.
    CalibrateGroups {
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// The utility loop's report (review-on-use §3): per-class retrieval
    /// record, what the precision gate blocks from extraction, and — with
    /// --floor — utility demotions (dry unless --apply)
    Utility {
        /// Days a fact must have been live to count as retrieval opportunity
        #[arg(long, default_value_t = 21)]
        days: i64,
        /// Classes with fewer eligible facts than this are not measured
        #[arg(long, default_value_t = 20)]
        min_facts: i64,
        /// Retrieval-rate floor for demotion + gating (unset = report only;
        /// the right number needs weeks of fact_usage data — open decision 3)
        #[arg(long)]
        floor: Option<f64>,
        /// Apply demotions instead of reporting what would demote
        #[arg(long)]
        apply: bool,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// The surfaced-verdict queue (review-on-use): live shadow facts that
    /// are about to matter — contradicting a reviewed fact, actually
    /// served in a context pack, or spot-checked by a sampled class.
    /// Verdicts: --confirm promotes to reviewed; --refute says never true.
    Shadow {
        /// Optional verb for driving surfaces: `list` answers the common
        /// review-row JSON shape (id/kind/title/detail) that mecha's
        /// /queues modal reads; `show <uid>` prints one fact in full.
        /// Bare `shadow` remains the human listing.
        action: Option<String>,
        /// The uid for `show`.
        id: Option<String>,
        /// Confirm a shadow fact by uid (promotes it to reviewed)
        #[arg(long, value_name = "FACT_UID")]
        confirm: Option<String>,
        /// Refute a shadow fact by uid (it was never true; retracts it)
        #[arg(long, value_name = "FACT_UID")]
        refute: Option<String>,
        /// Why, for --refute — feeds rejection memory, so say something
        #[arg(long)]
        reason: Option<String>,
        /// Max facts surfaced when listing
        #[arg(long, default_value_t = mecha_graph_core::shadow::DEFAULT_SURFACE_LIMIT)]
        limit: usize,
        /// Machine-readable output
        #[arg(long)]
        json: bool,
    },
    /// Bulk-convert the pending review backlog to shadow facts
    /// (review-on-use day one): every clean candidate goes live at tier
    /// 'shadow' — retrievable, rank-discounted, unreviewed — and earns a
    /// human verdict when a query pulls it. Commitments, precheck-flagged
    /// candidates and unresolvable subjects stay queued.
    ShadowConvert {
        /// Max candidates to convert in one run
        #[arg(long, default_value_t = 100_000)]
        limit: i64,
    },
    /// Promote a human alias to the display name for person nodes named
    /// by an email address. The address keeps resolving (identifier +
    /// alias); only what renders changes.
    FixPersonNames {
        /// Report what would change without writing
        #[arg(long)]
        dry_run: bool,
    },
    /// One-shot repair: node ids sitting in `subject`/`object`, which are
    /// NAMES. `linker:knn` wrote ids there, so its candidates could not be
    /// accepted (`cannot resolve subject 'topic-…'`) and `--create-subjects`
    /// minted placeholder nodes whose display name is another node's id.
    /// Merges the placeholders away and rewrites pending payloads to names.
    RepairIdPayloads {
        /// Report without writing
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
        /// Drop targets witnessed by fewer than N distinct sources.
        ///
        /// Gossip is two readers over independent sources, so `2` is its
        /// precondition: below that the probe is refused ("one witness
        /// cannot gossip"), exits 0 and produces nothing — while the node's
        /// slot-gaps stay open, so it scores just as highly tomorrow. One
        /// node was picked on five consecutive nights that way
        /// (2026-08-28 → 2026-09-01), a third of the nightly budget spent
        /// on a probe that could not run.
        ///
        /// **"Source" is connector-level, and that is weaker than it
        /// sounds.** `episode.source` holds `email.thread`,
        /// `bee.conversation`, `slack.thread`, `note`, `calendar.event` —
        /// about a dozen values. So a node whose evidence is forty email
        /// threads from forty different correspondents counts as ONE
        /// source and is dropped from the nightly, permanently, with the
        /// empty-targets branch reporting that no node has two witnessing
        /// sources.
        ///
        /// That is gossip's own definition, not this flag's:
        /// `kg_entity`'s coverage is `GROUP BY e.source`
        /// (`episode.rs`), and `choose_vantages` picks two rows from it.
        /// So the filter still never drops a node gossip would have
        /// accepted, which is what makes it a safe necessary condition —
        /// but a reader should not take "two witnesses" to mean two
        /// people. Widening it (per-correspondent vantages for
        /// `email.thread`) is gossip's change to make, not this filter's.
        ///
        /// **Necessary, not sufficient**, and off by default: gossip also
        /// wants `min_coverage` episodes per source inside its own window,
        /// which is measured at probe time and is not knowable from SQL.
        /// This removes only what no window could rescue. Default 0 keeps
        /// experiments (and `--include-cold`, whose nodes are often
        /// single-source) seeing the unfiltered ranking.
        #[arg(long, default_value_t = 0)]
        min_sources: i64,
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
        /// Only tasks associated with this person, project or topic. Unions
        /// `about`, `waiting_on` and `assigned_to`, plus tasks whose parent
        /// project is this node. Pair with --all for everything, open and
        /// finished, involving them.
        #[arg(long)]
        entity: Option<String>,
    },
    /// Scan task titles for entities the graph already knows, filing matches
    /// as unreviewed (`shadow`) `about` associations. Dry unless --apply
    ScanTasks {
        /// Actually mint the associations; omit to survey only
        #[arg(long)]
        apply: bool,
        /// Stop after this many associations (0 = no cap)
        #[arg(long, default_value_t = 0)]
        limit: usize,
    },
    /// Report date columns holding text that is not a date (a model's `when`
    /// written verbatim). Dry unless --apply, which nulls them
    RepairDates {
        /// Actually null the malformed values; omit to survey only
        #[arg(long)]
        apply: bool,
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
    /// Rename a node. The old name is kept as an alias, so everything that
    /// reached it by the old name still does.
    Rename {
        /// Node id, name or alias of the node to rename
        target: String,
        /// The name it should have
        new_name: String,
    },
    /// Create a person node that nothing in the graph proposed — for someone
    /// who has facts and episodes but no entity of their own.
    NewPerson {
        /// Their name
        name: String,
    },
    /// State a fact yourself. Lands live, not in the review queue: a thing
    /// the owner asserts is an instruction, not an inference about the world
    /// — the same rule that lets `kg_task_create` and `kind=alias` write
    /// directly.
    Assert {
        /// Subject: node id, name or alias
        subject: String,
        /// Predicate, from the existing vocabulary
        predicate: String,
        /// Object: node id, name or alias. Omit for an attribute-style fact
        /// and give --value instead.
        object: Option<String>,
        /// A literal object, when the object is not a node
        #[arg(long)]
        value: Option<String>,
        /// The sentence form — what search and a reader actually see. One is
        /// composed from the parts if you do not give one.
        #[arg(long)]
        statement: Option<String>,
    },
    /// Retract a fact you asserted. Takes the uid `assert` printed (or one
    /// from `entity --json`) — never a text match, because retracting the
    /// wrong fact because a substring matched is the failure this graph
    /// keeps finding.
    Retract {
        /// The fact's uid
        uid: String,
        /// When it stopped being true. Omitted, it is invalidated as of now
        /// — the right choice for a claim that was never right, as against
        /// one that has simply ended.
        #[arg(long)]
        as_of: Option<String>,
    },
    /// Rewrite a name inside PROPOSED fact candidates. Candidates store
    /// their subject and object as text and resolve them only on accept, so
    /// a name that has been reassigned leaves a queue pointing at whoever
    /// holds it now.
    RetextCandidates {
        /// The name as the candidates spell it
        from: String,
        /// What it should say
        #[arg(long)]
        to: String,
        /// Candidate ids to leave alone — the ones that really do mean the
        /// other person
        #[arg(long, value_delimiter = ',')]
        except: Vec<i64>,
        /// Show what would change and write nothing
        #[arg(long)]
        dry_run: bool,
    },
    /// The predicate vocabulary: what exists, what is doing work, and what
    /// nobody decided on.
    Predicates {
        #[command(subcommand)]
        action: PredicateAction,
    },
    /// Re-judge alias mentions already on file against the corroboration
    /// rule, and report what it would no longer link. REPORTS BY DEFAULT —
    /// `--apply` is opt-in, because this can retract thousands of rows.
    RelinkAliases {
        /// Actually retract. Without this, nothing is written.
        #[arg(long)]
        apply: bool,
        /// Stop after this many episodes
        #[arg(long)]
        limit: Option<i64>,
    },
    /// Run the entity detectors and file what they find as proposals. The
    /// entity-layer counterpart of `extract` + `precheck`: it proposes and
    /// never repairs, because the repair direction is usually not derivable
    /// from the data.
    Audit,
    /// Entity maintenance proposals: what the audit found, and deciding it.
    Proposals {
        #[command(subcommand)]
        action: ProposalAction,
    },
    /// Change a node's type, keeping its id and everything hanging off it.
    /// The type decides resolution rank, so an org filed as a topic loses to
    /// an event of the same name.
    Retype {
        /// Node id, name or alias
        target: String,
        /// The type it should be
        #[arg(long = "type", value_name = "TYPE")]
        node_type: String,
    },
    /// Create a node of any type in the closed set — an org, a place, a
    /// project — that nothing in the graph proposed.
    NewNode {
        /// person|place|org|project|goal|area|task|event|event_series|topic|
        /// artifact|document
        #[arg(long = "type", value_name = "TYPE")]
        node_type: String,
        /// Its name
        name: String,
    },
    /// Add an alias to a node: another way of saying the same name.
    Alias {
        /// Node id, name or alias of the node
        target: String,
        /// The alias to add
        alias: String,
    },
    /// Remove an alias from a node — for when the name belonged to somebody
    /// else. (`rename` keeps the old name on purpose; this is the other
    /// repair.)
    Unalias {
        /// Node id, name or alias of the node
        target: String,
        /// The alias to remove
        alias: String,
    },
    /// Move a deterministic identifier (an email, a handle) to another node.
    /// This is what decides where *future* ingest lands, so a split that
    /// leaves it behind re-merges on the next sync.
    MoveIdentifier {
        /// email | phone | slack_uid | handle | orcid | url | doi | path
        kind: String,
        /// The identifier's value
        value: String,
        /// Node id, name or alias to move it to
        #[arg(long)]
        to: String,
    },
    /// Re-point every fact endpoint from one node to another, leaving both
    /// nodes in place. For a contaminated node — an event that a fuzzy name
    /// match made the subject of facts about a person — where a merge would
    /// destroy the node and carry across the evidence that really is its own.
    MoveFacts {
        /// Node id, name or alias the facts are wrongly on
        from: String,
        /// Node id, name or alias they belong to
        #[arg(long)]
        to: String,
    },
    /// Move mentions in bulk, optionally narrowed to one extractor and/or
    /// one episode source — which is how the contaminating mentions are
    /// separated from the node's own, by how the graph came to believe each.
    MoveMentions {
        /// Node id, name or alias to move them off
        from: String,
        /// Node id, name or alias to move them to
        #[arg(long)]
        to: String,
        /// Only mentions made this way: alias|attendee|llm|regex|manual|…
        #[arg(long)]
        extractor: Option<String>,
        /// Only mentions from episodes of this source
        #[arg(long)]
        source: Option<String>,
    },
    /// Move one episode's mention from one node to another.
    MoveMention {
        /// The episode's uid
        episode_uid: String,
        /// Node id, name or alias it is currently on
        #[arg(long)]
        from: String,
        /// Node id, name or alias to move it to
        #[arg(long)]
        to: String,
    },
}

#[derive(Subcommand)]
enum PredicateAction {
    /// Every predicate with its live fact count and whether a human chose it
    List {
        /// Only the ones nobody decided on
        #[arg(long)]
        unblessed: bool,
    },
    /// Fold one predicate into another: facts re-pointed, the old name
    /// learned as an alias so extraction stops re-minting it
    Merge {
        /// The predicate to absorb
        from: String,
        /// The one that survives
        #[arg(long)]
        into: String,
    },
    /// Promote an auto-registered predicate to one somebody chose
    Bless {
        name: String,
        /// What it means — it goes into the extraction prompt
        #[arg(long)]
        description: String,
    },
}

#[derive(Subcommand)]
enum ProposalAction {
    /// What is waiting, strongest first
    List {
        /// Only this detector's class
        #[arg(long)]
        detector: Option<String>,
        #[arg(long, default_value_t = 20)]
        limit: i64,
    },
    /// Counts by detector
    Summary,
    /// One proposal, whole: what it would do and the evidence for it
    Show { id: i64 },
    /// Accept a proposal — and apply it, where its repair is fully
    /// determined by the proposal itself
    Accept { ids: Vec<i64> },
    /// Reject a proposal. Durable: the detector will not re-file it.
    Reject { ids: Vec<i64> },
    /// Drop one detector's PENDING proposals — for a detector that has been
    /// retuned, whose old output describes a rule that no longer exists.
    /// Decided proposals are never touched: they are the record of what has
    /// already been asked and answered, and dropping them would let a
    /// rejection be re-filed on the next run.
    Clear {
        #[arg(long)]
        detector: String,
    },
    /// File a merge proposal yourself — the owner's own finding, on the same
    /// record the detectors use, so a no-undo merge always leaves a decided
    /// proposal behind it. `--accept` decides and applies it in the same
    /// breath: the web's one-gesture merge, with the audit trail kept.
    FileMerge {
        /// The node to keep: id, name or alias
        keep: String,
        /// The duplicate to fold into it: id, name or alias
        dup: String,
        /// Decide and apply immediately
        #[arg(long)]
        accept: bool,
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

/// Resolve a node id, exact name, or exact alias to exactly one node id.
///
/// **Exact only — never the fuzzy tier.** `resolve_entity_all` falls back to
/// `LIKE '%name%'` when nothing exact matches, which is right for a lookup
/// and wrong for anything that then *writes*: `rename Wren "Wren Calder"`
/// would have found the event "SPSP Reedie Reunion" by substring and
/// renamed that. So a fuzzy-only match is reported as candidates for the
/// caller to pick from by id, rather than acted on.
///
/// Ambiguity is likewise a refusal with the options printed. The verbs this
/// serves all mutate, and the cost of guessing wrong is somebody's identity.
fn resolve_one(
    conn: &mecha_graph_core::rusqlite::Connection,
    target: &str,
) -> mecha_graph_core::Result<String> {
    // An id, given directly.
    if let Some(node) = graph::get_node(conn, target)? {
        return Ok(node.id);
    }
    let canonical = mecha_graph_core::ids::canonicalize(target);
    let exact: Vec<_> = graph::resolve_entity_all(conn, target)?
        .into_iter()
        .filter(|n| n.canonical_name == canonical || n.aliases.contains(&canonical))
        .collect();
    match exact.len() {
        1 => Ok(exact[0].id.clone()),
        0 => {
            let near = graph::resolve_entity_all(conn, target)?;
            if near.is_empty() {
                Err(mecha_graph_core::error::Error::Other(format!(
                    "nothing named {target:?}"
                )))
            } else {
                let mut msg = format!(
                    "nothing is named exactly {target:?}. Near matches — name one by id:\n"
                );
                for n in near {
                    msg.push_str(&format!("  {} ({}, {})\n", n.name, n.id, n.node_type));
                }
                Err(mecha_graph_core::error::Error::Other(msg))
            }
        }
        _ => {
            let mut msg = format!("{target:?} is ambiguous — name one by id:\n");
            for n in exact {
                msg.push_str(&format!("  {} ({}, {})\n", n.name, n.id, n.node_type));
            }
            Err(mecha_graph_core::error::Error::Other(msg))
        }
    }
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
            println!(
                "use:   MECHA_GRAPH_DB={} mecha-graph …   (or --db)",
                out.display()
            );
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
            let n_r = embed::embed_pending_rejects(&conn, &embedder, limit, batch)?;
            embed::set_embed_meta(
                &conn,
                &embedder.model,
                embedder.dims,
                embed::EmbedTask::Document.tag(),
            )?;
            println!("embedded {n_ep} episodes, {n_f} facts, {n_r} rejected statements");
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
            // JSON first, and it answers `[]` rather than a sentence: a
            // caller parsing this needs "no matches" to be a value, not a
            // line of prose it has to recognise.
            if want_json(cli_json, cli_text) {
                let mut out = Vec::new();
                for node in &matches {
                    let facts = match &as_of {
                        Some(d) => fact::facts_as_of(&conn, &node.id, d, 20)?,
                        None => fact::facts_for_node(&conn, &node.id, 20)?,
                    };
                    let pi = rollup::get_person_interaction(&conn, &node.id)?;
                    out.push(serde_json::json!({
                        "id": node.id,
                        "name": node.name,
                        "node_type": node.node_type,
                        "aliases": node.aliases,
                        "interactions": pi.as_ref().map(|p| p.interaction_count),
                        "last_seen_at": pi.as_ref().and_then(|p| p.last_seen_at.clone()),
                        "facts": facts.iter().map(|f| serde_json::json!({
                            "uid": f.uid,
                            "statement": f.statement,
                            "polarity": f.polarity,
                            "valid_from": f.valid_from,
                        })).collect::<Vec<_>>(),
                    }));
                }
                println!("{}", serde_json::to_string_pretty(&out)?);
                return Ok(());
            }
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

        Command::Link { auto: _, propose } => {
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
            // ...then the linker cascade (temporal → NPMI, plus the
            // candidate-staging tiers only when asked), then rollups.
            let cascade = mecha_graph_core::linkers::run_cascade(&conn, propose)?;
            let people = rollup::rebuild_person_interactions(&conn)?;
            let staged = if propose {
                format!(
                    "knn: {} staged · structural: {} staged · rules: {} staged",
                    cascade.knn_candidates, cascade.structural_candidates, cascade.rule_candidates
                )
            } else {
                "proposing tiers skipped (opt in with --propose)".to_string()
            };
            println!(
                "alias-scan: {linked} mentions · temporal: {} attributed · npmi: {} facts · {staged} · rollup: {people} people",
                cascade.temporal_mentions, cascade.npmi_facts
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

        Command::Ladder { promote } => {
            use mecha_graph_core::ladder;
            let moves = if promote {
                ladder::recompute_rungs(&conn, true)?
            } else {
                Vec::new()
            };
            let mut rows = ladder::ladder_view(&conn)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&rows)?);
            } else {
                // Pending first: the reason to read this table is "what is
                // sitting in the queue and would a promotion drain it".
                rows.sort_by(|a, b| b.pending.cmp(&a.pending).then(b.judged.cmp(&a.judged)));
                println!(
                    "{} class(es) · rung / earned · human record · Wilson LB · pending\n",
                    rows.len()
                );
                for v in &rows {
                    let arrow = if v.earned != v.rung {
                        format!(" -> {}", v.earned.as_str())
                    } else {
                        String::new()
                    };
                    println!(
                        "  {:>5}  {:<40} {:<8}{:<12} {:>4}/{:<4} lb {:.2}",
                        v.pending,
                        format!("{} . {}", v.proposer, v.predicate),
                        v.rung.as_str(),
                        arrow,
                        v.accepted,
                        v.judged,
                        v.wilson_lb,
                    );
                }
                if promote {
                    println!("\npromoted {} class(es)", moves.len());
                    for v in &moves {
                        println!(
                            "  {} . {}  {} -> {}  ({}/{}, lb {:.2})",
                            v.proposer,
                            v.predicate,
                            v.rung.as_str(),
                            v.earned.as_str(),
                            v.accepted,
                            v.judged,
                            v.wilson_lb
                        );
                    }
                } else {
                    let due = rows.iter().filter(|v| v.earned != v.rung).count();
                    if due > 0 {
                        println!(
                            "\n{due} class(es) have earned a rung their ledger does not hold                              - `mecha-graph ladder --promote` applies them"
                        );
                    }
                }
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

        Command::Review { top, clusters, proposers, samples, proposer, predicate, sample, seed, groups, across_classes, threshold, ids } => {
            if groups && across_classes {
                if predicate.is_some() {
                    return Err(mecha_graph_core::Error::Other("--across-classes has no class: drop --predicate, or drop --across-classes for the class view".into()));
                }
                let e = embed::Embedder::default();
                if !e.available() {
                    return Err(mecha_graph_core::Error::Other("embedding server not answering — groups need vectors".into()));
                }
                let th = threshold.unwrap_or(mecha_graph_core::similar::GLOBAL_GROUP_THRESHOLD);
                let (gs, considered) = mecha_graph_core::similar::groups_across_classes(&conn, &e, th, proposer.as_deref())?;
                if want_json(cli_json, cli_text) {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "v": 1,
                            "threshold": th,
                            "across_classes": true,
                            "considered": considered,
                            "groups": gs,
                        }))?
                    );
                    return Ok(());
                }
                let covered: usize = gs.iter().map(|g| g.size()).sum();
                // The singleton drop is reported, never silent: a view that
                // shows less than the queue must say how much less.
                println!(
                    "{} group(s) covering {covered} of {considered} pending (cosine >= {th:.2}; singletons stay in their class listings)\n",
                    gs.len()
                );
                for g in &gs {
                    let span: Vec<String> = g.classes.iter().map(|(c, n)| format!("{c} x{n}")).collect();
                    println!("  x{:<4} #{:<7} {}", g.size(), g.leader_id, g.leader_statement);
                    println!("           spans: {}", span.join(", "));
                    for sm in &g.sample {
                        println!("           ~ {sm}");
                    }
                }
                if !gs.is_empty() {
                    println!("\none verdict per group: mecha-graph accept|reject <leader-id> --cascade <ids> --across-classes");
                }
                return Ok(());
            }
            if groups {
                let (Some(p), Some(key)) = (proposer.as_deref(), predicate.as_deref()) else {
                    return Err(mecha_graph_core::Error::Other("--groups needs --proposer and --predicate: a group never crosses a class uninvited (--across-classes is the top layer)".into()));
                };
                let e = embed::Embedder::default();
                if !e.available() {
                    return Err(mecha_graph_core::Error::Other("embedding server not answering — groups need vectors".into()));
                }
                let th = threshold.unwrap_or(mecha_graph_core::similar::GROUP_THRESHOLD);
                let gs = mecha_graph_core::similar::groups_for_class(&conn, &e, p, key, th)?;
                if want_json(cli_json, cli_text) {
                    // An envelope, not a bare array: the threshold rides back
                    // so a caller adjusting it (the TUI's [ and ] keys) steps
                    // from the value that actually ran, never from its own
                    // copy of the constant — the drifted-literal trap.
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "v": 1,
                            "threshold": th,
                            "groups": gs,
                        }))?
                    );
                    return Ok(());
                }
                let covered: usize = gs.iter().map(|g| g.size()).sum();
                println!(
                    "{} group(s) covering {covered} candidate(s) in {p} . {key} (cosine >= {th:.2})\n",
                    gs.len()
                );
                for g in &gs {
                    println!("  x{:<4} #{:<7} {}", g.size(), g.leader_id, g.leader_statement);
                    for sm in &g.sample {
                        println!("           ~ {sm}");
                    }
                }
                if !gs.is_empty() {
                    println!("\none verdict per group: mecha-graph accept|reject <leader-id> --like");
                }
                return Ok(());
            }
            if proposers {
                let rows = mecha_graph_core::precheck::proposer_stats(&conn)?;
                if want_json(cli_json, cli_text) {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                    return Ok(());
                }
                let total: usize = rows.iter().map(|p| p.pending).sum();
                println!("{total} pending from {} proposer(s)\n", rows.len());
                println!(
                    "{:>6}  {:<30} {:>10} {:>9}  {:<9} {}",
                    "PEND", "PROPOSER", "YOU SAID", "CONFIDENT", "EVIDENCE", "AUTO-DROPPED"
                );
                for p in &rows {
                    // A rate with no denominator prints as a dash, never as
                    // 0% — "never judged" and "always rejected" are opposite
                    // findings and rendering them alike is what made the
                    // whole queue read as junk.
                    let rate = match p.accept_rate() {
                        Some(r) => format!("{:.0}% /{}", r * 100.0, p.judged()),
                        None => "— /none".into(),
                    };
                    let lb = match p.accept_lb {
                        Some(l) => format!("≥{:.0}%", l * 100.0),
                        None => "—".into(),
                    };
                    let evidence = match p.judged() {
                        0 => "unjudged",
                        1..=9 => "thin",
                        10..=29 => "some",
                        _ => "solid",
                    };
                    // Truncate rather than let a long proposer push every
                    // later column out of line — a table that only lines up
                    // for short names is a table nobody scans.
                    let name: String = if p.proposer.chars().count() > 30 {
                        p.proposer.chars().take(29).chain(std::iter::once('…')).collect()
                    } else {
                        p.proposer.clone()
                    };
                    println!(
                        "{:>6}  {:<30} {:>10} {:>9}  {:<9} {}",
                        p.pending, name, rate, lb, evidence, p.machine_rejected
                    );
                }
                println!(
                    "\n'you said' counts your verdicts only; auto-dropped are this pipeline's\n\
                     own dup/ephemeral rejects and are never folded into the rate.\n\
                     Drill in: mecha-graph review --clusters | mecha-graph tui (p)"
                );
                return Ok(());
            }
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
            // Filters and the random draw. `pending_candidates` is asked
            // for everything when we are selecting, because a draw from the
            // first N is a draw from the ordering — the thing `--sample`
            // exists to escape. `review_clusters` already reads the queue
            // whole, so this costs no more than the view beside it.
            let selecting =
                proposer.is_some() || predicate.is_some() || sample.is_some() || ids.is_some();
            let mut pending = fact::pending_candidates(&conn, if selecting { 100_000 } else { top })?;
            if let Some(p) = &proposer {
                pending.retain(|c| c.proposed_by.as_deref().unwrap_or("?") == p);
            }
            if let Some(pred) = &predicate {
                pending.retain(|c| {
                    mecha_graph_core::precheck::cluster_key(&c.payload).0 == *pred
                });
            }
            if let Some(list) = &ids {
                let order: Vec<i64> = list
                    .split(',')
                    .filter_map(|t| t.trim().parse().ok())
                    .collect();
                pending.retain(|c| order.contains(&c.id));
                pending.sort_by_key(|c| {
                    order.iter().position(|i| *i == c.id).unwrap_or(usize::MAX)
                });
            }
            if let Some(k) = sample {
                let used = seed.unwrap_or_else(fresh_seed);
                draw_sample(&mut pending, k, used);
                if !want_json(cli_json, cli_text) {
                    println!(
                        "sample of {} from {}{} — redraw with --seed {used}\n",
                        pending.len(),
                        proposer.as_deref().unwrap_or("everything"),
                        predicate.as_ref().map(|p| format!(" · {p}")).unwrap_or_default(),
                    );
                }
            } else if selecting {
                pending.truncate(top.max(0) as usize);
            }
            // Only for a reader. A prose line ahead of the array makes the
            // whole of stdout un-parseable, and `--json` exists to be parsed:
            // a caller asking for an empty result got
            // "did not answer JSON" instead of `[]`.
            if pending.is_empty() && !want_json(cli_json, cli_text) {
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

        Command::Bind { id, to } => {
            match fact::bind_subject(&conn, id, to.as_deref()) {
                Ok((old, new)) => println!("#{id} subject '{old}' → {new} — accept to promote"),
                Err(e) => {
                    println!("#{id} FAILED: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Accept { ids, proposer, predicate, contains, min_confidence, limit, dry_run, create_subjects, like, threshold, cascade, across_classes } => {
            if let Some(csv) = &cascade {
                let [seed] = ids[..] else {
                    return Err(mecha_graph_core::Error::Other("--cascade takes exactly one candidate id (the seed)".into()));
                };
                let listed: Vec<i64> = csv.split(',').filter_map(|t| t.trim().parse().ok()).collect();
                let members = if across_classes {
                    // Measured 2026-08-29: cross-class semantic twins agreed
                    // with each other's human verdict only ~63% of the time,
                    // at every floor. Crossing stays possible — the owner
                    // asked by flag — but never silently.
                    eprintln!(
                        "WARNING: cross-class cascades matched the owner's own verdict record \
                         only ~{:.0}% of the time (calibrate-groups, 2026-08-29) — expect to \
                         overwrite ~1 in 3; within-class runs ~{:.0}%",
                        mecha_graph_core::similar::MEASURED_CROSS_CLASS_AGREEMENT * 100.0,
                        mecha_graph_core::similar::MEASURED_SAME_CLASS_AGREEMENT * 100.0
                    );
                    mecha_graph_core::similar::vet_cascade_ids_across(&conn, seed, &listed)?
                } else {
                    mecha_graph_core::similar::vet_cascade_ids(&conn, seed, &listed)?
                };
                if dry_run {
                    println!("#{seed} (your verdict) + {} listed would accept", members.len());
                    return Ok(());
                }
                let uid = fact::accept_candidate_opts(&conn, seed, create_subjects, true)?;
                println!("#{seed} accepted -> fact {uid} (your verdict)");
                let (mut done, mut failed) = (0usize, 0usize);
                for id in &members {
                    match fact::accept_candidate_cascade(&conn, *id, seed) {
                        Ok(_) => done += 1,
                        Err(e) => {
                            failed += 1;
                            println!("  #{id} cascade FAILED: {e}");
                        }
                    }
                }
                println!(
                    "cascade: {done} accepted, {failed} left pending — one human verdict on the ladder"
                );
                return Ok(());
            }
            if like {
                let [seed] = ids[..] else {
                    return Err(mecha_graph_core::Error::Other("--like takes exactly one candidate id (the seed)".into()));
                };
                let e = embed::Embedder::default();
                if !e.available() {
                    return Err(mecha_graph_core::Error::Other("embedding server not answering — --like needs vectors".into()));
                }
                let similar = mecha_graph_core::similar::similar_to(&conn, &e, seed, threshold)?;
                if dry_run {
                    println!("#{seed} (your verdict) + {} similar would accept", similar.len());
                    for (id, sim) in &similar {
                        println!("  #{id}  cosine {sim:.2}");
                    }
                    return Ok(());
                }
                // The seed is the human verdict; if it cannot land, nothing
                // cascades — a fan-out from a failed verdict is a fan-out
                // from nothing.
                let uid = fact::accept_candidate_opts(&conn, seed, create_subjects, true)?;
                println!("#{seed} accepted -> fact {uid} (your verdict)");
                let (mut done, mut failed) = (0usize, 0usize);
                for (id, _sim) in &similar {
                    match fact::accept_candidate_cascade(&conn, *id, seed) {
                        Ok(_) => done += 1,
                        Err(e) => {
                            failed += 1;
                            println!("  #{id} cascade FAILED: {e}");
                        }
                    }
                }
                println!(
                    "cascade: {done} accepted, {failed} left pending — one human verdict on the ladder"
                );
                return Ok(());
            }
            let ids = resolve_triage_ids(
                &conn, ids, &proposer, &predicate, &contains, min_confidence, None, limit, dry_run,
            )?;
            for id in ids {
                // Commitment candidates materialize a Task; plain ones a fact.
                match mecha_graph_core::extract::accept_commitment(&conn, id) {
                    Ok(task_id) => println!("#{id} accepted → task {task_id}"),
                    Err(_) => match fact::accept_candidate_opts(&conn, id, create_subjects, true) {
                        Ok(uid) => println!("#{id} accepted → fact {uid}"),
                        Err(e) => println!("#{id} FAILED: {e}"),
                    },
                }
            }
        }

        Command::Reject { ids, reason, proposer, predicate, contains, max_confidence, limit, dry_run, like, threshold, cascade, across_classes } => {
            if let Some(csv) = &cascade {
                let [seed] = ids[..] else {
                    return Err(mecha_graph_core::Error::Other("--cascade takes exactly one candidate id (the seed)".into()));
                };
                let listed: Vec<i64> = csv.split(',').filter_map(|t| t.trim().parse().ok()).collect();
                let members = if across_classes {
                    eprintln!(
                        "WARNING: cross-class cascades matched the owner's own verdict record \
                         only ~{:.0}% of the time (calibrate-groups, 2026-08-29) — expect to \
                         overwrite ~1 in 3; within-class runs ~{:.0}%",
                        mecha_graph_core::similar::MEASURED_CROSS_CLASS_AGREEMENT * 100.0,
                        mecha_graph_core::similar::MEASURED_SAME_CLASS_AGREEMENT * 100.0
                    );
                    mecha_graph_core::similar::vet_cascade_ids_across(&conn, seed, &listed)?
                } else {
                    mecha_graph_core::similar::vet_cascade_ids(&conn, seed, &listed)?
                };
                if dry_run {
                    println!("#{seed} (your verdict) + {} listed would reject", members.len());
                    return Ok(());
                }
                fact::reject_candidate(&conn, seed, &reason)?;
                println!("#{seed} rejected (your verdict)");
                let (mut done, mut failed) = (0usize, 0usize);
                for id in &members {
                    match fact::reject_candidate_cascade(&conn, *id, seed, None) {
                        Ok(()) => done += 1,
                        Err(e) => {
                            failed += 1;
                            println!("  #{id} cascade FAILED: {e}");
                        }
                    }
                }
                println!(
                    "cascade: {done} rejected, {failed} left pending — one human verdict on the ladder"
                );
                return Ok(());
            }
            if like {
                let [seed] = ids[..] else {
                    return Err(mecha_graph_core::Error::Other("--like takes exactly one candidate id (the seed)".into()));
                };
                let e = embed::Embedder::default();
                if !e.available() {
                    return Err(mecha_graph_core::Error::Other("embedding server not answering — --like needs vectors".into()));
                }
                let similar = mecha_graph_core::similar::similar_to(&conn, &e, seed, threshold)?;
                if dry_run {
                    println!("#{seed} (your verdict) + {} similar would reject", similar.len());
                    for (id, sim) in &similar {
                        println!("  #{id}  cosine {sim:.2}");
                    }
                    return Ok(());
                }
                fact::reject_candidate(&conn, seed, &reason)?;
                println!("#{seed} rejected (your verdict)");
                let (mut done, mut failed) = (0usize, 0usize);
                for (id, sim) in &similar {
                    match fact::reject_candidate_cascade(&conn, *id, seed, Some(*sim)) {
                        Ok(()) => done += 1,
                        Err(e) => {
                            failed += 1;
                            println!("  #{id} cascade FAILED: {e}");
                        }
                    }
                }
                println!(
                    "cascade: {done} rejected, {failed} left pending — one human verdict on the ladder"
                );
                return Ok(());
            }
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
        Command::RepairDates { apply } => {
            let report = gtd::repair_unparseable_dates(&conn, apply)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else if report.found.is_empty() {
                println!("no malformed dates");
            } else {
                for b in &report.found {
                    println!("{:<24} {:<12} {}", b.column, b.value, b.label);
                }
                if apply {
                    println!("\nnulled {} value(s)", report.repaired);
                } else {
                    println!(
                        "\n{} malformed value(s) — dry run, re-run with --apply to null them",
                        report.found.len()
                    );
                }
            }
        }
        Command::ScanTasks { apply, limit } => {
            let report = gtd::propose_task_entities(&conn, apply, limit)?;
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&report)?);
            } else {
                let verb = if apply { "filed as shadow" } else { "to file" };
                println!(
                    "scanned {} open task(s): {} association(s) {verb}, \
                     {} already known, {} weak first-name match(es) refused",
                    report.scanned, report.minted, report.already, report.refused_weak
                );
                if report.capped {
                    println!("stopped at --limit {limit}; re-run to continue");
                }
                if report.minted > 0 && apply {
                    println!(
                        "these are UNREVIEWED — they earn a verdict when a query serves one \
                         (`mecha-graph shadow`)"
                    );
                } else if report.minted > 0 {
                    println!("dry run — re-run with --apply to file them");
                }
            }
        }
        Command::Tasks { all, entity } => {
            // Resolve first: an unknown name must not print an empty board,
            // which reads as "this person has no tasks".
            let tasks = match entity.as_deref().map(str::trim).filter(|s| !s.is_empty()) {
                Some(name) => {
                    // `resolve_about` so `--entity @owner` works here too.
                    let node = gtd::resolve_about(&conn, name)?.ok_or_else(|| {
                        mecha_graph_core::Error::Other(format!("no node matches '{name}'"))
                    })?;
                    gtd::tasks_for_entity(&conn, &node.id, all)?
                }
                None => gtd::list_tasks(&conn, all)?,
            };
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
                    if !t.about.is_empty() {
                        extra.push(format!("about {}", t.about.join(", ")));
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

        Command::Rename { target, new_name } => {
            let id = resolve_one(&conn, &target)?;
            let fix = graph::rename_node(&conn, &id, &new_name)?;
            println!(
                "renamed {} → {}  ({})\n  {:?} kept as an alias, so it still resolves",
                fix.from, fix.to, fix.node_id, fix.from
            );
            // The queue does not follow a rename: candidates store names as
            // text and resolve them on accept. That is harmless while the
            // old name still means this node — it is an alias now — and
            // becomes dangerous the moment the name is REASSIGNED to
            // somebody else, which is exactly what a split does.
            let queued = fact::candidates_naming(&conn, &fix.from)?;
            if queued > 0 {
                println!(
                    "  note: {queued} pending candidate(s) still say {:?}. They will resolve to \n                       whoever holds that name when accepted — fine while it is this node's alias, \n                       wrong if you give the name to someone else. `retext-candidates {:?} --to {:?}`",
                    fix.from, fix.from, fix.to
                );
            }
        }

        Command::NewPerson { name } => {
            let node = graph::create_person(&conn, &name, "manual")?;
            println!("created {} ({})", node.name, node.id);
        }

        Command::Assert {
            subject,
            predicate,
            object,
            value,
            statement,
        } => {
            let subject_id = resolve_one(&conn, &subject)?;
            let object_id = match &object {
                Some(o) => Some(resolve_one(&conn, o)?),
                None => None,
            };
            // The vocabulary is a foreign key, and refusing an unknown
            // predicate here is deliberate: the extractor already mints them
            // on demand (83 live, from a seed of about 40), and a hand verb
            // that did the same would add a second unreviewed way for the
            // vocabulary to grow.
            let known: bool = conn.query_row(
                "SELECT COUNT(*) > 0 FROM predicate WHERE name = ?1",
                mecha_graph_core::rusqlite::params![predicate],
                |r| r.get(0),
            )?;
            if !known {
                let mut stmt = conn.prepare(
                    "SELECT name FROM predicate WHERE name LIKE ?1 ORDER BY name LIMIT 8",
                )?;
                let near: Vec<String> = stmt
                    .query_map(
                        mecha_graph_core::rusqlite::params![format!("%{}%", predicate.split('_').next().unwrap_or(""))],
                        |r| r.get(0),
                    )?
                    .collect::<std::result::Result<_, _>>()?;
                let hint = if near.is_empty() {
                    "run `mecha-graph assert --help` and pick an existing one".to_string()
                } else {
                    format!("did you mean: {}", near.join(", "))
                };
                return Err(mecha_graph_core::error::Error::Other(format!(
                    "unknown predicate {predicate:?} — {hint}"
                )));
            }

            let subject_name = graph::get_node(&conn, &subject_id)?
                .map(|n| n.name)
                .unwrap_or_else(|| subject_id.clone());
            let object_name = match &object_id {
                Some(id) => graph::get_node(&conn, id)?.map(|n| n.name),
                None => None,
            };
            let statement = statement.unwrap_or_else(|| {
                let tail = object_name
                    .clone()
                    .or_else(|| value.clone())
                    .unwrap_or_default();
                format!("{subject_name} {} {tail}", predicate.replace('_', " "))
                    .trim()
                    .to_string()
            });
            let uid = fact::assert_fact(
                &conn,
                &subject_id,
                &predicate,
                object_id.as_deref(),
                value.as_deref(),
                &statement,
                None,
                None,
                1.0,
                // Not an extractor: this is the owner speaking, and the
                // provenance has to say so or a later audit reads a hand
                // assertion as a machine guess.
                "manual",
            )?;
            println!("{statement}  [{uid}]");
        }

        Command::Retract { uid, as_of } => {
            // Read it back before retracting, so the confirmation names what
            // actually went rather than the uid the caller typed — the same
            // reason `outbox show` resolves a staged path before printing it.
            let before = fact::get_fact_by_uid(&conn, &uid)?;
            fact::supersede_fact(&conn, &uid, as_of.as_deref())?;
            match before {
                Some(f) => println!("retracted: {}", f.statement),
                None => println!("retracted {uid}"),
            }
        }

        Command::RetextCandidates {
            from,
            to,
            except,
            dry_run,
        } => {
            let changed = fact::retext_candidates(&conn, &from, &to, &except, dry_run)?;
            for (id, statement) in &changed {
                println!("  #{id}  {statement}");
            }
            println!(
                "{} candidate(s) {}",
                changed.len(),
                if dry_run { "would change" } else { "rewritten" }
            );
            if !except.is_empty() {
                println!("  left alone: {except:?}");
            }
        }

        Command::Predicates { action } => match action {
            PredicateAction::List { unblessed } => {
                let mut stmt = conn.prepare(
                    "SELECT p.name,
                            (SELECT COUNT(*) FROM fact f
                              WHERE f.predicate = p.name AND f.valid_to IS NULL) AS live,
                            p.description = 'auto-registered' AS auto
                     FROM predicate p
                     WHERE (?1 = 0 OR p.description = 'auto-registered')
                     ORDER BY live DESC, p.name",
                )?;
                let rows = stmt.query_map(
                    mecha_graph_core::rusqlite::params![i64::from(unblessed)],
                    |r| {
                        Ok((
                            r.get::<_, String>(0)?,
                            r.get::<_, i64>(1)?,
                            r.get::<_, bool>(2)?,
                        ))
                    },
                )?;
                for r in rows {
                    let (name, live, auto) = r?;
                    println!(
                        "  {live:>5}  {}  {name}",
                        if auto { "auto " } else { "chosen" }
                    );
                }
            }
            PredicateAction::Merge { from, into } => {
                let (moved, blocked) = fact::merge_predicate(&conn, &from, &into)?;
                println!("{from} → {into}: {moved} fact(s) moved");
                if blocked > 0 {
                    println!(
                        "  {blocked} stayed: {into} already holds an identical live fact for them, \
                         so {from} was kept rather than emptied"
                    );
                }
            }
            PredicateAction::Bless { name, description } => {
                fact::bless_predicate(&conn, &name, &description)?;
                println!("{name}: {description}");
            }
        },

        Command::RelinkAliases { apply, limit } => {
            let (episodes, retracted) = episode::relink_alias_mentions(&conn, apply, limit)?;
            // Grouped by the NAME rather than listed per row: a thousand
            // refusals of one first name is one finding, and printing them
            // individually buries it.
            let mut by_name: std::collections::BTreeMap<String, i64> =
                std::collections::BTreeMap::new();
            for r in &retracted {
                *by_name.entry(r.alias.clone()).or_default() += 1;
            }
            let mut rows: Vec<_> = by_name.into_iter().collect();
            rows.sort_by(|a, b| b.1.cmp(&a.1));
            for (alias, n) in rows.iter().take(20) {
                println!("  {n:>6}  {alias}");
            }
            println!(
                "{} episode(s) examined · {} alias mention(s) {}",
                episodes,
                retracted.len(),
                if apply {
                    "retracted"
                } else {
                    "would be retracted — pass --apply to do it"
                }
            );
            if apply {
                rollup::rebuild_person_interactions(&conn)?;
                println!("rollup rebuilt");
            }
        }

        Command::Audit => {
            let found = entity_audit::run_all(&conn)?;
            let total: usize = found.iter().map(|(_, n)| n).sum();
            for (detector, n) in &found {
                if *n > 0 {
                    println!("  {n:>4} new  {detector}");
                }
            }
            println!("{total} new proposal(s) — `mecha-graph proposals list` to review");
        }

        Command::Proposals { action } => match action {
            ProposalAction::Summary => {
                let rows = entity_audit::summary(&conn)?;
                if want_json(cli_json, cli_text) {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                    return Ok(());
                }
                for r in rows {
                    let age = r
                        .oldest
                        .map(|t| format!("  oldest {t}"))
                        .unwrap_or_default();
                    println!(
                        "  {:>4} pending  {:>4} decided   {}{age}",
                        r.pending, r.decided, r.detector
                    );
                }
            }
            ProposalAction::List { detector, limit } => {
                let rows = entity_audit::pending(&conn, detector.as_deref(), limit)?;
                if want_json(cli_json, cli_text) {
                    println!("{}", serde_json::to_string_pretty(&rows)?);
                    return Ok(());
                }
                if rows.is_empty() {
                    println!("nothing pending");
                }
                for p in rows {
                    println!("#{} [{}] {}", p.id, p.detector, p.kind);
                    println!("   {}", p.subject_name);
                    if !p.other_name.is_empty() {
                        println!("   + {}", p.other_name);
                    }
                    println!("   {}", p.evidence);
                }
            }
            ProposalAction::Show { id } => {
                let Some(p) = entity_audit::get(&conn, id)? else {
                    return Err(mecha_graph_core::error::Error::Other(format!(
                        "no proposal #{id}"
                    )));
                };
                println!("proposal #{}  [{}]  {}", p.id, p.detector, p.status);
                println!("kind: {}", p.kind);
                println!("\n{}", p.subject_name);
                if !p.other_name.is_empty() {
                    println!("  +  {}", p.other_name);
                }
                println!("\n{}", p.evidence);
                if let Some(payload) = &p.payload {
                    println!("\nwould apply: {payload}");
                }
                println!("\nnodes: {} {}", p.subject_id, p.other_id);
            }
            ProposalAction::Accept { ids } => {
                for id in ids {
                    let Some(p) = entity_audit::get(&conn, id)? else {
                        println!("#{id}: no such proposal");
                        continue;
                    };
                    // Apply first, decide second: a proposal marked accepted
                    // whose repair then failed is a lie the queue keeps
                    // telling.
                    match entity_audit::apply(&conn, &p) {
                        Ok(msg) => {
                            entity_audit::decide(&conn, id, "accepted", "user")?;
                            println!("#{id}: {msg}");
                        }
                        Err(e) => println!("#{id}: NOT applied — {e}"),
                    }
                }
            }
            ProposalAction::Reject { ids } => {
                for id in ids {
                    match entity_audit::decide(&conn, id, "rejected", "user") {
                        Ok(()) => println!("#{id}: rejected"),
                        Err(e) => println!("#{id}: {e}"),
                    }
                }
            }
            ProposalAction::FileMerge { keep, dup, accept } => {
                let keep_id = resolve_one(&conn, &keep)?;
                let dup_id = resolve_one(&conn, &dup)?;
                if keep_id == dup_id {
                    return Err(mecha_graph_core::error::Error::Other(format!(
                        "{keep:?} and {dup:?} are the same node ({keep_id}) — nothing to merge"
                    )));
                }
                let keep_name = graph::get_node(&conn, &keep_id)?.map(|n| n.name).unwrap_or_default();
                let dup_name = graph::get_node(&conn, &dup_id)?.map(|n| n.name).unwrap_or_default();
                entity_audit::propose(
                    &conn,
                    "owner",
                    "merge",
                    &keep_id,
                    &dup_id,
                    None,
                    &format!("owner filed: fold {dup_name} ({dup_id}) into {keep_name} ({keep_id})"),
                    0.0,
                )?;
                // `propose` dedupes (INSERT OR IGNORE) and does not hand the
                // id back either way; the pending row is the truth for both
                // the fresh filing and the already-filed one.
                let id: i64 = conn.query_row(
                    "SELECT id FROM entity_proposal
                     WHERE kind = 'merge' AND subject_id = ?1 AND other_id = ?2
                       AND status = 'pending'",
                    mecha_graph_core::rusqlite::params![keep_id, dup_id],
                    |r| r.get(0),
                )?;
                if accept {
                    // Apply first, decide second — same order as Accept, for
                    // the same reason: an accepted proposal whose repair
                    // failed is a lie the queue keeps telling.
                    let p = entity_audit::get(&conn, id)?.ok_or_else(|| {
                        mecha_graph_core::error::Error::Other("proposal vanished mid-file".into())
                    })?;
                    let msg = entity_audit::apply(&conn, &p)?;
                    entity_audit::decide(&conn, id, "accepted", "user")?;
                    if want_json(cli_json, cli_text) {
                        println!(
                            "{}",
                            serde_json::json!({ "id": id, "applied": true, "result": msg })
                        );
                    } else {
                        println!("#{id}: {msg}");
                    }
                } else if want_json(cli_json, cli_text) {
                    println!("{}", serde_json::json!({ "id": id, "applied": false }));
                } else {
                    println!("#{id}: filed — `proposals accept {id}` applies it");
                }
            }
            ProposalAction::Clear { detector } => {
                let n = entity_audit::clear_pending(&conn, &detector)?;
                println!("{n} pending proposal(s) dropped from {detector} (decisions kept)");
            }
        },

        Command::Retype { target, node_type } => {
            let id = resolve_one(&conn, &target)?;
            let (was, now) = graph::retype_node(&conn, &id, &node_type)?;
            println!("{id}: {was} → {now}");
        }

        Command::NewNode { node_type, name } => {
            let node = graph::create_node(&conn, &node_type, &name, "manual")?;
            println!("created {} ({}, {})", node.name, node.node_type, node.id);
        }

        Command::Alias { target, alias } => {
            let id = resolve_one(&conn, &target)?;
            graph::add_alias(&conn, &id, &alias, "manual")?;
            println!("{id}: {alias:?} added as an alias");
        }

        Command::Unalias { target, alias } => {
            let id = resolve_one(&conn, &target)?;
            if graph::remove_alias(&conn, &id, &alias)? {
                println!("{id}: {alias:?} removed");
            } else {
                // Not an error: the caller asked for it to be gone, and it
                // is gone. Saying so anyway, because a silent no-op on a
                // repair reads as success on the wrong node.
                println!("{id}: no alias {alias:?} — nothing removed");
            }
        }

        Command::MoveIdentifier { kind, value, to } => {
            let to_id = resolve_one(&conn, &to)?;
            graph::move_identifier(&conn, &kind, &value, &to_id)?;
            println!("{kind} {value:?} now resolves to {to_id}");
        }

        Command::MoveFacts { from, to } => {
            let from_id = resolve_one(&conn, &from)?;
            let to_id = resolve_one(&conn, &to)?;
            let moved = graph::move_facts(&conn, &from_id, &to_id)?;
            println!(
                "{} subject(s), {} object(s) re-pointed {from_id} → {to_id}",
                moved.subjects, moved.objects
            );
            if moved.self_loops > 0 {
                println!(
                    "  {} fact(s) linking the two dropped — they would have become self-loops",
                    moved.self_loops
                );
            }
            if moved.blocked > 0 {
                // Reported, never resolved on the caller's behalf: the two
                // ways to resolve it are folding observation counts and
                // deleting evidence, and a partial move must do neither
                // silently.
                println!(
                    "  {} fact(s) stayed: the destination already holds an identical live fact",
                    moved.blocked
                );
            }
        }

        Command::MoveMentions {
            from,
            to,
            extractor,
            source,
        } => {
            let from_id = resolve_one(&conn, &from)?;
            let to_id = resolve_one(&conn, &to)?;
            let (moved, dropped) = graph::move_mentions(
                &conn,
                &from_id,
                &to_id,
                extractor.as_deref(),
                source.as_deref(),
            )?;
            rollup::rebuild_person_interactions(&conn)?;
            println!("{moved} mention(s) moved {from_id} → {to_id} (rollup rebuilt)");
            if dropped > 0 {
                println!("  {dropped} dropped as redundant — the destination already had them");
            }
        }

        Command::MoveMention {
            episode_uid,
            from,
            to,
        } => {
            let from_id = resolve_one(&conn, &from)?;
            let to_id = resolve_one(&conn, &to)?;
            graph::move_mention(&conn, &episode_uid, &from_id, &to_id)?;
            // The interaction rollup is derived from mentions, so a move
            // that skipped this would leave "last seen" describing a person
            // the episode is no longer about.
            rollup::rebuild_person_interactions(&conn)?;
            println!("episode {episode_uid}: {from_id} → {to_id} (rollup rebuilt)");
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
            use mecha_graph_core::sources::bee::BEE_PUSH_STUCK_ATTEMPTS;
            let r = mecha_graph_core::sources::bee::sync_bee_facts(&conn, pull_limit)?;
            let stuck = r
                .push_failures
                .iter()
                .filter(|f| f.attempts >= BEE_PUSH_STUCK_ATTEMPTS)
                .count();
            println!(
                "bee facts: {} staged for review · {} confirmed in Bee · {} deleted in Bee{}",
                r.staged,
                r.confirmed,
                r.deleted,
                // **"will retry" only while that is still a forecast.** Past
                // the stuck threshold the retry has been tried and has not
                // worked, and saying "will retry" there is what let one
                // verdict fail for eight nights while reading as routine.
                if r.push_errors == 0 {
                    String::new()
                } else if stuck > 0 {
                    format!(
                        " · {} push error(s), {stuck} STUCK (see below)",
                        r.push_errors
                    )
                } else {
                    format!(" · {} push error(s) (will retry)", r.push_errors)
                }
            );
            // The reason, on stderr beside the summary. A count that names
            // no cause cannot be acted on, which is the whole finding here.
            // Capped like the decay alarm printer in this same binary.
            //
            // **Not for an unreachable CLI** — that was the stated reason
            // and it is unreachable: a `bee` that will not run fails the
            // PULL first, which propagates with `?`, so this loop never
            // executes. The cap earns its place on the case that IS
            // reachable: `facts list` healthy while `confirm`/`delete`
            // fail — a revoked write scope, or a fact set deleted in bulk
            // on Bee's side — where every pending verdict fails in one
            // sweep and an uncapped loop turns one outage into a page of
            // stderr.
            // **Stuck first, so the cap cannot hide what the header names.**
            // The summary line says `N STUCK (see below)`; with more than ten
            // failures the stuck one could sit at position eleven and be
            // truncated away, leaving a header pointing at nothing — and the
            // stuck entries are the whole reason this list exists, since a
            // fresh failure is expected to resolve itself.
            let mut ordered: Vec<&_> = r.push_failures.iter().collect();
            ordered.sort_by_key(|f| std::cmp::Reverse(f.attempts));
            for f in ordered.iter().take(10) {
                let verb = if f.accepted { "confirm" } else { "delete" };
                let stuck_marker = if f.attempts >= BEE_PUSH_STUCK_ATTEMPTS {
                    " ⚑ STUCK"
                } else {
                    ""
                };
                eprintln!(
                    "  bee push failed{stuck_marker}: {verb} fact {} (candidate {}) — \
                     attempt {}, failing since {} — {}",
                    f.bee_fact_id, f.candidate_id, f.attempts, f.first_failed_at, f.error
                );
            }
            // Capped for the same reason as `push_failures` below — and more
            // pressingly, because the scenario that comment names (a fact set
            // deleted in bulk on Bee's side) fills THIS list, not that one:
            // every pending verdict comes back "Fact not found" at once.
            // Each tail directly under its own list — they had been ordered
            // opposite to the lists they continue, so "… and N more" sat
            // under the abandoned block while counting failures.
            if r.push_failures.len() > 10 {
                eprintln!(
                    "  … and {} more failure(s), lowest attempt counts first",
                    r.push_failures.len() - 10
                );
            }
            for t in r.push_terminal.iter().take(10) {
                eprintln!("  bee push abandoned: {t}");
            }
            if r.push_terminal.len() > 10 {
                eprintln!("  … and {} more abandoned", r.push_terminal.len() - 10);
            }
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
                 shadow-minted {} · left {} · subjects-backfilled {} · subjects-phrased {} · \
                 subjects-implied {} · subjects-minted {} · predicates-canonicalized {} · \
                 eventive {} · rejected-dup {} · rejected-semantic {} · commitment-dup {} · \
                 commitment-stale {}",
                r.scanned, r.dup_of_fact, r.dup_in_queue, r.semantic_dup, r.ephemeral_rejected,
                r.contradiction_flagged, r.similar_flagged, r.auto_accepted, r.shadow_minted,
                r.left_for_review,
                r.subject_backfilled, r.subject_phrased, r.subject_implied, r.subjects_minted,
                r.predicate_canonicalized, r.eventive_rejected, r.rejected_dup,
                r.rejected_semantic, r.commitment_dup, r.commitment_stale
            );
            if embedder.is_none() && !no_semantic {
                println!("(embedding server unreachable — semantic tier skipped)");
            }
            if r.semantic_skipped {
                // In the summary, not just stderr: nightly.sh greps the log
                // for this exact string and raises an ALERT line — a run
                // whose guard could not work must say so where the zeros
                // are read. Keep the string in step with the grep.
                println!("SEMANTIC TIERS SKIPPED mid-run — embedding failed; the zeros above are blindness, not cleanliness");
            }
        }

        Command::CalibrateGroups { json } => {
            let e = embed::Embedder::default();
            if !e.available() {
                return Err(mecha_graph_core::Error::Other(
                    "embedding server not answering — calibration needs vectors".into(),
                ));
            }
            let doc = mecha_graph_core::similar::calibrate_global_threshold(
                &conn,
                &e,
                embed::EmbedTask::Document,
            )?;
            let ded = mecha_graph_core::similar::calibrate_global_threshold(
                &conn,
                &e,
                embed::EmbedTask::Dedup,
            )?;
            if json {
                println!("{}", serde_json::json!({ "document": doc, "dedup": ded }));
            } else {
                println!(
                    "cascade agreement with {} human verdicts, by cosine floor:",
                    doc.verdicts
                );
                let fmt = |p: &mecha_graph_core::similar::CalibrationPoint| match p.agreement {
                    Some(a) => format!("{:>5.1}% ({:>6})", a * 100.0, p.pairs),
                    None => format!("    - ({:>6})", p.pairs),
                };
                println!("  floor   doc all (pairs)    doc same-class     doc cross-class    dedup all (pairs)");
                for (i, d) in doc.points.iter().enumerate() {
                    println!(
                        "  {:.2}   {:>16}   {:>16}   {:>16}   {:>16}",
                        d.threshold,
                        fmt(d),
                        fmt(&doc.same_class_points[i]),
                        fmt(&doc.cross_class_points[i]),
                        fmt(&ded.points[i]),
                    );
                }
                println!(
                    "(current GLOBAL_GROUP_THRESHOLD = {:.2}; a cascade's disagreement rate \
                     at a floor is the rate it would overwrite a verdict you gave differently)",
                    mecha_graph_core::similar::GLOBAL_GROUP_THRESHOLD
                );
            }
        }

        Command::Utility {
            days,
            min_facts,
            floor,
            apply,
            json,
        } => {
            use mecha_graph_core::ladder;
            let floors = floor.map(|f| ladder::UtilityFloors {
                floor: f,
                min_eligible: min_facts,
                opportunity_days: days,
            });
            let mut classes = ladder::class_utility(&conn, days)?;
            classes.retain(|c| c.eligible >= min_facts);
            classes.sort_by(|a, b| {
                a.rate
                    .unwrap_or(2.0)
                    .total_cmp(&b.rate.unwrap_or(2.0))
                    .then_with(|| b.eligible.cmp(&a.eligible))
            });
            let gated = ladder::gated_classes(&conn, floors.as_ref())?;
            let demotions = match &floors {
                Some(f) => ladder::utility_demotions(&conn, f, apply)?,
                None => vec![],
            };
            if json {
                println!(
                    "{}",
                    serde_json::json!({
                        "window_days": days, "min_eligible": min_facts,
                        "classes": classes, "gated": gated,
                        "demotions": demotions.iter().map(|(cu, from, to)| serde_json::json!({
                            "proposer": cu.proposer, "predicate": cu.predicate,
                            "rate": cu.rate, "from": from.as_str(), "to": to.as_str(),
                            "applied": apply,
                        })).collect::<Vec<_>>(),
                    })
                );
            } else {
                println!("class utility (eligible ≥ {min_facts}, {days}-day window), least useful first:");
                for c in classes.iter().take(15) {
                    let rate = c
                        .rate
                        .map(|r| format!("{:.0}%", r * 100.0))
                        .unwrap_or_else(|| "-".into());
                    println!(
                        "  {}·{}  live {} · eligible {} · retrieved {} ({rate})",
                        c.proposer, c.predicate, c.live, c.eligible, c.retrieved
                    );
                }
                for g in &gated {
                    println!("  GATED from extraction: {}·{} — {}", g.proposer, g.predicate, g.why);
                }
                for (cu, from, to) in &demotions {
                    println!(
                        "  {} {}·{}: {} → {} (retrieval {:.0}% over {} eligible)",
                        if apply { "DEMOTED" } else { "would demote" },
                        cu.proposer,
                        cu.predicate,
                        from.as_str(),
                        to.as_str(),
                        cu.rate.unwrap_or(0.0) * 100.0,
                        cu.eligible
                    );
                }
                // One grep-able line for the nightly log: what the loop
                // did, or that it is running dry — never silence.
                println!(
                    "utility: {} classes measured · {} gated · {} demoted{}",
                    classes.len(),
                    gated.len(),
                    demotions.len(),
                    if floors.is_none() {
                        " (report-only: no --floor set)"
                    } else if apply {
                        ""
                    } else {
                        " (dry run)"
                    }
                );
            }
        }

        Command::Shadow {
            action,
            id,
            confirm,
            refute,
            reason,
            limit,
            json,
        } => {
            match action.as_deref() {
                Some("list") => {
                    // The common review-row shape (id/kind/title/detail)
                    // that generic review surfaces read — one schema for
                    // rule proposals, harness candidates, entity proposals
                    // and now shadow verdicts.
                    let q = mecha_graph_core::shadow::surfaced(&conn, limit)?;
                    let rows: Vec<serde_json::Value> = q
                        .iter()
                        .map(|s| {
                            serde_json::json!({
                                "id": s.fact.uid,
                                "kind": format!(
                                    "{}·{}",
                                    s.fact.extractor.as_deref().unwrap_or("?"),
                                    s.fact.predicate
                                ),
                                "title": s.fact.statement,
                                "detail": s.reasons.join(" · "),
                            })
                        })
                        .collect();
                    println!("{}", serde_json::Value::Array(rows));
                    return Ok(());
                }
                Some("show") => {
                    let Some(uid) = id.as_deref() else {
                        return Err(mecha_graph_core::Error::Other(
                            "shadow show takes a fact uid".into(),
                        ));
                    };
                    let q = mecha_graph_core::shadow::surfaced(&conn, 1000)?;
                    let Some(s) = q.iter().find(|s| s.fact.uid == uid) else {
                        return Err(mecha_graph_core::Error::Other(format!(
                            "{uid} is not in the surfaced queue (decided already, or no longer triggering)"
                        )));
                    };
                    println!("{}", s.fact.statement);
                    println!(
                        "
class:      {} · {}
confidence: {:.2}
since:      {}",
                        s.fact.extractor.as_deref().unwrap_or("?"),
                        s.fact.predicate,
                        s.fact.confidence,
                        &s.fact.ingested_at[..10.min(s.fact.ingested_at.len())],
                    );
                    println!("
why it surfaced:");
                    for r in &s.reasons {
                        println!("• {r}");
                    }
                    println!(
                        "
accept: a human stands behind it (tier → reviewed)
reject: it was never true (retracted; the class learns)"
                    );
                    return Ok(());
                }
                Some(other) => {
                    return Err(mecha_graph_core::Error::Other(format!(
                        "unknown shadow verb '{other}' — list, show, or flags"
                    )));
                }
                None => {}
            }
            if let Some(uid) = confirm {
                fact::confirm_shadow_fact(&conn, &uid)?;
                println!("confirmed {uid} — now reviewed");
            } else if let Some(uid) = refute {
                let reason = reason.as_deref().unwrap_or("refuted at review");
                fact::refute_shadow_fact(&conn, &uid, reason)?;
                println!("refuted {uid} — retracted as never true");
            } else {
                let (q, total) =
                    mecha_graph_core::shadow::surfaced_counted(&conn, limit)?;
                let (live, served) = mecha_graph_core::shadow::shadow_counts(&conn)?;
                if json {
                    // `surfaced_total` is the count BEFORE the limit: a
                    // consumer reporting queue depth must have the depth,
                    // not this page's length.
                    println!(
                        "{}",
                        serde_json::json!({
                            "surfaced": q, "surfaced_total": total,
                            "shadow_live": live, "shadow_served": served,
                        })
                    );
                } else {
                    if q.is_empty() {
                        println!("nothing surfaced — no shadow fact is about to matter");
                    }
                    for s in &q {
                        let class = format!(
                            "{}·{}",
                            s.fact.extractor.as_deref().unwrap_or("?"),
                            s.fact.predicate
                        );
                        println!("{}  [{}]", s.fact.statement, class);
                        for r in &s.reasons {
                            println!("    ↳ {r}");
                        }
                        println!(
                            "    confirm: pkg shadow --confirm {u} · refute: pkg shadow --refute {u} --reason '…'",
                            u = s.fact.uid
                        );
                    }
                    if total > q.len() {
                        println!(
                            "(showing {} of {total} surfaced — --limit raises the page)",
                            q.len()
                        );
                    }
                    println!("({live} live shadow facts, {served} ever served)");
                }
            }
        }

        Command::ShadowConvert { limit } => {
            let r = fact::convert_pending_to_shadow(&conn, limit)?;
            println!(
                "scanned {} · minted {} · held {} (commitments + flagged) · \
                 unresolvable {} (bind, then re-run)",
                r.scanned, r.minted, r.held, r.unresolvable
            );
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

        Command::RepairIdPayloads { dry_run } => {
            let r = mecha_graph_core::linkers::repair_node_id_payloads(&conn, dry_run)?;
            if dry_run {
                println!("(dry run — nothing changed)\n");
            }
            let moved: i64 = r.placeholders_merged.iter().map(|(_, _, n)| n).sum();
            for (dup, keep, facts) in &r.placeholders_merged {
                let carried = match facts {
                    0 => String::new(),
                    n => format!("  ({n} accepted fact(s) move with it)"),
                };
                println!("  placeholder {dup}  →  merged into {keep}{carried}");
            }
            for id in &r.placeholders_orphaned {
                println!("  placeholder {id}  →  names no live node, LEFT ALONE");
            }
            for id in &r.unresolvable {
                println!("  candidate #{id}  →  names no live node, still pending");
            }
            println!(
                "\n{} placeholder node(s) {}merged · {} orphaned\n\
                 {} of {} pending candidate(s) {}rewritten to names · {} unresolvable",
                r.placeholders_merged.len(),
                if dry_run { "would be " } else { "" },
                r.placeholders_orphaned.len(),
                r.payloads_repaired,
                r.candidates_scanned,
                if dry_run { "would be " } else { "" },
                r.unresolvable.len(),
            );
            println!("{moved} accepted fact(s) re-pointed at the real entity");
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
                // "new or worse", never a running total. The old line said
                // "N data-integrity alarm(s) — mentions lost" and printed
                // the same N every night for eighteen days, which is how a
                // permanent number came to look like a nightly finding.
                println!(
                    "\n⚑ {} NEW or worsened input-set collapse(s); beliefs left untouched:",
                    r.integrity_alarms.len()
                );
                for (stmt, detail) in r.integrity_alarms.iter().take(10) {
                    println!("  · {stmt}\n    {detail}");
                }
            }
            // Still true, still unresolved, and not news. Reported as a
            // number so it cannot be mistaken for either zero or a fresh
            // finding — a dash is never zero, and neither is a backlog.
            // Self-naming, because in the steady state this change is built
            // to produce — 0 new, N continuing — the `⚑` header above never
            // prints, and an indented parenthetical then dangles under the
            // scan counts with nothing saying what the number counts.
            if r.integrity_alarms_continuing > 0 {
                let lead = if r.integrity_alarms.is_empty() {
                    "\ninput-set collapses: "
                } else {
                    "  "
                };
                let age = match &r.integrity_alarms_oldest {
                    Some(t) => format!(", oldest first seen {t}"),
                    None => String::new(),
                };
                println!(
                    "{lead}{} already reported and no worse since — unchanged, \
                     not resolved (beliefs left untouched){age}",
                    r.integrity_alarms_continuing
                );
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

        Command::ProbeTargets { limit, include_cold, min_sources } => {
            // Filter before truncating to `limit`, so the filter does not
            // eat the caller's quota: without the deep fetch, a run asking
            // for 25 gets 25-minus-the-rejects.
            //
            // **It raises the ceiling; it does not guarantee `limit`.**
            // `probe_targets_opts` truncates to `deep` itself and draws from
            // a fixed top-200 demanded pool, so on a graph where most
            // demanded nodes are single-source, `--limit 25 --min-sources 2`
            // can still return fewer than 25 with no signal saying so. Not a
            // live concern at the nightly's 3, and stated because the
            // earlier wording promised more than the code delivers.
            // `> 0`, not `> 1`: `retain` runs for any positive floor, and
            // `sources == 0` is reachable — a node with retrieval touches
            // but no mention rows ranks and reports zero. At `--min-sources
            // 1` the old guard skipped the deep fetch and then filtered
            // anyway, which is exactly what the comment below says must not
            // happen. The condition was one off from its own comment.
            let deep = if min_sources > 0 { limit.saturating_mul(4).max(50) } else { limit };
            let mut targets =
                mecha_graph_core::probe::probe_targets_opts(&conn, deep, include_cold)?;
            targets.retain(|t| t.sources >= min_sources);
            targets.truncate(limit);
            if want_json(cli_json, cli_text) {
                println!("{}", serde_json::to_string_pretty(&targets)?);
            } else {
                for t in &targets {
                    // `sources` in text too: it is the field `--min-sources`
                    // filters on, and it was visible only under `--json` —
                    // so an operator asking why a node was dropped had to
                    // re-run in another format to see the reason.
                    println!(
                        "{:6.1}  {} ({}) · {} touches · {} source(s) · missing: [{}] · stale: [{}]",
                        t.score, t.name, t.node_type, t.touches, t.sources,
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

        Command::Extract { limit, model, source, exclude_source, episode } => {
            let chat = mecha_graph_core::llm::ChatClient::connect(&model)?;
            let report = if let Some(ep) = episode {
                mecha_graph_core::extract::reextract_episode(&conn, &chat, &ep)?
            } else {
                let sources: Vec<&str> = source.iter().map(|s| s.as_str()).collect();
                let excluded: Vec<&str> = exclude_source.iter().map(|s| s.as_str()).collect();
                mecha_graph_core::extract::extract_pending(
                    &conn,
                    &chat,
                    limit,
                    (!sources.is_empty()).then_some(&sources[..]),
                    (!excluded.is_empty()).then_some(&excluded[..]),
                )?
            };
            println!(
                "extracted {} episodes → {} mentions, {} fact candidates, {} commitments ({} errors)",
                report.episodes, report.mentions, report.fact_candidates,
                report.commitment_candidates, report.errors
            );
            // The generation gate must be loud on every run it acts.
            if !report.gated.is_empty() {
                println!(
                    "GATED for llm — not extracted: {}",
                    report.gated.join(" · ")
                );
            }
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
            let gold = gold.unwrap_or_else(eval::default_gold_path);
            // Named, because "no such file" on a path the user never typed is
            // the confusing half of moving a default out of the repo.
            if !gold.exists() {
                return Err(mecha_graph_core::error::Error::Other(format!(
                    "no gold set at {} — it lives outside the repo, because it is mined \
                     from real episodes. Pass --gold, or set MECHA_GRAPH_GOLD.",
                    gold.display()
                )));
            }
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

    /// The draw is uniform, and that is the whole reason it exists.
    ///
    /// Reviewing the head of the queue and reading the result as a class's
    /// accept rate measures the *ordering* — age, id, whatever the index
    /// returns first. This test would fail on the obvious wrong
    /// implementation (`items.truncate(k)`), which is exactly the bias the
    /// flag is there to escape.
    #[test]
    fn the_sample_is_uniform_over_the_whole_population() {
        const N: usize = 50;
        const K: usize = 5;
        const DRAWS: usize = 4000;
        let mut seen = vec![0usize; N];
        for seed in 0..DRAWS as u64 {
            let mut pop: Vec<usize> = (0..N).collect();
            draw_sample(&mut pop, K, seed.wrapping_mul(0x2545_F491_4F6C_DD1D));
            assert_eq!(pop.len(), K, "a draw returns exactly k");
            let uniq: std::collections::HashSet<_> = pop.iter().collect();
            assert_eq!(uniq.len(), K, "and never the same item twice");
            for i in pop {
                seen[i] += 1;
            }
        }
        // Every element should appear about DRAWS*K/N times. A truncating
        // implementation leaves the tail at zero, which this catches with
        // enormous margin; the band is wide enough not to be flaky.
        let expected = (DRAWS * K / N) as f64;
        for (i, &count) in seen.iter().enumerate() {
            let ratio = count as f64 / expected;
            assert!(
                (0.75..1.25).contains(&ratio),
                "element {i} drawn {count} times, expected ~{expected:.0}"
            );
        }
    }

    /// A seed reproduces its draw exactly, and different seeds differ.
    ///
    /// Without this a sample is a number nobody can check — and the samples
    /// this feature exists for are the ones somebody will quote as a class's
    /// accept rate.
    #[test]
    fn a_seed_reproduces_its_draw() {
        let draw = |seed: u64| {
            let mut pop: Vec<usize> = (0..200).collect();
            draw_sample(&mut pop, 12, seed);
            pop
        };
        assert_eq!(draw(42), draw(42), "same seed, same sample");
        assert_ne!(draw(42), draw(43), "different seed, different sample");
    }

    /// Asking for more than exists returns everything, not a panic and not a
    /// short draw padded from somewhere.
    #[test]
    fn a_sample_larger_than_the_class_is_the_whole_class() {
        let mut pop: Vec<usize> = (0..3).collect();
        draw_sample(&mut pop, 10, 1);
        assert_eq!(pop, vec![0, 1, 2]);
        let mut empty: Vec<usize> = vec![];
        draw_sample(&mut empty, 5, 1);
        assert!(empty.is_empty());
    }
}
