use anyhow::{Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tracing::{info, warn};

use crate::llm::types::{
    ChatCompletionRequest, ChatCompletionResponse, Choice, EmbeddingProvider, EmbeddingRequest,
    EmbeddingResponse, LLMConfig, Message, ResponseMessage, Usage,
};

#[derive(Debug, Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a str,
}

#[derive(Debug, Serialize)]
struct OllamaEmbeddingsCompatRequest<'a> {
    model: &'a str,
    prompt: &'a str,
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    #[serde(default)]
    embeddings: Vec<Vec<f32>>,
    #[serde(default)]
    embedding: Vec<f32>,
}

#[derive(Clone)]
pub struct LLMClient {
    config: Arc<LLMConfig>,
    http_client: Arc<Client>,
}

/// Model-name prefixes that indicate a local Ollama-served model.
const LOCAL_MODEL_PREFIXES: &[&str] = &[
    "llama",
    "mistral",
    "phi",
    "qwen",
    "deepseek",
    "codellama",
    "solar",
];

// a colon in the name means "definitely an ollama tag, stop asking."
fn is_local_model(model: &str) -> bool {
    let m = model.to_ascii_lowercase();
    if m.contains(':') {
        return true;
    }
    LOCAL_MODEL_PREFIXES.iter().any(|p| m.starts_with(p))
}

fn ollama_base_url() -> String {
    let host = std::env::var("ROOK_OLLAMA_HOST").unwrap_or_else(|_| "127.0.0.1".to_string());
    let port = std::env::var("ROOK_OLLAMA_PORT").unwrap_or_else(|_| "11434".to_string());
    format!("http://{}:{}/v1", host, port)
}

