//! Choix du shell utilisé par l'outil `run_command`.
//!
//! Les modèles écrivent spontanément du bash. Sous Windows, on privilégie donc
//! Git Bash quand il est installé (comme Claude Code), puis PowerShell 7 (qui
//! accepte `&&`), et en dernier recours Windows PowerShell 5.1.

use std::path::{Path, PathBuf};

use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum ShellKind {
    /// bash (ou sh) natif sous macOS / Linux.
    Posix,
    /// bash de Git for Windows (MSYS2).
    GitBash,
    /// PowerShell 7+ (`pwsh.exe`).
    Pwsh,
    /// Windows PowerShell 5.1 (`powershell.exe`).
    WindowsPowerShell,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Shell {
    pub kind: ShellKind,
    pub program: PathBuf,
    /// Libellé affiché et transmis au modèle.
    pub name: String,
}

/// Cherche un exécutable dans le PATH.
fn which(exe: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).map(|d| d.join(exe)).find(|p| p.is_file())
}

/// Emplacements de `bash.exe` de Git for Windows. Le `bash.exe` de System32 (WSL)
/// est volontairement ignoré : il s'exécute dans une autre machine.
fn git_bash_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    if let Some(p) = std::env::var_os("OLLAMA_DESKTOP_GIT_BASH_PATH") {
        out.push(PathBuf::from(p));
    }
    // À partir de git.exe dans le PATH : <Git>\cmd\git.exe → <Git>\bin\bash.exe
    if let Some(git) = which("git.exe") {
        if let Some(root) = git.parent().and_then(Path::parent) {
            out.push(root.join("bin").join("bash.exe"));
            out.push(root.join("usr").join("bin").join("bash.exe"));
        }
    }
    for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
        if let Some(dir) = std::env::var_os(var) {
            out.push(PathBuf::from(dir).join("Git").join("bin").join("bash.exe"));
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        out.push(PathBuf::from(local).join("Programs").join("Git").join("bin").join("bash.exe"));
    }
    out
}

fn is_wsl_launcher(p: &Path) -> bool {
    let s = p.to_string_lossy().to_ascii_lowercase();
    s.contains("\\windows\\system32\\") || s.contains("\\windowsapps\\")
}

fn find_git_bash() -> Option<PathBuf> {
    git_bash_candidates().into_iter().find(|p| p.is_file() && !is_wsl_launcher(p))
}

fn find_pwsh() -> Option<PathBuf> {
    which("pwsh.exe").or_else(|| {
        ["ProgramFiles", "ProgramW6432"].iter().find_map(|v| {
            let p = PathBuf::from(std::env::var_os(v)?).join("PowerShell").join("7").join("pwsh.exe");
            p.is_file().then_some(p)
        })
    })
}

fn windows_powershell() -> PathBuf {
    std::env::var_os("SystemRoot")
        .map(|r| PathBuf::from(r).join("System32").join("WindowsPowerShell").join("v1.0").join("powershell.exe"))
        .filter(|p| p.is_file())
        .unwrap_or_else(|| PathBuf::from("powershell.exe"))
}

fn make(kind: ShellKind, program: PathBuf) -> Shell {
    let name = match kind {
        ShellKind::Posix => program.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "sh".into()),
        ShellKind::GitBash => "bash (Git Bash pour Windows)".into(),
        ShellKind::Pwsh => "PowerShell 7 (pwsh)".into(),
        ShellKind::WindowsPowerShell => "Windows PowerShell 5.1".into(),
    };
    Shell { kind, program, name }
}

/// `preference` : « auto », « bash », « pwsh » ou « powershell » (réglage utilisateur).
pub fn detect(preference: &str) -> Shell {
    if !cfg!(windows) {
        let bash = ["/bin/bash", "/usr/bin/bash", "/usr/local/bin/bash", "/opt/homebrew/bin/bash"]
            .iter()
            .map(PathBuf::from)
            .find(|p| p.is_file());
        return make(ShellKind::Posix, bash.unwrap_or_else(|| PathBuf::from("/bin/sh")));
    }
    let git_bash = || find_git_bash().map(|p| make(ShellKind::GitBash, p));
    let pwsh = || find_pwsh().map(|p| make(ShellKind::Pwsh, p));
    let ps51 = || make(ShellKind::WindowsPowerShell, windows_powershell());
    match preference {
        "bash" => git_bash().or_else(pwsh).unwrap_or_else(ps51),
        "pwsh" => pwsh().unwrap_or_else(ps51),
        "powershell" => ps51(),
        _ => git_bash().or_else(pwsh).unwrap_or_else(ps51),
    }
}

