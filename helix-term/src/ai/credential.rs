use std::collections::HashMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::provider::AiProviderKind;

/// Persistent credential store for AI provider API keys.
///
/// Credentials are stored in a JSON file in the Helix config directory.
/// The file is separate from normal editor configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AiCredentials {
    keys: HashMap<AiProviderKind, String>,
}

/// On-disk credential file format.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct CredentialFile {
    credentials: HashMap<String, String>,
}

impl AiCredentials {
    pub fn new() -> Self {
        Self::default()
    }

    /// Get the path to the credential file.
    fn credential_path() -> PathBuf {
        helix_loader::config_dir().join("ai_credentials.json")
    }

    /// Load credentials from disk. Returns empty if file doesn't exist.
    pub fn load() -> Self {
        let path = Self::credential_path();
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                let file: CredentialFile = serde_json::from_str(&content).unwrap_or_default();
                let keys: HashMap<AiProviderKind, String> = file
                    .credentials
                    .into_iter()
                    .filter_map(|(k, v)| {
                        let provider = match k.as_str() {
                            "opencode-zen" => AiProviderKind::OpenCodeZen,
                            "opencode-go" => AiProviderKind::OpenCodeGo,
                            _ => return None,
                        };
                        Some((provider, v))
                    })
                    .collect();
                Self { keys }
            }
            Err(_) => Self::default(),
        }
    }

    /// Save credentials to disk.
    pub fn save(&self) -> Result<(), String> {
        let path = Self::credential_path();

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("Failed to create config directory: {}", e))?;
        }

        let file = CredentialFile {
            credentials: self
                .keys
                .iter()
                .map(|(k, v)| {
                    let name = match k {
                        AiProviderKind::OpenCodeZen => "opencode-zen",
                        AiProviderKind::OpenCodeGo => "opencode-go",
                    };
                    (name.to_string(), v.clone())
                })
                .collect(),
        };

        let content = serde_json::to_string_pretty(&file)
            .map_err(|e| format!("Failed to serialize credentials: {}", e))?;

        std::fs::write(&path, content)
            .map_err(|e| format!("Failed to write credential file: {}", e))?;

        Ok(())
    }

    /// Store an API key for a provider.
    pub fn set_key(&mut self, provider: AiProviderKind, key: String) {
        self.keys.insert(provider, key);
    }

    /// Get the API key for a provider.
    pub fn get_key(&self, provider: AiProviderKind) -> Option<&str> {
        self.keys.get(&provider).map(|s| s.as_str())
    }

    /// Check if a provider has a configured key.
    pub fn has_key(&self, provider: AiProviderKind) -> bool {
        self.keys.contains_key(&provider)
    }

    /// Remove the key for a provider.
    pub fn remove_key(&mut self, provider: AiProviderKind) -> bool {
        self.keys.remove(&provider).is_some()
    }

    /// Get a masked representation of the key for display.
    pub fn masked_key(&self, provider: AiProviderKind) -> String {
        match self.keys.get(&provider) {
            Some(key) => {
                if key.len() <= 8 {
                    "*".repeat(key.len())
                } else {
                    format!("{}...{}", &key[..4], &key[key.len() - 4..])
                }
            }
            None => "not configured".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_credential_store_and_retrieve() {
        let mut creds = AiCredentials::new();
        assert!(!creds.has_key(AiProviderKind::OpenCodeZen));

        creds.set_key(AiProviderKind::OpenCodeZen, "sk-test-12345678".to_string());
        assert!(creds.has_key(AiProviderKind::OpenCodeZen));
        assert_eq!(creds.get_key(AiProviderKind::OpenCodeZen), Some("sk-test-12345678"));
        assert!(!creds.has_key(AiProviderKind::OpenCodeGo));
    }

    #[test]
    fn test_masked_key() {
        let mut creds = AiCredentials::new();
        creds.set_key(AiProviderKind::OpenCodeZen, "sk-1234567890abcdef".to_string());
        let masked = creds.masked_key(AiProviderKind::OpenCodeZen);
        assert!(masked.starts_with("sk-1"));
        assert!(masked.ends_with("cdef"));
        assert!(masked.contains("..."));
    }

    #[test]
    fn test_masked_key_short() {
        let mut creds = AiCredentials::new();
        creds.set_key(AiProviderKind::OpenCodeZen, "abc".to_string());
        let masked = creds.masked_key(AiProviderKind::OpenCodeZen);
        assert_eq!(masked, "***");
    }

    #[test]
    fn test_masked_key_not_configured() {
        let creds = AiCredentials::new();
        let masked = creds.masked_key(AiProviderKind::OpenCodeZen);
        assert_eq!(masked, "not configured");
    }

    #[test]
    fn test_remove_key() {
        let mut creds = AiCredentials::new();
        creds.set_key(AiProviderKind::OpenCodeGo, "key123".to_string());
        assert!(creds.remove_key(AiProviderKind::OpenCodeGo));
        assert!(!creds.has_key(AiProviderKind::OpenCodeGo));
        assert!(!creds.remove_key(AiProviderKind::OpenCodeGo));
    }

    #[test]
    fn test_credential_serialization_roundtrip() {
        let mut creds = AiCredentials::new();
        creds.set_key(AiProviderKind::OpenCodeZen, "zen-key-123".to_string());
        creds.set_key(AiProviderKind::OpenCodeGo, "go-key-456".to_string());

        let file = CredentialFile {
            credentials: creds
                .keys
                .iter()
                .map(|(k, v)| {
                    let name = match k {
                        AiProviderKind::OpenCodeZen => "opencode-zen",
                        AiProviderKind::OpenCodeGo => "opencode-go",
                    };
                    (name.to_string(), v.clone())
                })
                .collect(),
        };

        let json = serde_json::to_string(&file).unwrap();
        let loaded: CredentialFile = serde_json::from_str(&json).unwrap();
        let loaded_creds: AiCredentials = AiCredentials {
            keys: loaded
                .credentials
                .into_iter()
                .filter_map(|(k, v)| {
                    let provider = match k.as_str() {
                        "opencode-zen" => AiProviderKind::OpenCodeZen,
                        "opencode-go" => AiProviderKind::OpenCodeGo,
                        _ => return None,
                    };
                    Some((provider, v))
                })
                .collect(),
        };

        assert_eq!(
            loaded_creds.get_key(AiProviderKind::OpenCodeZen),
            Some("zen-key-123")
        );
        assert_eq!(
            loaded_creds.get_key(AiProviderKind::OpenCodeGo),
            Some("go-key-456")
        );
    }

    #[test]
    fn test_credential_path_is_not_empty() {
        let path = AiCredentials::credential_path();
        assert!(!path.as_os_str().is_empty());
    }
}
