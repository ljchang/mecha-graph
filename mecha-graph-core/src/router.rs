//! Query router (§8.1): filter first, rank second.
//!
//! 1. Entity detection — scan node_alias/node_identifier against the string
//!    (deterministic, sub-ms, NOT an LLM)
//! 2. Time extraction — "last", "in March" → range or ORDER BY
//! 3. Intent classify — LOOKUP | RECALL | AGGREGATE
//! 4. Dispatch: LOOKUP → person_interaction (no embeddings at all);
//!    RECALL → mention-constrained hybrid RRF; AGGREGATE → GROUP BY.
//!
//! Ambiguity is a feature: "June" matching three people returns the
//! disambiguation, not a silent guess.

use crate::embed::OllamaEmbedder;
use crate::episode;
use crate::error::Result;
use crate::graph;
use crate::rollup;
use crate::search;
use chrono::{Datelike, Utc};
use rusqlite::{params, Connection};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub enum Intent {
    Lookup,
    Recall,
    Aggregate,
}

#[derive(Debug, Clone, Serialize)]
pub struct DetectedEntity {
    pub node_id: String,
    pub name: String,
    pub node_type: String,
    pub matched: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct AmbiguousEntity {
    pub matched: String,
    pub candidates: Vec<AmbiguousCandidate>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AmbiguousCandidate {
    pub node_id: String,
    pub name: String,
    pub last_seen: Option<String>,
    pub interaction_count: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PackItem {
    pub kind: String, // episode|fact|person_interaction|node
    pub id: String,
    pub score: f64,
    pub occurred_at: Option<String>,
    pub valid_from: Option<String>,
    pub source: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    pub text: String,
}

/// The deliverable (§1): a token-bounded, provenance-carrying,
/// freshness-stamped slice. Versioned envelope, not prose (§9.1).
#[derive(Debug, Clone, Serialize)]
pub struct ContextPack {
    pub v: u32,
    pub query: String,
    pub intent: Intent,
    pub entities: Vec<DetectedEntity>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub ambiguous: Vec<AmbiguousEntity>,
    pub items: Vec<PackItem>,
    pub truncated: bool,
    pub budget_tokens: usize,
    pub generated_at: String,
    /// Retrieval scope this pack was built under (V013 wiring): a
    /// verifier must know whether an answer COULD have seen facts or
    /// evidence. Omitted in JSON for the default 'both'.
    #[serde(skip_serializing_if = "Scope::is_both")]
    pub scope: Scope,
    /// Sources this pack was restricted to, echoed like `tags` so a
    /// consumer (or a verifier) knows what it could NOT have seen.
    /// Omitted when unrestricted.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<String>,
    /// Time window this pack was restricted to, echoed so a comparison
    /// can prove both readers were shown the same era.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<TimeRange>,
    /// Point-of-use problems in what this pack serves (≤2, ranked by
    /// expected loss). pkg detects; the consumer judges. See [`crate::flags`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub flags: Vec<crate::flags::PackFlag>,
}

/// Which halves of the graph a retrieval may draw on (PLAN.md: the
/// gossip Answerers' blind split — A sees facts_only, B evidence_only).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    #[default]
    Both,
    FactsOnly,
    EvidenceOnly,
}

/// What a retrieval is allowed to see: which halves of the graph, and
/// which sources within them. **The two are orthogonal on purpose** —
/// `scope` splits a distillation from its origin, `sources` splits
/// independent observations of the world.
///
/// That distinction is the correction the 2026-08-13 probe run earned.
/// Facts are derived FROM episodes, so a facts-vs-evidence pair is never
/// two witnesses and its disagreements were uninformative. Two sources
/// ARE two witnesses: a calendar entry is a plan, a Bee transcript is
/// what was said, a Slack thread is what was written. A lens is what one
/// agent gets handed, and giving two agents different lenses is what
/// makes a disagreement between them mean something.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Lens {
    pub scope: Scope,
    /// Empty = every source. Validated by the caller against
    /// [`crate::search::known_sources`]; a typo must not read as silence.
    pub sources: Vec<String>,
    /// Explicit time window, overriding whatever the query text implies.
    ///
    /// **Comparing two sources without one compares two eras.** The
    /// calendar's evidence about a person can span 2017–2022 while Bee's
    /// spans 2024–2026; a difference between those is the world having
    /// moved, not the sources conflicting. Handing both readers the same
    /// window is what makes a disagreement mean "these cannot both be
    /// true" rather than "one of us is reading older mail" — the same
    /// distinction the write side draws between `close_valid_time` and
    /// `invalidate_never_true`.
    pub window: Option<TimeRange>,
}

impl Lens {
    pub fn scoped(scope: Scope) -> Self {
        Lens {
            scope,
            ..Default::default()
        }
    }
    pub fn from_sources(sources: Vec<String>) -> Self {
        Lens {
            sources,
            ..Default::default()
        }
    }
    /// The same window for every reader — the constructor a comparison
    /// should use, so the window cannot be forgotten on one side.
    pub fn windowed(sources: Vec<String>, from: Option<String>, to: Option<String>) -> Self {
        Lens {
            sources,
            window: Some(TimeRange { from, to }),
            ..Default::default()
        }
    }
    fn is_default(&self) -> bool {
        self.scope == Scope::Both && self.sources.is_empty() && self.window.is_none()
    }
}

impl Scope {
    pub fn facts(self) -> bool {
        self != Scope::EvidenceOnly
    }
    pub fn evidence(self) -> bool {
        self != Scope::FactsOnly
    }
    fn is_both(&self) -> bool {
        *self == Scope::Both
    }
    /// Parse a user/agent-supplied scope string; None on junk.
    pub fn parse(s: &str) -> Option<Scope> {
        match s {
            "both" => Some(Scope::Both),
            "facts" | "facts_only" => Some(Scope::FactsOnly),
            "evidence" | "evidence_only" => Some(Scope::EvidenceOnly),
            _ => None,
        }
    }
}

// ─── 1. Entity detection ─────────────────────────────────────────────────────

const STOPWORDS: &[&str] = &[
    "when",
    "did",
    "last",
    "meet",
    "with",
    "what",
    "who",
    "the",
    "about",
    "and",
    "for",
    "was",
    "were",
    "does",
    "how",
    "many",
    "much",
    "that",
    "this",
    "have",
    "has",
    "say",
    "said",
    "tell",
    "told",
    "from",
    "week",
    "month",
    "year",
    "today",
    "yesterday",
    "time",
];

/// Deterministic entity detection: known aliases scanned against the query
/// string via SQL `instr` on the indexed alias table — sub-ms, not an LLM.
pub fn detect_entities(
    conn: &Connection,
    query: &str,
) -> Result<(Vec<DetectedEntity>, Vec<AmbiguousEntity>)> {
    // Punctuation → spaces so "June?" still hits alias "june" at a boundary.
    let normalized: String = query
        .to_lowercase()
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == '@' {
                c
            } else {
                ' '
            }
        })
        .collect();
    let q = format!(
        " {} ",
        normalized.split_whitespace().collect::<Vec<_>>().join(" ")
    );

    // Find all alias substrings present in the query (word-boundary padded).
    // Event/document/artifact nodes are retrieval TARGETS, not query anchors —
    // a recurring event named "Meet with Iris" must not shadow the person
    // Iris in "when did I last meet with Iris?".
    let mut stmt = conn.prepare_cached(
        "SELECT DISTINCT a.alias, a.node_id, n.name, n.node_type
         FROM node_alias a JOIN nodes n ON n.id = a.node_id
         WHERE length(a.alias) >= 3 AND instr(?1, ' ' || a.alias || ' ') > 0
           AND n.node_type NOT IN ('event','event_series','document','artifact')
         UNION
         SELECT DISTINCT n.canonical_name, n.id, n.name, n.node_type
         FROM nodes n
         WHERE length(n.canonical_name) >= 3
           AND instr(?1, ' ' || n.canonical_name || ' ') > 0
           AND n.node_type NOT IN ('event','event_series','document','artifact')",
    )?;
    let rows: Vec<(String, String, String, String)> = stmt
        .query_map(params![q], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<std::result::Result<_, _>>()?;

    // Group by matched string; prefer the longest matches (drop matches fully
    // contained in a longer one, e.g. "june" inside "june chen").
    let mut by_match: std::collections::HashMap<String, Vec<(String, String, String)>> =
        std::collections::HashMap::new();
    for (alias, node_id, name, node_type) in rows {
        if STOPWORDS.contains(&alias.as_str()) {
            continue;
        }
        by_match
            .entry(alias)
            .or_default()
            .push((node_id, name, node_type));
    }
    let matches: Vec<String> = by_match.keys().cloned().collect();
    let contained: std::collections::HashSet<String> = matches
        .iter()
        .filter(|m| matches.iter().any(|o| o != *m && o.contains(m.as_str())))
        .cloned()
        .collect();

    let mut entities = Vec::new();
    let mut ambiguous = Vec::new();
    for (matched, mut nodes) in by_match {
        if contained.contains(&matched) {
            continue;
        }
        nodes.sort();
        nodes.dedup();
        if nodes.len() == 1 {
            let (node_id, name, node_type) = nodes.pop().unwrap();
            entities.push(DetectedEntity {
                node_id,
                name,
                node_type,
                matched,
            });
        } else {
            let mut candidates = Vec::new();
            for (node_id, name, _t) in nodes {
                let pi = rollup::get_person_interaction(conn, &node_id)?;
                candidates.push(AmbiguousCandidate {
                    node_id,
                    name,
                    last_seen: pi.as_ref().and_then(|p| p.last_seen_at.clone()),
                    interaction_count: pi.map(|p| p.interaction_count).unwrap_or(0),
                });
            }
            candidates.sort_by(|a, b| b.interaction_count.cmp(&a.interaction_count));
            ambiguous.push(AmbiguousEntity {
                matched,
                candidates,
            });
        }
    }
    Ok((entities, ambiguous))
}

