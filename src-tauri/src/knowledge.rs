//! Fichiers de référence des projets : joints en entier s'ils sont petits, sinon seuls les extraits
//! les plus pertinents pour la question sont envoyés (recherche BM25 locale, rien ne quitte la machine).

use serde::Serialize;

use crate::storage::Project;
use crate::textsearch::{bm25_rank, chunk};

/// En dessous, les fichiers sont joints en entier à chaque message (≈ 17 k tokens).
pub const FULL_CONTEXT_CHARS: usize = 60_000;
/// Taille des extraits joints automatiquement à chaque message.
const EXCERPT_BUDGET: usize = 14_000;
/// Taille des extraits renvoyés par l'outil de recherche.
pub const TOOL_BUDGET: usize = 10_000;
const CHUNK_SIZE: usize = 1_500;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectContext {
    /// « full » (tout est joint) ou « excerpts » (extraits pertinents).
    pub mode: &'static str,
    pub text: String,
}

pub fn total_chars(p: &Project) -> usize {
    p.files.iter().map(|f| f.text.chars().count()).sum()
}

fn kchars(n: usize) -> String {
    if n < 1000 { format!("{n} car.") } else { format!("{} k car.", n / 1000) }
}

/// Extraits les plus pertinents pour `query`, dans la limite de `budget` caractères.
pub fn search(p: &Project, query: &str, budget: usize) -> String {
    let chunks: Vec<_> = p.files.iter().flat_map(|f| chunk(&f.name, &f.text, CHUNK_SIZE)).collect();
    let mut used = 0;
    let mut out = Vec::new();
    for (i, _) in bm25_rank(&chunks, query, 40) {
        let c = &chunks[i];
        let len = c.text.chars().count();
        if used + len > budget && !out.is_empty() {
            break;
        }
        used += len;
        out.push(format!("<extrait fichier=\"{}\">\n{}\n</extrait>", c.source, c.text.trim()));
    }
    out.join("\n\n")
}

/// Contenu des fichiers de référence à joindre pour la question `query`.
pub fn context(p: &Project, query: &str) -> ProjectContext {
    if total_chars(p) <= FULL_CONTEXT_CHARS {
        let text = p.files.iter().map(|f| format!("<fichier nom=\"{}\">\n{}\n</fichier>", f.name, f.text)).collect::<Vec<_>>().join("\n\n");
        return ProjectContext { mode: "full", text };
    }
    let list = p.files.iter().map(|f| format!("- {} ({})", f.name, kchars(f.text.chars().count()))).collect::<Vec<_>>().join("\n");
    let found = search(p, query, EXCERPT_BUDGET);
    let text = format!(
        "Les fichiers de référence sont trop volumineux pour être joints en entier :\n{list}\n\n{}\
         Utilise l'outil search_project_files pour chercher d'autres passages (plusieurs requêtes si besoin) avant de répondre.",
        if found.is_empty() {
            "Aucun extrait ne correspond directement à la dernière question.\n\n".to_string()
        } else {
            format!("Extraits les plus pertinents pour la dernière question :\n\n{found}\n\n")
        }
    );
    ProjectContext { mode: "excerpts", text }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn project(files: Vec<(&str, String)>) -> Project {
        serde_json::from_value(serde_json::json!({
            "id": "p", "name": "P", "instructions": "", "createdAt": 0, "updatedAt": 0,
            "files": files.into_iter().map(|(n, t)| serde_json::json!({"name": n, "text": t})).collect::<Vec<_>>(),
        }))
        .unwrap()
    }

    #[test]
    fn small_projects_are_joined_whole_and_large_ones_searched() {
        let small = project(vec![("a.md", "Le mot de passe Wi-Fi invité change chaque mois.".into())]);
        let c = context(&small, "wifi");
        assert_eq!(c.mode, "full");
        assert!(c.text.contains("<fichier nom=\"a.md\">"));

        let filler = "Paragraphe sans rapport avec la question, sur la météo et le jardinage.\n".repeat(1200);
        let large = project(vec![
            ("bruit.md", filler.clone()),
            ("infra.md", format!("{filler}\nLe serveur de sauvegarde pbs1 a l'adresse 10.0.0.42.\n{filler}")),
        ]);
        assert!(total_chars(&large) > FULL_CONTEXT_CHARS);
        let c = context(&large, "Quelle est l'adresse du serveur de sauvegarde ?");
        assert_eq!(c.mode, "excerpts");
        assert!(c.text.contains("10.0.0.42"), "{}", &c.text[..500.min(c.text.len())]);
        assert!(c.text.chars().count() < EXCERPT_BUDGET + 2_000);
        assert!(c.text.contains("search_project_files"));
    }
}
