use anyhow::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::memory::storage::MemoryStorage;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum FeedbackRating {
    Negative,
    Neutral,
    Positive,
}

impl FeedbackRating {
    pub fn as_int(self) -> i64 {
        match self {
            Self::Negative => -1,
            Self::Neutral => 0,
            Self::Positive => 1,
        }
    }

    pub fn from_label(label: &str) -> Self {
        match label.trim().to_ascii_lowercase().as_str() {
            "negative" | "bad" | "wrong" | "useless" | "downvote" => Self::Negative,
            "positive" | "good" | "helpful" | "upvote" => Self::Positive,
            _ => Self::Neutral,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryFeedback {
    pub id: String,
    pub node_id: String,
    pub query_text: Option<String>,
    pub rating: i64,
    pub reason: Option<String>,
    pub source: Option<String>,
    pub created_at: i64,
}

#[derive(Clone)]
pub struct MemoryFeedbackStore {
    storage: MemoryStorage,
}

impl MemoryFeedbackStore {
    pub fn new(storage: MemoryStorage) -> Self {
        Self { storage }
    }

    pub fn record(
        &self,
        node_id: &str,
        query_text: Option<&str>,
        rating: FeedbackRating,
        reason: Option<&str>,
        source: Option<&str>,
        // Conversation / session id for dedup.  When provided, any previous vote
        // from this session for the same node is removed before inserting — so
        // changing your mind from 👍 to 👎 during a session doesn't double-count.
        user_session: Option<&str>,
    ) -> Result<MemoryFeedback> {
        let id = Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;

        // Dedup: remove the previous vote for this (node, session) so last click wins.
        if let Some(session) = user_session {
            conn.execute(
                "DELETE FROM memory_feedback WHERE node_id = ?1 AND user_session = ?2",
                rusqlite::params![node_id, session],
            )?;
        }

        conn.execute(
            "INSERT INTO memory_feedback (id, node_id, query_text, rating, reason, source, user_session, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                &id,
                node_id,
                query_text.unwrap_or(""),
                rating.as_int(),
                reason.unwrap_or(""),
                source.unwrap_or(""),
                user_session,
                now,
            ],
        )?;

        Ok(MemoryFeedback {
            id,
            node_id: node_id.to_string(),
            query_text: query_text.map(|s| s.to_string()),
            rating: rating.as_int(),
            reason: reason.map(|s| s.to_string()),
            source: source.map(|s| s.to_string()),
            created_at: now,
        })
    }

    pub fn aggregate_for_node(&self, node_id: &str) -> Result<(i64, i64, i64)> {
        let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;
        let (total, positives, negatives): (i64, i64, i64) = conn.query_row(
            "SELECT COALESCE(SUM(rating), 0), COALESCE(SUM(CASE WHEN rating > 0 THEN 1 ELSE 0 END), 0), COALESCE(SUM(CASE WHEN rating < 0 THEN 1 ELSE 0 END), 0) FROM memory_feedback WHERE node_id = ?1",
            [&node_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;

        Ok((total, positives, negatives))
    }

    pub fn confidence_adjustment(&self, node_id: &str) -> Result<f32> {
        let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);

        // Fetch all votes with their ages and apply exponential time-decay so
        // old feedback fades rather than locking a node's score forever.
        // Half-life: 30 days — a vote from 30 days ago counts half as much as
        // a vote from today. A 90-day-old vote is down to ~12%.
        const HALF_LIFE_SECS: f64 = 30.0 * 86_400.0;

        let rows: Vec<(i64, i64)> = conn
            .prepare("SELECT rating, created_at FROM memory_feedback WHERE node_id = ?1")?
            .query_map(rusqlite::params![node_id], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if rows.is_empty() {
            return Ok(0.0);
        }

        let mut weighted_sum = 0.0f64;
        let mut weight_total = 0.0f64;
        for (rating, created_at) in &rows {
            let age_secs = (now - created_at).max(0) as f64;
            let decay = 2.0f64.powf(-age_secs / HALF_LIFE_SECS);
            weighted_sum += (*rating as f64) * decay;
            weight_total += decay;
        }

        if weight_total < 1e-9 {
            return Ok(0.0);
        }
        let normalized = (weighted_sum / weight_total).clamp(-1.0, 1.0);
        Ok((normalized * 0.25) as f32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::graph::{GraphMemory, NodeType};
    use crate::memory::storage::MemoryStorage;

    #[test]
    fn negative_feedback_lowers_confidence() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let node = graph
            .create_node(NodeType::Concept, "bad memory", None)
            .unwrap();

        let store = MemoryFeedbackStore::new(storage);
        store
            .record(
                &node.id,
                Some("query"),
                FeedbackRating::Negative,
                Some("wrong"),
                Some("user"),
                None,
            )
            .unwrap();

        let adjustment = store.confidence_adjustment(&node.id).unwrap();
        assert!(adjustment < 0.0);
    }
}
