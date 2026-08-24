use anyhow::{Context, Result};
use futures_util::StreamExt;
use reqwest::Client;

use super::config::AiConfig;
use super::credential::AiCredentials;
use super::provider::{
    AiErrorResponse, AiMessage, AiModelListResponse, AiProviderKind, AiRequest, AiResponse,
};

/// A single chunk from a streaming AI response.
#[derive(Debug, Clone)]
pub struct AiStreamChunk {
    pub content: String,
    pub done: bool,
}

/// Client for making requests to AI providers.
#[derive(Clone)]
pub struct AiClient {
    http: Client,
}

impl AiClient {
    pub fn new() -> Result<Self> {
        let http = Client::builder()
            .timeout(std::time::Duration::from_secs(120))
            .build()
            .context("failed to create HTTP client")?;
        Ok(Self { http })
    }

    /// Fetch available models from a provider.
    pub async fn fetch_models(
        &self,
        provider: AiProviderKind,
        credentials: &AiCredentials,
    ) -> Result<Vec<super::provider::AiModel>> {
        let api_key = credentials
            .get_key(provider)
            .context(format!("{} API key is not configured", provider))?;

        let url = provider.models_url();
        let resp = self
            .http
            .get(&url)
            .header("Authorization", format!("Bearer {}", api_key))
            .send()
            .await
            .context(format!("failed to connect to {}", provider))?;

        let status = resp.status();
        let body = resp
            .text()
            .await
            .context("failed to read response body")?;

        if !status.is_success() {
            let error: AiErrorResponse =
                serde_json::from_str(&body).unwrap_or(AiErrorResponse {
                    r#type: Some("http_error".to_string()),
                    error: Some(super::provider::AiErrorDetail {
                        r#type: None,
                        message: Some(format!("HTTP {}: {}", status, body)),
                    }),
                });
            let msg = error
                .error
                .and_then(|e| e.message)
                .unwrap_or_else(|| format!("HTTP {}", status));
            anyhow::bail!("{} model discovery failed: {}", provider, msg);
        }

        let list: AiModelListResponse =
            serde_json::from_str(&body).context("failed to parse model list")?;

        Ok(list.data.unwrap_or_default())
    }

    /// Send a chat completion request.
    pub async fn chat(
        &self,
        provider: AiProviderKind,
        credentials: &AiCredentials,
        config: &AiConfig,
        messages: Vec<AiMessage>,
    ) -> Result<String> {
        let api_key = credentials
            .get_key(provider)
            .context(format!("{} API key is not configured", provider))?;

        let model = config
            .model
            .as_deref()
            .context("no model selected. Use :ai-model to select one.")?;

        let url = provider.chat_url(model);

        let request = AiRequest {
            model: model.to_string(),
            messages,
            max_tokens: config.max_tokens,
            stream: Some(false),
        };

        let body = serde_json::to_string(&request).context("failed to serialize request")?;

        // Build headers based on model type
        let headers = if model.starts_with("claude-") {
            provider.anthropic_auth_headers(api_key)
        } else {
            provider.auth_headers(api_key)
        };

        let mut req_builder = self.http.post(&url);
        for (key, value) in &headers {
            req_builder = req_builder.header(key.as_str(), value.as_str());
        }

        let resp = req_builder
            .body(body)
            .send()
            .await
            .context(format!("failed to send request to {}", provider))?;

        let status = resp.status();
        let resp_body = resp
            .text()
            .await
            .context("failed to read response body")?;

        if !status.is_success() {
            let error: AiErrorResponse =
                serde_json::from_str(&resp_body).unwrap_or(AiErrorResponse {
                    r#type: Some("http_error".to_string()),
                    error: Some(super::provider::AiErrorDetail {
                        r#type: None,
                        message: Some(format!("HTTP {}: {}", status, resp_body)),
                    }),
                });
            let msg = error
                .error
                .and_then(|e| e.message)
                .unwrap_or_else(|| format!("HTTP {}", status));
            anyhow::bail!("{} request failed: {}", provider, msg);
        }

        let response: AiResponse =
            serde_json::from_str(&resp_body).context("failed to parse AI response")?;

        if let Some(error) = response.error {
            let msg = error.message.unwrap_or_else(|| "unknown error".to_string());
            anyhow::bail!("{} API error: {}", provider, msg);
        }

        let content = response
            .choices
            .and_then(|choices| choices.into_iter().next())
            .and_then(|choice| choice.message)
            .and_then(|msg| msg.content)
            .unwrap_or_default();

        Ok(content)
    }

