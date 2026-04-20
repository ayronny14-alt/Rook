use anyhow::{Context, Result};
use notify::{Config, Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::llm::client::LLMClient;
use crate::memory::embedding::{EmbeddingMemory, EmbeddingType};
use crate::memory::graph::{GraphMemory, Node, NodeType, Relationship};
use crate::memory::object::ObjectMemoryStore;
use crate::memory::storage::MemoryStorage;

const MAX_FILE_BYTES_FOR_CONTENT: usize = 512 * 1024;
const CHUNK_LINES: usize = 32;
const MAX_CHUNKS_PER_FILE: usize = 24;
/// Fallback hash-embedding dimensionality (used when no LLM is available).
const EMBEDDING_DIMS: usize = 32;
const SKIP_SEGMENTS: &[&str] = &[
    // Version control
    ".git",
    ".svn",
    ".hg",
    // Build artifacts
    "target",
    "build",
    "dist",
    "out",
    "obj",
    "bin",
    // Node / JS ecosystem
    "node_modules",
    ".npm",
    ".yarn",
    ".pnpm-store",
    // Python
    "__pycache__",
    ".venv",
    "venv",
    "env",
    ".mypy_cache",
    // Rust / Cargo
    ".cargo",
    "registry",
    "git",
    // Java / JVM
    ".gradle",
    ".m2",
    // OS / System
    "windows",
    "program files",
    "program files (x86)",
    "system32",
    "$recycle.bin",
    "appdata",
    "local",
    "roaming",
    "library",
    "caches",
    "system",
    "volumes",
    // Cloud sync metadata
    ".dropbox",
    ".dropbox.cache",
    ".syncthing",
    ".stfolder",
    // IDE metadata
    ".idea",
    ".vscode",
    ".vs",
    ".settings",
    // Containers / VMs
    "docker",
    ".docker",
    "virtualbox vms",
    // Misc noise
    "tmp",
    "temp",
    "cache",
    "logs",
    "log",
    ".ds_store",
];

#[derive(Debug, Clone, Default)]
struct StructuralFingerprint {
    imports: Vec<String>,
    symbols: Vec<String>,
    comments: Vec<String>,
    line_count: usize,
    symbol_count: usize,
    cyclomatic_complexity: usize,
}

#[derive(Debug, Clone)]
struct ChunkFingerprint {
    summary: String,
    content: String,
}

#[derive(Clone)]
pub struct FileIndexer {
    memory: MemoryStorage,
    /// When Some, file chunks are embedded with real neural vectors via the LLM client.
    /// When None, falls back to deterministic hash embeddings (offline / no API key).
    llm: Option<LLMClient>,
    watched_dirs: Arc<Mutex<Vec<PathBuf>>>,
    content_hashes: Arc<Mutex<HashMap<PathBuf, String>>>,
    watcher: Arc<Mutex<Option<RecommendedWatcher>>>,
}

impl FileIndexer {
    pub fn new(memory: MemoryStorage) -> Self {
        Self {
            memory,
            llm: None,
            watched_dirs: Arc::new(Mutex::new(Vec::new())),
            content_hashes: Arc::new(Mutex::new(HashMap::new())),
            watcher: Arc::new(Mutex::new(None)),
        }
    }

    /// Create an indexer that uses real LLM embeddings for file chunks.
    /// Use this when a conversation sets a working directory via `change_dir`.
    pub fn new_with_llm(memory: MemoryStorage, llm: LLMClient) -> Self {
        Self {
            memory,
            llm: Some(llm),
            watched_dirs: Arc::new(Mutex::new(Vec::new())),
            content_hashes: Arc::new(Mutex::new(HashMap::new())),
            watcher: Arc::new(Mutex::new(None)),
        }
    }

    /// Compute an embedding vector for `text`, using the LLM client when available
    /// and falling back to a deterministic hash embedding otherwise.
    async fn embed(text: &str, llm: &Option<LLMClient>) -> Vec<f32> {
        if let Some(client) = llm {
            match client.get_embedding(text).await {
                Ok(v) => return v,
                Err(e) => warn!("LLM embedding failed, using hash fallback: {}", e),
            }
        }
        Self::hash_embedding(text, EMBEDDING_DIMS)
    }

    pub fn start_watching(&self) -> Result<()> {
        if self.watcher.lock().unwrap().is_some() {
            return Ok(());
        }

        let memory = self.memory.clone();
        let llm = self.llm.clone();
        let hashes = self.content_hashes.clone();
        let (tx, mut rx) = mpsc::channel::<Event>(256);

        let mut watcher = RecommendedWatcher::new(
            move |res: notify::Result<Event>| {
                if let Ok(event) = res {
                    let _ = tx.blocking_send(event);
                }
            },
            Config::default().with_poll_interval(Duration::from_secs(2)),
        )?;

        let dirs_to_watch = {
            let mut dirs = self.watched_dirs.lock().unwrap();
            if dirs.is_empty() {
                if let Ok(current_dir) = std::env::current_dir() {
                    dirs.push(current_dir);
                }
            }
            dirs.clone()
        };

        for dir in &dirs_to_watch {
            if dir.exists() {
                watcher.watch(dir, RecursiveMode::Recursive)?;
                info!("Watching directory: {}", dir.display());
            }
        }

        *self.watcher.lock().unwrap() = Some(watcher);

        tokio::spawn(async move {
            while let Some(event) = rx.recv().await {
                if let Err(e) = Self::handle_event(&event, &memory, &hashes, &llm).await {
                    error!("Error handling file event: {}", e);
                }
            }
        });

        for dir in dirs_to_watch {
            let indexer = self.clone();
            tokio::spawn(async move {
                match indexer.index_directory(&dir).await {
                    Ok(count) => info!(
                        "Initial local index completed for {} files under {}",
                        count,
                        dir.display()
                    ),
                    Err(err) => warn!(
                        "Initial local indexing failed for {}: {}",
                        dir.display(),
                        err
                    ),
                }
            });
        }

        info!("File watcher started with local deterministic summarization enabled");
        Ok(())
    }

    pub fn watch_directory(&self, dir: &Path) -> Result<()> {
        #[allow(dead_code)]
        let canonical = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
        let mut dirs = self.watched_dirs.lock().unwrap();
        if dirs.iter().any(|existing| existing == &canonical) {
            return Ok(());
        }
        dirs.push(canonical.clone());
        drop(dirs);

        if let Some(watcher) = self.watcher.lock().unwrap().as_mut() {
            watcher.watch(&canonical, RecursiveMode::Recursive)?;
        }

        info!("Watching directory: {}", canonical.display());
        Ok(())
    }

    pub async fn index_directory(&self, root: &Path) -> Result<usize> {
        if !root.exists() {
            return Ok(0);
        }

        let mut indexed = 0usize;
        let mut pending = vec![root.to_path_buf()];

        while let Some(current) = pending.pop() {
            if Self::should_skip_path(&current) {
                continue;
            }

            let entries = match std::fs::read_dir(&current) {
                Ok(entries) => entries,
                Err(err) => {
                    warn!(
                        "Skipping unreadable directory {}: {}",
                        current.display(),
                        err
                    );
                    continue;
                }
            };

            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    if !Self::should_skip_path(&path) {
                        pending.push(path);
                    }
                    continue;
                }

                if path.is_file() {
                    match self.index_file(&path).await {
                        Ok(()) => indexed += 1,
                        Err(err) => warn!("Failed to index {}: {}", path.display(), err),
                    }
                }
            }
        }

        Ok(indexed)
    }

    pub async fn index_file(&self, path: &Path) -> Result<()> {
        Self::index_path_internal(&self.memory, &self.content_hashes, path, true, &self.llm).await
    }

    async fn handle_event(
        event: &Event,
        memory: &MemoryStorage,
        hashes: &Arc<Mutex<HashMap<PathBuf, String>>>,
        llm: &Option<LLMClient>,
    ) -> Result<()> {
        if matches!(
            event.kind,
            EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
        ) {
            for path in &event.paths {
                if !path.is_file() {
                    continue;
                }

                debug!("File event: {:?} {}", event.kind, path.display());
                if matches!(event.kind, EventKind::Remove(_)) {
                    hashes.lock().unwrap().remove(path);
                    Self::record_index_state(memory, path, None, 0, "pending")?;
                    continue;
                }

                if let Err(err) = Self::index_path_internal(memory, hashes, path, false, llm).await
                {
                    warn!(
                        "Failed to re-index {} after change: {}",
                        path.display(),
                        err
                    );
                }
            }
        }
        Ok(())
    }

    async fn index_path_internal(
        memory: &MemoryStorage,
        hashes: &Arc<Mutex<HashMap<PathBuf, String>>>,
        path: &Path,
        allow_skipped_path: bool,
        llm: &Option<LLMClient>,
    ) -> Result<()> {
        if !path.is_file() || (!allow_skipped_path && Self::should_skip_path(path)) {
            return Ok(());
        }

        let fs_metadata = tokio::fs::metadata(path)
            .await
            .with_context(|| format!("Failed to read metadata for {}", path.display()))?;
        let raw_bytes = tokio::fs::read(path)
            .await
            .with_context(|| format!("Failed to read {}", path.display()))?;
        let hash = Self::compute_hash(&raw_bytes);

        {
            let mut known_hashes = hashes.lock().unwrap();
            if let Some(existing_hash) = known_hashes.get(path) {
                if existing_hash == &hash {
                    debug!("File unchanged, skipping: {}", path.display());
                    return Ok(());
                }
            }
            known_hashes.insert(path.to_path_buf(), hash.clone());
        }

        let content = if raw_bytes.len() <= MAX_FILE_BYTES_FOR_CONTENT {
            String::from_utf8(raw_bytes.clone()).ok()
        } else {
            None
        };
        let is_text = content.is_some();

        let file_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_else(|| "unknown".to_string());
        let mime_type = Self::guess_mime_type(path, is_text);
        let file_type = Self::guess_file_type(path, is_text);
        let metadata_summary =
            Self::build_metadata_summary(path, &fs_metadata, &mime_type, &file_type);

        let fingerprint = content
            .as_deref()
            .map(Self::extract_structure)
            .unwrap_or_default();
        let structural_summary = Self::format_structure_summary(&fingerprint, is_text);
        let chunk_fingerprints = content
            .as_deref()
            .map(Self::chunk_fingerprints)
            .unwrap_or_default();

        let mut metadata_json =
            Self::build_file_metadata(path, &fs_metadata, &mime_type, &file_type);
        let neural = llm.is_some();
        if let Some(object) = metadata_json.as_object_mut() {
            object.insert(
                "line_count".to_string(),
                serde_json::json!(fingerprint.line_count),
            );
            object.insert(
                "symbol_count".to_string(),
                serde_json::json!(fingerprint.symbol_count),
            );
            object.insert(
                "cyclomatic_complexity".to_string(),
                serde_json::json!(fingerprint.cyclomatic_complexity),
            );
            object.insert(
                "chunk_count".to_string(),
                serde_json::json!(chunk_fingerprints.len()),
            );
            object.insert(
                "indexing_mode".to_string(),
                serde_json::json!(if neural { "neural" } else { "hash" }),
            );
        }

        let graph = GraphMemory::new(memory.clone());
        let object_store = ObjectMemoryStore::new(memory.clone());
        let embedding_store = EmbeddingMemory::new(memory.clone());

        let node = Self::upsert_file_node(&graph, &file_name, path, metadata_json)?;
        let mut object_memory = ObjectMemoryStore::create_for_node(&node.id);
        object_memory.summary = Some(metadata_summary.clone());
        object_memory.key_facts = Self::build_key_facts(
            path,
            &mime_type,
            &file_type,
            &fingerprint,
            chunk_fingerprints.len(),
        );
        object_memory.extracted_structure = Some(structural_summary.clone());
        object_memory.code_signatures = Some(Self::format_signatures_and_chunks(
            &fingerprint,
            &chunk_fingerprints,
        ));
        object_memory.todos = content
            .as_deref()
            .map(Self::extract_todos)
            .unwrap_or_default();
        object_memory.content_hash = Some(hash.clone());
        object_memory.tags = Self::generate_tags(path, &mime_type, &file_type, &fingerprint);
        object_store.upsert(&node.id, &object_memory)?;

        // Embed the File node summary
        embedding_store.delete_embeddings_for_node(&node.id)?;
        let summary_text = format!("{}\n{}", metadata_summary, structural_summary);
        let summary_vector = Self::embed(&summary_text, llm).await;
        embedding_store.store(
            &node.id,
            EmbeddingType::Summary,
            &summary_vector,
            Some(&summary_text),
        )?;

        if let Some(text_content) = content.as_deref() {
            let content_excerpt = Self::truncate_text(text_content, 1200);
            let content_vector = Self::embed(&content_excerpt, llm).await;
            embedding_store.store(
                &node.id,
                EmbeddingType::Content,
                &content_vector,
                Some(&content_excerpt),
            )?;

            // Delete stale FileChunk nodes that belonged to this File
            Self::delete_file_chunks(memory, &node.id)?;

            // Create a FileChunk node for each chunk (with neural embeddings)
            let path_str = path.to_string_lossy().to_string();
            for (chunk_idx, chunk) in chunk_fingerprints
                .iter()
                .enumerate()
                .take(MAX_CHUNKS_PER_FILE)
            {
                let line_start = chunk_idx * CHUNK_LINES;
                let line_end = (line_start + CHUNK_LINES).min(fingerprint.line_count);

                // Title: first non-empty line of the chunk (≤60 chars)
                let chunk_title = chunk.summary.chars().take(60).collect::<String>();
                let chunk_label = if chunk_title.trim().is_empty() {
                    format!("{} chunk {}", file_name, chunk_idx + 1)
                } else {
                    chunk_title
                };

                let chunk_meta = serde_json::json!({
                    "parent_file_id": node.id,
                    "parent_path":    path_str,
                    "chunk_index":    chunk_idx,
                    "line_start":     line_start,
                    "line_end":       line_end,
                    "content":        chunk.content,
                });

                match graph.create_node(NodeType::FileChunk, &chunk_label, Some(chunk_meta)) {
                    Ok(chunk_node) => {
                        // Edge: File → FileChunk (Contains)
                        let _ = graph.create_edge_if_not_exists(
                            &node.id,
                            &chunk_node.id,
                            Relationship::Contains,
                            1.0,
                        );

                        // Store chunk content in object memory for retrieval
                        let mut obj = ObjectMemoryStore::create_for_node(&chunk_node.id);
                        obj.summary = Some(chunk.content.clone());
                        obj.key_facts = vec![chunk.summary.clone()];
                        let _ = object_store.upsert(&chunk_node.id, &obj);

                        // Neural (or hash) embedding of the chunk
                        let embed_text =
                            format!("{} | {}\n{}", file_name, chunk.summary, chunk.content);
                        let chunk_vec = Self::embed(&embed_text, llm).await;
                        let _ = embedding_store.store(
                            &chunk_node.id,
                            EmbeddingType::Content,
                            &chunk_vec,
                            Some(&embed_text),
                        );
                    }
                    Err(e) => warn!(
                        "Failed to create FileChunk node for {}: {}",
                        path.display(),
                        e
                    ),
                }
            }

            Self::sync_dependency_nodes(memory, &graph, &node.id, &fingerprint.imports)?;
        }

        Self::record_index_state(memory, path, Some(&hash), fs_metadata.len(), "indexed")?;
        info!(
            "Indexed file: {} -> node {} ({} chunks, {} embeddings)",
            path.display(),
            node.id,
            chunk_fingerprints.len().min(MAX_CHUNKS_PER_FILE),
            if neural { "neural" } else { "hash" },
        );
        Ok(())
    }

    /// Delete all FileChunk nodes whose parent is `file_node_id`.
    fn delete_file_chunks(memory: &MemoryStorage, file_node_id: &str) -> Result<()> {
        let conn = memory.get_connection().map_err(anyhow::Error::msg)?;
        // Find chunk node ids via the Contains edge
        let mut stmt = conn.prepare(
            "SELECT n.id FROM nodes n
             JOIN edges e ON e.target_id = n.id
             WHERE e.source_id = ?1
               AND e.relationship = 'contains'
               AND n.node_type = 'file_chunk'",
        )?;
        let chunk_ids: Vec<String> = stmt
            .query_map(rusqlite::params![file_node_id], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        for cid in &chunk_ids {
            conn.execute("DELETE FROM nodes WHERE id = ?1", rusqlite::params![cid])?;
        }
        Ok(())
    }

    fn upsert_file_node(
        graph: &GraphMemory,
        file_name: &str,
        path: &Path,
        metadata: serde_json::Value,
    ) -> Result<Node> {
        let path_str = path.to_string_lossy().to_string();
        let existing = graph
            .search_nodes(Some(NodeType::File), Some(file_name))?
            .into_iter()
            .find(|candidate| {
                candidate
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("path"))
                    .and_then(|value| value.as_str())
                    == Some(path_str.as_str())
            });

        if let Some(mut node) = existing {
            graph.update_node(&node.id, Some(file_name), Some(metadata.clone()))?;
            node.title = file_name.to_string();
            node.metadata = Some(metadata);
            return Ok(node);
        }

        Ok(graph.create_node(NodeType::File, file_name, Some(metadata))?)
    }

    fn sync_dependency_nodes(
        memory: &MemoryStorage,
        graph: &GraphMemory,
        file_node_id: &str,
        imports: &[String],
    ) -> Result<()> {
        for dependency in imports.iter().take(24) {
            let dependency_node = graph
                .search_nodes(Some(NodeType::Dependency), Some(dependency))?
                .into_iter()
                .find(|node| node.title.eq_ignore_ascii_case(dependency))
                .map(Ok)
                .unwrap_or_else(|| {
                    graph.create_node(
                        NodeType::Dependency,
                        dependency,
                        Some(serde_json::json!({ "kind": "import", "source": "local_indexer" })),
                    )
                })?;

            let already_linked = {
                let conn = memory.get_connection().map_err(anyhow::Error::msg)?;
                let count: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM edges WHERE source_id = ?1 AND target_id = ?2 AND relationship = 'imports'",
                    rusqlite::params![file_node_id, &dependency_node.id],
                    |row| row.get(0),
                )?;
                count > 0
            };

            if !already_linked {
                graph.create_edge(
                    file_node_id,
                    &dependency_node.id,
                    Relationship::Imports,
                    0.72,
                    Some(serde_json::json!({ "source": "local_indexer" })),
                )?;
            }
        }

        Ok(())
    }

    fn record_index_state(
        memory: &MemoryStorage,
        path: &Path,
        hash: Option<&str>,
        file_size: u64,
        status: &str,
    ) -> Result<()> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        let conn = memory.get_connection().map_err(anyhow::Error::msg)?;
        conn.execute(
            "INSERT INTO indexing_state (path, last_indexed, content_hash, file_size, status)
             VALUES (?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(path) DO UPDATE SET
                last_indexed = excluded.last_indexed,
                content_hash = excluded.content_hash,
                file_size = excluded.file_size,
                status = excluded.status",
            rusqlite::params![
                path.to_string_lossy().to_string(),
                now,
                hash.unwrap_or_default(),
                file_size as i64,
                status,
            ],
        )?;
        Ok(())
    }

    fn should_skip_path(path: &Path) -> bool {
        path.components()
            .filter_map(|component| component.as_os_str().to_str())
            .any(|segment| {
                SKIP_SEGMENTS
                    .iter()
                    .any(|skip| segment.eq_ignore_ascii_case(skip))
            })
    }

    fn compute_hash(content: &[u8]) -> String {
        format!("{:x}", Sha256::digest(content))
    }

    fn build_metadata_summary(
        path: &Path,
        metadata: &std::fs::Metadata,
        mime_type: &str,
        file_type: &str,
    ) -> String {
        let extension = path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default();
        let parent_folders = path
            .parent()
            .map(Self::parent_folder_names)
            .unwrap_or_else(|| "root".to_string());
        let modified = metadata
            .modified()
            .ok()
            .map(Self::format_system_time)
            .unwrap_or_else(|| "unknown".to_string());
        let created = metadata
            .created()
            .ok()
            .map(Self::format_system_time)
            .unwrap_or_else(|| "unknown".to_string());

        format!(
            "Type: {}\nPath: {}\nExtension: {}\nSize: {}\nModified: {}\nCreated: {}\nPath depth: {}\nParent folders: {}\nMIME type: {}",
            file_type,
            path.display(),
            if extension.is_empty() { "none" } else { extension },
            Self::format_bytes(metadata.len()),
            modified,
            created,
            path.components().count(),
            parent_folders,
            mime_type,
        )
    }

    fn build_file_metadata(
        path: &Path,
        metadata: &std::fs::Metadata,
        mime_type: &str,
        file_type: &str,
    ) -> serde_json::Value {
        let modified = metadata
            .modified()
            .ok()
            .map(Self::format_system_time)
            .unwrap_or_else(|| "unknown".to_string());
        let created = metadata
            .created()
            .ok()
            .map(Self::format_system_time)
            .unwrap_or_else(|| "unknown".to_string());

        serde_json::json!({
            "path": path.to_string_lossy().to_string(),
            "extension": path.extension().and_then(|value| value.to_str()).unwrap_or_default(),
            "size_bytes": metadata.len(),
            "size_human": Self::format_bytes(metadata.len()),
            "modified": modified,
            "created": created,
            "path_depth": path.components().count(),
            "parent_folders": path.parent().map(Self::parent_folder_names).unwrap_or_else(|| "root".to_string()),
            "mime_type": mime_type,
            "file_type": file_type,
        })
    }

    fn extract_structure(content: &str) -> StructuralFingerprint {
        let mut fingerprint = StructuralFingerprint {
            line_count: content.lines().count(),
            cyclomatic_complexity: 1,
            ..Default::default()
        };

        for line in content.lines() {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }

            if Self::is_comment_line(trimmed) && fingerprint.comments.len() < 5 {
                fingerprint.comments.push(Self::truncate_text(trimmed, 100));
            }

            if let Some(import) = Self::extract_import_target(trimmed) {
                fingerprint.imports.push(import);
            }

            if let Some(symbol) = Self::extract_symbol_signature(trimmed) {
                fingerprint.symbols.push(symbol);
            }

            fingerprint.cyclomatic_complexity += Self::count_control_flow_tokens(trimmed);
        }

        fingerprint.imports.sort();
        fingerprint.imports.dedup();
        fingerprint.symbols.sort();
        fingerprint.symbols.dedup();
        fingerprint.symbol_count = fingerprint.symbols.len();
        fingerprint
    }

    fn format_structure_summary(fingerprint: &StructuralFingerprint, is_text: bool) -> String {
        if !is_text {
            return "Structural summary: binary or non-UTF8 file; metadata-only indexing applied."
                .to_string();
        }

        let imports = if fingerprint.imports.is_empty() {
            "none".to_string()
        } else {
            fingerprint
                .imports
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        let symbols = if fingerprint.symbols.is_empty() {
            "none".to_string()
        } else {
            fingerprint
                .symbols
                .iter()
                .take(12)
                .cloned()
                .collect::<Vec<_>>()
                .join(", ")
        };
        let comments = if fingerprint.comments.is_empty() {
            "none".to_string()
        } else {
            fingerprint.comments.join(" | ")
        };

        format!(
            "Lines: {}\nSymbols: {}\nImports: {}\nTop-level comments: {}\nApprox cyclomatic complexity: {}\nDependency graph: {}",
            fingerprint.line_count,
            symbols,
            imports,
            comments,
            fingerprint.cyclomatic_complexity,
            imports,
        )
    }

    fn chunk_fingerprints(content: &str) -> Vec<ChunkFingerprint> {
        let lines: Vec<&str> = content.lines().collect();
        let mut chunks = Vec::new();

        for (index, segment) in lines
            .chunks(CHUNK_LINES)
            .take(MAX_CHUNKS_PER_FILE)
            .enumerate()
        {
            let start_line = index * CHUNK_LINES + 1;
            let end_line = start_line + segment.len().saturating_sub(1);
            let chunk_content = segment.join("\n");
            let preview = Self::truncate_text(
                segment
                    .iter()
                    .map(|line| line.trim())
                    .find(|line| !line.is_empty())
                    .unwrap_or(""),
                90,
            );
            let chunk_symbols = segment
                .iter()
                .filter_map(|line| Self::extract_symbol_signature(line.trim()))
                .take(4)
                .collect::<Vec<_>>();

            let symbol_text = if chunk_symbols.is_empty() {
                "none".to_string()
            } else {
                chunk_symbols.join(", ")
            };

            chunks.push(ChunkFingerprint {
                summary: format!(
                    "Chunk {} (lines {}-{}). Symbols: {}. Preview: {}",
                    index + 1,
                    start_line,
                    end_line,
                    symbol_text,
                    preview,
                ),
                content: chunk_content,
            });
        }

        chunks
    }

    fn format_signatures_and_chunks(
        fingerprint: &StructuralFingerprint,
        chunks: &[ChunkFingerprint],
    ) -> String {
        let signatures = if fingerprint.symbols.is_empty() {
            "none".to_string()
        } else {
            fingerprint.symbols.join("\n")
        };

        let chunk_lines = if chunks.is_empty() {
            "Chunk fingerprints: none".to_string()
        } else {
            chunks
                .iter()
                .map(|chunk| chunk.summary.clone())
                .collect::<Vec<_>>()
                .join("\n")
        };

        format!(
            "Signatures:\n{}\n\nChunk fingerprints:\n{}",
            signatures, chunk_lines
        )
    }

    fn build_key_facts(
        path: &Path,
        mime_type: &str,
        file_type: &str,
        fingerprint: &StructuralFingerprint,
        chunk_count: usize,
    ) -> Vec<String> {
        let mut facts = vec![
            format!("File type: {}", file_type),
            format!("MIME type: {}", mime_type),
            format!("Path depth: {}", path.components().count()),
            format!("Indexed with local deterministic 4-layer summarization"),
        ];

        if fingerprint.line_count > 0 {
            facts.push(format!("Line count: {}", fingerprint.line_count));
        }
        if fingerprint.symbol_count > 0 {
            facts.push(format!("Symbol count: {}", fingerprint.symbol_count));
        }
        facts.push(format!("Chunk fingerprints: {}", chunk_count));
        facts.push(format!(
            "Approx cyclomatic complexity: {}",
            fingerprint.cyclomatic_complexity
        ));

        if !fingerprint.imports.is_empty() {
            facts.push(format!("Imports: {}", fingerprint.imports.join(", ")));
        }

        facts
    }

    fn extract_todos(content: &str) -> Vec<String> {
        content
            .lines()
            .map(str::trim)
            .filter(|line| line.contains("TODO") || line.contains("FIXME") || line.contains("HACK"))
            .map(ToString::to_string)
            .collect()
    }

    fn generate_tags(
        path: &Path,
        mime_type: &str,
        file_type: &str,
        fingerprint: &StructuralFingerprint,
    ) -> Vec<String> {
        let mut tags = HashSet::new();
        tags.insert("local-indexed".to_string());

        if let Some(extension) = path.extension().and_then(|value| value.to_str()) {
            tags.insert(extension.to_ascii_lowercase());
        }

        if mime_type.starts_with("text/") {
            tags.insert("text".to_string());
        }
        if file_type.contains("source") {
            tags.insert("code".to_string());
        }
        if fingerprint.cyclomatic_complexity >= 8 {
            tags.insert("complex".to_string());
        }

        for import in fingerprint.imports.iter().take(6) {
            let normalized = import
                .split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '-'))
                .find(|segment| !segment.is_empty())
                .unwrap_or(import)
                .to_ascii_lowercase();
            tags.insert(normalized);
        }

        let mut sorted = tags.into_iter().collect::<Vec<_>>();
        sorted.sort();
        sorted
    }

    fn extract_import_target(trimmed: &str) -> Option<String> {
        if let Some(rest) = trimmed.strip_prefix("use ") {
            return Some(rest.trim_end_matches(';').to_string());
        }
        if let Some(rest) = trimmed.strip_prefix("import ") {
            return Some(
                rest.split_whitespace()
                    .take(3)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        if let Some(rest) = trimmed.strip_prefix("from ") {
            return Some(
                rest.split_whitespace()
                    .take(2)
                    .collect::<Vec<_>>()
                    .join(" "),
            );
        }
        if trimmed.starts_with("#include") {
            return Some(trimmed.to_string());
        }
        None
    }

    fn extract_symbol_signature(trimmed: &str) -> Option<String> {
        let starts = [
            "fn ",
            "pub fn ",
            "async fn ",
            "pub async fn ",
            "def ",
            "class ",
            "struct ",
            "pub struct ",
            "enum ",
            "pub enum ",
            "trait ",
            "pub trait ",
            "interface ",
            "impl ",
        ];

        if starts.iter().any(|prefix| trimmed.starts_with(prefix)) {
            return Some(
                trimmed
                    .trim_end_matches('{')
                    .trim_end_matches(':')
                    .trim()
                    .chars()
                    .take(120)
                    .collect(),
            );
        }

        None
    }

    fn is_comment_line(trimmed: &str) -> bool {
        trimmed.starts_with("//")
            || trimmed.starts_with("#")
            || trimmed.starts_with("/*")
            || trimmed.starts_with("* ")
            || trimmed.starts_with("\"\"\"")
            || trimmed.starts_with("'''")
    }

    fn count_control_flow_tokens(line: &str) -> usize {
        [
            " if ", " for ", " while ", " match ", " case ", " elif ", " except ", "&&", "||",
        ]
        .iter()
        .filter(|token| {
            let padded = format!(" {} ", line);
            padded.contains(**token)
        })
        .count()
    }

    fn hash_embedding(text: &str, dims: usize) -> Vec<f32> {
        let mut values = vec![0.0_f32; dims];
        for (index, byte) in text.bytes().enumerate() {
            values[index % dims] += (byte as f32) / 255.0;
        }

        let norm = values.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut values {
                *value /= norm;
            }
        }

        values
    }

    fn guess_mime_type(path: &Path, is_text: bool) -> String {
        match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "rs" => "text/x-rust".to_string(),
            "py" => "text/x-python".to_string(),
            "js" => "text/javascript".to_string(),
            "ts" => "text/typescript".to_string(),
            "tsx" => "text/tsx".to_string(),
            "jsx" => "text/jsx".to_string(),
            "svelte" => "text/x-svelte".to_string(),
            "json" => "application/json".to_string(),
            "md" => "text/markdown".to_string(),
            "toml" => "application/toml".to_string(),
            "yaml" | "yml" => "application/yaml".to_string(),
            "html" => "text/html".to_string(),
            "css" => "text/css".to_string(),
            "cpp" | "cc" | "cxx" => "text/x-c++src".to_string(),
            "c" => "text/x-csrc".to_string(),
            "h" | "hpp" => "text/x-chdr".to_string(),
            "xaml" => "application/xaml+xml".to_string(),
            _ if is_text => "text/plain".to_string(),
            _ => "application/octet-stream".to_string(),
        }
    }

    fn guess_file_type(path: &Path, is_text: bool) -> String {
        match path
            .extension()
            .and_then(|value| value.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "rs" => "Rust source file".to_string(),
            "py" => "Python source file".to_string(),
            "js" | "ts" | "tsx" | "jsx" | "svelte" => "Web source file".to_string(),
            "json" | "yaml" | "yml" | "toml" => "Configuration file".to_string(),
            "md" => "Markdown document".to_string(),
            "cpp" | "cc" | "cxx" | "c" | "h" | "hpp" => "C/C++ source file".to_string(),
            "xaml" => "XAML UI file".to_string(),
            _ if is_text => "Text file".to_string(),
            _ => "Binary file".to_string(),
        }
    }

    fn parent_folder_names(path: &Path) -> String {
        let mut names = path
            .components()
            .filter_map(|component| component.as_os_str().to_str())
            .map(ToString::to_string)
            .collect::<Vec<_>>();

        if names.len() > 4 {
            names = names[names.len().saturating_sub(4)..].to_vec();
        }

        names.join(" > ")
    }

    fn format_system_time(time: std::time::SystemTime) -> String {
        let datetime: chrono::DateTime<chrono::Utc> = time.into();
        datetime.format("%Y-%m-%d %H:%M:%S UTC").to_string()
    }

    fn format_bytes(bytes: u64) -> String {
        if bytes < 1024 {
            return format!("{} B", bytes);
        }
        if bytes < 1024 * 1024 {
            return format!("{:.1} KB", bytes as f64 / 1024.0);
        }
        if bytes < 1024 * 1024 * 1024 {
            return format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0));
        }
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    }

    fn truncate_text(text: &str, max_len: usize) -> String {
        let trimmed = text.trim();
        if trimmed.chars().count() <= max_len {
            return trimmed.to_string();
        }
        trimmed.chars().take(max_len).collect::<String>()
    }
}

