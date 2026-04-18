use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EmbeddingProvider {
    Mock,
    OpenAiCompatible,
    Ollama,
}

impl EmbeddingProvider {
    pub fn from_env_value(value: &str) -> Self {
        match value.trim().to_ascii_lowercase().as_str() {
            "mock" => Self::Mock,
            "openai" | "openai-compatible" | "openai_compatible" | "remote" => {
                Self::OpenAiCompatible
            }
            "ollama" | "local" | "local_ollama" => Self::Ollama,
            _ => Self::Ollama,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LLMConfig {
    pub base_url: String,
    pub api_key: String,
    pub model: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub embedding_provider: EmbeddingProvider,
    pub embedding_base_url: String,
    pub embedding_model: String,
    pub embedding_api_key: String,
}

impl LLMConfig {
    pub fn from_env() -> Self {
        let base_url = std::env::var("ROOK_LLM_BASE_URL")
            .unwrap_or_else(|_| "https://api.openai.com/v1".to_string());
        let api_key = std::env::var("ROOK_LLM_API_KEY").unwrap_or_else(|_| String::new());

        let embedding_provider = std::env::var("ROOK_EMBEDDING_PROVIDER")
            .map(|v| EmbeddingProvider::from_env_value(&v))
            .unwrap_or_else(|_| {
                if api_key.trim().is_empty() {
                    EmbeddingProvider::Ollama
                } else {
                    EmbeddingProvider::OpenAiCompatible
                }
            });

        let embedding_base_url =
            std::env::var("ROOK_EMBEDDING_BASE_URL").unwrap_or_else(|_| match embedding_provider {
                EmbeddingProvider::Ollama => "http://127.0.0.1:11434".to_string(),
                _ => base_url.clone(),
            });

        let embedding_model =
            std::env::var("ROOK_EMBEDDING_MODEL").unwrap_or_else(|_| match embedding_provider {
                EmbeddingProvider::Ollama => "nomic-embed-text".to_string(),
                _ => "text-embedding-3-small".to_string(),
            });

        let embedding_api_key =
            std::env::var("ROOK_EMBEDDING_API_KEY").unwrap_or_else(|_| api_key.clone());

        // Emit startup warnings for common misconfigurations
        if api_key.trim().is_empty() {
            if base_url.contains("openai.com") || base_url.contains("anthropic.com") {
                tracing::warn!(
                    "ROOK_LLM_API_KEY is not set but base_url points to a remote provider ({}). LLM calls will likely fail.",
                    base_url
                );
            } else {
                tracing::info!(
                    "ROOK_LLM_API_KEY is not set; assuming local/mock endpoint at {}",
                    base_url
                );
            }
        }
        if embedding_api_key.trim().is_empty()
            && embedding_provider == EmbeddingProvider::OpenAiCompatible
        {
            tracing::warn!(
                "ROOK_EMBEDDING_API_KEY is not set but embedding_provider is openai-compatible. Embedding calls may fail."
            );
        }

        Self {
            base_url,
            api_key,
            model: std::env::var("ROOK_LLM_MODEL").unwrap_or_else(|_| "openai-fast".to_string()),
            max_tokens: std::env::var("ROOK_LLM_MAX_TOKENS")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(8192),
            temperature: std::env::var("ROOK_LLM_TEMPERATURE")
                .ok()
                .and_then(|v| v.parse().ok())
                .unwrap_or(0.7),
            embedding_provider,
            embedding_base_url,
            embedding_model,
            embedding_api_key,
        }
    }
}

// A message in the conversation history, sent to the LLM API.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct Message {
    pub role: String,
    /// Plain-text content used for regular messages and as the human-readable
    /// portion when `content_blocks` is also present.
    #[serde(default)]
    pub content: String,
    /// Raw content-block array from an Anthropic-format extended-thinking response.
    /// When present this is serialized *in place of* the plain `content` string so
    /// that thought_signatures round-trip correctly (Vertex AI requirement).
    #[serde(skip)]
    pub content_blocks: Option<serde_json::Value>,
    /// Populated on assistant messages that invoke tools.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ToolCallDefinition>>,
    /// Required on role:"tool" messages — must match the tool_call id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    /// Raw reasoning payload returned by the model on the previous turn.
    /// Gemini 3 / Vertex thought-signature round-trip requires this to be
    /// echoed back on every subsequent assistant turn that has tool calls,
    /// otherwise the provider 400s. Providers that don't use it ignore the
    /// field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<serde_json::Value>,
}

impl serde::Serialize for Message {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut len = 2; // role + content always present
        if self.tool_calls.is_some() {
            len += 1;
        }
        if self.tool_call_id.is_some() {
            len += 1;
        }
        if self.reasoning.is_some() {
            len += 1;
        }

        let mut map = serializer.serialize_map(Some(len))?;
        map.serialize_entry("role", &self.role)?;
        match &self.content_blocks {
            // replay raw blocks (thinking + text + tool_use) so vertex ai
            // extended-thinking thought_signatures are preserved.
            Some(blocks) => map.serialize_entry("content", blocks)?,
            None => map.serialize_entry("content", &self.content)?,
        }
        if let Some(tc) = &self.tool_calls {
            map.serialize_entry("tool_calls", tc)?;
        }
        if let Some(id) = &self.tool_call_id {
            map.serialize_entry("tool_call_id", id)?;
        }
        if let Some(r) = &self.reasoning {
            map.serialize_entry("reasoning", r)?;
        }
        map.end()
    }
}

impl Message {
    /// Convenience: plain text message (system / user / assistant without tools).
    pub fn text(role: &str, content: impl Into<String>) -> Self {
        Self {
            role: role.to_string(),
            content: content.into(),
            ..Default::default()
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCallDefinition {
    pub id: String,
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    #[serde(rename = "type")]
    pub tool_type: String,
    pub function: FunctionDefinition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolDefinition>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
    /// Stable per-conversation id. OpenAI uses this for prompt-cache sharding;
    /// other providers ignore the field. Setting it per conversation lets the
    /// static system+tools portion reuse the cache across all turns.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub user: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingRequest {
    pub model: String,
    pub input: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingResponse {
    pub data: Vec<EmbeddingData>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EmbeddingData {
    pub embedding: Vec<f32>,
    pub index: usize,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChatCompletionResponse {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub choices: Vec<Choice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Choice {
    pub message: ResponseMessage,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResponseMessage {
    #[serde(default)]
    pub role: String,
    /// Content from the LLM — either a JSON string (standard OpenAI format) or a
    /// JSON array of content blocks (Anthropic-format extended thinking from Vertex AI).
    /// Use [`text_content`] to get the human-readable text portion.
    pub content: Option<serde_json::Value>,
    pub tool_calls: Option<Vec<ToolCallDefinition>>,
}

impl ResponseMessage {
    /// Extract the text portion of the content, handling both:
    /// - OpenAI string format:  `"content": "text"`
    /// - Anthropic block array: `"content": [{"type":"text","text":"..."},...]`
    pub fn text_content(&self) -> Option<String> {
        match &self.content {
            Some(serde_json::Value::String(s)) if !s.is_empty() => Some(s.clone()),
            Some(serde_json::Value::Array(blocks)) => {
                let text: String = blocks
                    .iter()
                    .filter_map(|b| {
                        if b.get("type").and_then(|t| t.as_str()) == Some("text") {
                            b.get("text").and_then(|t| t.as_str()).map(String::from)
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("");
                if text.is_empty() {
                    None
                } else {
                    Some(text)
                }
            }
            _ => None,
        }
    }

    /// Extract thinking/reasoning blocks from an Anthropic extended-thinking response.
    /// Returns all `{"type":"thinking","thinking":"..."}` block contents concatenated.
    /// Also handles `{"type":"reasoning","reasoning":"..."}` for OpenRouter-style responses.
    pub fn thinking_content(&self) -> Option<String> {
        match &self.content {
            Some(serde_json::Value::Array(blocks)) => {
                let thought: String = blocks
                    .iter()
                    .filter_map(|b| {
                        let block_type = b.get("type").and_then(|t| t.as_str());
                        match block_type {
                            Some("thinking") => {
                                b.get("thinking").and_then(|t| t.as_str()).map(String::from)
                            }
                            Some("reasoning") => b
                                .get("reasoning")
                                .and_then(|t| t.as_str())
                                .map(String::from),
                            _ => None,
                        }
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                if thought.is_empty() {
                    None
                } else {
                    Some(thought)
                }
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Usage {
    #[serde(default)]
    pub prompt_tokens: u32,
    #[serde(default)]
    pub completion_tokens: u32,
    #[serde(default)]
    pub total_tokens: u32,
}

/// One chunk from an SSE `stream: true` response.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamChunkResponse {
    #[serde(default)]
    pub choices: Vec<StreamChoice>,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamChoice {
    pub delta: Option<StreamDelta>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamDelta {
    pub content: Option<String>,
    pub role: Option<String>,
    /// OpenRouter / Anthropic streaming thinking content (delta)
    #[serde(default)]
    pub reasoning: Option<String>,
    /// Some providers use `reasoning_content` instead of `reasoning`
    #[serde(default)]
    pub reasoning_content: Option<String>,
    /// Streaming tool call deltas — arguments arrive as incremental string fragments
    #[serde(default)]
    pub tool_calls: Option<Vec<StreamToolCallDelta>>,
}

/// A single tool call delta from an SSE stream.
#[derive(Debug, Clone, Deserialize)]
pub struct StreamToolCallDelta {
    pub index: usize,
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub function: Option<StreamFunctionDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct StreamFunctionDelta {
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
}

impl StreamDelta {
    /// Returns any thinking/reasoning text from this delta chunk, checking
    /// both `reasoning` and `reasoning_content` fields.
    pub fn thinking_delta(&self) -> Option<&str> {
        self.reasoning
            .as_deref()
            .or(self.reasoning_content.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::EmbeddingProvider;

    #[test]
    fn parses_embedding_provider_aliases() {
        assert_eq!(
            EmbeddingProvider::from_env_value("ollama"),
            EmbeddingProvider::Ollama
        );
        assert_eq!(
            EmbeddingProvider::from_env_value("local"),
            EmbeddingProvider::Ollama
        );
        assert_eq!(
            EmbeddingProvider::from_env_value("mock"),
            EmbeddingProvider::Mock
        );
        assert_eq!(
            EmbeddingProvider::from_env_value("openai-compatible"),
            EmbeddingProvider::OpenAiCompatible
        );
    }
}
