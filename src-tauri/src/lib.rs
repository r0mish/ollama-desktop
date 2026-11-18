mod audio;
mod embed;
mod git;
mod mcp;
mod ollama;
mod pdfscan;
mod shell;
mod ssh;
mod storage;
mod knowledge;
mod target;
mod textsearch;
mod tools;
mod tunnel;
mod updates;
mod vault;
mod web;
mod workspace;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use base64::Engine;
use tauri::{ipc::Channel, AppHandle, Manager, State};
use tauri_plugin_dialog::DialogExt;
use tokio::sync::Notify;

use mcp::{McpManager, ServerStatus};
use ollama::{ChatRequest, ModelInfo, PullProgress, RunningModel, StreamEvent};
use storage::{Conversation, ConversationSummary, Settings};
use tools::{ToolContext, ToolDef, ToolOutput};

const KEYRING_SERVICE: &str = "dev.r0mish.ollama-desktop";
const KEYRING_USER: &str = "ollama-api-key";
const ENV_API_KEY: &str = "OLLAMA_API_KEY";

/// Dictée vocale en cours.
#[derive(Default)]
struct Dictation(Mutex<Option<audio::Recording>>);

/// Requêtes en cours, annulables depuis l'interface.
#[derive(Default)]
struct Streams(Mutex<HashMap<String, Arc<Notify>>>);

type CmdResult<T> = Result<T, String>;

/// Entrée du trousseau pour la clé API d'un serveur Ollama.
fn keyring_entry(server: &str) -> CmdResult<keyring::Entry> {
    let user = if server.is_empty() || server == "default" { KEYRING_USER.to_string() } else { format!("{KEYRING_USER}:{server}") };
    keyring::Entry::new(KEYRING_SERVICE, &user).map_err(|e| e.to_string())
}

/// Clé API facultative (Ollama derrière un proxy authentifiant, ollama.com) : trousseau du
/// système d'abord, variable d'environnement en repli. Vide = aucune authentification.
fn api_key(server: &str) -> String {
    if let Ok(key) = keyring_entry(server).and_then(|e| e.get_password().map_err(|e| e.to_string())) {
        if !key.is_empty() {
            return key;
        }
    }
    std::env::var(ENV_API_KEY).unwrap_or_default()
}

/// Connexion au serveur Ollama actif : adresse (celle du tunnel SSH le cas échéant) et clé API.
struct Conn {
    settings: Settings,
    server: storage::Server,
    base: String,
    key: String,
}

async fn conn(app: &AppHandle, tunnels: &tunnel::Tunnels) -> CmdResult<Conn> {
    let settings = storage::load_settings(app)?;
    let server = settings.active();
    let base = match &server.ssh_host {
        Some(host_id) => {
            let host = find_host(app, host_id).map_err(|_| format!("Le serveur SSH du tunnel « {} » n'existe plus : corrigez-le dans les réglages.", server.name))?;
            let port = tunnels.ensure(&server.id, &host, server.remote_port.unwrap_or(11434)).await?;
            format!("http://127.0.0.1:{port}")
        }
        None => server.url.trim().to_string(),
    };
    let key = api_key(&server.id);
    Ok(Conn { settings, server, base, key })
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct KeyStatus {
    configured: bool,
    source: Option<&'static str>,
}

#[tauri::command]
fn api_key_status(server: String) -> KeyStatus {
    let in_keyring = keyring_entry(&server)
        .and_then(|e| e.get_password().map_err(|e| e.to_string()))
        .map(|k| !k.is_empty())
        .unwrap_or(false);
    if in_keyring {
        return KeyStatus { configured: true, source: Some("keyring") };
    }
    if std::env::var(ENV_API_KEY).map(|k| !k.is_empty()).unwrap_or(false) {
        return KeyStatus { configured: true, source: Some("env") };
    }
    KeyStatus { configured: false, source: None }
}

#[tauri::command]
fn set_api_key(server: String, key: String) -> CmdResult<()> {
    let key = key.trim();
    if key.is_empty() {
        return Err("Clé vide".into());
    }
    keyring_entry(&server)?
        .set_password(key)
        .map_err(|e| format!("Impossible d'écrire dans le trousseau du système : {e}"))
}

#[tauri::command]
fn delete_api_key(server: String) -> CmdResult<()> {
    match keyring_entry(&server)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
        Err(e) => Err(e.to_string()),
    }
}

#[tauri::command]
fn get_settings(app: AppHandle) -> CmdResult<Settings> {
    storage::load_settings(&app)
}

