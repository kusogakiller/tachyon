use serde::{Deserialize, Serialize};

/// Supported AI providers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AiProviderKind {
    OpenCodeZen,
    OpenCodeGo,
}

impl AiProviderKind {
    pub fn display_name(self) -> &'static str {
        match self {
            AiProviderKind::OpenCodeZen => "OpenCode Zen",
            AiProviderKind::OpenCodeGo => "OpenCode Go",
        }
    }

    pub fn base_url(self) -> &'static str {
        match self {
            AiProviderKind::OpenCodeZen => "https://opencode.ai/zen/v1",
            AiProviderKind::OpenCodeGo => "https://opencode.ai/zen/go/v1",
        }
    }
}

impl std::fmt::Display for AiProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.display_name())
    }
}

/// A model returned by the provider's model discovery endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AiModel {
    pub id: String,
    #[serde(default)]
    pub object: String,
    #[serde(default)]
    pub created: Option<i64>,
    #[serde(default)]
    pub owned_by: Option<String>,
}

impl std::fmt::Display for AiModel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.id)
    }
}

/// A message in the chat conversation.
#[derive(Debug, Clone, Serialize)]
pub struct AiMessage {
    pub role: String,
    pub content: String,
}

/// A request to the AI provider.
#[derive(Debug, Clone, Serialize)]
pub struct AiRequest {
    pub model: String,
    pub messages: Vec<AiMessage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<bool>,
}

