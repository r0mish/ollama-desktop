//! Connexions SSH via le client OpenSSH du système : ~/.ssh/config, alias,
//! ProxyJump, agent et clés fonctionnent comme dans un terminal.
//!
//! - `BatchMode=yes` : jamais d'invite interactive (mot de passe, phrase secrète) ;
//!   l'authentification passe par l'agent SSH ou une clé sans phrase secrète.
//! - Hôtes inconnus : l'empreinte est montrée à l'utilisateur avant toute confiance.
//! - Sous macOS/Linux, les connexions sont réutilisées (ControlMaster).

use std::path::PathBuf;
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::Notify;

type R<T> = Result<T, String>;

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct SshHost {
    #[serde(default)]
    pub id: String,
    /// Nom affiché.
    pub label: String,
    /// Alias de ~/.ssh/config, nom d'hôte ou adresse IP.
    pub host: String,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub user: Option<String>,
    /// Clé privée à utiliser (sinon : agent SSH et configuration habituelle).
    #[serde(default)]
    pub identity_file: Option<String>,
    /// Dernier système détecté (`uname -srm`), pour informer le modèle.
    #[serde(default)]
    pub system: Option<String>,
}

pub struct Output {
    pub code: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: String,
}

impl Output {
    pub fn stdout_text(&self) -> String {
        String::from_utf8_lossy(&self.stdout).into_owned()
    }
}

/// Citation shell POSIX : 'it'\''s'.
pub fn quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

fn exe(name: &str) -> PathBuf {
    if cfg!(windows) {
        // Client OpenSSH intégré à Windows 10/11.
        if let Some(root) = std::env::var_os("SystemRoot") {
            let p = PathBuf::from(root).join("System32").join("OpenSSH").join(format!("{name}.exe"));
            if p.is_file() {
                return p;
            }
        }
        PathBuf::from(format!("{name}.exe"))
    } else {
        PathBuf::from(name)
    }
}

fn valid_token(s: &str) -> bool {
    !s.is_empty() && !s.starts_with('-') && !s.chars().any(|c| c.is_whitespace() || c.is_control())
}

pub fn validate(h: &SshHost) -> R<()> {
    if !valid_token(&h.host) {
        return Err("Hôte invalide (ni espace, ni tiret initial)".into());
    }
    if let Some(u) = h.user.as_deref().filter(|u| !u.is_empty()) {
        if !valid_token(u) {
            return Err("Nom d'utilisateur invalide".into());
        }
    }
    if let Some(k) = h.identity_file.as_deref().filter(|k| !k.is_empty()) {
        if !expand_home(k).is_file() {
            return Err(format!("Clé privée introuvable : {k}"));
        }
    }
    Ok(())
}

fn home() -> Option<PathBuf> {
    std::env::var_os("HOME").or_else(|| std::env::var_os("USERPROFILE")).map(PathBuf::from)
}

fn expand_home(p: &str) -> PathBuf {
    match (p.strip_prefix("~/").or_else(|| p.strip_prefix("~\\")), home()) {
        (Some(rest), Some(h)) => h.join(rest),
        _ => PathBuf::from(p),
    }
}

/// Dossier court pour les sockets ControlMaster (limite de ~104 caractères sous macOS).
#[cfg(unix)]
fn control_dir() -> Option<PathBuf> {
    use std::os::unix::fs::{DirBuilderExt, MetadataExt};
    let uid = unsafe { libc::getuid() };
    let dir = PathBuf::from(format!("/tmp/md-ssh-{uid}"));
    if !dir.exists() {
        std::fs::DirBuilder::new().mode(0o700).create(&dir).ok()?;
    }
    // On refuse un dossier qui ne nous appartient pas ou ouvert à d'autres.
    let meta = std::fs::metadata(&dir).ok()?;
    (meta.uid() == uid && meta.mode() & 0o077 == 0).then_some(dir)
}