#[tauri::command]
async fn save_settings(app: AppHandle, tunnels: State<'_, Arc<tunnel::Tunnels>>, settings: Settings) -> CmdResult<()> {
    if settings.servers.is_empty() {
        return Err("Au moins un serveur Ollama est nécessaire".into());
    }
    for v in &settings.servers {
        if v.ssh_host.is_none() {
            ollama::validate_base_url(v.url.trim()).map_err(|e| format!("Serveur « {} » : {e}", v.name))?;
        }
    }
    // Les tunnels des serveurs inactifs sont fermés ; celui du serveur actif est rouvert au besoin.
    tunnels.close_except(Some(&settings.active().id)).await;
    if !settings.transcription_url.trim().is_empty() {
        ollama::validate_base_url(settings.transcription_url.trim())?;
    }
    if settings.encrypt_data != vault::enabled() {
        let res = if settings.encrypt_data {
            enable_encryption(&app).map(|_| ())
        } else {
            vault::set_enabled(false).and_then(|_| storage::reseal_all(&app).map(|_| ()))
        };
        match res {
            Ok(()) => *vault::LAST_ERROR.lock().unwrap() = None,
            // Trousseau indisponible : les réglages sont enregistrés, l'erreur est affichée.
            Err(e) => *vault::LAST_ERROR.lock().unwrap() = Some(e),
        }
    }
    storage::save_settings(&app, &settings)
}

/// Active le chiffrement puis chiffre les fichiers existants. Ne crée jamais de nouvelle clé
/// si des données sont déjà chiffrées avec une autre (trousseau perdu) : elles deviendraient irrécupérables.
fn enable_encryption(app: &AppHandle) -> CmdResult<usize> {
    if vault::key(false)?.is_none() && storage::has_sealed_files(app)? {
        return Err("Des données chiffrées existent, mais leur clé est absente du trousseau : restaurez votre clé de récupération (Réglages → Général).".into());
    }
    vault::set_enabled(true)?;
    storage::reseal_all(app)
}

#[tauri::command]
fn vault_status() -> vault::Status {
    vault::status()
}

#[tauri::command]
fn vault_export_key() -> CmdResult<String> {
    vault::export_key()
}

/// Restaure une clé de récupération puis vérifie qu'elle ouvre bien les données existantes.
#[tauri::command]
fn vault_import_key(app: AppHandle, key: String) -> CmdResult<()> {
    vault::import_key(&key)?;
    storage::list_conversations(&app)?;
    if storage::load_settings(&app)?.encrypt_data {
        enable_encryption(&app)?;
        *vault::LAST_ERROR.lock().unwrap() = None;
    }
    Ok(())
}

// ------------------------------------------------------------------ serveur Ollama et modèles

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct OllamaStatus {
    /// Identifiant et nom du serveur actif.
    id: String,
    name: String,
    /// Adresse affichée (celle du serveur, pas le port local d'un tunnel).
    url: String,
    /// Nom du serveur SSH quand la connexion passe par un tunnel.
    tunnel: Option<String>,
    version: Option<String>,
    error: Option<String>,
    /// HTTP en clair hors de cette machine : la clé API éventuelle transite sans chiffrement.
    insecure: bool,
}

#[tauri::command]
async fn server_status(app: AppHandle, tunnels: State<'_, Arc<tunnel::Tunnels>>) -> CmdResult<OllamaStatus> {
    let settings = storage::load_settings(&app)?;
    let server = settings.active();
    let tunnel = server.ssh_host.as_deref().map(|id| find_host(&app, id).map(|h| h.label).unwrap_or_else(|_| "serveur SSH supprimé".into()));
    let url = match &tunnel {
        Some(label) => format!("{label} · tunnel SSH vers le port {}", server.remote_port.unwrap_or(11434)),
        None => server.url.trim().to_string(),
    };
    let insecure = tunnel.is_none() && url.starts_with("http://") && !ollama::is_loopback(&url);
    let (version, error) = match conn(&app, &tunnels).await {
        Ok(c) => match ollama::version(&c.base, &c.key).await {
            Ok(v) => (Some(v), None),
            // Derrière un tunnel, l'adresse locale n'aide pas : on nomme le serveur.
            Err(e) if tunnel.is_some() => (None, Some(format!("Tunnel SSH ouvert, mais Ollama ne répond pas sur le port {} du serveur ({e})", c.server.remote_port.unwrap_or(11434)))),
            Err(e) => (None, Some(e)),
        },
        Err(e) => (None, Some(e)),
    };
    Ok(OllamaStatus { id: server.id, name: server.name, url, tunnel, version, error, insecure })
}

#[tauri::command]
async fn list_models(app: AppHandle, tunnels: State<'_, Arc<tunnel::Tunnels>>) -> CmdResult<Vec<ModelInfo>> {
    let c = conn(&app, &tunnels).await?;
    ollama::list_models(&c.base, &c.key).await
}

#[tauri::command]
async fn running_models(app: AppHandle, tunnels: State<'_, Arc<tunnel::Tunnels>>) -> CmdResult<Vec<RunningModel>> {
    let c = conn(&app, &tunnels).await?;
    ollama::running_models(&c.base, &c.key).await
}

/// Télécharge un modèle ; renvoie false si l'utilisateur a annulé.
#[tauri::command]
async fn pull_model(
    app: AppHandle,
    streams: State<'_, Streams>,
    tunnels: State<'_, Arc<tunnel::Tunnels>>,
    request_id: String,
    model: String,
    on_progress: Channel<PullProgress>,
) -> CmdResult<bool> {
    let model = model.trim().to_string();
    if model.is_empty() || model.chars().any(|c| c.is_whitespace()) {
        return Err("Nom de modèle invalide (ex. qwen3:8b)".into());
    }
    let c = conn(&app, &tunnels).await?;
    let cancel = Arc::new(Notify::new());
    streams.0.lock().unwrap().insert(request_id.clone(), cancel.clone());
    let res = ollama::pull_model(&c.base, &c.key, &model, &on_progress, cancel).await;
    streams.0.lock().unwrap().remove(&request_id);
    res
}

