use crate::memory::storage::MemoryStorage;
use rusqlite::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Node {
    pub id: String,
    pub node_type: NodeType,
    pub title: String,
    pub created_at: i64,
    pub updated_at: i64,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NodeType {
    Project,
    File,
    /// A semantically-embedded chunk of a File (32-line window, neural embedding).
    FileChunk,
    Website,
    Tool,
    Task,
    Concept,
    UiState,
    Dependency,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Edge {
    pub id: String,
    pub source_id: String,
    pub target_id: String,
    pub relationship: Relationship,
    pub strength: f64,
    pub created_at: i64,
    pub metadata: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relationship {
    Contains,
    DependsOn,
    References,
    Implements,
    Modifies,
    RelatesTo,
    Inherits,
    Calls,
    Imports,
    Documents,
}

impl std::fmt::Display for Relationship {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Relationship::Contains => write!(f, "contains"),
            Relationship::DependsOn => write!(f, "depends_on"),
            Relationship::References => write!(f, "references"),
            Relationship::Implements => write!(f, "implements"),
            Relationship::Modifies => write!(f, "modifies"),
            Relationship::RelatesTo => write!(f, "relates_to"),
            Relationship::Inherits => write!(f, "inherits"),
            Relationship::Calls => write!(f, "calls"),
            Relationship::Imports => write!(f, "imports"),
            Relationship::Documents => write!(f, "documents"),
        }
    }
}

impl std::str::FromStr for Relationship {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "contains" => Ok(Relationship::Contains),
            "depends_on" => Ok(Relationship::DependsOn),
            "references" => Ok(Relationship::References),
            "implements" => Ok(Relationship::Implements),
            "modifies" => Ok(Relationship::Modifies),
            "relates_to" => Ok(Relationship::RelatesTo),
            "inherits" => Ok(Relationship::Inherits),
            "calls" => Ok(Relationship::Calls),
            "imports" => Ok(Relationship::Imports),
            "documents" => Ok(Relationship::Documents),
            _ => Err(format!("Unknown relationship: {}", s)),
        }
    }
}

impl std::fmt::Display for NodeType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            NodeType::Project => write!(f, "project"),
            NodeType::File => write!(f, "file"),
            NodeType::FileChunk => write!(f, "file_chunk"),
            NodeType::Website => write!(f, "website"),
            NodeType::Tool => write!(f, "tool"),
            NodeType::Task => write!(f, "task"),
            NodeType::Concept => write!(f, "concept"),
            NodeType::UiState => write!(f, "ui_state"),
            NodeType::Dependency => write!(f, "dependency"),
        }
    }
}

impl std::str::FromStr for NodeType {
    type Err = String;
    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "project" => Ok(NodeType::Project),
            "file" => Ok(NodeType::File),
            "file_chunk" => Ok(NodeType::FileChunk),
            "website" => Ok(NodeType::Website),
            "tool" => Ok(NodeType::Tool),
            "task" => Ok(NodeType::Task),
            "concept" => Ok(NodeType::Concept),
            "ui_state" => Ok(NodeType::UiState),
            "dependency" => Ok(NodeType::Dependency),
            _ => Err(format!("Unknown node type: {}", s)),
        }
    }
}

pub struct GraphMemory {
    storage: MemoryStorage,
}

fn parse_node(row: &rusqlite::Row<'_>) -> rusqlite::Result<Node> {
    let node_type_str: String = row.get(1)?;
    let node_type = node_type_str.parse().unwrap_or(NodeType::Concept);
    let metadata_str: Option<String> = row.get(5)?;
    let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
    Ok(Node {
        id: row.get(0)?,
        node_type,
        title: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
        metadata,
    })
}

fn parse_edge(row: &rusqlite::Row<'_>) -> rusqlite::Result<Edge> {
    let rel_str: String = row.get(7)?;
    let relationship = rel_str.parse().unwrap_or(Relationship::RelatesTo);
    let metadata_str: Option<String> = row.get(10)?;
    let metadata = metadata_str.and_then(|s| serde_json::from_str(&s).ok());
    Ok(Edge {
        id: row.get(6)?,
        source_id: row.get(11)?,
        target_id: row.get(12)?,
        relationship,
        strength: row.get(8)?,
        created_at: row.get(9)?,
        metadata,
    })
}

