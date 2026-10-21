//! Dossier de projet de l'agent : local, ou distant via SSH.
//! Les mêmes outils (lecture, recherche, édition, commandes, git) s'appliquent aux deux.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Notify;

use crate::ssh::{self, quote, SshHost};
use crate::workspace;

type R<T> = Result<T, String>;

const FILE_OP_TIMEOUT: Duration = Duration::from_secs(60);
const SKIP_DIRS: [&str; 7] = [".git", "node_modules", "target", ".venv", "__pycache__", ".next", "vendor"];

/// Fichiers de consignes reconnus, par ordre de priorité.
pub const INSTRUCTION_FILES: [&str; 3] = ["AGENTS.md", "OLLAMA.md", "CLAUDE.md"];
const MAX_INSTRUCTION_BYTES: usize = 32 * 1024;
const SEP: &str = "\u{1}__MDFILE__";

#[derive(serde::Serialize, Clone, Debug)]
pub struct InstructionFile {
    pub name: String,
    pub content: String,
    pub truncated: bool,
}

/// Tronque, et ignore les doublons (CLAUDE.md lien symbolique vers AGENTS.md…).
fn dedupe_instructions(raw: Vec<(String, Vec<u8>)>) -> Vec<InstructionFile> {
    let mut out: Vec<InstructionFile> = Vec::new();
    for (name, bytes) in raw {
        let truncated = bytes.len() > MAX_INSTRUCTION_BYTES;
        let mut content = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX_INSTRUCTION_BYTES)]).into_owned();
        if truncated {
            content.push_str("\n[… tronqué]");
        }
        let content = content.trim().to_string();
        if content.is_empty() || out.iter().any(|f| f.content == content) {
            continue;
        }
        out.push(InstructionFile { name, content, truncated });
    }
    out
}

pub enum Target {
    Local(PathBuf),
    Remote { host: SshHost, root: String },
}

/// Préfixe des dossiers distants : `ssh:<id d'hôte>:<chemin absolu>`.
pub fn parse_remote(ws: &str) -> Option<(&str, &str)> {
    let rest = ws.strip_prefix("ssh:")?;
    let (id, path) = rest.split_once(':')?;
    path.starts_with('/').then_some((id, path))
}

pub fn remote_workspace(host_id: &str, path: &str) -> String {
    format!("ssh:{host_id}:{path}")
}

/// Normalise un chemin distant (style POSIX) et vérifie qu'il reste sous `root`.
pub fn remote_path(root: &str, rel: &str) -> R<String> {
    let rel = rel.trim();
    let joined = if rel.starts_with('/') { rel.to_string() } else { format!("{}/{}", root.trim_end_matches('/'), rel) };
    let mut parts: Vec<&str> = Vec::new();
    for c in joined.split('/') {
        match c {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            c => parts.push(c),
        }
    }
    let norm = format!("/{}", parts.join("/"));
    let root_norm = format!("/{}", root.split('/').filter(|c| !c.is_empty()).collect::<Vec<_>>().join("/"));
    if norm == root_norm || norm.starts_with(&format!("{}/", root_norm.trim_end_matches('/'))) || root_norm == "/" {
        Ok(norm)
    } else {
        Err(format!("Accès refusé : {rel} est en dehors du dossier de projet"))
    }
}

fn rel_of(root: &str, abs: &str) -> String {
    let r = root.trim_end_matches('/');
    abs.strip_prefix(r).map(|s| s.trim_start_matches('/')).filter(|s| !s.is_empty()).unwrap_or(".").to_string()
}

/// Dossier de projet autorisé correspondant à `ws` (chemin local ou `ssh:<hôte>:<chemin>`).
pub fn resolve(ws: &str, approved: &[String], hosts: &[SshHost]) -> R<Target> {
    // Les conversations d'anciennes versions peuvent contenir `\\?\C:\…`.
    let ws = &workspace::normalize_workspace(ws);
    if !approved.iter().any(|a| a == ws) {
        return Err("Dossier de projet non autorisé : ouvrez-le via « Ouvrir un dossier » ou « Serveur SSH »".into());
    }
    match parse_remote(ws) {
        Some((id, path)) => {
            let host = hosts.iter().find(|h| h.id == id).cloned().ok_or("Serveur SSH supprimé des réglages")?;
            Ok(Target::Remote { host, root: path.to_string() })
        }
        None => Ok(Target::Local(PathBuf::from(ws))),
    }
}

