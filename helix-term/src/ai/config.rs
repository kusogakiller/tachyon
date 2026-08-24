use serde::{Deserialize, Serialize};

use super::provider::AiProviderKind;

/// AI configuration stored in the editor config.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", default)]
pub struct AiConfig {
    /// The active AI provider.
    pub provider: Option<AiProviderKind>,
    /// The selected model ID.
    pub model: Option<String>,
    /// Maximum tokens for responses.
    pub max_tokens: Option<u32>,
    /// Request timeout in seconds.
    pub timeout_secs: Option<u64>,
}

impl AiConfig {
    pub fn timeout_duration(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.timeout_secs.unwrap_or(60))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config() {
        let config = AiConfig::default();
        assert!(config.provider.is_none());
        assert!(config.model.is_none());
        assert!(config.max_tokens.is_none());
        assert_eq!(config.timeout_secs, None);
    }

    #[test]
    fn test_config_serialization() {
        let config = AiConfig {
            provider: Some(AiProviderKind::OpenCodeZen),
            model: Some("gpt-4".to_string()),
            max_tokens: Some(4096),
            timeout_secs: Some(30),
        };
        let json = serde_json::to_string(&config).unwrap();
        assert!(json.contains("open-code-zen"));
        assert!(json.contains("gpt-4"));
    }

    #[test]
    fn test_config_deserialization() {
        let json = r#"{
            "provider": "open-code-go",
            "model": "claude-opus-4-5",
            "max-tokens": 8192,
            "timeout-secs": 120
        }"#;
        let config: AiConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.provider, Some(AiProviderKind::OpenCodeGo));
        assert_eq!(config.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(config.max_tokens, Some(8192));
        assert_eq!(config.timeout_secs, Some(120));
    }

    #[test]
    fn test_timeout_duration_default() {
        let config = AiConfig::default();
        assert_eq!(config.timeout_duration(), std::time::Duration::from_secs(60));
    }

    #[test]
    fn test_timeout_duration_custom() {
        let config = AiConfig {
            timeout_secs: Some(30),
            ..Default::default()
        };
        assert_eq!(config.timeout_duration(), std::time::Duration::from_secs(30));
    }
}
