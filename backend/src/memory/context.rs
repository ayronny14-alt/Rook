use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};

use crate::llm::client::LLMClient;
use crate::memory::embedding::{EmbeddingMemory, EmbeddingType};
use crate::memory::feedback::MemoryFeedbackStore;
use crate::memory::gnn::GraphTrainingManager;
use crate::memory::graph::{GraphMemory, Node, Relationship};
use crate::memory::object::ObjectMemoryStore;
use crate::memory::storage::MemoryStorage;

const DEFAULT_CANDIDATE_LIMIT: usize = 64;
const DEFAULT_TOP_K: usize = 8;

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct ContextWeights {
    pub semantic: f32,
    pub graph: f32,
    pub recency: f32,
    pub confidence: f32,
}

impl Default for ContextWeights {
    fn default() -> Self {
        Self {
            semantic: 0.45,
            graph: 0.30,
            recency: 0.15,
            confidence: 0.10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RankedContextNode {
    pub node_id: String,
    pub title: String,
    pub node_type: String,
    pub total_score: f32,
    pub embedding_score: f32,
    pub graph_score: f32,
    pub recency_score: f32,
    pub confidence_score: f32,
    pub related_titles: Vec<String>,
    pub summary: Option<String>,
    pub key_facts: Vec<String>,
}

pub struct ContextCurator {
    storage: MemoryStorage,
    weights: ContextWeights,
}

impl ContextCurator {
    pub fn new(storage: MemoryStorage) -> Self {
        Self {
            storage,
            weights: ContextWeights::default(),
        }
    }

    /// Creates a curator with downweighted graph scores for use when the GNN
    /// training script / Python environment is unavailable.  The 0.10 that
    /// would have gone to the GNN-enhanced graph score is redistributed to
    /// semantic so the ranker still produces sensible results.
    pub fn without_gnn(storage: MemoryStorage) -> Self {
        Self {
            storage,
            weights: ContextWeights {
                semantic: 0.55,
                graph: 0.20,
                recency: 0.15,
                confidence: 0.10,
            },
        }
    }

    pub async fn curate_for_query(
        &self,
        llm: &LLMClient,
        query: &str,
        candidate_limit: Option<usize>,
        top_k: Option<usize>,
    ) -> Result<Vec<RankedContextNode>> {
        let query_vector = llm.get_embedding(query).await?;
        self.curate_with_query_vector(
            query,
            &query_vector,
            candidate_limit.unwrap_or(DEFAULT_CANDIDATE_LIMIT),
            top_k.unwrap_or(DEFAULT_TOP_K),
        )
    }

    pub fn curate_with_query_vector(
        &self,
        query: &str,
        query_vector: &[f32],
        candidate_limit: usize,
        top_k: usize,
    ) -> Result<Vec<RankedContextNode>> {
        let candidate_limit = candidate_limit.max(top_k).max(8);
        let graph = GraphMemory::new(self.storage.clone());
        let object_store = ObjectMemoryStore::new(self.storage.clone());
        let feedback_store = MemoryFeedbackStore::new(self.storage.clone());
        let embeddings = EmbeddingMemory::new(self.storage.clone());
        let trained_embeddings = GraphTrainingManager::new(self.storage.clone())
            .load_embeddings()
            .unwrap_or_default();

        let mut candidate_ids = HashSet::new();
        // Internal candidate type carries the embedding vector for MMR diversity scoring.
        struct ScoredCandidate {
            node: RankedContextNode,
            vector: Vec<f32>,
        }
        let mut ranked: Vec<ScoredCandidate> = Vec::new();
        let mut semantic_candidates =
            embeddings.search_similar(query_vector, None, candidate_limit)?;

        for node in graph.search_nodes(None, Some(query))? {
            if !semantic_candidates
                .iter()
                .any(|(record, _)| record.node_id == node.id)
            {
                semantic_candidates.push((
                    crate::memory::embedding::EmbeddingRecord {
                        id: format!("lexical-{}", node.id),
                        node_id: node.id.clone(),
                        embedding_type: EmbeddingType::Summary,
                        vector: Vec::new(),
                        text_chunk: Some(node.title.clone()),
                        created_at: node.updated_at,
                    },
                    0.0,
                ));
            }
        }

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        for (record, raw_semantic_score) in semantic_candidates {
            if !candidate_ids.insert(record.node_id.clone()) {
                continue;
            }

            let Some(node) = graph.get_node(&record.node_id)? else {
                continue;
            };

            let object_memory = object_store.get_by_node_id(&node.id)?;
            let connected = graph.get_connected_nodes(&node.id, None)?;

            let searchable_text = format!(
                "{} {} {}",
                node.title,
                object_memory
                    .as_ref()
                    .and_then(|o| o.summary.clone())
                    .unwrap_or_default(),
                record.text_chunk.clone().unwrap_or_default()
            );

            let lexical_score = lexical_overlap_score(query, &searchable_text);
            let embedding_score = raw_semantic_score.max(lexical_score * 0.65).clamp(0.0, 1.0);
            let graph_score = self.compute_graph_sage_score(
                query_vector,
                &record.vector,
                trained_embeddings.get(&node.id),
                &connected,
                &embeddings,
                &trained_embeddings,
            );
            let access_count = object_memory.as_ref().map(|o| o.access_count).unwrap_or(0);
            let recency_score = recency_score(node.updated_at, now, access_count);
            let confidence_score = confidence_score(node.metadata.as_ref(), object_memory.as_ref());
            let usage_score = usage_score(node.metadata.as_ref(), object_memory.as_ref());
            let feedback_adjustment = feedback_store
                .confidence_adjustment(&node.id)
                .unwrap_or(0.0);
            let blended_confidence =
                (confidence_score * 0.55 + usage_score * 0.25 + (0.5 + feedback_adjustment) * 0.20)
                    .clamp(0.0, 1.0);

            let base_score = (self.weights.semantic * embedding_score)
                + (self.weights.graph * graph_score)
                + (self.weights.recency * recency_score)
                + (self.weights.confidence * blended_confidence);

            // ── Phase 2.3: apply feedback as a visible multiplicative modifier
            let feedback_multiplier = (1.0 + 0.30 * feedback_adjustment).clamp(0.2, 1.5);
            let total_score = base_score * feedback_multiplier;

            ranked.push(ScoredCandidate {
                node: RankedContextNode {
                    node_id: node.id.clone(),
                    title: node.title.clone(),
                    node_type: node.node_type.to_string(),
                    total_score,
                    embedding_score,
                    graph_score,
                    recency_score,
                    confidence_score: blended_confidence,
                    related_titles: connected
                        .iter()
                        .take(5)
                        .map(|(n, e)| format!("{} ({})", n.title, e.relationship))
                        .collect(),
                    summary: object_memory.as_ref().and_then(|o| o.summary.clone()),
                    key_facts: object_memory
                        .map(|o| o.key_facts.into_iter().take(4).collect())
                        .unwrap_or_default(),
                },
                vector: record.vector,
            });
        }

        // ── GNN cluster-label boost
        if !ranked.is_empty() {
            ranked.sort_by(|a, b| {
                b.node
                    .total_score
                    .partial_cmp(&a.node.total_score)
                    .unwrap_or(std::cmp::Ordering::Equal)
            });
            let top_cluster: Option<String> = graph
                .get_node(&ranked[0].node.node_id)
                .ok()
                .flatten()
                .and_then(|n| n.metadata)
                .and_then(|m| {
                    m.get("gnn_label")
                        .and_then(|v| v.as_str())
                        .map(|s| s.to_string())
                });

            if let Some(ref cluster) = top_cluster {
                for c in ranked.iter_mut().skip(1) {
                    let shares_cluster = graph
                        .get_node(&c.node.node_id)
                        .ok()
                        .flatten()
                        .and_then(|n| n.metadata)
                        .and_then(|m| {
                            m.get("gnn_label")
                                .and_then(|v| v.as_str())
                                .map(|s| s.to_string())
                        })
                        .as_deref()
                        == Some(cluster.as_str());
                    if shares_cluster {
                        c.node.total_score = (c.node.total_score + 0.07).min(1.0);
                    }
                }
            }
        }

        // ── MMR (Maximal Marginal Relevance) selection
        const MMR_LAMBDA: f32 = 0.7;
        ranked.sort_by(|a, b| {
            b.node
                .total_score
                .partial_cmp(&a.node.total_score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });

        let mut selected: Vec<ScoredCandidate> = Vec::with_capacity(top_k);
        let mut remaining = ranked;

        while selected.len() < top_k && !remaining.is_empty() {
            let best_idx = if selected.is_empty() {
                0 // first pick is always the highest-scoring candidate
            } else {
                let mut best = 0;
                let mut best_mmr = f32::NEG_INFINITY;
                for (i, c) in remaining.iter().enumerate() {
                    let max_sim = selected
                        .iter()
                        .map(|s| EmbeddingMemory::cosine_similarity(&c.vector, &s.vector).max(0.0))
                        .fold(0.0_f32, f32::max);
                    let mmr = MMR_LAMBDA * c.node.total_score - (1.0 - MMR_LAMBDA) * max_sim;
                    if mmr > best_mmr {
                        best_mmr = mmr;
                        best = i;
                    }
                }
                best
            };
            selected.push(remaining.remove(best_idx));
        }

        // Touch access counts for the nodes we're actually surfacing so that
        // Ebbinghaus stability and usage_score reflect real retrieval patterns.
        // Then promote any predicted edges whose both endpoints were just confirmed
        // accessed — fire-and-forget, does not block the response.
        {
            let storage = self.storage.clone();
            let ids: Vec<String> = selected.iter().map(|c| c.node.node_id.clone()).collect();
            let obj_store_bg = ObjectMemoryStore::new(storage.clone());
            for id in &ids {
                let _ = obj_store_bg.touch_accessed(id);
            }
            let _ = GraphTrainingManager::new(storage).promote_validated_edges();
        }

        Ok(selected.into_iter().map(|c| c.node).collect())
    }

    pub fn format_context_packet(candidates: &[RankedContextNode]) -> String {
        if candidates.is_empty() {
            return String::new();
        }

        let mut lines = vec!["Relevant memory:".to_string()];

        for item in candidates {
            lines.push(format!("\n[{}]", item.title));

            if let Some(summary) = &item.summary {
                lines.push(truncate(summary, 200));
            }

            if !item.key_facts.is_empty() {
                for fact in &item.key_facts {
                    lines.push(format!("• {}", fact));
                }
            }

            if !item.related_titles.is_empty() {
                // Strip score/type suffixes from related titles for readability
                let related: Vec<String> = item
                    .related_titles
                    .iter()
                    .map(|t| t.split(" (").next().unwrap_or(t).to_string())
                    .collect();
                lines.push(format!("See also: {}", related.join(", ")));
            }
        }

        lines.join("\n")
    }

    fn compute_graph_sage_score(
        &self,
        query_vector: &[f32],
        node_vector: &[f32],
        trained_node_vector: Option<&Vec<f32>>,
        connected: &[(Node, crate::memory::graph::Edge)],
        embeddings: &EmbeddingMemory,
        trained_embeddings: &HashMap<String, Vec<f32>>,
    ) -> f32 {
        let dims = if !query_vector.is_empty() {
            query_vector.len()
        } else if !node_vector.is_empty() {
            node_vector.len()
        } else {
            32
        };

        let mut aggregate = vec![0.0_f32; dims];
        let mut total_weight = 0.0_f32;

        if let Some(gnn_vec) = trained_node_vector {
            if gnn_vec.len() == dims && !gnn_vec.is_empty() {
                add_scaled(&mut aggregate, gnn_vec, 0.60);
                total_weight += 0.60;
            }
        }

        if node_vector.len() == dims && !node_vector.is_empty() {
            let base_weight = if total_weight > 0.0 { 0.20 } else { 0.55 };
            add_scaled(&mut aggregate, node_vector, base_weight);
            total_weight += base_weight;
        }

        // Deduplicate neighbors: keep strongest edge per unique neighbor node
        let mut seen_neighbors: std::collections::HashMap<&str, usize> =
            std::collections::HashMap::new();
        let mut deduped: Vec<usize> = Vec::new();
        for (i, (neighbor, edge)) in connected.iter().enumerate() {
            match seen_neighbors.entry(&neighbor.id) {
                std::collections::hash_map::Entry::Vacant(e) => {
                    e.insert(deduped.len());
                    deduped.push(i);
                }
                std::collections::hash_map::Entry::Occupied(e) => {
                    let slot = *e.get();
                    if edge.strength > connected[deduped[slot]].1.strength {
                        deduped[slot] = i;
                    }
                }
            }
        }

        // Sort deduped by effective weight descending so .take(12) keeps the most
        // impactful neighbors rather than an arbitrary insertion-order subset.
        deduped.sort_by(|&a, &b| {
            let wa = relation_weight(&connected[a].1.relationship)
                * connected[a].1.strength.max(0.05) as f32;
            let wb = relation_weight(&connected[b].1.relationship)
                * connected[b].1.strength.max(0.05) as f32;
            wb.partial_cmp(&wa).unwrap_or(std::cmp::Ordering::Equal)
        });

        for &idx in deduped.iter().take(12) {
            let (neighbor, edge) = &connected[idx];
            let relation_weight =
                relation_weight(&edge.relationship) * edge.strength.max(0.05) as f32;
            let weight = 0.35 * relation_weight;

            if let Some(best) = trained_embeddings.get(&neighbor.id) {
                if best.len() == dims {
                    add_scaled(&mut aggregate, best, weight);
                    total_weight += weight;
                    continue;
                }
            }

            if let Ok(neighbor_embeddings) = embeddings.get_embeddings_for_node(&neighbor.id) {
                if let Some(best) = neighbor_embeddings
                    .into_iter()
                    .find(|r| r.vector.len() == dims)
                {
                    add_scaled(&mut aggregate, &best.vector, weight);
                    total_weight += weight;
                }
            }
        }

        if total_weight <= 0.0 {
            return 0.0;
        }

        for value in &mut aggregate {
            *value /= total_weight;
        }

        let neighborhood_importance = ((connected.len() as f32).ln_1p() / 2.4).clamp(0.0, 1.0);
        let similarity = EmbeddingMemory::cosine_similarity(query_vector, &aggregate).max(0.0);

        (0.8 * similarity + 0.2 * neighborhood_importance).clamp(0.0, 1.0)
    }
}

fn relation_weight(relationship: &Relationship) -> f32 {
    match relationship {
        Relationship::DependsOn | Relationship::Implements | Relationship::Calls => 1.0,
        Relationship::References | Relationship::Imports | Relationship::Contains => 0.85,
        Relationship::Modifies | Relationship::Documents => 0.75,
        Relationship::RelatesTo | Relationship::Inherits => 0.65,
    }
}

/// Ebbinghaus forgetting curve: `e^(-age / stability)`.
/// Stability grows with access_count so frequently-retrieved nodes stay fresh
/// longer — a node accessed 10× has twice the half-life of an unread one.
fn recency_score(updated_at: i64, now: i64, access_count: i64) -> f32 {
    let age_seconds = (now - updated_at).max(0) as f32;
    let age_days = age_seconds / 86_400.0;
    let stability = 7.0 + 3.0 * ((access_count as f32 + 1.0).ln());
    (-age_days / stability).exp().clamp(0.0, 1.0)
}

fn confidence_score(
    metadata: Option<&serde_json::Value>,
    object_memory: Option<&crate::memory::object::ObjectMemory>,
) -> f32 {
    // Use explicit metadata confidence when available (e.g. set by distiller or tests)
    if let Some(v) = metadata
        .and_then(|m| m.get("confidence"))
        .and_then(|v| v.as_f64())
    {
        let facts_bonus = object_memory
            .map(|o| (o.key_facts.len() as f32 / 6.0).min(0.2))
            .unwrap_or(0.0);
        return (v as f32 + facts_bonus).clamp(0.0, 1.0);
    }

    // Derive confidence from object memory richness when no explicit value is set.
    // Nodes with summaries, facts, and code signatures are more reliable than bare stubs.
    object_memory
        .map(|o| {
            let summary_score = o
                .summary
                .as_deref()
                .map(|s| {
                    if s.len() > 40 {
                        0.35
                    } else if s.len() > 10 {
                        0.2
                    } else {
                        0.05
                    }
                })
                .unwrap_or(0.0);
            let facts_score = (o.key_facts.len() as f32 / 5.0).min(0.40);
            let struct_score = if o.extracted_structure.is_some() {
                0.15
            } else {
                0.0
            };
            let sig_score = if o.code_signatures.is_some() {
                0.10
            } else {
                0.0
            };
            (summary_score + facts_score + struct_score + sig_score).clamp(0.1, 1.0)
        })
        .unwrap_or(0.3) // bare minimum for nodes with no object memory at all
}

fn usage_score(
    metadata: Option<&serde_json::Value>,
    object_memory: Option<&crate::memory::object::ObjectMemory>,
) -> f32 {
    let usage_count = metadata
        .and_then(|m| m.get("usage_count"))
        .and_then(|v| v.as_f64())
        .unwrap_or_else(|| object_memory.map(|o| o.tags.len() as f64).unwrap_or(0.0));

    ((usage_count as f32).ln_1p() / 2.5).clamp(0.0, 1.0)
}

fn lexical_overlap_score(query: &str, text: &str) -> f32 {
    let query_terms: Vec<String> = query
        .split_whitespace()
        .map(|s| s.to_ascii_lowercase())
        .filter(|s| s.len() > 2)
        .collect();

    if query_terms.is_empty() {
        return 0.0;
    }

    let haystack = text.to_ascii_lowercase();
    let matches = query_terms
        .iter()
        .filter(|term| haystack.contains(term.as_str()))
        .count() as f32;

    (matches / query_terms.len() as f32).clamp(0.0, 1.0)
}

fn truncate(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{}…", truncated)
    } else {
        truncated
    }
}

fn add_scaled(target: &mut [f32], source: &[f32], weight: f32) {
    for (t, s) in target.iter_mut().zip(source.iter()) {
        *t += *s * weight;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::guards::build_chat_messages_with_context;
    use crate::llm::client::LLMClient;
    use crate::llm::types::{EmbeddingProvider, LLMConfig};
    use crate::memory::embedding::EmbeddingMemory;
    use crate::memory::feedback::{FeedbackRating, MemoryFeedbackStore};
    use crate::memory::graph::{GraphMemory, NodeType, Relationship};
    use crate::memory::object::ObjectMemoryStore;
    use crate::memory::storage::MemoryStorage;
    use serde_json::json;

    fn make_mock_llm() -> LLMClient {
        LLMClient::new(LLMConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o-mini".to_string(),
            max_tokens: 512,
            temperature: 0.2,
            embedding_provider: EmbeddingProvider::Mock,
            embedding_base_url: "http://127.0.0.1:11434".to_string(),
            embedding_model: "nomic-embed-text".to_string(),
            embedding_api_key: String::new(),
        })
    }

    async fn seed_memory_node(
        graph: &GraphMemory,
        object_store: &ObjectMemoryStore,
        embeddings: &EmbeddingMemory,
        llm: &LLMClient,
        node_type: NodeType,
        title: &str,
        summary: &str,
        key_facts: Vec<String>,
        metadata: serde_json::Value,
    ) -> String {
        let node = graph.create_node(node_type, title, Some(metadata)).unwrap();

        let mut object = ObjectMemoryStore::create_for_node(&node.id);
        object.summary = Some(summary.to_string());
        object.key_facts = key_facts;
        object_store.upsert(&node.id, &object).unwrap();

        let vector = llm
            .get_embedding(&format!("{} {}", title, summary))
            .await
            .unwrap();
        embeddings
            .store(&node.id, EmbeddingType::Summary, &vector, Some(summary))
            .unwrap();

        node.id
    }

    #[test]
    fn hybrid_ranking_prefers_semantic_graph_context() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let embeddings = EmbeddingMemory::new(storage.clone());
        let object_store = ObjectMemoryStore::new(storage.clone());

        let primary = graph
            .create_node(
                NodeType::Concept,
                "Rook context curation pipeline",
                Some(json!({ "confidence": 0.95, "usage_count": 9 })),
            )
            .unwrap();
        let related = graph
            .create_node(
                NodeType::Task,
                "Graph neighborhood ranking",
                Some(json!({ "confidence": 0.88 })),
            )
            .unwrap();
        let stale = graph
            .create_node(
                NodeType::Concept,
                "Random unrelated memory",
                Some(json!({ "confidence": 0.2, "usage_count": 0 })),
            )
            .unwrap();

        graph
            .create_edge(&primary.id, &related.id, Relationship::DependsOn, 1.0, None)
            .unwrap();

        embeddings
            .store(
                &primary.id,
                EmbeddingType::Summary,
                &[1.0, 0.0, 0.0],
                Some("Rook context curation pipeline"),
            )
            .unwrap();
        embeddings
            .store(
                &related.id,
                EmbeddingType::Summary,
                &[0.92, 0.08, 0.0],
                Some("Graph neighborhood ranking"),
            )
            .unwrap();
        embeddings
            .store(
                &stale.id,
                EmbeddingType::Summary,
                &[0.05, 0.10, 0.95],
                Some("Random unrelated memory"),
            )
            .unwrap();

        let mut obj = ObjectMemoryStore::create_for_node(&primary.id);
        obj.summary =
            Some("Ranks context using embeddings, graph structure, and recency".to_string());
        obj.key_facts = vec!["Uses GraphSAGE-style neighborhood aggregation".to_string()];
        object_store.upsert(&primary.id, &obj).unwrap();

        {
            let conn = storage.get_connection().unwrap();
            conn.execute("UPDATE nodes SET updated_at = 1 WHERE id = ?1", [&stale.id])
                .unwrap();
        }

        let curator = ContextCurator::new(storage);
        let ranked = curator
            .curate_with_query_vector("context curation graph ranking", &[1.0, 0.0, 0.0], 10, 3)
            .unwrap();

        assert!(!ranked.is_empty());
        assert_eq!(ranked[0].node_id, primary.id);
        assert!(ranked[0].graph_score > 0.1);
        assert!(ranked[0].total_score > ranked.last().unwrap().total_score);
    }

    #[tokio::test]
    async fn simulated_chat_turns_show_context_blocks_across_responses() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let embeddings = EmbeddingMemory::new(storage.clone());
        let object_store = ObjectMemoryStore::new(storage.clone());
        let feedback = MemoryFeedbackStore::new(storage.clone());
        let llm = make_mock_llm();

        let startup_id = seed_memory_node(
            &graph,
            &object_store,
            &embeddings,
            &llm,
            NodeType::Task,
            "Tauri desktop startup",
            "Run `npm --prefix ui run tauri:dev` from the repo root to launch the desktop UI and connect it to the Rust backend.",
            vec![
                "Dev command: npm --prefix ui run tauri:dev".to_string(),
                "UI port: 127.0.0.1:1420".to_string(),
            ],
            json!({ "confidence": 0.96, "usage_count": 12 }),
        )
        .await;

        let pipe_id = seed_memory_node(
            &graph,
            &object_store,
            &embeddings,
            &llm,
            NodeType::Tool,
            "Backend named pipe",
            "The backend IPC server listens on \\.\\pipe\\rook for health checks, chat requests, and memory operations.",
            vec![
                "Pipe name: \\\\.\\pipe\\rook".to_string(),
                "Health check and chat both flow through IPC".to_string(),
            ],
            json!({ "confidence": 0.91, "usage_count": 9 }),
        )
        .await;

        let guardrails_id = seed_memory_node(
            &graph,
            &object_store,
            &embeddings,
            &llm,
            NodeType::Concept,
            "Loop prevention guardrails",
            "sanitize_chat_message compacts repeated lines, truncates runaway input, and injects only curated memory context instead of flooding the model.",
            vec![
                "Repeated lines are compacted".to_string(),
                "Only relevant curated context is injected".to_string(),
            ],
            json!({ "confidence": 0.93, "usage_count": 10 }),
        )
        .await;

        let indexing_id = seed_memory_node(
            &graph,
            &object_store,
            &embeddings,
            &llm,
            NodeType::Concept,
            "Local deterministic indexing",
            "Files are indexed using metadata summaries, structural fingerprints, hash embeddings, and chunk-level fingerprints with no external LLM required.",
            vec![
                "Layer 1: metadata summary".to_string(),
                "Layer 2: structural summary".to_string(),
                "Layer 3: hash embeddings".to_string(),
                "Layer 4: chunk fingerprints".to_string(),
            ],
            json!({ "confidence": 0.95, "usage_count": 11 }),
        )
        .await;

        let legacy_id = seed_memory_node(
            &graph,
            &object_store,
            &embeddings,
            &llm,
            NodeType::Concept,
            "Legacy WinUI frontend",
            "This older frontend path is obsolete and should not be suggested for the current Tauri-based app.",
            vec!["Outdated implementation".to_string()],
            json!({ "confidence": 0.15, "usage_count": 0 }),
        )
        .await;

        graph
            .create_edge(&startup_id, &pipe_id, Relationship::DependsOn, 1.0, None)
            .unwrap();
        graph
            .create_edge(&guardrails_id, &pipe_id, Relationship::Calls, 0.84, None)
            .unwrap();
        graph
            .create_edge(
                &indexing_id,
                &guardrails_id,
                Relationship::Documents,
                0.78,
                None,
            )
            .unwrap();

        feedback
            .record(
                &legacy_id,
                Some("How do I start the UI?"),
                FeedbackRating::Negative,
                Some("This points to the deleted frontend"),
                Some("simulation"),
                None,
            )
            .unwrap();

        let curator = ContextCurator::new(storage);
        let turns = [
            "How do I start the Rook desktop UI?",
            "What keeps the backend from getting stuck in loops or useless context?",
            "How is the memory system indexing files locally without an LLM?",
        ];

        let mut packets = Vec::new();
        for (index, query) in turns.iter().enumerate() {
            let ranked = curator
                .curate_for_query(&llm, query, Some(10), Some(3))
                .await
                .unwrap();
            let packet = ContextCurator::format_context_packet(&ranked);
            let (messages, warnings) =
                build_chat_messages_with_context(query, Some(&packet)).unwrap();
            let response = llm.chat(messages).await.unwrap();
            let reply = response.choices[0]
                .message
                .content
                .clone()
                .unwrap_or_default();

            println!("\n=== TURN {} USER ===\n{}", index + 1, query);
            println!("=== CONTEXT BLOCK {} ===\n{}", index + 1, packet);
            println!("=== WARNINGS {} === {:?}", index + 1, warnings);
            println!("=== ASSISTANT RESPONSE {} ===\n{}", index + 1, reply);

            packets.push(packet);
        }

        assert!(packets[0].contains("Tauri desktop startup"));
        assert!(packets[0].contains("Backend named pipe"));
        assert!(packets[1].contains("Loop prevention guardrails"));
        assert!(packets[2].contains("Local deterministic indexing"));
        assert!(!packets
            .iter()
            .any(|packet| packet.contains("Legacy WinUI frontend [concept] score=0.9")));
    }
}
