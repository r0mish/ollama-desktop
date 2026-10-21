//! Outils web : recherche via SearXNG (auto-hébergeable) et lecture de pages.

use std::net::IpAddr;
use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

const MAX_PAGE_BYTES: usize = 3 * 1024 * 1024;
const MAX_REDIRECTS: usize = 5;

fn client() -> Result<reqwest::Client, String> {
    reqwest::Client::builder()
        // UA générique : on ne transmet rien d'identifiant aux sites visités.
        .user_agent("Mozilla/5.0 (compatible; OllamaDesktop)")
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())
}

pub async fn searxng(instance: &str, query: &str) -> Result<String, String> {
    let instance = instance.trim().trim_end_matches('/');
    if instance.is_empty() {
        return Err("Aucune instance SearXNG configurée (Réglages → Outils).".into());
    }
    let resp = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(10))
        .timeout(Duration::from_secs(30))
        .build()
        .map_err(|e| e.to_string())?
        .get(format!("{instance}/search"))
        .query(&[("q", query), ("format", "json")])
        .send()
        .await
        .map_err(|e| format!("SearXNG injoignable : {e}"))?;
    if !resp.status().is_success() {
        return Err(format!(
            "SearXNG a répondu {} (le format JSON doit être activé : search.formats dans settings.yml)",
            resp.status()
        ));
    }
    let v: Value = resp.json().await.map_err(|e| format!("Réponse SearXNG invalide : {e}"))?;
    let mut out = String::new();
    for (i, r) in v["results"].as_array().into_iter().flatten().take(8).enumerate() {
        out.push_str(&format!(
            "[{}] {}\n{}\n{}\n\n",
            i + 1,
            r["title"].as_str().unwrap_or_default(),
            r["url"].as_str().unwrap_or_default(),
            r["content"].as_str().unwrap_or_default().trim()
        ));
    }
    if out.is_empty() {
        return Err("Aucun résultat".into());
    }
    Ok(out)
}

pub(crate) fn is_private(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            v4.is_private()
                || v4.is_loopback()
                || v4.is_link_local()
                || v4.is_unspecified()
                || v4.is_broadcast()
                || v4.octets()[0] == 100 && (64..128).contains(&v4.octets()[1]) // CGNAT
        }
        IpAddr::V6(v6) => {
            let seg = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || (seg & 0xfe00) == 0xfc00 // ULA
                || (seg & 0xffc0) == 0xfe80 // lien local
                || v6.to_ipv4_mapped().is_some_and(|v4| is_private(IpAddr::V4(v4)))
        }
    }
}

/// Le modèle ne doit pas pouvoir sonder le réseau local (routeur, NAS, services internes…).
async fn check_public(url: &reqwest::Url) -> Result<(), String> {
    if !matches!(url.scheme(), "http" | "https") {
        return Err("Seules les URL http(s) sont autorisées".into());
    }
    let host = url.host_str().ok_or("URL sans hôte")?;
    let port = url.port_or_known_default().unwrap_or(443);
    let addrs = tokio::net::lookup_host((host.trim_matches(|c| c == '[' || c == ']'), port))
        .await
        .map_err(|e| format!("Résolution DNS impossible pour {host} : {e}"))?;
    for a in addrs {
        if is_private(a.ip()) {
            return Err(format!("Accès refusé : {host} pointe vers une adresse privée ou locale"));
        }
    }
    Ok(())
}

pub async fn fetch_url(url: &str, max_chars: usize) -> Result<String, String> {
    let client = client()?;
    let mut url = reqwest::Url::parse(url).map_err(|e| format!("URL invalide : {e}"))?;
    let mut resp = None;
    for _ in 0..=MAX_REDIRECTS {
        check_public(&url).await?;
        let r = client.get(url.clone()).send().await.map_err(|e| format!("Téléchargement impossible : {e}"))?;
        if r.status().is_redirection() {
            let loc = r
                .headers()
                .get(reqwest::header::LOCATION)
                .and_then(|l| l.to_str().ok())
                .ok_or("Redirection sans destination")?;
            url = url.join(loc).map_err(|e| e.to_string())?;
            continue;
        }
        resp = Some(r);
        break;
    }
    let resp = resp.ok_or("Trop de redirections")?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("{url} a répondu {status}"));
    }
    let ctype = resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_ascii_lowercase();

    let mut body = Vec::new();
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| e.to_string())?;
        body.extend_from_slice(&chunk);
        if body.len() > MAX_PAGE_BYTES {
            body.truncate(MAX_PAGE_BYTES);
            break;
        }
    }

    let text = if ctype.contains("html") || ctype.is_empty() && body.starts_with(b"<") {
        html2text::from_read(&body[..], 100).map_err(|e| e.to_string())?
    } else if ctype.starts_with("text/") || ctype.contains("json") || ctype.contains("xml") {
        String::from_utf8_lossy(&body).into_owned()
    } else {
        return Err(format!("Type de contenu non pris en charge : {ctype}"));
    };
    let text = collapse_blank_lines(&text);
    let total = text.chars().count();
    let mut out: String = text.chars().take(max_chars).collect();
    if total > max_chars {
        out.push_str(&format!("\n\n[… tronqué : {max_chars} caractères affichés sur {total}]"));
    }
    Ok(format!("URL : {url}\n\n{out}"))
}

fn collapse_blank_lines(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut blank = 0;
    for line in s.lines() {
        if line.trim().is_empty() {
            blank += 1;
            if blank > 1 {
                continue;
            }
        } else {
            blank = 0;
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_ranges_are_blocked() {
        for ip in ["127.0.0.1", "10.1.2.3", "192.168.1.1", "172.16.0.1", "169.254.1.1", "100.64.0.1", "::1", "fd00::1", "fe80::1", "::ffff:192.168.0.1"] {
            assert!(is_private(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "51.15.0.1", "2a01:4f8::1"] {
            assert!(!is_private(ip.parse().unwrap()), "{ip}");
        }
    }

    #[tokio::test]
    async fn localhost_urls_are_refused() {
        let err = fetch_url("http://localhost:8080/admin", 1000).await.unwrap_err();
        assert!(err.contains("privée"), "{err}");
    }
}
