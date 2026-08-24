pub mod client;
pub mod commands;
pub mod config;
pub mod context;
pub mod credential;
pub mod provider;
pub mod response;

use std::sync::{Mutex, OnceLock};

pub use commands::AiState;
pub use config::AiConfig;
pub use credential::AiCredentials;
pub use provider::{AiModel, AiProviderKind};

/// Global AI state, initialized once at startup.
static AI_STATE: OnceLock<Mutex<AiState>> = OnceLock::new();

/// Initialize the global AI state. Called once during application startup.
pub fn init() {
    let credentials = AiCredentials::load();
    let _ = AI_STATE.set(Mutex::new(AiState::new_with_credentials(credentials)));
}

/// Access the global AI state (read-only).
pub fn state() -> Option<std::sync::MutexGuard<'static, AiState>> {
    AI_STATE.get().map(|m| m.lock().unwrap())
}
