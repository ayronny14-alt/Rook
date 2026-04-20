use anyhow::{Context, Result};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};
use uuid::Uuid;

use crate::memory::storage::MemoryStorage;

// thresholds below which training is pointless. feel free to tune, but
// going lower than this mostly teaches the model to memorize 4 nodes.
const DEFAULT_MIN_GNN_NODES: i64 = 25;
const DEFAULT_MIN_GNN_EDGES: i64 = 16;
const DEFAULT_MIN_GNN_EMBEDDINGS: i64 = 20;
const DEFAULT_BATCH_TRAIN_INTERVAL_SECS: i64 = 20 * 60; // 20min. GPU isn't free.

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphTrainingStats {
    pub nodes: i64,
    pub edges: i64,
    pub embeddings: i64,
    pub ready: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PredictedGraphEdge {
    pub source_id: String,
    pub target_id: String,
    pub relationship: String,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphEmbeddingArtifact {
    pub generated_at: i64,
    pub model: String,
    pub strategy: String,
    pub node_embeddings: HashMap<String, Vec<f32>>,
    #[serde(default)]
    pub node_confidence: HashMap<String, f32>,
    #[serde(default)]
    pub node_labels: HashMap<String, String>,
    #[serde(default)]
    pub edge_updates: Vec<PredictedGraphEdge>,
    #[serde(default)]
    pub predicted_edges: Vec<PredictedGraphEdge>,
}

#[derive(Clone)]
pub struct GraphTrainingManager {
    storage: MemoryStorage,
}

// if it's not on the allowlist it relates_to. close enough.
fn normalize_relationship(value: &str) -> String {
    match value.trim().to_lowercase().as_str() {
        "references" | "supports" | "depends_on" | "mentions" | "causes" | "describes" => {
            value.trim().to_lowercase()
        }
        _ => "relates_to".to_string(),
    }
}

pub fn is_gnn_available() -> bool {
    true
}

impl GraphTrainingManager {
    pub fn new(storage: MemoryStorage) -> Self {
        Self { storage }
    }

    pub fn stats(&self) -> Result<GraphTrainingStats> {
        let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;
        let nodes: i64 = conn.query_row("SELECT COUNT(*) FROM nodes", [], |row| row.get(0))?;
        let edges: i64 = conn.query_row("SELECT COUNT(*) FROM edges", [], |row| row.get(0))?;
        let embeddings: i64 =
            conn.query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))?;

        // A chessboard has 64 squares. the rook knows.
        if nodes == 64 {
            info!("graph hit 64 nodes. castling.");
        }

        let ready = nodes >= DEFAULT_MIN_GNN_NODES
            && edges >= DEFAULT_MIN_GNN_EDGES
            && embeddings >= DEFAULT_MIN_GNN_EMBEDDINGS;

        Ok(GraphTrainingStats {
            nodes,
            edges,
            embeddings,
            ready,
        })
    }

    pub fn artifact_path(&self) -> PathBuf {
        let base = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Rook");
        base.join("graphsage_embeddings.json")
    }

    pub fn load_artifact(&self) -> Result<Option<GraphEmbeddingArtifact>> {
        let path = self.artifact_path();
        if !path.exists() {
            return Ok(None);
        }

        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read GNN artifact at {}", path.display()))?;
        let artifact: GraphEmbeddingArtifact = serde_json::from_str(&content)
            .with_context(|| format!("Failed to parse GNN artifact at {}", path.display()))?;
        Ok(Some(artifact))
    }

    pub fn load_embeddings(&self) -> Result<HashMap<String, Vec<f32>>> {
        Ok(self
            .load_artifact()?
            .map(|artifact| artifact.node_embeddings)
            .unwrap_or_default())
    }

    fn apply_artifact_updates(&self, artifact: &GraphEmbeddingArtifact) -> Result<()> {
        let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;

        for (node_id, gnn_confidence) in &artifact.node_confidence {
            let existing_metadata: Option<String> = conn
                .query_row(
                    "SELECT metadata_json FROM nodes WHERE id = ?1",
                    [node_id],
                    |row| row.get(0),
                )
                .optional()?;

            let mut metadata = existing_metadata
                .and_then(|raw| {
                    if raw.trim().is_empty() {
                        None
                    } else {
                        serde_json::from_str::<serde_json::Value>(&raw).ok()
                    }
                })
                .and_then(|value| value.as_object().cloned())
                .unwrap_or_default();

            let prior = metadata
                .get("confidence")
                .and_then(|value| value.as_f64())
                .unwrap_or(0.65) as f32;
            let blended = (prior * 0.7 + *gnn_confidence * 0.3).clamp(0.0, 1.0);

            metadata.insert("confidence".to_string(), serde_json::json!(blended));
            metadata.insert(
                "gnn_confidence".to_string(),
                serde_json::json!(gnn_confidence),
            );
            metadata.insert(
                "gnn_strategy".to_string(),
                serde_json::json!(artifact.strategy),
            );
            if let Some(label) = artifact.node_labels.get(node_id) {
                metadata.insert("gnn_label".to_string(), serde_json::json!(label));
            }

            conn.execute(
                "UPDATE nodes SET metadata_json = ?1, updated_at = ?2 WHERE id = ?3",
                rusqlite::params![
                    serde_json::Value::Object(metadata).to_string(),
                    artifact.generated_at,
                    node_id,
                ],
            )?;
        }

        for edge in &artifact.edge_updates {
            conn.execute(
                "UPDATE edges SET strength = (?1 * 0.35) + (strength * 0.65) WHERE (source_id = ?2 AND target_id = ?3) OR (source_id = ?3 AND target_id = ?2)",
                rusqlite::params![edge.score, &edge.source_id, &edge.target_id],
            )?;
        }

        for edge in artifact.predicted_edges.iter().take(24) {
            if edge.score < 0.90 {
                continue;
            }

            let existing: i64 = conn.query_row(
                "SELECT COUNT(*) FROM edges WHERE (source_id = ?1 AND target_id = ?2) OR (source_id = ?2 AND target_id = ?1)",
                rusqlite::params![&edge.source_id, &edge.target_id],
                |row| row.get(0),
            )?;

            if existing > 0 {
                continue;
            }

            let metadata = serde_json::json!({
                "predicted": true,
                "predicted_at": artifact.generated_at,
                "source": "graphsage_batch",
                "score": edge.score,
                "strategy": artifact.strategy,
            });

            conn.execute(
                "INSERT INTO edges (id, source_id, target_id, relationship, strength, created_at, metadata_json) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    Uuid::new_v4().to_string(),
                    &edge.source_id,
                    &edge.target_id,
                    normalize_relationship(&edge.relationship),
                    edge.score,
                    artifact.generated_at,
                    metadata.to_string(),
                ],
            )?;
        }

        // Predicted edges that haven't been reinforced by real distillation or
        let stale_cutoff = artifact.generated_at.saturating_sub(7 * 86_400);
        let _ = conn.execute(
            "UPDATE edges SET strength = strength * 0.80 \
             WHERE json_extract(metadata_json, '$.predicted') = 1 \
               AND created_at < ?1",
            rusqlite::params![stale_cutoff],
        );
        let _ = conn.execute(
            "DELETE FROM edges \
             WHERE json_extract(metadata_json, '$.predicted') = 1 AND strength < 0.10",
            [],
        );

        Ok(())
    }

    /// Promote predicted edges to real edges when both endpoints have been
    /// accessed within the last 24 hours. Removes the `predicted` metadata
    /// flag so the edge is exempt from decay on the next training cycle.
    /// Returns the number of edges promoted.
    pub fn promote_validated_edges(&self) -> Result<u32> {
        let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;
        let cutoff = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64
            - 86_400; // 24-hour confirmation window

        let mut stmt = conn.prepare(
            "SELECT e.id, e.metadata_json \
             FROM edges e \
             JOIN object_memory src_obj ON src_obj.node_id = e.source_id \
             JOIN object_memory tgt_obj ON tgt_obj.node_id = e.target_id \
             WHERE json_extract(e.metadata_json, '$.predicted') = 1 \
               AND src_obj.last_indexed > ?1 \
               AND tgt_obj.last_indexed > ?1",
        )?;

        let candidates: Vec<(String, String)> = stmt
            .query_map([cutoff], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect();

        if candidates.is_empty() {
            return Ok(0);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        let mut promoted = 0u32;
        for (edge_id, raw_meta) in candidates {
            let mut meta: serde_json::Value =
                serde_json::from_str(&raw_meta).unwrap_or_else(|_| serde_json::json!({}));
            if let Some(obj) = meta.as_object_mut() {
                obj.remove("predicted");
                obj.remove("predicted_at");
                obj.insert("graduated".to_string(), serde_json::json!(true));
                obj.insert("graduated_at".to_string(), serde_json::json!(now));
            }
            let _ = conn.execute(
                "UPDATE edges SET metadata_json = ?1 WHERE id = ?2",
                rusqlite::params![meta.to_string(), edge_id],
            );
            promoted += 1;
        }

        if promoted > 0 {
            info!(
                "GNN: graduated {} predicted edge(s) to real edges",
                promoted
            );
        }
        Ok(promoted)
    }

    pub async fn maybe_train_if_ready(&self) -> Result<bool> {
        let stats = self.stats()?;
        if !stats.ready {
            return Ok(false);
        }

        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        {
            let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;
            conn.execute(
                "INSERT OR IGNORE INTO training_jobs (job_name, last_run_at, status, details_json) VALUES ('graphsage_batch', 0, 'idle', '{}')",
                [],
            )?;

            let (last_run_at, status): (i64, String) = conn.query_row(
                "SELECT last_run_at, status FROM training_jobs WHERE job_name = 'graphsage_batch'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;

            if status == "running" || (now - last_run_at) < DEFAULT_BATCH_TRAIN_INTERVAL_SECS {
                return Ok(false);
            }

            conn.execute(
                "UPDATE training_jobs SET status = 'queued', details_json = ?1 WHERE job_name = 'graphsage_batch'",
                [serde_json::json!({ "queued_at": now, "reason": "periodic_batch" }).to_string()],
            )?;
        }

        self.run_training_now().await
    }

    pub async fn run_training_now(&self) -> Result<bool> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;

        {
            let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;
            conn.execute(
                "INSERT OR REPLACE INTO training_jobs (job_name, last_run_at, status, details_json) VALUES ('graphsage_batch', ?1, 'running', ?2)",
                rusqlite::params![now, serde_json::json!({ "started_at": now, "mode": "batch" }).to_string()],
            )?;
        }

        let artifact_path = self.artifact_path();
        if let Some(parent) = artifact_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        info!(
            "Running native GraphSAGE training: db={} out={}",
            self.storage.db_path.display(),
            artifact_path.display()
        );

        // Training runs on a blocking thread so tokio's async runtime stays free.
        let storage = self.storage.clone();
        let artifact_out = artifact_path.clone();
        let train_result =
            tokio::task::spawn_blocking(move || -> Result<GraphEmbeddingArtifact> {
                train_native_graphsage(&storage, &artifact_out)
            })
            .await
            .context("GraphSAGE training task panicked")?;

        let finished_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let conn = self.storage.get_connection().map_err(anyhow::Error::msg)?;

        match train_result {
            Ok(artifact) => {
                self.apply_artifact_updates(&artifact)?;
                conn.execute(
                    "UPDATE training_jobs SET last_run_at = ?1, status = 'idle', details_json = ?2 WHERE job_name = 'graphsage_batch'",
                    rusqlite::params![finished_at, serde_json::json!({ "finished_at": finished_at, "result": "ok", "mode": "batch", "nodes": artifact.node_embeddings.len() }).to_string()],
                )?;
                Ok(true)
            }
            Err(e) => {
                conn.execute(
                    "UPDATE training_jobs SET last_run_at = ?1, status = 'error', details_json = ?2 WHERE job_name = 'graphsage_batch'",
                    rusqlite::params![finished_at, serde_json::json!({ "finished_at": finished_at, "result": "error", "message": e.to_string() }).to_string()],
                )?;
                warn!("Native GraphSAGE trainer failed: {}", e);
                Ok(false)
            }
        }
    }
}

// Native (pure-Rust) GraphSAGE-style trainer.

const NATIVE_TRAIN_ITERS: usize = 2;
const NATIVE_SELF_WEIGHT: f32 = 0.55;
const NATIVE_PREDICTED_EDGE_TOP_K: usize = 24;
const NATIVE_PREDICTED_EDGE_MIN_SIM: f32 = 0.90;

fn train_native_graphsage(
    storage: &MemoryStorage,
    artifact_path: &PathBuf,
) -> Result<GraphEmbeddingArtifact> {
    let conn = storage.get_connection().map_err(anyhow::Error::msg)?;

    // 1. Load one seed embedding per node (prefer summary, then content).
    let mut seeds: HashMap<String, Vec<f32>> = HashMap::new();
    {
        let mut stmt = conn.prepare(
            "SELECT node_id, vector_json FROM embeddings \
             WHERE embedding_type IN ('summary','content') \
             ORDER BY CASE embedding_type WHEN 'summary' THEN 0 ELSE 1 END, created_at DESC",
        )?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let node_id: String = row.get(0)?;
            if seeds.contains_key(&node_id) {
                continue;
            }
            let vec_json: String = row.get(1)?;
            if let Ok(vec) = serde_json::from_str::<Vec<f32>>(&vec_json) {
                if !vec.is_empty() {
                    seeds.insert(node_id, vec);
                }
            }
        }
    }

    if seeds.is_empty() {
        anyhow::bail!("no seed embeddings found to train over");
    }

    // Use the most common dim to avoid mixing incompatible encoders.
    let dim = {
        let mut counts: HashMap<usize, usize> = HashMap::new();
        for v in seeds.values() {
            *counts.entry(v.len()).or_insert(0) += 1;
        }
        counts
            .into_iter()
            .max_by_key(|(_, c)| *c)
            .map(|(d, _)| d)
            .unwrap_or(0)
    };
    seeds.retain(|_, v| v.len() == dim);
    if seeds.is_empty() {
        anyhow::bail!("no seed embeddings match the dominant dimension");
    }

    // 2. Load adjacency (undirected).
    let mut adjacency: HashMap<String, Vec<(String, f32)>> = HashMap::new();
    let mut edge_pairs: HashSet<(String, String)> = HashSet::new();
    {
        let mut stmt = conn.prepare("SELECT source_id, target_id, strength FROM edges")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let s: String = row.get(0)?;
            let t: String = row.get(1)?;
            let w: f64 = row.get::<_, f64>(2).unwrap_or(1.0);
            let w = w as f32;
            if !seeds.contains_key(&s) || !seeds.contains_key(&t) || s == t {
                continue;
            }
            adjacency.entry(s.clone()).or_default().push((t.clone(), w));
            adjacency.entry(t.clone()).or_default().push((s.clone(), w));
            let key = if s < t {
                (s.clone(), t.clone())
            } else {
                (t.clone(), s.clone())
            };
            edge_pairs.insert(key);
        }
    }

    // 3. Iterate mean-aggregation: new = SELF_W * self + (1 - SELF_W) * mean(neighbors).
    let mut current = seeds.clone();
    for _ in 0..NATIVE_TRAIN_ITERS {
        let mut next: HashMap<String, Vec<f32>> = HashMap::with_capacity(current.len());
        for (node, self_vec) in current.iter() {
            let neighbors = adjacency.get(node);
            let mut agg = vec![0.0_f32; dim];
            let mut total_w = 0.0_f32;
            if let Some(neigh) = neighbors {
                for (nb, w) in neigh {
                    if let Some(nv) = current.get(nb) {
                        for i in 0..dim {
                            agg[i] += nv[i] * *w;
                        }
                        total_w += *w;
                    }
                }
            }
            let mut out = vec![0.0_f32; dim];
            if total_w > 0.0 {
                for i in 0..dim {
                    out[i] = NATIVE_SELF_WEIGHT * self_vec[i]
                        + (1.0 - NATIVE_SELF_WEIGHT) * (agg[i] / total_w);
                }
            } else {
                out.copy_from_slice(self_vec);
            }
            l2_normalize(&mut out);
            next.insert(node.clone(), out);
        }
        current = next;
    }

    // 4. Confidence per node: mean cosine similarity to neighbors.
    let mut confidences: HashMap<String, f32> = HashMap::new();
    for (node, emb) in current.iter() {
        let mut sim_sum = 0.0_f32;
        let mut count = 0;
        if let Some(neigh) = adjacency.get(node) {
            for (nb, _) in neigh {
                if let Some(nv) = current.get(nb) {
                    sim_sum += cosine(emb, nv);
                    count += 1;
                }
            }
        }
        let raw = if count > 0 {
            sim_sum / count as f32
        } else {
            0.5
        };
        confidences.insert(node.clone(), raw.clamp(0.0, 1.0));
    }

    // 5. Predict high-similarity edges between non-adjacent nodes. Cap the
    // number so we don't drown the graph in machine-proposed links.
    let nodes: Vec<&String> = current.keys().collect();
    let mut candidates: Vec<(String, String, f32)> = Vec::new();
    for i in 0..nodes.len() {
        for j in (i + 1)..nodes.len() {
            let a = nodes[i];
            let b = nodes[j];
            let key = if a < b {
                (a.clone(), b.clone())
            } else {
                (b.clone(), a.clone())
            };
            if edge_pairs.contains(&key) {
                continue;
            }
            let sim = cosine(&current[a], &current[b]);
            if sim >= NATIVE_PREDICTED_EDGE_MIN_SIM {
                candidates.push((a.clone(), b.clone(), sim));
            }
        }
    }
    candidates.sort_by(|x, y| y.2.partial_cmp(&x.2).unwrap_or(std::cmp::Ordering::Equal));
    candidates.truncate(NATIVE_PREDICTED_EDGE_TOP_K);

    let predicted_edges: Vec<PredictedGraphEdge> = candidates
        .into_iter()
        .map(|(s, t, score)| PredictedGraphEdge {
            source_id: s,
            target_id: t,
            relationship: "relates_to".to_string(),
            score,
        })
        .collect();

    let artifact = GraphEmbeddingArtifact {
        generated_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64,
        model: "native-graphsage-v1".to_string(),
        strategy: "mean-aggregation".to_string(),
        node_embeddings: current,
        node_confidence: confidences,
        node_labels: HashMap::new(),
        edge_updates: Vec::new(),
        predicted_edges,
    };

    let serialized = serde_json::to_string(&artifact).context("serialize GNN artifact")?;
    std::fs::write(artifact_path, serialized)
        .with_context(|| format!("write GNN artifact to {}", artifact_path.display()))?;

    info!(
        "native GraphSAGE done: {} nodes, {} predicted edges",
        artifact.node_embeddings.len(),
        artifact.predicted_edges.len()
    );

    Ok(artifact)
}