// Stable per-install id for the `user` field on chat requests. OpenAI uses this
// to shard prompt caching; setting a consistent value means the cached system +
// tools portion is reused across every turn in this install. The value is a
// short hash of the api_key (so it differs per user but doesn't leak the key).
fn install_user_id(api_key: &str) -> Option<String> {
    if api_key.trim().is_empty() {
        return None;
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(api_key.as_bytes());
    let hex: String = digest[..8].iter().map(|b| format!("{:02x}", b)).collect();
    Some(format!("rook-{}", hex))
}

impl LLMClient {
    pub fn new(config: LLMConfig) -> Self {
        let http_client = Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .build()
            .unwrap_or_else(|_| Client::new());
        if config.api_key.trim().is_empty() {
            warn!(
                "LLMClient initialised in MOCK mode (no API key for model='{}', base_url='{}'). \
                 Chat and tool responses will be deterministic placeholders. \
                 Set an API key in Settings to talk to a real provider.",
                config.model, config.base_url
            );
        } else {
            info!(
                "LLMClient ready: model='{}' base_url='{}' (live)",
                config.model, config.base_url
            );
        }
        Self {
            config: Arc::new(config),
            http_client: Arc::new(http_client),
        }
    }

    /// Return a reference to the underlying config (used to clone + override at runtime).
    pub fn base_config(&self) -> &LLMConfig {
        &self.config
    }

    /// Cheapest model available at the configured provider's /models endpoint.
    /// Used to route auxiliary calls (distillation, auto-title, intent classifier)
    /// off the user's primary paid model. Falls back to the primary model id if
    /// the endpoint doesn't advertise a cheaper tier.
    pub async fn cheapest_model(&self) -> String {
        if let Some(m) = crate::llm::cheap_model::cheapest_model(
            &self.http_client,
            &self.config.base_url,
            &self.config.api_key,
        )
        .await
        {
            return m;
        }
        self.config.model.clone()
    }

    /// Run a non-streaming chat completion against a specific model + no tools,
    /// using the primary base_url / api_key. Used by auxiliary callers that
    /// want to target the cheapest available model without rewriting routing.
    pub async fn chat_with_model_override(
        &self,
        messages: Vec<Message>,
        model: &str,
        max_tokens: u32,
    ) -> Result<ChatCompletionResponse> {
        if self.config.api_key.trim().is_empty() {
            return Ok(self.mock_chat_response(&messages, false));
        }
        let request = ChatCompletionRequest {
            model: model.to_string(),
            messages,
            tools: None,
            max_tokens: Some(max_tokens),
            temperature: Some(0.2),
            stream: Some(false),
            user: install_user_id(&self.config.api_key),
        };
        let resp = self
            .http_client
            .post(format!("{}/chat/completions", self.config.base_url))
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(60))
            .json(&request)
            .send()
            .await?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM override returned {}: {}", status, body);
        }
        resp.json::<ChatCompletionResponse>()
            .await
            .context("override JSON parse")
    }

    /// True when the active config has no API key — Chat returns deterministic
    /// mock responses. The frontend can use this to render a "MOCK" badge so
    /// users don't think bad outputs are bugs.
    pub fn is_mock_mode(&self) -> bool {
        self.config.api_key.trim().is_empty()
    }

    pub async fn chat(&self, messages: Vec<Message>) -> Result<ChatCompletionResponse> {
        if self.config.api_key.trim().is_empty() {
            return Ok(self.mock_chat_response(&messages, false));
        }

        let request = ChatCompletionRequest {
            model: self.config.model.clone(),
            messages,
            tools: None,
            max_tokens: Some(self.config.max_tokens),
            temperature: Some(self.config.temperature),
            stream: Some(false),
            user: install_user_id(&self.config.api_key),
        };

        let resp = self
            .http_client
            .post(format!("{}/chat/completions", self.config.base_url))
            .header("Authorization", format!("Bearer {}", self.config.api_key))
            .header("Content-Type", "application/json")
            .timeout(std::time::Duration::from_secs(120))
            .json(&request)
            .send()
            .await?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API returned {}: {}", status, body);
        }

        let response = resp.json::<ChatCompletionResponse>().await?;
        Ok(response)
    }

    pub async fn chat_for_model(
        &self,
        messages: Vec<Message>,
        model: Option<&str>,
    ) -> Result<ChatCompletionResponse> {
        let is_local = model.map(is_local_model).unwrap_or(false);

        if is_local {
            let model_name = model.expect("is_local is true iff model is Some");
            let ollama_url = format!("{}/chat/completions", ollama_base_url());

            let request = ChatCompletionRequest {
                model: model_name.to_string(),
                messages,
                tools: None,
                max_tokens: Some(self.config.max_tokens),
                temperature: Some(self.config.temperature),
                stream: Some(false),
                user: None,
            };

            let resp = self
                .http_client
                .post(&ollama_url)
                .header("Content-Type", "application/json")
                .timeout(std::time::Duration::from_secs(120))
                .json(&request)
                .send()
                .await
                .context("Ollama request failed")?;

            if !resp.status().is_success() {
                let status = resp.status();
                let body = resp.text().await.unwrap_or_default();
                anyhow::bail!("Ollama returned {}: {}", status, body);
            }

            return resp
                .json::<ChatCompletionResponse>()
                .await
                .context("Ollama JSON parse");
        }

        // Fall through to normal (remote / mock) path
        self.chat(messages).await
    }

    /// Like `chat_for_model` but sends `stream: true` and returns the raw
    /// `reqwest::Response` so the caller can parse SSE events.
    /// Returns `Ok(None)` for mock mode (no API key, non-local model).
    pub async fn chat_stream_for_model(
        &self,
        messages: Vec<Message>,
        model: Option<&str>,
    ) -> Result<Option<reqwest::Response>> {
        let is_local = model.map(is_local_model).unwrap_or(false);

        let (use_model, base_url, api_key) = if is_local {
            let m = model
                .expect("is_local is true iff model is Some")
                .to_string();
            let url = ollama_base_url();
            (m, url, String::new())
        } else {
            if self.config.api_key.trim().is_empty() {
                return Ok(None); // mock mode — no streaming available
            }
            (
                self.config.model.clone(),
                self.config.base_url.clone(),
                self.config.api_key.clone(),
            )
        };

        let request = ChatCompletionRequest {
            model: use_model,
            messages,
            tools: None,
            max_tokens: Some(self.config.max_tokens),
            temperature: Some(self.config.temperature),
            stream: Some(true),
            user: install_user_id(&self.config.api_key),
        };

        let mut req = self
            .http_client
            .post(format!("{}/chat/completions", base_url))
            .header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
        }
        let resp = req.json(&request).send().await.context("stream request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API returned {}: {}", status, body);
        }

        Ok(Some(resp))
    }

    /// Like `chat_stream_for_model` but includes tool definitions in the request.
    /// Returns the raw SSE response for the caller to parse streaming tool call deltas.
    pub async fn chat_stream_with_tools_for_model(
        &self,
        messages: Vec<Message>,
        tools: Vec<crate::llm::types::ToolDefinition>,
        model: Option<&str>,
    ) -> Result<Option<reqwest::Response>> {
        let is_local = model.map(is_local_model).unwrap_or(false);

        let (use_model, base_url, api_key) = if is_local {
            let m = model
                .expect("is_local is true iff model is Some")
                .to_string();
            let url = ollama_base_url();
            (m, url, String::new())
        } else {
            if self.config.api_key.trim().is_empty() {
                return Ok(None);
            }
            (
                self.config.model.clone(),
                self.config.base_url.clone(),
                self.config.api_key.clone(),
            )
        };

        let request = ChatCompletionRequest {
            model: use_model,
            messages,
            tools: if tools.is_empty() { None } else { Some(tools) },
            max_tokens: Some(self.config.max_tokens),
            temperature: Some(self.config.temperature),
            stream: Some(true),
            user: None,
        };

        let mut req = self
            .http_client
            .post(format!("{}/chat/completions", base_url))
            .header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
        }
        let resp = req
            .json(&request)
            .send()
            .await
            .context("stream+tools request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM API returned {}: {}", status, body);
        }
        Ok(Some(resp))
    }

    #[allow(dead_code)]
    pub async fn chat_with_tools(
        &self,
        messages: Vec<Message>,
        tools: Vec<crate::llm::types::ToolDefinition>,
    ) -> Result<ChatCompletionResponse> {
        self.chat_with_tools_for_model(messages, tools, None).await
    }

    pub async fn chat_with_tools_for_model(
        &self,
        messages: Vec<Message>,
        tools: Vec<crate::llm::types::ToolDefinition>,
        model: Option<&str>,
    ) -> Result<ChatCompletionResponse> {
        let is_local = model.map(is_local_model).unwrap_or(false);

        let (use_model, base_url, api_key) = if is_local {
            let m = model
                .expect("is_local is true iff model is Some")
                .to_string();
            let url = ollama_base_url();
            (m, url, String::new())
        } else {
            if self.config.api_key.trim().is_empty() {
                return Ok(self.mock_chat_response(&messages, !tools.is_empty()));
            }
            (
                self.config.model.clone(),
                self.config.base_url.clone(),
                self.config.api_key.clone(),
            )
        };

        let request = ChatCompletionRequest {
            model: use_model,
            messages,
            tools: Some(tools),
            max_tokens: Some(self.config.max_tokens),
            temperature: Some(self.config.temperature),
            stream: Some(false),
            user: install_user_id(&self.config.api_key),
        };

        let mut req = self
            .http_client
            .post(format!("{}/chat/completions", base_url))
            .header("Content-Type", "application/json");
        if !api_key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", api_key));
        }
        let resp = req
            .json(&request)
            .send()
            .await
            .context("chat_with_tools request")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().await.unwrap_or_default();
            anyhow::bail!("LLM returned {}: {}", status, body);
        }

        resp.json::<ChatCompletionResponse>()
            .await
            .context("chat_with_tools JSON parse")
    }

    pub async fn get_embedding(&self, text: &str) -> Result<Vec<f32>> {
        match self.config.embedding_provider {
            EmbeddingProvider::Mock => Ok(self.mock_embedding(text)),
            EmbeddingProvider::OpenAiCompatible => self.get_remote_embedding(text).await,
            EmbeddingProvider::Ollama => match self.get_ollama_embedding(text).await {
                Ok(embedding) => Ok(embedding),
                Err(err) => {
                    warn!(
                        "Falling back to deterministic mock embeddings because Ollama embedding request failed: {}",
                        err
                    );
                    Ok(self.mock_embedding(text))
                }
            },
        }
    }

    async fn get_remote_embedding(&self, text: &str) -> Result<Vec<f32>> {
        if self.config.embedding_api_key.trim().is_empty() {
            return Ok(self.mock_embedding(text));
        }

        let request = EmbeddingRequest {
            model: self.config.embedding_model.clone(),
            input: text.to_string(),
        };

        let response = self
            .http_client
            .post(format!("{}/embeddings", self.config.embedding_base_url))
            .header(
                "Authorization",
                format!("Bearer {}", self.config.embedding_api_key),
            )
            .header("Content-Type", "application/json")
            .json(&request)
            .send()
            .await?
            .json::<EmbeddingResponse>()
            .await?;

        if let Some(data) = response.data.first() {
            Ok(data.embedding.clone())
        } else {
            anyhow::bail!("No embedding data returned")
        }
    }

    async fn get_ollama_embedding(&self, text: &str) -> Result<Vec<f32>> {
        let base_url = self.config.embedding_base_url.trim_end_matches('/');

        let embed_request = OllamaEmbedRequest {
            model: &self.config.embedding_model,
            input: text,
        };

        let embed_response = self
            .http_client
            .post(format!("{}/api/embed", base_url))
            .json(&embed_request)
            .send()
            .await;

        if let Ok(response) = embed_response {
            if response.status().is_success() {
                let payload = response
                    .json::<OllamaEmbedResponse>()
                    .await
                    .context("Failed to parse Ollama /api/embed response")?;
                return Self::extract_ollama_embedding(payload);
            }
        }

        let compat_request = OllamaEmbeddingsCompatRequest {
            model: &self.config.embedding_model,
            prompt: text,
        };

        let response = self
            .http_client
            .post(format!("{}/api/embeddings", base_url))
            .json(&compat_request)
            .send()
            .await
            .context("Failed to call Ollama embeddings endpoint")?;

        let payload = response
            .json::<OllamaEmbedResponse>()
            .await
            .context("Failed to parse Ollama /api/embeddings response")?;

        Self::extract_ollama_embedding(payload)
    }

    fn extract_ollama_embedding(payload: OllamaEmbedResponse) -> Result<Vec<f32>> {
        if let Some(first) = payload.embeddings.into_iter().next() {
            if !first.is_empty() {
                return Ok(first);
            }
        }

        if !payload.embedding.is_empty() {
            return Ok(payload.embedding);
        }

        anyhow::bail!("Ollama returned no embedding vector")
    }

    #[allow(dead_code)]
    pub async fn get_embeddings_batch(&self, texts: Vec<&str>) -> Result<Vec<Vec<f32>>> {
        let mut embeddings = Vec::new();
        for text in texts {
            let emb = self.get_embedding(text).await?;
            embeddings.push(emb);
        }
        Ok(embeddings)
    }

    fn mock_chat_response(&self, messages: &[Message], tool_mode: bool) -> ChatCompletionResponse {
        let user_content = messages
            .iter()
            .rev()
            .find(|m| m.role == "user")
            .map(|m| m.content.clone())
            .unwrap_or_else(|| "No user message provided.".to_string());

        let prefix = if tool_mode {
            "Local simulation mode (tools enabled)."
        } else {
            "Local simulation mode."
        };

        let summary: String = user_content.chars().take(500).collect();
        ChatCompletionResponse {
            id: Some("mock-chat-response".to_string()),
            choices: vec![Choice {
                message: ResponseMessage {
                    role: "assistant".to_string(),
                    content: Some(serde_json::Value::String(format!(
                        "{} Backend is connected and responding without an external LLM key.\n\nRequest summary:\n{}",
                        prefix, summary
                    ))),
                    tool_calls: None,
                },
                finish_reason: Some("stop".to_string()),
            }],
            usage: Some(Usage {
                prompt_tokens: 0,
                completion_tokens: 0,
                total_tokens: 0,
            }),
        }
    }

    fn mock_embedding(&self, text: &str) -> Vec<f32> {
        let mut values = vec![0.0_f32; 32];
        for (i, byte) in text.bytes().enumerate() {
            values[i % 32] += (byte as f32) / 255.0;
        }

        let norm = values.iter().map(|v| v * v).sum::<f32>().sqrt();
        if norm > 0.0 {
            for value in &mut values {
                *value /= norm;
            }
        }

        values
    }
}

