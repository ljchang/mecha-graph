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
        let mut stmt =
            conn.prepare_cached("SELECT * FROM nodes WHERE canonical_name LIKE ?1 LIMIT 5")?;
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

        let collides: bool = conn.query_row(
            "SELECT COUNT(*) > 0 FROM nodes WHERE canonical_name = ?1 AND id != ?2",
            params![new_canon, node_id],
            |r| r.get(0),
        )?;
        if collides {
            skipped.push(format!(
                "{old} → {new_name}: that name is already another node — a merge question, not a rename"
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
        let b = get_or_create_person(&conn, Some("N@Example.EDU"), "Nadia Petrova", "email")
            .unwrap();
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
        let dup = get_or_create_person(
            &conn,
            Some("f0000xy@example.edu"),
            "Dana Fields",
            "cal",
        )
        .unwrap();
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
        assert_eq!(
            get_node(&conn, &n.id).unwrap().unwrap().name,
            "Vera Holt"
        );
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
        upsert_node(&conn, &Node::new("p7", "person", "Luke Skywalker")).unwrap();

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
        let keep = get_or_create_person(&conn, Some("i@example.com"), "Iris Calder", "cal").unwrap();
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
        let g2 =
            get_or_create_person(&conn, Some("june.r@y.com"), "June Rodriguez", "t").unwrap();
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
