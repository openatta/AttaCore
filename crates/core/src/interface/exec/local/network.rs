//! `LocalNetwork` — outbound requests from this process.

use std::collections::BTreeMap;
use std::time::Duration;

use crate::interface::exec::{ExecError, HttpRequest, HttpResponse, Network, Origin};

/// This process's own network, under the deployment's egress policy.
pub struct LocalNetwork {
    client: reqwest::Client,
    /// Hosts the model is allowed to reach. Empty means no restriction, which
    /// is what an unconfigured deployment has always had.
    allowed_domains: Vec<String>,
    /// Whether agent-directed egress is refused outright.
    agent_offline: bool,
}

impl Default for LocalNetwork {
    fn default() -> Self {
        Self::new(Vec::new(), false)
    }
}

impl LocalNetwork {
    pub fn new(allowed_domains: Vec<String>, agent_offline: bool) -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(60))
                .build()
                .unwrap_or_default(),
            allowed_domains,
            agent_offline,
        }
    }

    fn host_of(url: &str) -> Option<String> {
        url::Url::parse(url)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
    }
}

#[async_trait::async_trait]
impl Network for LocalNetwork {
    async fn send(&self, req: HttpRequest, origin: Origin) -> Result<HttpResponse, ExecError> {
        let host = Self::host_of(&req.url)
            .ok_or_else(|| ExecError::failed(format!("not a url: {}", req.url)))?;
        if !self.permits(&host, origin) {
            return Err(ExecError::denied(format!(
                "{host} is not in the allowed domains for model-directed requests"
            )));
        }

        let method = reqwest::Method::from_bytes(req.method.as_bytes())
            .map_err(|e| ExecError::failed(format!("{}: {e}", req.method)))?;
        let mut b = self.client.request(method, &req.url);
        for (k, v) in &req.headers {
            b = b.header(k, v);
        }
        if let Some(body) = req.body {
            b = b.body(body);
        }

        let resp = b.send().await.map_err(|e| {
            // A request that never reached a server is this side failing, and
            // the two are different sentences to whoever reads them.
            if e.is_connect() || e.is_timeout() {
                ExecError::unavailable(format!("{}: {e}", req.url))
            } else {
                ExecError::failed(format!("{}: {e}", req.url))
            }
        })?;

        let status = resp.status().as_u16();
        let headers: BTreeMap<String, String> = resp
            .headers()
            .iter()
            .filter_map(|(k, v)| v.to_str().ok().map(|v| (k.to_string(), v.to_string())))
            .collect();
        let body = resp
            .bytes()
            .await
            .map_err(|e| ExecError::failed(format!("reading the response body: {e}")))?
            .to_vec();
        Ok(HttpResponse {
            status,
            headers,
            body,
        })
    }

    fn permits(&self, host: &str, origin: Origin) -> bool {
        match origin {
            // An operator chose this endpoint. Applying the model's allowlist
            // to it would cut the agent off from its own model.
            Origin::Operator => true,
            Origin::Agent if self.agent_offline => false,
            Origin::Agent => {
                self.allowed_domains.is_empty()
                    || self.allowed_domains.iter().any(|d| {
                        host == d || host.ends_with(&format!(".{d}"))
                    })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allowlist_binds_the_model_and_not_the_operator() {
        let n = LocalNetwork::new(vec!["example.com".into()], false);
        assert!(n.permits("example.com", Origin::Agent));
        assert!(n.permits("api.example.com", Origin::Agent), "subdomains too");
        assert!(!n.permits("evil.test", Origin::Agent));
        assert!(
            n.permits("evil.test", Origin::Operator),
            "an operator-configured endpoint is not the model's choice to be \
             restricted from — applying the allowlist here is how an agent \
             loses its own model"
        );
    }

    #[test]
    fn an_empty_allowlist_is_no_restriction() {
        let n = LocalNetwork::default();
        assert!(n.permits("anything.test", Origin::Agent));
    }

    #[test]
    fn offline_stops_the_model_and_leaves_the_engine_running() {
        let n = LocalNetwork::new(Vec::new(), true);
        assert!(!n.permits("example.com", Origin::Agent));
        assert!(n.permits("example.com", Origin::Operator));
    }

    #[test]
    fn a_suffix_that_is_not_a_subdomain_does_not_match() {
        let n = LocalNetwork::new(vec!["example.com".into()], false);
        assert!(
            !n.permits("notexample.com", Origin::Agent),
            "matching on a bare suffix would let any attacker-registered \
             domain ending in the allowed one through"
        );
    }
}