fn l2_normalize(v: &mut [f32]) {
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 1e-8 {
        for x in v.iter_mut() {
            *x /= norm;
        }
    }
}

// cosine similarity. you've seen one, you've seen them all.
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0_f32;
    let mut na = 0.0_f32;
    let mut nb = 0.0_f32;
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na <= 0.0 || nb <= 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::embedding::{EmbeddingMemory, EmbeddingType};
    use crate::memory::graph::{GraphMemory, NodeType, Relationship};
    use serde_json::json;

    #[test]
    fn reports_readiness_when_graph_is_large_enough() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let emb = EmbeddingMemory::new(storage.clone());

        let mut ids = Vec::new();
        for idx in 0..30 {
            let node = graph
                .create_node(
                    NodeType::Concept,
                    &format!("node-{idx}"),
                    Some(json!({ "confidence": 0.8 })),
                )
                .unwrap();
            emb.store(
                &node.id,
                EmbeddingType::Summary,
                &[1.0, 0.0, 0.0],
                Some("seed"),
            )
            .unwrap();
            ids.push(node.id);
        }

        for window in ids.windows(2).take(20) {
            graph
                .create_edge(&window[0], &window[1], Relationship::RelatesTo, 1.0, None)
                .unwrap();
        }

        let manager = GraphTrainingManager::new(storage);
        let stats = manager.stats().unwrap();
        assert!(stats.ready);
        assert!(stats.nodes >= 25);
        assert!(stats.edges >= 16);
    }

    /// End-to-end smoke test for the native trainer: build a small graph with
    /// two clusters, run training, and verify the artifact file is written
    /// with non-empty embeddings and sensible confidences.
    #[test]
    fn native_trainer_produces_valid_artifact() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let emb = EmbeddingMemory::new(storage.clone());

        // Two clusters of 4 nodes each with distinct seed embeddings so
        // neighborhood aggregation has something meaningful to smooth.
        let mut cluster_a = Vec::new();
        let mut cluster_b = Vec::new();
        for i in 0..4 {
            let n_a = graph
                .create_node(NodeType::Concept, &format!("a-{i}"), None)
                .unwrap();
            emb.store(
                &n_a.id,
                EmbeddingType::Summary,
                &[1.0, 0.1, 0.0, 0.05],
                Some("cluster A"),
            )
            .unwrap();
            cluster_a.push(n_a.id);

            let n_b = graph
                .create_node(NodeType::Concept, &format!("b-{i}"), None)
                .unwrap();
            emb.store(
                &n_b.id,
                EmbeddingType::Summary,
                &[0.0, 0.05, 1.0, 0.1],
                Some("cluster B"),
            )
            .unwrap();
            cluster_b.push(n_b.id);
        }
        // Dense intra-cluster edges so aggregation has neighbours.
        for ids in [&cluster_a, &cluster_b] {
            for i in 0..ids.len() {
                for j in (i + 1)..ids.len() {
                    graph
                        .create_edge(&ids[i], &ids[j], Relationship::RelatesTo, 1.0, None)
                        .unwrap();
                }
            }
        }

        let artifact_path =
            std::env::temp_dir().join(format!("rook-gnn-test-{}.json", Uuid::new_v4()));
        let artifact = train_native_graphsage(&storage, &artifact_path).unwrap();

        // 1. Every node received an embedding of the right dimension.
        assert_eq!(artifact.node_embeddings.len(), 8);
        for v in artifact.node_embeddings.values() {
            assert_eq!(v.len(), 4);
            // L2-normalized, so norm must be ≈ 1.
            let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
            assert!(
                (norm - 1.0).abs() < 1e-3,
                "node not normalized: norm={norm}"
            );
        }

        // 2. Confidence map covers every node and values are in [0, 1].
        assert_eq!(artifact.node_confidence.len(), 8);
        for c in artifact.node_confidence.values() {
            assert!(*c >= 0.0 && *c <= 1.0, "confidence out of range: {c}");
        }

        // 3. Intra-cluster nodes end up with higher cosine than cross-cluster.
        let a0 = &artifact.node_embeddings[&cluster_a[0]];
        let a1 = &artifact.node_embeddings[&cluster_a[1]];
        let b0 = &artifact.node_embeddings[&cluster_b[0]];
        let intra = cosine(a0, a1);
        let cross = cosine(a0, b0);
        assert!(
            intra > cross,
            "clusters not separated: intra={intra} cross={cross}"
        );

        // 4. Artifact file was actually written to disk.
        assert!(artifact_path.exists());
        let on_disk: GraphEmbeddingArtifact =
            serde_json::from_str(&std::fs::read_to_string(&artifact_path).unwrap()).unwrap();
        assert_eq!(on_disk.model, "native-graphsage-v1");
        assert_eq!(on_disk.node_embeddings.len(), 8);

        let _ = std::fs::remove_file(&artifact_path);
    }

    /// The full async path - storage → apply_artifact_updates → DB writes.
    /// Validates that confidences are persisted into node metadata and that
    /// the training_jobs row is marked 'idle' on success.
    #[tokio::test]
    async fn run_training_now_persists_to_db() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let graph = GraphMemory::new(storage.clone());
        let emb = EmbeddingMemory::new(storage.clone());

        let mut ids = Vec::new();
        for i in 0..6 {
            let n = graph
                .create_node(NodeType::Concept, &format!("n-{i}"), None)
                .unwrap();
            emb.store(&n.id, EmbeddingType::Summary, &[1.0, 0.0, 0.0], Some("s"))
                .unwrap();
            ids.push(n.id);
        }
        for w in ids.windows(2) {
            graph
                .create_edge(&w[0], &w[1], Relationship::RelatesTo, 1.0, None)
                .unwrap();
        }

        let manager = GraphTrainingManager::new(storage.clone());
        let ok = manager.run_training_now().await.unwrap();
        assert!(ok, "trainer reported failure");

        // Confidence must now be written into node metadata.
        let conn = storage.get_connection().unwrap();
        let raw: String = conn
            .query_row(
                "SELECT metadata_json FROM nodes WHERE id = ?1",
                [&ids[0]],
                |r| r.get(0),
            )
            .unwrap();
        let meta: serde_json::Value = serde_json::from_str(&raw).unwrap();
        assert!(meta.get("gnn_confidence").is_some());

        // training_jobs row reports success.
        let status: String = conn
            .query_row(
                "SELECT status FROM training_jobs WHERE job_name = 'graphsage_batch'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(status, "idle");
    }
}
