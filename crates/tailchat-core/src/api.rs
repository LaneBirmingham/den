//! Local control API.
//!
//! Every running tailchat (app, terminal client, headless server) listens on
//! a random loopback port. Requests are newline-delimited JSON:
//!
//! ```text
//! → {"id":1,"token":"…","method":"message.send","params":{"text":"hi"}}
//! ← {"id":1,"ok":true,"result":{…}}
//! → {"id":2,"token":"…","method":"events.subscribe","params":{"events":["message.*"]}}
//! ← {"id":2,"ok":true,"result":{"subscribed":["message.*"]}}
//! ← {"event":"message.created","data":{…}}            (one line per event)
//! ```
//!
//! The address and token are passed to mods as `TAILCHAT_API` and
//! `TAILCHAT_API_TOKEN`, and written to `<state>/instances/<pid>.json` so the
//! `tailchat api` CLI (and agents) can find a running instance.
//! `api.methods` describes everything a node supports.

use std::future::Future;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::Arc;

use anyhow::{Context, bail};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::task::AbortHandle;

use crate::events::{self, Event};
use crate::paths::Paths;

pub type ApiResult = Result<Value, String>;
pub type ApiFuture = Pin<Box<dyn Future<Output = ApiResult> + Send>>;

#[derive(Clone, Debug, Serialize)]
pub struct MethodDoc {
    pub name: &'static str,
    pub params: &'static str,
    pub description: &'static str,
}

pub const fn doc(name: &'static str, params: &'static str, description: &'static str) -> MethodDoc {
    MethodDoc { name, params, description }
}

/// A node that can be driven through the API.
pub trait ApiHandler: Send + Sync + 'static {
    fn call(&self, method: &str, params: Value) -> ApiFuture;
    fn events(&self) -> broadcast::Receiver<Event>;
    fn methods(&self) -> Vec<MethodDoc>;
    /// `client` or `server`.
    fn kind(&self) -> &'static str;
    /// Human label for instance listings (room name, nick…).
    fn label(&self) -> String;
}

/// Turn an immediate value into an [`ApiFuture`].
pub fn ready(v: ApiResult) -> ApiFuture {
    Box::pin(std::future::ready(v))
}

pub fn param_str(params: &Value, key: &str) -> Option<String> {
    params.get(key).and_then(Value::as_str).map(str::to_owned)
}

pub fn require_str(params: &Value, key: &str) -> Result<String, String> {
    param_str(params, key).ok_or_else(|| format!("missing string param `{key}`"))
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct InstanceInfo {
    pub addr: String,
    pub token: String,
    pub pid: u32,
    pub kind: String,
    pub label: String,
    pub started: i64,
}

pub struct ApiServer {
    pub addr: SocketAddr,
    pub token: String,
    file: PathBuf,
    task: AbortHandle,
}

impl ApiServer {
    pub async fn start(handler: Arc<dyn ApiHandler>, paths: &Paths) -> anyhow::Result<ApiServer> {
        let listener = TcpListener::bind("127.0.0.1:0").await.context("binding control API")?;
        let addr = listener.local_addr()?;
        let token = format!("{:016x}{:016x}", rand::random::<u64>(), rand::random::<u64>());
        let info = InstanceInfo {
            addr: addr.to_string(),
            token: token.clone(),
            pid: std::process::id(),
            kind: handler.kind().into(),
            label: handler.label(),
            started: crate::now_ms(),
        };
        let dir = paths.instances_dir();
        std::fs::create_dir_all(&dir)?;
        let file = dir.join(format!("{}.json", std::process::id()));
        write_private(&file, &serde_json::to_vec_pretty(&info)?)?;

        let tok = token.clone();
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else { continue };
                let h = handler.clone();
                let t = tok.clone();
                tokio::spawn(async move {
                    if let Err(e) = serve_conn(h, stream, t).await {
                        tracing::debug!("api connection: {e:#}");
                    }
                });
            }
        })
        .abort_handle();
        tracing::info!("control API on {addr}");
        Ok(ApiServer { addr, token, file, task })
    }

    /// Environment variables handed to mods.
    pub fn env(&self) -> Vec<(String, String)> {
        vec![("TAILCHAT_API".into(), self.addr.to_string()), ("TAILCHAT_API_TOKEN".into(), self.token.clone())]
    }
}