impl Target {
    async fn sh(&self, script: &str, stdin: Option<Vec<u8>>) -> R<ssh::Output> {
        match self {
            Target::Remote { host, .. } => ssh::exec(host, script, stdin, FILE_OP_TIMEOUT, None).await,
            Target::Local(_) => Err("opération réservée aux dossiers distants".into()),
        }
    }

    fn root_q(&self) -> String {
        match self {
            Target::Remote { root, .. } => quote(root),
            Target::Local(p) => quote(&p.display().to_string()),
        }
    }

    fn rpath(&self, rel: &str) -> R<String> {
        match self {
            Target::Remote { root, .. } => remote_path(root, rel),
            Target::Local(_) => Err("chemin local".into()),
        }
    }

    /// `path` est-il dans le dossier de projet ?
    pub fn contains(&self, path: &str) -> bool {
        match self {
            Target::Local(r) => workspace::resolve(r, path).is_ok(),
            Target::Remote { root, .. } => remote_path(root, path).is_ok(),
        }
    }

    /// Chemin absolu de `path` (relatif à la racine du projet ou déjà absolu), normalisé.
    pub fn absolute(&self, path: &str) -> String {
        let path = path.trim();
        match self {
            Target::Local(r) => {
                let p = Path::new(path);
                let joined = if p.is_absolute() { p.to_path_buf() } else { r.join(p) };
                let mut out = PathBuf::new();
                for c in joined.components() {
                    match c {
                        Component::ParentDir => {
                            out.pop();
                        }
                        Component::CurDir => {}
                        other => out.push(other),
                    }
                }
                workspace::simplify_path(&out).display().to_string()
            }
            Target::Remote { root, .. } => {
                let joined = if path.starts_with('/') { path.to_string() } else { format!("{}/{path}", root.trim_end_matches('/')) };
                remote_path("/", &joined).unwrap_or(joined)
            }
        }
    }

    /// Accès système : même machine, racine du système de fichiers contenant `abs`.
    pub fn system_root(&self, abs: &str) -> Target {
        match self {
            Target::Local(_) => Target::Local(Path::new(abs).ancestors().last().map(Path::to_path_buf).unwrap_or_else(|| PathBuf::from("/"))),
            Target::Remote { host, .. } => Target::Remote { host: host.clone(), root: "/".into() },
        }
    }

    /// Contenu d'un fichier distant (None s'il n'existe pas).
    async fn cat(&self, rel: &str) -> R<Option<Vec<u8>>> {
        let p = quote(&self.rpath(rel)?);
        let script = format!(
            "if [ ! -e {p} ]; then exit 44; fi\nif [ -d {p} ]; then echo 'est un dossier, utilisez list_files' >&2; exit 2; fi\n\
             s=$(wc -c < {p}) || exit 1\nif [ \"$s\" -gt {max} ]; then echo \"trop volumineux ($s octets)\" >&2; exit 3; fi\ncat -- {p}",
            max = workspace::MAX_READ_BYTES
        );
        let o = self.sh(&script, None).await?;
        match o.code {
            Some(0) => Ok(Some(o.stdout)),
            Some(44) => Ok(None),
            _ => Err(format!("{rel} : {}", o.stderr.trim())),
        }
    }

    async fn write_remote(&self, rel: &str, content: &str) -> R<()> {
        let p = quote(&self.rpath(rel)?);
        let script = format!("mkdir -p -- \"$(dirname -- {p})\" && cat > {p}");
        let o = self.sh(&script, Some(content.as_bytes().to_vec())).await?;
        if o.code == Some(0) {
            Ok(())
        } else {
            Err(format!("{rel} : {}", o.stderr.trim()))
        }
    }