#[tauri::command]
async fn delete_model(app: AppHandle, tunnels: State<'_, Arc<tunnel::Tunnels>>, model: String) -> CmdResult<()> {
    let c = conn(&app, &tunnels).await?;
    ollama::delete_model(&c.base, &c.key, &model).await
}

#[tauri::command]
async fn unload_model(app: AppHandle, tunnels: State<'_, Arc<tunnel::Tunnels>>, model: String) -> CmdResult<()> {
    let c = conn(&app, &tunnels).await?;
    ollama::unload_model(&c.base, &c.key, &model).await
}

#[tauri::command]
async fn preload_model(app: AppHandle, tunnels: State<'_, Arc<tunnel::Tunnels>>, model: String, num_ctx: Option<u32>) -> CmdResult<()> {
    let c = conn(&app, &tunnels).await?;
    ollama::preload_model(&c.base, &c.key, &model, &c.settings.keep_alive, num_ctx).await
}

#[tauri::command]
fn list_assistants(app: AppHandle) -> Vec<serde_json::Value> {
    storage::load_assistants(&app)
}

#[tauri::command]
fn save_assistants(app: AppHandle, assistants: Vec<serde_json::Value>) -> CmdResult<()> {
    storage::save_assistants(&app, &assistants)
}

#[tauri::command]
fn list_conversations(app: AppHandle) -> CmdResult<Vec<ConversationSummary>> {
    storage::list_conversations(&app)
}

#[tauri::command]
fn load_conversation(app: AppHandle, id: String) -> CmdResult<Conversation> {
    storage::load_conversation(&app, &id)
}

#[tauri::command]
fn save_conversation(app: AppHandle, conversation: Conversation) -> CmdResult<()> {
    storage::save_conversation(&app, &conversation)
}

#[tauri::command]
async fn search_conversations(app: AppHandle, query: String) -> CmdResult<Vec<storage::SearchHit>> {
    tauri::async_runtime::spawn_blocking(move || storage::search_conversations(&app, &query))
        .await
        .map_err(|e| e.to_string())?
}

#[tauri::command]
fn delete_conversation(app: AppHandle, id: String) -> CmdResult<()> {
    storage::delete_conversation(&app, &id)
}

#[tauri::command]
async fn export_conversation(app: AppHandle, id: String) -> CmdResult<bool> {
    storage::export_conversation(&app, &id).await
}

#[tauri::command]
fn data_dir(app: AppHandle) -> CmdResult<String> {
    storage::data_dir(&app).map(|p| p.display().to_string())
}

#[tauri::command]
async fn chat_stream(
    app: AppHandle,
    streams: State<'_, Streams>,
    tunnels: State<'_, Arc<tunnel::Tunnels>>,
    request_id: String,
    request: ChatRequest,
    on_event: Channel<StreamEvent>,
) -> CmdResult<()> {
    let c = conn(&app, &tunnels).await?;
    let cancel = Arc::new(Notify::new());
    streams.0.lock().unwrap().insert(request_id.clone(), cancel.clone());
    let result = ollama::chat_stream(&c.base, &c.key, &c.settings.keep_alive, request, &on_event, cancel).await;
    streams.0.lock().unwrap().remove(&request_id);
    result
}

#[tauri::command]
fn cancel_stream(streams: State<'_, Streams>, request_id: String) {
    if let Some(n) = streams.0.lock().unwrap().get(&request_id) {
        n.notify_one();
    }
}

// ------------------------------------------------------------------ outils

#[tauri::command]
fn tool_definitions(app: AppHandle, mcp: State<'_, Arc<McpManager>>, ctx: ToolContext) -> Vec<ToolDef> {
    let settings = storage::load_settings(&app).unwrap_or_default();
    tools::definitions(&ctx, &mcp, &shell::detect(&settings.shell))
}

#[tauri::command]
async fn preview_tool(app: AppHandle, ctx: ToolContext, name: String, arguments: String) -> CmdResult<Option<String>> {
    tools::preview(&ctx, &storage::load_workspaces(&app), &storage::load_ssh_hosts(&app), &name, &arguments).await
}

/// Chemin hors du dossier de projet visé par un appel (accès système), pour exiger l'approbation.
#[tauri::command]
fn tool_outside(app: AppHandle, ctx: ToolContext, name: String, arguments: String) -> CmdResult<Option<String>> {
    tools::outside(&ctx, &storage::load_workspaces(&app), &storage::load_ssh_hosts(&app), &name, &arguments)
}

/// Appel d'outil à exécuter.
#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ToolCallReq {
    name: String,
    arguments: String,
    /// Mot de passe sudo saisi pour cette commande (jamais enregistré).
    sudo_password: Option<String>,
}

