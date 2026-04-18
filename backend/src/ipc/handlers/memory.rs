use anyhow::Result;
use tracing::warn;

use super::HandlerCtx;
use crate::ipc::protocol::IPCResponse;
use crate::memory::graph::GraphMemory;

pub async fn handle_create_node(
    ctx: &HandlerCtx,
    id: &str,
    node_type: &str,
    title: &str,
    metadata: Option<serde_json::Value>,
) -> Result<IPCResponse> {
    let graph = GraphMemory::new(ctx.memory.clone());
    let nt = node_type
        .parse::<crate::memory::graph::NodeType>()
        .map_err(|e| anyhow::anyhow!(e))?;

    let node = graph.create_node(nt, title, metadata.clone())?;

    let semantic_text = match &metadata {
        Some(meta) => format!("{}\n{}", title, meta),
        None => title.to_string(),
    };

    match ctx.llm.get_embedding(&semantic_text).await {
        Ok(vector) => {
            let embedding_memory =
                crate::memory::embedding::EmbeddingMemory::new(ctx.memory.clone());
            let _ = embedding_memory.delete_embeddings_for_node(&node.id);
            if let Err(err) = embedding_memory.store(
                &node.id,
                crate::memory::embedding::EmbeddingType::Summary,
                &vector,
                Some(&semantic_text),
            ) {
                warn!("Failed to store embedding for node {}: {}", node.id, err);
            }
        }
        Err(err) => {
            warn!("Failed to generate embedding for node {}: {}", node.id, err);
        }
    }

    let training_manager = crate::memory::gnn::GraphTrainingManager::new(ctx.memory.clone());
    tokio::spawn(async move {
        if let Err(err) = training_manager.maybe_train_if_ready().await {
            warn!("GraphSAGE training check failed: {}", err);
        }
    });

    let node_json = serde_json::to_value(&node)?;
    Ok(IPCResponse::NodeCreated {
        id: id.to_string(),
        node: node_json,
    })
}

pub async fn handle_query_memory(
    ctx: &HandlerCtx,
    id: &str,
    query: &str,
    node_type: Option<&str>,
    limit: Option<usize>,
) -> Result<IPCResponse> {
    let graph = GraphMemory::new(ctx.memory.clone());
    let nt = node_type
        .map(|s| s.parse::<crate::memory::graph::NodeType>())
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?;

    let nodes = graph.search_nodes(nt, Some(query))?;
    let nodes_json: Vec<serde_json::Value> = nodes
        .into_iter()
        .filter_map(|n| serde_json::to_value(&n).ok())
        .take(limit.unwrap_or(20))
        .collect();

    Ok(IPCResponse::MemoryResults {
        id: id.to_string(),
        nodes: nodes_json,
    })
}

pub async fn handle_get_node(ctx: &HandlerCtx, id: &str, node_id: &str) -> Result<IPCResponse> {
    let graph = GraphMemory::new(ctx.memory.clone());
    let obj_store = crate::memory::object::ObjectMemoryStore::new(ctx.memory.clone());

    let node = graph.get_node(node_id)?;
    match node {
        Some(n) => {
            let node_json = serde_json::to_value(&n)?;
            let obj_mem = obj_store.get_by_node_id(node_id)?;
            let obj_json = obj_mem.map(|o| serde_json::to_value(&o)).transpose()?;
            Ok(IPCResponse::NodeDetails {
                id: id.to_string(),
                node: node_json,
                object_memory: obj_json,
            })
        }
        None => Ok(IPCResponse::Error {
            id: id.to_string(),
            message: format!("Node not found: {}", node_id),
        }),
    }
}

pub async fn handle_get_connected_nodes(
    ctx: &HandlerCtx,
    id: &str,
    node_id: &str,
    relationship: Option<&str>,
) -> Result<IPCResponse> {
    let graph = GraphMemory::new(ctx.memory.clone());
    let rel = relationship
        .map(|s| s.parse::<crate::memory::graph::Relationship>())
        .transpose()
        .map_err(|e| anyhow::anyhow!(e))?;

    let connected = graph.get_connected_nodes(node_id, rel)?;
    let nodes_json: Vec<serde_json::Value> = connected
        .iter()
        .filter_map(|(n, _)| serde_json::to_value(n).ok())
        .collect();
    let edges_json: Vec<serde_json::Value> = connected
        .iter()
        .filter_map(|(_, e)| serde_json::to_value(e).ok())
        .collect();

    Ok(IPCResponse::ConnectedNodes {
        id: id.to_string(),
        nodes: nodes_json,
        edges: edges_json,
    })
}

pub async fn handle_search_embeddings(
    ctx: &HandlerCtx,
    id: &str,
    query: &str,
    limit: Option<usize>,
) -> Result<IPCResponse> {
    let vector = ctx.llm.get_embedding(query).await?;
    let top_k = limit.unwrap_or(10);
    let curated = crate::memory::context::ContextCurator::new(ctx.memory.clone())
        .curate_with_query_vector(query, &vector, top_k.saturating_mul(8), top_k)?;

    let results_json: Vec<serde_json::Value> = curated
        .into_iter()
        .map(|item| {
            serde_json::json!({
                "node_id": item.node_id,
                "title": item.title,
                "node_type": item.node_type,
                "score": item.total_score,
                "semantic_score": item.embedding_score,
                "graph_score": item.graph_score,
                "recency_score": item.recency_score,
                "confidence_score": item.confidence_score,
                "summary": item.summary,
                "key_facts": item.key_facts,
                "related_titles": item.related_titles,
            })
        })
        .collect();

    Ok(IPCResponse::EmbeddingResults {
        id: id.to_string(),
        results: results_json,
    })
}

pub async fn handle_submit_memory_feedback(
    ctx: &HandlerCtx,
    id: &str,
    node_id: &str,
    query: Option<&str>,
    rating: &str,
    reason: Option<&str>,
    source: Option<&str>,
    session_id: Option<&str>,
) -> Result<IPCResponse> {
    let feedback_store = crate::memory::feedback::MemoryFeedbackStore::new(ctx.memory.clone());
    let rating = crate::memory::feedback::FeedbackRating::from_label(rating);
    let feedback = feedback_store.record(node_id, query, rating, reason, source, session_id)?;

    if let Ok(adj) = feedback_store.confidence_adjustment(node_id) {
        let graph = GraphMemory::new(ctx.memory.clone());
        if let Ok(Some(node)) = graph.get_node(node_id) {
            let mut meta = node.metadata.unwrap_or(serde_json::json!({}));
            let existing = meta
                .get("confidence")
                .and_then(|v| v.as_f64())
                .unwrap_or(0.75);
            let new_confidence = (existing + adj as f64).clamp(0.1, 1.0);
            meta["confidence"] = serde_json::json!(new_confidence);
            let _ = graph.update_node(node_id, None, Some(meta));
        }
    }

    let training_manager = crate::memory::gnn::GraphTrainingManager::new(ctx.memory.clone());
    tokio::spawn(async move {
        if let Err(err) = training_manager.maybe_train_if_ready().await {
            warn!(
                "Periodic batch training check after feedback failed: {}",
                err
            );
        }
    });

    Ok(IPCResponse::FeedbackRecorded {
        id: id.to_string(),
        success: true,
        details: serde_json::to_value(feedback)?,
    })
}
