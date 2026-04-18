use crate::memory::storage::MemoryStorage;
use rusqlite::Result;
use serde::{Deserialize, Serialize};
use tracing::warn;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRecord {
    pub id: String,
    pub node_id: String,
    pub embedding_type: EmbeddingType,
    pub vector: Vec<f32>,
    pub text_chunk: Option<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingType {
    Summary,
    Content,
    Fact,
    UiSnapshot,
}

impl std::fmt::Display for EmbeddingType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            EmbeddingType::Summary => write!(f, "summary"),
            EmbeddingType::Content => write!(f, "content"),
            EmbeddingType::Fact => write!(f, "fact"),
            EmbeddingType::UiSnapshot => write!(f, "ui_snapshot"),
        }
    }
}

impl std::str::FromStr for EmbeddingType {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "summary" => Ok(EmbeddingType::Summary),
            "content" => Ok(EmbeddingType::Content),
            "fact" => Ok(EmbeddingType::Fact),
            "ui_snapshot" => Ok(EmbeddingType::UiSnapshot),
            _ => Err(format!("Unknown embedding type: {}", s)),
        }
    }
}

pub struct EmbeddingMemory {
    storage: MemoryStorage,
}

impl EmbeddingMemory {
    pub fn new(storage: MemoryStorage) -> Self {
        Self { storage }
    }

    pub fn store(
        &self,
        node_id: &str,
        embedding_type: EmbeddingType,
        vector: &[f32],
        text_chunk: Option<&str>,
    ) -> Result<String> {
        let id = Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let dim = vector.len() as i64;

        let vector_json = serde_json::to_string(vector).unwrap_or_default();
        let text = text_chunk.unwrap_or_default();

        let conn = self.storage.sql_conn()?;
        conn.execute(
            "INSERT INTO embeddings (id, node_id, embedding_type, vector_json, text_chunk, dim, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![&id, node_id, &embedding_type.to_string(), &vector_json, text, dim, now],
        )?;

        Ok(id)
    }

    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
        if a.len() != b.len() || a.is_empty() {
            return 0.0;
        }

        let dot_product: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
        let norm_a: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
        let norm_b: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();

        if norm_a == 0.0 || norm_b == 0.0 {
            return 0.0;
        }

