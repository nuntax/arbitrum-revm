//! Minimal JSON-RPC client with strict response handling.

use std::sync::atomic::{AtomicU64, Ordering};

use eyre::{Result, bail, eyre};
use reqwest::Url;
use serde_json::{Value, json};

pub struct RpcClient {
    client: reqwest::Client,
    url: Url,
    request_id: AtomicU64,
}

impl RpcClient {
    pub fn new(url: &str) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder().build()?,
            url: Url::parse(url)?,
            request_id: AtomicU64::new(1),
        })
    }

    pub async fn request(&self, method: &str, params: Value) -> Result<Value> {
        let id = self.request_id.fetch_add(1, Ordering::Relaxed);
        let response = self
            .client
            .post(self.url.clone())
            .json(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}))
            .send()
            .await?
            .error_for_status()?;
        let response: Value = response.json().await?;
        if let Some(error) = response.get("error") {
            bail!("Nitro RPC {method} failed: {error}");
        }
        response
            .get("result")
            .cloned()
            .ok_or_else(|| eyre!("Nitro RPC {method} returned no result: {response}"))
    }
}