#[tauri::command]
async fn run_tool(
    app: AppHandle,
    streams: State<'_, Streams>,
    request_id: String,
    ctx: ToolContext,
    call: ToolCallReq,
    on_output: Channel<String>,
) -> CmdResult<ToolOutput> {
    let mcp = app.state::<Arc<McpManager>>();
    let tunnels = app.state::<Arc<tunnel::Tunnels>>();
    let ToolCallReq { name, arguments, sudo_password } = call;
    let settings = storage::load_settings(&app)?;
    let approved = storage::load_workspaces(&app);
    let hosts = storage::load_ssh_hosts(&app);
    let project = ctx.project.as_deref().and_then(|id| storage::load_project(&app, id).ok());
    let cancel = Arc::new(Notify::new());
    streams.0.lock().unwrap().insert(request_id.clone(), cancel.clone());
    let live: workspace::Live = Arc::new(move |text| {
        let _ = on_output.send(text);
    });
    let sudo = sudo_password.filter(|p| !p.is_empty() && !p.contains(['\n', '\r']));
    let embed = match (&ctx.project, name.as_str()) {
        (Some(id), "search_project_files") => embed_for(&app, &tunnels, id).await,
        _ => None,
    };
    let env = tools::RunEnv { settings: &settings, approved: &approved, hosts: &hosts, mcp: &mcp, cancel: cancel.clone(), project, live: Some(live), sudo, embed };
    let out = if name == "run_command" {
        // La commande gère elle-même l'annulation (arrêt du groupe de processus).
        tools::run(&ctx, env, &name, &arguments).await
    } else {
        tokio::select! {
            o = tools::run(&ctx, env, &name, &arguments) => o,
            _ = cancel.notified() => ToolOutput { content: "Annulé par l'utilisateur".into(), is_error: true, ..Default::default() },
        }
    };
    streams.0.lock().unwrap().remove(&request_id);
    Ok(out)
}

/// Annule une modification de fichier faite par l'agent (contenu précédent, ou suppression d'un fichier créé).
#[tauri::command]
async fn restore_file(app: AppHandle, ctx: ToolContext, path: String, before: Option<String>) -> CmdResult<()> {
    tools::restore(&ctx, &storage::load_workspaces(&app), &storage::load_ssh_hosts(&app), &path, before.as_deref()).await
}

// ------------------------------------------------------------------ dossiers de projet

#[tauri::command]
async fn pick_workspace(app: AppHandle) -> CmdResult<Option<String>> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().set_title("Choisir le dossier du projet").pick_folder(move |p| {
        let _ = tx.send(p);
    });
    let Some(path) = rx.await.map_err(|e| e.to_string())? else { return Ok(None) };
    let path = path.into_path().map_err(|e| e.to_string())?;
    let path = workspace::simplify_path(&path.canonicalize().map_err(|e| e.to_string())?).display().to_string();
    storage::add_workspace(&app, &path)?;
    Ok(Some(path))
}

#[tauri::command]
fn list_workspaces(app: AppHandle) -> Vec<String> {
    storage::load_workspaces(&app)
}

#[tauri::command]
fn forget_workspace(app: AppHandle, path: String) -> CmdResult<Vec<String>> {
    storage::remove_workspace(&app, &path)
}

#[tauri::command]
fn touch_workspace(app: AppHandle, path: String) -> CmdResult<()> {
    // Ne fait que remonter un dossier déjà autorisé en tête de liste.
    if storage::load_workspaces(&app).contains(&workspace::normalize_workspace(&path)) {
        storage::add_workspace(&app, &path)?;
    }
    Ok(())
}

/// Fichiers de consignes (AGENTS.md…) du dossier de projet.
#[tauri::command]
async fn workspace_instructions(app: AppHandle, workspace: String) -> CmdResult<Vec<target::InstructionFile>> {
    git_target(&app, &workspace)?.instruction_files().await
}

// ------------------------------------------------------------------ projets

#[tauri::command]
fn list_projects(app: AppHandle) -> CmdResult<Vec<storage::Project>> {
    storage::list_projects(&app)
}

#[tauri::command]
fn save_project(app: AppHandle, project: storage::Project) -> CmdResult<()> {
    storage::save_project(&app, &project)
}

