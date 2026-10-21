//! Client pour l'API native d'Ollama (`/api/*`) : chat en streaming avec appels d'outils
//! et raisonnement, gestion des modèles (liste, capacités, téléchargement, suppression,
//! déchargement). Plus une transcription audio vers un serveur compatible OpenAI.

use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::ipc::Channel;
use tokio::sync::Notify;

pub const DEFAULT_BASE_URL: &str = "http://localhost:11434";

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    pub role: String,
    pub content: String,
    /// Images en data URL (`data:image/png;base64,…`) ou base64 brut.
    #[serde(default)]
    pub images: Vec<String>,
    /// Appels d'outils d'un message assistant : `[{id, name, arguments}]`.
    #[serde(default)]
    pub tool_calls: Vec<ToolCall>,
    /// Pour un message `tool` : l'appel auquel il répond.
    pub tool_call_id: Option<String>,
    pub name: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// JSON encodé en chaîne (Ollama renvoie un objet : il est sérialisé pour l'interface).
    pub arguments: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<ChatMessage>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    /// Définitions d'outils (`[{type: "function", function: {...}}]`).
    #[serde(default)]
    pub tools: Vec<Value>,
    /// « none » pour interdire les appels (résumé de contexte) : les outils ne sont alors pas envoyés.
    pub tool_choice: Option<String>,
    /// Fenêtre de contexte demandée au serveur (`options.num_ctx`).
    pub num_ctx: Option<u32>,
    /// Raisonnement : booléen, ou niveau (« low », « medium », « high ») pour gpt-oss.
    pub think: Option<Value>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum StreamEvent {
    Delta { text: String },
    Reasoning { text: String },
    #[serde(rename_all = "camelCase")]
    Usage { prompt_tokens: u64, completion_tokens: u64, tokens_per_sec: Option<f64> },
    ToolCalls { calls: Vec<ToolCall> },
    /// Nouvel essai après une erreur temporaire (503, réseau).
    #[serde(rename_all = "camelCase")]
    Retry { attempt: u32, max_attempts: u32, delay_secs: u64, reason: String },
    #[serde(rename_all = "camelCase")]
    Done { finish_reason: Option<String>, cancelled: bool },
}

#[derive(Debug, Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ModelInfo {
    pub id: String,
    pub family: Option<String>,
    pub parameter_size: Option<String>,
    pub quantization: Option<String>,
    /// Taille sur disque, en octets.
    pub size: u64,
    pub modified_at: Option<String>,
    pub vision: bool,
    pub tools: bool,
    pub thinking: bool,
    /// Contexte maximal du modèle (métadonnées GGUF).
    pub max_context_length: Option<u64>,
    /// Modèle exécuté sur un serveur distant (modèles « cloud »).
    pub remote: bool,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RunningModel {
    pub name: String,
    pub size: u64,
    pub size_vram: u64,
    pub expires_at: Option<String>,
    pub context_length: Option<u64>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PullProgress {
    pub status: String,
    /// Couche en cours de téléchargement.
    pub digest: Option<String>,
    pub total: Option<u64>,
    pub completed: Option<u64>,
}

/// Délai max avant les en-têtes de réponse (chargement du modèle + lecture de l'invite),
/// puis entre deux morceaux du flux.
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(600);
const STALL_TIMEOUT: Duration = Duration::from_secs(300);
const MAX_ATTEMPTS: u32 = 4;

fn is_retryable(status: reqwest::StatusCode) -> bool {
    matches!(status.as_u16(), 429 | 502 | 503 | 504)
}

/// Hôte d'une URL : adresse IP ou nom (en minuscules, sans point final).
enum Host {
    Ip(std::net::IpAddr),
    Name(String),
}

fn host_of(u: &reqwest::Url) -> Option<Host> {
    let h = u.host_str()?.trim_start_matches('[').trim_end_matches(']');
    Some(match h.parse() {
        Ok(ip) => Host::Ip(ip),
        Err(_) => Host::Name(h.trim_end_matches('.').to_ascii_lowercase()),
    })
}

/// Hôte considéré comme local : boucle locale, IP privée, `.local`, `.lan`, nom sans domaine…
fn is_local_host(host: &Host) -> bool {
    match host {
        Host::Ip(ip) => crate::web::is_private(*ip),
        Host::Name(n) => {
            n == "localhost"
                || !n.contains('.')
                || [".localhost", ".local", ".lan", ".home", ".home.arpa", ".internal", ".intranet", ".ts.net"]
                    .iter()
                    .any(|s| n.ends_with(s))
        }
    }
}

/// HTTPS partout ; HTTP en clair accepté pour un serveur local ou du réseau privé
/// (Ollama est souvent sur une machine du LAN, sans TLS).
pub fn validate_base_url(url: &str) -> Result<(), String> {
    let u = reqwest::Url::parse(url).map_err(|e| format!("URL invalide : {e}"))?;
    match u.scheme() {
        "https" => Ok(()),
        "http" if host_of(&u).is_some_and(|h| is_local_host(&h)) => Ok(()),
        _ => Err("L'URL doit être en HTTPS (HTTP accepté seulement pour localhost ou le réseau local)".into()),
    }
}

/// Le serveur est-il sur cette machine ? (une clé API en HTTP n'y transite pas sur le réseau)
pub fn is_loopback(url: &str) -> bool {
    reqwest::Url::parse(url).ok().and_then(|u| host_of(&u)).is_some_and(|h| match h {
        Host::Ip(ip) => ip.is_loopback(),
        Host::Name(n) => n == "localhost",
    })
}

fn endpoint(base: &str, path: &str) -> String {
    let base = base.trim().trim_end_matches('/');
    let base = base.strip_suffix("/api").or_else(|| base.strip_suffix("/v1")).unwrap_or(base);
    format!("{base}/api/{path}")
}

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        .user_agent(concat!("ollama-desktop/", env!("CARGO_PKG_VERSION")))
        .connect_timeout(Duration::from_secs(10))
        .build()
        .map_err(|e| e.to_string())
}

