//! OAuth 2.0 PKCE client — local HTTP callback, token exchange, refresh, storage.
//!
//! Provider-agnostic (configured via settings.json). Exposes interfaces
//! for application-layer auth flows (CLI login, MCP OAuth bearer).

pub mod callback;
pub mod client;
pub mod pkce;
pub mod provider;
pub mod store;

pub use callback::{CallbackError, CallbackListener, CallbackResult};
pub use client::{token_to_stored, OAuth2Client, OAuthError, TokenResponse};
pub use pkce::{PkceMethod, PkceVerifier};
pub use provider::ProviderConfig;
pub use store::TokenStore;
