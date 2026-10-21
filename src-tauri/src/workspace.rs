//! Outils du mode Code : lecture/écriture de fichiers et commandes, confinés au
//! dossier de projet choisi par l'utilisateur via la boîte de dialogue native.

use std::fs;
use std::path::{Component, Path, PathBuf};
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use ignore::WalkBuilder;
use regex::Regex;
use tokio::io::AsyncReadExt;
use tokio::sync::Notify;

use crate::shell::Shell;

type R<T> = Result<T, String>;

pub const MAX_READ_BYTES: u64 = 5 * 1024 * 1024;
const DEFAULT_READ_LINES: usize = 2000;
const MAX_LINE_CHARS: usize = 2000;
pub const MAX_LIST_ENTRIES: usize = 500;
pub const MAX_SEARCH_MATCHES: usize = 200;
const MAX_OUTPUT_CHARS: usize = 30_000;

/// Retire le préfixe « verbatim » `\\?\` que `canonicalize()` ajoute sous Windows :
/// beaucoup de programmes (cmd.exe, scripts npm…) le refusent comme dossier courant.
pub fn simplify_path(p: &Path) -> PathBuf {
    let s = p.to_string_lossy();
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = s.strip_prefix(r"\\?\") {
        if rest.as_bytes().get(1) == Some(&b':') {
            return PathBuf::from(rest);
        }
    }
    p.to_path_buf()
}

/// Forme canonique d'un dossier de projet enregistré (chemins locaux sans préfixe `\\?\`).
pub fn normalize_workspace(ws: &str) -> String {
    if ws.starts_with("ssh:") {
        return ws.to_string();
    }
    simplify_path(Path::new(ws)).display().to_string()
}

/// Normalisation purement lexicale (`a/./b/../c` → `a/c`), valable même si le chemin n'existe pas.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other),
        }
    }
    out
}

/// Résout un chemin relatif (ou absolu) en garantissant qu'il reste dans `root`,
/// y compris à travers les liens symboliques.
pub fn resolve(root: &Path, rel: &str) -> R<PathBuf> {
    let root = root.canonicalize().map_err(|e| format!("Dossier de projet introuvable : {e}"))?;
    let rel = rel.trim();
    let candidate = if rel.is_empty() || rel == "." {
        root.clone()
    } else {
        let p = Path::new(rel);
        normalize(&if p.is_absolute() { p.to_path_buf() } else { root.join(p) })
    };
    // On remonte jusqu'au premier ancêtre existant pour suivre les liens symboliques.
    let mut existing = candidate.as_path();
    let mut tail = Vec::new();
    while !existing.exists() {
        tail.push(existing.file_name().ok_or("Chemin invalide")?.to_owned());
        existing = existing.parent().ok_or("Chemin invalide")?;
    }
    let mut real = existing.canonicalize().map_err(|e| e.to_string())?;
    for t in tail.iter().rev() {
        real.push(t);
    }
    if !real.starts_with(&root) {
        return Err(format!("Accès refusé : {rel} est en dehors du dossier de projet"));
    }
    Ok(real)
}