/// Serveur d'embedding et fichier d'index d'un projet, si la recherche sémantique est configurée.
async fn embed_for(app: &AppHandle, tunnels: &tunnel::Tunnels, project_id: &str) -> Option<(embed::EmbedConn, std::path::PathBuf)> {
    let settings = storage::load_settings(app).ok()?;
    let model = settings.embedding_model.trim().to_string();
    if model.is_empty() {
        return None;
    }
    let c = conn(app, tunnels).await.ok()?;
    Some((embed::EmbedConn { base: c.base, key: c.key, model }, storage::project_index_path(app, project_id).ok()?))
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectContextOut {
    #[serde(flatten)]
    context: knowledge::ProjectContext,
    /// « semantic » (mots-clés + sens), « keywords », ou la raison pour laquelle le sens n'a pas servi.
    search: String,
}

/// Fichiers de référence à joindre pour une question : en entier, ou extraits pertinents (gros projets).
/// `on_progress` : avancement de l'indexation sémantique (extraits faits, total).
#[tauri::command]
async fn project_context(
    app: AppHandle,
    tunnels: State<'_, Arc<tunnel::Tunnels>>,
    id: String,
    query: String,
    on_progress: Channel<[usize; 2]>,
) -> CmdResult<ProjectContextOut> {
    let project = storage::load_project(&app, &id)?;
    let mut search = "keywords".to_string();
    let mut semantic = None;
    if knowledge::total_chars(&project) > knowledge::FULL_CONTEXT_CHARS {
        if let Some((conn, index)) = embed_for(&app, &tunnels, &id).await {
            let progress = move |done: usize, total: usize| {
                let _ = on_progress.send([done, total]);
            };
            match embed::rank(&index, &conn, &knowledge::chunks(&project), &query, &progress).await {
                Ok(order) => {
                    semantic = Some(order);
                    search = "semantic".into();
                }
                // Modèle absent, serveur occupé… : la recherche par mots-clés prend le relais.
                Err(e) => search = e,
            }
        }
    }
    tauri::async_runtime::spawn_blocking(move || ProjectContextOut { context: knowledge::context(&project, &query, semantic.as_deref()), search })
        .await
        .map_err(|e| e.to_string())
}

#[tauri::command]
fn delete_project(app: AppHandle, id: String) -> CmdResult<()> {
    storage::delete_project(&app, &id)
}

// ------------------------------------------------------------------ dictée vocale

#[tauri::command]
async fn dictation_start(dictation: State<'_, Dictation>, on_level: Channel<f32>) -> CmdResult<()> {
    let old = dictation.0.lock().unwrap().take();
    // L'ouverture du micro peut prendre du temps : hors du thread de l'interface.
    let rec = tauri::async_runtime::spawn_blocking(move || {
        if let Some(old) = old {
            old.finish();
        }
        audio::start(on_level)
    })
    .await
    .map_err(|e| e.to_string())??;
    if let Some(other) = dictation.0.lock().unwrap().replace(rec) {
        other.finish();
    }
    Ok(())
}

/// Arrête l'enregistrement ; le transcrit si `transcribe`, sinon l'abandonne.
#[tauri::command]
async fn dictation_stop(app: AppHandle, dictation: State<'_, Dictation>, transcribe: bool) -> CmdResult<Option<String>> {
    let Some(rec) = dictation.0.lock().unwrap().take() else { return Ok(None) };
    let captured = tauri::async_runtime::spawn_blocking(move || rec.finish()).await.map_err(|e| e.to_string())?;
    if !transcribe {
        return Ok(None);
    }
    if captured.seconds < 0.4 {
        return Err("Enregistrement trop court".into());
    }
    if captured.silent {
        return Err("Aucun son capté : vérifiez le micro et l'autorisation d'accès dans les réglages du système.".into());
    }
    let settings = storage::load_settings(&app)?;
    let url = settings.transcription_url.trim();
    if url.is_empty() {
        return Err("Aucun serveur de transcription configuré (Réglages → Outils → Dictée vocale).".into());
    }
    let (model, lang, vocab) = (settings.transcription_model.trim(), settings.transcription_language.trim(), settings.vocabulary());
    ollama::transcribe(url, model, lang, &vocab, captured.wav).await.map(Some)
}

// ------------------------------------------------------------------ SSH

#[tauri::command]
fn ssh_hosts(app: AppHandle) -> Vec<ssh::SshHost> {
    storage::load_ssh_hosts(&app)
}

#[tauri::command]
fn ssh_save_host(app: AppHandle, mut host: ssh::SshHost) -> CmdResult<ssh::SshHost> {
    host.host = host.host.trim().to_string();
    host.label = if host.label.trim().is_empty() { host.host.clone() } else { host.label.trim().to_string() };
    host.user = host.user.filter(|u| !u.trim().is_empty());
    host.identity_file = host.identity_file.filter(|k| !k.trim().is_empty());
    ssh::validate(&host)?;
    let mut hosts = storage::load_ssh_hosts(&app);
    if host.id.is_empty() {
        host.id = uuid::Uuid::new_v4().simple().to_string()[..8].to_owned();
    }
    hosts.retain(|h| h.id != host.id);
    hosts.push(host.clone());
    hosts.sort_by_key(|h| h.label.to_lowercase());
    storage::save_ssh_hosts(&app, &hosts)?;
    Ok(host)
}

#[tauri::command]
fn ssh_delete_host(app: AppHandle, id: String) -> CmdResult<()> {
    let mut hosts = storage::load_ssh_hosts(&app);
    hosts.retain(|h| h.id != id);
    storage::save_ssh_hosts(&app, &hosts)?;
    // Les dossiers distants de cet hôte ne sont plus autorisés.
    for ws in storage::load_workspaces(&app) {
        if target::parse_remote(&ws).is_some_and(|(hid, _)| hid == id) {
            storage::remove_workspace(&app, &ws)?;
        }
    }
    Ok(())
}

#[tauri::command]
async fn ssh_test(app: AppHandle, host: ssh::SshHost) -> CmdResult<ssh::TestResult> {
    let res = ssh::test(&host).await;
    // Mémorise le système détecté pour informer le modèle.
    if res.ok && !host.id.is_empty() {
        let mut hosts = storage::load_ssh_hosts(&app);
        if let Some(h) = hosts.iter_mut().find(|h| h.id == host.id) {
            h.system = res.system.clone();
            storage::save_ssh_hosts(&app, &hosts)?;
        }
    }
    Ok(res)
}

#[tauri::command]
async fn ssh_trust(host: ssh::SshHost) -> CmdResult<()> {
    ssh::trust(&host).await
}

/// Installe la clé de l'application sur le serveur ; le mot de passe ne sert qu'à cet appel.
#[tauri::command]
async fn ssh_install_key(host: ssh::SshHost, password: String) -> CmdResult<ssh::KeySetup> {
    ssh::install_key(&host, &password).await
}

#[tauri::command]
async fn ssh_public_key() -> CmdResult<String> {
    ssh::public_key().await
}

#[tauri::command]
fn ssh_config_hosts() -> Vec<ssh::ConfigHost> {
    ssh::config_hosts()
}

fn find_host(app: &AppHandle, id: &str) -> CmdResult<ssh::SshHost> {
    storage::load_ssh_hosts(app).into_iter().find(|h| h.id == id).ok_or_else(|| "Serveur SSH inconnu".to_string())
}

#[tauri::command]
async fn ssh_list_dirs(app: AppHandle, host_id: String, path: String) -> CmdResult<ssh::DirListing> {
    ssh::list_dirs(&find_host(&app, &host_id)?, &path).await
}

/// Autorise un dossier distant comme dossier de projet (après vérification de son existence).
#[tauri::command]
async fn ssh_open_workspace(app: AppHandle, host_id: String, path: String) -> CmdResult<String> {
    let host = find_host(&app, &host_id)?;
    let listing = ssh::list_dirs(&host, &path).await?;
    let ws = target::remote_workspace(&host.id, &listing.path);
    storage::add_workspace(&app, &ws)?;
    Ok(ws)
}

// ------------------------------------------------------------------ git

fn git_target(app: &AppHandle, workspace: &str) -> CmdResult<target::Target> {
    target::resolve(workspace, &storage::load_workspaces(app), &storage::load_ssh_hosts(app))
}

#[tauri::command]
async fn git_status(app: AppHandle, workspace: String) -> CmdResult<git::GitStatus> {
    git::status(&git_target(&app, &workspace)?).await
}

#[tauri::command]
async fn git_diff(app: AppHandle, workspace: String, path: Option<String>, staged: bool, untracked: bool) -> CmdResult<String> {
    git::diff(&git_target(&app, &workspace)?, path.as_deref(), staged, untracked).await
}

#[tauri::command]
async fn git_stage(app: AppHandle, workspace: String, paths: Vec<String>) -> CmdResult<()> {
    git::stage(&git_target(&app, &workspace)?, &paths).await
}

#[tauri::command]
async fn git_unstage(app: AppHandle, workspace: String, paths: Vec<String>) -> CmdResult<()> {
    git::unstage(&git_target(&app, &workspace)?, &paths).await
}

#[tauri::command]
async fn git_discard(app: AppHandle, workspace: String, tracked: Vec<String>, untracked: Vec<String>) -> CmdResult<()> {
    git::discard(&git_target(&app, &workspace)?, &tracked, &untracked).await
}

#[tauri::command]
async fn git_commit(app: AppHandle, workspace: String, message: String) -> CmdResult<String> {
    git::commit(&git_target(&app, &workspace)?, &message).await
}

#[tauri::command]
async fn git_sync(app: AppHandle, workspace: String, action: String) -> CmdResult<String> {
    let t = git_target(&app, &workspace)?;
    match action.as_str() {
        "push" => git::push(&t).await,
        "pull" => git::pull(&t).await,
        "fetch" => git::fetch(&t).await.map(|_| String::new()),
        _ => Err("Action inconnue".into()),
    }
}

#[tauri::command]
async fn git_branches(app: AppHandle, workspace: String) -> CmdResult<Vec<String>> {
    git::branches(&git_target(&app, &workspace)?).await
}

#[tauri::command]
async fn git_checkout(app: AppHandle, workspace: String, branch: String, create: bool) -> CmdResult<()> {
    git::checkout(&git_target(&app, &workspace)?, &branch, create).await
}

#[tauri::command]
async fn git_log(app: AppHandle, workspace: String, count: u32) -> CmdResult<Vec<git::Commit>> {
    git::log(&git_target(&app, &workspace)?, count).await
}

#[tauri::command]
async fn git_init(app: AppHandle, workspace: String) -> CmdResult<()> {
    git::init(&git_target(&app, &workspace)?).await
}

// ------------------------------------------------------------------ documents

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct DocumentText {
    text: String,
    method: &'static str,
    /// PDF sans texte extractible : à faire lire par un modèle vision (`pdf_page_images`).
    scanned: bool,
}

fn extract_pdf_local(bytes: Vec<u8>) -> CmdResult<(String, usize)> {
    let text = pdf_extract::extract_text_from_mem(&bytes).map_err(|e| e.to_string())?;
    let pages = text.matches('\u{c}').count().max(1);
    Ok((text, pages))
}

/// Convertit un PDF en texte, localement (rien n'est envoyé).
#[tauri::command]
async fn process_document(data_base64: String) -> CmdResult<DocumentText> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64.as_bytes())
        .map_err(|e| format!("Fichier illisible : {e}"))?;
    if !bytes.starts_with(b"%PDF") {
        return Err("Ce fichier n'est pas un PDF".into());
    }
    let (text, pages) = tokio::task::spawn_blocking(move || extract_pdf_local(bytes))
        .await
        .map_err(|_| "L'extraction a échoué sur ce PDF".to_string())?
        .map_err(|e| format!("Extraction impossible : {e}"))?;
    // Presque pas de texte : PDF scanné (images de pages).
    let scanned = text.trim().chars().count() < 20 * pages;
    Ok(DocumentText { text: if scanned { String::new() } else { text }, method: "local", scanned })
}