        dot_product / (norm_a * norm_b)
    }

    pub fn search_similar(
        &self,
        query_vector: &[f32],
        embedding_type: Option<EmbeddingType>,
        limit: usize,
    ) -> Result<Vec<(EmbeddingRecord, f32)>> {
        // Nothing to search against — bail early rather than scoring everything 0.0.
        if query_vector.is_empty() {
            warn!("search_similar: query_vector is empty — embedding model may have failed; skipping semantic search");
            return Ok(vec![]);
        }
        let dim = query_vector.len() as i64;
        let conn = self.storage.sql_conn()?;
        let mut results = Vec::new();

        // Filter by dim at the SQL level so vectors from a different model (e.g. a
        // 768-dim encoder mixed with 1536-dim rows) are never scored.  Legacy rows
        // with dim IS NULL are still included for backwards compatibility.
        if let Some(et) = embedding_type {
            let sql = "SELECT id, node_id, embedding_type, vector_json, text_chunk, created_at \
                       FROM embeddings WHERE embedding_type = ?1 AND (dim = ?2 OR dim IS NULL)";
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(
                rusqlite::params![&et.to_string(), dim],
                Self::row_to_embedding,
            )?;
            for row in rows {
                let record = row?;
                let similarity = Self::cosine_similarity(query_vector, &record.vector);
                results.push((record, similarity));
            }
        } else {
            let sql = "SELECT id, node_id, embedding_type, vector_json, text_chunk, created_at \
                       FROM embeddings WHERE (dim = ?1 OR dim IS NULL)";
            let mut stmt = conn.prepare(sql)?;
            let rows = stmt.query_map(rusqlite::params![dim], Self::row_to_embedding)?;
            for row in rows {
                let record = row?;
                let similarity = Self::cosine_similarity(query_vector, &record.vector);
                results.push((record, similarity));
            }
        }

        results.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        results.truncate(limit);

        Ok(results)
    }

    fn row_to_embedding(row: &rusqlite::Row<'_>) -> rusqlite::Result<EmbeddingRecord> {
        let type_str: String = row.get(2)?;
        let et = type_str.parse().unwrap_or(EmbeddingType::Summary);
        let vector_str: String = row.get(3)?;
        let vector: Vec<f32> = serde_json::from_str(&vector_str).unwrap_or_default();

        Ok(EmbeddingRecord {
            id: row.get(0)?,
            node_id: row.get(1)?,
            embedding_type: et,
            vector,
            text_chunk: {
                let s: String = row.get(4)?;
                if s.is_empty() {
                    None
                } else {
                    Some(s)
                }
            },
            created_at: row.get(5)?,
        })
    }

    #[allow(dead_code)]
    pub fn search_by_text(
        &self,
        query_vector: &[f32],
        limit: usize,
    ) -> Result<Vec<(EmbeddingRecord, f32)>> {
        self.search_similar(query_vector, None, limit)
    }

    pub fn get_embeddings_for_node(&self, node_id: &str) -> Result<Vec<EmbeddingRecord>> {
        let conn = self.storage.sql_conn()?;
        let mut stmt = conn.prepare(
            "SELECT id, node_id, embedding_type, vector_json, text_chunk, created_at FROM embeddings WHERE node_id = ?1"
        )?;

        let rows = stmt.query_map([&node_id], |row| {
            let type_str: String = row.get(2)?;
            let et = type_str.parse().unwrap_or(EmbeddingType::Summary);
            let vector_str: String = row.get(3)?;
            let vector: Vec<f32> = serde_json::from_str(&vector_str).unwrap_or_default();

            Ok(EmbeddingRecord {
                id: row.get(0)?,
                node_id: row.get(1)?,
                embedding_type: et,
                vector,
                text_chunk: {
                    let s: String = row.get(4)?;
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                },
                created_at: row.get(5)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }

        Ok(result)
    }

    pub fn delete_embeddings_for_node(&self, node_id: &str) -> Result<()> {
        let conn = self.storage.sql_conn()?;
        conn.execute("DELETE FROM embeddings WHERE node_id = ?1", [node_id])?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{
        graph::{GraphMemory, NodeType},
        storage::MemoryStorage,
    };

    fn make_vec(x: f32, y: f32, z: f32) -> Vec<f32> {
        let v = vec![x, y, z];
        let mag = (v.iter().map(|a| a * a).sum::<f32>()).sqrt();
        if mag == 0.0 {
            v
        } else {
            v.iter().map(|a| a / mag).collect()
        }
    }

    #[test]
    fn cosine_similarity_identical_is_one() {
        let v = make_vec(1.0, 2.0, 3.0);
        let sim = EmbeddingMemory::cosine_similarity(&v, &v);
        assert!(
            (sim - 1.0).abs() < 1e-5,
            "identical vectors should have similarity ~1.0"
        );
    }

    #[test]
    fn cosine_similarity_orthogonal_is_zero() {
        let a = make_vec(1.0, 0.0, 0.0);
        let b = make_vec(0.0, 1.0, 0.0);
        let sim = EmbeddingMemory::cosine_similarity(&a, &b);
        assert!(
            sim.abs() < 1e-5,
            "orthogonal vectors should have similarity ~0.0"
        );
    }

    #[test]
    fn store_and_search_returns_stored_record() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let node = graph.create_node(NodeType::Concept, "test", None).unwrap();
        let em = EmbeddingMemory::new(storage);
        let v = make_vec(1.0, 0.0, 0.0);
        let id = em
            .store(&node.id, EmbeddingType::Summary, &v, Some("hello"))
            .unwrap();
        assert!(!id.is_empty());

        let results = em.search_similar(&v, None, 5).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].0.node_id, node.id);
        assert!((results[0].1 - 1.0).abs() < 1e-5);
    }

    #[test]
    fn delete_embeddings_for_node_removes_records() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let node = graph
            .create_node(NodeType::Concept, "delete-me", None)
            .unwrap();
        let em = EmbeddingMemory::new(storage);
        let v = make_vec(1.0, 0.0, 0.0);
        em.store(&node.id, EmbeddingType::Summary, &v, None)
            .unwrap();
        em.delete_embeddings_for_node(&node.id).unwrap();
        let results = em.search_similar(&v, None, 5).unwrap();
        assert!(results.is_empty());
    }

    #[test]
    fn search_similar_ranks_closest_first() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let n_close = graph.create_node(NodeType::Concept, "close", None).unwrap();
        let n_far = graph.create_node(NodeType::Concept, "far", None).unwrap();
        let em = EmbeddingMemory::new(storage);
        let close = make_vec(1.0, 0.1, 0.0);
        let far = make_vec(0.0, 0.0, 1.0);
        let query = make_vec(1.0, 0.0, 0.0);
        em.store(&n_close.id, EmbeddingType::Summary, &close, None)
            .unwrap();
        em.store(&n_far.id, EmbeddingType::Summary, &far, None)
            .unwrap();
        let results = em.search_similar(&query, None, 5).unwrap();
        assert_eq!(
            results[0].0.node_id, n_close.id,
            "closest node should rank first"
        );
    }
}
