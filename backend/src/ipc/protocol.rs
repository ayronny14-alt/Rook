use serde::{Deserialize, Serialize};
use uuid::Uuid;

// REQUEST TYPES (Frontend -> Backend)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IPCRequest {
    Chat {
        id: String,
        conversation_id: Option<String>,
        message: String,
        agent_mode: Option<String>,
        model: Option<String>,
        #[serde(default)]
        images: Option<Vec<String>>,
        /// Optional system prompt override from the user (Settings → Chat).
        #[serde(default)]
        system_prompt: Option<String>,
        /// Per-request temperature override (0.0-2.0).
        #[serde(default)]
        temperature: Option<f32>,
        /// Per-request max_tokens override.
        #[serde(default)]
        max_tokens: Option<u32>,
    },
    CreateNode {
        id: String,
        node_type: String,
        title: String,
        metadata: Option<serde_json::Value>,
    },
    QueryMemory {
        id: String,
        query: String,
        node_type: Option<String>,
        limit: Option<usize>,
    },
    GetNode {
        id: String,
        node_id: String,
    },
    GetConnectedNodes {
        id: String,
        node_id: String,
        relationship: Option<String>,
    },
    SearchEmbeddings {
        id: String,
        query: String,
        limit: Option<usize>,
    },
    SubmitMemoryFeedback {
        id: String,
        node_id: String,
        query: Option<String>,
        rating: String,
        reason: Option<String>,
        source: Option<String>,
        /// Conversation / session id for dedup: last vote per (node, session) wins.
        #[serde(default)]
        session_id: Option<String>,
    },
    /// Ask the backend for memory system capabilities (GNN status, node/edge counts).
    GetMemoryCapabilities {
        id: String,
    },
    ReadFile {
        id: String,
        path: String,
        conversation_id: Option<String>,
    },
    WriteFile {
        id: String,
        path: String,
        content: String,
    },
    SpawnBrowser {
        id: String,
        headless: bool,
    },
    SpawnCdpBrowser {
        id: String,
        headless: bool,
    },
    CdpNavigate {
        id: String,
        url: String,
    },
    CdpClick {
        id: String,
        selector: String,
    },
    CdpType {
        id: String,
        selector: String,
        text: String,
    },
    CdpEvaluate {
        id: String,
        js: String,
    },
    Screenshot {
        id: String,
        selector: Option<String>,
        full: Option<bool>,
    },
    KillBrowser {
        id: String,
    },
    BrowserDebuggingUrl {
        id: String,
    },
    WebSearch {
        id: String,
        query: String,
        conversation_id: Option<String>,
    },
    ExecuteSkill {
        id: String,
        skill_name: String,
        args: serde_json::Value,
    },
    ListSkills {
        id: String,
    },
    GetPreviews {
        id: String,
    },
    CreatePreview {
        id: String,
        name: String,
        content: String,
    },
    HealthCheck {
        id: String,
    },
    /// Cancel a pending in-flight request by its original request id.
    CancelRequest {
        id: String,
        target_id: String,
    },
    /// Approve or reject a set of pending tool calls returned from a Code-mode response.
    ApprovePendingTools {
        id: String,
        conversation_id: String,
        approved: bool,
    },
    /// Git awareness tools.
    GitStatus {
        id: String,
        path: Option<String>,
    },
    GitLog {
        id: String,
        n: Option<usize>,
    },
    GitDiff {
        id: String,
        file_path: Option<String>,
    },
    GitBranch {
        id: String,
    },
    GitCommit {
        id: String,
        message: String,
        files: Vec<String>,
    },
    /// Search GitHub for plugins matching the free-form query.
    SearchPlugins {
        id: String,
        query: String,
    },
    /// List all plugins (optionally filtered by type: "skill"|"connector"|"mcp").
    ListPlugins {
        id: String,
        plugin_type: Option<String>,
    },
    /// Install a plugin by its registry id.  If the plugin is not yet in the local
    /// registry (e.g. it came from the browse list, not a search), the caller may
    /// supply inline metadata so the backend can upsert it before installing.
    InstallPlugin {
        id: String,
        plugin_id: String,
        /// Inline plugin name - required when the plugin is not yet in the registry.
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        description: Option<String>,
        /// "mcp" | "skill" | "connector"
        #[serde(default)]
        plugin_type: Option<String>,
        #[serde(default)]
        repo_url: Option<String>,
        #[serde(default)]
        owner: Option<String>,
        #[serde(default)]
        repo: Option<String>,
        #[serde(default)]
        stars: Option<i64>,
    },
    /// Enable or disable an installed plugin.
    SetPluginEnabled {
        id: String,
        plugin_id: String,
        enabled: bool,
    },
    /// Uninstall a plugin (delete local files, mark available).
    UninstallPlugin {
        id: String,
        plugin_id: String,
    },
    /// Start an MCP server process.
    StartMcp {
        id: String,
        plugin_id: String,
    },
    /// Stop a running MCP server process.
    StopMcp {
        id: String,
        plugin_id: String,
    },
    /// Ask the backend to launch (or verify) the local Gemma model process.
    LaunchGemma {
        id: String,
        /// The Ollama model tag to pull and/or launch, e.g. "gemma4:latest".
        model: String,
    },
    /// Push updated LLM settings from the Settings page to the running backend.
    UpdateConfig {
        id: String,
        base_url: String,
        api_key: String,
        model: String,
    },
    /// Electron signals graceful shutdown; backend should flush WAL and exit.
    GracefulShutdown {
        id: String,
    },
    // scheduler
    ListScheduledTasks {
        id: String,
    },
    CreateScheduledTask {
        id: String,
        name: String,
        cadence: String,
        prompt: String,
        #[serde(default)]
        output_channel: Option<String>,
    },
    ApproveScheduledTask {
        id: String,
        task_id: String,
    },
    CancelScheduledTask {
        id: String,
        task_id: String,
    },
    PauseScheduledTask {
        id: String,
        task_id: String,
    },
    ResumeScheduledTask {
        id: String,
        task_id: String,
    },
    /// List all conversations stored in SQLite.
    GetConversations {
        id: String,
    },
    /// Full-text search across conversation titles and message content.
    SearchConversations {
        id: String,
        query: String,
    },
    /// Regenerate the last assistant message in a conversation.
    RegenerateLastMessage {
        id: String,
        conversation_id: String,
        model: Option<String>,
    },
    /// Retrieve a single conversation's messages.
    GetConversationMessages {
        id: String,
        conversation_id: String,
    },
    /// Pin or unpin a conversation in the sidebar.
    PinConversation {
        id: String,
        conversation_id: String,
        pinned: bool,
    },
    /// Index a local directory for semantic (neural) file search.
    IndexDirectory {
        id: String,
        path: String,
    },
    /// Delete a conversation and all its messages.
    DeleteConversation {
        id: String,
        conversation_id: String,
    },
    /// Rename a conversation.
    RenameConversation {
        id: String,
        conversation_id: String,
        title: String,
    },
    /// Pin or unpin a memory node (exempt from eviction).
    SetNodePinned {
        id: String,
        node_id: String,
        pinned: bool,
    },
    /// Delete a memory node and cascade edges.
    DeleteNode {
        id: String,
        node_id: String,
    },
    /// Update a node's summary (user override).
    UpdateNodeSummary {
        id: String,
        node_id: String,
        summary: String,
    },
    /// Get memory health stats (node count, evictions, avg confidence, etc.).
    GetMemoryHealth {
        id: String,
    },
    /// Export the full memory graph as JSON (no embeddings).
    ExportMemory {
        id: String,
    },
    /// Import a previously exported memory JSON.
    ImportMemory {
        id: String,
        data: serde_json::Value,
        conflict: String, // "overwrite" | "keep" | "merge"
    },
    /// Log a tool execution for the audit trail.
    LogToolAudit {
        id: String,
        tool_name: String,
        args_json: String,
        result_summary: String,
        conversation_id: Option<String>,
    },
    /// Retrieve the tool audit log (most recent first).
    GetToolAuditLog {
        id: String,
        limit: Option<usize>,
    },
    /// Get or set a user_facts key-value pair.
    GetUserFact {
        id: String,
        key: String,
    },
    SetUserFact {
        id: String,
        key: String,
        value: String,
    },
    /// Fetch the current session todo list (from Team mode `todo_write` tool).
    GetSessionTodos {
        id: String,
        conversation_id: String,
    },
    /// Retrieve aggregated stats for the admin dashboard.
    GetAppStats {
        id: String,
    },
}