#[cfg(test)]
mod tests {
    use super::{LLMClient, OllamaEmbedResponse};
    use crate::llm::types::{EmbeddingProvider, LLMConfig};

    #[test]
    fn extracts_embedding_from_ollama_shapes() {
        let payload = OllamaEmbedResponse {
            embeddings: vec![vec![0.1, 0.2, 0.3]],
            embedding: vec![],
        };
        let vector = LLMClient::extract_ollama_embedding(payload).unwrap();
        assert_eq!(vector.len(), 3);

        let compat_payload = OllamaEmbedResponse {
            embeddings: vec![],
            embedding: vec![0.4, 0.5],
        };
        let compat_vector = LLMClient::extract_ollama_embedding(compat_payload).unwrap();
        assert_eq!(compat_vector.len(), 2);
    }

    #[tokio::test]
    async fn mock_provider_still_returns_vector() {
        let client = LLMClient::new(LLMConfig {
            base_url: "https://api.openai.com/v1".to_string(),
            api_key: String::new(),
            model: "gpt-4o".to_string(),
            max_tokens: 1024,
            temperature: 0.2,
            embedding_provider: EmbeddingProvider::Mock,
            embedding_base_url: "http://127.0.0.1:11434".to_string(),
            embedding_model: "nomic-embed-text".to_string(),
            embedding_api_key: String::new(),
        });

        let vector = client
            .get_embedding("local embedding smoke test")
            .await
            .unwrap();
        assert!(!vector.is_empty());
    }
}