/// Hard cap on graph nodes. Above this threshold, eviction runs automatically.
/// Ten thousand is plenty. if you have more facts about your life than that
/// you should probably write a memoir instead of running this.
const NODE_LIMIT: i64 = 10_000;
/// How many extra nodes to evict per pass (keeps eviction infrequent).
const EVICT_BATCH: i64 = 500;

impl GraphMemory {
    pub fn new(storage: MemoryStorage) -> Self {
        Self { storage }
    }

    pub fn create_node(
        &self,
        node_type: NodeType,
        title: &str,
        metadata: Option<serde_json::Value>,
    ) -> Result<Node> {
        let id = Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let metadata_json = metadata
            .as_ref()
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .unwrap_or_default();

        let conn = self.storage.sql_conn()?;
        conn.execute(
            "INSERT INTO nodes (id, node_type, title, created_at, updated_at, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            [&id, &node_type.to_string(), title, &now.to_string(), &now.to_string(), &metadata_json],
        )?;

        // Best-effort eviction - never fails the insert.
        self.evict_if_over_limit();

        Ok(Node {
            id,
            node_type,
            title: title.to_string(),
            created_at: now,
            updated_at: now,
            metadata,
        })
    }

    /// Silently prunes the oldest nodes when the graph exceeds NODE_LIMIT.
    /// Strategy: remove isolated nodes (no edges) first - they carry no
    /// relationship context - then fall back to oldest-by-updated_at.
    /// Edge CASCADE ensures referencing rows are cleaned up automatically.
    fn evict_if_over_limit(&self) {
        let conn = match self.storage.get_connection() {
            Ok(c) => c,
            Err(_) => return,
        };

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap_or(0);

        if count <= NODE_LIMIT {
            return;
        }

        let to_evict = count - NODE_LIMIT + EVICT_BATCH;

        // Pass 1: isolated nodes (no edges) - cheapest to remove.
        // Exclude pinned nodes (user_facts key = 'node_pinned:<id>', value = 'true').
        let _ = conn.execute(
            "DELETE FROM nodes WHERE id IN (
                SELECT n.id FROM nodes n
                LEFT JOIN edges e ON e.source_id = n.id OR e.target_id = n.id
                LEFT JOIN user_facts uf ON uf.key = 'node_pinned:' || n.id AND uf.value = 'true'
                WHERE e.id IS NULL AND uf.key IS NULL
                ORDER BY n.updated_at ASC
                LIMIT ?1
            )",
            rusqlite::params![to_evict],
        );

        // Pass 2: if still over limit, remove oldest by updated_at (still respecting pins).
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
            .unwrap_or(0);

