//! Client MCP (Model Context Protocol) minimal : transports stdio et Streamable HTTP,
//! compatible avec les serveurs « modernes » (2026-07-28, sans état) et « legacy »
//! (poignée de main `initialize`, 2025-11-25 et antérieurs).
//!
//! Configuration au format de Claude Desktop :
//! `{"mcpServers": {"nom": {"command": "npx", "args": [...], "env": {...}} | {"url": "...", "headers": {...}}}}`

use std::collections::{HashMap, VecDeque};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin};
use tokio::sync::{oneshot, Mutex};

type R<T> = Result<T, String>;

const MODERN_VERSION: &str = "2026-07-28";
const LEGACY_VERSION: &str = "2025-11-25";
const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Debug, Clone, Deserialize, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ServerConfig {
    pub command: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    pub cwd: Option<String>,
    pub url: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct McpConfig {
    #[serde(default)]
    pub mcp_servers: HashMap<String, ServerConfig>,
}

pub fn parse_config(text: &str) -> R<McpConfig> {
    if text.trim().is_empty() {
        return Ok(McpConfig::default());
    }
    let cfg: McpConfig = serde_json::from_str(text).map_err(|e| format!("JSON invalide : {e}"))?;
    for (name, s) in &cfg.mcp_servers {
        match (&s.command, &s.url) {
            (Some(_), None) | (None, Some(_)) => {}
            _ => return Err(format!("Serveur « {name} » : indiquez soit `command`, soit `url`")),
        }
    }
    Ok(cfg)
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolInfo {
    /// Nom exposé au modèle (`mcp_<serveur>_<outil>`).
    pub exposed: String,
    pub name: String,
    pub description: String,
    #[serde(skip)]
    pub schema: Value,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ServerStatus {
    pub name: String,
    pub state: &'static str, // starting | ready | error | disabled
    pub error: Option<String>,
    pub transport: &'static str,
    pub protocol: Option<String>,
    pub tools: Vec<ToolInfo>,
}

// ------------------------------------------------------------------ transports

type Pending = Arc<StdMutex<HashMap<u64, oneshot::Sender<Value>>>>;

struct Stdio_ {
    stdin: Arc<Mutex<ChildStdin>>,
    pending: Pending,
    child: Mutex<Child>,
    stderr_tail: Arc<StdMutex<VecDeque<String>>>,
}

struct Http {
    client: reqwest::Client,
    url: String,
    headers: HashMap<String, String>,
    session_id: StdMutex<Option<String>>,
}

enum Transport {
    Stdio(Box<Stdio_>),
    Http(Http),
}

#[derive(Clone, PartialEq)]
enum Era {
    Modern(String),
    Legacy(String),
}

struct Conn {
    transport: Transport,
    era: StdMutex<Era>,
    next_id: AtomicU64,
}

/// Erreur JSON-RPC renvoyée par le serveur.
struct RpcError {
    code: i64,
    message: String,
    data: Value,
}

enum CallError {
    Rpc(RpcError),
    Other(String),
}

impl From<String> for CallError {
    fn from(s: String) -> Self {
        CallError::Other(s)
    }
}

impl CallError {
    fn text(self) -> String {
        match self {
            CallError::Rpc(e) => format!("{} (code {})", e.message, e.code),
            CallError::Other(s) => s,
        }
    }
}

fn client_info() -> Value {
    json!({ "name": "ollama-desktop", "version": env!("CARGO_PKG_VERSION") })
}

/// Encodage des valeurs d'en-tête non ASCII (spécification 2026-07-28).
fn header_value(s: &str) -> String {
    use base64::Engine;
    if s.chars().all(|c| c.is_ascii_graphic() || c == ' ') && !s.starts_with(' ') && !s.ends_with(' ') {
        s.to_owned()
    } else {
        format!("=?base64?{}?=", base64::engine::general_purpose::STANDARD.encode(s))
    }
}

fn parse_sse_for_id(body: &str, id: u64) -> Option<Value> {
    let mut data = String::new();
    for line in body.lines().chain(std::iter::once("")) {
        if let Some(d) = line.strip_prefix("data:") {
            data.push_str(d.trim_start());
            data.push('\n');
        } else if line.is_empty() && !data.is_empty() {
            if let Ok(v) = serde_json::from_str::<Value>(data.trim()) {
                if v["id"].as_u64() == Some(id) {
                    return Some(v);
                }
            }
            data.clear();
        }
    }
    None
}

impl Conn {
    fn era(&self) -> Era {
        self.era.lock().unwrap().clone()
    }

    async fn send_raw(&self, msg: &Value, method: &str, name: Option<&str>, timeout: Duration) -> Result<Option<Value>, CallError> {
        let id = msg["id"].as_u64();
        match &self.transport {
            Transport::Stdio(s) => {
                let rx = id.map(|id| {
                    let (tx, rx) = oneshot::channel();
                    s.pending.lock().unwrap().insert(id, tx);
                    rx
                });
                let mut line = serde_json::to_string(msg).map_err(|e| e.to_string())?;
                line.push('\n');
                {
                    let mut stdin = s.stdin.lock().await;
                    stdin.write_all(line.as_bytes()).await.map_err(|e| format!("Serveur arrêté : {e}"))?;
                    stdin.flush().await.map_err(|e| e.to_string())?;
                }
                let Some(rx) = rx else { return Ok(None) };
                match tokio::time::timeout(timeout, rx).await {
                    Ok(Ok(v)) => Ok(Some(v)),
                    Ok(Err(_)) => Err(CallError::Other(format!("Le serveur s'est arrêté. {}", self.stderr_tail()))),
                    Err(_) => {
                        s.pending.lock().unwrap().remove(&id.unwrap());
                        Err(CallError::Other("Délai de réponse dépassé".into()))
                    }
                }
            }
            Transport::Http(h) => {
                let mut req = h
                    .client
                    .post(&h.url)
                    .timeout(timeout)
                    .header("Content-Type", "application/json")
                    .header("Accept", "application/json, text/event-stream")
                    .header("Mcp-Method", method);
                let version = match self.era() {
                    Era::Modern(v) | Era::Legacy(v) => v,
                };
                req = req.header("MCP-Protocol-Version", version);
                if let Some(n) = name {
                    req = req.header("Mcp-Name", header_value(n));
                }
                if let Some(sid) = h.session_id.lock().unwrap().clone() {
                    req = req.header("Mcp-Session-Id", sid);
                }
                for (k, v) in &h.headers {
                    req = req.header(k, v);
                }
                let resp = req.json(msg).send().await.map_err(|e| format!("Connexion impossible : {e}"))?;
                if let Some(sid) = resp.headers().get("mcp-session-id").and_then(|v| v.to_str().ok()) {
                    *h.session_id.lock().unwrap() = Some(sid.to_owned());
                }
                let status = resp.status();
                let is_sse = resp
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|c| c.contains("text/event-stream"));
                let Some(id) = id else { return Ok(None) };
                let body = if is_sse {
                    // Lecture du flux jusqu'à la réponse correspondant à notre requête.
                    let mut buf = String::new();
                    let mut stream = resp.bytes_stream();
                    while let Some(chunk) = stream.next().await {
                        buf.push_str(&String::from_utf8_lossy(&chunk.map_err(|e| e.to_string())?));
                        if let Some(v) = parse_sse_for_id(&buf, id) {
                            return Ok(Some(v));
                        }
                    }
                    return Err(CallError::Other(format!("Flux SSE terminé sans réponse (HTTP {status})")));
                } else {
                    resp.text().await.map_err(|e| e.to_string())?
                };
                match serde_json::from_str::<Value>(&body) {
                    Ok(v) if v.get("result").is_some() || v.get("error").is_some() => Ok(Some(v)),
                    _ if status == reqwest::StatusCode::UNAUTHORIZED => Err(CallError::Other(
                        "Authentification requise (401) : ajoutez un en-tête Authorization dans la configuration".into(),
                    )),
                    _ => Err(CallError::Other(format!("HTTP {status} : {}", body.chars().take(300).collect::<String>()))),
                }
            }
        }
    }

    async fn request_timeout(&self, method: &str, params: Value, timeout: Duration) -> Result<Value, CallError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let mut params = if params.is_object() { params } else { json!({}) };
        if let Era::Modern(v) = self.era() {
            let meta = params.as_object_mut().unwrap().entry("_meta").or_insert_with(|| json!({}));
            meta["io.modelcontextprotocol/protocolVersion"] = json!(v);
            meta["io.modelcontextprotocol/clientInfo"] = client_info();
            meta["io.modelcontextprotocol/clientCapabilities"] = json!({});
        }
        let name = params["name"].as_str().map(str::to_owned);
        let msg = json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params });
        let resp = self.send_raw(&msg, method, name.as_deref(), timeout).await?.ok_or("Réponse vide".to_string())?;
        if let Some(err) = resp.get("error").filter(|e| !e.is_null()) {
            return Err(CallError::Rpc(RpcError {
                code: err["code"].as_i64().unwrap_or(0),
                message: err["message"].as_str().unwrap_or("erreur").to_owned(),
                data: err["data"].clone(),
            }));
        }
        Ok(resp["result"].clone())
    }

    async fn request(&self, method: &str, params: Value) -> Result<Value, CallError> {
        self.request_timeout(method, params, REQUEST_TIMEOUT).await
    }

    async fn notify(&self, method: &str) -> R<()> {
        let msg = json!({ "jsonrpc": "2.0", "method": method });
        self.send_raw(&msg, method, None, REQUEST_TIMEOUT).await.map(|_| ()).map_err(CallError::text)
    }

    fn stderr_tail(&self) -> String {
        match &self.transport {
            Transport::Stdio(s) => {
                let tail: Vec<String> = s.stderr_tail.lock().unwrap().iter().cloned().collect();
                if tail.is_empty() { String::new() } else { format!("Sortie d'erreur :\n{}", tail.join("\n")) }
            }
            Transport::Http(_) => String::new(),
        }
    }

    /// Détermine la génération du serveur, puis effectue la poignée de main si besoin.
    async fn handshake(&self) -> R<()> {
        match self.request_timeout("server/discover", json!({}), PROBE_TIMEOUT).await {
            Ok(res) => {
                let versions: Vec<&str> = res["supportedVersions"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
                if versions.contains(&MODERN_VERSION) {
                    return Ok(());
                }
                // Serveur double génération ne proposant que des versions legacy.
            }
            Err(CallError::Rpc(e)) if e.code == -32022 => {
                let supported: Vec<String> = e.data["supported"].as_array().into_iter().flatten().filter_map(|v| v.as_str().map(str::to_owned)).collect();
                if supported.iter().any(|v| v.as_str() >= MODERN_VERSION) {
                    return Err(format!("Versions MCP proposées non prises en charge : {}", supported.join(", ")));
                }
            }
            Err(_) => {}
        }
        *self.era.lock().unwrap() = Era::Legacy(LEGACY_VERSION.into());
        let res = self
            .request(
                "initialize",
                json!({ "protocolVersion": LEGACY_VERSION, "capabilities": {}, "clientInfo": client_info() }),
            )
            .await
            .map_err(|e| format!("{} {}", e.text(), self.stderr_tail()))?;
        if let Some(v) = res["protocolVersion"].as_str() {
            *self.era.lock().unwrap() = Era::Legacy(v.to_owned());
        }
        self.notify("notifications/initialized").await
    }

    async fn list_tools(&self) -> R<Vec<(String, String, Value)>> {
        let mut out = Vec::new();
        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!({ "cursor": c }),
                None => json!({}),
            };
            let res = self.request("tools/list", params).await.map_err(CallError::text)?;
            for t in res["tools"].as_array().into_iter().flatten() {
                let Some(name) = t["name"].as_str() else { continue };
                let desc = t["description"].as_str().or(t["title"].as_str()).unwrap_or_default().to_owned();
                let schema = if t["inputSchema"].is_object() { t["inputSchema"].clone() } else { json!({ "type": "object", "properties": {} }) };
                out.push((name.to_owned(), desc, schema));
            }
            cursor = res["nextCursor"].as_str().map(str::to_owned);
            if cursor.is_none() || out.len() > 1000 {
                break;
            }
        }
        Ok(out)
    }

    async fn shutdown(&self) {
        if let Transport::Stdio(s) = &self.transport {
            let _ = s.child.lock().await.kill().await;
        }
    }
}