    /// Fichiers de consignes à la racine du projet (AGENTS.md…), lus automatiquement par l'agent.
    pub async fn instruction_files(&self) -> R<Vec<InstructionFile>> {
        let raw: Vec<(String, Vec<u8>)> = match self {
            Target::Local(root) => INSTRUCTION_FILES
                .iter()
                .filter_map(|name| {
                    let p = root.join(name);
                    p.is_file().then(|| std::fs::read(&p).ok().map(|b| (name.to_string(), b))).flatten()
                })
                .collect(),
            Target::Remote { .. } => {
                // Un seul aller-retour SSH pour tous les fichiers.
                let names = INSTRUCTION_FILES.iter().map(|n| quote(n)).collect::<Vec<_>>().join(" ");
                let script = format!(
                    "cd -- {} || exit 1\nfor f in {names}; do if [ -f \"$f\" ]; then printf '\\001__MDFILE__%s\\n' \"$f\"; head -c {max} -- \"$f\"; fi; done",
                    self.root_q(),
                    max = MAX_INSTRUCTION_BYTES + 1
                );
                let o = self.sh(&script, None).await?;
                if o.code != Some(0) {
                    return Err(o.stderr.trim().to_string());
                }
                let text = o.stdout;
                let mut out = Vec::new();
                let sep = SEP.as_bytes();
                let starts: Vec<usize> = text.windows(sep.len()).enumerate().filter(|(_, w)| *w == sep).map(|(i, _)| i).collect();
                for (k, &i) in starts.iter().enumerate() {
                    let end = starts.get(k + 1).copied().unwrap_or(text.len());
                    let chunk = &text[i + sep.len()..end];
                    if let Some(nl) = chunk.iter().position(|&b| b == b'\n') {
                        out.push((String::from_utf8_lossy(&chunk[..nl]).into_owned(), chunk[nl + 1..].to_vec()));
                    }
                }
                out
            }
        };
        Ok(dedupe_instructions(raw))
    }

    pub async fn list_files(&self, rel: &str, depth: usize) -> R<String> {
        let root = match self {
            Target::Local(r) => return workspace::list_files(r, rel, depth),
            Target::Remote { root, .. } => root.clone(),
        };
        let abs = self.rpath(if rel.trim().is_empty() { "." } else { rel })?;
        let prune = SKIP_DIRS.iter().map(|d| format!("-name {}", quote(d))).collect::<Vec<_>>().join(" -o ");
        let script = format!(
            "cd -- {} || exit 1\nfind {} -mindepth 1 -maxdepth {} \\( {prune} \\) -prune -o -type d -exec printf '%s/\\n' {{}} + -o -print 2>/dev/null | head -n {}",
            self.root_q(),
            quote(&rel_of(&root, &abs)),
            depth.clamp(1, 6),
            workspace::MAX_LIST_ENTRIES + 1
        );
        let o = self.sh(&script, None).await?;
        let text = o.stdout_text();
        let mut entries: Vec<String> = text.lines().map(|l| l.trim_start_matches("./").to_string()).filter(|l| !l.is_empty()).collect();
        entries.sort();
        let more = entries.len() > workspace::MAX_LIST_ENTRIES;
        entries.truncate(workspace::MAX_LIST_ENTRIES);
        let mut out = entries.join("\n");
        if more {
            out.push_str("\n[… liste tronquée]");
        }
        if out.is_empty() {
            out = "(dossier vide)".into();
        }
        Ok(out)
    }

    pub async fn read_file(&self, rel: &str, offset: Option<usize>, limit: Option<usize>) -> R<String> {
        match self {
            Target::Local(r) => workspace::read_file(r, rel, offset, limit),
            Target::Remote { .. } => {
                let bytes = self.cat(rel).await?.ok_or_else(|| format!("{rel} : fichier introuvable"))?;
                workspace::number_lines(rel, &bytes, offset, limit)
            }
        }
    }