fn rel_display(root: &Path, p: &Path) -> String {
    let root = root.canonicalize().unwrap_or_else(|_| root.to_path_buf());
    p.strip_prefix(&root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

pub fn list_files(root: &Path, path: &str, depth: usize) -> R<String> {
    let dir = resolve(root, path)?;
    if !dir.is_dir() {
        return Err(format!("{path} n'est pas un dossier"));
    }
    let mut entries: Vec<String> = WalkBuilder::new(&dir)
        .max_depth(Some(depth.clamp(1, 6)))
        .hidden(false)
        .filter_entry(|e| e.file_name() != ".git")
        .build()
        .flatten()
        .filter(|e| e.depth() > 0)
        .map(|e| {
            let mut s = rel_display(root, e.path());
            if e.file_type().is_some_and(|t| t.is_dir()) {
                s.push('/');
            }
            s
        })
        .collect();
    entries.sort();
    let total = entries.len();
    entries.truncate(MAX_LIST_ENTRIES);
    let mut out = entries.join("\n");
    if total > MAX_LIST_ENTRIES {
        out.push_str(&format!("\n[… {} entrées supplémentaires non affichées]", total - MAX_LIST_ENTRIES));
    }
    if out.is_empty() {
        out = "(dossier vide)".into();
    }
    Ok(out)
}

pub fn read_file(root: &Path, path: &str, offset: Option<usize>, limit: Option<usize>) -> R<String> {
    let p = resolve(root, path)?;
    let meta = fs::metadata(&p).map_err(|e| format!("{path} : {e}"))?;
    if meta.is_dir() {
        return Err(format!("{path} est un dossier, utilisez list_files"));
    }
    if meta.len() > MAX_READ_BYTES {
        return Err(format!("{path} est trop volumineux ({} Mo)", meta.len() / 1_048_576));
    }
    let bytes = fs::read(&p).map_err(|e| e.to_string())?;
    number_lines(path, &bytes, offset, limit)
}

/// Contenu numéroté (numéro<TAB>ligne), avec pagination ; partagé avec les dossiers distants.
pub fn number_lines(path: &str, bytes: &[u8], offset: Option<usize>, limit: Option<usize>) -> R<String> {
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Err(format!("{path} est un fichier binaire"));
    }
    let text = String::from_utf8_lossy(bytes);
    let start = offset.unwrap_or(1).max(1);
    let limit = limit.unwrap_or(DEFAULT_READ_LINES).clamp(1, DEFAULT_READ_LINES);
    let total = text.lines().count();
    let mut out = String::new();
    for (i, line) in text.lines().enumerate().skip(start - 1).take(limit) {
        let line: String = line.chars().take(MAX_LINE_CHARS).collect();
        out.push_str(&format!("{:>6}\t{}\n", i + 1, line));
    }
    if total == 0 {
        return Ok("(fichier vide)".into());
    }
    let end = (start - 1 + limit).min(total);
    if start > 1 || end < total {
        out.push_str(&format!("\n[lignes {start}-{end} sur {total}]"));
    }
    Ok(out)
}

pub fn search(root: &Path, pattern: &str, path: &str, glob: Option<&str>) -> R<String> {
    let re = Regex::new(pattern).map_err(|e| format!("Expression régulière invalide : {e}"))?;
    let base = resolve(root, path)?;
    let mut walker = WalkBuilder::new(&base);
    walker.hidden(false).filter_entry(|e| e.file_name() != ".git");
    if let Some(g) = glob.filter(|g| !g.trim().is_empty()) {
        let mut ob = ignore::overrides::OverrideBuilder::new(&base);
        ob.add(g).map_err(|e| e.to_string())?;
        walker.overrides(ob.build().map_err(|e| e.to_string())?);
    }
    let mut out = String::new();
    let mut count = 0;
    'files: for entry in walker.build().flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if entry.metadata().map(|m| m.len() > MAX_READ_BYTES).unwrap_or(true) {
            continue;
        }
        let Ok(bytes) = fs::read(entry.path()) else { continue };
        if bytes.iter().take(8000).any(|&b| b == 0) {
            continue;
        }
        let text = String::from_utf8_lossy(&bytes);
        for (i, line) in text.lines().enumerate() {
            if re.is_match(line) {
                let line: String = line.trim().chars().take(300).collect();
                out.push_str(&format!("{}:{}: {}\n", rel_display(root, entry.path()), i + 1, line));
                count += 1;
                if count >= MAX_SEARCH_MATCHES {
                    out.push_str(&format!("[limite de {MAX_SEARCH_MATCHES} résultats atteinte]\n"));
                    break 'files;
                }
            }
        }
    }
    Ok(if out.is_empty() { "Aucune correspondance".into() } else { out })
}

