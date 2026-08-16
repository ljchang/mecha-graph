//! `person_interaction` rollup (§4.6): makes "when did I last meet June?" a
//! primary-key lookup. Per-channel recency matters semantically — "met" means
//! calendar or Bee co-presence, not email.

use crate::error::Result;
use rusqlite::{params, Connection};

/// Map an episode source to the per-channel recency column it updates.
fn channel_column(source: &str) -> Option<&'static str> {
    match source {
        "calendar.event" => Some("last_meeting_at"),
        "bee.conversation" | "bee.daily" => Some("last_spoken_at"),
        "email.thread" => Some("last_email_at"),
        "sms" => Some("last_message_at"),
        "slack.thread" => Some("last_slack_at"),
        _ => None,
    }
}

/// Rebuild the rollup for every person from `mention` × `episode`.
/// Idempotent and cheap at personal scale; run after each ingest batch.
pub fn rebuild_person_interactions(conn: &Connection) -> Result<usize> {
    conn.execute("DELETE FROM person_interaction", [])?;

    conn.execute_batch(
        "INSERT INTO person_interaction
             (node_id, first_seen_at, last_seen_at, last_channel, last_episode_id, interaction_count)
         SELECT m.node_id,
                MIN(e.occurred_at),
                MAX(e.occurred_at),
                (SELECT e2.source FROM episode e2 JOIN mention m2 ON m2.episode_id = e2.id
                 WHERE m2.node_id = m.node_id AND e2.occurred_at <= datetime('now') ORDER BY e2.occurred_at DESC LIMIT 1),
                (SELECT e2.uid FROM episode e2 JOIN mention m2 ON m2.episode_id = e2.id
                 WHERE m2.node_id = m.node_id AND e2.occurred_at <= datetime('now') ORDER BY e2.occurred_at DESC LIMIT 1),
                COUNT(*)
         FROM mention m
         JOIN episode e ON e.id = m.episode_id
         JOIN nodes n ON n.id = m.node_id
         WHERE n.node_type = 'person'
           AND e.occurred_at <= datetime('now')   -- future meetings aren't interactions yet
         GROUP BY m.node_id;",
    )?;

    for (source, col) in [
        ("calendar.event", "last_meeting_at"),
        ("bee.conversation", "last_spoken_at"),
        ("email.thread", "last_email_at"),
        ("sms", "last_message_at"),
        ("slack.thread", "last_slack_at"),
    ] {
        conn.execute(
            &format!(
                "UPDATE person_interaction SET {col} = (
                     SELECT MAX(e.occurred_at) FROM episode e
                     JOIN mention m ON m.episode_id = e.id
                     WHERE m.node_id = person_interaction.node_id AND e.source = ?1 AND e.occurred_at <= datetime('now')
                 )"
            ),
            params![source],
        )?;
    }

    let n: i64 = conn.query_row("SELECT COUNT(*) FROM person_interaction", [], |r| r.get(0))?;
    Ok(n as usize)
}

/// Incremental update for one (episode, person) pair at ingest time.
/// Future episodes (scheduled meetings) are not interactions yet — the
/// nightly rebuild picks them up once they've happened.
pub fn touch_person(
    conn: &Connection,
    node_id: &str,
    episode_uid: &str,
    source: &str,
    occurred_at: &str,
) -> Result<()> {
    if occurred_at > crate::ids::now().as_str() {
        return Ok(());
    }
    conn.execute(
        "INSERT INTO person_interaction (node_id, first_seen_at, last_seen_at, last_channel,
                                         last_episode_id, interaction_count)
         VALUES (?1, ?2, ?2, ?3, ?4, 1)
         ON CONFLICT(node_id) DO UPDATE SET
             first_seen_at = MIN(first_seen_at, excluded.first_seen_at),
             last_seen_at = MAX(last_seen_at, excluded.last_seen_at),
             last_channel = CASE WHEN excluded.last_seen_at >= last_seen_at
                                 THEN excluded.last_channel ELSE last_channel END,
             last_episode_id = CASE WHEN excluded.last_seen_at >= last_seen_at
                                    THEN excluded.last_episode_id ELSE last_episode_id END,
             interaction_count = interaction_count + 1",
        params![node_id, occurred_at, source, episode_uid],
    )?;

    if let Some(col) = channel_column(source) {
        conn.execute(
            &format!(
                "UPDATE person_interaction SET {col} = MAX(COALESCE({col}, ''), ?2) WHERE node_id = ?1"
            ),
            params![node_id, occurred_at],
        )?;
    }
    Ok(())
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct PersonInteraction {
    pub node_id: String,
    pub first_seen_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub last_channel: Option<String>,
    pub last_episode_id: Option<String>,
    pub interaction_count: i64,
    pub last_meeting_at: Option<String>,
    pub last_spoken_at: Option<String>,
    pub last_email_at: Option<String>,
    pub last_message_at: Option<String>,
    pub last_slack_at: Option<String>,
}

pub fn get_person_interaction(
    conn: &Connection,
    node_id: &str,
) -> Result<Option<PersonInteraction>> {
    use rusqlite::OptionalExtension;
    Ok(conn
        .query_row(
            "SELECT * FROM person_interaction WHERE node_id = ?1",
            params![node_id],
            |r| {
                Ok(PersonInteraction {
                    node_id: r.get("node_id")?,
                    first_seen_at: r.get("first_seen_at")?,
                    last_seen_at: r.get("last_seen_at")?,
                    last_channel: r.get("last_channel")?,
                    last_episode_id: r.get("last_episode_id")?,
                    interaction_count: r.get("interaction_count")?,
                    last_meeting_at: r.get("last_meeting_at")?,
                    last_spoken_at: r.get("last_spoken_at")?,
                    last_email_at: r.get("last_email_at")?,
                    last_message_at: r.get("last_message_at")?,
                    last_slack_at: r.get("last_slack_at")?,
                })
            },
        )
        .optional()?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::open_memory;
    use crate::episode::{add_mention, upsert_episode, Episode};
    use crate::graph::{upsert_node, Node};

    #[test]
    fn test_rollup_per_channel_recency() {
        let conn = open_memory().unwrap();
        upsert_node(&conn, &Node::new("june", "person", "June")).unwrap();

        let mk = |src: &str, sid: &str, at: &str| Episode {
            id: 0,
            uid: String::new(),
            source: src.into(),
            source_id: sid.into(),
            source_ref: None,
            body: format!("episode {sid}"),
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
        };

        // Email is most recent, but the last *meeting* was earlier.
        let (e1, _) =
            upsert_episode(&conn, &mk("calendar.event", "c1", "2026-07-30 10:00:00")).unwrap();
        let (e2, _) =
            upsert_episode(&conn, &mk("email.thread", "m1", "2026-08-01 09:00:00")).unwrap();
        add_mention(&conn, e1, "june", "attendee", 1.0).unwrap();
        add_mention(&conn, e2, "june", "attendee", 1.0).unwrap();

        rebuild_person_interactions(&conn).unwrap();
        let pi = get_person_interaction(&conn, "june").unwrap().unwrap();
        assert_eq!(pi.interaction_count, 2);
        assert_eq!(pi.last_seen_at.as_deref(), Some("2026-08-01 09:00:00"));
        assert_eq!(pi.last_meeting_at.as_deref(), Some("2026-07-30 10:00:00"));
        assert_eq!(pi.last_email_at.as_deref(), Some("2026-08-01 09:00:00"));
        assert_eq!(pi.last_channel.as_deref(), Some("email.thread"));
    }
}
