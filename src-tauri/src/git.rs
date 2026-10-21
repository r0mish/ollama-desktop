//! Opérations git sur le dossier de projet (local ou distant).
//! Les arguments sont passés sans shell : aucun risque d'injection via un nom de fichier.

use std::time::Duration;

use serde::Serialize;

use crate::target::Target;

type R<T> = Result<T, String>;

const LOCAL_OPS: Duration = Duration::from_secs(30);
const NETWORK_OPS: Duration = Duration::from_secs(180);
const MAX_DIFF_CHARS: usize = 200_000;

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct FileStatus {
    pub path: String,
    pub orig_path: Option<String>,
    /// Statut dans l'index (X) et dans l'arbre de travail (Y), format porcelain.
    pub index: String,
    pub worktree: String,
}

#[derive(Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct GitStatus {
    pub is_repo: bool,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: u32,
    pub behind: u32,
    pub files: Vec<FileStatus>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Commit {
    pub hash: String,
    pub author: String,
    pub date: String,
    pub subject: String,
}

async fn git(t: &Target, args: &[&str], stdin: Option<Vec<u8>>, timeout: Duration) -> R<crate::ssh::Output> {
    let mut full: Vec<String> = ["-c", "color.ui=false", "-c", "core.quotepath=false", "--no-pager"].iter().map(|s| s.to_string()).collect();
    full.extend(args.iter().map(|s| s.to_string()));
    let env = [("GIT_TERMINAL_PROMPT", "0"), ("GIT_OPTIONAL_LOCKS", "0"), ("LC_ALL", "C")];
    t.exec_program("git", &full, stdin, &env, timeout).await
}

/// Exécute git et renvoie stdout, ou l'erreur de git.
async fn run(t: &Target, args: &[&str], stdin: Option<Vec<u8>>, timeout: Duration) -> R<String> {
    let o = git(t, args, stdin, timeout).await?;
    if o.code == Some(0) {
        Ok(o.stdout_text())
    } else {
        let msg = if o.stderr.trim().is_empty() { o.stdout_text() } else { o.stderr.clone() };
        Err(friendly(msg.trim()))
    }
}

fn friendly(msg: &str) -> String {
    let l = msg.to_lowercase();
    if l.contains("terminal prompts disabled") || l.contains("could not read username") || l.contains("authentication failed") {
        format!("Authentification git requise : configurez un gestionnaire d'identifiants ou une clé SSH pour ce dépôt.\n{msg}")
    } else if l.contains("please tell me who you are") || l.contains("author identity unknown") {
        format!("Identité git non configurée : git config --global user.name / user.email.\n{msg}")
    } else {
        msg.to_string()
    }
}

fn parse_branch_line(line: &str, st: &mut GitStatus) {
    let l = line.trim_start_matches("## ");
    if let Some(b) = l.strip_prefix("No commits yet on ").or_else(|| l.strip_prefix("Initial commit on ")) {
        st.branch = Some(b.trim().to_string());
        return;
    }
    let (head, tracking) = match l.split_once(" [") {
        Some((h, t)) => (h, Some(t.trim_end_matches(']'))),
        None => (l, None),
    };
    match head.split_once("...") {
        Some((b, u)) => {
            st.branch = Some(b.to_string());
            st.upstream = Some(u.to_string());
        }
        None => st.branch = Some(head.to_string()),
    }
    if st.branch.as_deref() == Some("HEAD (no branch)") {
        st.branch = Some("HEAD détachée".into());
    }
    for part in tracking.unwrap_or_default().split(", ") {
        if let Some(n) = part.strip_prefix("ahead ") {
            st.ahead = n.parse().unwrap_or(0);
        } else if let Some(n) = part.strip_prefix("behind ") {
            st.behind = n.parse().unwrap_or(0);
        }
    }
}

pub fn parse_status(raw: &str) -> GitStatus {
    let mut st = GitStatus { is_repo: true, ..Default::default() };
    let mut it = raw.split('\0').filter(|s| !s.is_empty());
    while let Some(entry) = it.next() {
        if entry.starts_with("## ") {
            parse_branch_line(entry, &mut st);
            continue;
        }
        if entry.len() < 4 {
            continue;
        }
        let (x, y, path) = (&entry[0..1], &entry[1..2], &entry[3..]);
        let orig_path = if x == "R" || x == "C" { it.next().map(str::to_owned) } else { None };
        st.files.push(FileStatus { path: path.to_string(), orig_path, index: x.into(), worktree: y.into() });
    }
    st
}

pub async fn status(t: &Target) -> R<GitStatus> {
    let o = git(t, &["status", "--porcelain=v1", "-b", "-z", "--untracked-files=all"], None, LOCAL_OPS).await?;
    if o.code != Some(0) {
        if o.stderr.contains("not a git repository") {
            return Ok(GitStatus::default());
        }
        if o.stderr.contains("No such file") || o.stderr.contains("not found") || o.code == Some(127) {
            return Err("git n'est pas installé sur cette machine".into());
        }
        return Err(friendly(o.stderr.trim()));
    }
    Ok(parse_status(&o.stdout_text()))
}

