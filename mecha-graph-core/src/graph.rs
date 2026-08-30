//! Entity layer (§4.2). Lifted from FlowMail's `db/graph.rs` with the spec's
//! changes applied: `card_id` → `scope_id`, aliases promoted from a JSON column
//! to the indexed `node_alias` table, `node_emails` generalized to
//! `node_identifier`, and `edges` read through the fact-backed view.

use crate::error::Result;
use crate::ids::canonicalize;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

/// Closed node-type set (§4.2). Open-ended types cause junk-node explosion.
pub const NODE_TYPES: &[&str] = &[
    "person",
    "place",
    "org",
    "project",
    "goal",
    "area",
    "task",
    // Something that acts rather than someone who does. `mecha` is the only
    // one, and it exists so a task can wait on the agent without the agent
    // being filed as a person: `waiting_on` is delegation, and Linear rebuilt
    // their data model around the fact that an agent cannot be held
    // accountable the way a person can. Making it a `person` node would put
    // it in every people-shaped view — who owes me things, who I collaborate
    // with — and quietly answer "who is responsible" with the wrong kind of
    // thing.
    "agent",
    "event",
    "event_series",
    "topic",
    "artifact",
    "document",
];

fn default_confidence() -> f64 {
    1.0
}

fn default_source() -> String {
    "manual".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub node_type: String,
    pub name: String,
    pub canonical_name: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub properties: serde_json::Value,
    #[serde(default = "default_confidence")]
    pub confidence: f64,
    #[serde(default = "default_source")]
    pub source: String,
    pub source_ref: Option<String>,
    pub scope_id: Option<String>,
    #[serde(default)]
    pub access_count: i32,
    #[serde(default)]
    pub created_at: String,
    #[serde(default)]
    pub updated_at: String,
}

impl Node {
    pub fn new(id: &str, node_type: &str, name: &str) -> Self {
        Node {
            id: id.to_string(),
            node_type: node_type.to_string(),
            name: name.to_string(),
            canonical_name: canonicalize(name),
            aliases: vec![],
            properties: serde_json::json!({}),
            confidence: 1.0,
            source: "manual".to_string(),
            source_ref: None,
            scope_id: None,
            access_count: 0,
            created_at: String::new(),
            updated_at: String::new(),
        }
    }

    pub fn get_property(&self, key: &str) -> Option<&str> {
        self.properties.get(key)?.as_str()
    }

    pub fn set_property(&mut self, key: &str, value: serde_json::Value) {
        if let serde_json::Value::Object(ref mut map) = self.properties {
            map.insert(key.to_string(), value);
        }
    }
}

/// An edge is a row of the fact-backed `edges` view (§4.3).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub from_id: String,
    pub predicate: String,
    pub to_id: String,
    pub weight: f64,
    pub tags: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NeighborhoodEntry {
    pub node: Node,
    pub edge: Option<Edge>,
    pub depth: i32,
}

// ─── Row Mappers ─────────────────────────────────────────────────────────────

/// Map a `nodes` row. Aliases are NOT loaded here (separate table); use
/// [`load_aliases`] or the getters below, which populate them.
fn row_to_node_bare(row: &rusqlite::Row) -> std::result::Result<Node, rusqlite::Error> {
    let properties_str: String = row.get("properties")?;
    let properties: serde_json::Value = serde_json::from_str(&properties_str)
        .unwrap_or(serde_json::Value::Object(Default::default()));

    Ok(Node {
        id: row.get("id")?,
        node_type: row.get("node_type")?,
        name: row.get("name")?,
        canonical_name: row.get("canonical_name")?,
        aliases: vec![],
        properties,
        confidence: row.get("confidence")?,
        source: row.get("source")?,
        source_ref: row.get("source_ref")?,
        scope_id: row.get("scope_id")?,
        access_count: row.get("access_count")?,
        created_at: row.get("created_at")?,
        updated_at: row.get("updated_at")?,
    })
}

fn row_to_edge(row: &rusqlite::Row) -> std::result::Result<Edge, rusqlite::Error> {
    Ok(Edge {
        id: row.get("id")?,
        from_id: row.get("from_id")?,
        predicate: row.get("predicate")?,
        to_id: row.get("to_id")?,
        weight: row.get("weight")?,
        tags: row.get("tags")?,
    })
}

pub fn load_aliases(conn: &Connection, node_id: &str) -> Result<Vec<String>> {
    let mut stmt =
        conn.prepare_cached("SELECT alias FROM node_alias WHERE node_id = ?1 ORDER BY alias")?;
    let aliases = stmt
        .query_map(params![node_id], |r| r.get(0))?
        .collect::<std::result::Result<Vec<String>, _>>()?;
    Ok(aliases)
}

fn with_aliases(conn: &Connection, mut node: Node) -> Result<Node> {
    node.aliases = load_aliases(conn, &node.id)?;
    Ok(node)
}

// ─── Node CRUD ───────────────────────────────────────────────────────────────

pub fn get_node(conn: &Connection, id: &str) -> Result<Option<Node>> {
    let node = conn
        .query_row(
            "SELECT * FROM nodes WHERE id = ?1",
            params![id],
            row_to_node_bare,
        )
        .optional()?;
    node.map(|n| with_aliases(conn, n)).transpose()
}

/// Deterministic identity lookup (§4.2): email, phone, slack_uid, url, …
pub fn get_node_by_identifier(conn: &Connection, kind: &str, value: &str) -> Result<Option<Node>> {
    let node = conn
        .query_row(
            "SELECT n.* FROM nodes n
             JOIN node_identifier ni ON ni.node_id = n.id
             WHERE ni.kind = ?1 AND ni.value = ?2",
            params![kind, value],
            row_to_node_bare,
        )
        .optional()?;
    node.map(|n| with_aliases(conn, n)).transpose()
}

pub fn get_nodes_by_type(conn: &Connection, node_type: &str, limit: i64) -> Result<Vec<Node>> {
    let mut stmt =
        conn.prepare("SELECT * FROM nodes WHERE node_type = ?1 ORDER BY updated_at DESC LIMIT ?2")?;
    let nodes: Vec<Node> = stmt
        .query_map(params![node_type, limit], row_to_node_bare)?
        .collect::<std::result::Result<_, _>>()?;
    nodes.into_iter().map(|n| with_aliases(conn, n)).collect()
}

pub fn get_nodes_by_canonical(conn: &Connection, canonical_name: &str) -> Result<Vec<Node>> {
    let mut stmt = conn.prepare("SELECT * FROM nodes WHERE canonical_name = ?1")?;
    let nodes: Vec<Node> = stmt
        .query_map(params![canonical_name], row_to_node_bare)?
        .collect::<std::result::Result<_, _>>()?;
    nodes.into_iter().map(|n| with_aliases(conn, n)).collect()
}

/// Upsert a node and replace its alias set. `node_type` must be in the closed
/// set (§4.2) — extractors should return null rather than invent types.
pub fn upsert_node(conn: &Connection, node: &Node) -> Result<()> {
    if !NODE_TYPES.contains(&node.node_type.as_str()) {
        return Err(crate::error::Error::Other(format!(
            "node_type '{}' not in closed set {:?}",
            node.node_type, NODE_TYPES
        )));
    }
    let properties_json =
        serde_json::to_string(&node.properties).unwrap_or_else(|_| "{}".to_string());

    conn.execute(
        "INSERT INTO nodes (id, node_type, name, canonical_name, properties, confidence, source, source_ref, scope_id, access_count)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(id) DO UPDATE SET
             node_type = excluded.node_type,
             name = excluded.name,
             canonical_name = excluded.canonical_name,
             properties = excluded.properties,
             confidence = excluded.confidence,
             source_ref = excluded.source_ref,
             scope_id = excluded.scope_id,
             access_count = excluded.access_count,
             updated_at = datetime('now')",
        params![
            node.id,
            node.node_type,
            node.name,
            node.canonical_name,
            properties_json,
            node.confidence,
            node.source,
            node.source_ref,
            node.scope_id,
            node.access_count,
        ],
    )?;

    for alias in &node.aliases {
        add_alias(conn, &node.id, alias, "manual")?;
    }
    Ok(())
}

pub fn delete_node(conn: &Connection, id: &str) -> Result<()> {
    conn.execute("DELETE FROM nodes WHERE id = ?1", params![id])?;
    Ok(())
}

/// A live-typeahead suggestion: the node plus what matched ("iris.calder.gr"
/// may be the string that hit, while the node is named "Iris Calder").
#[derive(Debug, Clone, serde::Serialize)]
pub struct Suggestion {
    pub node: Node,
    /// The name/alias/identifier that matched the partial input.
    pub matched: String,
    pub via: &'static str, // name | alias | identifier
}