impl Drop for ApiServer {
    fn drop(&mut self) {
        self.task.abort();
        let _ = std::fs::remove_file(&self.file);
    }
}

fn write_private(path: &PathBuf, bytes: &[u8]) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create(true).truncate(true).mode(0o600).open(path)?;
        f.write_all(bytes)
    }
    #[cfg(not(unix))]
    {
        std::fs::write(path, bytes)
    }
}

#[derive(Deserialize)]
struct Request {
    #[serde(default)]
    id: Value,
    #[serde(default)]
    token: String,
    method: String,
    #[serde(default)]
    params: Value,
}

fn constant_time_eq(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

async fn serve_conn(handler: Arc<dyn ApiHandler>, stream: TcpStream, token: String) -> anyhow::Result<()> {
    let (rd, mut wr) = stream.into_split();
    let mut lines = BufReader::new(rd).lines();
    while let Some(line) = lines.next_line().await? {
        if line.trim().is_empty() {
            continue;
        }
        let req: Request = match serde_json::from_str(&line) {
            Ok(r) => r,
            Err(e) => {
                write_line(&mut wr, &json!({"id": null, "ok": false, "error": format!("bad request: {e}")})).await?;
                continue;
            }
        };
        if !constant_time_eq(&req.token, &token) {
            write_line(&mut wr, &json!({"id": req.id, "ok": false, "error": "bad token"})).await?;
            continue;
        }
        let params = if req.params.is_null() { json!({}) } else { req.params };
        match req.method.as_str() {
            "events.subscribe" => {
                let filter: Vec<String> = params.get("events").and_then(|v| serde_json::from_value(v.clone()).ok()).unwrap_or_default();
                let mut rx = handler.events();
                write_line(&mut wr, &json!({"id": req.id, "ok": true, "result": {"subscribed": filter}})).await?;
                loop {
                    tokio::select! {
                        ev = rx.recv() => match ev {
                            Ok(ev) if events::matches_any(&filter, &ev.event) => write_line(&mut wr, &ev).await?,
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(n)) => {
                                write_line(&mut wr, &json!({"event": "api.lagged", "data": {"skipped": n}})).await?;
                            }
                            Err(broadcast::error::RecvError::Closed) => return Ok(()),
                        },
                        // Any input (or EOF) after subscribing ends the stream.
                        _ = lines.next_line() => return Ok(()),
                    }
                }
            }
            "api.methods" => {
                let mut methods = vec![
                    doc("api.methods", "{}", "List methods this node supports"),
                    doc("api.ping", "{}", "Liveness check"),
                    doc("events.subscribe", "{events?: [\"message.*\", ...]}", "Stream events as JSON lines"),
                ];
                methods.extend(handler.methods());
                let result = json!({"kind": handler.kind(), "methods": methods, "events": events::names::ALL});
                write_line(&mut wr, &json!({"id": req.id, "ok": true, "result": result})).await?;
            }
            "api.ping" => {
                write_line(&mut wr, &json!({"id": req.id, "ok": true, "result": {"pong": true, "kind": handler.kind()}})).await?;
            }
            method => {
                let resp = match handler.call(method, params).await {
                    Ok(result) => json!({"id": req.id, "ok": true, "result": result}),
                    Err(error) => json!({"id": req.id, "ok": false, "error": error}),
                };
                write_line(&mut wr, &resp).await?;
            }
        }
    }
    Ok(())
}

