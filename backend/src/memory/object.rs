#[allow(dead_code)]
use crate::memory::storage::MemoryStorage;
use rusqlite::{OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObjectMemory {
    pub id: String,
    pub node_id: String,
    pub summary: Option<String>,
    pub key_facts: Vec<String>,
    pub extracted_structure: Option<String>,
    pub code_signatures: Option<String>,
    pub todos: Vec<String>,
    pub ui_snapshot: Option<serde_json::Value>,
    pub tags: Vec<String>,
    pub content_hash: Option<String>,
    pub last_indexed: i64,
    /// Number of times this node has been retrieved from memory. Used by
    /// usage_score and Ebbinghaus recency to reward frequently accessed nodes.
    pub access_count: i64,
}

pub struct ObjectMemoryStore {
    storage: MemoryStorage,
}

impl ObjectMemoryStore {
    pub fn new(storage: MemoryStorage) -> Self {
        Self { storage }
    }

    pub fn upsert(&self, node_id: &str, object: &ObjectMemory) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let key_facts_json = serde_json::to_string(&object.key_facts).unwrap_or_default();
        let todos_json = serde_json::to_string(&object.todos).unwrap_or_default();
        let tags_json = serde_json::to_string(&object.tags).unwrap_or_default();
        let ui_snapshot_json = object
            .ui_snapshot
            .as_ref()
            .map(|v| serde_json::to_string(v).unwrap_or_default())
            .unwrap_or_default();

        let conn = self.storage.get_connection().unwrap();
        // access_count is omitted from ON CONFLICT UPDATE so it is only
        // ever incremented by touch_accessed() — upserts never reset it.
        conn.execute(
            "INSERT INTO object_memory (id, node_id, summary, key_facts_json, extracted_structure, code_signatures, todos_json, ui_snapshot_json, tags_json, content_hash, last_indexed, access_count)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)
             ON CONFLICT(node_id) DO UPDATE SET
                summary = excluded.summary,
                key_facts_json = excluded.key_facts_json,
                extracted_structure = excluded.extracted_structure,
                code_signatures = excluded.code_signatures,
                todos_json = excluded.todos_json,
                ui_snapshot_json = excluded.ui_snapshot_json,
                tags_json = excluded.tags_json,
                content_hash = excluded.content_hash,
                last_indexed = excluded.last_indexed",
            [
                &object.id,
                node_id,
                &object.summary.clone().unwrap_or_default(),
                &key_facts_json,
                &object.extracted_structure.clone().unwrap_or_default(),
                &object.code_signatures.clone().unwrap_or_default(),
                &todos_json,
                &ui_snapshot_json,
                &tags_json,
                &object.content_hash.clone().unwrap_or_default(),
                &now.to_string(),
                &object.access_count.to_string(),
            ],
        )?;

        Ok(())
    }

    pub fn get_by_node_id(&self, node_id: &str) -> Result<Option<ObjectMemory>> {
        let conn = self.storage.get_connection().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, node_id, summary, key_facts_json, extracted_structure, code_signatures, todos_json, ui_snapshot_json, tags_json, content_hash, last_indexed, COALESCE(access_count, 0)
             FROM object_memory WHERE node_id = ?1"
        )?;

        let result = stmt
            .query_row([&node_id], |row| {
                let key_facts_str: String = row.get(3)?;
                let key_facts = serde_json::from_str(&key_facts_str).unwrap_or_default();

                let todos_str: String = row.get(6)?;
                let todos = serde_json::from_str(&todos_str).unwrap_or_default();

                let tags_str: String = row.get(8)?;
                let tags = serde_json::from_str(&tags_str).unwrap_or_default();

                let ui_snapshot_str: String = row.get(7)?;
                let ui_snapshot = if ui_snapshot_str.is_empty() {
                    None
                } else {
                    serde_json::from_str(&ui_snapshot_str).ok()
                };

                Ok(ObjectMemory {
                    id: row.get(0)?,
                    node_id: row.get(1)?,
                    summary: {
                        let s: String = row.get(2)?;
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    },
                    key_facts,
                    extracted_structure: {
                        let s: String = row.get(4)?;
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    },
                    code_signatures: {
                        let s: String = row.get(5)?;
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    },
                    todos,
                    ui_snapshot,
                    tags,
                    content_hash: {
                        let s: String = row.get(9)?;
                        if s.is_empty() {
                            None
                        } else {
                            Some(s)
                        }
                    },
                    last_indexed: row.get(10)?,
                    access_count: row.get(11)?,
                })
            })
            .optional()?;

        Ok(result)
    }

    pub fn create_for_node(node_id: &str) -> ObjectMemory {
        ObjectMemory {
            id: Uuid::new_v4().to_string(),
            node_id: node_id.to_string(),
            summary: None,
            key_facts: Vec::new(),
            extracted_structure: None,
            code_signatures: None,
            todos: Vec::new(),
            ui_snapshot: None,
            tags: Vec::new(),
            content_hash: None,
            last_indexed: 0,
            access_count: 0,
        }
    }

    pub fn add_key_fact(&self, node_id: &str, fact: &str) -> Result<()> {
        #[allow(dead_code)]
        if let Some(mut obj) = self.get_by_node_id(node_id)? {
            obj.key_facts.push(fact.to_string());
            self.upsert(node_id, &obj)?;
        }
        Ok(())
    }
    pub fn add_todo(&self, node_id: &str, todo: &str) -> Result<()> {
        #[allow(dead_code)]
        if let Some(mut obj) = self.get_by_node_id(node_id)? {
            obj.todos.push(todo.to_string());
            self.upsert(node_id, &obj)?;
        }
        Ok(())
    }

    pub fn add_tag(&self, node_id: &str, tag: &str) -> Result<()> {
        #[allow(dead_code)]
        if let Some(mut obj) = self.get_by_node_id(node_id)? {
            if !obj.tags.contains(&tag.to_string()) {
                obj.tags.push(tag.to_string());
                self.upsert(node_id, &obj)?;
            }
        }
        Ok(())
    }

    /// Update the last_accessed timestamp for a node (called when node is retrieved from memory).
    /// Also bumps `nodes.updated_at` so recency scoring reflects genuine access patterns.
    pub fn touch_accessed(&self, node_id: &str) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let conn = self.storage.get_connection().unwrap();
        let _ = conn.execute(
            "UPDATE object_memory SET last_indexed = ?1, access_count = access_count + 1 WHERE node_id = ?2",
            rusqlite::params![now, node_id],
        );
        // Also update the graph node's updated_at so recency_score sees the latest access
        let _ = conn.execute(
            "UPDATE nodes SET updated_at = ?1 WHERE id = ?2",
            rusqlite::params![now, node_id],
        );
        Ok(())
    }

    pub fn search_by_tag(&self, tag: &str) -> Result<Vec<ObjectMemory>> {
        #[allow(dead_code)]
        let conn = self.storage.get_connection().unwrap();
        let pattern = format!("%\"{}\"%", tag);
        let mut stmt = conn.prepare(
            "SELECT id, node_id, summary, key_facts_json, extracted_structure, code_signatures, todos_json, ui_snapshot_json, tags_json, content_hash, last_indexed, COALESCE(access_count, 0)
             FROM object_memory WHERE tags_json LIKE ?1"
        )?;

        let rows = stmt.query_map([&pattern], |row| {
            let key_facts_str: String = row.get(3)?;
            let key_facts = serde_json::from_str(&key_facts_str).unwrap_or_default();

            let todos_str: String = row.get(6)?;
            let todos = serde_json::from_str(&todos_str).unwrap_or_default();

            let tags_str: String = row.get(8)?;
            let tags = serde_json::from_str(&tags_str).unwrap_or_default();

            let ui_snapshot_str: String = row.get(7)?;
            let ui_snapshot = if ui_snapshot_str.is_empty() {
                None
            } else {
                serde_json::from_str(&ui_snapshot_str).ok()
            };

            Ok(ObjectMemory {
                id: row.get(0)?,
                node_id: row.get(1)?,
                summary: {
                    let s: String = row.get(2)?;
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                },
                key_facts,
                extracted_structure: {
                    let s: String = row.get(4)?;
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                },
                code_signatures: {
                    let s: String = row.get(5)?;
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                },
                todos,
                ui_snapshot,
                tags,
                content_hash: {
                    let s: String = row.get(9)?;
                    if s.is_empty() {
                        None
                    } else {
                        Some(s)
                    }
                },
                last_indexed: row.get(10)?,
                access_count: row.get(11)?,
            })
        })?;

        let mut result = Vec::new();
        for row in rows {
            result.push(row?);
        }

        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::{graph::GraphMemory, graph::NodeType, storage::MemoryStorage};

    fn make_store() -> (MemoryStorage, GraphMemory, ObjectMemoryStore) {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let store = ObjectMemoryStore::new(storage.clone());
        (storage, graph, store)
    }

    #[test]
    fn upsert_and_get_roundtrip() {
        let (_, graph, store) = make_store();
        let node = graph
            .create_node(NodeType::Concept, "wiki page", None)
            .unwrap();
        let mut obj = ObjectMemoryStore::create_for_node(&node.id);
        obj.summary = Some("A test summary".to_string());
        obj.key_facts = vec!["fact one".to_string(), "fact two".to_string()];
        store.upsert(&node.id, &obj).unwrap();

        let fetched = store
            .get_by_node_id(&node.id)
            .unwrap()
            .expect("should exist");
        assert_eq!(fetched.summary.as_deref(), Some("A test summary"));
        assert_eq!(fetched.key_facts.len(), 2);
    }

    #[test]
    fn upsert_overwrites_existing() {
        let (_, graph, store) = make_store();
        let node = graph.create_node(NodeType::File, "main.rs", None).unwrap();
        let mut obj = ObjectMemoryStore::create_for_node(&node.id);
        obj.summary = Some("first".to_string());
        store.upsert(&node.id, &obj).unwrap();

        obj.summary = Some("second".to_string());
        store.upsert(&node.id, &obj).unwrap();

        let fetched = store.get_by_node_id(&node.id).unwrap().unwrap();
        assert_eq!(fetched.summary.as_deref(), Some("second"));
    }

    #[test]
    fn get_by_node_id_returns_none_for_missing() {
        let (_, _, store) = make_store();
        let result = store.get_by_node_id("nonexistent-id").unwrap();
        assert!(result.is_none());
    }

    #[test]
    fn touch_accessed_updates_timestamp() {
        let (_, graph, store) = make_store();
        let node = graph
            .create_node(NodeType::Concept, "touched node", None)
            .unwrap();
        let mut obj = ObjectMemoryStore::create_for_node(&node.id);
        obj.last_indexed = 0;
        store.upsert(&node.id, &obj).unwrap();

        store.touch_accessed(&node.id).unwrap();
        let fetched = store.get_by_node_id(&node.id).unwrap().unwrap();
        assert!(
            fetched.last_indexed > 0,
            "last_indexed should be updated by touch_accessed"
        );
    }
}
