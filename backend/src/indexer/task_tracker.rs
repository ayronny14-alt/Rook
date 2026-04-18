use anyhow::Result;
use tracing::{debug, info};

use crate::memory::graph::{GraphMemory, NodeType, Relationship};
use crate::memory::object::ObjectMemoryStore;
use crate::memory::storage::MemoryStorage;

#[derive(Debug, Clone)]
pub struct TaskInfo {
    pub id: String,
    pub title: String,
    pub description: String,
    pub project_id: Option<String>,
    pub status: String,
    pub metadata: Option<serde_json::Value>,
}

pub struct TaskTracker {
    memory: MemoryStorage,
}

impl TaskTracker {
    pub fn new(memory: MemoryStorage) -> Self {
        Self { memory }
    }

    pub fn create_project(&self, name: &str, description: &str) -> Result<String> {
        let graph = GraphMemory::new(self.memory.clone());
        let metadata = serde_json::json!({
            "description": description,
        });

        let node = graph.create_node(NodeType::Project, name, Some(metadata))?;
        info!("Created project: {} -> node {}", name, node.id);
        Ok(node.id)
    }

    pub fn create_task(
        &self,
        title: &str,
        description: &str,
        project_id: Option<&str>,
    ) -> Result<String> {
        let graph = GraphMemory::new(self.memory.clone());
        let obj_store = ObjectMemoryStore::new(self.memory.clone());

        let metadata = serde_json::json!({
            "description": description,
            "status": "open",
        });

        let node = graph.create_node(NodeType::Task, title, Some(metadata))?;

        let mut obj = ObjectMemoryStore::create_for_node(&node.id);
        obj.summary = Some(description.to_string());
        obj_store.upsert(&node.id, &obj)?;

        if let Some(pid) = project_id {
            graph.create_edge(pid, &node.id, Relationship::Contains, 1.0, None)?;
        }

        info!("Created task: {} -> node {}", title, node.id);
        Ok(node.id)
    }

    pub fn update_task_status(&self, task_id: &str, status: &str) -> Result<()> {
        let graph = GraphMemory::new(self.memory.clone());
        let metadata = serde_json::json!({
            "status": status,
        });

        graph.update_node(task_id, None, Some(metadata))?;
        debug!("Updated task {} status to {}", task_id, status);
        Ok(())
    }

    pub fn get_tasks_for_project(&self, project_id: &str) -> Result<Vec<TaskInfo>> {
        let graph = GraphMemory::new(self.memory.clone());
        let connected = graph.get_connected_nodes(project_id, Some(Relationship::Contains))?;

        let tasks: Vec<TaskInfo> = connected
            .into_iter()
            .filter(|(node, _)| matches!(node.node_type, NodeType::Task))
            .map(|(node, _)| TaskInfo {
                id: node.id.clone(),
                title: node.title.clone(),
                description: node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                project_id: Some(project_id.to_string()),
                status: node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("status"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("open")
                    .to_string(),
                metadata: node.metadata,
            })
            .collect();

        Ok(tasks)
    }

    pub fn get_projects(&self) -> Result<Vec<TaskInfo>> {
        let graph = GraphMemory::new(self.memory.clone());
        let nodes = graph.search_nodes(Some(NodeType::Project), None)?;

        let projects: Vec<TaskInfo> = nodes
            .into_iter()
            .map(|node| TaskInfo {
                id: node.id.clone(),
                title: node.title.clone(),
                description: node
                    .metadata
                    .as_ref()
                    .and_then(|m| m.get("description"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string(),
                project_id: None,
                status: "active".to_string(),
                metadata: node.metadata,
            })
            .collect();

        Ok(projects)
    }
}