// RESPONSE TYPES (Backend -> Frontend)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IPCResponse {
    ScheduledTaskList {
        id: String,
        tasks: Vec<serde_json::Value>,
    },
    ScheduledTaskAction {
        id: String,
        task_id: String,
        action: String,
        success: bool,
        message: String,
    },
    Chat {
        id: String,
        conversation_id: String,
        content: String,
        tool_calls: Option<Vec<ToolCallResult>>,
        context_packet: Option<serde_json::Value>,
        /// Token usage from the LLM for this turn.
        usage: Option<TokenUsage>,
    },
    NodeCreated {
        id: String,
        node: serde_json::Value,
    },
    MemoryResults {
        id: String,
        nodes: Vec<serde_json::Value>,
    },
    NodeDetails {
        id: String,
        node: serde_json::Value,
        object_memory: Option<serde_json::Value>,
    },
    ConnectedNodes {
        id: String,
        nodes: Vec<serde_json::Value>,
        edges: Vec<serde_json::Value>,
    },
    EmbeddingResults {
        id: String,
        results: Vec<serde_json::Value>,
    },
    FeedbackRecorded {
        id: String,
        success: bool,
        details: serde_json::Value,
    },
    FileContent {
        id: String,
        path: String,
        content: String,
    },
    FileWritten {
        id: String,
        path: String,
        success: bool,
    },
    WebSearchResults {
        id: String,
        results: Vec<serde_json::Value>,
    },
    SpawnBrowserResult {
        id: String,
        debugging_url: String,
    },
    SpawnCdpBrowserResult {
        id: String,
        debugging_url: String,
    },
    CdpNavigateResult {
        id: String,
        content: String,
    },
    CdpActionResult {
        id: String,
        success: bool,
        details: Option<serde_json::Value>,
    },
    ScreenshotResult {
        id: String,
        image_base64: String,
    },
    BrowserDebuggingUrl {
        id: String,
        url: Option<String>,
    },
    SkillExecuted {
        id: String,
        result: serde_json::Value,
    },
    SkillsList {
        id: String,
        skills: Vec<serde_json::Value>,
    },
    Previews {
        id: String,
        previews: Vec<serde_json::Value>,
    },
    PreviewCreated {
        id: String,
        name: String,
        path: String,
    },
    HealthCheck {
        id: String,
        status: String,
        /// True when the LLM client has no API key and is returning deterministic
        /// mock responses. UI should render a clearly visible "MOCK" badge so
        /// users don't misread placeholder output as real model behavior.
        #[serde(default)]
        mock_mode: bool,
        /// Active model name (informational, lets the UI confirm what's running).
        #[serde(default)]
        model: String,
    },
    Error {
        id: String,
        message: String,
    },
    /// Streaming token chunk - sent for each partial text fragment before `ChatDone`.
    ChatChunk {
        id: String,
        token: String,
    },
    /// Reasoning/thinking block from an extended-thinking model (e.g. Claude with thinking
    /// enabled, or OpenRouter's `reasoning` field). Emitted BEFORE content chunks so the
    /// UI can render a collapsible "Thinking…" pane above the answer.
    ChatThinking {
        id: String,
        thinking: String,
    },
    /// Curated memory context that was injected into this chat's prompt.
    /// Emitted BEFORE the first `ChatChunk` so the UI can render the memory
    /// panel while the model is still thinking. `nodes` is the raw
    /// `RankedContextNode` list from the sliding-window / context curator.
    ContextCurated {
        id: String,
        conversation_id: String,
        nodes: serde_json::Value,
    },
    /// Emitted alongside ContextCurated with the current user_facts rows so
    /// the UI can show the global user profile in the memory panel.
    UserFactsLoaded {
        id: String,
        conversation_id: String,
        /// Array of {key, value} objects from the user_facts table.
        facts: Vec<serde_json::Value>,
    },
    /// Terminal marker of a streaming chat response.
    ChatDone {
        id: String,
        conversation_id: String,
        usage: Option<TokenUsage>,
    },
    /// Cancelled acknowledgement.
    Cancelled {
        id: String,
        target_id: String,
    },
    /// Code-mode tool calls awaiting diff review + approval from the user.
    PendingToolApproval {
        id: String,
        conversation_id: String,
        /// Human-readable diff for each proposed code_edit (empty for terminal calls).
        diffs: Vec<PendingEditDiff>,
    },
    /// Git command results.
    GitResult {
        id: String,
        output: String,
    },
    /// Outcome of a LaunchGemma request.
    GemmaLaunched {
        id: String,
        /// true = process is up and responding; false = not enough RAM or timed out.
        success: bool,
        message: String,
    },
    /// Outcome of an UpdateConfig request.
    ConfigUpdated {
        id: String,
        success: bool,
    },
    /// Memory system capability snapshot.  Emitted after HealthCheck at startup
    /// and in response to GetMemoryCapabilities.  The UI uses `gnn_available` to
    /// show an accurate GNN status chip in the memory panel header.
    MemoryCapabilities {
        id: String,
        /// True when Python + torch + the GNN training script are all present.
        gnn_available: bool,
        /// Unix timestamp of the last successful GNN training run (if any).
        last_trained_at: Option<i64>,
        node_count: i64,
        edge_count: i64,
        embedding_count: i64,
    },
    /// Auto-generated title for a conversation.
    ConversationTitled {
        id: String,
        conversation_id: String,
        title: String,
    },
    /// List of conversation summaries.
    ConversationList {
        id: String,
        conversations: Vec<serde_json::Value>,
    },
    /// Messages within a conversation.
    ConversationMessages {
        id: String,
        conversation_id: String,
        messages: Vec<serde_json::Value>,
    },
    /// Acknowledgement of a pin/unpin operation.
    ConversationPinned {
        id: String,
        conversation_id: String,
        pinned: bool,
    },
    /// Result of an IndexDirectory operation.
    DirectoryIndexed {
        id: String,
        path: String,
        files_indexed: usize,
    },
    /// Acknowledgement of conversation deletion.
    ConversationDeleted {
        id: String,
        conversation_id: String,
    },
    /// Acknowledgement of conversation rename.
    ConversationRenamed {
        id: String,
        conversation_id: String,
        title: String,
    },
    /// Memory health stats.
    MemoryHealth {
        id: String,
        node_count: i64,
        edge_count: i64,
        embedding_count: i64,
        pinned_count: i64,
        avg_confidence: f64,
        db_size_bytes: u64,
        gnn_available: bool,
        last_trained_at: Option<i64>,
    },
    /// Full memory export payload.
    MemoryExport {
        id: String,
        data: serde_json::Value,
    },
    /// Acknowledgement of memory import.
    MemoryImportResult {
        id: String,
        imported: usize,
        skipped: usize,
        errors: usize,
    },
    /// Tool audit log entries.
    ToolAuditLog {
        id: String,
        entries: Vec<serde_json::Value>,
    },
    /// Single user_fact value.
    UserFact {
        id: String,
        key: String,
        value: Option<String>,
    },
    /// Current session todo list for a conversation.
    SessionTodos {
        id: String,
        conversation_id: String,
        todos: Vec<serde_json::Value>,
    },
    /// Application-wide stats for the admin dashboard.
    AppStats {
        id: String,
        conversation_count: i64,
        message_count: i64,
        user_message_count: i64,
        conversations_today: i64,
        messages_today: i64,
        conversations_this_week: i64,
        memory_node_count: i64,
        memory_edge_count: i64,
        embedding_count: i64,
        plugin_installed_count: i64,
        plugin_enabled_count: i64,
        tool_calls_count: i64,
        db_size_bytes: u64,
        started_at: i64,
        oldest_conversation_at: Option<i64>,
    },
    /// Generic success acknowledgement.
    Ok {
        id: String,
    },
    /// List of plugins from a search or catalog query.
    PluginList {
        id: String,
        plugins: Vec<PluginInfo>,
    },
    /// Confirmation of install/uninstall/enable/start/stop.
    PluginAction {
        id: String,
        plugin_id: String,
        action: String,
        success: bool,
        message: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub name: String,
    pub args: serde_json::Value,
    pub result: String,
}

/// Serialisable mirror of `plugins::registry::Plugin` for the IPC layer.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginInfo {
    pub id: String,
    pub name: String,
    pub description: Option<String>,
    pub plugin_type: String,
    pub repo_url: String,
    pub owner: String,
    pub repo: String,
    pub stars: i64,
    pub entry_point: Option<String>,
    pub install_path: Option<String>,
    pub status: String,
    pub enabled: bool,
    #[serde(default)]
    pub running: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TokenUsage {
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub total_tokens: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PendingEditDiff {
    pub tool_name: String,
    pub path: String,
    pub diff: String,
    pub args: serde_json::Value,
}

// STREAMING EVENTS (for chat streaming)
// ============================================================

#[allow(dead_code)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum StreamEvent {
    Token {
        id: String,
        token: String,
    },
    ToolCall {
        id: String,
        name: String,
        args: serde_json::Value,
    },
    ToolResult {
        id: String,
        name: String,
        result: String,
    },
    Done {
        id: String,
    },
    Error {
        id: String,
        message: String,
    },
}

#[allow(dead_code)]
impl IPCRequest {
    pub fn generate_id() -> String {
        Uuid::new_v4().to_string()
    }
}
