use dirs;
use r2d2::Pool;
use r2d2_sqlite::SqliteConnectionManager;
use std::path::PathBuf;

const SCHEMA: &str = r#"
-- ============================================================
-- LAYER 1: GRAPH MEMORY (Relationships between entities)
-- ============================================================

CREATE TABLE IF NOT EXISTS nodes (
    id TEXT PRIMARY KEY,
    node_type TEXT NOT NULL,        -- 'project', 'file', 'website', 'tool', 'task', 'concept', 'ui_state'
    title TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    metadata_json TEXT              -- Arbitrary JSON metadata
);

CREATE TABLE IF NOT EXISTS edges (
    id TEXT PRIMARY KEY,
    source_id TEXT NOT NULL,
    target_id TEXT NOT NULL,
    relationship TEXT NOT NULL,     -- 'contains', 'depends_on', 'references', 'implements', 'modifies', 'relates_to'
    strength REAL DEFAULT 1.0,      -- Weight of relationship (0.0 to 1.0)
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    metadata_json TEXT,
    FOREIGN KEY (source_id) REFERENCES nodes(id) ON DELETE CASCADE,
    FOREIGN KEY (target_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_edges_source ON edges(source_id);
CREATE INDEX IF NOT EXISTS idx_edges_target ON edges(target_id);
CREATE INDEX IF NOT EXISTS idx_nodes_type ON nodes(node_type);

-- ============================================================
-- LAYER 2: OBJECT MEMORY (Precise detail for each node)
-- ============================================================

CREATE TABLE IF NOT EXISTS object_memory (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL UNIQUE,
    summary TEXT,
    key_facts_json TEXT,            -- JSON array of key facts
    extracted_structure TEXT,       -- Code structure, UI layout, etc.
    code_signatures TEXT,           -- Function/class signatures
    todos_json TEXT,                -- JSON array of TODOs found
    ui_snapshot_json TEXT,          -- UI layout snapshot
    tags_json TEXT,                 -- JSON array of tags
    content_hash TEXT,              -- Hash of content for change detection
    last_indexed INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_object_memory_node ON object_memory(node_id);
CREATE INDEX IF NOT EXISTS idx_object_memory_hash ON object_memory(content_hash);

-- ============================================================
-- LAYER 3: EMBEDDING MEMORY (Semantic search)
-- ============================================================

CREATE TABLE IF NOT EXISTS embeddings (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    embedding_type TEXT NOT NULL,   -- 'summary', 'content', 'fact', 'ui_snapshot'
    vector_json TEXT NOT NULL,      -- JSON array of f32 values
    text_chunk TEXT,                -- Original text that was embedded
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_embeddings_node ON embeddings(node_id);
CREATE INDEX IF NOT EXISTS idx_embeddings_type ON embeddings(embedding_type);

-- ============================================================
-- CONVERSATION & SESSION MEMORY
-- ============================================================

CREATE TABLE IF NOT EXISTS conversations (
    id TEXT PRIMARY KEY,
    title TEXT,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    metadata_json TEXT
);

CREATE TABLE IF NOT EXISTS messages (
    id TEXT PRIMARY KEY,
    conversation_id TEXT NOT NULL,
    role TEXT NOT NULL,             -- 'user', 'assistant', 'system'
    content TEXT NOT NULL,
    tool_calls_json TEXT,           -- JSON array of tool calls
    tool_result_json TEXT,          -- JSON of tool result
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (conversation_id) REFERENCES conversations(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id);

-- ============================================================
-- FEEDBACK MEMORY / TRAINING SIGNALS
-- ============================================================

CREATE TABLE IF NOT EXISTS memory_feedback (
    id TEXT PRIMARY KEY,
    node_id TEXT NOT NULL,
    query_text TEXT,
    rating INTEGER NOT NULL,        -- -1 bad/useless, 0 neutral, +1 good/helpful
    reason TEXT,
    source TEXT,                    -- 'user', 'llm', 'system'
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    FOREIGN KEY (node_id) REFERENCES nodes(id) ON DELETE CASCADE
);

CREATE INDEX IF NOT EXISTS idx_memory_feedback_node ON memory_feedback(node_id);
CREATE INDEX IF NOT EXISTS idx_memory_feedback_created ON memory_feedback(created_at);

CREATE TABLE IF NOT EXISTS training_jobs (
    job_name TEXT PRIMARY KEY,
    last_run_at INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'idle',
    details_json TEXT
);

-- ============================================================
-- SKILLS REGISTRY
-- ============================================================

CREATE TABLE IF NOT EXISTS skills (
    id TEXT PRIMARY KEY,
    name TEXT NOT NULL UNIQUE,
    description TEXT,
    source_path TEXT NOT NULL,      -- Path to skill definition (e.g., .docu/skills/)
    config_json TEXT,               -- Skill-specific configuration
    enabled INTEGER NOT NULL DEFAULT 1,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

-- ============================================================
-- PLUGINS (GitHub-sourced skills, connectors, MCP servers)
-- ============================================================

CREATE TABLE IF NOT EXISTS plugins (
    id TEXT PRIMARY KEY,            -- UUID
    name TEXT NOT NULL,
    description TEXT,
    plugin_type TEXT NOT NULL,      -- 'skill', 'connector', 'mcp'
    repo_url TEXT NOT NULL,         -- GitHub repo URL
    owner TEXT NOT NULL,            -- GitHub owner
    repo TEXT NOT NULL,             -- GitHub repo name
    stars INTEGER NOT NULL DEFAULT 0,
    entry_point TEXT,               -- Detected executable / command for MCP
    install_path TEXT,              -- Local path after install (NULL if not installed)
    status TEXT NOT NULL DEFAULT 'available', -- 'available', 'installing', 'installed', 'error'
    enabled INTEGER NOT NULL DEFAULT 0,
    config_json TEXT,               -- User-supplied config (env vars, args, etc.)
    error_msg TEXT,
    installed_at INTEGER,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_plugins_type ON plugins(plugin_type);
CREATE INDEX IF NOT EXISTS idx_plugins_status ON plugins(status);
CREATE INDEX IF NOT EXISTS idx_plugins_enabled ON plugins(enabled);

-- ============================================================
-- INDEXING STATE
-- ============================================================

CREATE TABLE IF NOT EXISTS indexing_state (
    path TEXT PRIMARY KEY,
    last_indexed INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    content_hash TEXT,
    file_size INTEGER,
    status TEXT NOT NULL DEFAULT 'pending'  -- 'pending', 'indexed', 'error'
);

CREATE INDEX IF NOT EXISTS idx_indexing_state_status ON indexing_state(status);

-- ============================================================
-- CONTEXT PACKETS & TASK STATE
-- ============================================================

CREATE TABLE IF NOT EXISTS context_packets (
    id TEXT PRIMARY KEY,
    conversation_id TEXT,
    packet_json TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

CREATE TABLE IF NOT EXISTS tasks (
    conversation_id TEXT PRIMARY KEY,
    task_text TEXT,
    working_set_json TEXT,
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

CREATE INDEX IF NOT EXISTS idx_context_packets_conv ON context_packets(conversation_id);

-- ============================================================
-- USER FACTS (global key-value store — always injected)
-- ============================================================

CREATE TABLE IF NOT EXISTS user_facts (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL,
    created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    updated_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
);

-- ============================================================
-- TOOL AUDIT LOG
-- ============================================================

CREATE TABLE IF NOT EXISTS tool_audit (
    id TEXT PRIMARY KEY,
    timestamp INTEGER NOT NULL DEFAULT (strftime('%s', 'now')),
    tool_name TEXT NOT NULL,
    args_json TEXT,
    result_summary TEXT,
    conversation_id TEXT
);

CREATE INDEX IF NOT EXISTS idx_tool_audit_timestamp ON tool_audit(timestamp);
CREATE INDEX IF NOT EXISTS idx_tool_audit_tool ON tool_audit(tool_name);
"#;

pub type DbPool = Pool<SqliteConnectionManager>;
pub type DbConn = r2d2::PooledConnection<SqliteConnectionManager>;

#[derive(Clone)]
pub struct MemoryStorage {
    pool: DbPool,
    pub db_path: PathBuf,
}

impl MemoryStorage {
    pub fn new() -> anyhow::Result<Self> {
        let db_path = Self::get_db_path();

        // Ensure parent directory exists
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let manager = SqliteConnectionManager::file(&db_path).with_init(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL; PRAGMA foreign_keys=ON;",
            )
        });

        let pool = Pool::builder()
            .max_size(8)
            .build(manager)
            .map_err(|e| anyhow::anyhow!("Failed to build DB pool: {}", e))?;

        // Run schema + migrations on a single dedicated bootstrap connection.
        {
            let conn = pool.get().map_err(|e| anyhow::anyhow!("Pool get: {}", e))?;
            conn.execute_batch(SCHEMA)?;
            Self::run_migrations(&conn)?;
        }

        Ok(Self { pool, db_path })
    }

    pub fn new_in_memory() -> anyhow::Result<Self> {
        let manager = SqliteConnectionManager::memory()
            .with_init(|conn| conn.execute_batch("PRAGMA foreign_keys=ON;"));
        let pool = Pool::builder()
            .max_size(1)
            .build(manager)
            .map_err(|e| anyhow::anyhow!("Failed to build in-memory DB pool: {}", e))?;
        {
            let conn = pool.get().map_err(|e| anyhow::anyhow!("Pool get: {}", e))?;
            conn.execute_batch(SCHEMA)?;
            Self::run_migrations(&conn)?;
        }
        Ok(Self {
            pool,
            db_path: PathBuf::from(":memory:"),
        })
    }

    /// Incremental schema migrations that add columns / indexes introduced after
    /// the initial release. Each step is guarded so it is safe to run on any
    /// existing database (columns that already exist are left untouched).
    fn run_migrations(conn: &rusqlite::Connection) -> anyhow::Result<()> {
        // Phase 3.1 — embedding dimension column for cross-model mismatch detection.
        // Storing dim lets search_similar skip vectors from a different model without
        // relying on cosine_similarity silently returning 0.0 for len mismatches.
        if !Self::column_exists(conn, "embeddings", "dim")? {
            conn.execute_batch("ALTER TABLE embeddings ADD COLUMN dim INTEGER;")?;
        }
        // Phase 3.3 — session tracking on feedback rows so a user clicking 👍/👎
        // multiple times in the same conversation only counts once (last vote wins).
        if !Self::column_exists(conn, "memory_feedback", "user_session")? {
            conn.execute_batch("ALTER TABLE memory_feedback ADD COLUMN user_session TEXT;")?;
        }
        // Partial unique index: one feedback row per (node, session).
        // The WHERE clause makes it a no-op for NULL sessions (anonymous / system feedback).
        conn.execute_batch(
            "CREATE UNIQUE INDEX IF NOT EXISTS idx_feedback_node_session \
             ON memory_feedback(node_id, user_session) WHERE user_session IS NOT NULL;",
        )?;

        // v1.3.4 — `context_packets` is an orphaned table; nothing writes to it
        // any more since the sliding-window assembler replaced it. Drop it to
        // reclaim space and remove schema confusion.
        conn.execute_batch("DROP TABLE IF EXISTS context_packets;")?;

        // v1.3.5 — incremental VACUUM so deleted embedding/node rows don't cause
        // unbounded file growth. auto_vacuum=INCREMENTAL reclaims pages lazily
        // (a PRAGMA incremental_vacuum call, run periodically, does the actual
        // reclaim — this just sets the mode on first migration).
        conn.execute_batch("PRAGMA auto_vacuum = INCREMENTAL;")?;

        // v1.3.6 — real access counter so usage_score is based on actual retrieval
        // frequency rather than tags.len() (which was a noisy proxy).
        if !Self::column_exists(conn, "object_memory", "access_count")? {
            conn.execute_batch(
                "ALTER TABLE object_memory ADD COLUMN access_count INTEGER NOT NULL DEFAULT 0;",
            )?;
        }

        // v1.3.8 — compound and covering indexes for hot query paths.
        // embeddings(node_id, embedding_type): speeds up per-node type lookups.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_embeddings_node_type ON embeddings(node_id, embedding_type);",
        )?;
        // nodes(updated_at): eviction and recency queries scan this column.
        conn.execute_batch("CREATE INDEX IF NOT EXISTS idx_nodes_updated ON nodes(updated_at);")?;
        // memory_feedback(node_id, created_at): confidence_adjustment sorts by age per node.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_feedback_node_created ON memory_feedback(node_id, created_at);",
        )?;
        // object_memory(access_count): ordering by access frequency.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_object_memory_access ON object_memory(access_count DESC);",
        )?;
        // object_memory(last_indexed): "resume last session" queries.
        conn.execute_batch(
            "CREATE INDEX IF NOT EXISTS idx_object_memory_last_indexed ON object_memory(last_indexed DESC);",
        )?;

        // v1.3.7 — user_fact_history tracks old values before overwrite so we can
        // detect value drift over time and surface conflicts in the distiller.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS user_fact_history (
                id TEXT PRIMARY KEY,
                key TEXT NOT NULL,
                old_value TEXT NOT NULL,
                new_value TEXT NOT NULL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s', 'now'))
            );
            CREATE INDEX IF NOT EXISTS idx_ufh_key ON user_fact_history(key);",
        )?;

        Ok(())
    }

    fn column_exists(
        conn: &rusqlite::Connection,
        table: &str,
        column: &str,
    ) -> anyhow::Result<bool> {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({})", table))?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let col_name: String = row.get(1)?;
            if col_name == column {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn get_db_path() -> PathBuf {
        let data_dir = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Rook");
        // Use a separate db in dev mode so test data never pollutes the prod db
        let filename = if std::env::var("ROOK_DEV").is_ok() {
            "memory_dev.db"
        } else {
            "memory.db"
        };
        data_dir.join(filename)
    }

    /// Acquire a pooled SQLite connection. Each call returns an independent
    /// connection from the pool so that concurrent callers never block each other.
    pub fn get_connection(&self) -> std::result::Result<DbConn, String> {
        self.pool.get().map_err(|e| e.to_string())
    }

    // same thing, but returns a rusqlite::Error so `?` works without a parade of map_err calls.
    pub fn sql_conn(&self) -> rusqlite::Result<DbConn> {
        self.pool.get().map_err(|e| {
            rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some(format!("connection pool exhausted: {}", e)),
            )
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_in_memory_creates_all_tables() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        // context_packets was dropped in the v1.3.5 migration — excluded here.
        let tables = [
            "nodes",
            "edges",
            "object_memory",
            "embeddings",
            "conversations",
            "messages",
            "memory_feedback",
            "training_jobs",
            "skills",
            "indexing_state",
            "tasks",
        ];
        for table in &tables {
            let count: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='{table}'"
                    ),
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(count, 1, "table '{table}' should exist");
        }
    }

    #[test]
    fn get_connection_lock_released_after_scope() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        {
            let conn = storage.get_connection().unwrap();
            let n: i64 = conn
                .query_row("SELECT COUNT(*) FROM nodes", [], |r| r.get(0))
                .unwrap();
            assert_eq!(n, 0);
        } // lock released here
          // second acquisition should succeed
        let conn2 = storage.get_connection().unwrap();
        let n2: i64 = conn2
            .query_row("SELECT COUNT(*) FROM edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(n2, 0);
    }

    #[test]
    fn user_facts_insert_and_retrieve() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        conn.execute(
            "INSERT INTO user_facts (key, value) VALUES ('test_key', 'test_value')",
            [],
        )
        .unwrap();
        let val: String = conn
            .query_row(
                "SELECT value FROM user_facts WHERE key = 'test_key'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(val, "test_value");
    }

    #[test]
    fn user_facts_upsert_updates_value() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        conn.execute("INSERT INTO user_facts (key, value) VALUES ('k', 'v1')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO user_facts (key, value, updated_at) VALUES ('k', 'v2', strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            [],
        ).unwrap();
        let val: String = conn
            .query_row("SELECT value FROM user_facts WHERE key = 'k'", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(val, "v2");
    }

    #[test]
    fn conversations_created_and_messages_inserted() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        conn.execute(
            "INSERT INTO conversations (id, title) VALUES ('conv-1', 'Test conv')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content) VALUES ('msg-1', 'conv-1', 'user', 'hello')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO messages (id, conversation_id, role, content) VALUES ('msg-2', 'conv-1', 'assistant', 'world')",
            [],
        ).unwrap();
        let msg_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = 'conv-1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(msg_count, 2);
    }

    #[test]
    fn tool_audit_log_inserts_and_queries() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        conn.execute(
            "INSERT INTO tool_audit (id, tool_name, args_json, result_summary) VALUES ('a1', 'read_file', '{\"path\":\"x\"}', 'ok')",
            [],
        ).unwrap();
        conn.execute(
            "INSERT INTO tool_audit (id, tool_name, args_json, result_summary) VALUES ('a2', 'write_file', '{\"path\":\"y\"}', 'ok')",
            [],
        ).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM tool_audit", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let name: String = conn
            .query_row(
                "SELECT tool_name FROM tool_audit ORDER BY timestamp DESC LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // Both rows inserted in same second; either order is valid — just check it's a known name
        assert!(name == "read_file" || name == "write_file");
    }

    #[test]
    fn node_pinned_flag_via_user_facts() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        // pin a node
        conn.execute(
            "INSERT INTO user_facts (key, value) VALUES ('node_pinned:n1', 'true')",
            [],
        )
        .unwrap();
        let pinned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM user_facts WHERE key LIKE 'node_pinned:%' AND value = 'true'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pinned, 1);
        // unpin
        conn.execute(
            "UPDATE user_facts SET value = 'false' WHERE key = 'node_pinned:n1'",
            [],
        )
        .unwrap();
        let still_pinned: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM user_facts WHERE key LIKE 'node_pinned:%' AND value = 'true'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(still_pinned, 0);
    }

    #[test]
    fn memory_feedback_unique_index_per_session() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        // Insert a node so FK holds
        conn.execute(
            "INSERT INTO nodes (id, node_type, title) VALUES ('n1', 'concept', 'Test')",
            [],
        )
        .unwrap();
        // First vote
        conn.execute(
            "INSERT INTO memory_feedback (id, node_id, rating, source, user_session)
             VALUES ('f1', 'n1', 1, 'user', 'sess-abc')",
            [],
        )
        .unwrap();
        // Second vote same session — should fail with UNIQUE violation
        let result = conn.execute(
            "INSERT INTO memory_feedback (id, node_id, rating, source, user_session)
             VALUES ('f2', 'n1', -1, 'user', 'sess-abc')",
            [],
        );
        assert!(
            result.is_err(),
            "duplicate (node, session) should be rejected"
        );
    }

    #[test]
    fn conversations_cascade_delete_messages() {
        let storage = MemoryStorage::new_in_memory().unwrap();
        let conn = storage.get_connection().unwrap();
        conn.execute(
            "INSERT INTO conversations (id, title) VALUES ('c1', 'T')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO messages (id, conversation_id, role, content) VALUES ('m1','c1','user','hi')", []).unwrap();
        conn.execute("DELETE FROM conversations WHERE id = 'c1'", [])
            .unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM messages WHERE conversation_id = 'c1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 0, "cascade delete should remove child messages");
    }
}