    pub async fn search(&self, pattern: &str, rel: &str, glob: Option<&str>) -> R<String> {
        let root = match self {
            Target::Local(r) => {
                let (r, p, rel, g) = (r.clone(), pattern.to_owned(), rel.to_owned(), glob.map(str::to_owned));
                return tokio::task::spawn_blocking(move || workspace::search(&r, &p, &rel, g.as_deref()))
                    .await
                    .map_err(|e| e.to_string())?;
            }
            Target::Remote { root, .. } => root.clone(),
        };
        let path = quote(&rel_of(&root, &self.rpath(if rel.trim().is_empty() { "." } else { rel })?));
        let pat = quote(pattern);
        let max = workspace::MAX_SEARCH_MATCHES;
        let rg_glob = glob.filter(|g| !g.trim().is_empty()).map(|g| format!("-g {}", quote(g))).unwrap_or_default();
        let grep_glob = glob.filter(|g| !g.trim().is_empty()).map(|g| format!("--include={}", quote(g.rsplit('/').next().unwrap_or(g)))).unwrap_or_default();
        let excludes = SKIP_DIRS.iter().map(|d| format!("--exclude-dir={}", quote(d))).collect::<Vec<_>>().join(" ");
        // ripgrep si disponible (respecte .gitignore), sinon grep -E.
        let script = format!(
            "cd -- {} || exit 1\nif command -v rg >/dev/null 2>&1; then rg -n --no-heading --color never {rg_glob} -e {pat} -- {path} | head -n {max}; \
             else grep -rnIE {excludes} {grep_glob} -e {pat} -- {path} | head -n {max}; fi",
            self.root_q()
        );
        let o = self.sh(&script, None).await?;
        let lines: Vec<String> = o
            .stdout_text()
            .lines()
            .map(|l| {
                let l = l.trim_start_matches("./");
                l.chars().take(400).collect()
            })
            .collect();
        if lines.is_empty() {
            return Ok(if o.stderr.trim().is_empty() { "Aucune correspondance".into() } else { format!("Erreur : {}", o.stderr.trim()) });
        }
        let mut out = lines.join("\n");
        if lines.len() >= max {
            out.push_str(&format!("\n[limite de {max} résultats atteinte]"));
        }
        Ok(out)
    }

    pub async fn preview_write(&self, rel: &str, content: &str) -> R<String> {
        match self {
            Target::Local(r) => workspace::preview_write(r, rel, content),
            Target::Remote { .. } => {
                let old = self.cat(rel).await?.map(|b| String::from_utf8_lossy(&b).into_owned()).unwrap_or_default();
                Ok(workspace::unified_diff(rel, &old, content))
            }
        }
    }

    pub async fn preview_edit(&self, rel: &str, old: &str, new: &str, all: bool) -> R<String> {
        match self {
            Target::Local(r) => workspace::preview_edit(r, rel, old, new, all),
            Target::Remote { .. } => {
                let cur = self.cat(rel).await?.ok_or_else(|| format!("{rel} : fichier introuvable"))?;
                let cur = String::from_utf8_lossy(&cur).into_owned();
                let updated = workspace::apply_edit(&cur, old, new, all)?;
                Ok(workspace::unified_diff(rel, &cur, &updated))
            }
        }
    }

    pub async fn write_file(&self, rel: &str, content: &str) -> R<(String, String)> {
        match self {
            Target::Local(r) => workspace::write_file(r, rel, content),
            Target::Remote { .. } => {
                let old = self.cat(rel).await?.map(|b| String::from_utf8_lossy(&b).into_owned());
                self.write_remote(rel, content).await?;
                let verb = if old.is_some() { "réécrit" } else { "créé" };
                Ok((
                    format!("Fichier {rel} {verb} ({} lignes).", content.lines().count()),
                    workspace::unified_diff(rel, old.as_deref().unwrap_or(""), content),
                ))
            }
        }
    }

    pub async fn edit_file(&self, rel: &str, old: &str, new: &str, all: bool) -> R<(String, String)> {
        match self {
            Target::Local(r) => workspace::edit_file(r, rel, old, new, all),
            Target::Remote { .. } => {
                let cur = self.cat(rel).await?.ok_or_else(|| format!("{rel} : fichier introuvable"))?;
                let cur = String::from_utf8_lossy(&cur).into_owned();
                let updated = workspace::apply_edit(&cur, old, new, all)?;
                self.write_remote(rel, &updated).await?;
                Ok((format!("Fichier {rel} modifié."), workspace::unified_diff(rel, &cur, &updated)))
            }
        }
    }