fn cap(mut s: String) -> String {
    if s.len() > MAX_DIFF_CHARS {
        let mut cut = MAX_DIFF_CHARS;
        while !s.is_char_boundary(cut) {
            cut -= 1;
        }
        s.truncate(cut);
        s.push_str("\n[… diff tronqué]");
    }
    s
}

/// Diff d'un fichier (ou de tout le dépôt si `path` est vide). `staged` : contenu de l'index.
pub async fn diff(t: &Target, path: Option<&str>, staged: bool, untracked: bool) -> R<String> {
    if untracked {
        if let Some(p) = path {
            // Fichier non suivi : diff contre un fichier vide (code 1 = différences).
            let o = git(t, &["diff", "--no-index", "--", "/dev/null", p], None, LOCAL_OPS).await?;
            return Ok(cap(o.stdout_text()));
        }
    }
    let mut args = vec!["diff"];
    if staged {
        args.push("--cached");
    }
    if let Some(p) = path {
        args.extend(["--", p]);
    }
    run(t, &args, None, LOCAL_OPS).await.map(cap)
}

fn with_paths<'a>(base: &[&'a str], paths: &'a [String]) -> Vec<&'a str> {
    let mut v = base.to_vec();
    v.push("--");
    v.extend(paths.iter().map(String::as_str));
    v
}

pub async fn stage(t: &Target, paths: &[String]) -> R<()> {
    let args = if paths.is_empty() { vec!["add", "-A"] } else { with_paths(&["add", "-A"], paths) };
    run(t, &args, None, LOCAL_OPS).await.map(|_| ())
}

pub async fn unstage(t: &Target, paths: &[String]) -> R<()> {
    let args = if paths.is_empty() { vec!["reset", "-q"] } else { with_paths(&["reset", "-q"], paths) };
    match run(t, &args, None, LOCAL_OPS).await {
        Ok(_) => Ok(()),
        // Dépôt sans commit : pas de HEAD pour reset.
        Err(_) => {
            let args = if paths.is_empty() { vec!["rm", "-r", "-q", "--cached", "."] } else { with_paths(&["rm", "-r", "-q", "--cached"], paths) };
            run(t, &args, None, LOCAL_OPS).await.map(|_| ())
        }
    }
}

/// Annule les modifications locales : restaure les fichiers suivis, supprime les non suivis.
pub async fn discard(t: &Target, tracked: &[String], untracked: &[String]) -> R<()> {
    if !tracked.is_empty() {
        run(t, &with_paths(&["checkout", "-q"], tracked), None, LOCAL_OPS).await?;
    }
    if !untracked.is_empty() {
        run(t, &with_paths(&["clean", "-f", "-q"], untracked), None, LOCAL_OPS).await?;
    }
    Ok(())
}

pub async fn commit(t: &Target, message: &str) -> R<String> {
    if message.trim().is_empty() {
        return Err("Message de commit vide".into());
    }
    run(t, &["commit", "-q", "-F", "-"], Some(message.as_bytes().to_vec()), LOCAL_OPS).await?;
    run(t, &["log", "-1", "--pretty=format:%h %s"], None, LOCAL_OPS).await
}

pub async fn push(t: &Target) -> R<String> {
    let st = status(t).await?;
    let o = if st.upstream.is_some() {
        git(t, &["push"], None, NETWORK_OPS).await?
    } else {
        git(t, &["push", "-u", "origin", "HEAD"], None, NETWORK_OPS).await?
    };
    if o.code == Some(0) {
        Ok(o.stderr.trim().to_string())
    } else {
        Err(friendly(o.stderr.trim()))
    }
}

pub async fn pull(t: &Target) -> R<String> {
    let o = git(t, &["pull", "--ff-only"], None, NETWORK_OPS).await?;
    if o.code == Some(0) {
        Ok(format!("{}{}", o.stdout_text().trim(), o.stderr.trim()))
    } else {
        Err(friendly(o.stderr.trim()))
    }
}

pub async fn fetch(t: &Target) -> R<()> {
    run(t, &["fetch", "--prune"], None, NETWORK_OPS).await.map(|_| ())
}

pub async fn branches(t: &Target) -> R<Vec<String>> {
    let out = run(t, &["branch", "--format=%(refname:short)"], None, LOCAL_OPS).await?;
    Ok(out.lines().map(str::trim).filter(|l| !l.is_empty()).map(str::to_owned).collect())
}

fn valid_branch(name: &str) -> R<()> {
    if name.is_empty() || name.starts_with('-') || name.chars().any(|c| c.is_whitespace() || "~^:?*[\\".contains(c)) || name.contains("..") {
        Err(format!("Nom de branche invalide : {name}"))
    } else {
        Ok(())
    }
}

