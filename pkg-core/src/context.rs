//! Context assembly (§4.5): `context(n)` = instructions along the scope_id
//! chain (root → n, innermost wins) + summary(n) + facts from the bounded edge
//! neighborhood. Inheritance needs a tree (scope_id); association is the graph.

use crate::error::Result;
use crate::fact;
use crate::graph;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct NodeContext {
    pub instruction: String,
    pub summary: String,
    pub summary_updated_at: Option<String>,
    pub summary_stale: bool,
}

pub fn get_node_context(conn: &Connection, node_id: &str) -> Result<Option<NodeContext>> {
    Ok(conn
        .query_row(
            "SELECT instruction, summary, summary_updated_at, summary_stale
             FROM node_context WHERE node_id = ?1",
            params![node_id],
            |r| {
                Ok(NodeContext {
                    instruction: r.get(0)?,
                    summary: r.get(1)?,
                    summary_updated_at: r.get(2)?,
                    summary_stale: r.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()?)
}

/// Set the hand-authored instruction. NEVER auto-modified (§4.5) — this is the
/// only writer, and it is only reachable from user-facing surfaces.
pub fn set_instruction(conn: &Connection, node_id: &str, instruction: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_context (node_id, instruction) VALUES (?1, ?2)
         ON CONFLICT(node_id) DO UPDATE SET instruction = excluded.instruction",
        params![node_id, instruction],
    )?;
    Ok(())
}

/// Set the generated summary (materialized view; refreshable).
pub fn set_summary(conn: &Connection, node_id: &str, summary: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO node_context (node_id, summary, summary_updated_at, summary_stale)
         VALUES (?1, ?2, datetime('now'), 0)
         ON CONFLICT(node_id) DO UPDATE SET
             summary = excluded.summary,
             summary_updated_at = datetime('now'),
             summary_stale = 0",
        params![node_id, summary],
    )?;
    Ok(())
}

/// Walk the scope_id chain root → node. Cycle-guarded.
pub fn scope_chain(conn: &Connection, node_id: &str) -> Result<Vec<String>> {
    let mut chain = vec![node_id.to_string()];
    let mut seen: std::collections::HashSet<String> = [node_id.to_string()].into_iter().collect();
    let mut current = node_id.to_string();

    while let Some(parent) = conn
        .query_row(
            "SELECT scope_id FROM nodes WHERE id = ?1",
            params![current],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten()
    {
        if !seen.insert(parent.clone()) {
            break; // cycle
        }
        chain.push(parent.clone());
        current = parent;
    }
    chain.reverse(); // root first
    Ok(chain)
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextSection {
    pub node_id: String,
    pub node_name: String,
    pub instruction: Option<String>,
    pub summary: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct AssembledContext {
    /// Root-first instruction chain; innermost wins on conflict — the consumer
    /// should present them in this order so later (inner) overrides earlier.
    pub sections: Vec<ContextSection>,
    /// Facts from the bounded edge neighborhood, rendered with valid_from
    /// ("as of ..."), because undated personal facts age badly (§8.3).
    pub facts: Vec<String>,
    pub truncated: bool,
    pub budget_tokens: usize,
}

fn estimate_tokens(s: &str) -> usize {
    s.len() / 4
}

/// Assemble context for a node under an explicit token budget:
/// ~40% innermost scope / 35% rest of chain / 25% neighborhood facts, and
/// generated summaries are truncated before hand-written instructions (§4.5).
pub fn assemble_context(
    conn: &Connection,
    node_id: &str,
    budget_tokens: usize,
) -> Result<AssembledContext> {
    let chain = scope_chain(conn, node_id)?;
    let mut sections = Vec::new();
    let mut truncated = false;

    let inner_budget = budget_tokens * 40 / 100;
    let chain_budget = budget_tokens * 35 / 100;
    let fact_budget = budget_tokens.saturating_sub(inner_budget + chain_budget);

    let mut spent_chain = 0usize;
    for (i, id) in chain.iter().enumerate() {
        let is_innermost = i == chain.len() - 1;
        let budget = if is_innermost {
            inner_budget
        } else {
            chain_budget
        };
        let Some(node) = graph::get_node(conn, id)? else {
            continue;
        };
        let ctx = get_node_context(conn, id)?.unwrap_or_default();
        if ctx.instruction.is_empty() && ctx.summary.is_empty() {
            continue;
        }

        // Instruction outranks summary: truncate summary first, instruction last.
        let mut instruction = ctx.instruction.clone();
        let mut summary = ctx.summary.clone();
        let avail = if is_innermost {
            budget
        } else {
            budget.saturating_sub(spent_chain)
        };
        let instr_tokens = estimate_tokens(&instruction);
        if instr_tokens > avail {
            instruction.truncate(avail * 4);
            summary.clear();
            truncated = true;
        } else if instr_tokens + estimate_tokens(&summary) > avail {
            summary.truncate((avail - instr_tokens) * 4);
            truncated = true;
        }
        if !is_innermost {
            spent_chain += estimate_tokens(&instruction) + estimate_tokens(&summary);
        }

        sections.push(ContextSection {
            node_id: id.clone(),
            node_name: node.name,
            instruction: (!instruction.is_empty()).then_some(instruction),
            summary: (!summary.is_empty()).then_some(summary),
        });
    }

    // Neighborhood facts, bounded.
    let mut fact_lines = Vec::new();
    let mut spent_facts = 0usize;
    for f in fact::facts_for_node(conn, node_id, 50)? {
        // A denial is settled knowledge; mark it so a consumer cannot
        // mistake it for a weak positive.
        let neg = if f.polarity == "negative" {
            "[KNOWN FALSE] "
        } else {
            ""
        };
        let line = match &f.valid_from {
            Some(v) => format!("{neg}as of {}: {}", v, f.statement),
            None => format!("{neg}{}", f.statement),
        };
        let t = estimate_tokens(&line);
        if spent_facts + t > fact_budget {
            truncated = true;
            break;
        }
        spent_facts += t;
        fact_lines.push(line);
    }

    Ok(AssembledContext {
        sections,
        facts: fact_lines,
        truncated,
        budget_tokens,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::graph::{upsert_node, Node};

    #[test]
    fn test_scope_chain_and_context_composition() {
        let conn = open_memory().unwrap();

        let mut goal = Node::new("goal-r01", "goal", "Land R01");
        upsert_node(&conn, &goal).unwrap();
        let mut project = Node::new("proj-aim2", "project", "Aim 2");
        project.scope_id = Some("goal-r01".into());
        upsert_node(&conn, &project).unwrap();
        let mut task = Node::new("task-1", "task", "Email Nadia re: pilot data");
        task.scope_id = Some("proj-aim2".into());
        upsert_node(&conn, &task).unwrap();
        goal.scope_id = None;

        set_instruction(
            &conn,
            "goal-r01",
            "Grant writing: always tie to specific aims.",
        )
        .unwrap();
        set_instruction(&conn, "proj-aim2", "Cite APA. Lead with data.").unwrap();

        let chain = scope_chain(&conn, "task-1").unwrap();
        assert_eq!(chain, vec!["goal-r01", "proj-aim2", "task-1"]);

        let ctx = assemble_context(&conn, "task-1", 500).unwrap();
        assert_eq!(ctx.sections.len(), 2); // task itself has no context row
        assert_eq!(ctx.sections[0].node_id, "goal-r01"); // root first
        assert_eq!(ctx.sections[1].node_id, "proj-aim2");
    }

    #[test]
    fn test_scope_cycle_guard() {
        let conn = open_memory().unwrap();
        let mut a = Node::new("a", "area", "A");
        a.scope_id = Some("b".into());
        let mut b = Node::new("b", "area", "B");
        b.scope_id = Some("a".into());
        upsert_node(&conn, &a).unwrap();
        upsert_node(&conn, &b).unwrap();

        let chain = scope_chain(&conn, "a").unwrap();
        assert_eq!(chain.len(), 2, "cycle must terminate");
    }
}
