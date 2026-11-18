// Pas de console supplémentaire sous Windows en release.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

fn main() {
    // ssh relance l'application comme programme SSH_ASKPASS (installation d'une clé par mot de passe).
    if let Some(code) = ollama_desktop_lib::askpass() {
        std::process::exit(code);
    }
    ollama_desktop_lib::run()
}