/// The AI provider's response.
#[derive(Debug, Clone, Deserialize)]
pub struct AiResponse {
    pub choices: Option<Vec<AiChoice>>,
    #[serde(default)]
    pub error: Option<AiError>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiChoice {
    pub message: Option<AiChoiceMessage>,
    pub delta: Option<AiChoiceDelta>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiChoiceMessage {
    pub content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiChoiceDelta {
    pub content: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiError {
    #[serde(default)]
    pub r#type: Option<String>,
    #[serde(default)]
    pub message: Option<String>,
}

/// Model list response from the provider.
#[derive(Debug, Clone, Deserialize)]
pub struct AiModelListResponse {
    pub data: Option<Vec<AiModel>>,
}

/// Error response from the provider (OpenCode-specific format).
#[derive(Debug, Clone, Deserialize)]
pub struct AiErrorResponse {
    pub r#type: Option<String>,
    pub error: Option<AiErrorDetail>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct AiErrorDetail {
    pub r#type: Option<String>,
    pub message: Option<String>,
}

impl AiProviderKind {
    /// Build the authentication headers for a Chat Completions request.
    pub fn auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        vec![
            ("Authorization".to_string(), format!("Bearer {}", api_key)),
            ("Content-Type".to_string(), "application/json".to_string()),
        ]
    }

    /// Build the authentication headers for an Anthropic Messages request.
    pub fn anthropic_auth_headers(&self, api_key: &str) -> Vec<(String, String)> {
        vec![
            ("x-api-key".to_string(), api_key.to_string()),
            ("Content-Type".to_string(), "application/json".to_string()),
            ("anthropic-version".to_string(), "2023-06-01".to_string()),
        ]
    }

    /// Determine which endpoint to use based on the model name.
    fn endpoint_for_model(&self, model: &str) -> &'static str {
        if model.starts_with("claude-") {
            "messages"
        } else {
            "chat/completions"
        }
    }

    /// Build the full URL for a request.
    pub fn chat_url(&self, model: &str) -> String {
        let endpoint = self.endpoint_for_model(model);
        format!("{}/{}", self.base_url(), endpoint)
    }

    /// Build the full URL for model discovery.
    pub fn models_url(&self) -> String {
        format!("{}/models", self.base_url())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_provider_display_names() {
        assert_eq!(AiProviderKind::OpenCodeZen.display_name(), "OpenCode Zen");
        assert_eq!(AiProviderKind::OpenCodeGo.display_name(), "OpenCode Go");
    }

    #[test]
    fn test_provider_base_urls() {
        assert_eq!(
            AiProviderKind::OpenCodeZen.base_url(),
            "https://opencode.ai/zen/v1"
        );
        assert_eq!(
            AiProviderKind::OpenCodeGo.base_url(),
            "https://opencode.ai/zen/go/v1"
        );
    }

    #[test]
    fn test_chat_url() {
        assert_eq!(
            AiProviderKind::OpenCodeZen.chat_url("gpt-4"),
            "https://opencode.ai/zen/v1/chat/completions"
        );
        assert_eq!(
            AiProviderKind::OpenCodeGo.chat_url("claude-opus-4-5"),
            "https://opencode.ai/zen/go/v1/messages"
        );
    }

    #[test]
    fn test_models_url() {
        assert_eq!(
            AiProviderKind::OpenCodeZen.models_url(),
            "https://opencode.ai/zen/v1/models"
        );
    }

    #[test]
    fn test_auth_headers() {
        let headers = AiProviderKind::OpenCodeZen.auth_headers("test-key");
        assert_eq!(headers.len(), 2);
        assert_eq!(headers[0].0, "Authorization");
        assert_eq!(headers[0].1, "Bearer test-key");
    }

    #[test]
    fn test_anthropic_auth_headers() {
        let headers = AiProviderKind::OpenCodeGo.anthropic_auth_headers("test-key");
        assert!(headers.iter().any(|(k, _)| k == "x-api-key"));
        assert!(headers
            .iter()
            .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01"));
    }

    #[test]
    fn test_model_serialization() {
        let model = AiModel {
            id: "gpt-4".to_string(),
            object: "model".to_string(),
            created: Some(1234567890),
            owned_by: Some("opencode".to_string()),
        };
        let json = serde_json::to_string(&model).unwrap();
        assert!(json.contains("gpt-4"));
    }

    #[test]
    fn test_request_serialization() {
        let req = AiRequest {
            model: "gpt-4".to_string(),
            messages: vec![AiMessage {
                role: "user".to_string(),
                content: "hello".to_string(),
            }],
            max_tokens: Some(100),
            stream: Some(false),
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("gpt-4"));
        assert!(json.contains("hello"));
    }

    #[test]
    fn test_response_deserialization() {
        let json = r#"{
            "choices": [{
                "message": {
                    "content": "Hello! How can I help?"
                }
            }]
        }"#;
        let resp: AiResponse = serde_json::from_str(json).unwrap();
        assert!(resp.choices.is_some());
        let choices = resp.choices.unwrap();
        assert_eq!(choices.len(), 1);
        assert_eq!(
            choices[0].message.as_ref().unwrap().content.as_deref(),
            Some("Hello! How can I help?")
        );
    }

    #[test]
    fn test_error_response_deserialization() {
        let json = r#"{
            "type": "error",
            "error": {
                "type": "AuthError",
                "message": "Missing API key."
            }
        }"#;
        let resp: AiErrorResponse = serde_json::from_str(json).unwrap();
        assert_eq!(resp.r#type.as_deref(), Some("error"));
        assert_eq!(
            resp.error.as_ref().unwrap().r#type.as_deref(),
            Some("AuthError")
        );
    }

    #[test]
    fn test_model_list_deserialization() {
        let json = r#"{
            "object": "list",
            "data": [
                {"id": "gpt-4", "object": "model", "created": 123, "owned_by": "opencode"},
                {"id": "claude-opus-4-5", "object": "model"}
            ]
        }"#;
        let resp: AiModelListResponse = serde_json::from_str(json).unwrap();
        let models = resp.data.unwrap();
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].id, "gpt-4");
        assert_eq!(models[1].id, "claude-opus-4-5");
    }

    #[test]
    fn test_endpoint_routing() {
        assert_eq!(
            AiProviderKind::OpenCodeZen.endpoint_for_model("gpt-4"),
            "chat/completions"
        );
        assert_eq!(
            AiProviderKind::OpenCodeZen.endpoint_for_model("claude-opus-4-5"),
            "messages"
        );
        assert_eq!(
            AiProviderKind::OpenCodeZen.endpoint_for_model("deepseek-v4"),
            "chat/completions"
        );
    }
}