/// Requête avec la clé API éventuelle (Ollama derrière un proxy authentifiant, ollama.com).
fn authed(req: reqwest::RequestBuilder, key: &str) -> reqwest::RequestBuilder {
    if key.is_empty() {
        req
    } else {
        req.bearer_auth(key)
    }
}

fn connect_error(base: &str, e: &reqwest::Error) -> String {
    if e.is_connect() {
        format!("Ollama injoignable à {base} : vérifiez qu'il est lancé (« ollama serve ») et l'URL dans les réglages.")
    } else if e.is_timeout() {
        format!("Ollama ne répond pas à {base} (délai dépassé).")
    } else {
        format!("Connexion impossible à {base} : {e}")
    }
}

/// Message d'erreur lisible à partir d'une réponse `{"error": "…"}`.
fn explain(status: u16, detail: &str, model: Option<&str>) -> String {
    let l = detail.to_lowercase();
    if l.contains("does not support tools") {
        return "Ce modèle ne prend pas en charge les appels d'outils (agent de code, recherche web, MCP). \
                Choisissez un modèle compatible, par ex. qwen3, devstral, mistral-small3.2 ou gpt-oss."
            .into();
    }
    if l.contains("does not support thinking") {
        return "Ce modèle ne gère pas le raisonnement : désactivez-le dans Réglages → Général.".into();
    }
    if status == 404 && l.contains("not found") {
        return match model {
            Some(m) => format!("Le modèle « {m} » n'est pas installé sur le serveur Ollama : téléchargez-le depuis le gestionnaire de modèles."),
            None => format!("Introuvable (404) : {detail}"),
        };
    }
    if l.contains("requires more system memory") || l.contains("out of memory") || l.contains("cudamalloc") {
        return format!("Mémoire insuffisante pour ce modèle : {detail}. Essayez un modèle plus petit, une quantification plus forte ou une fenêtre de contexte réduite (Réglages → Général).");
    }
    match status {
        401 | 403 => "Accès refusé par le serveur (401/403) : vérifiez la clé API dans les réglages.".into(),
        _ => format!("Erreur Ollama ({status}) : {detail}"),
    }
}

async fn api_error(resp: reqwest::Response, model: Option<&str>) -> String {
    let status = resp.status().as_u16();
    let body = resp.text().await.unwrap_or_default();
    let detail = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|v| v.get("error").map(|m| m.as_str().map(str::to_owned).unwrap_or_else(|| m.to_string())))
        .unwrap_or(body);
    explain(status, detail.trim(), model)
}