/// Pages lues au plus dans un PDF scanné (une requête au modèle vision par page).
const MAX_SCANNED_PAGES: usize = 40;

/// Images des pages d'un PDF scanné, pour les faire transcrire par un modèle vision.
#[tauri::command]
async fn pdf_page_images(data_base64: String) -> CmdResult<pdfscan::PageImages> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data_base64.as_bytes())
        .map_err(|e| format!("Fichier illisible : {e}"))?;
    tokio::task::spawn_blocking(move || pdfscan::page_images(&bytes, MAX_SCANNED_PAGES))
        .await
        .map_err(|e| e.to_string())?
}

// ------------------------------------------------------------------ MCP

#[tauri::command]
fn mcp_get_config(app: AppHandle) -> String {
    storage::load_mcp_config(&app)
}

#[tauri::command]
async fn mcp_save_config(app: AppHandle, mcp: State<'_, Arc<McpManager>>, text: String) -> CmdResult<()> {
    let cfg = mcp::parse_config(&text)?;
    storage::save_mcp_config(&app, &text)?;
    mcp.inner().restart(cfg).await;
    Ok(())
}

#[tauri::command]
async fn mcp_restart(app: AppHandle, mcp: State<'_, Arc<McpManager>>) -> CmdResult<()> {
    let cfg = mcp::parse_config(&storage::load_mcp_config(&app))?;
    mcp.inner().restart(cfg).await;
    Ok(())
}