async fn write_line<W: AsyncWriteExt + Unpin, T: Serialize>(w: &mut W, v: &T) -> anyhow::Result<()> {
    let mut buf = serde_json::to_vec(v)?;
    buf.push(b'\n');
    w.write_all(&buf).await?;
    w.flush().await?;
    Ok(())
}

/// Running instances, newest first. Stale files are left for [`ApiClient::discover`] to prune.
pub fn list_instances(paths: &Paths) -> Vec<(PathBuf, InstanceInfo)> {
    let mut out: Vec<(PathBuf, InstanceInfo)> = std::fs::read_dir(paths.instances_dir())
        .into_iter()
        .flatten()
        .flatten()
        .filter_map(|e| {
            let p = e.path();
            let info: InstanceInfo = serde_json::from_slice(&std::fs::read(&p).ok()?).ok()?;
            Some((p, info))
        })
        .collect();
    out.sort_by_key(|b| std::cmp::Reverse(b.1.started));
    out
}

/// Client for the control API (used by `tailchat api` and tests).
pub struct ApiClient {
    lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    wr: tokio::net::tcp::OwnedWriteHalf,
    token: String,
    next_id: u64,
}

impl ApiClient {
    pub async fn connect(addr: &str, token: &str) -> anyhow::Result<ApiClient> {
        let stream = TcpStream::connect(addr).await.with_context(|| format!("connecting to control API {addr}"))?;
        let (rd, wr) = stream.into_split();
        Ok(ApiClient { lines: BufReader::new(rd).lines(), wr, token: token.into(), next_id: 1 })
    }

    /// `$TAILCHAT_API`/`$TAILCHAT_API_TOKEN` if set, otherwise the newest live
    /// instance (optionally of `kind`), pruning stale instance files.
    pub async fn discover(paths: &Paths, kind: Option<&str>) -> anyhow::Result<(ApiClient, InstanceInfo)> {
        if let (Ok(addr), Ok(token)) = (std::env::var("TAILCHAT_API"), std::env::var("TAILCHAT_API_TOKEN")) {
            let c = ApiClient::connect(&addr, &token).await?;
            let info = InstanceInfo { addr, token, pid: 0, kind: "env".into(), label: String::new(), started: 0 };
            return Ok((c, info));
        }
        for (path, info) in list_instances(paths) {
            if kind.is_some_and(|k| k != info.kind) {
                continue;
            }
            match ApiClient::connect(&info.addr, &info.token).await {
                Ok(mut c) => {
                    if c.call("api.ping", json!({})).await.is_ok() {
                        return Ok((c, info));
                    }
                }
                Err(_) => {
                    let _ = std::fs::remove_file(&path);
                }
            }
        }
        bail!("no running tailchat found (start the app, `tailchat chat`, or `tailchat serve`)")
    }

    pub async fn call(&mut self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id;
        self.next_id += 1;
        let req = json!({"id": id, "token": self.token, "method": method, "params": params});
        write_line(&mut self.wr, &req).await?;
        let line = self.lines.next_line().await?.context("control API closed the connection")?;
        let resp: Value = serde_json::from_str(&line)?;
        if resp["ok"].as_bool() == Some(true) {
            Ok(resp["result"].clone())
        } else {
            bail!("{}", resp["error"].as_str().unwrap_or("request failed"))
        }
    }

    /// Switch this connection to event streaming.
    pub async fn subscribe(mut self, events: &[String]) -> anyhow::Result<EventStream> {
        self.call("events.subscribe", json!({"events": events})).await?;
        Ok(EventStream { lines: self.lines, _wr: self.wr })
    }
}

pub struct EventStream {
    lines: tokio::io::Lines<BufReader<tokio::net::tcp::OwnedReadHalf>>,
    _wr: tokio::net::tcp::OwnedWriteHalf,
}

impl EventStream {
    pub async fn next(&mut self) -> anyhow::Result<Option<Event>> {
        match self.lines.next_line().await? {
            Some(line) => Ok(Some(serde_json::from_str(&line)?)),
            None => Ok(None),
        }
    }
}