    /// Send a streaming chat completion request.
    /// Returns a channel that yields response chunks.
    pub async fn chat_stream(
        &self,
        provider: AiProviderKind,
        credentials: &AiCredentials,
        config: &AiConfig,
        messages: Vec<AiMessage>,
    ) -> Result<tokio::sync::mpsc::Receiver<AiStreamChunk>> {
        let api_key = credentials
            .get_key(provider)
            .context(format!("{} API key is not configured", provider))?;

        let model = config
            .model
            .as_deref()
            .context("no model selected. Use :ai-model to select one.")?
            .to_string();

        let url = provider.chat_url(&model);

        let request = AiRequest {
            model: model.clone(),
            messages,
            max_tokens: config.max_tokens,
            stream: Some(true),
        };

        let body = serde_json::to_string(&request).context("failed to serialize request")?;

        let headers = if model.starts_with("claude-") {
            provider.anthropic_auth_headers(api_key)
        } else {
            provider.auth_headers(api_key)
        };

        let mut req_builder = self.http.post(&url);
        for (key, value) in &headers {
            req_builder = req_builder.header(key.as_str(), value.as_str());
        }

        let resp = req_builder
            .body(body)
            .send()
            .await
            .context(format!("failed to send streaming request to {}", provider))?;

        let status = resp.status();
        if !status.is_success() {
            let resp_body = resp.text().await.unwrap_or_default();
            let error: AiErrorResponse =
                serde_json::from_str(&resp_body).unwrap_or(AiErrorResponse {
                    r#type: Some("http_error".to_string()),
                    error: Some(super::provider::AiErrorDetail {
                        r#type: None,
                        message: Some(format!("HTTP {}: {}", status, resp_body)),
                    }),
                });
            let msg = error
                .error
                .and_then(|e| e.message)
                .unwrap_or_else(|| format!("HTTP {}", status));
            anyhow::bail!("{} streaming request failed: {}", provider, msg);
        }

        let (tx, rx) = tokio::sync::mpsc::channel(64);

        tokio::spawn(async move {
            let mut stream = resp.bytes_stream();
            let mut buffer = String::new();
            let mut sent_done = false;

            while let Some(chunk_result) = stream.next().await {
                let chunk = match chunk_result {
                    Ok(c) => c,
                    Err(e) => {
                        let _ = tx
                            .send(AiStreamChunk {
                                content: format!("\n[Stream error: {}]", e),
                                done: true,
                            })
                            .await;
                        break;
                    }
                };

                buffer.push_str(&String::from_utf8_lossy(&chunk));

                // Process complete lines from SSE
                while let Some(line_end) = buffer.find('\n') {
                    let line = buffer[..line_end].to_string();
                    buffer = buffer[line_end + 1..].to_string();

                    if line.starts_with("data: ") {
                        let data = &line[6..];
                        if data.trim() == "[DONE]" {
                            let _ = tx
                                .send(AiStreamChunk {
                                    content: String::new(),
                                    done: true,
                                })
                                .await;
                            sent_done = true;
                            break;
                        }

                        // Try to parse as Chat Completions chunk
                        if let Ok(chunk) = serde_json::from_str::<AiResponse>(data) {
                            if let Some(content) = chunk
                                .choices
                                .and_then(|c| c.into_iter().next())
                                .and_then(|c| c.delta)
                                .and_then(|d| d.content)
                            {
                                if !content.is_empty() {
                                    let _ = tx
                                        .send(AiStreamChunk {
                                            content,
                                            done: false,
                                        })
                                        .await;
                                }
                            }
                        }
                    }
                }

                if sent_done {
                    break;
                }
            }

            if !sent_done {
                let _ = tx
                    .send(AiStreamChunk {
                        content: String::new(),
                        done: true,
                    })
                    .await;
            }
        });

        Ok(rx)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_client_creation() {
        let client = AiClient::new();
        assert!(client.is_ok());
    }
}