#[tauri::command]
fn mcp_status(mcp: State<'_, Arc<McpManager>>) -> Vec<ServerStatus> {
    mcp.status()
}

#[tauri::command]
fn platform_info(app: AppHandle) -> serde_json::Value {
    let settings = storage::load_settings(&app).unwrap_or_default();
    let sh = shell::detect(&settings.shell);
    serde_json::json!({
        "os": std::env::consts::OS,
        "shell": sh.name,
        "shellPath": sh.program.display().to_string(),
        "shellGuidance": shell::guidance(&sh),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

#[tauri::command]
fn record_usage(app: AppHandle, day: String, model: String, prompt_tokens: u64, completion_tokens: u64) -> CmdResult<()> {
    storage::record_usage(&app, &day, &model, prompt_tokens, completion_tokens)
}

#[tauri::command]
fn get_usage(app: AppHandle) -> storage::UsageLog {
    storage::load_usage(&app)
}

#[tauri::command]
async fn check_update() -> CmdResult<updates::UpdateInfo> {
    updates::check().await
}

/// Clé publique de signature des mises à jour, fournie au moment de la compilation (CI).
fn updater_pubkey() -> Option<&'static str> {
    option_env!("TAURI_UPDATER_PUBKEY").map(str::trim).filter(|k| !k.is_empty())
}

/// Installation automatique possible ? (build signé ; sous Linux, seulement l'AppImage :
/// les paquets .deb/.rpm se mettent à jour avec le gestionnaire de paquets).
#[tauri::command]
fn updater_supported() -> bool {
    updater_pubkey().is_some() && (!cfg!(target_os = "linux") || std::env::var_os("APPIMAGE").is_some())
}

/// Télécharge la mise à jour, vérifie sa signature, l'installe puis redémarre l'application.
#[tauri::command]
async fn install_update(app: AppHandle, on_progress: Channel<serde_json::Value>) -> CmdResult<()> {
    use tauri_plugin_updater::UpdaterExt;
    if !updater_supported() {
        return Err("Installation automatique indisponible pour ce type d'installation".into());
    }
    let update = app
        .updater()
        .map_err(|e| e.to_string())?
        .check()
        .await
        .map_err(|e| format!("Vérification impossible : {e}"))?
        .ok_or("Aucune mise à jour disponible")?;
    let mut done: u64 = 0;
    update
        .download_and_install(
            |chunk, total| {
                done += chunk as u64;
                let _ = on_progress.send(serde_json::json!({ "done": done, "total": total }));
            },
            || {},
        )
        .await
        .map_err(|e| format!("Installation impossible : {e}"))?;
    app.restart()
}

/// Mode « askpass » (voir `ssh::askpass_main`) : code de sortie, ou None lors d'un lancement normal.
pub fn askpass() -> Option<i32> {
    ssh::askpass_main()
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    #[cfg(unix)]
    mcp::fix_path_env();

    let mut builder = tauri::Builder::default();
    if let Some(pubkey) = updater_pubkey() {
        builder = builder.plugin(tauri_plugin_updater::Builder::new().pubkey(pubkey).build());
    }
    let app = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(Streams::default())
        .manage(Dictation::default())
        .manage(Arc::new(McpManager::default()))
        .manage(Arc::new(tunnel::Tunnels::default()))
        .setup(|app| {
            // Chiffrement des données locales : activé avant tout accès aux conversations,
            // puis conversion des fichiers existants (synchrone, pour éviter toute course).
            if storage::load_settings(app.handle()).map(|s| s.encrypt_data).unwrap_or(false) {
                match enable_encryption(app.handle()) {
                    Ok(n) if n > 0 => eprintln!("chiffrement : {n} fichier(s) converti(s)"),
                    Ok(_) => {}
                    Err(e) => *vault::LAST_ERROR.lock().unwrap() = Some(e),
                }
            }
            let handle = app.handle().clone();
            tauri::async_runtime::spawn(async move {
                let mgr = handle.state::<Arc<McpManager>>().inner().clone();
                match mcp::parse_config(&storage::load_mcp_config(&handle)) {
                    Ok(cfg) => mgr.restart(cfg).await,
                    Err(e) => eprintln!("mcp.json invalide : {e}"),
                }
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            api_key_status,
            server_status,
            list_assistants,
            save_assistants,
            running_models,
            pull_model,
            delete_model,
            unload_model,
            preload_model,
            vault_status,
            vault_export_key,
            vault_import_key,
            set_api_key,
            delete_api_key,
            get_settings,
            save_settings,
            list_models,
            list_conversations,
            load_conversation,
            save_conversation,
            delete_conversation,
            search_conversations,
            export_conversation,
            data_dir,
            chat_stream,
            cancel_stream,
            tool_definitions,
            preview_tool,
            tool_outside,
            run_tool,
            restore_file,
            pick_workspace,
            list_workspaces,
            forget_workspace,
            touch_workspace,
            process_document,
            pdf_page_images,
            mcp_get_config,
            mcp_save_config,
            mcp_restart,
            mcp_status,
            platform_info,
            record_usage,
            get_usage,
            check_update,
            updater_supported,
            install_update,
            ssh_hosts,
            ssh_save_host,
            ssh_delete_host,
            ssh_test,
            ssh_trust,
            ssh_install_key,
            ssh_public_key,
            ssh_config_hosts,
            ssh_list_dirs,
            ssh_open_workspace,
            git_status,
            git_diff,
            git_stage,
            git_unstage,
            git_discard,
            git_commit,
            git_sync,
            git_branches,
            git_checkout,
            git_log,
            git_init,
            workspace_instructions,
            list_projects,
            save_project,
            delete_project,
            project_context,
            dictation_start,
            dictation_stop,
        ])
        .build(tauri::generate_context!())
        .expect("erreur au lancement de l'application");

    app.run(|handle, event| {
        if let tauri::RunEvent::Exit = event {
            // Arrêt propre des serveurs MCP (processus enfants).
            let mgr = handle.state::<Arc<McpManager>>().inner().clone();
            tauri::async_runtime::block_on(mgr.shutdown());
            let tunnels = handle.state::<Arc<tunnel::Tunnels>>().inner().clone();
            tauri::async_runtime::block_on(tunnels.close_except(None));
        }
    });
}

#[cfg(test)]
mod tests {
    /// PDF minimal d'une page contenant `text` (police standard Helvetica).
    pub fn tiny_pdf(text: &str) -> Vec<u8> {
        let stream = format!("BT /F1 11 Tf 40 760 Td ({text}) Tj ET");
        let objs = [
            "<< /Type /Catalog /Pages 2 0 R >>".to_string(),
            "<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_string(),
            "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 612 792] /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_string(),
            format!("<< /Length {} >>\nstream\n{stream}\nendstream", stream.len()),
            "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_string(),
        ];
        let mut out = String::from("%PDF-1.4\n");
        let mut offsets = Vec::new();
        for (i, o) in objs.iter().enumerate() {
            offsets.push(out.len());
            out.push_str(&format!("{} 0 obj\n{o}\nendobj\n", i + 1));
        }
        let xref = out.len();
        out.push_str(&format!("xref\n0 {}\n0000000000 65535 f \n", objs.len() + 1));
        for o in offsets {
            out.push_str(&format!("{o:010} 00000 n \n"));
        }
        out.push_str(&format!("trailer\n<< /Size {} /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF\n", objs.len() + 1));
        out.into_bytes()
    }

    #[test]
    fn local_pdf_extraction() {
        let (text, pages) = super::extract_pdf_local(tiny_pdf("Rapport annuel 2026")).unwrap();
        assert!(text.contains("Rapport annuel 2026"), "{text:?}");
        assert_eq!(pages, 1);
    }
}