    pub async fn run_command(&self, shell: &crate::shell::Shell, command: &str, timeout_secs: u64, cancel: Arc<Notify>) -> R<String> {
        match self {
            Target::Local(r) => workspace::run_command(r, shell, command, timeout_secs, cancel).await,
            Target::Remote { host, .. } => {
                let script = format!(
                    "cd -- {} || exit 1\nexport CI=1 NO_COLOR=1 GIT_PAGER=cat PAGER=cat GIT_TERMINAL_PROMPT=0\n\
                     if command -v bash >/dev/null 2>&1; then exec bash -c {c}; else exec sh -c {c}; fi",
                    self.root_q(),
                    c = quote(command)
                );
                let o = ssh::exec(host, &script, None, Duration::from_secs(timeout_secs.clamp(1, 3600)), Some(cancel)).await?;
                Ok(workspace::format_output(o.code, &o.stdout, &o.stderr))
            }
        }
    }

    /// Lance un programme (sans shell) dans le dossier de projet.
    pub async fn exec_program(&self, program: &str, args: &[String], stdin: Option<Vec<u8>>, env: &[(&str, &str)], timeout: Duration) -> R<ssh::Output> {
        match self {
            Target::Local(root) => {
                use tokio::io::AsyncWriteExt;
                let mut cmd = tokio::process::Command::new(program);
                cmd.args(args)
                    .current_dir(workspace::simplify_path(root))
                    .stdin(if stdin.is_some() { std::process::Stdio::piped() } else { std::process::Stdio::null() })
                    .stdout(std::process::Stdio::piped())
                    .stderr(std::process::Stdio::piped())
                    .kill_on_drop(true);
                for (k, v) in env {
                    cmd.env(k, v);
                }
                #[cfg(windows)]
                cmd.creation_flags(0x0800_0000);
                let mut child = cmd.spawn().map_err(|e| format!("Impossible de lancer {program} : {e}"))?;
                if let Some(data) = stdin {
                    let mut si = child.stdin.take().unwrap();
                    si.write_all(&data).await.map_err(|e| e.to_string())?;
                    drop(si);
                }
                let out = tokio::time::timeout(timeout, child.wait_with_output())
                    .await
                    .map_err(|_| format!("{program} : délai dépassé ({} s)", timeout.as_secs()))?
                    .map_err(|e| e.to_string())?;
                Ok(ssh::Output { code: out.status.code(), stdout: out.stdout, stderr: String::from_utf8_lossy(&out.stderr).into_owned() })
            }
            Target::Remote { host, .. } => {
                let env_s: String = env.iter().map(|(k, v)| format!("{k}={} ", quote(v))).collect();
                let args_s: String = args.iter().map(|a| format!(" {}", quote(a))).collect();
                let script = format!("cd -- {} || exit 1\n{env_s}exec {}{args_s}", self.root_q(), quote(program));
                ssh::exec(host, &script, stdin, timeout, None).await
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remote_paths_stay_inside_root() {
        assert_eq!(remote_path("/srv/app", "src/main.rs").unwrap(), "/srv/app/src/main.rs");
        assert_eq!(remote_path("/srv/app", ".").unwrap(), "/srv/app");
        assert_eq!(remote_path("/srv/app/", "./a/../b").unwrap(), "/srv/app/b");
        assert_eq!(remote_path("/srv/app", "/srv/app/x").unwrap(), "/srv/app/x");
        assert!(remote_path("/srv/app", "../etc/passwd").is_err());
        assert!(remote_path("/srv/app", "/etc/passwd").is_err());
        assert!(remote_path("/srv/app", "/srv/application").is_err());
    }

    #[test]
    fn remote_workspace_round_trip() {
        let ws = remote_workspace("h1", "/var/www/site");
        assert_eq!(parse_remote(&ws), Some(("h1", "/var/www/site")));
        assert_eq!(parse_remote("/home/me/projet"), None);
        assert_eq!(parse_remote(r"C:\projet"), None);
    }

    #[test]
    fn system_access_paths() {
        let h = SshHost { id: "h".into(), label: "h".into(), host: "h".into(), port: None, user: None, identity_file: None, system: None };
        let t = Target::Remote { host: h, root: "/srv/app".into() };
        assert!(t.contains("src/main.rs") && !t.contains("/etc/fail2ban/jail.local") && !t.contains("../x"));
        assert_eq!(t.absolute("/etc/fail2ban/jail.local"), "/etc/fail2ban/jail.local");
        assert_eq!(t.absolute("../other/x"), "/srv/other/x");
        assert!(t.system_root("/etc/fail2ban/jail.local").contains("/etc/fail2ban/jail.local"));

        let dir = std::env::temp_dir().join(format!("md-sys-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = Target::Local(dir.clone());
        let outside = std::env::temp_dir().join("md-outside.txt").display().to_string();
        assert!(!t.contains(&outside));
        assert!(t.system_root(&t.absolute(&outside)).contains(&outside));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[tokio::test]
    async fn local_instruction_files() {
        let dir = std::env::temp_dir().join(format!("md-instr-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let t = Target::Local(dir.clone());
        assert!(t.instruction_files().await.unwrap().is_empty());
        std::fs::write(dir.join("AGENTS.md"), "# Règles\nTests : cargo test\n").unwrap();
        std::fs::write(dir.join("CLAUDE.md"), "# Règles\nTests : cargo test\n").unwrap();
        std::fs::write(dir.join("OLLAMA.md"), "x".repeat(MAX_INSTRUCTION_BYTES + 10)).unwrap();
        let files = t.instruction_files().await.unwrap();
        let names: Vec<&str> = files.iter().map(|f| f.name.as_str()).collect();
        // CLAUDE.md identique à AGENTS.md : ignoré.
        assert_eq!(names, ["AGENTS.md", "OLLAMA.md"]);
        assert!(files[1].truncated && files[1].content.ends_with("[… tronqué]"));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}

/// Test d'intégration contre un vrai serveur SSH, activé par variables d'environnement :
/// MD_SSH_TEST_HOST=127.0.0.1 MD_SSH_TEST_PORT=2222 MD_SSH_TEST_KEY=~/.ssh/cle MD_SSH_TEST_USER=root
#[cfg(test)]
mod ssh_integration {
    use super::*;

    fn host() -> Option<SshHost> {
        Some(SshHost {
            id: "test".into(),
            label: "test".into(),
            host: std::env::var("MD_SSH_TEST_HOST").ok()?,
            port: std::env::var("MD_SSH_TEST_PORT").ok().and_then(|p| p.parse().ok()),
            user: std::env::var("MD_SSH_TEST_USER").ok(),
            identity_file: std::env::var("MD_SSH_TEST_KEY").ok(),
            system: None,
        })
    }

    #[tokio::test]
    async fn full_remote_workflow() {
        let Some(h) = host() else { return };
        // 1. Hôte inconnu : empreinte proposée, puis confiance.
        let t1 = ssh::test(&h).await;
        if !t1.ok {
            assert!(t1.needs_trust, "{:?}", t1.error);
            assert!(t1.fingerprints.iter().any(|f| f.contains("SHA256:")), "{:?}", t1.fingerprints);
            ssh::trust(&h).await.unwrap();
        }
        let t2 = ssh::test(&h).await;
        assert!(t2.ok, "{:?}", t2.error);
        assert!(t2.system.unwrap_or_default().contains("Linux"));

        // 2. Dossier distant avec espace et apostrophe dans le nom.
        let root = format!("/tmp/md remote l'essai {}", uuid::Uuid::new_v4().simple());
        ssh::exec(&h, &format!("mkdir -p {}", quote(&root)), None, Duration::from_secs(20), None).await.unwrap();
        let listing = ssh::list_dirs(&h, "/tmp").await.unwrap();
        assert!(listing.dirs.iter().any(|d| root.ends_with(d.as_str())));
        let t = Target::Remote { host: h.clone(), root: root.clone() };

        // 3. Fichiers.
        let (msg, diff) = t.write_file("src/main.py", "print('bonjour')\n").await.unwrap();
        assert!(msg.contains("créé") && diff.contains("+print('bonjour')"));
        assert!(t.read_file("src/main.py", None, None).await.unwrap().contains("     1\tprint('bonjour')"));
        let prev = t.preview_edit("src/main.py", "bonjour", "salut", false).await.unwrap();
        assert!(prev.contains("+print('salut')"));
        t.edit_file("src/main.py", "bonjour", "salut", false).await.unwrap();
        assert!(t.read_file("src/main.py", None, None).await.unwrap().contains("salut"));
        assert!(t.list_files(".", 3).await.unwrap().contains("src/main.py"));
        assert!(t.search("salut", ".", Some("*.py")).await.unwrap().contains("src/main.py:1:"));
        assert!(t.read_file("../../etc/passwd", None, None).await.is_err());

        // 4. Commandes (bash, code de sortie, dossier courant).
        let sh = crate::shell::detect("auto");
        let out = t.run_command(&sh, "pwd && [[ 1 -eq 1 ]] && echo bash-ok", 30, Arc::new(Notify::new())).await.unwrap();
        assert!(out.contains("Code de sortie : 0") && out.contains("bash-ok") && out.contains(&root), "{out}");
        let out = t.run_command(&sh, "exit 7", 30, Arc::new(Notify::new())).await.unwrap();
        assert!(out.contains("Code de sortie : 7"), "{out}");

        // 5. Annulation : le processus distant est tué.
        let cancel = Arc::new(Notify::new());
        let c2 = cancel.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(2)).await;
            c2.notify_one();
        });
        let err = t.run_command(&sh, "sleep 300 # md-cancel-marker", 60, cancel).await.unwrap_err();
        assert!(err.contains("annulée"));
        let ps = ssh::exec(&h, "ps -eo args | grep -c '[m]d-cancel-marker' || true", None, Duration::from_secs(20), None).await.unwrap();
        let detail = ssh::exec(&h, "ps -eo pid,pgid,stat,args | grep '[m]d-cancel-marker' || true", None, Duration::from_secs(20), None).await.unwrap();
        assert_eq!(ps.stdout_text().trim(), "0", "le processus distant tourne encore : {}", detail.stdout_text());

        // 6. Fichiers de consignes.
        assert!(t.instruction_files().await.unwrap().is_empty());
        t.write_file("AGENTS.md", "Toujours lancer pytest.").await.unwrap();
        t.write_file("CLAUDE.md", "Style : PEP 8").await.unwrap();
        let files = t.instruction_files().await.unwrap();
        assert_eq!(files.len(), 2, "{files:?}");
        assert_eq!((files[0].name.as_str(), files[0].content.as_str()), ("AGENTS.md", "Toujours lancer pytest."));
        assert_eq!((files[1].name.as_str(), files[1].content.as_str()), ("CLAUDE.md", "Style : PEP 8"));

        // 7. Git distant.
        assert!(!crate::git::status(&t).await.unwrap().is_repo);
        crate::git::init(&t).await.unwrap();
        t.run_command(&sh, "git config user.name T && git config user.email t@e.org", 30, Arc::new(Notify::new())).await.unwrap();
        crate::git::stage(&t, &[]).await.unwrap();
        assert!(crate::git::commit(&t, "init distant").await.unwrap().contains("init distant"));
        t.write_file("src/main.py", "print('modifié')\n").await.unwrap();
        let st = crate::git::status(&t).await.unwrap();
        assert_eq!(st.files.len(), 1);
        assert!(crate::git::diff(&t, Some("src/main.py"), false, false).await.unwrap().contains("+print('modifié')"));
        assert_eq!(crate::git::log(&t, 5).await.unwrap().len(), 1);

        ssh::exec(&h, &format!("rm -rf {}", quote(&root)), None, Duration::from_secs(20), None).await.unwrap();
    }
}