async fn connect_stdio(cfg: &ServerConfig) -> R<Conn> {
    let command = cfg.command.as_deref().unwrap_or_default();
    #[cfg(windows)]
    let mut cmd = {
        // `cmd /C` pour trouver npx.cmd, uvx.exe… via le PATH.
        let mut c = tokio::process::Command::new("cmd");
        c.arg("/C").arg(command).args(&cfg.args);
        c.creation_flags(0x0800_0000);
        c
    };
    #[cfg(not(windows))]
    let mut cmd = {
        let mut c = tokio::process::Command::new(command);
        c.args(&cfg.args);
        c
    };
    cmd.envs(&cfg.env)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = &cfg.cwd {
        cmd.current_dir(cwd);
    }
    let mut child = cmd.spawn().map_err(|e| format!("Impossible de lancer « {command} » : {e}"))?;
    let stdin = Arc::new(Mutex::new(child.stdin.take().unwrap()));
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    let pending: Pending = Default::default();
    let stderr_tail: Arc<StdMutex<VecDeque<String>>> = Default::default();

    // Lecture de stdout : routage des réponses, réponse aux requêtes du serveur.
    {
        let pending = pending.clone();
        let stdin = stdin.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stdout).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let Ok(msg) = serde_json::from_str::<Value>(&line) else { continue };
                match (msg.get("id").filter(|i| !i.is_null()), msg["method"].as_str()) {
                    (Some(id), None) => {
                        if let Some(tx) = id.as_u64().and_then(|id| pending.lock().unwrap().remove(&id)) {
                            let _ = tx.send(msg);
                        }
                    }
                    (Some(id), Some(method)) => {
                        let reply = match method {
                            "ping" => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
                            "roots/list" => json!({ "jsonrpc": "2.0", "id": id, "result": { "roots": [] } }),
                            _ => json!({ "jsonrpc": "2.0", "id": id, "error": { "code": -32601, "message": "Non pris en charge" } }),
                        };
                        let mut s = stdin.lock().await;
                        let _ = s.write_all(format!("{reply}\n").as_bytes()).await;
                        let _ = s.flush().await;
                    }
                    _ => {} // notifications : ignorées
                }
            }
            // Processus terminé : on réveille les requêtes en attente.
            pending.lock().unwrap().clear();
        });
    }
    {
        let tail = stderr_tail.clone();
        tauri::async_runtime::spawn(async move {
            let mut lines = BufReader::new(stderr).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let mut t = tail.lock().unwrap();
                t.push_back(line);
                if t.len() > 20 {
                    t.pop_front();
                }
            }
        });
    }
    Ok(Conn {
        transport: Transport::Stdio(Box::new(Stdio_ { stdin, pending, child: Mutex::new(child), stderr_tail })),
        era: StdMutex::new(Era::Modern(MODERN_VERSION.into())),
        next_id: AtomicU64::new(1),
    })
}