/// The statement's own leading noun phrase, for claims that name nothing
/// the graph knows. "The gutter cleaning service is scheduled for July 6"
/// is a claim about the gutter cleaning service; a topic-shaped subject
/// beats an empty one, and it must NOT default to the graph's owner —
/// owner-default attribution is the wearable's failure mode, not a repair.
///
/// Deliberately dumb: strip one leading article, take words up to the first
/// verb-ish token, cap at four, refuse pronouns. Returns None unless a verb
/// was actually found — a fragment with no predicate has no subject.
pub fn subject_phrase(text: &str) -> Option<String> {
    const VERBS: &[&str] = &[
        "is", "are", "was", "were", "has", "have", "had", "will", "would", "should", "can",
        "could", "may", "might", "must", "does", "do", "did", "needs", "went", "goes", "comes",
        "came", "includes", "involves", "requires", "remains", "became", "becomes", "seems",
        "appears", "gets", "got",
    ];
    const PRONOUNS: &[&str] = &[
        "he", "she", "they", "it", "we", "i", "you", "there", "this", "that", "these", "those",
        "someone", "somebody", "everyone",
    ];
    let mut words = text.split_whitespace().peekable();
    if let Some(&first) = words.peek() {
        let f = first.to_lowercase();
        if f == "the" || f == "a" || f == "an" {
            words.next();
        }
    }
    let mut phrase: Vec<String> = Vec::new();
    let mut hit_verb = false;
    for w in words {
        let bare: String = w
            .chars()
            .filter(|c| c.is_alphanumeric() || *c == '-' || *c == '\'')
            .collect();
        let lower = bare.to_lowercase();
        if VERBS.contains(&lower.as_str()) {
            hit_verb = true;
            break;
        }
        if phrase.is_empty() && PRONOUNS.contains(&lower.as_str()) {
            return None;
        }
        if bare.is_empty() || phrase.len() == 4 {
            break;
        }
        phrase.push(bare);
    }
    (hit_verb && !phrase.is_empty())
        .then(|| phrase.join(" "))
        .filter(|p| p.len() >= 3)
}

#[cfg(test)]
mod subject_phrase_tests {
    use super::subject_phrase;

    #[test]
    fn a_noun_phrase_is_found_or_refused() {
        assert_eq!(
            subject_phrase("The gutter cleaning service is scheduled for July 6.").as_deref(),
            Some("gutter cleaning service")
        );
        assert_eq!(
            subject_phrase("A water filtration system needs replacing.").as_deref(),
            Some("water filtration system")
        );
        // Pronouns are not subjects — they are exactly the diarization trap.
        assert_eq!(subject_phrase("He is planning a trip."), None);
        assert_eq!(subject_phrase("It was a long day."), None);
        // No verb in reach → no subject-predicate shape → nothing.
        assert_eq!(
            subject_phrase("Miscellaneous household notes and errands list"),
            None
        );
        // A five-word runway without a verb gives up rather than guessing.
        assert_eq!(
            subject_phrase("Big old rusty red garden tractor is broken"),
            None
        );
    }
}

// ─── 2. Time extraction ──────────────────────────────────────────────────────

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct TimeRange {
    pub from: Option<String>,
    pub to: Option<String>,
}

const MONTHS: &[(&str, u32)] = &[
    ("january", 1),
    ("february", 2),
    ("march", 3),
    ("april", 4),
    ("may", 5),
    ("june", 6),
    ("july", 7),
    ("august", 8),
    ("september", 9),
    ("october", 10),
    ("november", 11),
    ("december", 12),
];

/// Lightweight time extraction: month names, years, "yesterday", "last week/month".
pub fn extract_time(query: &str) -> Option<TimeRange> {
    let q = query.to_lowercase();
    let now = Utc::now();

    if q.contains("yesterday") {
        let d = now.date_naive() - chrono::Duration::days(1);
        return Some(TimeRange {
            from: Some(format!("{d} 00:00:00")),
            to: Some(format!("{d} 23:59:59")),
        });
    }
    if q.contains("last week") {
        let d = now.date_naive() - chrono::Duration::days(7);
        return Some(TimeRange {
            from: Some(format!("{d} 00:00:00")),
            to: None,
        });
    }
    if q.contains("last month") {
        let d = now.date_naive() - chrono::Duration::days(31);
        return Some(TimeRange {
            from: Some(format!("{d} 00:00:00")),
            to: None,
        });
    }

    // Explicit year in the query, e.g. "2025".
    let year: Option<i32> = q
        .split(|c: char| !c.is_ascii_digit())
        .filter(|t| t.len() == 4)
        .filter_map(|t| t.parse().ok())
        .find(|y| (2000..=2100).contains(y));

    for (name, m) in MONTHS {
        if q.contains(&format!("in {name}")) || q.contains(&format!("during {name}")) {
            let y = year.unwrap_or_else(|| {
                // Most recent occurrence of that month.
                if *m > now.month() {
                    now.year() - 1
                } else {
                    now.year()
                }
            });
            let from = format!("{y:04}-{m:02}-01 00:00:00");
            let (ny, nm) = if *m == 12 { (y + 1, 1) } else { (y, m + 1) };
            let to = format!("{ny:04}-{nm:02}-01 00:00:00");
            return Some(TimeRange {
                from: Some(from),
                to: Some(to),
            });
        }
    }
    if let Some(y) = year {
        return Some(TimeRange {
            from: Some(format!("{y:04}-01-01 00:00:00")),
            to: Some(format!("{:04}-01-01 00:00:00", y + 1)),
        });
    }
    None
}

// ─── 2b. Tag extraction ──────────────────────────────────────────────────────

/// Pull explicit `#tag` tokens out of a query. Returns the canonicalized tags
/// (lowercase, `#` stripped — matching `annotate_episode`) and the query with
/// those tokens removed, so a tag never leaks into BM25 terms, entity
/// detection, or the embedding. `#` alone and `#123`-style fragments with no
/// letters are left in place.
pub fn extract_tags(query: &str) -> (Vec<String>, String) {
    let mut tags = vec![];
    let mut rest = vec![];
    for token in query.split_whitespace() {
        let stripped = token.trim_start_matches('#');
        let is_tag = token.starts_with('#') && stripped.chars().any(|c| c.is_alphabetic());
        if is_tag {
            let tag = stripped
                .trim_end_matches(|c: char| !c.is_alphanumeric())
                .to_lowercase();
            if !tag.is_empty() && !tags.contains(&tag) {
                tags.push(tag);
            }
        } else {
            rest.push(token);
        }
    }
    (tags, rest.join(" "))
}

// ─── 3. Intent classification ────────────────────────────────────────────────

pub fn classify_intent(query: &str) -> Intent {
    let q = query.to_lowercase();
    let lookup_markers = [
        "when did i last",
        "when was the last",
        "last time i",
        "last meet",
        "last spoke",
        "last talk",
        "last email",
        "how long since",
    ];
    if lookup_markers.iter().any(|m| q.contains(m)) {
        return Intent::Lookup;
    }
    // "the most" alone would misroute recall queries ("the meeting I enjoyed
    // the most"); require the verb-attached forms.
    let aggregate_markers = [
        "how many",
        "how much",
        "how often",
        "count ",
        "per week",
        "per month",
        "trend",
        "most frequent",
        "which projects",
        "stalled",
        "with the most",
        "to the most",
        "most often",
        "most interactions",
        "most time with",
    ];
    if aggregate_markers.iter().any(|m| q.contains(m)) {
        return Intent::Aggregate;
    }
    Intent::Recall
}

// ─── 4. Dispatch ─────────────────────────────────────────────────────────────

fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Which per-channel recency column does a LOOKUP query want? "met/meet/saw"
/// means calendar or co-presence, not email (§4.6).
fn lookup_column(query: &str) -> &'static str {
    let q = query.to_lowercase();
    if q.contains("meet") || q.contains("met ") || q.contains("saw") || q.contains("meeting") {
        "meeting_or_spoken"
    } else if q.contains("email") {
        "last_email_at"
    } else if q.contains("spoke") || q.contains("talk") || q.contains("conversation") {
        "last_spoken_at"
    } else if q.contains("slack") {
        "last_slack_at"
    } else if q.contains("text") || q.contains("message") {
        "last_message_at"
    } else {
        "last_seen_at"
    }
}