        if count > NODE_LIMIT {
            let to_evict = count - NODE_LIMIT + EVICT_BATCH;
            let _ = conn.execute(
                "DELETE FROM nodes WHERE id IN (
                    SELECT n.id FROM nodes n
                    LEFT JOIN user_facts uf ON uf.key = 'node_pinned:' || n.id AND uf.value = 'true'
                    WHERE uf.key IS NULL
                    ORDER BY n.updated_at ASC LIMIT ?1
                )",
                rusqlite::params![to_evict],
            );
        }

        // Reclaim pages freed by the DELETE - incremental_vacuum releases one
        // free-page batch per call without blocking the connection pool.
        let _ = conn.execute_batch("PRAGMA incremental_vacuum(100);");
    }

    pub fn create_edge(
        &self,
        source_id: &str,
        target_id: &str,
        relationship: Relationship,
        strength: f64,
        metadata: Option<serde_json::Value>,
    ) -> Result<Edge> {
        let id = Uuid::new_v4().to_string();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let metadata_json = metadata
            .as_ref()
            .map(|m| serde_json::to_string(m).unwrap_or_default())
            .unwrap_or_default();

        let conn = self.storage.sql_conn()?;
        conn.execute(
            "INSERT INTO edges (id, source_id, target_id, relationship, strength, created_at, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            [&id, source_id, target_id, &relationship.to_string(), &strength.to_string(), &now.to_string(), &metadata_json],
        )?;

        Ok(Edge {
            id,
            source_id: source_id.to_string(),
            target_id: target_id.to_string(),
            relationship,
            strength,
            created_at: now,
            metadata,
        })
    }

    pub fn get_node(&self, id: &str) -> Result<Option<Node>> {
        let conn = self.storage.sql_conn()?;
        let mut stmt = conn.prepare("SELECT id, node_type, title, created_at, updated_at, metadata_json FROM nodes WHERE id = ?1")?;
        let result = stmt.query_row([&id], parse_node).ok();
        Ok(result)
    }

    pub fn get_connected_nodes(
        &self,
        node_id: &str,
        relationship: Option<Relationship>,
    ) -> Result<Vec<(Node, Edge)>> {
        let conn = self.storage.sql_conn()?;
        let query = match &relationship {
              Some(_rel) => "SELECT n.id, n.node_type, n.title, n.created_at, n.updated_at, n.metadata_json, \
                  e.id, e.relationship, e.strength, e.created_at, e.metadata_json, \
                  e.source_id, e.target_id \
                  FROM nodes n JOIN edges e ON (n.id = e.source_id OR n.id = e.target_id) \
                  WHERE (e.source_id = ?1 OR e.target_id = ?1) AND n.id != ?1 AND e.relationship = ?2".to_string(),
              None => "SELECT n.id, n.node_type, n.title, n.created_at, n.updated_at, n.metadata_json, \
                  e.id, e.relationship, e.strength, e.created_at, e.metadata_json, \
                  e.source_id, e.target_id \
                  FROM nodes n JOIN edges e ON (n.id = e.source_id OR n.id = e.target_id) \
                  WHERE (e.source_id = ?1 OR e.target_id = ?1) AND n.id != ?1".to_string(),
        };

        let mut stmt = conn.prepare(&query)?;
        let rows: Box<dyn Iterator<Item = rusqlite::Result<(Node, Edge)>>> = match &relationship {
            Some(rel) => {
                let rel_str = rel.to_string();
                Box::new(
                    stmt.query_map(rusqlite::params![&node_id, &rel_str], |row| {
                        Ok((parse_node(row)?, parse_edge(row)?))
                    })?,
                )
            }
            None => Box::new(
                stmt.query_map([&node_id], |row| Ok((parse_node(row)?, parse_edge(row)?)))?,
            ),
        };

        Ok(rows.filter_map(|r| r.ok()).collect())
    }

    /// Create a directed edge only if no edge with the same (source, target, relationship) already
    /// exists in either direction. Returns `None` if a duplicate was detected.
    pub fn create_edge_if_not_exists(
        &self,
        source_id: &str,
        target_id: &str,
        relationship: Relationship,
        strength: f64,
    ) -> Result<Option<Edge>> {
        let exists = {
            let conn = self.storage.sql_conn()?;
            conn.query_row(
                "SELECT COUNT(*) FROM edges WHERE ((source_id = ?1 AND target_id = ?2) OR (source_id = ?2 AND target_id = ?1)) AND relationship = ?3",
                rusqlite::params![source_id, target_id, relationship.to_string()],
                |row| row.get::<_, i64>(0),
            ).map(|c| c > 0).unwrap_or(false)
        };
        if exists {
            return Ok(None);
        }
        self.create_edge(source_id, target_id, relationship, strength, None)
            .map(Some)
    }

    pub fn update_node(
        &self,
        id: &str,
        title: Option<&str>,
        metadata: Option<serde_json::Value>,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let mut conn = self.storage.sql_conn()?;
        let tx = conn.transaction()?;
        if let Some(t) = title {
            tx.execute(
                "UPDATE nodes SET title = ?1, updated_at = ?2 WHERE id = ?3",
                [t, &now.to_string(), id],
            )?;
        }
        if let Some(m) = metadata {
            let mj = serde_json::to_string(&m)
                .map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            tx.execute(
                "UPDATE nodes SET metadata_json = ?1, updated_at = ?2 WHERE id = ?3",
                [&mj, &now.to_string(), id],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    #[allow(dead_code)]
    pub fn delete_node(&self, id: &str) -> Result<()> {
        let conn = self.storage.sql_conn()?;
        conn.execute("DELETE FROM nodes WHERE id = ?1", [id])?;
        Ok(())
    }

    pub fn search_nodes(
        &self,
        node_type: Option<NodeType>,
        query: Option<&str>,
    ) -> Result<Vec<Node>> {
        let conn = self.storage.sql_conn()?;
        let mut sql = String::from("SELECT id, node_type, title, created_at, updated_at, metadata_json FROM nodes WHERE 1=1");
        let mut params: Vec<String> = Vec::new();

        if let Some(nt) = &node_type {
            sql.push_str(" AND node_type = ?");
            params.push(nt.to_string());
        }
        if let Some(q) = query {
            // Escape LIKE wildcards to prevent user input from acting as patterns
            let escaped = q
                .replace('\\', "\\\\")
                .replace('%', "\\%")
                .replace('_', "\\_");
            sql.push_str(" AND (title LIKE ? ESCAPE '\\' OR metadata_json LIKE ? ESCAPE '\\')");
            params.push(format!("%{}%", escaped));
            params.push(format!("%{}%", escaped));
        }

        sql.push_str(" ORDER BY created_at DESC LIMIT 500");
        let mut stmt = conn.prepare(&sql)?;
        let param_refs: Vec<&dyn rusqlite::ToSql> =
            params.iter().map(|p| p as &dyn rusqlite::ToSql).collect();
        let rows = stmt.query_map(rusqlite::params_from_iter(param_refs.iter()), parse_node)?;
        Ok(rows.filter_map(|r| r.ok()).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::storage::MemoryStorage;

    #[test]
    fn create_and_get_node_roundtrip() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage);
        let node = graph
            .create_node(NodeType::Concept, "test node", None)
            .unwrap();
        let fetched = graph
            .get_node(&node.id)
            .unwrap()
            .expect("node should exist");
        assert_eq!(fetched.id, node.id);
        assert_eq!(fetched.title, "test node");
        assert!(matches!(fetched.node_type, NodeType::Concept));
    }

    #[test]
    fn update_node_changes_title_and_metadata() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage);
        let node = graph.create_node(NodeType::File, "original", None).unwrap();
        graph
            .update_node(
                &node.id,
                Some("updated"),
                Some(serde_json::json!({ "k": "v" })),
            )
            .unwrap();
        let fetched = graph.get_node(&node.id).unwrap().unwrap();
        assert_eq!(fetched.title, "updated");
        assert_eq!(fetched.metadata.unwrap()["k"], "v");
    }

    #[test]
    fn create_edge_and_get_connected_nodes() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage);
        let a = graph.create_node(NodeType::Concept, "A", None).unwrap();
        let b = graph.create_node(NodeType::Concept, "B", None).unwrap();
        graph
            .create_edge(&a.id, &b.id, Relationship::References, 0.8, None)
            .unwrap();
        let connected = graph.get_connected_nodes(&a.id, None).unwrap();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].0.id, b.id);
    }

    #[test]
    fn search_nodes_by_title() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage);
        graph
            .create_node(NodeType::File, "src/main.rs", None)
            .unwrap();
        graph
            .create_node(NodeType::File, "src/lib.rs", None)
            .unwrap();
        graph
            .create_node(NodeType::Concept, "something else", None)
            .unwrap();
        let results = graph
            .search_nodes(Some(NodeType::File), Some("main"))
            .unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].title, "src/main.rs");
    }

    #[test]
    fn create_edge_if_not_exists_deduplicates() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage);
        let a = graph.create_node(NodeType::Concept, "A", None).unwrap();
        let b = graph.create_node(NodeType::Concept, "B", None).unwrap();
        let e1 = graph
            .create_edge_if_not_exists(&a.id, &b.id, Relationship::RelatesTo, 0.5)
            .unwrap();
        let e2 = graph
            .create_edge_if_not_exists(&a.id, &b.id, Relationship::RelatesTo, 0.5)
            .unwrap();
        assert!(e1.is_some(), "first call should create an edge");
        assert!(e2.is_none(), "duplicate call should return None");
        let connected = graph.get_connected_nodes(&a.id, None).unwrap();
        assert_eq!(connected.len(), 1, "only one edge should exist");
    }

    #[test]
    fn delete_node_cascades_to_edges() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage);
        let a = graph.create_node(NodeType::Concept, "A", None).unwrap();
        let b = graph.create_node(NodeType::Concept, "B", None).unwrap();
        graph
            .create_edge(&a.id, &b.id, Relationship::References, 1.0, None)
            .unwrap();
        graph.delete_node(&a.id).unwrap();
        let connected = graph.get_connected_nodes(&b.id, None).unwrap();
        assert!(
            connected.is_empty(),
            "edges should cascade-delete with the node"
        );
    }
}