fn connect_http(cfg: &ServerConfig) -> R<Conn> {
    let url = cfg.url.clone().unwrap_or_default();
    crate::ollama::validate_base_url(&url)?;
    let client = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .build()
        .map_err(|e| e.to_string())?;
    Ok(Conn {
        transport: Transport::Http(Http { client, url, headers: cfg.headers.clone(), session_id: StdMutex::new(None) }),
        era: StdMutex::new(Era::Modern(MODERN_VERSION.into())),
        next_id: AtomicU64::new(1),
    })
}

// ------------------------------------------------------------------ gestionnaire

struct Server {
    name: String,
    transport: &'static str,
    state: StdMutex<(&'static str, Option<String>)>,
    tools: StdMutex<Vec<ToolInfo>>,
    conn: Mutex<Option<Arc<Conn>>>,
}

#[derive(Default)]
pub struct McpManager {
    servers: StdMutex<Vec<Arc<Server>>>,
}

fn sanitize(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || c == '_' || c == '-' { c } else { '_' }).collect()
}

/// Nom de fonction valide pour l'API (`^[a-zA-Z0-9_-]{1,64}$`), unique.
fn exposed_name(server: &str, tool: &str, taken: &[String]) -> String {
    let base: String = format!("mcp_{}_{}", sanitize(server), sanitize(tool)).chars().take(60).collect();
    let mut name = base.clone();
    let mut i = 2;
    while taken.contains(&name) {
        name = format!("{base}{i}");
        i += 1;
    }
    name
}

