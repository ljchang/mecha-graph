//! Enrichment envelope (§6): one contract for every source — define the shape
//! once, vary only the prompt (or, for Bee, map native fields in for free).

use crate::error::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

pub const ENVELOPE_SCHEMA_VERSION: i64 = 1;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Envelope {
    #[serde(default)]
    pub summary: String,
    #[serde(default)]
    pub key_points: Vec<String>,
    #[serde(default)]
    pub entities: Vec<EntityRef>,
    #[serde(default)]
    pub dates: Vec<DateRef>,
    #[serde(default)]
    pub commitments: Vec<Commitment>,
    #[serde(default)]
    pub topics: Vec<String>,
    #[serde(default)]
    pub decisions: Vec<Decision>,
    #[serde(default)]
    pub open_questions: Vec<String>,
    #[serde(default = "default_sensitivity")]
    pub sensitivity: String,
}

fn default_sensitivity() -> String {
    "personal".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EntityRef {
    pub name: String,
    #[serde(rename = "type")]
    pub entity_type: String,
    /// Bridges straight to node_identifier — deterministic when present (§6).
    #[serde(default)]
    pub identifier: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DateRef {
    pub text: String,
    /// Resolved at enrichment time against episode.occurred_at — "by Friday"
    /// is unresolvable later once the anchor is gone (§6).
    #[serde(default)]
    pub resolved: Option<String>,
    #[serde(default)]
    pub kind: Option<String>, // due|event|mention
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Commitment {
    pub who: String,
    pub what: String,
    #[serde(default)]
    pub when: Option<String>,
    /// "owed_by_me" | "owed_to_me" — inverting this is worse than not
    /// extracting (§6), so extractors must set it explicitly.
    pub direction: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Decision {
    pub choice: String,
    #[serde(default)]
    pub rationale: Option<String>,
}

pub fn store_enrichment(
    conn: &Connection,
    episode_id: i64,
    envelope: &Envelope,
    model: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO episode_enrichment (episode_id, schema_version, payload, model)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(episode_id) DO UPDATE SET
             schema_version = excluded.schema_version,
             payload = excluded.payload,
             model = excluded.model,
             created_at = datetime('now')",
        params![
            episode_id,
            ENVELOPE_SCHEMA_VERSION,
            serde_json::to_string(envelope)?,
            model
        ],
    )?;
    Ok(())
}

pub fn get_enrichment(conn: &Connection, episode_id: i64) -> Result<Option<Envelope>> {
    let payload: Option<String> = conn
        .query_row(
            "SELECT payload FROM episode_enrichment WHERE episode_id = ?1",
            params![episode_id],
            |r| r.get(0),
        )
        .optional()?;
    Ok(payload.and_then(|p| serde_json::from_str(&p).ok()))
}