/// Run the full router. Returns a token-bounded ContextPack.
///
/// `tool` names the caller in the query ledger (`cli.query`, `tui.search`,
/// `mcp.kg_search`); `None` skips the ledger entirely — the eval path, whose
/// repeated gold queries would corrupt the demand signal (V009).
#[allow(clippy::too_many_arguments)]
pub fn query(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    query_str: &str,
    k: usize,
    budget_tokens: usize,
    include_private: bool,
    tool: Option<&str>,
) -> Result<ContextPack> {
    query_lens(
        conn,
        embedder,
        query_str,
        k,
        budget_tokens,
        include_private,
        tool,
        Lens::default(),
    )
}

/// [`query`] restricted to a [`Scope`]. Retained for callers that only
/// need the facts/evidence split; [`query_lens`] is the general form.
#[allow(clippy::too_many_arguments)]
pub fn query_scoped(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    query_str: &str,
    k: usize,
    budget_tokens: usize,
    include_private: bool,
    tool: Option<&str>,
    scope: Scope,
) -> Result<ContextPack> {
    query_lens(
        conn,
        embedder,
        query_str,
        k,
        budget_tokens,
        include_private,
        tool,
        Lens::scoped(scope),
    )
}

/// [`query`] through an explicit [`Lens`] — the general form. `scope`
/// splits facts from evidence (aggregates are rollup-derived, so they
/// count as facts); `sources` restricts which observations are visible,
/// which is how two agents are given genuinely different evidence.
#[allow(clippy::too_many_arguments)]
pub fn query_lens(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    query_str: &str,
    k: usize,
    budget_tokens: usize,
    include_private: bool,
    tool: Option<&str>,
    lens: Lens,
) -> Result<ContextPack> {
    let scope = lens.scope;
    // A lensed retrieval is probe-shaped, not organic demand. Recording
    // it would corrupt retrieval_touch and loop the Selector's demand
    // ranking back into itself (probing a node makes it look demanded →
    // probed more) — the V009 eval rule, applied again. Source-scoped
    // reads count here too: an agent given only Bee must not teach the
    // Selector that Bee's people are what the owner asks about.
    let tool = if lens.is_default() { tool } else { None };

    // `#tag` tokens are filters, not search terms: strip them before entity
    // detection, time extraction, and ranking ever see the string. Only
    // vocabulary tags filter — an unknown `#foo` (a Slack channel, a typo)
    // goes back into the text query rather than matching nothing.
    let (candidate_tags, mut stripped) = extract_tags(query_str);
    let mut tags = vec![];
    for t in candidate_tags {
        if episode::tag_exists(conn, &t)? {
            tags.push(t);
        } else if stripped.is_empty() {
            stripped = t;
        } else {
            stripped = format!("{stripped} {t}");
        }
    }
    let text_query = stripped.as_str();

    let (entities, ambiguous) = detect_entities(conn, text_query)?;
    // An explicit window overrides the query text: a probe that says
    // "in June" must not have its window quietly widened by a phrase in
    // the question, and both readers in a comparison must get the same
    // one or the comparison is between eras.
    let time = match &lens.window {
        Some(w) => Some(w.clone()),
        None => extract_time(text_query),
    };
    let intent = classify_intent(text_query);

    let mut pack = ContextPack {
        v: 1,
        query: query_str.to_string(),
        intent,
        entities: entities.clone(),
        tags: tags.clone(),
        ambiguous,
        items: vec![],
        truncated: false,
        budget_tokens,
        generated_at: crate::ids::now(),
        scope,
        sources: lens.sources.clone(),
        window: lens.window.clone(),
        flags: vec![],
    };

    // Blocked on a "which June?" — return the disambiguation immediately
    // (§11.2: resolve at the point of use).
    if !pack.ambiguous.is_empty() && entities.is_empty() {
        // Ambiguity IS the interesting gap — record it (best-effort; V009).
        if let Some(t) = tool {
            let _ = crate::ledger::record(conn, t, &pack);
        }
        return Ok(pack);
    }

    match intent {
        Intent::Lookup if scope.facts() => {
            // Structured lookup — no embeddings at all (§8.1). The
            // rollup row is a derived fact, so evidence_only skips it
            // and answers from episodes below.
            for e in &entities {
                if let Some(pi) = rollup::get_person_interaction(conn, &e.node_id)? {
                    let col = lookup_column(query_str);
                    let (label, value) = match col {
                        "meeting_or_spoken" => {
                            let best = [&pi.last_meeting_at, &pi.last_spoken_at]
                                .into_iter()
                                .flatten()
                                .max()
                                .cloned();
                            ("last met (calendar/co-presence)", best)
                        }
                        "last_email_at" => ("last email", pi.last_email_at.clone()),
                        "last_spoken_at" => ("last spoken", pi.last_spoken_at.clone()),
                        "last_slack_at" => ("last slack", pi.last_slack_at.clone()),
                        "last_message_at" => ("last message", pi.last_message_at.clone()),
                        _ => ("last seen", pi.last_seen_at.clone()),
                    };
                    let text = match &value {
                        Some(v) => format!(
                            "{}: {} — {} ({} interactions total, last channel {})",
                            e.name,
                            label,
                            v,
                            pi.interaction_count,
                            pi.last_channel.as_deref().unwrap_or("?")
                        ),
                        None => format!("{}: no recorded {}", e.name, label),
                    };
                    pack.items.push(PackItem {
                        kind: "person_interaction".into(),
                        id: e.node_id.clone(),
                        score: 1.0,
                        occurred_at: value,
                        valid_from: None,
                        source: Some("person_interaction".into()),
                        tags: vec![],
                        text,
                    });
                    graph::increment_node_access(conn, &e.node_id)?;
                }
            }
            // Fall through to a small recall supplement if nothing matched.
            if pack.items.is_empty() {
                recall_into(
                    conn,
                    embedder,
                    text_query,
                    &tags,
                    &entities,
                    &time,
                    k,
                    include_private,
                    scope,
                    &lens.sources,
                    &mut pack,
                )?;
            }
        }
        Intent::Aggregate if scope.facts() => {
            // Aggregates are rollup-derived — they count as facts.
            aggregate_into(conn, text_query, &entities, &time, k, &mut pack)?;
        }
        _ => {
            // Recall, plus Lookup/Aggregate under evidence_only.
            recall_into(
                conn,
                embedder,
                text_query,
                &tags,
                &entities,
                &time,
                k,
                include_private,
                scope,
                &lens.sources,
                &mut pack,
            )?;
        }
    }

    // Rank across kinds: facts and episodes carry the same RRF scale, so a
    // strongly-matched episode must outrank weakly-matched facts — pushing all
    // facts first let ten mediocre facts bury the right episode below
    // recall@10. Stable, so equal scores keep push order (facts, then
    // episodes; fallback recency).
    pack.items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
    });

    // Token budget: dedupe then truncate (§8.3).
    let mut seen_texts = std::collections::HashSet::new();
    pack.items
        .retain(|i| seen_texts.insert(crate::ids::content_hash(&i.text)));
    let mut spent = 0usize;
    let budget = budget_tokens;
    let before = pack.items.len();
    pack.items.retain(|i| {
        spent += estimate_tokens(&i.text);
        spent <= budget
    });
    if pack.items.len() < before {
        pack.truncated = true;
    }

    // Point-of-use flags over what is actually served (after ranking and
    // truncation). Best-effort like the ledger: surfacing must never break
    // retrieval. Scoped (probe) queries still get flags — the Verifier
    // wants them — but only real tools write flag_shown events below.
    let _ = crate::flags::flag_pack(conn, &mut pack, include_private);

    // Ledger + demand bump (V009). Best-effort: telemetry must never break
    // retrieval, so the Result is deliberately dropped.
    if let Some(t) = tool {
        let _ = crate::ledger::record(conn, t, &pack);
        // flag_shown events: the denominator of the flag-actioned ratio
        // (RESEARCH_LOOP observability) — event_log's first real writer.
        for f in &pack.flags {
            let payload = serde_json::json!({
                "kind": f.kind, "subject_id": f.subject_id,
                "predicate": f.predicate, "fact_uids": f.fact_uids,
                "tool": t,
            });
            let _ = crate::ledger::log_event(
                conn,
                "flag_shown",
                Some(&f.subject_id),
                Some(&payload.to_string()),
            );
        }
    }

    Ok(pack)
}

/// ACT-R arm weight. RRF rank-gaps near the top run ~0.0003 (1/61 − 1/62),
/// so a typical hot item (A ≈ 1–2) moves one or two ranks and the clamp
/// (±[-2,4] in `ledger::activation`) bounds any item to a few ranks either
/// way — a tie-break, never a takeover. Deliberately low: the rich-get-
/// richer feedback loop is real (ACT-R itself adds noise for this reason),
/// and the write side is already guarded (eval and scoped probes never
/// bump touches).
const ACT_WEIGHT: f64 = 0.0004;