pub fn unified_diff(path: &str, old: &str, new: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    let mut s = diff
        .unified_diff()
        .context_radius(3)
        .header(&format!("a/{}", path.trim_start_matches('/')), &format!("b/{}", path.trim_start_matches('/')))
        .to_string();
    if s.len() > 60_000 {
        let mut cut = 60_000;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[… diff tronqué]");
    }
    s
}

/// Contenu après remplacement, ou erreur si `old` est absent / ambigu.
pub fn apply_edit(current: &str, old: &str, new: &str, replace_all: bool) -> R<String> {
    if old.is_empty() {
        return Err("old_string ne peut pas être vide (utilisez write_file pour créer un fichier)".into());
    }
    // Tolère les fichiers en CRLF quand le modèle envoie des \n.
    let (old, new) = if !current.contains(old) && current.contains("\r\n") && !old.contains('\r') {
        (old.replace('\n', "\r\n"), new.replace('\n', "\r\n"))
    } else {
        (old.to_owned(), new.to_owned())
    };
    match current.matches(old.as_str()).count() {
        0 => Err("old_string introuvable dans le fichier (vérifiez espaces et indentation, relisez le fichier)".into()),
        1 => Ok(current.replacen(old.as_str(), &new, 1)),
        _ if replace_all => Ok(current.replace(old.as_str(), &new)),
        n => Err(format!("old_string apparaît {n} fois : ajoutez du contexte pour la rendre unique, ou replace_all=true")),
    }
}

/// Calcule le diff d'une modification sans rien écrire (pour l'approbation).
pub fn preview_write(root: &Path, path: &str, content: &str) -> R<String> {
    let p = resolve(root, path)?;
    let old = fs::read_to_string(&p).unwrap_or_default();
    Ok(unified_diff(path, &old, content))
}

pub fn preview_edit(root: &Path, path: &str, old: &str, new: &str, replace_all: bool) -> R<String> {
    let p = resolve(root, path)?;
    let current = fs::read_to_string(&p).map_err(|e| format!("{path} : {e}"))?;
    let updated = apply_edit(&current, old, new, replace_all)?;
    Ok(unified_diff(path, &current, &updated))
}

pub fn write_file(root: &Path, path: &str, content: &str) -> R<(String, String)> {
    let p = resolve(root, path)?;
    let old = fs::read_to_string(&p).ok();
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(&p, content).map_err(|e| format!("{path} : {e}"))?;
    let diff = unified_diff(path, old.as_deref().unwrap_or(""), content);
    let msg = match old {
        Some(_) => format!("Fichier {path} réécrit ({} lignes).", content.lines().count()),
        None => format!("Fichier {path} créé ({} lignes).", content.lines().count()),
    };
    Ok((msg, diff))
}

pub fn edit_file(root: &Path, path: &str, old: &str, new: &str, replace_all: bool) -> R<(String, String)> {
    let p = resolve(root, path)?;
    let current = fs::read_to_string(&p).map_err(|e| format!("{path} : {e}"))?;
    let updated = apply_edit(&current, old, new, replace_all)?;
    fs::write(&p, &updated).map_err(|e| e.to_string())?;
    Ok((format!("Fichier {path} modifié."), unified_diff(path, &current, &updated)))
}

pub fn truncate_middle(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        return s.to_owned();
    }
    let head: String = s.chars().take(max / 2).collect();
    let tail: String = s.chars().skip(n - max / 2).collect();
    format!("{head}\n\n[… {} caractères omis …]\n\n{tail}", n - max)
}

