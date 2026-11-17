//! Client for mihomo's REST API over its unix-socket controller.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{header, Method, Request, StatusCode};
use hyper_util::rt::TokioIo;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::{json, Value};
use tokio::net::UnixStream;

const TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Debug, Clone)]
pub struct Api {
    socket: PathBuf,
}

impl Api {
    pub fn new(socket: impl Into<PathBuf>) -> Self {
        Self {
            socket: socket.into(),
        }
    }

    pub async fn version(&self) -> Result<String> {
        let v = self.request(Method::GET, "/version", None).await?;
        Ok(v["version"].as_str().unwrap_or_default().to_owned())
    }

    /// Make mihomo load the config file at `path`.
    pub async fn reload(&self, path: &Path) -> Result<()> {
        let body = json!({ "path": path.to_string_lossy() });
        self.request(Method::PUT, "/configs?force=true", Some(body))
            .await?;
        Ok(())
    }

    /// All proxies and groups, as mihomo reports them.
    pub async fn proxies(&self) -> Result<Value> {
        self.request(Method::GET, "/proxies", None).await
    }

    /// Pick `name` in the selector group `group`.
    pub async fn select(&self, group: &str, name: &str) -> Result<()> {
        let path = format!("/proxies/{}", enc(group));
        self.request(Method::PUT, &path, Some(json!({ "name": name })))
            .await?;
        Ok(())
    }

    /// Latency of `proxy` in ms.
    pub async fn delay(&self, proxy: &str, url: &str, timeout_ms: u32) -> Result<u32> {
        let path = format!(
            "/proxies/{}/delay?url={}&timeout={timeout_ms}",
            enc(proxy),
            enc(url)
        );
        let (status, v) = self.raw(Method::GET, &path, None).await?;
        // mihomo answers 503/504 when the probe fails or times out.
        if matches!(status.as_u16(), 408 | 503 | 504) {
            bail!("`{proxy}` did not respond within {timeout_ms} ms");
        }
        check(status, &path, &v)?;
        v["delay"]
            .as_u64()
            .map(|d| d as u32)
            .context("no delay in response")
    }

    async fn request(&self, method: Method, path: &str, body: Option<Value>) -> Result<Value> {
        let (status, value) = self.raw(method, path, body).await?;
        check(status, path, &value)?;
        Ok(value)
    }

    async fn raw(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        tokio::time::timeout(TIMEOUT, self.request_inner(method, path, body))
            .await
            .context("mihomo API timed out")?
    }

    async fn request_inner(
        &self,
        method: Method,
        path: &str,
        body: Option<Value>,
    ) -> Result<(StatusCode, Value)> {
        let stream = UnixStream::connect(&self.socket)
            .await
            .with_context(|| format!("connecting to {}", self.socket.display()))?;
        let (mut sender, conn) =
            hyper::client::conn::http1::handshake(TokioIo::new(stream)).await?;
        tokio::spawn(conn);

        let body = body.map(|b| b.to_string()).unwrap_or_default();
        let req = Request::builder()
            .method(method)
            .uri(path)
            .header(header::HOST, "mihomo")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Full::new(Bytes::from(body)))?;
        let resp = sender.send_request(req).await?;
        let status = resp.status();
        let bytes = resp.into_body().collect().await?.to_bytes();
        let value: Value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok((status, value))
    }
}

fn check(status: StatusCode, path: &str, value: &Value) -> Result<()> {
    if status.is_success() {
        return Ok(());
    }
    let endpoint = path.split('?').next().unwrap_or(path);
    let msg = value["message"].as_str().unwrap_or("");
    bail!("mihomo API {endpoint}: {status} {msg}")
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}