#[cfg(test)]
mod tests {
    use super::FileIndexer;
    use crate::memory::embedding::{EmbeddingMemory, EmbeddingType};
    use crate::memory::graph::{GraphMemory, NodeType};
    use crate::memory::object::ObjectMemoryStore;
    use crate::memory::storage::MemoryStorage;
    use uuid::Uuid;

    #[test]
    fn skips_irrelevant_cache_and_build_paths() {
        assert!(FileIndexer::should_skip_path(std::path::Path::new(
            "C:/repo/.git/index.lock"
        )));
        assert!(FileIndexer::should_skip_path(std::path::Path::new(
            "C:/repo/node_modules/pkg/index.js"
        )));
        assert!(FileIndexer::should_skip_path(std::path::Path::new(
            "C:/Users/Aaron/AppData/Local/Temp/file.txt"
        )));
        assert!(FileIndexer::should_skip_path(std::path::Path::new(
            "C:/repo/.vscode/settings.json"
        )));
        assert!(!FileIndexer::should_skip_path(std::path::Path::new(
            "C:/repo/src/main.rs"
        )));
    }

    #[tokio::test]
    async fn indexes_four_layer_local_summary_for_code_files() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let indexer = FileIndexer::new(storage.clone());

        let temp_file = std::env::temp_dir().join(format!("rook-index-{}.py", Uuid::new_v4()));
        std::fs::write(
            &temp_file,
            "import json\nimport sqlite3\n\n# Graph training helper\nclass GraphSageTrainer:\n    pass\n\ndef summarize_predictions(items):\n    if items:\n        return len(items)\n    return 0\n",
        )
        .unwrap();

