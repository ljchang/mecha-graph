//! Human-readable rendering for CLI output. Machine consumers get JSON:
//! by default the choice is automatic (TTY → human, pipe → JSON), with
//! --text / --json forcing either way. MCP always speaks JSON envelopes.

use mecha_graph_core::router::ContextPack;
use mecha_graph_core::stats::HealthStats;

/// ANSI helpers — only applied when writing to a TTY.
pub struct Style {
    pub enabled: bool,
}

impl Style {
    pub fn bold(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[1m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn dim(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[2m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
    pub fn accent(&self, s: &str) -> String {
        if self.enabled {
            format!("\x1b[36m{s}\x1b[0m")
        } else {
            s.to_string()
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("")
}

pub fn render_pack(pack: &ContextPack, style: &Style) -> String {
    let mut out = String::new();

    let intent = format!("{:?}", pack.intent).to_lowercase();
    out.push_str(&format!(
        "{} {}\n",
        style.bold(&pack.query),
        style.dim(&format!("[{intent}]"))
    ));

    if !pack.entities.is_empty() {
        let names: Vec<String> = pack
            .entities
            .iter()
            .map(|e| format!("{} ({})", e.name, e.node_type))
            .collect();
        out.push_str(&style.dim(&format!("entities: {}\n", names.join(", "))));
    }

    // Point-of-use flags (≤2, answer-changing only): the pack says what is
    // wrong with what it serves; the reader judges.
    for f in &pack.flags {
        out.push_str(&format!(
            "{} {}\n",
            style.bold(&format!("⚑ {}", f.kind)),
            f.detail
        ));
    }

    // Ambiguity is a feature (§8.1): surface it prominently.
    for amb in &pack.ambiguous {
        out.push_str(&format!(
            "\n{}\n",
            style.bold(&format!(
                "\"{}\" is ambiguous — which did you mean?",
                amb.matched
            ))
        ));
        for c in &amb.candidates {
            out.push_str(&format!(
                "  · {} {}\n",
                c.name,
                style.dim(&format!(
                    "({} interactions{})",
                    c.interaction_count,
                    c.last_seen
                        .as_deref()
                        .map(|s| format!(", last seen {}", &s[..10.min(s.len())]))
                        .unwrap_or_default()
                ))
            ));
        }
    }

    if pack.items.is_empty() && pack.ambiguous.is_empty() {
        out.push_str(&style.dim("no results\n"));
        return out;
    }

    for (i, item) in pack.items.iter().enumerate() {
        match item.kind.as_str() {
            // Lookup answers are one-liners — show them plainly.
            "person_interaction" | "aggregate" => {
                out.push_str(&format!("\n{} {}\n", style.accent("→"), item.text));
            }
            "fact" => {
                out.push_str(&format!(
                    "\n{}. {} {}\n",
                    i + 1,
                    item.text,
                    style.dim("[fact]")
                ));
            }
            _ => {
                let when = item
                    .occurred_at
                    .as_deref()
                    .map(|s| s[..16.min(s.len())].to_string())
                    .unwrap_or_default();
                let src = item.source.as_deref().unwrap_or("?");
                let tags = if item.tags.is_empty() {
                    String::new()
                } else {
                    format!(" #{}", item.tags.join(" #"))
                };
                out.push_str(&format!(
                    "\n{}. {} {}\n",
                    i + 1,
                    style.bold(first_line(&item.text)),
                    style.dim(&format!("[{when} · {src}{tags}]"))
                ));
                // Body preview: a few lines, indented, without the title line.
                for line in item
                    .text
                    .lines()
                    .skip(1)
                    .filter(|l| !l.trim().is_empty())
                    .take(3)
                {
                    let line: String = line.chars().take(110).collect();
                    out.push_str(&format!("   {line}\n"));
                }
                out.push_str(&style.dim(&format!("   id: {}\n", item.id)));
            }
        }
    }

    if pack.truncated {
        out.push_str(&style.dim(&format!(
            "\n(truncated to ~{} tokens; raise with --budget)\n",
            pack.budget_tokens
        )));
    }
    out
}

pub fn render_stats(h: &HealthStats, style: &Style) -> String {
    let mut out = String::new();
    out.push_str(&style.bold("episodes\n"));
    let total: i64 = h.episodes_by_source.iter().map(|(_, n)| n).sum();
    for (source, n) in &h.episodes_by_source {
        out.push_str(&format!("  {source:<18} {n:>7}\n"));
    }
    out.push_str(&format!("  {:<18} {total:>7}\n", style.dim("total")));

    out.push_str(&style.bold("\ngraph\n"));
    for (t, n) in &h.nodes_by_type {
        out.push_str(&format!("  {t:<18} {n:>7}\n"));
    }
    out.push_str(&format!(
        "  {:<18} {:>7}   {}\n",
        "facts (live)",
        h.facts_live,
        style.dim(&format!("({} incl. history)", h.facts_total))
    ));

    out.push_str(&style.bold("\npipeline\n"));
    out.push_str(&format!(
        "  enriched {:.1}% · embedded {:.1}% · isolated nodes {:.1}%\n",
        h.enriched_pct, h.embedded_pct, h.isolated_pct
    ));
    out.push_str(&format!(
        "  review queue {} · live contradictions {} · llm-only facts {}\n",
        h.merge_queue_depth, h.live_contradictions, h.llm_only_facts
    ));
    out.push_str(&format!(
        "  decayed beliefs {} (valid time closed, never invalidated)\n",
        h.decayed_beliefs
    ));

    out.push_str(&style.bold("\nsources\n"));
    for s in &h.ingest_state {
        let flag = if s.last_error.is_some() {
            style.accent("ERROR")
        } else if s.stale {
            style.accent("STALE")
        } else {
            "ok".to_string()
        };
        out.push_str(&format!(
            "  {:<16} {:<6} last ok {}\n",
            s.source,
            flag,
            s.last_ok_at.as_deref().unwrap_or("never")
        ));
        if let Some(e) = &s.last_error {
            out.push_str(&format!("    {}\n", style.dim(e)));
        }
    }

    // §11.4: a number without an action is decoration — flag what needs one.
    let mut alerts = Vec::new();
    if h.merge_queue_depth > 10 {
        alerts.push(format!(
            "review queue {} > 10 → mecha-graph review",
            h.merge_queue_depth
        ));
    }
    if h.isolated_pct > 25.0 {
        alerts.push(format!(
            "isolated {:.0}% > 25% → check Tier 1/2 linkers",
            h.isolated_pct
        ));
    }
    if h.live_contradictions > 0 {
        alerts.push(format!(
            "{} live contradictions → fact review",
            h.live_contradictions
        ));
    }
    for s in &h.ingest_state {
        if s.stale {
            alerts.push(format!("{} stale > 24h → check the source", s.source));
        }
    }
    if !alerts.is_empty() {
        out.push_str(&style.bold("\nalerts\n"));
        for a in alerts {
            out.push_str(&format!("  ⚠ {a}\n"));
        }
    }
    out
}