/// Version du serveur : sert aussi de test de connexion.
pub async fn version(base: &str, key: &str) -> Result<String, String> {
    let resp = authed(client()?.get(endpoint(base, "version")), key)
        .timeout(Duration::from_secs(8))
        .send()
        .await
        .map_err(|e| connect_error(base, &e))?;
    if !resp.status().is_success() {
        return Err(api_error(resp, None).await);
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(v["version"].as_str().unwrap_or("?").to_owned())
}

/// Contexte maximal dans `model_info` : clé `<architecture>.context_length`.
fn context_length(info: &Value) -> Option<u64> {
    let arch = info["general.architecture"].as_str();
    arch.and_then(|a| info[format!("{a}.context_length")].as_u64()).or_else(|| {
        info.as_object()?
            .iter()
            .find(|(k, _)| k.ends_with(".context_length"))
            .and_then(|(_, v)| v.as_u64())
    })
}

fn apply_show(m: &mut ModelInfo, show: &Value) {
    if let Some(caps) = show["capabilities"].as_array() {
        let has = |c: &str| caps.iter().any(|x| x == c);
        m.vision = has("vision");
        m.tools = has("tools");
        m.thinking = has("thinking");
    }
    m.max_context_length = context_length(&show["model_info"]);
}

/// Modèle utilisable en chat ? (exclut les modèles d'embedding seuls)
fn is_chat_model(show: &Value) -> bool {
    match show["capabilities"].as_array() {
        Some(caps) if !caps.is_empty() => caps.iter().any(|c| c == "completion"),
        _ => true,
    }
}

async fn show(client: &reqwest::Client, base: &str, key: &str, model: &str) -> Option<Value> {
    let resp = authed(client.post(endpoint(base, "show")), key)
        .json(&json!({ "model": model }))
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .ok()?;
    resp.status().is_success().then_some(())?;
    resp.json().await.ok()
}

pub async fn list_models(base: &str, key: &str) -> Result<Vec<ModelInfo>, String> {
    let client = client()?;
    let resp = authed(client.get(endpoint(base, "tags")), key)
        .timeout(Duration::from_secs(20))
        .send()
        .await
        .map_err(|e| connect_error(base, &e))?;
    if !resp.status().is_success() {
        return Err(api_error(resp, None).await);
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    let mut models: Vec<ModelInfo> = body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let d = &m["details"];
            Some(ModelInfo {
                id: m["name"].as_str().or_else(|| m["model"].as_str())?.to_owned(),
                family: d["family"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
                parameter_size: d["parameter_size"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
                quantization: d["quantization_level"].as_str().filter(|s| !s.is_empty()).map(str::to_owned),
                size: m["size"].as_u64().unwrap_or(0),
                modified_at: m["modified_at"].as_str().map(str::to_owned),
                // Capacités inconnues tant que /api/show n'a pas répondu : on ne bloque rien.
                vision: true,
                tools: true,
                thinking: false,
                max_context_length: None,
                remote: m["remote_host"].as_str().is_some_and(|h| !h.is_empty()),
            })
        })
        .collect();
    // Capacités et contexte de chaque modèle, en parallèle (requêtes locales et rapides).
    let shows = futures_util::future::join_all(models.iter().map(|m| show(&client, base, key, &m.id))).await;
    let mut keep = Vec::with_capacity(models.len());
    for (mut m, s) in models.drain(..).zip(shows) {
        if let Some(s) = s {
            if !is_chat_model(&s) {
                continue;
            }
            apply_show(&mut m, &s);
        }
        keep.push(m);
    }
    keep.sort_by(|a, b| a.id.cmp(&b.id));
    Ok(keep)
}

pub async fn running_models(base: &str, key: &str) -> Result<Vec<RunningModel>, String> {
    let resp = authed(client()?.get(endpoint(base, "ps")), key)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| connect_error(base, &e))?;
    if !resp.status().is_success() {
        return Err(api_error(resp, None).await);
    }
    let body: Value = resp.json().await.map_err(|e| e.to_string())?;
    Ok(body["models"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|m| {
            Some(RunningModel {
                name: m["name"].as_str().or_else(|| m["model"].as_str())?.to_owned(),
                size: m["size"].as_u64().unwrap_or(0),
                size_vram: m["size_vram"].as_u64().unwrap_or(0),
                expires_at: m["expires_at"].as_str().map(str::to_owned),
                context_length: m["context_length"].as_u64(),
            })
        })
        .collect())
}

/// Télécharge (ou met à jour) un modèle depuis le registre, avec progression.
/// Un téléchargement interrompu reprend là où il s'était arrêté.
pub async fn pull_model(base: &str, key: &str, model: &str, on_progress: &Channel<PullProgress>, cancel: Arc<Notify>) -> Result<bool, String> {
    let resp = tokio::select! {
        r = authed(client()?.post(endpoint(base, "pull")), key).json(&json!({ "model": model, "stream": true })).send() => r.map_err(|e| connect_error(base, &e))?,
        _ = cancel.notified() => return Ok(false),
    };
    if !resp.status().is_success() {
        return Err(api_error(resp, None).await);
    }
    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    loop {
        let chunk = tokio::select! {
            c = tokio::time::timeout(STALL_TIMEOUT, stream.next()) => c.map_err(|_| "Téléchargement bloqué : aucune donnée depuis 5 min".to_string())?,
            _ = cancel.notified() => return Ok(false),
        };
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk.map_err(|e| format!("Téléchargement interrompu : {e}"))?);
        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            if let Some(err) = v["error"].as_str() {
                return Err(if err.contains("file does not exist") || err.contains("not found") {
                    format!("Modèle « {model} » introuvable dans le registre Ollama (vérifiez le nom et l'étiquette, ex. qwen3:8b).")
                } else {
                    format!("Téléchargement impossible : {err}")
                });
            }
            let status = v["status"].as_str().unwrap_or_default().to_owned();
            let _ = on_progress.send(PullProgress { status: status.clone(), digest: v["digest"].as_str().map(str::to_owned), total: v["total"].as_u64(), completed: v["completed"].as_u64() });
            if status == "success" {
                return Ok(true);
            }
        }
    }
    Ok(true)
}

pub async fn delete_model(base: &str, key: &str, model: &str) -> Result<(), String> {
    let resp = authed(client()?.delete(endpoint(base, "delete")), key)
        .json(&json!({ "model": model }))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| connect_error(base, &e))?;
    if !resp.status().is_success() {
        return Err(api_error(resp, Some(model)).await);
    }
    Ok(())
}

/// Libère la mémoire (RAM/VRAM) occupée par un modèle chargé.
pub async fn unload_model(base: &str, key: &str, model: &str) -> Result<(), String> {
    let resp = authed(client()?.post(endpoint(base, "generate")), key)
        .json(&json!({ "model": model, "keep_alive": 0 }))
        .timeout(Duration::from_secs(60))
        .send()
        .await
        .map_err(|e| connect_error(base, &e))?;
    if !resp.status().is_success() {
        return Err(api_error(resp, Some(model)).await);
    }
    Ok(())
}

