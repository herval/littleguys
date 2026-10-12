//! The real [`BridgeHost`]: the Hub's runtimes and the Activity's events.

use std::sync::Arc;
use std::time::Duration;

use futures_util::future::BoxFuture;
use futures_util::StreamExt;
use serde_json::Value;
use tokio::sync::broadcast;

use super::host::{BridgeHost, HostEvent, MediaFetch};
use crate::activity::Activity;
use crate::gateway::client::RpcError;
use crate::hub::Hub;

pub struct AppHost {
    hub: Arc<Hub>,
    activity: Arc<Activity>,
}

impl AppHost {
    pub fn new(hub: Arc<Hub>, activity: Arc<Activity>) -> Arc<Self> {
        Arc::new(Self { hub, activity })
    }
}

fn unknown(gateway: &str) -> RpcError {
    RpcError::new("UNKNOWN_GATEWAY", format!("no bot {gateway}"))
}

impl BridgeHost for AppHost {
    /// Every bot that's turned on, in the sidebar's order, under the name it shows.
    fn statuses(&self) -> Vec<Value> {
        self.activity
            .roster()
            .gateways
            .into_iter()
            .filter(|g| g.enabled)
            .filter_map(|g| {
                let mut status = serde_json::to_value(&g.status).ok()?;
                status["gatewayId"] = g.id.into();
                status["gatewayName"] = g.name.into();
                Some(status)
            })
            .collect()
    }

    fn call(&self, gateway: &str, method: &str, params: Value, timeout: Option<Duration>) -> BoxFuture<'static, Result<Value, RpcError>> {
        let client = self.hub.client(gateway);
        let (gateway, method) = (gateway.to_string(), method.to_string());
        Box::pin(async move {
            let client = client.ok_or_else(|| unknown(&gateway))?;
            match timeout {
                Some(t) => client.call_with_timeout(&method, params, t).await,
                None => client.call(&method, params).await,
            }
        })
    }

    fn reconnect(&self, gateway: &str) -> Result<(), RpcError> {
        self.hub.client(gateway).ok_or_else(|| unknown(gateway))?;
        self.hub.reconnect(Some(gateway));
        Ok(())
    }

    fn media(
        &self,
        gateway: &str,
        source: &str,
        session_key: Option<&str>,
        agent_id: Option<&str>,
    ) -> BoxFuture<'static, Result<MediaFetch, RpcError>> {
        let client = self.hub.client(gateway);
        let gateway = gateway.to_string();
        let (source, session_key, agent_id) = (source.to_string(), session_key.map(str::to_string), agent_id.map(str::to_string));
        Box::pin(async move {
            let client = client.ok_or_else(|| unknown(&gateway))?;
            let info = match crate::media::media_url(&client, source, session_key, agent_id).await {
                Ok(info) => info,
                Err(e) if e.code == "UNSUPPORTED" => {
                    return Ok(MediaFetch::Unavailable { code: Some(e.code), reason: Some(e.message) });
                }
                Err(e) => return Err(e),
            };
            if let Some(path) = info["path"].as_str().filter(|_| info["available"] == true) {
                return local_file(path, info["mimeType"].as_str()).await;
            }
            let url = match info["url"].as_str() {
                Some(url) if info["available"] == true => url.to_string(),
                _ => {
                    return Ok(MediaFetch::Unavailable {
                        code: info["code"].as_str().map(str::to_string),
                        reason: info["reason"].as_str().map(str::to_string),
                    })
                }
            };
            // The ticket is the credential.
            let res = reqwest::Client::new()
                .get(&url)
                .timeout(Duration::from_secs(600))
                .send()
                .await
                .map_err(|e| RpcError::new("UNAVAILABLE", e.to_string()))?;
            if !res.status().is_success() {
                return Ok(MediaFetch::Unavailable { code: Some("http".into()), reason: Some(format!("the gateway answered {}", res.status())) });
            }
            let mime_type = res
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|v| v.split(';').next().unwrap_or_default().trim().to_string())
                .or_else(|| info["mimeType"].as_str().map(str::to_string));
            let size_bytes = res.content_length().or_else(|| info["sizeBytes"].as_u64());
            let body = res.bytes_stream().map(|chunk| chunk.map(|b| b.to_vec()).map_err(|e| e.to_string())).boxed();
            Ok(MediaFetch::Available { mime_type, size_bytes, body })
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<HostEvent> {
        self.activity.subscribe_bridge()
    }
}

/// A file on this Mac (a local guy's), streamed from disk.
async fn local_file(path: &str, mime_type: Option<&str>) -> Result<MediaFetch, RpcError> {
    use tokio::io::AsyncReadExt;
    let file = match tokio::fs::File::open(path).await {
        Ok(file) => file,
        Err(e) => return Ok(MediaFetch::Unavailable { code: Some("not_found".into()), reason: Some(e.to_string()) }),
    };
    let size_bytes = file.metadata().await.ok().map(|m| m.len());
    let body = futures_util::stream::unfold(Some(file), |file| async move {
        let mut file = file?;
        let mut chunk = vec![0; 64 * 1024];
        match file.read(&mut chunk).await {
            Ok(0) => None,
            Ok(n) => {
                chunk.truncate(n);
                Some((Ok(chunk), Some(file)))
            }
            Err(e) => Some((Err(e.to_string()), None)),
        }
    })
    .boxed();
    Ok(MediaFetch::Available { mime_type: mime_type.map(str::to_string), size_bytes, body })
}