/// Ranked prefix/substring suggestions for a partial entity name — the
/// substrate for typeahead lookup and ghost-text field completion. Matches
/// names, aliases, and identifiers (emails, slack UIDs); event/document
/// nodes are excluded (never query anchors, §8.1). Rank: match tier (exact,
/// prefix, substring) → type priority (people first) → access_count.
pub fn suggest_entities(conn: &Connection, partial: &str, limit: usize) -> Result<Vec<Suggestion>> {
    let canonical = canonicalize(partial);
    if canonical.len() < 2 {
        return Ok(vec![]);
    }
    let escaped = canonical
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let prefix = format!("{escaped}%");
    let infix = format!("%{escaped}%");

    // (tier, matched, via, node) — tier 0 exact, 1 prefix, 2 substring.
    let mut best: std::collections::HashMap<String, (u8, String, &'static str, Node)> =
        std::collections::HashMap::new();
    let mut consider = |tier: u8, matched: String, via: &'static str, node: Node| {
        if ANCHOR_LAST_TYPES.contains(&node.node_type.as_str()) {
            return;
        }
        match best.get(&node.id) {
            Some((t, ..)) if *t <= tier => {}
            _ => {
                best.insert(node.id.clone(), (tier, matched, via, node));
            }
        }
    };

    {
        let mut stmt = conn.prepare_cached(
            "SELECT * FROM nodes WHERE canonical_name LIKE ?1 ESCAPE '\\' LIMIT 50",
        )?;
        let nodes: Vec<Node> = stmt
            .query_map(params![infix], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        for n in nodes {
            let tier = if n.canonical_name == canonical {
                0
            } else if n.canonical_name.starts_with(&canonical) {
                1
            } else {
                2
            };
            let matched = n.name.clone();
            consider(tier, matched, "name", n);
        }
    }
    {
        let mut stmt = conn.prepare_cached(
            "SELECT a.alias, n.* FROM node_alias a JOIN nodes n ON n.id = a.node_id
             WHERE a.alias LIKE ?1 ESCAPE '\\' LIMIT 50",
        )?;
        let rows: Vec<(String, Node)> = stmt
            .query_map(params![infix], |r| {
                Ok((r.get::<_, String>(0)?, row_to_node_bare(r)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        for (alias, n) in rows {
            let tier = if alias == canonical {
                0
            } else if alias.starts_with(&canonical) {
                1
            } else {
                2
            };
            consider(tier, alias, "alias", n);
        }
    }
    {
        let mut stmt = conn.prepare_cached(
            "SELECT i.value, n.* FROM node_identifier i JOIN nodes n ON n.id = i.node_id
             WHERE i.value LIKE ?1 ESCAPE '\\' LIMIT 50",
        )?;
        let rows: Vec<(String, Node)> = stmt
            .query_map(params![prefix], |r| {
                Ok((r.get::<_, String>(0)?, row_to_node_bare(r)?))
            })?
            .collect::<std::result::Result<_, _>>()?;
        for (value, n) in rows {
            let tier = if value == canonical { 0 } else { 1 };
            consider(tier, value, "identifier", n);
        }
    }

    let mut out: Vec<(u8, String, &'static str, Node)> = best.into_values().collect();
    out.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(type_priority(&a.3.node_type).cmp(&type_priority(&b.3.node_type)))
            .then(b.3.access_count.cmp(&a.3.access_count))
            .then(a.3.name.cmp(&b.3.name))
    });
    out.truncate(limit);
    out.into_iter()
        .map(|(_, matched, via, node)| {
            Ok(Suggestion {
                node: with_aliases(conn, node)?,
                matched,
                via,
            })
        })
        .collect()
}

pub fn increment_node_access(conn: &Connection, id: &str) -> Result<()> {
    conn.execute(
        "UPDATE nodes SET access_count = access_count + 1, updated_at = datetime('now') WHERE id = ?1",
        params![id],
    )?;
    Ok(())
}

// ─── Aliases & Identifiers ───────────────────────────────────────────────────

pub fn add_alias(conn: &Connection, node_id: &str, alias: &str, source: &str) -> Result<()> {
    let alias = canonicalize(alias);
    if alias.is_empty() {
        return Ok(());
    }
    conn.execute(
        "INSERT OR IGNORE INTO node_alias (node_id, alias, source) VALUES (?1, ?2, ?3)",
        params![node_id, alias, source],
    )?;
    Ok(())
}

/// Two sources asserting the same identifier are the same entity — this table
/// is the deterministic merge substrate (§4.2). Values must be normalized by
/// the caller (lowercase email, E.164 phone, canonical URL).
/// The graph's owner — the person whose life this is. Explicit, never
/// guessed: mechanisms that need "who is this graph about" (implied-subject
/// binding for wearable claims, coverage measurement) read it here rather
/// than each inventing a heuristic. Stored as a node_identifier
/// (kind 'self', value 'owner'), so it survives merges like any identifier.
pub fn owner_node(conn: &Connection) -> Result<Option<Node>> {
    get_node_by_identifier(conn, "self", "owner")
}

/// Set (or move) the owner mark. One owner at a time, by construction.
pub fn set_owner(conn: &Connection, node_id: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM node_identifier WHERE kind = 'self' AND value = 'owner'",
        [],
    )?;
    upsert_identifier(conn, "self", "owner", node_id, "owner")
}

pub fn upsert_identifier(
    conn: &Connection,
    kind: &str,
    value: &str,
    node_id: &str,
    source: &str,
) -> Result<()> {
    conn.execute(
        "INSERT OR IGNORE INTO node_identifier (kind, value, node_id, source) VALUES (?1, ?2, ?3, ?4)",
        params![kind, value, node_id, source],
    )?;
    Ok(())
}

// ─── Entity Resolution ───────────────────────────────────────────────────────

/// Node types that are retrieval targets rather than identity anchors: they
/// only resolve when nothing better matches. Keeps a calendar event titled
/// "Nadia" from polluting resolution of the person Nadia.
const ANCHOR_LAST_TYPES: &[&str] = &["event", "event_series", "document", "artifact"];

fn type_priority(t: &str) -> usize {
    // Lower = shown first.
    const ORDER: &[&str] = &[
        "person",
        "org",
        "project",
        "goal",
        "area",
        "task",
        "topic",
        "place",
        "event_series",
        "event",
        "artifact",
        "document",
    ];
    ORDER.iter().position(|x| *x == t).unwrap_or(ORDER.len())
}

/// Resolve a name/alias to ALL matching nodes. Ambiguity is a feature (§8.1):
/// callers surface multiple matches rather than silently guessing. Results
/// are ordered person-first, and event/document nodes are suppressed when a
/// stronger anchor type also matches.
pub fn resolve_entity_all(conn: &Connection, name_or_alias: &str) -> Result<Vec<Node>> {
    let canonical = canonicalize(name_or_alias);
    // An empty name must resolve to nothing: the fuzzy tier below would turn
    // it into LIKE '%%' and hand back five arbitrary nodes.
    if canonical.is_empty() {
        return Ok(Vec::new());
    }
    let mut out: Vec<Node> = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // 1. Exact canonical match
    {
        let mut stmt = conn.prepare_cached("SELECT * FROM nodes WHERE canonical_name = ?1")?;
        let nodes: Vec<Node> = stmt
            .query_map(params![canonical], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        for n in nodes {
            if seen.insert(n.id.clone()) {
                out.push(with_aliases(conn, n)?);
            }
        }
    }

    // 2. Alias table (indexed — this is why aliases are not a JSON column)
    {
        let mut stmt = conn.prepare_cached(
            "SELECT n.* FROM nodes n JOIN node_alias a ON a.node_id = n.id WHERE a.alias = ?1",
        )?;
        let nodes: Vec<Node> = stmt
            .query_map(params![canonical], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        for n in nodes {
            if seen.insert(n.id.clone()) {
                out.push(with_aliases(conn, n)?);
            }
        }
    }

    // 3. Identifier (e.g. resolving an email address directly)
    {
        let mut stmt = conn.prepare_cached(
            "SELECT n.* FROM nodes n JOIN node_identifier i ON i.node_id = n.id WHERE i.value = ?1",
        )?;
        let nodes: Vec<Node> = stmt
            .query_map(params![canonical], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        for n in nodes {
            if seen.insert(n.id.clone()) {
                out.push(with_aliases(conn, n)?);
            }
        }
    }

    // 4. Fuzzy name LIKE fallback — when nothing matched, or when only
    // retrieval-target types (events named after people) matched.
    let only_anchor_last = !out.is_empty()
        && out
            .iter()
            .all(|n| ANCHOR_LAST_TYPES.contains(&n.node_type.as_str()));
    if out.is_empty() || only_anchor_last {
        let fuzzy = format!("%{}%", canonical);
        // Anchor types first, and the ordering is the whole correctness of
        // this tier. The rule below already says events must not shadow
        // people — but it can only drop what the LIMIT let through, and an
        // unordered `LIMIT 5` over 13,400 calendar events fills all five
        // slots before a person is reached. Measured: searching a surname
        // returned five events and no people at all, so `/entity` could not
        // find two nodes to merge and the drop-rule below had nothing to
        // drop *to*. Ordering makes the existing rule reachable rather than
        // replacing it.
        let mut stmt = conn.prepare_cached(
            "SELECT * FROM nodes WHERE canonical_name LIKE ?1
             ORDER BY CASE WHEN node_type IN ('event','event_series','document','artifact')
                           THEN 1 ELSE 0 END,
                      access_count DESC, updated_at DESC
             LIMIT 5",
        )?;
        let nodes: Vec<Node> = stmt
            .query_map(params![fuzzy], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        for n in nodes {
            if seen.insert(n.id.clone()) {
                out.push(with_aliases(conn, n)?);
            }
        }
    }

    // Anchor types outrank retrieval-target types; drop events/documents
    // entirely when something stronger also matched.
    if out
        .iter()
        .any(|n| !ANCHOR_LAST_TYPES.contains(&n.node_type.as_str()))
    {
        out.retain(|n| !ANCHOR_LAST_TYPES.contains(&n.node_type.as_str()));
    }
    out.sort_by(|a, b| {
        type_priority(&a.node_type)
            .cmp(&type_priority(&b.node_type))
            .then(b.access_count.cmp(&a.access_count))
    });

    Ok(out)
}

/// Single-result resolution: first match, if any.
pub fn resolve_entity(conn: &Connection, name_or_alias: &str) -> Result<Option<Node>> {
    Ok(resolve_entity_all(conn, name_or_alias)?.into_iter().next())
}

/// One person node whose display name is an identifier rather than a name.
#[derive(Debug, Clone, serde::Serialize)]
pub struct NameFix {
    pub node_id: String,
    pub from: String,
    pub to: String,
}

/// Title-case a lowercased alias for display. Aliases are stored folded,
/// so this is the only way back to a name that reads like one.
///
/// Particles stay lowercase ("van der berg" → "van der Berg") and single
/// letters become initials ("v r hale" → "V R Hale"). It is a
/// heuristic and will occasionally be wrong; that is tolerable because
/// the fold-cased alias is preserved and the fix is one UPDATE to undo,
/// whereas leaving an email where a name belongs is wrong every time.
fn title_case_name(s: &str) -> String {
    const PARTICLES: &[&str] = &[
        "van", "der", "den", "de", "di", "da", "del", "la", "le", "von", "bin", "al",
    ];
    s.split_whitespace()
        .enumerate()
        .map(|(i, w)| {
            if w.chars().count() == 1 {
                return w.to_uppercase();
            }
            if i > 0 && PARTICLES.contains(&w) {
                return w.to_string();
            }
            let mut c = w.chars();
            match c.next() {
                Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Promote a human alias to `name` for person nodes named by an email.
///
/// These arise in `get_or_create_person` when a source supplies an
/// address with no display name — the node becomes
/// `veraholt@example.com` and "vera holt" lives only as an alias.
/// Everything that renders a person then shows an address: context
/// packs, summaries, `kg_entity`, the TUI. It can also read as an
/// identity split, because a question about
/// `iris.calder@example.com` meets a fact reading "Iris Calder works at
/// Westfield" and the model will not bridge them.
///
/// Safe because the address keeps resolving: it is stored in
/// `node_identifier` (resolution path 3) and re-added here as an alias
/// (path 2), so only path 1 — canonical_name — changes hands.
///
/// Skips rather than guesses when the target name is already some other
/// node's canonical name: that is a merge question, and renaming into a
/// collision would manufacture ambiguity where none existed.
pub fn promote_human_names(
    conn: &Connection,
    dry_run: bool,
) -> Result<(Vec<NameFix>, Vec<String>)> {
    let candidates: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT id, name FROM nodes
             WHERE node_type = 'person' AND name LIKE '%@%'
             ORDER BY name",
        )?;
        let rows = stmt
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };

    let mut fixes = vec![];
    let mut skipped = vec![];
    for (node_id, old) in candidates {
        // The best human alias: multi-word, not an address, longest.
        let best = load_aliases(conn, &node_id)?
            .into_iter()
            .filter(|a| !a.contains('@') && a.contains(' ') && a.len() >= 5)
            .max_by_key(|a| a.len());
        let Some(best) = best else {
            skipped.push(format!("{old}: no multi-word human alias to promote"));
            continue;
        };
        let new_name = title_case_name(&best);
        let new_canon = canonicalize(&new_name);

        // The same definition the interactive verbs use, so a bulk pass can
        // never quietly do what `rename_node` refuses.
        if let Some(other) = canonical_collision(conn, &new_canon, Some(&node_id))? {
            skipped.push(format!(
                "{old} → {new_name}: that name is already {} ({}) — a merge question, not a rename",
                other.name, other.id
            ));
            continue;
        }

        if !dry_run {
            // Keep the address findable by name as well as by identifier.
            add_alias(conn, &node_id, &old, "email-name")?;
            conn.execute(
                "UPDATE nodes SET name = ?2, canonical_name = ?3, updated_at = datetime('now')
                 WHERE id = ?1",
                params![node_id, new_name, new_canon],
            )?;
        }
        fixes.push(NameFix {
            node_id,
            from: old,
            to: new_name,
        });
    }
    Ok((fixes, skipped))
}

/// Is `canonical` already some *other* node's canonical name?
///
/// The one definition behind every refusal in this family, so `rename_node`,
/// `create_person` and `promote_human_names` cannot drift into disagreeing
/// about what a collision is — which would show up as a bulk pass quietly
/// doing what the interactive verb refuses.
///
/// Deliberately **canonical names only, never aliases**. Two nodes sharing a
/// canonical name is ambiguity that did not exist a moment ago; a name that
/// merely collides with some node's *alias* is ambiguity that already
/// existed, and refusing there would block the ordinary case of giving a
/// node the name it is already aliased by — which is exactly the repair
/// this was written for.
fn canonical_collision(
    conn: &Connection,
    canonical: &str,
    exclude_id: Option<&str>,
) -> Result<Option<Node>> {
    let mut stmt =
        conn.prepare_cached("SELECT * FROM nodes WHERE canonical_name = ?1 AND id != ?2 LIMIT 1")?;
    let found: Vec<Node> = stmt
        .query_map(
            params![canonical, exclude_id.unwrap_or("")],
            row_to_node_bare,
        )?
        .collect::<std::result::Result<_, _>>()?;
    Ok(found.into_iter().next())
}

/// Rename a node, keeping every existing way of reaching it.
///
/// **The old name becomes an alias**, which is the whole reason this is safe
/// to offer interactively: resolution path 1 (canonical_name) changes hands,
/// paths 2 (alias) and 3 (identifier) do not, so every episode, query and
/// habit that reached the node by its old name still reaches it. Nothing is
/// rewritten anywhere else — facts store node *ids*, so the statements
/// already recorded keep their wording, which is correct: a fact is a record
/// of what a source said, not a view that should silently restate itself.
///
/// Refuses rather than guesses when the new name is already another node's
/// canonical name: that is a merge question (`merge_nodes`), and renaming
/// into a collision would manufacture ambiguity where none existed. The
/// rule and its wording are shared with `promote_human_names`, the bulk
/// pass that has always had it.
pub fn rename_node(conn: &Connection, node_id: &str, new_name: &str) -> Result<NameFix> {
    let new_name = new_name.trim();
    if new_name.is_empty() {
        return Err(crate::error::Error::Other(
            "a node needs a name; refusing to rename to nothing".into(),
        ));
    }
    let Some(node) = get_node(conn, node_id)? else {
        return Err(crate::error::Error::Other(format!(
            "no node with id {node_id}"
        )));
    };
    let new_canon = canonicalize(new_name);
    if new_canon == node.canonical_name && new_name == node.name {
        return Err(crate::error::Error::Other(format!(
            "{} is already named {new_name:?}",
            node.id
        )));
    }
    if let Some(other) = canonical_collision(conn, &new_canon, Some(node_id))? {
        return Err(crate::error::Error::Other(format!(
            "{} → {new_name:?}: that name is already {} ({}) — a merge question, not a rename. \
             If they are the same, use `merge {} {}`.",
            node.name, other.name, other.id, other.id, node.id
        )));
    }

    // Before the update, so a failure here leaves the node reachable by the
    // name it still has.
    add_alias(conn, node_id, &node.name, "rename")?;
    conn.execute(
        "UPDATE nodes SET name = ?2, canonical_name = ?3, updated_at = datetime('now')
         WHERE id = ?1",
        params![node_id, new_name, new_canon],
    )?;
    Ok(NameFix {
        node_id: node_id.to_string(),
        from: node.name,
        to: new_name.to_string(),
    })
}

/// Create a person node that nothing in the graph proposed.
///
/// The gap this fills: a person can be the subject of forty facts and have
/// no node, because nodes are minted by ingest (`get_or_create_person`, off
/// an email or an attendee list) and a person who only ever appears in
/// spoken conversation has neither. There was no way to say "this person
/// exists" — and no way to reach one either, since `merge_nodes` keeps the
/// survivor's name, so the workaround for a bad name needed a node that only
/// this function can make.
///
/// **Stricter about collisions than `rename_node`, on purpose.** A rename
/// moves a name that already has evidence behind it; a create invents a node
/// with none, so it must not land on top of a name that already resolves
/// exactly — including by alias, which a rename tolerates. Inventing a
/// second "Wren" is not a repair.
///
/// **And it checks exact resolution, never `resolve_entity_all`.** That
/// function's fourth tier is a `LIKE '%name%'` fallback, so "Wren" resolves
/// today to the *event* "SPSP Wrench Reunion" — a substring hit. Refusing on
/// a fuzzy match would make the missing-person case, the one case this
/// exists for, the one case it cannot serve.
pub fn create_person(conn: &Connection, name: &str, source: &str) -> Result<Node> {
    create_node(conn, "person", name, source)
}

/// Create a node of any type in the closed set that nothing in the graph
/// proposed. `create_person` is the common case spelled short.
///
/// **The first-name alias is a person-only nicety and stays that way.**
/// Spoken sources say "Marisol" and mean a person; nobody says
/// "Psychological" and means the department. Minting one for an org would
/// hand every multi-word institution a one-word magnet for unrelated text —
/// which is the exact mechanism that took three repairs to undo today.
pub fn create_node(conn: &Connection, node_type: &str, name: &str, source: &str) -> Result<Node> {
    let name = name.trim();
    if !NODE_TYPES.contains(&node_type) {
        return Err(crate::error::Error::Other(format!(
            "node_type '{node_type}' not in closed set {NODE_TYPES:?}"
        )));
    }
    if name.is_empty() {
        return Err(crate::error::Error::Other(format!(
            "a node needs a name; refusing to create an unnamed {node_type}"
        )));
    }
    let canonical = canonicalize(name);
    if let Some(other) = canonical_collision(conn, &canonical, None)? {
        return Err(crate::error::Error::Other(format!(
            "{name:?} is already {} ({}) — nothing to create. \
             Rename it if the name is wrong, or alias it if this is another way of saying it.",
            other.name, other.id
        )));
    }
    let by_alias: Vec<Node> = {
        let mut stmt = conn.prepare_cached(
            "SELECT n.* FROM nodes n JOIN node_alias a ON a.node_id = n.id WHERE a.alias = ?1",
        )?;
        let rows: Vec<Node> = stmt
            .query_map(params![canonical], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        rows
    };
    if let Some(other) = by_alias.into_iter().next() {
        return Err(crate::error::Error::Other(format!(
            "{name:?} already resolves to {} ({}) as an alias — creating a second node \
             would split that name across two. Rename that node if it is the one you mean.",
            other.name, other.id
        )));
    }

    let id = format!("{node_type}-{}", uuid::Uuid::new_v4());
    let mut node = Node::new(&id, node_type, name);
    node.source = source.to_string();
    upsert_node(conn, &node)?;
    // The first-name alias, on `get_or_create_person`'s reasoning: spoken
    // sources and queries use bare first names, and a collision there is
    // what the disambiguation envelope is for. People only — see above.
    if node_type == "person" {
        if let Some(first) = name.split_whitespace().next() {
            if first.len() >= 3 && first != name {
                add_alias(conn, &id, first, "firstname")?;
            }
        }
    }
    get_node(conn, &id).map(|n| n.unwrap())
}

/// Change a node's type, keeping its id and everything hanging off it.
///
/// The type is not cosmetic: `resolve_entity_all` ranks anchor types above
/// retrieval targets, so an org filed as a `topic` loses to an event of the
/// same name, and a calendar resource filed as a `person` shows up in
/// people-shaped answers. Both were real in this graph — the Ostrander Brain
/// Imaging Center was a topic, and several `@group.calendar.google.com`
/// addresses were people.
///
/// A rename with a `create` on the side would work and is worse: the node
/// would keep its old id nowhere, so every fact, mention and rollup pointing
/// at it would have to be moved, and a partial move is how a repair becomes
/// a second problem. Changing one column moves nothing.
pub fn retype_node(conn: &Connection, node_id: &str, node_type: &str) -> Result<(String, String)> {
    if !NODE_TYPES.contains(&node_type) {
        return Err(crate::error::Error::Other(format!(
            "node_type '{node_type}' not in closed set {NODE_TYPES:?}"
        )));
    }
    let Some(node) = get_node(conn, node_id)? else {
        return Err(crate::error::Error::Other(format!(
            "no node with id {node_id}"
        )));
    };
    if node.node_type == node_type {
        return Err(crate::error::Error::Other(format!(
            "{} is already a {node_type}",
            node.name
        )));
    }
    conn.execute(
        "UPDATE nodes SET node_type = ?2, updated_at = datetime('now') WHERE id = ?1",
        params![node_id, node_type],
    )?;
    // The id keeps its old prefix, and that is deliberate. It is an opaque
    // key that every fact, mention and rollup already references; rewriting
    // it to match the new type would mean rewriting all of them to gain a
    // string nobody resolves on.
    Ok((node.node_type, node_type.to_string()))
}

/// Is `name` free for `exclude_id` to take? The public face of
/// `canonical_collision`, for callers that need to know *before* proposing a
/// rename — a proposal that cannot be applied is worse than no proposal.
pub fn canonical_collision_free_name(
    conn: &Connection,
    name: &str,
    exclude_id: &str,
) -> Result<bool> {
    Ok(canonical_collision(conn, &canonicalize(name), Some(exclude_id))?.is_none())
}

/// Title-case a human name, particles and initials handled. Public so a
/// proposer can show the name it would apply rather than a lowercased alias.
pub fn title_case_public(s: &str) -> String {
    title_case_name(s)
}

/// Remove one alias from a node. Returns whether a row was actually there.
///
/// The counterpart `rename_node` needs and deliberately does not do itself.
/// Renaming keeps the old name as an alias because the usual case is *the
/// name was wrong* — everything that reached the node by it should keep
/// doing so. The case this serves is the other one: **the name belonged to
/// somebody else**, and keeping it would preserve exactly the conflation
/// being undone. Two different repairs, so two verbs rather than a flag.
pub fn remove_alias(conn: &Connection, node_id: &str, alias: &str) -> Result<bool> {
    let alias = canonicalize(alias);
    let n = conn.execute(
        "DELETE FROM node_alias WHERE node_id = ?1 AND alias = ?2",
        params![node_id, alias],
    )?;
    Ok(n > 0)
}

/// Move a deterministic identifier — an email, a handle — to another node.
///
/// An identifier is the strongest claim in the graph: `get_or_create_person`
/// resolves on it before anything else, so it decides where *future* ingest
/// lands. Splitting two people apart without moving it means the next email
/// re-merges them, which is why this is a verb and not a manual step.
///
/// The destination must exist. A missing target would otherwise orphan the
/// identifier into a row pointing at nothing, and the foreign key would take
/// the whole transaction with it at a confusing moment.
pub fn move_identifier(conn: &Connection, kind: &str, value: &str, to_node: &str) -> Result<()> {
    let value = value.trim().to_lowercase();
    if get_node(conn, to_node)?.is_none() {
        return Err(crate::error::Error::Other(format!(
            "no node with id {to_node}"
        )));
    }
    let n = conn.execute(
        "UPDATE node_identifier SET node_id = ?3 WHERE kind = ?1 AND value = ?2",
        params![kind, value, to_node],
    )?;
    if n == 0 {
        return Err(crate::error::Error::Other(format!(
            "no {kind} identifier {value:?} on any node"
        )));
    }
    Ok(())
}

/// Move one episode's mention from one node to another.
///
/// Keyed on the episode's **uid** rather than its rowid, because a uid is
/// what every other surface prints and what a person can copy out of a
/// listing; rowids are an implementation detail nobody should be asked to
/// handle.
///
/// `INSERT OR REPLACE` rather than `UPDATE`: the primary key is
/// `(episode_id, node_id)`, so an update collides when the destination
/// already mentions that episode — which is the ordinary case when a split
/// is putting an episode where a node already sits. The old row is dropped
/// either way, which is the whole intent.
pub fn move_mention(conn: &Connection, episode_uid: &str, from: &str, to: &str) -> Result<()> {
    let episode_id: i64 = conn
        .query_row(
            "SELECT id FROM episode WHERE uid = ?1",
            params![episode_uid],
            |r| r.get(0),
        )
        .map_err(|_| crate::error::Error::Other(format!("no episode with uid {episode_uid}")))?;
    if get_node(conn, to)?.is_none() {
        return Err(crate::error::Error::Other(format!("no node with id {to}")));
    }
    let row: Option<(String, f64)> = conn
        .query_row(
            "SELECT extractor, confidence FROM mention WHERE episode_id = ?1 AND node_id = ?2",
            params![episode_id, from],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((extractor, confidence)) = row else {
        return Err(crate::error::Error::Other(format!(
            "{from} does not mention episode {episode_uid}"
        )));
    };
    conn.execute(
        "INSERT OR REPLACE INTO mention (episode_id, node_id, extractor, confidence)
         VALUES (?1, ?2, ?3, ?4)",
        params![episode_id, to, extractor, confidence],
    )?;
    conn.execute(
        "DELETE FROM mention WHERE episode_id = ?1 AND node_id = ?2",
        params![episode_id, from],
    )?;
    Ok(())
}

/// What a bulk reattribution actually did.
///
/// `blocked` is the count that could not move because the destination
/// already holds an identical live fact — reported rather than resolved,
/// because the two ways to resolve it are folding observation counts (which
/// `merge_nodes` may do, since it is about to delete the node anyway) and
/// deleting evidence, and neither should happen silently inside a partial
/// move.
#[derive(Debug, Default, PartialEq)]
pub struct Reattributed {
    pub subjects: usize,
    pub objects: usize,
    pub self_loops: usize,
    pub blocked: usize,
}

/// Re-point every fact endpoint from one node to another, leaving both nodes
/// in place.
///
/// This is the half of `merge_nodes` that a *contamination* needs and a
/// merge does not. The case: a fuzzy substring match made an event node the
/// subject of twenty-one facts about a person — "Wren is a twin daughter"
/// filed under "SPSP Wrench Reunion" — so the facts are true and only their
/// endpoint is wrong. A merge would be the wrong tool twice over: it would
/// destroy the event, which is a real event, and it would carry across the
/// one mention that genuinely belongs to it.
///
/// Self-loops are dropped **before** the re-point rather than after. A fact
/// linking the two nodes becomes `X → X` once they are the same, which is
/// meaningless — but deleting `subject_id = to AND object_id = to`
/// afterwards would also take any self-loop the destination already had.
/// Cutting the ones that this move would create is precise; cleaning up
/// afterwards is not.
pub fn move_facts(conn: &Connection, from: &str, to: &str) -> Result<Reattributed> {
    if from == to {
        return Err(crate::error::Error::Other(
            "cannot move a node's facts onto itself".into(),
        ));
    }
    if get_node(conn, to)?.is_none() {
        return Err(crate::error::Error::Other(format!("no node with id {to}")));
    }
    if get_node(conn, from)?.is_none() {
        return Err(crate::error::Error::Other(format!(
            "no node with id {from}"
        )));
    }

    let tx = conn.is_autocommit();
    if tx {
        conn.execute_batch("BEGIN;")?;
    }
    let result = (|| -> Result<Reattributed> {
        // Bound to locals in execution order rather than built as a struct
        // literal: the sequence is load-bearing — the self-loop cut must
        // happen *before* the re-point — and a literal invites a future
        // reorder that silently changes what runs first.
        let self_loops = conn.execute(
            "DELETE FROM fact
             WHERE (subject_id = ?1 AND object_id = ?2)
                OR (subject_id = ?2 AND object_id = ?1)",
            params![from, to],
        )?;
        // OR IGNORE for the live-unique index, exactly as the merge path does.
        let subjects = conn.execute(
            "UPDATE OR IGNORE fact SET subject_id = ?2 WHERE subject_id = ?1",
            params![from, to],
        )?;
        let objects = conn.execute(
            "UPDATE OR IGNORE fact SET object_id = ?2 WHERE object_id = ?1",
            params![from, to],
        )?;
        let blocked = conn.query_row(
            "SELECT COUNT(*) FROM fact WHERE subject_id = ?1 OR object_id = ?1",
            params![from],
            |r| r.get::<_, i64>(0),
        )? as usize;
        Ok(Reattributed {
            subjects,
            objects,
            self_loops,
            blocked,
        })
    })();
    if tx {
        match &result {
            Ok(_) => conn.execute_batch("COMMIT;")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK;");
            }
        }
    }
    result
}

/// Re-point mentions in bulk, optionally narrowed to one extractor and/or
/// one episode source. Returns (moved, dropped-as-redundant).
///
/// The narrowing is the point. A contaminated node is usually not *entirely*
/// contaminated: the SPSP reunion event had 474 mentions, of which 473 were
/// `llm` hits from bee episodes about a toddler and exactly one was the
/// `attendee` row that is genuinely the event's. `--extractor llm` is what
/// separates them, and it separates them by *how the graph came to believe
/// it* rather than by anything a reader has to judge case by case.
///
/// A mention that cannot move because the destination already mentions that
/// episode is **dropped** rather than left behind, and this is the one place
/// here that deletes. It is safe because the row is redundant by
/// construction — the same (episode, node) pair already exists on the
/// destination, which is the whole state the move was trying to reach.
/// Leaving it would keep the contaminated node showing mentions the repair
/// was meant to take away.
pub fn move_mentions(
    conn: &Connection,
    from: &str,
    to: &str,
    extractor: Option<&str>,
    source: Option<&str>,
) -> Result<(usize, usize)> {
    if from == to {
        return Err(crate::error::Error::Other(
            "cannot move a node's mentions onto itself".into(),
        ));
    }
    if get_node(conn, to)?.is_none() {
        return Err(crate::error::Error::Other(format!("no node with id {to}")));
    }
    // Both placeholders are always bound and always present in the SQL:
    // rusqlite counts parameters strictly, so a clause built in only when
    // its option is Some leaves the binding list the wrong length. An empty
    // string is the "no filter" value, which no real extractor or source
    // can collide with.
    let filter = " AND (?3 = '' OR extractor = ?3) \
                  AND (?4 = '' OR episode_id IN (SELECT id FROM episode WHERE source = ?4))";
    let ex = extractor.unwrap_or_default();
    let src = source.unwrap_or_default();

    let tx = conn.is_autocommit();
    if tx {
        conn.execute_batch("BEGIN;")?;
    }
    let result = (|| -> Result<(usize, usize)> {
        let moved = conn.execute(
            &format!("UPDATE OR IGNORE mention SET node_id = ?2 WHERE node_id = ?1{filter}"),
            params![from, to, ex, src],
        )?;
        let dropped = conn.execute(
            &format!(
                "DELETE FROM mention WHERE node_id = ?1{filter}
                 AND episode_id IN (SELECT episode_id FROM mention WHERE node_id = ?2)"
            ),
            params![from, to, ex, src],
        )?;
        Ok((moved, dropped))
    })();
    if tx {
        match &result {
            Ok(_) => conn.execute_batch("COMMIT;")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK;");
            }
        }
    }
    result
}

/// Get-or-create a person node keyed by a deterministic identifier (Tier 1).
/// If the identifier is known, returns the existing node (adding the display
/// name as an alias); otherwise creates a new person node.
pub fn get_or_create_person(
    conn: &Connection,
    email: Option<&str>,
    display_name: &str,
    source: &str,
) -> Result<Node> {
    let email_norm = email.map(|e| e.trim().to_lowercase());

    if let Some(ref em) = email_norm {
        if let Some(node) = get_node_by_identifier(conn, "email", em)? {
            if !display_name.is_empty() {
                add_alias(conn, &node.id, display_name, "attendee")?;
            }
            return get_node(conn, &node.id).map(|n| n.unwrap());
        }
    }

    // No identifier match: try exact name resolution among persons before creating.
    if email_norm.is_none() && !display_name.is_empty() {
        let existing = resolve_entity_all(conn, display_name)?
            .into_iter()
            .find(|n| n.node_type == "person");
        if let Some(node) = existing {
            return Ok(node);
        }
    }

    let name = if display_name.is_empty() {
        email_norm.clone().unwrap_or_default()
    } else {
        display_name.to_string()
    };
    let id = format!("person-{}", uuid::Uuid::new_v4());
    let mut node = Node::new(&id, "person", &name);
    node.source = source.to_string();
    upsert_node(conn, &node)?;
    if !display_name.is_empty() {
        add_alias(conn, &id, display_name, source)?;
        // First name too: spoken sources (Bee) and queries use bare first
        // names. Collisions ("June" ×2) are fine — that's what the
        // disambiguation envelope is for (§8.1).
        if let Some(first) = display_name.split_whitespace().next() {
            if first.len() >= 3 {
                add_alias(conn, &id, first, "firstname")?;
            }
        }
    }
    if let Some(ref em) = email_norm {
        upsert_identifier(conn, "email", em, &id, source)?;
        // The local part of the email is a weak alias but useful for matching.
        if let Some(local) = em.split('@').next() {
            add_alias(conn, &id, local, source)?;
        }
    }
    get_node(conn, &id).map(|n| n.unwrap())
}

// ─── Search ──────────────────────────────────────────────────────────────────

pub fn search_nodes(
    conn: &Connection,
    query: &str,
    node_type_filter: Option<&str>,
    limit: i64,
) -> Result<Vec<Node>> {
    let pattern = format!("%{}%", canonicalize(query));

    let nodes: Vec<Node> = if let Some(nt) = node_type_filter {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT n.* FROM nodes n
             LEFT JOIN node_alias a ON a.node_id = n.id
             WHERE n.node_type = ?3
               AND (n.canonical_name LIKE ?1 OR a.alias LIKE ?2)
             ORDER BY n.access_count DESC, n.updated_at DESC
             LIMIT ?4",
        )?;
        let collected: Vec<Node> = stmt
            .query_map(params![pattern, pattern, nt, limit], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        collected
    } else {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT n.* FROM nodes n
             LEFT JOIN node_alias a ON a.node_id = n.id
             WHERE (n.canonical_name LIKE ?1 OR a.alias LIKE ?2)
             ORDER BY n.access_count DESC, n.updated_at DESC
             LIMIT ?3",
        )?;
        let collected: Vec<Node> = stmt
            .query_map(params![pattern, pattern, limit], row_to_node_bare)?
            .collect::<std::result::Result<_, _>>()?;
        collected
    };
    nodes.into_iter().map(|n| with_aliases(conn, n)).collect()
}

// ─── Edges (read side — writes go through fact::assert_fact) ─────────────────

pub fn get_edges_from(conn: &Connection, node_id: &str) -> Result<Vec<Edge>> {
    let mut stmt = conn.prepare_cached("SELECT * FROM edges WHERE from_id = ?1")?;
    let edges = stmt
        .query_map(params![node_id], row_to_edge)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(edges)
}

pub fn get_edges_to(conn: &Connection, node_id: &str) -> Result<Vec<Edge>> {
    let mut stmt = conn.prepare_cached("SELECT * FROM edges WHERE to_id = ?1")?;
    let edges = stmt
        .query_map(params![node_id], row_to_edge)?
        .collect::<std::result::Result<_, _>>()?;
    Ok(edges)
}

// ─── Graph Traversal ─────────────────────────────────────────────────────────

/// Fan out from seed nodes through the (fact-backed) edges view up to
/// `max_depth` hops, bidirectionally. `max_results` hard-caps expansion —
/// unbounded traversal on a dense personal graph returns everything (§8.2).
pub fn get_neighborhood(
    conn: &Connection,
    seed_ids: &[&str],
    max_depth: i32,
    predicate_filter: Option<&[&str]>,
    max_results: Option<usize>,
) -> Result<Vec<NeighborhoodEntry>> {
    let limit = max_results.unwrap_or(50);

    if predicate_filter.is_some_and(|f| f.is_empty()) {
        return Ok(Vec::new());
    }

    let mut results: Vec<NeighborhoodEntry> = Vec::new();
    let mut visited: std::collections::HashSet<String> =
        seed_ids.iter().map(|s| s.to_string()).collect();
    let mut frontier: Vec<String> = seed_ids.iter().map(|s| s.to_string()).collect();

    let predicate_clause = match predicate_filter {
        Some(filter) => {
            let placeholders: Vec<String> =
                (0..filter.len()).map(|i| format!("?{}", i + 2)).collect();
            format!(" AND e.predicate IN ({})", placeholders.join(", "))
        }
        None => String::new(),
    };

    let outgoing_sql = format!(
        "SELECT e.id AS edge_id, e.from_id, e.predicate, e.to_id, e.weight, e.tags, n.*
         FROM edges e JOIN nodes n ON n.id = e.to_id WHERE e.from_id = ?1{predicate_clause}"
    );
    let incoming_sql = format!(
        "SELECT e.id AS edge_id, e.from_id, e.predicate, e.to_id, e.weight, e.tags, n.*
         FROM edges e JOIN nodes n ON n.id = e.from_id WHERE e.to_id = ?1{predicate_clause}"
    );

    let map_row = |row: &rusqlite::Row| -> std::result::Result<(Edge, Node), rusqlite::Error> {
        let edge = Edge {
            id: row.get("edge_id")?,
            from_id: row.get("from_id")?,
            predicate: row.get("predicate")?,
            to_id: row.get("to_id")?,
            weight: row.get("weight")?,
            tags: row.get("tags")?,
        };
        let node = row_to_node_bare(row)?;
        Ok((edge, node))
    };

    let mut outgoing_stmt = conn.prepare_cached(&outgoing_sql)?;
    let mut incoming_stmt = conn.prepare_cached(&incoming_sql)?;

    for depth in 1..=max_depth {
        let mut next_frontier: Vec<String> = Vec::new();

        for node_id in &frontier {
            if results.len() >= limit {
                break;
            }

            let mut bind: Vec<&dyn rusqlite::ToSql> =
                Vec::with_capacity(1 + predicate_filter.map_or(0, |f| f.len()));
            bind.push(node_id);
            if let Some(filter) = predicate_filter {
                for predicate in filter {
                    bind.push(predicate);
                }
            }

            for stmt in [&mut outgoing_stmt, &mut incoming_stmt] {
                if results.len() >= limit {
                    break;
                }
                let found: Vec<(Edge, Node)> = stmt
                    .query_map(&bind[..], map_row)?
                    .collect::<std::result::Result<_, _>>()?;
                for (edge, node) in found {
                    if results.len() >= limit {
                        break;
                    }
                    if !visited.contains(&node.id) {
                        visited.insert(node.id.clone());
                        next_frontier.push(node.id.clone());
                        results.push(NeighborhoodEntry {
                            node,
                            edge: Some(edge),
                            depth,
                        });
                    }
                }
            }
        }

        if results.len() >= limit {
            break;
        }
        frontier = next_frontier;
        if frontier.is_empty() {
            break;
        }
    }

    Ok(results)
}

// ─── Entity merge (§7 resolution; §9.3 merge review) ─────────────────────────

/// Merge `dup` into `keep`: every identifier, alias, mention, fact endpoint,
/// and detail row moves to `keep`; `dup`'s name becomes an alias of `keep`;
/// `dup` is deleted. A wrong merge silently fuses two people's facts and has
/// no clean undo (§7) — callers gate this behind exact-identity confidence
/// or human review.
pub fn merge_nodes(conn: &Connection, keep_id: &str, dup_id: &str) -> Result<()> {
    if keep_id == dup_id {
        return Err(crate::error::Error::Other(
            "cannot merge a node into itself".into(),
        ));
    }
    let keep = get_node(conn, keep_id)?
        .ok_or_else(|| crate::error::Error::Other(format!("no node {keep_id}")))?;
    let dup = get_node(conn, dup_id)?
        .ok_or_else(|| crate::error::Error::Other(format!("no node {dup_id}")))?;

    let tx_active = conn.is_autocommit();
    if tx_active {
        conn.execute_batch("BEGIN;")?;
    }
    let result = (|| -> Result<()> {
        // Identifiers & aliases: move, ignoring collisions with keep's own.
        conn.execute(
            "UPDATE OR IGNORE node_identifier SET node_id = ?1 WHERE node_id = ?2",
            params![keep_id, dup_id],
        )?;
        conn.execute(
            "UPDATE OR IGNORE node_alias SET node_id = ?1 WHERE node_id = ?2",
            params![keep_id, dup_id],
        )?;
        // Mentions: re-point; duplicates collapse via OR IGNORE.
        conn.execute(
            "UPDATE OR IGNORE mention SET node_id = ?1 WHERE node_id = ?2",
            params![keep_id, dup_id],
        )?;
        // Facts: re-point endpoints. OR IGNORE handles the live-unique index;
        // leftovers (exact-duplicate live facts) die with the node cascade.
        conn.execute(
            "UPDATE OR IGNORE fact SET subject_id = ?1 WHERE subject_id = ?2",
            params![keep_id, dup_id],
        )?;
        conn.execute(
            "UPDATE OR IGNORE fact SET object_id = ?1 WHERE object_id = ?2",
            params![keep_id, dup_id],
        )?;
        // Detail/context rows move only when keep has none.
        for table in [
            "task_detail",
            "project_detail",
            "goal_detail",
            "node_context",
        ] {
            conn.execute(
                &format!("UPDATE OR IGNORE {table} SET node_id = ?1 WHERE node_id = ?2"),
                params![keep_id, dup_id],
            )?;
        }
        conn.execute(
            "UPDATE OR IGNORE assign_rule SET node_id = ?1 WHERE node_id = ?2",
            params![keep_id, dup_id],
        )?;
        conn.execute(
            "UPDATE OR IGNORE external_ref SET node_id = ?1 WHERE node_id = ?2",
            params![keep_id, dup_id],
        )?;

        // Leftovers the OR IGNOREs skipped are exact duplicates of facts the
        // keep node already carries (the live-unique index blocked the move).
        // Fold their observation counts into keep's copy, then drop them —
        // fact.object_id and task_detail.parent_id have no ON DELETE CASCADE,
        // so anything still pointing at dup would block the node delete.
        conn.execute(
            // Polarity must match: merging a node that holds "X works_at Y"
            // with one that holds a live DENIAL of the same predicate must
            // not count the denial as corroboration of the claim. Same rule
            // assert_fact enforces on the write path ("a positive sighting
            // must never corroborate a negation") — merges are a write path
            // too, and became reachable once corrections started producing
            // real negations.
            "UPDATE fact SET observation_count = observation_count + 1
             WHERE valid_to IS NULL AND invalidated_at IS NULL
               AND (subject_id = ?1 OR object_id = ?1)
               AND EXISTS (
                 SELECT 1 FROM fact d
                 WHERE (d.subject_id = ?2 OR d.object_id = ?2)
                   AND d.predicate = fact.predicate
                   AND d.polarity = fact.polarity
                   AND d.valid_to IS NULL AND d.invalidated_at IS NULL)",
            params![keep_id, dup_id],
        )?;
        conn.execute(
            "DELETE FROM fact WHERE subject_id = ?1 OR object_id = ?1",
            params![dup_id],
        )?;
        conn.execute(
            "UPDATE task_detail SET parent_id = ?1 WHERE parent_id = ?2",
            params![keep_id, dup_id],
        )?;
        // Facts between keep and dup became self-loops when the endpoints
        // merged ("X and X frequently co-occur") — meaningless, drop them.
        conn.execute(
            "DELETE FROM fact WHERE subject_id = ?1 AND object_id = ?1",
            params![keep_id],
        )?;
        // scope_id columns are plain TEXT (no FK) but should follow the merge.
        for table in ["nodes", "episode", "task_detail"] {
            conn.execute(
                &format!("UPDATE {table} SET scope_id = ?1 WHERE scope_id = ?2"),
                params![keep_id, dup_id],
            )?;
        }

        // The duplicate's name lives on as an alias.
        add_alias(conn, keep_id, &dup.name, "merge")?;
        conn.execute(
            "UPDATE nodes SET access_count = access_count + ?2, updated_at = datetime('now')
             WHERE id = ?1",
            params![keep_id, dup.access_count],
        )?;
        // Cascades take mention/alias/identifier leftovers and detail rows.
        conn.execute("DELETE FROM nodes WHERE id = ?1", params![dup_id])?;
        let _ = keep;
        Ok(())
    })();

    if tx_active {
        match &result {
            Ok(()) => conn.execute_batch("COMMIT;")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK;");
            }
        }
    }
    result
}

/// Duplicate-person candidates: same canonical name (≥ 2 tokens — single
/// tokens like "june" are too collision-prone), different nodes. The §9.3
/// merge-review list.
/// Two person nodes whose names are the same name.
///
/// Compared with punctuation and spacing normalised away, because an
/// exact-equality join misses the commonest split there is: "Ada B Lovelace"
/// and "Ada B. Lovelace" are one person and one period. Such a pair can sit
/// as two separate nodes while `pkg dups` reports nothing, because the
/// email pass only matches email-named nodes against name observations and
/// this pass demanded byte equality — so a punctuation variant was
/// invisible to both.
///
/// Still multi-token only: two people really can be "June", and that is
/// what the disambiguation envelope is for. Nothing merges automatically.
pub fn duplicate_person_candidates(conn: &Connection) -> Result<Vec<(String, String, String)>> {
    let people = get_nodes_by_type(conn, "person", i64::MAX)?;
    let key = |s: &str| -> String {
        s.chars()
            .map(|c| if c.is_alphanumeric() { c } else { ' ' })
            .collect::<String>()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
            .to_lowercase()
    };

    let mut groups: std::collections::BTreeMap<String, Vec<&Node>> = Default::default();
    for p in &people {
        let k = key(&p.canonical_name);
        if k.contains(' ') {
            groups.entry(k).or_default().push(p);
        }
    }

    let mut out = vec![];
    for (k, members) in groups {
        for i in 0..members.len() {
            for j in (i + 1)..members.len() {
                let (a, b) = (members[i], members[j]);
                let (a, b) = if a.id < b.id { (a, b) } else { (b, a) };
                out.push((a.id.clone(), b.id.clone(), k.clone()));
            }
        }
    }
    Ok(out)
}

// ─── Health Scoring (lifted unchanged) ───────────────────────────────────────

/// Email-identity duplicate candidates (the second §9.3 pass): person nodes
/// *named by* a bare email address — calendar attendees who never carried a
/// display name — matched against named people whose name the email's local
/// part spells out. `iris.calder@example.com` ↔ "Iris Calder",
/// `veraholt@…` ↔ "Vera Holt".
///
/// The rule: squash the local part to letters only; it must start with the
/// person's first name and end with their last name (and be at least their
/// combined length). That tolerates dots, digits, middle initials on the
/// email side and middle names on the name side. The name side includes
/// ALIASES, so an email-named node that once saw a display name can pair
/// with another email-named node of the same human. Single-token names are
/// skipped entirely — same policy as `duplicate_person_candidates`, they're
/// too collision-prone. Candidates are for REVIEW (`pkg merge`, TUI merge
/// screen); nothing auto-merges.
///
/// Returns (named_node_id, email_node_id, "Name ↔ email") — named node
/// first, because `pkg dups` suggests the first element as the keep and the
/// named node is the natural keep.
pub fn email_duplicate_candidates(conn: &Connection) -> Result<Vec<(String, String, String)>> {
    let people = get_nodes_by_type(conn, "person", i64::MAX)?;

    let squash = |s: &str| -> String {
        s.chars()
            .filter(|c| c.is_alphabetic())
            .collect::<String>()
            .to_lowercase()
    };

    // The "named" side is any multi-token name OBSERVATION: a node's own
    // name, or any of its aliases — an email-named node that once saw a
    // display name ("Vera Holt" → alias) counts, which is what pairs two
    // email-named nodes of the same human.
    let mut named: Vec<(&Node, String, String, String)> = vec![]; // node, first, last, squashed
    let mut email_named: Vec<(&Node, String)> = vec![]; // node, squashed local part
    for n in &people {
        if let Some(local) = n.canonical_name.split_once('@').map(|(l, _)| l) {
            let sq = squash(local);
            if !sq.is_empty() {
                email_named.push((n, sq));
            }
        }
        let mut name_obs = vec![n.canonical_name.clone()];
        name_obs.extend(load_aliases(conn, &n.id)?);
        for name in name_obs {
            if name.contains('@') {
                continue;
            }
            let tokens: Vec<&str> = name.split_whitespace().collect();
            if tokens.len() >= 2 {
                let (first, last) = (tokens[0], tokens[tokens.len() - 1]);
                if first.len() >= 3 && last.len() >= 2 {
                    named.push((n, squash(first), squash(last), squash(&name)));
                }
            }
        }
    }

    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for (en, local) in &email_named {
        for (nn, first, last, full) in &named {
            if en.id == nn.id {
                continue;
            }
            let hit = local == full
                || (local.len() >= first.len() + last.len()
                    && local.starts_with(first.as_str())
                    && local.ends_with(last.as_str()));
            // Dedupe by unordered pair: the same two nodes can match through
            // several aliases, or in both directions.
            let key = if en.id < nn.id {
                (en.id.clone(), nn.id.clone())
            } else {
                (nn.id.clone(), en.id.clone())
            };
            if hit && seen.insert(key) {
                out.push((
                    nn.id.clone(),
                    en.id.clone(),
                    format!("{} ↔ {}", nn.name, en.name),
                ));
            }
        }
    }
    out.sort_by(|a, b| a.2.cmp(&b.2));
    Ok(out)
}

pub fn compute_health_score(confidence: f64, days_since_update: f64, access_count: i32) -> f64 {
    let recency = (-0.015 * days_since_update).exp();
    let access_boost = (0.3 + 0.1 * access_count as f64).min(1.0);
    confidence * recency * access_boost
}

pub fn compute_health(node: &Node) -> f64 {
    let now = chrono::Utc::now().timestamp() as f64;
    let updated_epoch =
        chrono::NaiveDateTime::parse_from_str(&node.updated_at, "%Y-%m-%d %H:%M:%S")
            .map(|dt| dt.and_utc().timestamp() as f64)
            .unwrap_or(now);
    let days_since_update = (now - updated_epoch) / 86400.0;
    compute_health_score(node.confidence, days_since_update, node.access_count)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::fact::assert_fact;

    #[test]
    fn test_upsert_get_node_roundtrip() {
        let conn = open_memory().unwrap();
        let mut node = Node::new("node-1", "person", "Alice Smith");
        node.aliases = vec!["Alice".into(), "Dr. Smith".into()];
        node.properties = serde_json::json!({ "org": "Acme" });
        node.confidence = 0.9;
        upsert_node(&conn, &node).unwrap();

        let fetched = get_node(&conn, "node-1").unwrap().expect("node exists");
        assert_eq!(fetched.name, "Alice Smith");
        assert_eq!(fetched.canonical_name, "alice smith");
        // Aliases are lowercased on storage.
        assert_eq!(fetched.aliases, vec!["alice", "dr. smith"]);
        assert_eq!(fetched.get_property("org"), Some("Acme"));
    }

    #[test]
    fn test_empty_name_resolves_to_nothing() {
        // Regression: an empty name used to reach the fuzzy tier as LIKE '%%'
        // and come back with arbitrary nodes — which let an empty-subject
        // candidate bind its fact to a random entity on accept.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("node-1", "person", "Alice Smith")).unwrap();
        assert!(resolve_entity_all(&conn, "").unwrap().is_empty());
        assert!(resolve_entity_all(&conn, "   ").unwrap().is_empty());
        assert!(resolve_entity(&conn, "").unwrap().is_none());
    }

    #[test]
    fn test_closed_node_type_set() {
        let conn = open_memory().unwrap();
        let node = Node::new("x", "banana", "Bad Type");
        assert!(upsert_node(&conn, &node).is_err());
    }

    #[test]
    fn test_identifier_resolution() {
        let conn = open_memory().unwrap();
        let node = Node::new("node-1", "person", "Bob Jones");
        upsert_node(&conn, &node).unwrap();
        upsert_identifier(&conn, "email", "bob@example.com", "node-1", "test").unwrap();

        let by_email = get_node_by_identifier(&conn, "email", "bob@example.com")
            .unwrap()
            .expect("found");
        assert_eq!(by_email.id, "node-1");
    }

    #[test]
    fn test_get_or_create_person_dedupes_by_email() {
        let conn = open_memory().unwrap();
        let a = get_or_create_person(&conn, Some("n@example.edu"), "Nadia", "calendar").unwrap();
        let b =
            get_or_create_person(&conn, Some("N@Example.EDU"), "Nadia Petrova", "email").unwrap();
        assert_eq!(a.id, b.id, "same email must resolve to the same node");
        // Both display names became aliases.
        assert!(b.aliases.contains(&"nadia".to_string()));
        assert!(b.aliases.contains(&"nadia petrova".to_string()));
    }

    #[test]
    fn test_merge_with_colliding_facts() {
        // Regression: both nodes carry a live fact to the same third node —
        // the move collides with idx_fact_live, and the leftover used to
        // block the dup delete via fact.object_id's non-cascading FK.
        let conn = open_memory().unwrap();
        let keep =
            get_or_create_person(&conn, Some("d@example.edu"), "Dana Fields", "cal").unwrap();
        let dup =
            get_or_create_person(&conn, Some("f0000xy@example.edu"), "Dana Fields", "cal").unwrap();
        assert_ne!(keep.id, dup.id);
        upsert_node(&conn, &Node::new("lab", "org", "The Lab")).unwrap();

        // Same triple on both; plus dup appears as an OBJECT of a fact.
        crate::fact::assert_fact(
            &conn,
            &keep.id,
            "member_of",
            Some("lab"),
            None,
            "Dana is in the lab",
            None,
            None,
            0.9,
            "npmi",
        )
        .unwrap();
        crate::fact::assert_fact(
            &conn,
            &dup.id,
            "member_of",
            Some("lab"),
            None,
            "Dana is in the lab",
            None,
            None,
            0.9,
            "npmi",
        )
        .unwrap();
        crate::fact::assert_fact(
            &conn,
            "lab",
            "related_to",
            Some(&dup.id),
            None,
            "lab related to Dana",
            None,
            None,
            0.7,
            "npmi",
        )
        .unwrap();

        merge_nodes(&conn, &keep.id, &dup.id).unwrap();

        assert!(get_node(&conn, &dup.id).unwrap().is_none());
        let facts = crate::fact::facts_for_node(&conn, &keep.id, 20).unwrap();
        let member = facts.iter().find(|f| f.predicate == "member_of").unwrap();
        assert_eq!(
            member.observation_count, 2,
            "collided fact corroborates keep's"
        );
        assert!(
            facts.iter().any(|f| f.predicate == "related_to"),
            "object-side fact moved"
        );
        // Both emails on the survivor.
        let merged = get_node(&conn, &keep.id).unwrap().unwrap();
        let by_dup_email = get_node_by_identifier(&conn, "email", "f0000xy@example.edu")
            .unwrap()
            .unwrap();
        assert_eq!(by_dup_email.id, merged.id);
    }

    #[test]
    fn test_email_named_people_get_their_names_back() {
        let conn = open_memory().unwrap();
        // The shape get_or_create_person produces when a source gives an
        // address and no display name.
        let n = get_or_create_person(&conn, Some("veraholt@example.com"), "", "t").unwrap();
        add_alias(&conn, &n.id, "vera holt", "manual").unwrap();
        assert_eq!(n.name, "veraholt@example.com");

        let (fixes, _) = promote_human_names(&conn, false).unwrap();
        assert_eq!(fixes.len(), 1);
        assert_eq!(
            fixes[0].to, "Vera Holt",
            "folded alias comes back title-cased"
        );

        // What renders is a name now.
        assert_eq!(get_node(&conn, &n.id).unwrap().unwrap().name, "Vera Holt");
        // And every way of finding her still works: by new name, by the
        // old address (identifier path AND alias path), by first name.
        for lookup in ["Vera Holt", "veraholt@example.com", "vera"] {
            assert_eq!(
                resolve_entity(&conn, lookup).unwrap().map(|x| x.id),
                Some(n.id.clone()),
                "lookup by {lookup} must still resolve"
            );
        }
    }

    /// The other absorption shape: `resolve_entity_all`'s fuzzy tier made an
    /// *event* the subject of every fact about a person, because her name is
    /// a substring of its title. The facts are true and only their endpoint
    /// is wrong, and the event is a real event — so a merge is the wrong
    /// tool twice over.
    #[test]
    fn facts_move_off_a_contaminated_node_without_destroying_it() {
        let conn = open_memory().unwrap();
        upsert_node(
            &conn,
            &Node::new("event-spsp", "event", "SPSP Wrench Reunion"),
        )
        .unwrap();
        let wren = create_person(&conn, "Wren Calder", "t").unwrap();
        let avery = create_person(&conn, "Avery J Calder", "t").unwrap();
        // Facts that landed on the event: one with it as subject, one as
        // object, and one linking it to the person it is really about.
        // Seeded predicates only: `fact.predicate` is a foreign key into the
        // predicate vocabulary, and the everyday `is`/`has` the extractor
        // mints on demand are not in a fresh database.
        for (subj, obj, pred, stmt) in [
            ("event-spsp", None, "about", "Wren is a twin daughter."),
            (
                "event-spsp",
                Some(avery.id.as_str()),
                "related_to",
                "Wren is Avery's daughter.",
            ),
            (
                avery.id.as_str(),
                Some("event-spsp"),
                "member_of",
                "Avery's daughters include Wren.",
            ),
            (
                "event-spsp",
                Some(wren.id.as_str()),
                "attended",
                "co-occurrence",
            ),
        ] {
            conn.execute(
                "INSERT INTO fact (uid, subject_id, predicate, object_id, statement, polarity,
                                   confidence, observation_count, valid_from)
                 VALUES (hex(randomblob(8)), ?1, ?2, ?3, ?4, 'positive', 1.0, 1, datetime('now'))",
                params![subj, pred, obj, stmt],
            )
            .unwrap();
        }

        let moved = move_facts(&conn, "event-spsp", &wren.id).unwrap();
        assert_eq!(moved.subjects, 2, "{moved:?}");
        assert_eq!(moved.objects, 1, "{moved:?}");
        // The event↔Wren link would have become Wren → Wren; cut, not kept.
        assert_eq!(moved.self_loops, 1, "{moved:?}");
        assert_eq!(moved.blocked, 0, "{moved:?}");

        // The event survives — it is a real event.
        assert!(get_node(&conn, "event-spsp").unwrap().is_some());
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact WHERE subject_id='event-spsp' OR object_id='event-spsp'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "nothing still hangs off the event");
        // A pre-existing self-loop on the destination is none of this
        // function's business, and cleaning up after the re-point would have
        // eaten it.
        let on_edie: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact WHERE subject_id=?1 OR object_id=?1",
                params![wren.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(on_edie, 3);
    }

    /// The narrowing is the whole point: a contaminated node is usually not
    /// entirely contaminated, and `extractor` separates the two by how the
    /// graph came to believe each one rather than by a reader's judgement.
    #[test]
    fn mentions_move_by_extractor_leaving_the_nodes_own_evidence() {
        let conn = open_memory().unwrap();
        upsert_node(
            &conn,
            &Node::new("event-spsp", "event", "SPSP Wrench Reunion"),
        )
        .unwrap();
        let wren = create_person(&conn, "Wren Calder", "t").unwrap();
        let mk = |sid: &str, src: &str| {
            let ep = crate::episode::Episode {
                id: 0,
                uid: String::new(),
                source: src.into(),
                source_id: sid.into(),
                source_ref: None,
                body: format!("body {sid}"),
                occurred_at: "2026-08-01 12:00:00".into(),
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
            crate::episode::upsert_episode(&conn, &ep).unwrap().0
        };
        let a = mk("a", "bee.conversation");
        let b = mk("b", "bee.daily");
        let c = mk("c", "calendar.event");
        for (ep, ex) in [(a, "llm"), (b, "llm"), (c, "attendee")] {
            conn.execute(
                "INSERT INTO mention (episode_id, node_id, extractor, confidence)
                 VALUES (?1, 'event-spsp', ?2, 1.0)",
                params![ep, ex],
            )
            .unwrap();
        }

        let (moved, dropped) =
            move_mentions(&conn, "event-spsp", &wren.id, Some("llm"), None).unwrap();
        assert_eq!((moved, dropped), (2, 0));
        let kept: (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), COALESCE(MIN(extractor),'') FROM mention WHERE node_id='event-spsp'",
                [],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(kept, (1, "attendee".into()), "the event keeps its own row");
    }

    /// A mention the destination already has is redundant by construction,
    /// so it is dropped rather than stranded — otherwise the contaminated
    /// node keeps showing mentions the repair was meant to take away.
    #[test]
    fn a_redundant_mention_is_dropped_rather_than_left_behind() {
        let conn = open_memory().unwrap();
        upsert_node(
            &conn,
            &Node::new("event-spsp", "event", "SPSP Wrench Reunion"),
        )
        .unwrap();
        let wren = create_person(&conn, "Wren Calder", "t").unwrap();
        let ep = crate::episode::Episode {
            id: 0,
            uid: String::new(),
            source: "bee.daily".into(),
            source_id: "shared".into(),
            source_ref: None,
            body: "both mention it".into(),
            occurred_at: "2026-08-01 12:00:00".into(),
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
        let (ep_id, _) = crate::episode::upsert_episode(&conn, &ep).unwrap();
        for node in ["event-spsp", wren.id.as_str()] {
            conn.execute(
                "INSERT INTO mention (episode_id, node_id, extractor, confidence)
                 VALUES (?1, ?2, 'llm', 1.0)",
                params![ep_id, node],
            )
            .unwrap();
        }
        let (moved, dropped) =
            move_mentions(&conn, "event-spsp", &wren.id, Some("llm"), None).unwrap();
        assert_eq!((moved, dropped), (0, 1));
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE node_id='event-spsp'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0);
    }

    #[test]
    fn bulk_moves_refuse_a_node_onto_itself_or_a_missing_target() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "Wren Calder", "t").unwrap();
        assert!(move_facts(&conn, &n.id, &n.id).is_err());
        assert!(move_facts(&conn, &n.id, "person-nope").is_err());
        assert!(move_facts(&conn, "person-nope", &n.id).is_err());
        assert!(move_mentions(&conn, &n.id, &n.id, None, None).is_err());
        assert!(move_mentions(&conn, &n.id, "person-nope", None, None).is_err());
    }

    /// The repair this family exists for, end to end: a first-name alias
    /// pulled one person's whole history onto another person's node, and
    /// undoing it means moving the *smaller* side out — the identifier that
    /// decides where future ingest lands, the names that belong to them, and
    /// the handful of episodes that were really theirs.
    #[test]
    fn a_conflated_node_can_be_split_apart() {
        let conn = open_memory().unwrap();
        // The node as it really was: minted from a student's email, then
        // aliased by first name, then a decade of somebody else's life.
        let stuck = get_or_create_person(
            &conn,
            Some("marisol.b.farrow.27@ostrander.edu"),
            "Marisol B. Farrow",
            "llm",
        )
        .unwrap();
        add_alias(&conn, &stuck.id, "marisol", "firstname").unwrap();
        assert_eq!(
            resolve_entity_all(&conn, "Marisol").unwrap()[0].id,
            stuck.id,
            "precondition: the bare first name lands on the student"
        );

        // Invert: the node is overwhelmingly the daughter's, so she keeps it.
        rename_node(&conn, &stuck.id, "Marisol Quinn Calder").unwrap();
        // …but the old name must NOT stay an alias here — it is someone
        // else's name, and keeping it is the conflation.
        assert!(remove_alias(&conn, &stuck.id, "Marisol B. Farrow").unwrap());
        assert!(remove_alias(&conn, &stuck.id, "marisol.b.farrow.27").unwrap());

        let student = create_person(&conn, "Marisol B. Farrow", "manual").unwrap();
        move_identifier(
            &conn,
            "email",
            "marisol.b.farrow.27@ostrander.edu",
            &student.id,
        )
        .unwrap();

        // Each name now reaches exactly one person.
        let calder = resolve_entity_all(&conn, "Marisol Quinn Calder").unwrap();
        assert_eq!(calder.len(), 1);
        assert_eq!(calder[0].id, stuck.id);
        let farrow = resolve_entity_all(&conn, "Marisol B. Farrow").unwrap();
        assert_eq!(farrow.len(), 1);
        assert_eq!(farrow[0].id, student.id);
        // And the identifier decides where the next email lands — the whole
        // point of moving it, and what stops the split re-merging tomorrow.
        let again = get_or_create_person(
            &conn,
            Some("marisol.b.farrow.27@ostrander.edu"),
            "Marisol B. Farrow",
            "llm",
        )
        .unwrap();
        assert_eq!(
            again.id, student.id,
            "future ingest must land on the student"
        );
    }

    /// A mention moves with its extractor and confidence intact — the
    /// provenance is the evidence, and a move that flattened it to "manual"
    /// would launder how the graph came to believe something.
    #[test]
    fn moving_a_mention_carries_its_provenance() {
        let conn = open_memory().unwrap();
        let from = create_person(&conn, "Marisol Quinn Calder", "t").unwrap();
        let to = create_person(&conn, "Marisol B. Farrow", "t").unwrap();
        let ep = crate::episode::Episode {
            id: 0,
            uid: String::new(),
            source: "reflect.note".into(),
            source_id: "advising-2023".into(),
            source_ref: None,
            body: "First Year Advising 2023".into(),
            occurred_at: "2026-08-03 12:00:00".into(),
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
        let (ep_id, _) = crate::episode::upsert_episode(&conn, &ep).unwrap();
        let uid: String = conn
            .query_row(
                "SELECT uid FROM episode WHERE id = ?1",
                params![ep_id],
                |r| r.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO mention (episode_id, node_id, extractor, confidence)
             VALUES (?1, ?2, 'alias', 0.75)",
            params![ep_id, from.id],
        )
        .unwrap();

        move_mention(&conn, &uid, &from.id, &to.id).unwrap();

        let moved: (String, f64) = conn
            .query_row(
                "SELECT extractor, confidence FROM mention WHERE episode_id=?1 AND node_id=?2",
                params![ep_id, to.id],
                |r| Ok((r.get(0).unwrap(), r.get(1).unwrap())),
            )
            .unwrap();
        assert_eq!(moved.0, "alias");
        assert!((moved.1 - 0.75).abs() < f64::EPSILON);
        let left: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM mention WHERE episode_id=?1 AND node_id=?2",
                params![ep_id, from.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(left, 0, "the mention moved rather than being copied");
    }

    /// Every one of these refuses rather than half-doing the job — a split
    /// left halfway is worse than one not started.
    #[test]
    fn the_split_verbs_refuse_what_they_cannot_do() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "Vera Holt", "t").unwrap();
        // An alias that was not there is `false`, not an error: the caller
        // asked for it to be gone and it is gone.
        assert!(!remove_alias(&conn, &n.id, "never-was").unwrap());
        assert!(move_identifier(&conn, "email", "nobody@x.com", &n.id).is_err());
        assert!(move_identifier(&conn, "email", "a@x.com", "person-nope").is_err());
        assert!(move_mention(&conn, "no-such-uid", &n.id, &n.id).is_err());
    }

    /// The property that makes a rename safe to offer as one keypress: the
    /// old name keeps resolving, because it becomes an alias. Everything
    /// that reached the node before still reaches it.
    #[test]
    fn a_rename_leaves_the_old_name_resolving() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "Marisol B. Farrow", "t").unwrap();

        let fix = rename_node(&conn, &n.id, "Marisol Calder").unwrap();
        assert_eq!(fix.from, "Marisol B. Farrow");
        assert_eq!(fix.to, "Marisol Calder");

        let after = get_node(&conn, &n.id).unwrap().unwrap();
        assert_eq!(after.name, "Marisol Calder");
        assert_eq!(after.canonical_name, "marisol calder");

        // Both names still land on the same node — path 1 changed hands,
        // path 2 picked up what it dropped.
        for name in ["Marisol Calder", "Marisol B. Farrow"] {
            let hits = resolve_entity_all(&conn, name).unwrap();
            assert_eq!(hits.len(), 1, "{name} resolved to {} nodes", hits.len());
            assert_eq!(hits[0].id, n.id, "{name} resolved to the wrong node");
        }
    }

    /// Renaming onto a name another node already owns is a merge question,
    /// and the refusal names the command that answers it.
    #[test]
    fn a_rename_into_a_collision_is_refused_and_names_the_merge() {
        let conn = open_memory().unwrap();
        let keep = create_person(&conn, "Vera Holt", "t").unwrap();
        let dup = create_person(&conn, "V. Holt", "t").unwrap();

        let err = rename_node(&conn, &dup.id, "Vera Holt")
            .unwrap_err()
            .to_string();
        assert!(err.contains("merge question"), "{err}");
        assert!(
            err.contains(&keep.id),
            "the refusal must name the other node: {err}"
        );
        assert!(err.contains("merge"), "{err}");
        // Refused means unchanged.
        assert_eq!(get_node(&conn, &dup.id).unwrap().unwrap().name, "V. Holt");
    }

    /// A node may be renamed to a name it is *already aliased by* — which is
    /// the common repair, and which a collision rule keyed on aliases rather
    /// than canonical names would have refused.
    #[test]
    fn a_rename_to_the_nodes_own_alias_is_allowed() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "Marisol B. Farrow", "t").unwrap();
        add_alias(&conn, &n.id, "Marisol Calder", "manual").unwrap();

        rename_node(&conn, &n.id, "Marisol Calder").unwrap();
        assert_eq!(
            get_node(&conn, &n.id).unwrap().unwrap().name,
            "Marisol Calder"
        );
    }

    /// The trap this whole verb had to be written around. `resolve_entity_all`
    /// falls back to `LIKE '%name%'`, so "Wren" resolves to the *event* "SPSP
    /// Wrench Reunion" — a substring hit. Refusing to create on a fuzzy match
    /// would make the missing-person case the one case create cannot serve,
    /// which is the only case it exists for.
    #[test]
    fn creating_a_person_ignores_a_fuzzy_substring_match() {
        let conn = open_memory().unwrap();
        upsert_node(
            &conn,
            &Node::new("event-reunion", "event", "SPSP Wrench Reunion"),
        )
        .unwrap();
        // The precondition: the name really does resolve, to the wrong thing.
        let before = resolve_entity_all(&conn, "Wren").unwrap();
        assert_eq!(before.len(), 1);
        assert_eq!(before[0].node_type, "event");

        let wren = create_person(&conn, "Wren", "t").unwrap();
        assert_eq!(wren.node_type, "person");

        // And now the person outranks the event, which is `resolve_entity_all`'s
        // own anchor-type rule doing the work.
        let after = resolve_entity_all(&conn, "Wren").unwrap();
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!(after[0].id, wren.id);
    }

    /// The type is not cosmetic — `resolve_entity_all` ranks anchor types
    /// above retrieval targets — and changing it must move nothing, because
    /// a partial move is how a repair becomes a second problem.
    #[test]
    fn retyping_keeps_the_id_and_everything_hanging_off_it() {
        let conn = open_memory().unwrap();
        upsert_node(
            &conn,
            &Node::new("topic-obic", "topic", "Ostrander Brain Imaging Center"),
        )
        .unwrap();
        add_alias(&conn, "topic-obic", "OBIC", "manual").unwrap();

        let (was, now) = retype_node(&conn, "topic-obic", "org").unwrap();
        assert_eq!((was.as_str(), now.as_str()), ("topic", "org"));

        let after = get_node(&conn, "topic-obic").unwrap().unwrap();
        assert_eq!(after.node_type, "org");
        assert_eq!(
            after.id, "topic-obic",
            "the id is an opaque key and does not move"
        );
        assert_eq!(load_aliases(&conn, "topic-obic").unwrap(), vec!["obic"]);
        assert_eq!(
            resolve_entity_all(&conn, "OBIC").unwrap()[0].id,
            "topic-obic"
        );
    }

    #[test]
    fn retyping_refuses_an_unknown_type_a_no_op_and_a_missing_node() {
        let conn = open_memory().unwrap();
        let n = create_node(&conn, "topic", "A Thing", "t").unwrap();
        assert!(retype_node(&conn, &n.id, "institution").is_err());
        assert!(
            retype_node(&conn, &n.id, "topic").is_err(),
            "already that type"
        );
        assert!(retype_node(&conn, "nope-1", "org").is_err());
    }

    /// An org is not a person, and the difference that matters is the
    /// first-name alias: minting one for "Psychological & Brain Sciences
    /// Department" would give it "Psychological" as a one-word magnet, which
    /// is the mechanism behind every conflation repaired today.
    #[test]
    fn only_people_get_a_first_name_alias() {
        let conn = open_memory().unwrap();
        let person = create_node(&conn, "person", "Emma Calloway", "t").unwrap();
        assert_eq!(load_aliases(&conn, &person.id).unwrap(), vec!["emma"]);

        let org = create_node(
            &conn,
            "org",
            "Psychological & Brain Sciences Department",
            "t",
        )
        .unwrap();
        assert!(
            load_aliases(&conn, &org.id).unwrap().is_empty(),
            "an org must not be given a one-word magnet"
        );
        assert_eq!(org.node_type, "org");
        assert!(
            org.id.starts_with("org-"),
            "id carries the type: {}",
            org.id
        );
    }

    /// The closed set is enforced here too, not only on `upsert_node` — a
    /// caller reaching this with a typo should be told, not have a node of
    /// an unknown type refused three layers down.
    #[test]
    fn creating_a_node_of_an_unknown_type_is_refused() {
        let conn = open_memory().unwrap();
        let err = create_node(&conn, "institution", "Ostrander", "t")
            .unwrap_err()
            .to_string();
        assert!(err.contains("closed set"), "{err}");
        assert!(create_node(&conn, "org", "  ", "t").is_err());
    }

    /// Create is stricter than rename: an exact name, or an exact alias, is
    /// something that already resolves, and a second node under it splits
    /// the name across two rather than repairing anything.
    #[test]
    fn creating_a_person_refuses_a_name_that_already_resolves_exactly() {
        let conn = open_memory().unwrap();
        let existing = create_person(&conn, "Vera Holt", "t").unwrap();
        add_alias(&conn, &existing.id, "Vee", "manual").unwrap();

        let by_name = create_person(&conn, "vera holt", "t")
            .unwrap_err()
            .to_string();
        assert!(by_name.contains(&existing.id), "{by_name}");
        assert!(by_name.contains("Rename"), "{by_name}");

        let by_alias = create_person(&conn, "Vee", "t").unwrap_err().to_string();
        assert!(by_alias.contains("alias"), "{by_alias}");
        assert!(by_alias.contains(&existing.id), "{by_alias}");
    }

    /// A name is not optional, and neither verb may blank one.
    #[test]
    fn neither_verb_accepts_an_empty_name() {
        let conn = open_memory().unwrap();
        let n = create_person(&conn, "Vera Holt", "t").unwrap();
        assert!(create_person(&conn, "   ", "t").is_err());
        assert!(rename_node(&conn, &n.id, "  ").is_err());
        assert!(rename_node(&conn, "person-nope", "Anything").is_err());
        assert_eq!(get_node(&conn, &n.id).unwrap().unwrap().name, "Vera Holt");
    }

    #[test]
    fn test_rename_refuses_to_manufacture_ambiguity() {
        let conn = open_memory().unwrap();
        // Another node already owns the human name.
        upsert_node(&conn, &Node::new("person-real", "person", "Vera Holt")).unwrap();
        let n = get_or_create_person(&conn, Some("veraholt@example.com"), "", "t").unwrap();
        add_alias(&conn, &n.id, "vera holt", "manual").unwrap();

        let (fixes, skipped) = promote_human_names(&conn, false).unwrap();
        assert!(
            fixes.is_empty(),
            "renaming into a collision is a merge question"
        );
        assert!(skipped[0].contains("merge question"));
        assert_eq!(
            get_node(&conn, &n.id).unwrap().unwrap().name,
            "veraholt@example.com"
        );

        // A node with nothing human to promote is left alone, not blanked.
        let bare = get_or_create_person(&conn, Some("noname@x.com"), "", "t").unwrap();
        let (fixes, skipped) = promote_human_names(&conn, false).unwrap();
        assert!(fixes.is_empty());
        assert!(skipped
            .iter()
            .any(|s| s.contains("no multi-word human alias")));
        assert_eq!(
            get_node(&conn, &bare.id).unwrap().unwrap().name,
            "noname@x.com"
        );
    }

    #[test]
    fn test_title_case_keeps_particles_and_initials() {
        assert_eq!(title_case_name("vera holt"), "Vera Holt");
        assert_eq!(title_case_name("frans van der berg"), "Frans van der Berg");
        assert_eq!(title_case_name("victor r hale"), "Victor R Hale");
    }

    #[test]
    fn a_period_does_not_hide_a_duplicate() {
        // The motivating bug: "Ada B Lovelace" and "Ada B. Lovelace" can
        // sit as separate nodes while `pkg dups` reports nothing, because
        // this pass demanded byte equality and the email pass only looks
        // at email-named nodes.
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("p1", "person", "Ada B Lovelace")).unwrap();
        upsert_node(&conn, &Node::new("p2", "person", "Ada B. Lovelace")).unwrap();
        // Exact duplicates still pair, as before.
        upsert_node(&conn, &Node::new("p3", "person", "June Choi")).unwrap();
        upsert_node(&conn, &Node::new("p4", "person", "June Choi")).unwrap();
        // A single-token name never pairs: two people really can be "June",
        // and that is what the disambiguation envelope is for.
        upsert_node(&conn, &Node::new("p5", "person", "June")).unwrap();
        upsert_node(&conn, &Node::new("p6", "person", "June")).unwrap();
        // Genuinely different people do not pair.
        upsert_node(&conn, &Node::new("p7", "person", "Avery Skywalker")).unwrap();

        let dups = duplicate_person_candidates(&conn).unwrap();
        let pairs: Vec<(&str, &str)> = dups
            .iter()
            .map(|(a, b, _)| (a.as_str(), b.as_str()))
            .collect();
        assert!(
            pairs.contains(&("p1", "p2")),
            "a period is not a different person"
        );
        assert!(pairs.contains(&("p3", "p4")), "exact duplicates still pair");
        assert!(
            !pairs.iter().any(|(a, _)| *a == "p5"),
            "single-token names never pair"
        );
        assert!(!pairs.iter().any(|(a, b)| *a == "p7" || *b == "p7"));
        assert_eq!(dups.len(), 2);
    }

    #[test]
    fn test_merge_never_lets_a_denial_corroborate_a_claim() {
        // A merge is a write path, so it owes the same polarity guarantee
        // assert_fact gives: folding observation counts on predicate alone
        // would let "X does NOT work at Y" strengthen "X works at Y".
        let conn = open_memory().unwrap();
        let keep = get_or_create_person(&conn, Some("a@x.com"), "Ada Lovelace", "t").unwrap();
        let dup = get_or_create_person(&conn, Some("b@x.com"), "Ada Lovelace", "t").unwrap();
        upsert_node(&conn, &Node::new("org", "org", "X Corp")).unwrap();

        crate::fact::assert_fact(
            &conn,
            &keep.id,
            "works_at",
            Some("org"),
            None,
            "Ada works at X Corp",
            None,
            None,
            0.9,
            "llm",
        )
        .unwrap();
        crate::fact::assert_negative_fact(
            &conn,
            &dup.id,
            "works_at",
            Some("org"),
            None,
            "Ada does NOT work at X Corp",
            None,
            0.95,
            "user",
        )
        .unwrap();

        merge_nodes(&conn, &keep.id, &dup.id).unwrap();

        let facts = crate::fact::facts_for_node(&conn, &keep.id, 20).unwrap();
        let positive = facts
            .iter()
            .find(|f| f.predicate == "works_at" && f.polarity == "positive")
            .expect("the claim survives the merge");
        assert_eq!(
            positive.observation_count, 1,
            "a denial must never corroborate the claim it denies"
        );
    }

    #[test]
    fn test_suggest_entities_ranked_typeahead() {
        let conn = open_memory().unwrap();
        let p = get_or_create_person(&conn, Some("iris.calder@example.com"), "Iris Calder", "cal")
            .unwrap();
        get_or_create_person(&conn, Some("i.andrews@x.com"), "Iris Andrews", "cal").unwrap();
        upsert_node(&conn, &Node::new("iris-topic", "topic", "iris-methods")).unwrap();
        // Event literally named "Iris" must never appear in suggestions.
        upsert_node(&conn, &Node::new("ev1", "event", "Iris")).unwrap();
        // Accessed nodes rank first within a tier.
        increment_node_access(&conn, &p.id).unwrap();

        let sug = suggest_entities(&conn, "iri", 5).unwrap();
        assert!(!sug.is_empty());
        assert!(
            sug.iter().all(|s| s.node.node_type != "event"),
            "events excluded"
        );
        assert_eq!(
            sug[0].node.name, "Iris Calder",
            "person + accessed ranks first"
        );

        // Identifier prefix matches too (email typeahead).
        let sug = suggest_entities(&conn, "iris.calder@", 5).unwrap();
        assert_eq!(sug[0].node.id, p.id);
        assert_eq!(sug[0].via, "identifier");

        // Sub-2-char input suggests nothing (noise guard).
        assert!(suggest_entities(&conn, "i", 5).unwrap().is_empty());
    }

    #[test]
    fn test_merge_drops_self_loop_facts() {
        // Regression: an NPMI "A and B frequently co-occur" fact between the
        // two nodes being merged becomes a self-loop after endpoint rewrite
        // ("Iris related_to Iris") — it must not survive the merge.
        let conn = open_memory().unwrap();
        let keep =
            get_or_create_person(&conn, Some("i@example.com"), "Iris Calder", "cal").unwrap();
        let dup = get_or_create_person(&conn, Some("i@example.edu"), "Iris", "cal").unwrap();
        crate::fact::assert_fact(
            &conn,
            &keep.id,
            "related_to",
            Some(&dup.id),
            None,
            "Iris Calder and Iris frequently co-occur",
            None,
            None,
            0.8,
            "npmi",
        )
        .unwrap();

        merge_nodes(&conn, &keep.id, &dup.id).unwrap();

        let self_loops: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM fact WHERE subject_id = ?1 AND object_id = ?1",
                rusqlite::params![keep.id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            self_loops, 0,
            "merge must drop facts that became self-loops"
        );
    }

    /// The drop-rule alone is not enough: it can only remove what the fuzzy
    /// tier's LIMIT let through. With enough events matching a substring,
    /// all five slots filled before any person was reached — so a surname
    /// search returned five events and no people, and `/entity` could not
    /// find two nodes to offer for a merge.
    #[test]
    fn a_surname_finds_people_even_when_events_swamp_the_substring() {
        let conn = open_memory().unwrap();
        // Far more events matching "choi" than the fuzzy tier will return.
        for i in 0..20 {
            upsert_node(
                &conn,
                &Node::new(
                    &format!("event-{i}"),
                    "event",
                    &format!("booked: YB whitlock {i}"),
                ),
            )
            .unwrap();
        }
        let a = create_person(&conn, "Dara Whitlock", "t").unwrap();
        let b = create_person(&conn, "Juno Whitlock", "t").unwrap();

        let hits = resolve_entity_all(&conn, "Whitlock").unwrap();
        let ids: Vec<&str> = hits.iter().map(|n| n.id.as_str()).collect();
        assert!(
            ids.contains(&a.id.as_str()),
            "Dara Whitlock missing: {ids:?}"
        );
        assert!(
            ids.contains(&b.id.as_str()),
            "Juno Whitlock missing: {ids:?}"
        );
        assert!(
            hits.iter().all(|n| n.node_type == "person"),
            "events shadowed the people: {:?}",
            hits.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_events_never_shadow_people_in_resolution() {
        let conn = open_memory().unwrap();
        let person = get_or_create_person(&conn, Some("n@x.edu"), "Nadia", "cal").unwrap();
        // Calendar events literally titled with the person's name.
        for i in 0..3 {
            let mut ev = Node::new(&format!("event-{i}"), "event", "Nadia");
            ev.source = "calendar".into();
            upsert_node(&conn, &ev).unwrap();
        }

        let matches = resolve_entity_all(&conn, "Nadia").unwrap();
        assert_eq!(matches.len(), 1, "events suppressed when a person matches");
        assert_eq!(matches[0].id, person.id);

        // But an event still resolves when it's the only thing matching.
        let mut ev = Node::new("event-lab", "event", "Lab Meeting");
        ev.source = "calendar".into();
        upsert_node(&conn, &ev).unwrap();
        let matches = resolve_entity_all(&conn, "Lab Meeting").unwrap();
        assert_eq!(matches.len(), 1);
        assert_eq!(matches[0].node_type, "event");
    }

    #[test]
    fn test_resolve_entity_ambiguity() {
        let conn = open_memory().unwrap();
        let g1 = get_or_create_person(&conn, Some("june.chen@x.com"), "June Chen", "t").unwrap();
        let g2 = get_or_create_person(&conn, Some("june.r@y.com"), "June Rodriguez", "t").unwrap();
        add_alias(&conn, &g1.id, "June", "manual").unwrap();
        add_alias(&conn, &g2.id, "June", "manual").unwrap();

        let matches = resolve_entity_all(&conn, "June").unwrap();
        assert_eq!(matches.len(), 2, "ambiguity must be surfaced, not guessed");
    }

    #[test]
    fn test_neighborhood_over_fact_view() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("jane", "person", "Jane")).unwrap();
        upsert_node(&conn, &Node::new("atlas", "project", "ATLAS")).unwrap();
        upsert_node(&conn, &Node::new("acme", "org", "Acme")).unwrap();

        assert_fact(
            &conn,
            "jane",
            "works_on",
            Some("atlas"),
            None,
            "Jane works on ATLAS",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();
        assert_fact(
            &conn,
            "jane",
            "works_at",
            Some("acme"),
            None,
            "Jane works at Acme",
            None,
            None,
            0.9,
            "manual",
        )
        .unwrap();

        let one_hop = get_neighborhood(&conn, &["jane"], 1, None, None).unwrap();
        assert_eq!(one_hop.len(), 2);

        let filtered = get_neighborhood(&conn, &["jane"], 1, Some(&["works_on"]), None).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].node.id, "atlas");

        // Empty filter matches nothing.
        let empty = get_neighborhood(&conn, &["jane"], 1, Some(&[]), None).unwrap();
        assert!(empty.is_empty());
    }

    #[test]
    fn test_email_duplicate_candidates() {
        let conn = crate::db::open_memory().unwrap();
        let mk = |id: &str, name: &str| {
            upsert_node(&conn, &Node::new(id, "person", name)).unwrap();
        };
        mk("p-iris", "Iris Calder");
        mk("p-vera", "Vera Holt");
        mk("p-ada", "Ada B Lovelace");
        mk("p-june", "June"); // single token: never matched
        mk("e-dotted", "iris.calder@example.com"); // dotted exact
        mk("e-concat", "veraholt@example.com"); // concatenated local part
        mk("e-initial", "ada.lovelace@example.edu"); // middle initial (name side)
        mk("e-opaque", "willow@example.com"); // no resemblance to anyone
        mk("e-june", "june@x.com"); // matches only a single-token name

        // Two email-named nodes of the same human: one carries a display-name
        // alias, which is the only name observation linking them.
        mk("e-sam-gmail", "sam.smith@example.com");
        mk("e-sam-work", "samsmith@corp.com");
        add_alias(&conn, "e-sam-gmail", "Sam Smith", "attendee").unwrap();

        let dups = email_duplicate_candidates(&conn).unwrap();
        let pairs: Vec<(&str, &str)> = dups
            .iter()
            .map(|(a, b, _)| (a.as_str(), b.as_str()))
            .collect();
        assert!(
            pairs.contains(&("p-iris", "e-dotted")),
            "dotted local part: {dups:?}"
        );
        assert!(
            pairs.contains(&("p-vera", "e-concat")),
            "concatenated local part"
        );
        assert!(
            pairs.contains(&("p-ada", "e-initial")),
            "middle initial tolerated"
        );
        assert!(
            pairs.contains(&("e-sam-gmail", "e-sam-work")),
            "alias observation must pair email-named nodes: {dups:?}"
        );
        assert_eq!(
            pairs.len(),
            4,
            "opaque and single-token-name emails must not match: {dups:?}"
        );

        // Merging consumes the candidate.
        merge_nodes(&conn, "p-iris", "e-dotted").unwrap();
        let after = email_duplicate_candidates(&conn).unwrap();
        assert!(!after.iter().any(|(_, b, _)| b == "e-dotted"));
    }
}