/// Charge un modèle en mémoire à l'avance (première réponse plus rapide).
pub async fn preload_model(base: &str, key: &str, model: &str, keep_alive: &str, num_ctx: Option<u32>) -> Result<(), String> {
    let mut body = json!({ "model": model });
    if let Some(ka) = keep_alive_value(keep_alive) {
        body["keep_alive"] = ka;
    }
    if let Some(n) = num_ctx.filter(|n| *n > 0) {
        body["options"] = json!({ "num_ctx": n });
    }
    let resp = authed(client()?.post(endpoint(base, "generate")), key)
        .json(&body)
        .timeout(FIRST_BYTE_TIMEOUT)
        .send()
        .await
        .map_err(|e| connect_error(base, &e))?;
    if !resp.status().is_success() {
        return Err(api_error(resp, Some(model)).await);
    }
    Ok(())
}

/// `keep_alive` : durée Go (« 10m », « 1h », « -1m ») ou nombre de secondes ; vide = défaut du serveur.
pub fn keep_alive_value(s: &str) -> Option<Value> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    Some(match s.parse::<i64>() {
        Ok(n) => json!(n),
        Err(_) => json!(s),
    })
}

/// Ollama attend les images en base64 brut, sans le préfixe `data:…;base64,`.
fn strip_data_url(s: &str) -> &str {
    match s.find(";base64,") {
        Some(i) if s.starts_with("data:") => &s[i + 8..],
        _ => s,
    }
}

fn to_api_message(m: &ChatMessage) -> Value {
    if m.role == "tool" {
        let mut v = json!({ "role": "tool", "content": m.content });
        if let Some(n) = &m.name {
            v["tool_name"] = json!(n);
        }
        if let Some(id) = &m.tool_call_id {
            v["tool_call_id"] = json!(id);
        }
        return v;
    }
    let mut v = json!({ "role": m.role, "content": m.content });
    if !m.tool_calls.is_empty() {
        let calls: Vec<Value> = m
            .tool_calls
            .iter()
            .map(|c| {
                let args = serde_json::from_str::<Value>(&c.arguments).ok().filter(Value::is_object).unwrap_or_else(|| json!({}));
                json!({ "id": c.id, "function": { "name": c.name, "arguments": args } })
            })
            .collect();
        v["tool_calls"] = json!(calls);
    }
    if !m.images.is_empty() {
        v["images"] = json!(m.images.iter().map(|i| strip_data_url(i)).collect::<Vec<_>>());
    }
    v
}

pub fn build_body(req: &ChatRequest, keep_alive: &str) -> Value {
    let mut body = json!({
        "model": req.model,
        "messages": req.messages.iter().map(to_api_message).collect::<Vec<_>>(),
        "stream": true,
    });
    let mut options = serde_json::Map::new();
    if let Some(t) = req.temperature {
        options.insert("temperature".into(), json!(t));
    }
    if let Some(m) = req.max_tokens.filter(|m| *m > 0) {
        options.insert("num_predict".into(), json!(m));
    }
    if let Some(n) = req.num_ctx.filter(|n| *n > 0) {
        options.insert("num_ctx".into(), json!(n));
    }
    if !options.is_empty() {
        body["options"] = Value::Object(options);
    }
    if !req.tools.is_empty() && req.tool_choice.as_deref() != Some("none") {
        body["tools"] = json!(req.tools);
    }
    if let Some(t) = req.think.as_ref().filter(|t| !t.is_null()) {
        body["think"] = t.clone();
    }
    if let Some(ka) = keep_alive_value(keep_alive) {
        body["keep_alive"] = ka;
    }
    body
}

/// Appels d'outils d'un morceau du flux. Ollama envoie chaque appel complet (arguments en objet).
fn parse_tool_calls(v: &Value) -> Vec<ToolCall> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|tc| {
            let f = &tc["function"];
            let name = f["name"].as_str()?.to_owned();
            let arguments = match &f["arguments"] {
                Value::String(s) if !s.trim().is_empty() => s.clone(),
                Value::Null => "{}".into(),
                other => other.to_string(),
            };
            let id = tc["id"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("call_{}", &uuid::Uuid::new_v4().simple().to_string()[..12]));
            Some(ToolCall { id, name, arguments })
        })
        .collect()
}