/// Exécute une commande shell dans le dossier du projet, avec délai max et annulation.
pub async fn run_command(root: &Path, shell: &Shell, command: &str, timeout_secs: u64, cancel: Arc<Notify>) -> R<String> {
    let root = simplify_path(&root.canonicalize().map_err(|e| e.to_string())?);
    let mut cmd = tokio::process::Command::new(&shell.program);
    cmd.args(crate::shell::args(shell, command));
    #[cfg(windows)]
    {
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        if shell.kind == crate::shell::ShellKind::GitBash {
            // Reste dans le dossier courant et évite les messages de profil.
            cmd.env("CHERE_INVOKING", "1").env("MSYS2_PATH_TYPE", "inherit");
        }
    }
    #[cfg(not(windows))]
    cmd.process_group(0); // pour tuer aussi les sous-processus
    cmd.current_dir(&root)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("GIT_PAGER", "cat")
        .env("PAGER", "cat")
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| format!("Lancement impossible : {e}"))?;
    let pid = child.id();
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let read_out = async move {
        let mut b = Vec::new();
        let _ = stdout.read_to_end(&mut b).await;
        b
    };
    let read_err = async move {
        let mut b = Vec::new();
        let _ = stderr.read_to_end(&mut b).await;
        b
    };
    let run = async {
        let (o, e, status) = tokio::join!(read_out, read_err, child.wait());
        (o, e, status)
    };

    let kill_group = || {
        #[cfg(unix)]
        if let Some(pid) = pid {
            unsafe {
                libc::killpg(pid as i32, libc::SIGKILL);
            }
        }
        // Windows : taskkill /T arrête aussi les processus lancés par la commande.
        #[cfg(windows)]
        if let Some(pid) = pid {
            use std::os::windows::process::CommandExt;
            let _ = std::process::Command::new("taskkill")
                .args(["/T", "/F", "/PID", &pid.to_string()])
                .creation_flags(0x0800_0000)
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status();
        }
    };

    let (o, e, status) = tokio::select! {
        r = run => r,
        _ = tokio::time::sleep(Duration::from_secs(timeout_secs.clamp(1, 3600))) => {
            kill_group();
            return Err(format!("Délai dépassé ({timeout_secs} s) : commande interrompue"));
        }
        _ = cancel.notified() => {
            kill_group();
            return Err("Commande annulée par l'utilisateur".into());
        }
    };
    let status = status.map_err(|e| e.to_string())?;
    Ok(format_output(status.code(), &o, &String::from_utf8_lossy(&e)))
}