fn common_args(h: &SshHost) -> Vec<String> {
    let mut a: Vec<String> = [
        "-o", "BatchMode=yes",
        "-o", "ConnectTimeout=15",
        "-o", "ServerAliveInterval=15",
        "-o", "ServerAliveCountMax=3",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect();
    if let Some(p) = h.port {
        a.extend(["-p".into(), p.to_string()]);
    }
    if let Some(u) = h.user.as_deref().filter(|u| !u.is_empty()) {
        a.extend(["-l".into(), u.into()]);
    }
    if let Some(k) = h.identity_file.as_deref().filter(|k| !k.is_empty()) {
        a.extend(["-i".into(), expand_home(k).display().to_string(), "-o".into(), "IdentitiesOnly=yes".into()]);
    }
    a
}

fn base_command(h: &SshHost, extra: &[&str]) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(exe("ssh"));
    cmd.arg("-T").args(common_args(h)).args(extra);
    #[cfg(unix)]
    if let Some(dir) = control_dir() {
        cmd.args([
            "-o",
            "ControlMaster=auto",
            "-o",
            &format!("ControlPath={}/%C", dir.display()),
            "-o",
            "ControlPersist=600",
        ]);
    }
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    cmd.kill_on_drop(true);
    cmd
}

/// Traduit les erreurs de connexion d'OpenSSH en messages clairs.
fn connection_error(stderr: &str) -> Option<String> {
    let s = stderr.to_lowercase();
    let msg = if s.contains("host key verification failed") || s.contains("no matching host key") && s.contains("known_hosts") {
        "HOSTKEY: hôte inconnu ou clé d'hôte modifiée. Vérifiez l'empreinte avant de lui faire confiance."
    } else if s.contains("remote host identification has changed") {
        "ATTENTION : la clé de cet hôte a changé (attaque possible ou serveur réinstallé). Vérifiez puis corrigez ~/.ssh/known_hosts."
    } else if s.contains("permission denied") {
        "Authentification refusée : ajoutez votre clé à l'agent SSH (ssh-add) ou indiquez une clé sans phrase secrète. Les mots de passe ne sont pas pris en charge."
    } else if s.contains("could not resolve hostname") {
        "Nom d'hôte introuvable (DNS ou alias ~/.ssh/config)."
    } else if s.contains("connection refused") {
        "Connexion refusée (service SSH arrêté ou mauvais port)."
    } else if s.contains("timed out") || s.contains("no route to host") || s.contains("network is unreachable") {
        "Serveur injoignable (délai dépassé)."
    } else if s.contains("connection closed") || s.contains("broken pipe") || s.contains("kex_exchange_identification") {
        "La connexion SSH a été fermée par le serveur."
    } else {
        return None;
    };
    Some(msg.into())
}

const PID_MARKER: &str = "__MDPID__";

/// Exécute `script` avec `sh` sur l'hôte distant.
/// Si `cancel` se déclenche (ou au délai), le groupe de processus distant est tué.
pub async fn exec(h: &SshHost, script: &str, stdin: Option<Vec<u8>>, timeout: Duration, cancel: Option<Arc<Notify>>) -> R<Output> {
    validate(h)?;
    // $$ est le PID du shell qui exécute le script, chef de session côté sshd :
    // il sert à tuer toute la commande en cas d'annulation.
    let remote = format!("exec sh -c {}", quote(&format!("echo {PID_MARKER}$$ >&2\n{script}")));
    let mut cmd = base_command(h, &[]);
    cmd.arg("--").arg(&h.host).arg(remote);
    cmd.stdin(if stdin.is_some() { Stdio::piped() } else { Stdio::null() })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().map_err(|e| format!("Impossible de lancer ssh : {e}. Le client OpenSSH est-il installé ?"))?;

    if let Some(data) = stdin {
        let mut si = child.stdin.take().unwrap();
        tokio::spawn(async move {
            let _ = si.write_all(&data).await;
            let _ = si.shutdown().await;
        });
    }
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let err_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let err_task = {
        let buf = err_buf.clone();
        tokio::spawn(async move {
            let mut chunk = [0u8; 4096];
            while let Ok(n) = err.read(&mut chunk).await {
                if n == 0 {
                    break;
                }
                buf.lock().unwrap().extend_from_slice(&chunk[..n]);
            }
        })
    };
    let run = async {
        let mut o = Vec::new();
        let _ = out.read_to_end(&mut o).await;
        let status = child.wait().await;
        let _ = err_task.await;
        (o, status)
    };
    let cancelled = async {
        match &cancel {
            Some(c) => c.notified().await,
            None => std::future::pending().await,
        }
    };

    let remote_pid = || {
        let e = String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned();
        e.lines().find_map(|l| l.trim().strip_prefix(PID_MARKER).and_then(|p| p.parse::<u32>().ok()))
    };
    // Arrêt du groupe de processus distant : TERM, attente, puis KILL si nécessaire.
    // Exécuté par sh (indépendamment du shell de connexion) et attendu avant de rendre la main.
    let kill_remote = |pid: Option<u32>| {
        let host = h.clone();
        async move {
            let Some(pid) = pid else { return };
            let script = format!(
                "kill -TERM -{pid} 2>/dev/null || exit 0; for i in 1 2 3 4 5; do kill -0 -{pid} 2>/dev/null || exit 0; sleep 1; done; kill -KILL -{pid} 2>/dev/null; true"
            );
            let mut k = base_command(&host, &[]);
            k.arg("--").arg(&host.host).arg(format!("exec sh -c {}", quote(&script)));
            k.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::null());
            if let Ok(mut c) = k.spawn() {
                let _ = tokio::time::timeout(Duration::from_secs(30), c.wait()).await;
            }
        }
    };

    let (stdout, status) = tokio::select! {
        r = run => r,
        _ = tokio::time::sleep(timeout) => {
            kill_remote(remote_pid()).await;
            return Err(format!("Délai dépassé ({} s) : commande distante interrompue", timeout.as_secs()));
        }
        _ = cancelled => {
            kill_remote(remote_pid()).await;
            return Err("Commande annulée par l'utilisateur".into());
        }
    };
    let status = status.map_err(|e| e.to_string())?;
    let raw_err = String::from_utf8_lossy(&err_buf.lock().unwrap()).into_owned();
    let marker_seen = raw_err.contains(PID_MARKER);
    let stderr: String = raw_err
        .lines()
        .filter(|l| !l.trim().starts_with(PID_MARKER))
        .collect::<Vec<_>>()
        .join("\n");
    // Code 255 sans que le script ait démarré : échec de connexion SSH.
    if status.code() == Some(255) && !marker_seen {
        return Err(connection_error(&stderr).unwrap_or_else(|| format!("Échec de la connexion SSH : {}", stderr.trim())));
    }
    Ok(Output { code: status.code(), stdout, stderr })
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct TestResult {
    pub ok: bool,
    pub error: Option<String>,
    /// L'hôte n'est pas (encore) dans known_hosts : afficher les empreintes.
    pub needs_trust: bool,
    pub fingerprints: Vec<String>,
    pub system: Option<String>,
    pub user: Option<String>,
    pub home: Option<String>,
    pub git: Option<String>,
}