fn result_to_text(res: &Value) -> (String, bool) {
    let is_error = res["isError"].as_bool().unwrap_or(false);
    if res["resultType"] == "input_required" {
        return ("Le serveur demande une interaction (input_required) non prise en charge par ce client.".into(), true);
    }
    let mut parts = Vec::new();
    for c in res["content"].as_array().into_iter().flatten() {
        match c["type"].as_str() {
            Some("text") => parts.push(c["text"].as_str().unwrap_or_default().to_owned()),
            Some("image") => parts.push(format!("[image {}]", c["mimeType"].as_str().unwrap_or(""))),
            Some("audio") => parts.push("[audio]".into()),
            Some("resource") => {
                let r = &c["resource"];
                parts.push(r["text"].as_str().map(str::to_owned).unwrap_or_else(|| format!("[ressource {}]", r["uri"].as_str().unwrap_or(""))));
            }
            Some("resource_link") => parts.push(format!("[lien] {} {}", c["name"].as_str().unwrap_or(""), c["uri"].as_str().unwrap_or(""))),
            _ => {}
        }
    }
    if parts.is_empty() && !res["structuredContent"].is_null() {
        parts.push(res["structuredContent"].to_string());
    }
    (parts.join("\n"), is_error)
}

impl McpManager {
    pub async fn restart(self: &Arc<Self>, cfg: McpConfig) {
        self.shutdown().await;
        let mut entries: Vec<_> = cfg.mcp_servers.into_iter().collect();
        entries.sort_by(|a, b| a.0.cmp(&b.0));
        let servers: Vec<(Arc<Server>, ServerConfig)> = entries
            .into_iter()
            .map(|(name, sc)| {
                let server = Arc::new(Server {
                    name,
                    transport: if sc.url.is_some() { "http" } else { "stdio" },
                    state: StdMutex::new(if sc.disabled { ("disabled", None) } else { ("starting", None) }),
                    tools: Default::default(),
                    conn: Mutex::new(None),
                });
                (server, sc)
            })
            .collect();
        // La liste doit être publiée avant que les connexions n'aboutissent (assign_names).
        *self.servers.lock().unwrap() = servers.iter().map(|(s, _)| s.clone()).collect();
        for (server, sc) in servers {
            if sc.disabled {
                continue;
            }
            let me = self.clone();
            tauri::async_runtime::spawn(async move {
                let result = async {
                    let conn = if sc.url.is_some() { connect_http(&sc)? } else { connect_stdio(&sc).await? };
                    conn.handshake().await?;
                    let tools = conn.list_tools().await?;
                    Ok::<_, String>((conn, tools))
                }
                .await;
                match result {
                    Ok((conn, tools)) => {
                        *server.conn.lock().await = Some(Arc::new(conn));
                        *server.tools.lock().unwrap() = tools
                            .into_iter()
                            .map(|(name, description, schema)| ToolInfo { exposed: String::new(), name, description, schema })
                            .collect();
                        *server.state.lock().unwrap() = ("ready", None);
                    }
                    Err(e) => *server.state.lock().unwrap() = ("error", Some(e)),
                }
                me.assign_names();
            });
        }
    }