/// Résultat d'une commande pour le modèle : code de sortie puis sorties fusionnées (tronquées).
pub fn format_output(code: Option<i32>, stdout: &[u8], stderr: &str) -> String {
    let mut out = String::from_utf8_lossy(stdout).into_owned();
    if !stderr.trim().is_empty() {
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(stderr);
    }
    let out = truncate_middle(out.trim_end(), MAX_OUTPUT_CHARS);
    let code = code.map(|c| c.to_string()).unwrap_or_else(|| "signal".into());
    format!("Code de sortie : {code}\n{}", if out.is_empty() { "(aucune sortie)".into() } else { out })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("md-ws-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(d.join("src")).unwrap();
        fs::write(d.join("src/main.rs"), "fn main() {\n    println!(\"a\");\n}\n").unwrap();
        d
    }

    #[test]
    fn verbatim_prefix_is_removed() {
        assert_eq!(simplify_path(Path::new(r"\\?\C:\Users\a")), PathBuf::from(r"C:\Users\a"));
        assert_eq!(simplify_path(Path::new(r"\\?\UNC\srv\share")), PathBuf::from(r"\\srv\share"));
        assert_eq!(simplify_path(Path::new("/home/a")), PathBuf::from("/home/a"));
        assert_eq!(normalize_workspace("ssh:h1:/srv/app"), "ssh:h1:/srv/app");
        assert_eq!(normalize_workspace("/home/a"), "/home/a");
    }

    #[test]
    fn paths_stay_inside_root() {
        let d = tmp();
        assert!(resolve(&d, "src/main.rs").is_ok());
        assert!(resolve(&d, "nouveau/fichier.txt").is_ok());
        assert!(resolve(&d, "../evil").is_err());
        assert!(resolve(&d, "src/../../evil").is_err());
        #[cfg(unix)]
        assert!(resolve(&d, "/etc/passwd").is_err());
        #[cfg(windows)]
        assert!(resolve(&d, r"C:\Windows\win.ini").is_err());
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", d.join("lien")).unwrap();
            assert!(resolve(&d, "lien/passwd").is_err());
        }
    }

    #[test]
    fn edit_requires_unique_match() {
        let d = tmp();
        let (_, diff) = edit_file(&d, "src/main.rs", "\"a\"", "\"b\"", false).unwrap();
        assert!(diff.contains("+    println!(\"b\");"));
        fs::write(d.join("x.txt"), "a a").unwrap();
        assert!(edit_file(&d, "x.txt", "a", "b", false).is_err());
        assert!(edit_file(&d, "x.txt", "a", "b", true).is_ok());
        assert_eq!(fs::read_to_string(d.join("x.txt")).unwrap(), "b b");
    }

    #[test]
    fn edit_tolerates_crlf() {
        let d = tmp();
        fs::write(d.join("w.txt"), "un\r\ndeux\r\n").unwrap();
        edit_file(&d, "w.txt", "un\ndeux", "1\n2", false).unwrap();
        assert_eq!(fs::read_to_string(d.join("w.txt")).unwrap(), "1\r\n2\r\n");
    }

    #[test]
    fn read_and_search() {
        let d = tmp();
        assert!(read_file(&d, "src/main.rs", None, None).unwrap().contains("     2\t    println!"));
        assert!(search(&d, "println", ".", Some("*.rs")).unwrap().contains("src/main.rs:2:"));
        assert!(list_files(&d, ".", 2).unwrap().contains("src/main.rs"));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn commands_run_in_root_and_time_out() {
        let d = tmp();
        let sh = crate::shell::detect("auto");
        let out = run_command(&d, &sh, "pwd && echo err >&2", 10, Arc::new(Notify::new())).await.unwrap();
        assert!(out.contains("Code de sortie : 0"));
        assert!(out.contains("err"));
        let err = run_command(&d, &sh, "sleep 5", 1, Arc::new(Notify::new())).await.unwrap_err();
        assert!(err.contains("Délai"));
    }

    /// Exécuté par la CI Windows : chaque shell disponible doit lancer une commande
    /// et remonter fidèlement le code de sortie.
    #[cfg(windows)]
    #[tokio::test]
    async fn windows_shells_run_commands() {
        use crate::shell::{detect, ShellKind};
        let d = tmp();
        for pref in ["auto", "pwsh", "powershell"] {
            let sh = detect(pref);
            let ok = run_command(&d, &sh, "echo bonjour", 60, Arc::new(Notify::new())).await.unwrap();
            assert!(ok.contains("Code de sortie : 0") && ok.contains("bonjour"), "{pref} ({:?}) : {ok}", sh.kind);
            let fail = if sh.kind == ShellKind::GitBash { "exit 3" } else { "cmd /c exit 3" };
            let ko = run_command(&d, &sh, fail, 60, Arc::new(Notify::new())).await.unwrap();
            assert!(ko.contains("Code de sortie : 3"), "{pref} ({:?}) : {ko}", sh.kind);
        }
        // Accents et écriture dans le dossier du projet.
        let sh = detect("auto");
        let cmd = if sh.kind == ShellKind::GitBash { "echo 'é' > accent.txt && cat accent.txt" } else { "Set-Content accent.txt 'é'; Get-Content accent.txt" };
        let out = run_command(&d, &sh, cmd, 60, Arc::new(Notify::new())).await.unwrap();
        assert!(out.contains('é'), "{:?} : {out}", sh.kind);
        assert!(d.join("accent.txt").is_file());
    }
}