pub async fn test(h: &SshHost) -> TestResult {
    let script = r#"printf '%s\n' "$(uname -srm 2>/dev/null)" "$(id -un 2>/dev/null)" "$HOME" "$(git --version 2>/dev/null)""#;
    match exec(h, script, None, Duration::from_secs(30), None).await {
        Ok(o) => {
            let text = o.stdout_text();
            let mut l = text.lines().map(|s| s.trim().to_string()).map(|s| (!s.is_empty()).then_some(s));
            TestResult {
                ok: true,
                system: l.next().flatten(),
                user: l.next().flatten(),
                home: l.next().flatten(),
                git: l.next().flatten(),
                ..Default::default()
            }
        }
        Err(e) if e.starts_with("HOSTKEY:") => {
            let fingerprints = scan(h).await.map(|(_, f)| f).unwrap_or_default();
            TestResult {
                error: Some(e.trim_start_matches("HOSTKEY:").trim().to_string()),
                needs_trust: true,
                fingerprints,
                ..Default::default()
            }
        }
        Err(e) => TestResult { error: Some(e), ..Default::default() },
    }
}

/// Nom d'hôte et port effectifs (résolution de ~/.ssh/config via `ssh -G`).
async fn resolve(h: &SshHost) -> R<(String, u16)> {
    let mut cmd = tokio::process::Command::new(exe("ssh"));
    cmd.arg("-G").args(common_args(h)).arg("--").arg(&h.host);
    #[cfg(windows)]
    cmd.creation_flags(0x0800_0000);
    let out = cmd.output().await.map_err(|e| e.to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let get = |k: &str| text.lines().find_map(|l| l.strip_prefix(k).map(|v| v.trim().to_string()));
    let hostname = get("hostname ").unwrap_or_else(|| h.host.clone());
    let port = get("port ").and_then(|p| p.parse().ok()).unwrap_or(22);
    Ok((hostname, port))
}

/// Clés publiques de l'hôte (format known_hosts) et leurs empreintes SHA256.
async fn scan(h: &SshHost) -> R<(Vec<String>, Vec<String>)> {
    let (hostname, port) = resolve(h).await?;
    let mut ks = tokio::process::Command::new(exe("ssh-keyscan"));
    ks.args(["-T", "10", "-p", &port.to_string(), "--", &hostname]);
    #[cfg(windows)]
    ks.creation_flags(0x0800_0000);
    let out = ks.output().await.map_err(|e| format!("ssh-keyscan indisponible : {e}"))?;
    let lines: Vec<String> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| !l.trim().is_empty() && !l.starts_with('#'))
        .map(str::to_owned)
        .collect();
    if lines.is_empty() {
        return Ok((lines, Vec::new()));
    }
    let mut kg = tokio::process::Command::new(exe("ssh-keygen"));
    kg.args(["-l", "-f", "-"]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    kg.creation_flags(0x0800_0000);
    let mut child = kg.spawn().map_err(|e| e.to_string())?;
    let mut si = child.stdin.take().unwrap();
    si.write_all(lines.join("\n").as_bytes()).await.map_err(|e| e.to_string())?;
    drop(si);
    let fp = child.wait_with_output().await.map_err(|e| e.to_string())?;
    let fingerprints = String::from_utf8_lossy(&fp.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
    Ok((lines, fingerprints))
}

/// Ajoute la clé de l'hôte à ~/.ssh/known_hosts, comme la réponse « yes » d'ssh.
pub async fn trust(h: &SshHost) -> R<()> {
    validate(h)?;
    let (lines, _) = scan(h).await.unwrap_or_default();
    if lines.is_empty() {
        // Rebond (ProxyJump…) : ssh-keyscan ne passe pas, on laisse ssh enregistrer la clé.
        let mut cmd = base_command(h, &["-o", "StrictHostKeyChecking=accept-new"]);
        cmd.arg("--").arg(&h.host).arg("true").stdin(Stdio::null());
        let out = cmd.output().await.map_err(|e| e.to_string())?;
        if !out.status.success() {
            let e = String::from_utf8_lossy(&out.stderr);
            return Err(connection_error(&e).unwrap_or_else(|| e.trim().to_string()));
        }
        return Ok(());
    }
    let dir = home().ok_or("Dossier personnel introuvable")?.join(".ssh");
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join("known_hosts");
    let mut existing = std::fs::read_to_string(&path).unwrap_or_default();
    if !existing.is_empty() && !existing.ends_with('\n') {
        existing.push('\n');
    }
    for l in lines {
        if !existing.contains(&l) {
            existing.push_str(&l);
            existing.push('\n');
        }
    }
    std::fs::write(&path, existing).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

#[derive(Serialize)]
pub struct DirListing {
    pub path: String,
    pub dirs: Vec<String>,
}

/// Liste les sous-dossiers d'un dossier distant (vide = dossier personnel).
pub async fn list_dirs(h: &SshHost, path: &str) -> R<DirListing> {
    let cd = if path.trim().is_empty() { "cd".to_string() } else { format!("cd -- {}", quote(path.trim())) };
    let script = format!(
        "{cd} 2>/dev/null || {{ echo 'Dossier introuvable' >&2; exit 2; }}\npwd\nfor d in .* *; do [ -d \"$d\" ] && [ \"$d\" != . ] && [ \"$d\" != .. ] && printf '%s\\n' \"$d\"; done; true"
    );
    let o = exec(h, &script, None, Duration::from_secs(30), None).await?;
    if o.code != Some(0) {
        return Err(o.stderr.trim().to_string());
    }
    let text = o.stdout_text();
    let mut lines = text.lines();
    let path = lines.next().unwrap_or("/").to_string();
    let mut dirs: Vec<String> = lines.map(str::to_owned).collect();
    dirs.sort_by_key(|d| (d.starts_with('.'), d.to_lowercase()));
    Ok(DirListing { path, dirs })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quoting_survives_single_quotes() {
        assert_eq!(quote("it's"), r"'it'\''s'");
        assert_eq!(quote("a b; rm -rf /"), "'a b; rm -rf /'");
    }

    #[test]
    fn hosts_cannot_inject_options() {
        let mut h = SshHost { host: "-oProxyCommand=evil".into(), ..Default::default() };
        assert!(validate(&h).is_err());
        h.host = "srv prod".into();
        assert!(validate(&h).is_err());
        h.host = "admin@10.0.0.5".into();
        assert!(validate(&h).is_ok());
        h.user = Some("-x".into());
        assert!(validate(&h).is_err());
    }

    #[test]
    fn connection_errors_are_translated() {
        assert!(connection_error("Host key verification failed.").unwrap().starts_with("HOSTKEY:"));
        assert!(connection_error("user@h: Permission denied (publickey).").unwrap().contains("agent SSH"));
        assert!(connection_error("ssh: Could not resolve hostname x").is_some());
    }
}