    fn assign_names(&self) {
        let servers = self.servers.lock().unwrap();
        let mut taken = Vec::new();
        for s in servers.iter() {
            for t in s.tools.lock().unwrap().iter_mut() {
                t.exposed = exposed_name(&s.name, &t.name, &taken);
                taken.push(t.exposed.clone());
            }
        }
    }

    pub async fn shutdown(&self) {
        let servers: Vec<_> = std::mem::take(&mut *self.servers.lock().unwrap());
        for s in servers {
            if let Some(c) = s.conn.lock().await.take() {
                c.shutdown().await;
            }
        }
    }

    pub fn status(&self) -> Vec<ServerStatus> {
        self.servers
            .lock()
            .unwrap()
            .iter()
            .map(|s| {
                let (state, error) = s.state.lock().unwrap().clone();
                let protocol = s.conn.try_lock().ok().and_then(|c| {
                    c.as_ref().map(|c| match c.era() {
                        Era::Modern(v) => v,
                        Era::Legacy(v) => format!("{v} (legacy)"),
                    })
                });
                ServerStatus { name: s.name.clone(), state, error, transport: s.transport, protocol, tools: s.tools.lock().unwrap().clone() }
            })
            .collect()
    }

    /// Définitions d'outils (format « function ») pour tous les serveurs prêts.
    pub fn tool_definitions(&self) -> Vec<Value> {
        self.status()
            .into_iter()
            .filter(|s| s.state == "ready")
            .flat_map(|s| {
                let server = s.name.clone();
                s.tools.into_iter().filter(|t| !t.exposed.is_empty()).map(move |t| {
                    let mut desc = format!("[MCP {server}] {}", t.description);
                    if desc.len() > 1000 {
                        let mut cut = 1000;
                        while !desc.is_char_boundary(cut) {
                            cut -= 1;
                        }
                        desc.truncate(cut);
                    }
                    json!({ "type": "function", "function": { "name": t.exposed, "description": desc, "parameters": t.schema } })
                })
            })
            .collect()
    }