pub async fn chat_stream(
    base: &str,
    key: &str,
    keep_alive: &str,
    req: ChatRequest,
    on_event: &Channel<StreamEvent>,
    cancel: Arc<Notify>,
) -> Result<(), String> {
    let body = build_body(&req, keep_alive);
    let client = client()?;
    let mut attempt = 0;
    let resp = loop {
        attempt += 1;
        let send = authed(client.post(endpoint(base, "chat")), key).json(&body).send();
        let outcome = tokio::select! {
            r = tokio::time::timeout(FIRST_BYTE_TIMEOUT, send) => r,
            _ = cancel.notified() => {
                let _ = on_event.send(StreamEvent::Done { finish_reason: None, cancelled: true });
                return Ok(());
            }
        };
        // Erreur temporaire (serveur occupé, redémarrage d'Ollama) : on réessaie avec une attente croissante.
        let (reason, retry_after) = match outcome {
            Ok(Ok(r)) if r.status().is_success() => break r,
            Ok(Ok(r)) if is_retryable(r.status()) && attempt < MAX_ATTEMPTS => {
                let ra = r
                    .headers()
                    .get(reqwest::header::RETRY_AFTER)
                    .and_then(|v| v.to_str().ok())
                    .and_then(|v| v.trim().parse::<u64>().ok());
                (format!("serveur occupé ({})", r.status()), ra)
            }
            Ok(Ok(r)) => return Err(api_error(r, Some(&req.model)).await),
            Ok(Err(e)) if attempt < MAX_ATTEMPTS && (e.is_connect() || e.is_request()) => (connect_error(base, &e), None),
            Ok(Err(e)) => return Err(connect_error(base, &e)),
            Err(_) => {
                return Err(format!(
                    "Le modèle n'a pas répondu en {} min (chargement ou lecture de l'invite trop longs). Essayez un modèle plus petit ou une fenêtre de contexte réduite.",
                    FIRST_BYTE_TIMEOUT.as_secs() / 60
                ))
            }
        };
        let delay = retry_after.unwrap_or(2u64.pow(attempt)).clamp(1, 60);
        let _ = on_event.send(StreamEvent::Retry { attempt, max_attempts: MAX_ATTEMPTS, delay_secs: delay, reason });
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_secs(delay)) => {}
            _ = cancel.notified() => {
                let _ = on_event.send(StreamEvent::Done { finish_reason: None, cancelled: true });
                return Ok(());
            }
        }
    };

    let mut stream = resp.bytes_stream();
    let mut buf: Vec<u8> = Vec::new();
    let mut calls: Vec<ToolCall> = Vec::new();
    let mut finish_reason: Option<String> = None;

    loop {
        let chunk = tokio::select! {
            c = tokio::time::timeout(STALL_TIMEOUT, stream.next()) => c.map_err(|_| {
                format!("Le flux s'est interrompu : aucune donnée reçue depuis {} min. Cliquez sur « Continuer » pour reprendre.", STALL_TIMEOUT.as_secs() / 60)
            })?,
            _ = cancel.notified() => {
                let _ = on_event.send(StreamEvent::Done { finish_reason: None, cancelled: true });
                return Ok(());
            }
        };
        let Some(chunk) = chunk else { break };
        buf.extend_from_slice(&chunk.map_err(|e| format!("Flux interrompu : {e}"))?);

        while let Some(pos) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=pos).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            if let Some(err) = v["error"].as_str() {
                return Err(explain(500, err, Some(&req.model)));
            }
            let msg = &v["message"];
            if let Some(t) = msg["thinking"].as_str().filter(|t| !t.is_empty()) {
                let _ = on_event.send(StreamEvent::Reasoning { text: t.to_owned() });
            }
            if let Some(t) = msg["content"].as_str().filter(|t| !t.is_empty()) {
                let _ = on_event.send(StreamEvent::Delta { text: t.to_owned() });
            }
            calls.extend(parse_tool_calls(&msg["tool_calls"]));
            if v["done"].as_bool() == Some(true) {
                finish_reason = v["done_reason"].as_str().map(str::to_owned);
                let prompt = v["prompt_eval_count"].as_u64().unwrap_or(0);
                let completion = v["eval_count"].as_u64().unwrap_or(0);
                let tps = v["eval_duration"].as_u64().filter(|d| *d > 0).map(|d| completion as f64 / (d as f64 / 1e9));
                if prompt + completion > 0 {
                    let _ = on_event.send(StreamEvent::Usage { prompt_tokens: prompt, completion_tokens: completion, tokens_per_sec: tps });
                }
            }
        }
    }

    if !calls.is_empty() {
        let _ = on_event.send(StreamEvent::ToolCalls { calls });
    }
    let _ = on_event.send(StreamEvent::Done { finish_reason, cancelled: false });
    Ok(())
}

// ------------------------------------------------------------------ transcription