/// Arguments pour exécuter `command` avec ce shell.
pub fn args(shell: &Shell, command: &str) -> Vec<String> {
    match shell.kind {
        ShellKind::Posix | ShellKind::GitBash => vec!["-c".into(), command.into()],
        ShellKind::Pwsh | ShellKind::WindowsPowerShell => {
            // Sortie en UTF-8, pas de barre de progression, et code de sortie fidèle :
            // celui du dernier programme natif, ou 1 si la dernière commande a échoué.
            let script = format!(
                "[Console]::OutputEncoding = [System.Text.Encoding]::UTF8\n\
                 $OutputEncoding = [System.Text.Encoding]::UTF8\n\
                 $ProgressPreference = 'SilentlyContinue'\n\
                 {command}\n\
                 if (-not $?) {{ if ($LASTEXITCODE) {{ exit $LASTEXITCODE }} else {{ exit 1 }} }} elseif ($LASTEXITCODE) {{ exit $LASTEXITCODE }}"
            );
            vec!["-NoLogo".into(), "-NoProfile".into(), "-NonInteractive".into(), "-ExecutionPolicy".into(), "Bypass".into(), "-Command".into(), script]
        }
    }
}

/// Consignes données au modèle pour écrire des commandes valides dans ce shell.
pub fn guidance(shell: &Shell) -> &'static str {
    match shell.kind {
        ShellKind::Posix => "Commands run with bash/sh: use POSIX shell syntax.",
        ShellKind::GitBash => {
            "Commands run in Git Bash on Windows: use bash syntax (&&, ls, cat, grep, rm -rf, export…). \
             Windows programs (npm, node, python, cargo, dotnet, git…) are on the PATH. \
             Use forward slashes; a Windows path C:\\dir\\file is /c/dir/file in bash."
        }
        ShellKind::Pwsh => {
            "Commands run in PowerShell 7 on Windows, NOT bash. && and || work, but use PowerShell commands: \
             Get-ChildItem (not ls -la), Get-Content (not cat/head/tail: use -TotalCount / -Tail), \
             Select-String (not grep), Remove-Item -Recurse -Force (not rm -rf), New-Item -ItemType Directory -Force (not mkdir -p), \
             Copy-Item / Move-Item, $env:NAME = 'value' (not export), Test-Path, `where.exe` (not which). \
             Prefer the list_files, read_file and search_files tools over shell commands to inspect files."
        }
        ShellKind::WindowsPowerShell => {
            "Commands run in Windows PowerShell 5.1, NOT bash. && and || DO NOT exist: chain with `cmd1; if ($?) { cmd2 }`. \
             Use PowerShell commands: Get-ChildItem (not ls -la), Get-Content (not cat/head/tail: use -TotalCount / -Tail), \
             Select-String (not grep), Remove-Item -Recurse -Force (not rm -rf), New-Item -ItemType Directory -Force (not mkdir -p), \
             Copy-Item / Move-Item, $env:NAME = 'value' (not export), Test-Path, `where.exe` (not which). \
             Prefer the list_files, read_file and search_files tools over shell commands to inspect files."
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wsl_launcher_is_never_used_as_git_bash() {
        assert!(is_wsl_launcher(Path::new(r"C:\Windows\System32\bash.exe")));
        assert!(is_wsl_launcher(Path::new(r"C:\Users\x\AppData\Local\Microsoft\WindowsApps\bash.exe")));
        assert!(!is_wsl_launcher(Path::new(r"C:\Program Files\Git\bin\bash.exe")));
    }

    #[test]
    fn powershell_script_reports_exit_codes() {
        let s = make(ShellKind::Pwsh, PathBuf::from("pwsh.exe"));
        let a = args(&s, "npm test");
        assert_eq!(a[a.len() - 2], "-Command");
        let script = &a[a.len() - 1];
        assert!(script.contains("npm test"));
        assert!(script.contains("exit $LASTEXITCODE"));
        assert!(script.contains("UTF8"));
    }

    #[test]
    fn detection_returns_a_runnable_shell() {
        let s = detect("auto");
        if cfg!(windows) {
            assert_ne!(s.kind, ShellKind::Posix);
        } else {
            assert_eq!(s.kind, ShellKind::Posix);
            assert!(s.program.is_file());
        }
    }
}