pub async fn checkout(t: &Target, branch: &str, create: bool) -> R<()> {
    valid_branch(branch)?;
    let args = if create { vec!["switch", "-c", branch] } else { vec!["switch", branch] };
    run(t, &args, None, LOCAL_OPS).await.map(|_| ())
}

pub async fn log(t: &Target, n: u32) -> R<Vec<Commit>> {
    let count = format!("-n{}", n.clamp(1, 200));
    match run(t, &["log", &count, "--pretty=format:%h%x1f%an%x1f%ar%x1f%s"], None, LOCAL_OPS).await {
        Ok(out) => Ok(out
            .lines()
            .filter_map(|l| {
                let mut p = l.split('\u{1f}');
                Some(Commit { hash: p.next()?.into(), author: p.next()?.into(), date: p.next()?.into(), subject: p.next().unwrap_or("").into() })
            })
            .collect()),
        // Dépôt sans commit.
        Err(e) if e.contains("does not have any commits") => Ok(Vec::new()),
        Err(e) => Err(e),
    }
}

pub async fn init(t: &Target) -> R<()> {
    run(t, &["init", "-q"], None, LOCAL_OPS).await.map(|_| ())
}

/// Résumé texte pour l'agent (outil git_status).
pub fn status_text(st: &GitStatus) -> String {
    if !st.is_repo {
        return "Ce dossier n'est pas un dépôt git.".into();
    }
    let mut out = format!("Branche : {}", st.branch.as_deref().unwrap_or("?"));
    if let Some(u) = &st.upstream {
        out.push_str(&format!(" (suit {u}, en avance de {}, en retard de {})", st.ahead, st.behind));
    }
    out.push('\n');
    if st.files.is_empty() {
        out.push_str("Arbre de travail propre.");
    }
    for f in &st.files {
        let code = format!("{}{}", f.index, f.worktree);
        match &f.orig_path {
            Some(o) => out.push_str(&format!("{code} {o} -> {}\n", f.path)),
            None => out.push_str(&format!("{code} {}\n", f.path)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn porcelain_status_is_parsed() {
        let raw = "## main...origin/main [ahead 2, behind 1]\0 M src/a.rs\0A  b.rs\0R  new.rs\0old.rs\0?? dossier avec espace/f.txt\0";
        let st = parse_status(raw);
        assert_eq!(st.branch.as_deref(), Some("main"));
        assert_eq!(st.upstream.as_deref(), Some("origin/main"));
        assert_eq!((st.ahead, st.behind), (2, 1));
        assert_eq!(st.files.len(), 4);
        assert_eq!(st.files[0].worktree, "M");
        assert_eq!(st.files[2].orig_path.as_deref(), Some("old.rs"));
        assert_eq!(st.files[3].path, "dossier avec espace/f.txt");
        assert_eq!(st.files[3].index, "?");
    }

    #[test]
    fn branch_lines_without_upstream() {
        assert_eq!(parse_status("## No commits yet on main\0").branch.as_deref(), Some("main"));
        assert_eq!(parse_status("## feature\0").upstream, None);
    }

    #[test]
    fn branch_names_are_validated() {
        assert!(valid_branch("feature/ssh").is_ok());
        assert!(valid_branch("-f").is_err());
        assert!(valid_branch("a b").is_err());
        assert!(valid_branch("a..b").is_err());
    }

    #[tokio::test]
    async fn local_repository_round_trip() {
        let d = std::env::temp_dir().join(format!("md-git-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&d).unwrap();
        let t = Target::Local(d.clone());
        assert!(!status(&t).await.unwrap().is_repo);
        init(&t).await.unwrap();
        let cfg = |k: &str, v: &str| std::process::Command::new("git").args(["config", k, v]).current_dir(&d).status().unwrap();
        cfg("user.name", "Test");
        cfg("user.email", "test@example.org");
        // Git pour Windows active autocrlf par défaut : on compare des octets exacts.
        cfg("core.autocrlf", "false");
        std::fs::write(d.join("a.txt"), "un\n").unwrap();
        let st = status(&t).await.unwrap();
        assert_eq!(st.files[0].index, "?");
        assert!(diff(&t, Some("a.txt"), false, true).await.unwrap().contains("+un"));
        stage(&t, &["a.txt".into()]).await.unwrap();
        assert!(diff(&t, None, true, false).await.unwrap().contains("+un"));
        let c = commit(&t, "feat: premier commit").await.unwrap();
        assert!(c.contains("feat: premier commit"));
        assert!(status(&t).await.unwrap().files.is_empty());
        std::fs::write(d.join("a.txt"), "deux\n").unwrap();
        discard(&t, &["a.txt".into()], &[]).await.unwrap();
        assert_eq!(std::fs::read_to_string(d.join("a.txt")).unwrap(), "un\n");
        checkout(&t, "feature/x", true).await.unwrap();
        assert!(branches(&t).await.unwrap().contains(&"feature/x".to_string()));
        assert_eq!(log(&t, 5).await.unwrap().len(), 1);
    }
}