fn touch_nudge(conn: &Connection, kind: &str, uid: &str) -> f64 {
    ACT_WEIGHT
        * crate::ledger::activation(conn, kind, uid)
            .ok()
            .flatten()
            .unwrap_or(0.0)
}

#[allow(clippy::too_many_arguments)]
fn recall_into(
    conn: &Connection,
    embedder: Option<&OllamaEmbedder>,
    query_str: &str,
    tags: &[String],
    entities: &[DetectedEntity],
    time: &Option<TimeRange>,
    k: usize,
    include_private: bool,
    scope: Scope,
    sources: &[String],
    pack: &mut ContextPack,
) -> Result<()> {
    let entity_filter = entities.first().map(|e| e.node_id.as_str());
    let has_text = !query_str.trim().is_empty();

    // Tag filter: episode ids carrying ALL requested tags (intersection).
    // Some(vec![]) is a real result — a tag nobody used matches nothing,
    // it does not mean "unfiltered".
    let tag_cands: Option<Vec<i64>> = if tags.is_empty() {
        None
    } else {
        let mut iter = tags.iter();
        let mut set: std::collections::HashSet<i64> =
            episode::episode_ids_with_tag(conn, iter.next().unwrap())?
                .into_iter()
                .collect();
        for t in iter {
            let next: std::collections::HashSet<i64> = episode::episode_ids_with_tag(conn, t)?
                .into_iter()
                .collect();
            set.retain(|i| next.contains(i));
        }
        Some(set.into_iter().collect())
    };

    // Source filter: the candidate collapse that lets two agents read
    // genuinely different evidence. Like tags, `Some(vec![])` is a real
    // (empty) result rather than "unfiltered" — a source with no episodes
    // must return nothing, not everything.
    let source_cands: Option<Vec<i64>> = if sources.is_empty() {
        None
    } else {
        Some(search::source_candidates(conn, sources)?)
    };
    // Intersected with tags for the fallback path, which takes one set.
    let anchor_cands: Option<Vec<i64>> = match (&tag_cands, &source_cands) {
        (Some(t), Some(s)) => {
            let sset: std::collections::HashSet<i64> = s.iter().copied().collect();
            Some(t.iter().copied().filter(|i| sset.contains(i)).collect())
        }
        (Some(t), None) => Some(t.clone()),
        (None, Some(s)) => Some(s.clone()),
        (None, None) => None,
    };

    // A tag- or time-only query has no text for either ranking arm — don't
    // embed the empty string, go straight to the anchored fallback.
    // (Episode-serving, so facts_only gets nothing here — correctly.)
    if !has_text {
        if scope.evidence() {
            anchored_fallback(conn, &anchor_cands, time, k, include_private, pack)?;
        }
        return Ok(());
    }

    // Facts first — they're distilled and cheap. Deliberately NOT
    // entity-filtered, unlike episodes: a fact's only entity linkage is
    // subject/object, which is too narrow — cross-entity questions
    // ("what app did I build for X's defense?") are answered by facts
    // that never reference X. Tried and reverted 2026-08-12: the hard
    // filter cost a gold-set recall miss; the eval guard caught it.
    let fact_hits = if scope.facts() {
        search::hybrid_facts(conn, embedder, query_str, include_private, k.min(10))?
    } else {
        vec![]
    };
    for hit in fact_hits {
        if let Some(f) = conn
            .query_row(
                "SELECT * FROM fact WHERE id = ?1",
                params![hit.id],
                crate::fact::row_to_fact,
            )
            .ok()
        {
            let text = match &f.valid_from {
                Some(v) => format!("as of {}: {}", v, f.statement),
                None => f.statement.clone(),
            };
            let nudge = touch_nudge(conn, "fact", &f.uid);
            pack.items.push(PackItem {
                kind: "fact".into(),
                id: f.uid,
                score: hit.score + nudge,
                occurred_at: None,
                valid_from: f.valid_from,
                source: f.extractor,
                tags: vec![],
                text,
            });
        }
    }

    let hits = if scope.evidence() {
        search::hybrid_episodes(
            conn,
            embedder,
            query_str,
            entity_filter,
            tag_cands.as_deref(),
            source_cands.as_deref(),
            include_private,
            k,
        )?
    } else {
        vec![]
    };
    for hit in hits {
        let Some(ep) = episode::get_episode(conn, hit.id)? else {
            continue;
        };
        // Time filter, when the query carried one.
        if let Some(t) = time {
            if let Some(from) = &t.from {
                if ep.occurred_at < *from {
                    continue;
                }
            }
            if let Some(to) = &t.to {
                if ep.occurred_at > *to {
                    continue;
                }
            }
        }
        let preview: String = ep.body.chars().take(700).collect();
        let nudge = touch_nudge(conn, "episode", &ep.uid);
        pack.items.push(PackItem {
            kind: "episode".into(),
            id: ep.uid,
            score: hit.score + nudge,
            occurred_at: Some(ep.occurred_at),
            valid_from: None,
            source: Some(ep.source),
            tags: episode::tags_for(conn, hit.id)?,
            text: preview,
        });
    }

    // Anchored fallback: the query carried a tag or time anchor but ranking
    // produced no episodes (e.g. the distinctive text appears in none of the
    // tagged episodes' bodies).
    let no_episodes = !pack.items.iter().any(|i| i.kind == "episode");
    if no_episodes {
        anchored_fallback(conn, &anchor_cands, time, k, include_private, pack)?;
    }
    Ok(())
}

/// Serve tag- and/or time-anchored episodes directly, newest first — for
/// queries with no rankable text ("#recommendation", "what happened
/// yesterday?") and as the fallback when ranking comes up empty. A tag set is
/// human-scale, so the tag arm filters in Rust; time-only keeps the SQL path.
fn anchored_fallback(
    conn: &Connection,
    tag_cands: &Option<Vec<i64>>,
    time: &Option<TimeRange>,
    k: usize,
    include_private: bool,
    pack: &mut ContextPack,
) -> Result<()> {
    let in_range = |occurred_at: &str, t: &TimeRange| {
        t.from.as_deref().is_none_or(|f| occurred_at >= f)
            && t.to.as_deref().is_none_or(|to| occurred_at < to)
    };

    let mut eps: Vec<crate::episode::Episode> = match (tag_cands, time) {
        (Some(ids), _) => {
            let mut eps = vec![];
            for id in ids {
                let Some(ep) = episode::get_episode(conn, *id)? else {
                    continue;
                };
                if !include_private && !matches!(ep.sensitivity.as_str(), "public" | "personal") {
                    continue;
                }
                if let Some(t) = time {
                    if !in_range(&ep.occurred_at, t) {
                        continue;
                    }
                }
                eps.push(ep);
            }
            eps
        }
        (None, Some(t)) => {
            let sens = if include_private {
                "('public','personal','private','secret')"
            } else {
                "('public','personal')"
            };
            let sql = format!(
                "SELECT * FROM episode
                 WHERE occurred_at >= ?1 AND occurred_at < ?2 AND sensitivity IN {sens}
                 ORDER BY occurred_at DESC LIMIT ?3"
            );
            let mut stmt = conn.prepare_cached(&sql)?;
            let eps: Vec<crate::episode::Episode> = stmt
                .query_map(
                    params![
                        t.from.clone().unwrap_or_else(|| "0000".into()),
                        t.to.clone().unwrap_or_else(|| "9999".into()),
                        k as i64
                    ],
                    crate::episode::row_to_episode,
                )?
                .collect::<std::result::Result<_, _>>()?;
            eps
        }
        (None, None) => vec![],
    };

    eps.sort_by(|a, b| b.occurred_at.cmp(&a.occurred_at));
    eps.truncate(k);
    for ep in eps {
        let preview: String = ep.body.chars().take(700).collect();
        pack.items.push(PackItem {
            kind: "episode".into(),
            id: ep.uid,
            score: 0.01,
            occurred_at: Some(ep.occurred_at),
            valid_from: None,
            source: Some(ep.source),
            tags: episode::tags_for(conn, ep.id)?,
            text: preview,
        });
    }
    Ok(())
}

/// An entity-less aggregate about *people* ("who do I interact with the
/// most?") ranks the person_interaction rollup; anything else falls back to
/// per-source episode counts. Whole-word match — `contains` would see "who"
/// in "whole" and "person" in "personal".
fn wants_people_ranking(query: &str) -> bool {
    let q = query.to_lowercase();
    let words: Vec<&str> = q
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();
    [
        "who",
        "whom",
        "people",
        "person",
        "interact",
        "interacts",
        "interacted",
        "interactions",
        "meet",
        "met",
        "talk",
        "talked",
    ]
    .iter()
    .any(|m| words.contains(m))
        || q.contains("spend time")
}