/// Transcription audio par un serveur compatible OpenAI `/v1/audio/transcriptions`
/// (Speaches/faster-whisper, whisper.cpp, Voxtral via vLLM…).
pub async fn transcribe(base: &str, model: &str, language: &str, vocabulary: &[String], wav: Vec<u8>) -> Result<String, String> {
    let part = reqwest::multipart::Part::bytes(wav)
        .file_name("dictee.wav")
        .mime_str("audio/wav")
        .map_err(|e| e.to_string())?;
    let mut form = reqwest::multipart::Form::new().part("file", part);
    if !model.is_empty() {
        form = form.text("model", model.to_owned());
    }
    if !language.is_empty() {
        form = form.text("language", language.to_owned());
    }
    // Vocabulaire métier : invite initiale (biais de Whisper vers ces graphies).
    if !vocabulary.is_empty() {
        form = form.text("prompt", vocabulary.join(", "));
    }
    let url = format!("{}/v1/audio/transcriptions", base.trim().trim_end_matches('/').trim_end_matches("/v1"));
    let resp = client()?
        .post(url)
        .multipart(form)
        .timeout(Duration::from_secs(300))
        .send()
        .await
        .map_err(|e| format!("Serveur de transcription injoignable : {e}"))?;
    if !resp.status().is_success() {
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        return Err(format!("Erreur du serveur de transcription ({status}) : {}", body.chars().take(300).collect::<String>()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    v["text"].as_str().map(|t| t.trim().to_owned()).ok_or_else(|| "Réponse de transcription invalide".into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msg(role: &str, content: &str) -> ChatMessage {
        ChatMessage { role: role.into(), content: content.into(), images: vec![], tool_calls: vec![], tool_call_id: None, name: None }
    }

    #[test]
    fn endpoint_normalises_base() {
        assert_eq!(endpoint("http://localhost:11434", "chat"), "http://localhost:11434/api/chat");
        assert_eq!(endpoint("http://localhost:11434/", "tags"), "http://localhost:11434/api/tags");
        assert_eq!(endpoint("http://localhost:11434/api", "tags"), "http://localhost:11434/api/tags");
        assert_eq!(endpoint("https://ollama.example.org/v1", "ps"), "https://ollama.example.org/api/ps");
    }

    #[test]
    fn base_url_validation() {
        assert!(validate_base_url("http://localhost:11434").is_ok());
        assert!(validate_base_url("http://127.0.0.1:11434").is_ok());
        assert!(validate_base_url("http://192.168.1.10:11434").is_ok());
        assert!(validate_base_url("http://[fd00::12]:11434").is_ok());
        assert!(validate_base_url("http://100.101.102.103:11434").is_ok());
        assert!(validate_base_url("http://gpu-box:11434").is_ok());
        assert!(validate_base_url("http://ollama.home.arpa:11434").is_ok());
        assert!(validate_base_url("https://ollama.example.org").is_ok());
        assert!(validate_base_url("http://ollama.example.org").is_err());
        assert!(validate_base_url("http://8.8.8.8:11434").is_err());
        assert!(validate_base_url("ftp://x").is_err());
        assert!(is_loopback("http://127.0.0.1:11434"));
        assert!(is_loopback("http://localhost:11434"));
        assert!(!is_loopback("http://192.168.1.10:11434"));
    }

    #[test]
    fn images_are_raw_base64() {
        let mut m = msg("user", "décris");
        m.images = vec!["data:image/png;base64,AAA".into(), "BBB".into()];
        let v = to_api_message(&m);
        assert_eq!(v["content"], "décris");
        assert_eq!(v["images"][0], "AAA");
        assert_eq!(v["images"][1], "BBB");
    }

    #[test]
    fn tool_messages_round_trip() {
        let mut a = msg("assistant", "");
        a.tool_calls = vec![ToolCall { id: "call_1".into(), name: "read_file".into(), arguments: "{\"path\":\"a\"}".into() }];
        let v = to_api_message(&a);
        assert_eq!(v["tool_calls"][0]["function"]["name"], "read_file");
        assert_eq!(v["tool_calls"][0]["function"]["arguments"]["path"], "a");
        let mut t = msg("tool", "ok");
        t.tool_call_id = Some("call_1".into());
        t.name = Some("read_file".into());
        let v = to_api_message(&t);
        assert_eq!(v["tool_name"], "read_file");
        assert_eq!(v["tool_call_id"], "call_1");
    }

    #[test]
    fn tool_calls_are_parsed_with_ids() {
        let v = json!([
            { "id": "call_x", "function": { "index": 0, "name": "read_file", "arguments": { "path": "src/main.rs" } } },
            { "function": { "name": "list_files", "arguments": {} } },
            { "function": { "arguments": {} } }
        ]);
        let calls = parse_tool_calls(&v);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].id, "call_x");
        assert_eq!(serde_json::from_str::<Value>(&calls[0].arguments).unwrap()["path"], "src/main.rs");
        assert!(calls[1].id.starts_with("call_"));
        assert_eq!(calls[1].arguments, "{}");
    }

    #[test]
    fn request_body_uses_options() {
        let req = ChatRequest {
            model: "qwen3:8b".into(),
            messages: vec![msg("user", "salut")],
            temperature: Some(0.2),
            max_tokens: Some(500),
            tools: vec![json!({ "type": "function", "function": { "name": "x" } })],
            tool_choice: None,
            num_ctx: Some(16384),
            think: Some(json!(true)),
        };
        let b = build_body(&req, "30m");
        assert_eq!(b["options"]["num_ctx"], 16384);
        assert_eq!(b["options"]["num_predict"], 500);
        assert_eq!(b["keep_alive"], "30m");
        assert_eq!(b["think"], true);
        assert_eq!(b["tools"].as_array().unwrap().len(), 1);
        let none = ChatRequest { tool_choice: Some("none".into()), think: None, num_ctx: None, max_tokens: None, temperature: None, ..req };
        let b = build_body(&none, "");
        assert!(b.get("tools").is_none() && b.get("options").is_none() && b.get("keep_alive").is_none() && b.get("think").is_none());
    }

    #[test]
    fn keep_alive_accepts_numbers_and_durations() {
        assert_eq!(keep_alive_value("300"), Some(json!(300)));
        assert_eq!(keep_alive_value("-1"), Some(json!(-1)));
        assert_eq!(keep_alive_value("10m"), Some(json!("10m")));
        assert_eq!(keep_alive_value(" "), None);
    }

    #[test]
    fn context_length_from_model_info() {
        let info = json!({ "general.architecture": "qwen3", "qwen3.context_length": 40960 });
        assert_eq!(context_length(&info), Some(40960));
        let info = json!({ "gemma3.context_length": 131072 });
        assert_eq!(context_length(&info), Some(131072));
        let mut m = ModelInfo::default();
        apply_show(&mut m, &json!({ "capabilities": ["completion", "tools", "thinking"], "model_info": info }));
        assert!(m.tools && m.thinking && !m.vision);
        assert!(!is_chat_model(&json!({ "capabilities": ["embedding"] })));
        assert!(is_chat_model(&json!({})));
    }

    #[test]
    fn errors_are_explained() {
        assert!(explain(400, "registry.ollama.ai/library/gemma3:latest does not support tools", None).contains("outils"));
        assert!(explain(404, "model 'foo' not found", Some("foo")).contains("pas installé"));
        assert!(explain(500, "model requires more system memory (12 GiB) than is available", None).contains("Mémoire insuffisante"));
    }

    // ------------------------------------------------------------ faux serveur Ollama

    use std::sync::Mutex as StdMutex;
    use tauri::ipc::InvokeResponseBody;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    type Requests = Arc<StdMutex<Vec<(String, Value)>>>;

    /// Serveur HTTP minimal : `route(chemin, corps JSON)` → (statut, corps de la réponse).
    async fn mock(route: fn(&str, &Value) -> (u16, String)) -> (String, Requests) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen: Requests = Arc::default();
        let log = seen.clone();
        tokio::spawn(async move {
            loop {
                let Ok((mut sock, _)) = listener.accept().await else { break };
                let log = log.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = [0u8; 8192];
                    // En-têtes puis corps (Content-Length).
                    let (head_end, len) = loop {
                        let n = sock.read(&mut tmp).await.unwrap();
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| l.strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap()))
                                .unwrap_or(0);
                            break (p + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut tmp).await.unwrap();
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let path = head.split_whitespace().nth(1).unwrap_or("/").to_owned();
                    let body: Value = serde_json::from_slice(&buf[head_end..head_end + len]).unwrap_or(Value::Null);
                    let (status, out) = route(&path, &body);
                    log.lock().unwrap().push((path, body));
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
                        out.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (base, seen)
    }

    fn collect<T: Serialize + Send + 'static>() -> (Channel<T>, Arc<StdMutex<Vec<Value>>>) {
        let events: Arc<StdMutex<Vec<Value>>> = Arc::default();
        let sink = events.clone();
        let ch = Channel::new(move |b| {
            if let InvokeResponseBody::Json(j) = b {
                sink.lock().unwrap().push(serde_json::from_str(&j).unwrap());
            }
            Ok(())
        });
        (ch, events)
    }

    fn ndjson(lines: &[Value]) -> String {
        lines.iter().map(|l| format!("{l}\n")).collect()
    }

    #[tokio::test]
    async fn chat_stream_against_mock_server() {
        let (base, seen) = mock(|path, _| {
            assert_eq!(path, "/api/chat");
            (200, ndjson(&[
                json!({ "message": { "role": "assistant", "content": "", "thinking": "Je réfléchis" }, "done": false }),
                json!({ "message": { "role": "assistant", "content": "Bon" }, "done": false }),
                json!({ "message": { "role": "assistant", "content": "jour" }, "done": false }),
                json!({ "message": { "role": "assistant", "content": "", "tool_calls": [
                    { "id": "call_a", "function": { "index": 0, "name": "read_file", "arguments": { "path": "README.md" } } }
                ] }, "done": false }),
                json!({ "message": { "role": "assistant", "content": "" }, "done": true, "done_reason": "stop",
                        "prompt_eval_count": 120, "eval_count": 40, "eval_duration": 2_000_000_000u64 }),
            ]))
        })
        .await;
        let mut user = msg("user", "salut");
        user.images = vec!["data:image/jpeg;base64,QUJD".into()];
        let req = ChatRequest {
            model: "qwen3:8b".into(),
            messages: vec![msg("system", "sys"), user],
            temperature: None,
            max_tokens: None,
            tools: vec![json!({ "type": "function", "function": { "name": "read_file", "parameters": {} } })],
            tool_choice: None,
            num_ctx: Some(32768),
            think: Some(json!(true)),
        };
        let (ch, events) = collect::<StreamEvent>();
        chat_stream(&base, "", "10m", req, &ch, Arc::new(Notify::new())).await.unwrap();

        let ev = events.lock().unwrap().clone();
        let kinds: Vec<&str> = ev.iter().map(|e| e["type"].as_str().unwrap()).collect();
        assert_eq!(kinds, ["reasoning", "delta", "delta", "usage", "toolCalls", "done"]);
        assert_eq!(ev[0]["text"], "Je réfléchis");
        assert_eq!(ev[3]["promptTokens"], 120);
        assert_eq!(ev[3]["tokensPerSec"], 20.0);
        assert_eq!(ev[4]["calls"][0]["id"], "call_a");
        assert_eq!(ev[4]["calls"][0]["arguments"], "{\"path\":\"README.md\"}");
        assert_eq!(ev[5]["finishReason"], "stop");

        let sent = &seen.lock().unwrap()[0].1;
        assert_eq!(sent["options"]["num_ctx"], 32768);
        assert_eq!(sent["think"], true);
        assert_eq!(sent["keep_alive"], "10m");
        assert_eq!(sent["stream"], true);
        assert_eq!(sent["messages"][1]["images"][0], "QUJD");
        assert_eq!(sent["tools"][0]["function"]["name"], "read_file");
    }

    #[tokio::test]
    async fn chat_errors_are_readable() {
        let (base, _) = mock(|_, _| (404, json!({ "error": "model \"absent:7b\" not found, try pulling it first" }).to_string())).await;
        let req = ChatRequest {
            model: "absent:7b".into(),
            messages: vec![msg("user", "x")],
            temperature: None,
            max_tokens: None,
            tools: vec![],
            tool_choice: None,
            num_ctx: None,
            think: None,
        };
        let (ch, _) = collect::<StreamEvent>();
        let err = chat_stream(&base, "", "", req, &ch, Arc::new(Notify::new())).await.unwrap_err();
        assert!(err.contains("absent:7b") && err.contains("pas installé"), "{err}");

        let err = list_models("http://127.0.0.1:9", "").await.unwrap_err();
        assert!(err.contains("injoignable"), "{err}");
    }

    #[tokio::test]
    async fn models_are_listed_with_capabilities() {
        let (base, _) = mock(|path, body| match path {
            "/api/tags" => (200, json!({ "models": [
                { "name": "qwen3:8b", "size": 5_200_000_000u64, "details": { "family": "qwen3", "parameter_size": "8.2B", "quantization_level": "Q4_K_M" } },
                { "name": "gemma3:4b", "size": 3_300_000_000u64, "details": { "parameter_size": "4.3B" } },
                { "name": "nomic-embed-text:latest", "size": 274_000_000u64, "details": {} }
            ] }).to_string()),
            "/api/show" => (200, match body["model"].as_str().unwrap() {
                "qwen3:8b" => json!({ "capabilities": ["completion", "tools", "thinking"], "model_info": { "general.architecture": "qwen3", "qwen3.context_length": 40960 } }),
                "gemma3:4b" => json!({ "capabilities": ["completion", "vision"], "model_info": { "general.architecture": "gemma3", "gemma3.context_length": 131072 } }),
                _ => json!({ "capabilities": ["embedding"] }),
            }.to_string()),
            "/api/ps" => (200, json!({ "models": [{ "name": "qwen3:8b", "size": 6_000_000_000u64, "size_vram": 6_000_000_000u64, "expires_at": "2026-10-21T15:00:00Z", "context_length": 16384 }] }).to_string()),
            "/api/version" => (200, json!({ "version": "0.13.5" }).to_string()),
            _ => (404, json!({ "error": "not found" }).to_string()),
        })
        .await;
        assert_eq!(version(&base, "").await.unwrap(), "0.13.5");
        let models = list_models(&base, "").await.unwrap();
        let ids: Vec<&str> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gemma3:4b", "qwen3:8b"]);
        assert!(models[0].vision && !models[0].tools && !models[0].thinking);
        assert!(models[1].tools && models[1].thinking && !models[1].vision);
        assert_eq!(models[1].max_context_length, Some(40960));
        assert_eq!(models[1].quantization.as_deref(), Some("Q4_K_M"));
        let ps = running_models(&base, "").await.unwrap();
        assert_eq!(ps[0].context_length, Some(16384));
        assert_eq!(ps[0].size_vram, 6_000_000_000);
    }

    #[tokio::test]
    async fn pull_reports_progress() {
        let (base, seen) = mock(|path, body| match (path, body["model"].as_str()) {
            ("/api/pull", Some("qwen3:0.6b")) => (200, ndjson(&[
                json!({ "status": "pulling manifest" }),
                json!({ "status": "pulling abc", "digest": "sha256:abc", "total": 1000, "completed": 500 }),
                json!({ "status": "pulling abc", "digest": "sha256:abc", "total": 1000, "completed": 1000 }),
                json!({ "status": "verifying sha256 digest" }),
                json!({ "status": "success" }),
            ])),
            ("/api/pull", _) => (200, ndjson(&[json!({ "status": "pulling manifest" }), json!({ "error": "pull model manifest: file does not exist" })])),
            ("/api/delete", _) | ("/api/generate", _) => (200, String::new()),
            _ => (404, "{}".into()),
        })
        .await;
        let (ch, events) = collect::<PullProgress>();
        assert!(pull_model(&base, "", "qwen3:0.6b", &ch, Arc::new(Notify::new())).await.unwrap());
        let ev = events.lock().unwrap().clone();
        assert_eq!(ev.len(), 5);
        assert_eq!(ev[1]["digest"], "sha256:abc");
        assert_eq!(ev[2]["completed"], 1000);

        let (ch, _) = collect::<PullProgress>();
        let err = pull_model(&base, "", "inexistant:1b", &ch, Arc::new(Notify::new())).await.unwrap_err();
        assert!(err.contains("introuvable"), "{err}");

        delete_model(&base, "", "qwen3:0.6b").await.unwrap();
        unload_model(&base, "", "qwen3:0.6b").await.unwrap();
        let log = seen.lock().unwrap();
        assert_eq!(log.last().unwrap().1["keep_alive"], 0);
    }
}
