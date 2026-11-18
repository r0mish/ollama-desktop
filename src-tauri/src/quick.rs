//! Saisie rapide : un raccourci global ouvre une mini-fenêtre ; la question saisie
//! part dans une nouvelle conversation de la fenêtre principale.
//!
//! Sous Wayland, une application ne peut pas capter de raccourci global : on associe alors
//! un raccourci du bureau à `ollama-desktop --quick`, qui prévient l'instance déjà lancée
//! par un socket Unix privé (`$XDG_RUNTIME_DIR`, accessible au seul utilisateur).

use std::sync::Mutex;
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter, Manager};
use tauri_plugin_global_shortcut::{GlobalShortcutExt, ShortcutState};

const LABEL: &str = "quick";
pub const FLAG: &str = "--quick";

/// Instant du dernier affichage : une perte de focus signalée juste après est un artefact
/// de l'apparition de la fenêtre, pas un clic ailleurs.
static SHOWN_AT: Mutex<Option<Instant>> = Mutex::new(None);
/// Instant du dernier masquage par perte de focus : la prise du raccourci global par le système
/// retire brièvement le focus, ce qui masque la fenêtre juste avant que le raccourci n'arrive.
static BLUR_HIDDEN_AT: Mutex<Option<Instant>> = Mutex::new(None);

fn recent(at: &Mutex<Option<Instant>>, ms: u64) -> bool {
    at.lock().unwrap().is_some_and(|t| t.elapsed() < Duration::from_millis(ms))
}

fn mark_shown() {
    *SHOWN_AT.lock().unwrap() = Some(Instant::now());
}

/// Affiche la mini-fenêtre (créée au premier usage), ou la masque si elle est visible.
pub fn toggle(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        if w.is_visible().unwrap_or(false) {
            let _ = w.hide();
        } else if recent(&BLUR_HIDDEN_AT, 400) {
            // Elle était visible à l'appui du raccourci : la bascule demandée est ce masquage.
        } else {
            mark_shown();
            let _ = w.center();
            let _ = w.show();
            let _ = w.set_focus();
        }
        return;
    }
    mark_shown();
    let built = tauri::WebviewWindowBuilder::new(app, LABEL, tauri::WebviewUrl::App("quick.html".into()))
        .title("Ollama Desktop : saisie rapide")
        .inner_size(640.0, 60.0)
        // Taille figée par min = max : `resizable(false)` ramènerait la fenêtre GTK à sa taille par défaut.
        .min_inner_size(640.0, 60.0)
        .max_inner_size(640.0, 60.0)
        .decorations(false)
        .always_on_top(true)
        .skip_taskbar(true)
        .center()
        .focused(true)
        .build();
    if let Err(e) = built {
        eprintln!("saisie rapide : {e}");
    }
}

/// La création d'une fenêtre ne doit pas se faire depuis la boucle d'événements (blocage sous Windows).
fn toggle_later(app: &AppHandle) {
    let app = app.clone();
    std::thread::spawn(move || toggle(&app));
}

fn wayland() -> bool {
    cfg!(target_os = "linux") && (std::env::var_os("WAYLAND_DISPLAY").is_some() || std::env::var("XDG_SESSION_TYPE").is_ok_and(|v| v == "wayland"))
}

/// (Ré)enregistre le raccourci global ; chaîne vide = aucun.
pub fn apply_shortcut(app: &AppHandle, shortcut: &str) -> Result<(), String> {
    let gs = app.global_shortcut();
    gs.unregister_all().map_err(|e| e.to_string())?;
    let shortcut = shortcut.trim();
    if shortcut.is_empty() {
        return Ok(());
    }
    if wayland() {
        return Err(format!(
            "Sous Wayland, une application ne peut pas capter de raccourci global. Dans les réglages clavier de votre bureau, associez un raccourci à la commande « ollama-desktop {FLAG} »."
        ));
    }
    gs.on_shortcut(shortcut, |app, _, event| {
        if event.state() == ShortcutState::Pressed {
            toggle_later(app);
        }
    })
    .map_err(|e| format!("Raccourci global « {shortcut} » indisponible (sans doute déjà pris par une autre application) : {e}"))
}

/// Envoie la question à la fenêtre principale, ramenée au premier plan.
pub fn submit(app: &AppHandle, text: &str) -> Result<(), String> {
    hide(app);
    let main = app.get_webview_window("main").ok_or("fenêtre principale fermée")?;
    let _ = main.unminimize();
    let _ = main.show();
    let _ = main.set_focus();
    main.emit("quick-prompt", text).map_err(|e| e.to_string())
}

/// La mini-fenêtre a perdu le focus : on la masque, sauf si elle vient d'apparaître.
pub fn blurred(app: &AppHandle) {
    if !recent(&SHOWN_AT, 500) {
        *BLUR_HIDDEN_AT.lock().unwrap() = Some(Instant::now());
        hide(app);
    }
}

pub fn hide(app: &AppHandle) {
    if let Some(w) = app.get_webview_window(LABEL) {
        let _ = w.hide();
    }
}

#[cfg(target_os = "linux")]
fn socket_path() -> Option<std::path::PathBuf> {
    Some(std::path::PathBuf::from(std::env::var_os("XDG_RUNTIME_DIR")?).join("ollama-desktop.sock"))
}

/// Lancé avec `--quick` alors qu'une instance tourne : la prévient et renvoie true (rien d'autre à faire).
pub fn signal_running_instance() -> bool {
    #[cfg(target_os = "linux")]
    if std::env::args().any(|a| a == FLAG) {
        use std::io::Write;
        if let Some(mut s) = socket_path().and_then(|p| std::os::unix::net::UnixStream::connect(p).ok()) {
            return s.write_all(b"quick\n").is_ok();
        }
    }
    false
}

/// Écoute les demandes `--quick` des lancements suivants (Linux). Sans effet si une autre
/// instance écoute déjà ; `--quick` au premier lancement ouvre directement la mini-fenêtre.
pub fn listen(app: &AppHandle) {
    #[cfg(target_os = "linux")]
    {
        use tokio::io::AsyncReadExt;
        if let Some(path) = socket_path() {
            if std::os::unix::net::UnixStream::connect(&path).is_err() {
                let _ = std::fs::remove_file(&path);
                let app = app.clone();
                tauri::async_runtime::spawn(async move {
                    let Ok(listener) = tokio::net::UnixListener::bind(&path) else { return };
                    while let Ok((mut conn, _)) = listener.accept().await {
                        let mut buf = [0u8; 16];
                        let n = tokio::time::timeout(std::time::Duration::from_secs(1), conn.read(&mut buf)).await.ok().and_then(Result::ok).unwrap_or(0);
                        if buf[..n].starts_with(b"quick") {
                            toggle(&app);
                        }
                    }
                });
            }
        }
    }
    if std::env::args().any(|a| a == FLAG) {
        toggle_later(app);
    }
}

/// Supprime le socket à la fermeture.
pub fn cleanup() {
    #[cfg(target_os = "linux")]
    if let Some(p) = socket_path() {
        let _ = std::fs::remove_file(p);
    }
}