        indexer.index_file(&temp_file).await.unwrap();

        let graph = GraphMemory::new(storage.clone());
        let node = graph
            .search_nodes(Some(NodeType::File), Some("rook-index-"))
            .unwrap()
            .into_iter()
            .find(|candidate| {
                candidate
                    .metadata
                    .as_ref()
                    .and_then(|meta| meta.get("path"))
                    .and_then(|value| value.as_str())
                    == Some(temp_file.to_string_lossy().as_ref())
            })
            .expect("file node should be created");

        let object = ObjectMemoryStore::new(storage.clone())
            .get_by_node_id(&node.id)
            .unwrap()
            .expect("object memory should exist");

        let summary = object.summary.unwrap_or_default();
        assert!(
            summary.contains("Type:"),
            "summary should include metadata type"
        );
        assert!(
            summary.contains("Path:"),
            "summary should include file path"
        );

        let structure = object.extracted_structure.unwrap_or_default();
        assert!(
            structure.contains("Imports:"),
            "structure should include imports"
        );
        assert!(
            structure.contains("GraphSageTrainer"),
            "structure should include symbols"
        );

        let code_signatures = object.code_signatures.unwrap_or_default();
        assert!(
            code_signatures.contains("Chunk"),
            "chunk fingerprints should be stored"
        );

        let embeddings = EmbeddingMemory::new(storage.clone())
            .get_embeddings_for_node(&node.id)
            .unwrap();
        assert!(
            embeddings
                .iter()
                .any(|record| matches!(record.embedding_type, EmbeddingType::Summary)),
            "summary embedding should be stored"
        );
        assert!(
            embeddings
                .iter()
                .any(|record| matches!(record.embedding_type, EmbeddingType::Fact)),
            "chunk-level fact embeddings should be stored"
        );

        let _ = std::fs::remove_file(&temp_file);
    }
}
