//! OAuth provider configuration. User specifies these in settings.json under
//! `oauth_providers.<name>`, or one of the bundled defaults can be referenced.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-provider OAuth configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ProviderConfig {
    /// Authorization endpoint (where we send the user's browser).
    pub authorize_url: String,
    /// Token endpoint (where we exchange the code).
    pub token_url: String,
    /// OAuth client_id. Public for CLI tools.
    pub client_id: String,
    /// Optional client_secret. CLI tools usually omit (use PKCE instead).
    #[serde(default)]
    pub client_secret: Option<String>,
    /// Scopes to request (space-joined when sent).
    #[serde(default)]
    pub scopes: Vec<String>,
    /// Extra headers for token requests (e.g. for `anthropic-beta` previews).
    #[serde(default)]
    pub token_request_headers: HashMap<String, String>,
    /// Extra query params on the authorize URL (e.g. `audience=…`).
    #[serde(default)]
    pub authorize_extra_params: HashMap<String, String>,
}

impl ProviderConfig {
    /// Return space-joined scope string.
    pub fn scope_string(&self) -> String {
        self.scopes.join(" ")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deserialize_minimal() {
        let v: ProviderConfig = serde_json::from_str(
            r#"{
                "authorize_url": "https://x.example/authorize",
                "token_url": "https://x.example/token",
                "client_id": "abc"
            }"#,
        )
        .unwrap();
        assert_eq!(v.client_id, "abc");
        assert!(v.scopes.is_empty());
    }

    #[test]
    fn scope_string_joins_with_space() {
        let v = ProviderConfig {
            authorize_url: "x".into(),
            token_url: "x".into(),
            client_id: "x".into(),
            client_secret: None,
            scopes: vec!["read".into(), "write".into()],
            token_request_headers: Default::default(),
            authorize_extra_params: Default::default(),
        };
        assert_eq!(v.scope_string(), "read write");
    }
}