fn aggregate_into(
    conn: &Connection,
    query_str: &str,
    entities: &[DetectedEntity],
    time: &Option<TimeRange>,
    k: usize,
    pack: &mut ContextPack,
) -> Result<()> {
    let (from, to) = match time {
        Some(t) => (
            t.from.clone().unwrap_or_else(|| "0000".into()),
            t.to.clone().unwrap_or_else(|| "9999".into()),
        ),
        None => ("0000".into(), "9999".into()),
    };

    if let Some(e) = entities.first() {
        // Interaction counts per channel for this entity in range.
        let mut stmt = conn.prepare_cached(
            "SELECT e.source, COUNT(*) FROM episode e
             JOIN mention m ON m.episode_id = e.id
             WHERE m.node_id = ?1 AND e.occurred_at >= ?2 AND e.occurred_at < ?3
             GROUP BY e.source ORDER BY COUNT(*) DESC",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![e.node_id, from, to], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for (source, count) in rows {
            pack.items.push(PackItem {
                kind: "aggregate".into(),
                id: e.node_id.clone(),
                score: count as f64,
                occurred_at: None,
                valid_from: None,
                source: Some(source.clone()),
                tags: vec![],
                text: format!("{}: {} episodes via {}", e.name, count, source),
            });
        }
    } else if wants_people_ranking(query_str) {
        // "who do I interact with the most?" — rank people, not sources. The
        // all-time answer comes straight off the person_interaction rollup;
        // a time range counts person-mentions inside it instead (the rollup
        // has no per-range counts).
        let rows: Vec<(String, String, i64, Option<String>)> = if time.is_some() {
            let mut stmt = conn.prepare_cached(
                "SELECT n.id, n.name, COUNT(*), MAX(e.occurred_at) FROM mention m
                 JOIN nodes n ON n.id = m.node_id AND n.node_type = 'person'
                 JOIN episode e ON e.id = m.episode_id
                 WHERE e.occurred_at >= ?1 AND e.occurred_at < ?2
                   AND e.occurred_at <= datetime('now')
                 GROUP BY n.id ORDER BY COUNT(*) DESC LIMIT ?3",
            )?;
            let rows: Vec<(String, String, i64, Option<String>)> = stmt
                .query_map(params![from, to, k as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            rows
        } else {
            let mut stmt = conn.prepare_cached(
                "SELECT n.id, n.name, pi.interaction_count, pi.last_seen_at
                 FROM person_interaction pi JOIN nodes n ON n.id = pi.node_id
                 ORDER BY pi.interaction_count DESC LIMIT ?1",
            )?;
            let rows: Vec<(String, String, i64, Option<String>)> = stmt
                .query_map(params![k as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
                })?
                .collect::<std::result::Result<_, _>>()?;
            rows
        };
        for (node_id, name, count, last_seen) in rows {
            let last = last_seen
                .map(|s| format!(", last seen {}", &s[..10.min(s.len())]))
                .unwrap_or_default();
            pack.items.push(PackItem {
                kind: "aggregate".into(),
                id: node_id,
                score: count as f64,
                occurred_at: None,
                valid_from: None,
                source: Some("person_interaction".into()),
                tags: vec![],
                text: format!("{name} — {count} interactions{last}"),
            });
        }
    } else {
        // Global: episodes by source in range.
        let mut stmt = conn.prepare_cached(
            "SELECT source, COUNT(*) FROM episode
             WHERE occurred_at >= ?1 AND occurred_at < ?2
             GROUP BY source ORDER BY COUNT(*) DESC",
        )?;
        let rows: Vec<(String, i64)> = stmt
            .query_map(params![from, to], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        for (source, count) in rows {
            pack.items.push(PackItem {
                kind: "aggregate".into(),
                id: source.clone(),
                score: count as f64,
                occurred_at: None,
                valid_from: None,
                source: Some(source.clone()),
                tags: vec![],
                text: format!("{count} episodes from {source}"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{add_mention, upsert_episode, Episode};
    use crate::graph::{add_alias, get_or_create_person};
    use crate::rollup::rebuild_person_interactions;

    fn ep(src: &str, sid: &str, body: &str, at: &str) -> Episode {
        Episode {
            id: 0,
            uid: String::new(),
            source: src.into(),
            source_id: sid.into(),
            source_ref: None,
            body: body.into(),
            occurred_at: at.into(),
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

    #[test]
    fn test_intent_classification() {
        assert_eq!(
            classify_intent("when did I last meet with June?"),
            Intent::Lookup
        );
        assert_eq!(
            classify_intent("what did Iris say about the pilot?"),
            Intent::Recall
        );
        assert_eq!(
            classify_intent("how many meetings did I have in July?"),
            Intent::Aggregate
        );
        assert_eq!(
            classify_intent("who do I interact with the most?"),
            Intent::Aggregate
        );
        // "the most" without a verb attachment must NOT flip recall queries.
        assert_eq!(
            classify_intent("the meeting I enjoyed the most"),
            Intent::Recall
        );
    }

    #[test]
    fn test_people_ranking_wants_whole_words() {
        assert!(wants_people_ranking("who do I interact with the most?"));
        assert!(wants_people_ranking("how many interactions per person"));
        // Substrings must not trigger: "who" in "whole", "person" in "personal".
        assert!(!wants_people_ranking("how many episodes in the whole year"));
        assert!(!wants_people_ranking("how many personal notes this month"));
    }

    #[test]
    fn test_time_extraction() {
        let t = extract_time("which projects stalled in July 2026?").unwrap();
        assert_eq!(t.from.as_deref(), Some("2026-07-01 00:00:00"));
        assert_eq!(t.to.as_deref(), Some("2026-08-01 00:00:00"));
        assert!(extract_time("what did Iris say about the pilot?").is_none());
    }

    #[test]
    fn test_lookup_uses_rollup_not_search() {
        let conn = open_memory().unwrap();
        let june = get_or_create_person(&conn, Some("june@x.com"), "June Chen", "t").unwrap();
        add_alias(&conn, &june.id, "June", "manual").unwrap();

        let (e1, _) = upsert_episode(
            &conn,
            &ep(
                "calendar.event",
                "c1",
                "1:1 with June",
                "2026-07-30 10:00:00",
            ),
        )
        .unwrap();
        add_mention(&conn, e1, &june.id, "attendee", 1.0).unwrap();
        rebuild_person_interactions(&conn).unwrap();

        let pack = query(
            &conn,
            None,
            "when did I last meet with June?",
            10,
            2000,
            false,
            None,
        )
        .unwrap();
        assert_eq!(pack.intent, Intent::Lookup);
        assert_eq!(pack.items.len(), 1);
        assert_eq!(pack.items[0].kind, "person_interaction");
        assert!(pack.items[0].text.contains("2026-07-30"));
    }

    #[test]
    fn test_event_nodes_do_not_shadow_people() {
        let conn = open_memory().unwrap();
        let iris = get_or_create_person(&conn, Some("iris@x.com"), "Iris", "t").unwrap();
        // A recurring calendar event whose name contains the person's name.
        for i in 0..3 {
            let mut ev = crate::graph::Node::new(&format!("event-{i}"), "event", "Meet with Iris");
            ev.source = "calendar".into();
            crate::graph::upsert_node(&conn, &ev).unwrap();
        }
        let (entities, ambiguous) =
            detect_entities(&conn, "when did I last meet with Iris?").unwrap();
        assert!(
            ambiguous.is_empty(),
            "event nodes must not create ambiguity"
        );
        assert_eq!(entities.len(), 1);
        assert_eq!(entities[0].node_id, iris.id, "person wins, not the event");
    }

    #[test]
    fn test_ambiguity_surfaces_disambiguation() {
        let conn = open_memory().unwrap();
        let g1 = get_or_create_person(&conn, Some("g1@x.com"), "June Chen", "t").unwrap();
        let g2 = get_or_create_person(&conn, Some("g2@y.com"), "June Rodriguez", "t").unwrap();
        add_alias(&conn, &g1.id, "June", "manual").unwrap();
        add_alias(&conn, &g2.id, "June", "manual").unwrap();

        let pack = query(
            &conn,
            None,
            "when did I last meet with June?",
            10,
            2000,
            false,
            None,
        )
        .unwrap();
        assert!(pack.items.is_empty(), "must not guess");
        assert_eq!(pack.ambiguous.len(), 1);
        assert_eq!(pack.ambiguous[0].candidates.len(), 2);
    }

    #[test]
    fn test_recall_constrained_by_entity() {
        let conn = open_memory().unwrap();
        let iris = get_or_create_person(&conn, Some("iris@x.com"), "Iris", "t").unwrap();

        let (e1, _) = upsert_episode(
            &conn,
            &ep(
                "bee.conversation",
                "b1",
                "Iris suggested rerunning the pilot analysis",
                "2026-07-01 10:00:00",
            ),
        )
        .unwrap();
        upsert_episode(
            &conn,
            &ep(
                "bee.conversation",
                "b2",
                "pilot program orientation for new students",
                "2026-07-02 10:00:00",
            ),
        )
        .unwrap();
        add_mention(&conn, e1, &iris.id, "alias", 0.9).unwrap();

        let pack = query(
            &conn,
            None,
            "conversation with Iris about the pilot",
            10,
            4000,
            true,
            None,
        )
        .unwrap();
        assert_eq!(pack.intent, Intent::Recall);
        let ep_items: Vec<_> = pack.items.iter().filter(|i| i.kind == "episode").collect();
        assert_eq!(
            ep_items.len(),
            1,
            "entity filter should exclude the other pilot episode"
        );
        assert!(ep_items[0].text.contains("Iris"));
    }

    #[test]
    fn test_extract_tags() {
        let (tags, rest) = extract_tags("#recommendation lunch spots near campus");
        assert_eq!(tags, vec!["recommendation"]);
        assert_eq!(rest, "lunch spots near campus");

        // Canonicalized like annotate_episode; duplicates collapse.
        let (tags, rest) = extract_tags("#Reading, #reading papers");
        assert_eq!(tags, vec!["reading"]);
        assert_eq!(rest, "papers");

        // Not tags: bare '#', issue-number fragments, plain text.
        let (tags, rest) = extract_tags("fix # and #123 in the tracker");
        assert!(tags.is_empty());
        assert_eq!(rest, "fix # and #123 in the tracker");
    }

    #[test]
    fn test_tag_filter_constrains_recall() {
        let conn = open_memory().unwrap();
        let (e1, _) = upsert_episode(
            &conn,
            &ep(
                "note",
                "n1",
                "great ramen place downtown",
                "2026-07-01 10:00:00",
            ),
        )
        .unwrap();
        upsert_episode(
            &conn,
            &ep(
                "note",
                "n2",
                "ramen supply chain article",
                "2026-07-02 10:00:00",
            ),
        )
        .unwrap();
        crate::episode::annotate_episode(&conn, e1, "tag", "#Recommendation").unwrap();

        let pack = query(&conn, None, "#recommendation ramen", 10, 4000, true, None).unwrap();
        assert_eq!(pack.tags, vec!["recommendation"]);
        let eps: Vec<_> = pack.items.iter().filter(|i| i.kind == "episode").collect();
        assert_eq!(
            eps.len(),
            1,
            "tag filter should exclude the untagged ramen episode"
        );
        assert!(eps[0].text.contains("downtown"));
        assert_eq!(
            eps[0].tags,
            vec!["recommendation"],
            "hit should surface its tags"
        );
    }

    #[test]
    fn test_tag_only_query_lists_newest_first() {
        let conn = open_memory().unwrap();
        let (e1, _) = upsert_episode(
            &conn,
            &ep("note", "n1", "older tagged", "2026-06-01 10:00:00"),
        )
        .unwrap();
        let (e2, _) = upsert_episode(
            &conn,
            &ep("note", "n2", "newer tagged", "2026-07-01 10:00:00"),
        )
        .unwrap();
        upsert_episode(
            &conn,
            &ep("note", "n3", "untagged noise", "2026-07-02 10:00:00"),
        )
        .unwrap();
        crate::episode::annotate_episode(&conn, e1, "tag", "reading").unwrap();
        crate::episode::annotate_episode(&conn, e2, "tag", "reading").unwrap();

        let pack = query(&conn, None, "#reading", 10, 4000, false, None).unwrap();
        let eps: Vec<_> = pack.items.iter().filter(|i| i.kind == "episode").collect();
        assert_eq!(eps.len(), 2);
        assert!(eps[0].text.contains("newer"), "newest first");
        assert!(eps[1].text.contains("older"));

        // A `#token` not in the tag vocabulary is NOT a filter — it degrades
        // to text (Slack channels, typos), which matches nothing here.
        let pack = query(&conn, None, "#nosuchtag", 10, 4000, true, None).unwrap();
        assert!(
            pack.tags.is_empty(),
            "unknown token must not become a filter"
        );
        assert!(pack.items.is_empty());
    }

    #[test]
    fn test_unknown_hash_token_searches_as_text() {
        let conn = open_memory().unwrap();
        // A Slack channel-day: '#papers' names a channel, not a tag.
        upsert_episode(
            &conn,
            &ep(
                "slack.thread",
                "s1",
                "#papers 2026-07-01: Iris shared the new attention paper",
                "2026-07-01 10:00:00",
            ),
        )
        .unwrap();

        let pack = query(
            &conn,
            None,
            "what was shared in #papers",
            10,
            4000,
            true,
            None,
        )
        .unwrap();
        assert!(pack.tags.is_empty());
        let eps: Vec<_> = pack.items.iter().filter(|i| i.kind == "episode").collect();
        assert_eq!(eps.len(), 1, "channel token must keep searching as text");

        // The moment 'papers' IS a vocabulary tag, the same token filters.
        let (e2, _) = upsert_episode(
            &conn,
            &ep("note", "n1", "reading list", "2026-07-02 10:00:00"),
        )
        .unwrap();
        crate::episode::annotate_episode(&conn, e2, "tag", "papers").unwrap();
        let pack = query(&conn, None, "#papers", 10, 4000, true, None).unwrap();
        assert_eq!(pack.tags, vec!["papers"]);
        assert_eq!(pack.items.len(), 1);
        assert!(pack.items[0].text.contains("reading list"));
    }

    #[test]
    fn test_aggregate_ranks_people_by_interaction() {
        let conn = open_memory().unwrap();
        let iris = get_or_create_person(&conn, Some("iris@x.com"), "Iris Calder", "t").unwrap();
        let june = get_or_create_person(&conn, Some("june@x.com"), "June Chen", "t").unwrap();
        for i in 0..3 {
            let (e, _) = upsert_episode(
                &conn,
                &ep(
                    "calendar.event",
                    &format!("c{i}"),
                    "sync meeting",
                    &format!("2026-07-0{} 10:00:00", i + 1),
                ),
            )
            .unwrap();
            add_mention(&conn, e, &iris.id, "attendee", 1.0).unwrap();
            if i == 0 {
                add_mention(&conn, e, &june.id, "attendee", 1.0).unwrap();
            }
        }
        rebuild_person_interactions(&conn).unwrap();

        let pack = query(
            &conn,
            None,
            "who do I interact with the most?",
            10,
            4000,
            false,
            None,
        )
        .unwrap();
        assert_eq!(pack.intent, Intent::Aggregate);
        assert!(pack.items.len() >= 2);
        assert!(
            pack.items[0].text.contains("Iris Calder"),
            "top item: {}",
            pack.items[0].text
        );
        assert!(pack.items[0].text.contains("3 interactions"));
        assert!(pack.items[1].text.contains("June Chen"));
    }

    #[test]
    fn test_pack_ranked_by_score_not_kind() {
        let conn = open_memory().unwrap();
        // An episode matching the query strongly must not sit below a pile of
        // weakly-matching facts.
        let (_e, _) = upsert_episode(
            &conn,
            &ep(
                "calendar.event",
                "c1",
                "Dinner w/ Mateo",
                "2026-05-21 22:00:00",
            ),
        )
        .unwrap();
        for i in 0..12 {
            crate::graph::upsert_node(
                &conn,
                &crate::graph::Node::new(
                    &format!("person-{i}"),
                    "person",
                    &format!("Attendee {i}"),
                ),
            )
            .unwrap();
            crate::fact::assert_fact(
                &conn,
                &format!("person-{i}"),
                "attended",
                None,
                Some("dinner"),
                &format!(
                    "attendee {i} attended \"Dinner\" on 2026-01-0{}",
                    (i % 9) + 1
                ),
                None,
                None,
                0.9,
                "test",
            )
            .unwrap();
        }

        let pack = query(&conn, None, "dinner with Mateo", 10, 8000, true, None).unwrap();
        let first_ep = pack.items.iter().position(|i| i.kind == "episode");
        assert!(
            first_ep.is_some_and(|r| r < 10),
            "the matching episode must rank in the top 10, got {:?}",
            pack.items
                .iter()
                .map(|i| i.kind.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_tag_only_query_respects_sensitivity() {
        let conn = open_memory().unwrap();
        let mut e = ep(
            "bee.conversation",
            "b1",
            "private tagged conversation",
            "2026-07-01 10:00:00",
        );
        e.sensitivity = "private".into();
        let (id, _) = upsert_episode(&conn, &e).unwrap();
        crate::episode::annotate_episode(&conn, id, "tag", "health").unwrap();

        let default = query(&conn, None, "#health", 10, 4000, false, None).unwrap();
        assert!(
            default.items.is_empty(),
            "private stays out of default retrieval"
        );
        let opted_in = query(&conn, None, "#health", 10, 4000, true, None).unwrap();
        assert_eq!(opted_in.items.len(), 1);
    }

    #[test]
    fn test_multiple_tags_intersect() {
        let conn = open_memory().unwrap();
        let (both, _) = upsert_episode(
            &conn,
            &ep("note", "n1", "tagged with both", "2026-07-01 10:00:00"),
        )
        .unwrap();
        let (one, _) = upsert_episode(
            &conn,
            &ep("note", "n2", "tagged with one", "2026-07-02 10:00:00"),
        )
        .unwrap();
        crate::episode::annotate_episode(&conn, both, "tag", "reading").unwrap();
        crate::episode::annotate_episode(&conn, both, "tag", "vision").unwrap();
        crate::episode::annotate_episode(&conn, one, "tag", "reading").unwrap();

        let pack = query(&conn, None, "#reading #vision", 10, 4000, true, None).unwrap();
        let eps: Vec<_> = pack.items.iter().filter(|i| i.kind == "episode").collect();
        assert_eq!(eps.len(), 1, "tags AND together");
        assert!(eps[0].text.contains("both"));
        assert_eq!(eps[0].tags, vec!["reading", "vision"]);
    }

    /// A graph where the same query is answerable from both halves: one
    /// episode (evidence) and one fact (belief) both matching "quantum".
    fn scoped_fixture() -> rusqlite::Connection {
        let conn = open_memory().unwrap();
        let (eid, _) = upsert_episode(
            &conn,
            &ep(
                "note",
                "s1",
                "long discussion about the quantum grant",
                "2026-07-01 10:00:00",
            ),
        )
        .unwrap();
        crate::graph::upsert_node(
            &conn,
            &crate::graph::Node::new("project-q", "project", "Quantum Grant"),
        )
        .unwrap();
        // The query resolves "Quantum Grant" as an entity, which filters
        // episodes to those mentioning it — link the evidence.
        add_mention(&conn, eid, "project-q", "test", 1.0).unwrap();
        crate::fact::assert_fact(
            &conn,
            "project-q",
            "works_on",
            None,
            Some("quantum"),
            "Ada works_on the quantum grant",
            None,
            None,
            0.9,
            "test",
        )
        .unwrap();
        conn
    }

    #[test]
    fn test_scope_blind_halves() {
        let conn = scoped_fixture();

        // Both: the two halves compete in one pack.
        let both = query(&conn, None, "quantum grant", 10, 4000, true, None).unwrap();
        assert!(both.items.iter().any(|i| i.kind == "fact"));
        assert!(both.items.iter().any(|i| i.kind == "episode"));

        // Answerer A: beliefs only — no episode may leak in.
        let facts = query_scoped(
            &conn,
            None,
            "quantum grant",
            10,
            4000,
            true,
            None,
            Scope::FactsOnly,
        )
        .unwrap();
        assert!(!facts.items.is_empty());
        assert!(
            facts.items.iter().all(|i| i.kind == "fact"),
            "facts_only must serve no evidence, got {:?}",
            facts
                .items
                .iter()
                .map(|i| i.kind.as_str())
                .collect::<Vec<_>>()
        );

        // Answerer B: evidence only — no fact may leak in.
        let evid = query_scoped(
            &conn,
            None,
            "quantum grant",
            10,
            4000,
            true,
            None,
            Scope::EvidenceOnly,
        )
        .unwrap();
        assert!(!evid.items.is_empty());
        assert!(
            evid.items.iter().all(|i| i.kind == "episode"),
            "evidence_only must serve no beliefs, got {:?}",
            evid.items
                .iter()
                .map(|i| i.kind.as_str())
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_scope_lookup_rollup_counts_as_fact() {
        let conn = open_memory().unwrap();
        let june = get_or_create_person(&conn, Some("june@x.com"), "June Chen", "t").unwrap();
        add_alias(&conn, &june.id, "June", "manual").unwrap();
        let (eid, _) = upsert_episode(
            &conn,
            &ep(
                "calendar.event",
                "c1",
                "Coffee with June",
                "2026-06-01 09:00:00",
            ),
        )
        .unwrap();
        add_mention(&conn, eid, &june.id, "attendee", 1.0).unwrap();
        rebuild_person_interactions(&conn).unwrap();

        let q = "when did I last meet with June?";
        let both = query(&conn, None, q, 10, 4000, true, None).unwrap();
        assert!(
            both.items.iter().any(|i| i.kind == "person_interaction"),
            "sanity: default lookup answers from the rollup"
        );

        // The rollup row is derived (a belief): evidence_only must skip the
        // structured answer and fall through to raw episodes.
        let evid = query_scoped(&conn, None, q, 10, 4000, true, None, Scope::EvidenceOnly).unwrap();
        assert!(
            evid.items.iter().all(|i| i.kind == "episode"),
            "evidence_only lookup must serve episodes, got {:?}",
            evid.items
                .iter()
                .map(|i| i.kind.as_str())
                .collect::<Vec<_>>()
        );
        assert!(!evid.items.is_empty(), "the fallthrough must still answer");
    }

    #[test]
    fn test_scoped_queries_never_ledger() {
        let conn = scoped_fixture();
        let count = |c: &rusqlite::Connection| -> i64 {
            c.query_row("SELECT COUNT(*) FROM query_log", [], |r| r.get(0))
                .unwrap()
        };

        // Probe-shaped retrieval: tool given, but scope != both → no demand.
        query_scoped(
            &conn,
            None,
            "quantum grant",
            10,
            4000,
            true,
            Some("mcp.kg_search"),
            Scope::FactsOnly,
        )
        .unwrap();
        query_scoped(
            &conn,
            None,
            "quantum grant",
            10,
            4000,
            true,
            Some("mcp.kg_search"),
            Scope::EvidenceOnly,
        )
        .unwrap();
        assert_eq!(
            count(&conn),
            0,
            "blind halves must not corrupt the demand signal"
        );

        // The same query at full scope ledgers normally.
        query(
            &conn,
            None,
            "quantum grant",
            10,
            4000,
            true,
            Some("mcp.kg_search"),
        )
        .unwrap();
        assert_eq!(count(&conn), 1);
    }

    #[test]
    fn test_scope_parse_and_envelope() {
        assert_eq!(Scope::parse("both"), Some(Scope::Both));
        assert_eq!(Scope::parse("facts"), Some(Scope::FactsOnly));
        assert_eq!(Scope::parse("facts_only"), Some(Scope::FactsOnly));
        assert_eq!(Scope::parse("evidence"), Some(Scope::EvidenceOnly));
        assert_eq!(Scope::parse("evidence_only"), Some(Scope::EvidenceOnly));
        assert_eq!(Scope::parse("everything"), None);

        // The default scope stays out of the JSON envelope (byte-identical
        // packs for every existing consumer); a blind half is stamped.
        let conn = scoped_fixture();
        let both = query(&conn, None, "quantum grant", 10, 4000, true, None).unwrap();
        let v = serde_json::to_value(&both).unwrap();
        assert!(v.get("scope").is_none(), "default scope is omitted");
        let facts = query_scoped(
            &conn,
            None,
            "quantum grant",
            10,
            4000,
            true,
            None,
            Scope::FactsOnly,
        )
        .unwrap();
        let v = serde_json::to_value(&facts).unwrap();
        assert_eq!(v["scope"], "facts_only");
    }

    /// The same person discussed in three independent sources.
    fn multi_source(conn: &rusqlite::Connection) -> String {
        let p = get_or_create_person(conn, Some("m@x.com"), "Rosa Marin", "t").unwrap();
        for (src, sid, body) in [
            (
                "calendar.event",
                "c1",
                "1:1 with Rosa Marin about the Example University move",
            ),
            (
                "bee.conversation",
                "b1",
                "Rosa Marin said she is joining Brown next term",
            ),
            (
                "slack.thread",
                "s1",
                "Rosa Marin shared the Example University onboarding doc",
            ),
        ] {
            let (id, _) = upsert_episode(conn, &ep(src, sid, body, "2026-06-01 10:00:00")).unwrap();
            add_mention(conn, id, &p.id, "manual", 1.0).unwrap();
        }
        p.id
    }

    #[test]
    fn test_source_lens_gives_two_agents_different_evidence() {
        let conn = open_memory().unwrap();
        multi_source(&conn);

        let via = |srcs: &[&str]| -> Vec<String> {
            let lens = Lens::from_sources(srcs.iter().map(|s| s.to_string()).collect());
            let pack = query_lens(&conn, None, "Rosa Marin", 10, 4000, true, None, lens).unwrap();
            pack.items
                .iter()
                .filter(|i| i.kind == "episode")
                .filter_map(|i| i.source.clone())
                .collect()
        };

        // Each agent sees only its own sources — this is what makes a
        // disagreement between them mean something.
        assert_eq!(via(&["bee.conversation"]), vec!["bee.conversation"]);
        let written = via(&["calendar.event", "slack.thread"]);
        assert_eq!(written.len(), 2);
        assert!(written.iter().all(|s| s != "bee.conversation"));

        // Unrestricted still sees everything.
        let all = query(&conn, None, "Rosa Marin", 10, 4000, true, None).unwrap();
        assert_eq!(all.items.iter().filter(|i| i.kind == "episode").count(), 3);
    }

    #[test]
    fn test_window_makes_two_sources_comparable() {
        // The real shape: the calendar's evidence about a person spans
        // years the transcripts do not. Without a shared window, a
        // difference between the two readers is the world having moved,
        // not the sources disagreeing.
        let conn = open_memory().unwrap();
        let p = get_or_create_person(&conn, Some("m@x.com"), "Rosa Marin", "t").unwrap();
        // Distinct bodies: the pack dedupes by content hash, so identical
        // text would collapse and the count would measure dedupe rather
        // than the window.
        for (src, sid, when) in [
            ("calendar.event", "old1", "2019-03-01 10:00:00"),
            ("calendar.event", "old2", "2019-06-01 10:00:00"),
            ("calendar.event", "new1", "2026-06-01 10:00:00"),
            ("bee.conversation", "new2", "2026-06-02 10:00:00"),
        ] {
            let body = format!("Rosa Marin and the pilot, {sid}");
            let (id, _) = upsert_episode(&conn, &ep(src, sid, &body, when)).unwrap();
            add_mention(&conn, id, &p.id, "manual", 1.0).unwrap();
        }

        let in_window = |srcs: Vec<String>| -> Vec<String> {
            let lens = Lens::windowed(
                srcs,
                Some("2026-01-01 00:00:00".into()),
                Some("2027-01-01 00:00:00".into()),
            );
            query_lens(
                &conn,
                None,
                "Rosa Marin pilot",
                10,
                4000,
                true,
                None,
                lens,
            )
            .unwrap()
            .items
            .iter()
            .filter(|i| i.kind == "episode")
            .filter_map(|i| i.occurred_at.clone())
            .collect()
        };

        // Same era on both sides — the precondition for a meaningful diff.
        let cal = in_window(vec!["calendar.event".into()]);
        assert_eq!(cal.len(), 1, "the 2019 calendar evidence is out of window");
        assert!(cal[0].starts_with("2026"));
        assert_eq!(in_window(vec!["bee.conversation".into()]).len(), 1);

        // Unwindowed, the calendar reader would be answering from 2019.
        let unwindowed = query_lens(
            &conn,
            None,
            "Rosa Marin pilot",
            10,
            4000,
            true,
            None,
            Lens::from_sources(vec!["calendar.event".into()]),
        )
        .unwrap();
        assert_eq!(
            unwindowed
                .items
                .iter()
                .filter(|i| i.kind == "episode")
                .count(),
            3,
            "without a window the two readers are comparing eras"
        );
    }

    #[test]
    fn test_explicit_window_overrides_the_query_text() {
        // A probe that fixes a window must not have it widened by a
        // phrase in the question — both readers get the same era or the
        // comparison is not one.
        let conn = open_memory().unwrap();
        let p = get_or_create_person(&conn, Some("m@x.com"), "Rosa Marin", "t").unwrap();
        for (sid, when) in [("a", "2019-07-04 10:00:00"), ("b", "2026-06-01 10:00:00")] {
            let (id, _) =
                upsert_episode(&conn, &ep("note", sid, "Rosa Marin pilot review", when)).unwrap();
            add_mention(&conn, id, &p.id, "manual", 1.0).unwrap();
        }
        let lens = Lens::windowed(
            vec![],
            Some("2026-01-01 00:00:00".into()),
            Some("2027-01-01 00:00:00".into()),
        );
        // The text says July 2019; the lens says 2026. The lens wins.
        let pack = query_lens(
            &conn,
            None,
            "Rosa Marin pilot review in July 2019",
            10,
            4000,
            true,
            None,
            lens,
        )
        .unwrap();
        let eps: Vec<_> = pack.items.iter().filter(|i| i.kind == "episode").collect();
        assert_eq!(eps.len(), 1);
        assert!(eps[0].occurred_at.as_deref().unwrap().starts_with("2026"));

        // And the pack says which window it used, so a comparison can
        // prove both readers were shown the same one.
        let v = serde_json::to_value(&pack).unwrap();
        assert_eq!(v["window"]["from"], "2026-01-01 00:00:00");
        // Omitted when unrestricted.
        let plain = query(&conn, None, "Rosa Marin", 10, 4000, true, None).unwrap();
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("window")
            .is_none());
    }

    #[test]
    fn test_source_filter_is_filter_first_not_post_filter() {
        // The reason this matters: post-filtering a fused ranking asks
        // "of the best hits overall, which are Bee?" — and returns
        // nothing when the top of the ranking is calendar. Bury one Bee
        // episode under many strong calendar matches and it must still
        // come back.
        let conn = open_memory().unwrap();
        let p = get_or_create_person(&conn, Some("m@x.com"), "Rosa Marin", "t").unwrap();
        for i in 0..30 {
            let (id, _) = upsert_episode(
                &conn,
                &ep(
                    "calendar.event",
                    &format!("c{i}"),
                    "Rosa Marin quarterly planning review",
                    "2026-06-01 10:00:00",
                ),
            )
            .unwrap();
            add_mention(&conn, id, &p.id, "manual", 1.0).unwrap();
        }
        let (id, _) = upsert_episode(
            &conn,
            &ep(
                "bee.conversation",
                "b1",
                "Rosa Marin quarterly planning review",
                "2026-06-02 10:00:00",
            ),
        )
        .unwrap();
        add_mention(&conn, id, &p.id, "manual", 1.0).unwrap();

        let lens = Lens::from_sources(vec!["bee.conversation".into()]);
        let pack = query_lens(
            &conn,
            None,
            "Rosa Marin quarterly planning review",
            5,
            4000,
            true,
            None,
            lens,
        )
        .unwrap();
        let eps: Vec<_> = pack.items.iter().filter(|i| i.kind == "episode").collect();
        assert_eq!(
            eps.len(),
            1,
            "the lone Bee episode survives 30 competing calendar hits"
        );
        assert_eq!(eps[0].source.as_deref(), Some("bee.conversation"));
    }

    #[test]
    fn test_lens_echoes_sources_and_never_ledgers() {
        let conn = open_memory().unwrap();
        multi_source(&conn);
        let lens = Lens::from_sources(vec!["bee.conversation".into()]);
        let pack = query_lens(
            &conn,
            None,
            "Rosa Marin",
            10,
            4000,
            true,
            Some("mcp.kg_search"),
            lens,
        )
        .unwrap();

        // Echoed, so a verifier knows what this pack could NOT have seen.
        assert_eq!(pack.sources, vec!["bee.conversation".to_string()]);
        let v = serde_json::to_value(&pack).unwrap();
        assert_eq!(v["sources"][0], "bee.conversation");

        // Source-scoped reads are probe-shaped: an agent handed only Bee
        // must not teach the Selector that Bee's people are what the
        // owner asks about.
        let logged: i64 = conn
            .query_row("SELECT COUNT(*) FROM query_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(logged, 0, "a lensed read never records demand");

        // The default lens still ledgers, and still omits both fields.
        query(
            &conn,
            None,
            "Rosa Marin",
            10,
            4000,
            true,
            Some("mcp.kg_search"),
        )
        .unwrap();
        let logged: i64 = conn
            .query_row("SELECT COUNT(*) FROM query_log", [], |r| r.get(0))
            .unwrap();
        assert_eq!(logged, 1);
        let plain = query(&conn, None, "Rosa Marin", 10, 4000, true, None).unwrap();
        assert!(serde_json::to_value(&plain)
            .unwrap()
            .get("sources")
            .is_none());
    }

    #[test]
    fn test_empty_source_is_a_filter_not_unfiltered() {
        // A source with no episodes must return nothing. Reading it as
        // "unfiltered" would hand an agent the whole graph while telling
        // it it was reading one source.
        let conn = open_memory().unwrap();
        multi_source(&conn);
        let lens = Lens::from_sources(vec!["mbox".into()]);
        let pack = query_lens(&conn, None, "Rosa Marin", 10, 4000, true, None, lens).unwrap();
        assert!(
            pack.items.iter().all(|i| i.kind != "episode"),
            "an unpopulated source yields no episodes, not every episode"
        );
    }

    #[test]
    fn test_activation_nudges_hot_item_past_a_near_tie() {
        let conn = open_memory().unwrap();
        crate::graph::upsert_node(&conn, &crate::graph::Node::new("topic-s", "topic", "Sigma"))
            .unwrap();
        // Two facts matching the same query; the longer statement ranks
        // below the shorter on BM25 length normalization.
        let hot = crate::fact::assert_fact(
            &conn,
            "topic-s",
            "has",
            None,
            Some("draft"),
            "the sigma protocol draft with extra review notes appended",
            None,
            None,
            0.8,
            "test",
        )
        .unwrap();
        crate::fact::assert_fact(
            &conn,
            "topic-s",
            "has",
            None,
            Some("draft2"),
            "the sigma protocol draft",
            None,
            None,
            0.8,
            "test",
        )
        .unwrap();

        let baseline = query(&conn, None, "sigma protocol draft", 10, 4000, true, None).unwrap();
        assert_ne!(
            baseline.items[0].id, hot,
            "sanity: the long fact starts behind"
        );

        // Real demand accumulates on the long one (ACT-R base level)...
        conn.execute(
            "INSERT INTO retrieval_touch (kind, ref_id, touches, first_at, last_at)
             VALUES ('fact', ?1, 30, datetime('now', '-3 days'), datetime('now'))",
            params![hot],
        )
        .unwrap();
        // ...and the activation arm lifts it past the near-tie.
        let after = query(&conn, None, "sigma protocol draft", 10, 4000, true, None).unwrap();
        assert_eq!(
            after.items[0].id, hot,
            "frequently-retrieved items surface faster (low-weight arm, ties only)"
        );
    }
}
