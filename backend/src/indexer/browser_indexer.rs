use anyhow::Result;
use tracing::info;

use crate::memory::embedding::{EmbeddingMemory, EmbeddingType};
use crate::memory::graph::{GraphMemory, NodeType};
use crate::memory::object::ObjectMemoryStore;
use crate::memory::storage::MemoryStorage;

const EMBEDDING_DIMS: usize = 32;

pub struct BrowserIndexer {
    memory: MemoryStorage,
}

impl BrowserIndexer {
    pub fn new(memory: MemoryStorage) -> Self {
        Self { memory }
    }

    pub async fn index_url(&self, url: &str, title: &str, content: &str) -> Result<()> {
        let graph = GraphMemory::new(self.memory.clone());
        let obj_store = ObjectMemoryStore::new(self.memory.clone());
        let emb_store = EmbeddingMemory::new(self.memory.clone());

        let metadata = serde_json::json!({ "url": url });

        // Upsert: reuse an existing Website node with the same title if present.
        let node = match graph
            .search_nodes(Some(NodeType::Website), Some(title))?
            .into_iter()
            .find(|n| {
                n.metadata
                    .as_ref()
                    .and_then(|m| m.get("url"))
                    .and_then(|v| v.as_str())
                    == Some(url)
            }) {
            Some(existing) => existing,
            None => graph.create_node(NodeType::Website, title, Some(metadata))?,
        };

        let summary = content
            .lines()
            .filter(|l| !l.trim().is_empty())
            .take(12)
            .collect::<Vec<_>>()
            .join(" ");

        let mut obj = ObjectMemoryStore::create_for_node(&node.id);
        obj.summary = Some(summary.clone());
        obj.extracted_structure = Some(format!("URL: {}\nTitle: {}", url, title));
        obj.tags = vec!["web".to_string(), "browser".to_string()];
        obj_store.upsert(&node.id, &obj)?;

        // Store hash-based embeddings so this node surfaces in semantic search.
        emb_store.delete_embeddings_for_node(&node.id)?;

        let summary_text = format!("{} {} {}", url, title, summary);
        let summary_vec = hash_embedding(&summary_text, EMBEDDING_DIMS);
        emb_store.store(
            &node.id,
            EmbeddingType::Summary,
            &summary_vec,
            Some(&summary_text),
        )?;

        if !content.is_empty() {
            let content_excerpt: String = content.chars().take(1200).collect();
            let content_vec = hash_embedding(&content_excerpt, EMBEDDING_DIMS);
            emb_store.store(
                &node.id,
                EmbeddingType::Content,
                &content_vec,
                Some(&content_excerpt),
            )?;
        }

        info!("Indexed URL: {} -> node {}", url, node.id);
        Ok(())
    }

    pub async fn search_websites(&self, query: &str) -> Result<Vec<serde_json::Value>> {
        let graph = GraphMemory::new(self.memory.clone());
        let nodes = graph.search_nodes(Some(NodeType::Website), Some(query))?;

        let results: Vec<serde_json::Value> = nodes
            .into_iter()
            .filter_map(|n| serde_json::to_value(&n).ok())
            .collect();

        Ok(results)
    }
}

/// Deterministic L2-normalised hash embedding — the same algorithm used by
/// `FileIndexer` so all indexed content shares a compatible vector space.
fn hash_embedding(text: &str, dims: usize) -> Vec<f32> {
    let mut values = vec![0.0_f32; dims];
    for (i, byte) in text.bytes().enumerate() {
        values[i % dims] += (byte as f32) / 255.0;
    }
    let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
    if norm > 0.0 {
        for v in &mut values {
            *v /= norm;
        }
    }
    values
}
