//! Détection des nouvelles versions publiées sur GitHub (aucune installation
//! automatique : l'utilisateur télécharge l'installateur proposé).

use std::time::Duration;

use serde::Serialize;
use serde_json::Value;

const REPO: &str = "r0mish/ollama-desktop";

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateInfo {
    pub current: String,
    pub latest: String,
    pub available: bool,
    pub release_url: String,
    /// Installateur adapté à ce système, s'il est présent dans la release.
    pub asset_name: Option<String>,
    pub asset_url: Option<String>,
    pub notes: String,
}

/// « v1.2.3 » → (1, 2, 3) ; les suffixes (-beta…) sont ignorés.
fn parse_version(v: &str) -> (u64, u64, u64) {
    let mut it = v.trim().trim_start_matches('v').split(['.', '-', '+']).map(|p| p.parse::<u64>().unwrap_or(0));
    (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Motifs de nom d'installateur par ordre de préférence pour ce système.
fn preferred_assets() -> Vec<&'static str> {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => vec!["_aarch64.dmg"],
        ("macos", _) => vec!["_x64.dmg"],
        ("windows", _) => vec!["-setup.exe", ".msi"],
        _ => {
            if std::env::var_os("APPIMAGE").is_some() {
                vec![".AppImage"]
            } else if std::path::Path::new("/etc/debian_version").exists() {
                vec![".deb", ".AppImage"]
            } else if std::path::Path::new("/etc/redhat-release").exists() || std::path::Path::new("/etc/fedora-release").exists() {
                vec![".rpm", ".AppImage"]
            } else {
                vec![".AppImage"]
            }
        }
    }
}

pub async fn check() -> Result<UpdateInfo, String> {
    let current = env!("CARGO_PKG_VERSION").to_string();
    let resp = reqwest::Client::builder()
        .user_agent(concat!("ollama-desktop/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(20))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .map_err(|e| format!("GitHub injoignable : {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("GitHub a répondu {}", resp.status()));
    }
    let v: Value = resp.json().await.map_err(|e| e.to_string())?;
    let latest = v["tag_name"].as_str().unwrap_or_default().trim_start_matches('v').to_string();
    let assets: Vec<(String, String)> = v["assets"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|a| Some((a["name"].as_str()?.to_owned(), a["browser_download_url"].as_str()?.to_owned())))
        .collect();
    let asset = preferred_assets()
        .into_iter()
        .find_map(|suffix| assets.iter().find(|(n, _)| n.ends_with(suffix)).cloned());
    Ok(UpdateInfo {
        available: !latest.is_empty() && parse_version(&latest) > parse_version(&current),
        current,
        latest,
        release_url: v["html_url"].as_str().unwrap_or_default().to_owned(),
        asset_name: asset.as_ref().map(|a| a.0.clone()),
        asset_url: asset.map(|a| a.1),
        notes: v["body"].as_str().unwrap_or_default().to_owned(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(parse_version("v0.10.0") > parse_version("0.9.9"));
        assert!(parse_version("1.0.0") > parse_version("0.99.0"));
        assert_eq!(parse_version("v1.2.3-beta"), (1, 2, 3));
        assert!(parse_version("0.2.0") <= parse_version("0.2.0"));
    }
}