    pub async fn call(&self, exposed: &str, arguments: Map<String, Value>) -> R<(String, bool)> {
        let found = self.servers.lock().unwrap().iter().find_map(|s| {
            s.tools.lock().unwrap().iter().find(|t| t.exposed == exposed).map(|t| (s.clone(), t.name.clone()))
        });
        let (server, tool) = found.ok_or_else(|| format!("Outil MCP inconnu : {exposed}"))?;
        let conn = server.conn.lock().await.clone().ok_or("Serveur MCP non connecté")?;
        let res = conn
            .request("tools/call", json!({ "name": tool, "arguments": arguments }))
            .await
            .map_err(CallError::text)?;
        Ok(result_to_text(&res))
    }
}

/// Les applications graphiques macOS/Linux n'héritent pas du PATH du shell
/// (Homebrew, nvm, ~/.local/bin…) : on le récupère une fois au démarrage.
#[cfg(unix)]
pub fn fix_path_env() {
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let out = std::process::Command::new(shell)
            .args(["-ilc", "printf '__PATH__%s__PATH__' \"$PATH\""])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output();
        let _ = tx.send(out);
    });
    if let Ok(Ok(out)) = rx.recv_timeout(Duration::from_secs(3)) {
        let s = String::from_utf8_lossy(&out.stdout);
        if let Some(path) = s.split("__PATH__").nth(1).filter(|p| !p.is_empty()) {
            std::env::set_var("PATH", path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_accepts_claude_desktop_format() {
        let cfg = parse_config(r#"{"mcpServers":{"fs":{"command":"npx","args":["-y","x"]},"web":{"url":"https://a.b/mcp","headers":{"Authorization":"Bearer t"}}}}"#).unwrap();
        assert_eq!(cfg.mcp_servers.len(), 2);
        assert!(parse_config(r#"{"mcpServers":{"bad":{}}}"#).is_err());
    }

    #[test]
    fn exposed_names_are_valid_and_unique() {
        let a = exposed_name("my server", "read.file", &[]);
        assert_eq!(a, "mcp_my_server_read_file");
        let b = exposed_name("my server", "read.file", std::slice::from_ref(&a));
        assert_ne!(a, b);
        assert!(exposed_name(&"x".repeat(100), "t", &[]).len() <= 64);
    }

    #[test]
    fn sse_parsing_finds_matching_response() {
        let body = "event: message\ndata: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\"}\n\ndata: {\"jsonrpc\":\"2.0\",\"id\":3,\"result\":{\"ok\":1}}\n\n";
        assert_eq!(parse_sse_for_id(body, 3).unwrap()["result"]["ok"], 1);
        assert!(parse_sse_for_id(body, 4).is_none());
    }

    #[test]
    fn tool_results_become_text() {
        let (t, e) = result_to_text(&json!({"content":[{"type":"text","text":"a"},{"type":"image","mimeType":"image/png"}],"isError":true}));
        assert_eq!(t, "a\n[image image/png]");
        assert!(e);
    }
}
