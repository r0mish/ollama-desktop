//! Tunnels SSH vers un serveur Ollama distant : `ssh -N -L` redirige un port local vers
//! le port d'Ollama sur le serveur (qui peut rester en écoute sur 127.0.0.1, sans être
//! exposé au réseau). Un tunnel par serveur, ouvert à la demande et réutilisé.

use std::collections::HashMap;
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use tokio::sync::Mutex;

use crate::ssh::{self, SshHost};

type R<T> = Result<T, String>;

struct Tunnel {
    child: tokio::process::Child,
    port: u16,
    /// Hôte et port distants : un changement de configuration rouvre le tunnel.
    signature: String,
}

#[derive(Default)]
pub struct Tunnels(Mutex<HashMap<String, Tunnel>>);

fn free_port() -> R<u16> {
    let l = std::net::TcpListener::bind(("127.0.0.1", 0)).map_err(|e| e.to_string())?;
    l.local_addr().map(|a| a.port()).map_err(|e| e.to_string())
}

async fn reachable(port: u16) -> bool {
    tokio::net::TcpStream::connect(("127.0.0.1", port)).await.is_ok()
}

impl Tunnels {
    /// Port local du tunnel vers `remote_port` sur `host`, ouvert si nécessaire.
    pub async fn ensure(&self, server_id: &str, host: &SshHost, remote_port: u16) -> R<u16> {
        let signature = format!("{}|{:?}|{:?}|{:?}|{remote_port}", host.host, host.port, host.user, host.identity_file);
        let mut map = self.0.lock().await;
        if let Some(t) = map.get_mut(server_id) {
            if t.signature == signature && matches!(t.child.try_wait(), Ok(None)) && reachable(t.port).await {
                return Ok(t.port);
            }
            let _ = t.child.start_kill();
            map.remove(server_id);
        }
        let port = free_port()?;
        let mut cmd = ssh::tunnel_command(host, port, remote_port)?;
        cmd.stdin(Stdio::null()).stdout(Stdio::null()).stderr(Stdio::piped());
        let mut child = cmd.spawn().map_err(|e| format!("Impossible de lancer ssh : {e}. Le client OpenSSH est-il installé ?"))?;
        // Prêt quand le port local accepte les connexions ; échec si ssh se termine avant.
        for _ in 0..200 {
            if let Ok(Some(_)) = child.try_wait() {
                let mut err = String::new();
                if let Some(mut e) = child.stderr.take() {
                    let _ = e.read_to_string(&mut err).await;
                }
                let msg = ssh::connection_error(&err).map(|m| ssh::clean_error(&m)).unwrap_or_else(|| err.trim().to_string());
                return Err(format!("Tunnel SSH vers « {} » impossible : {msg}", host.label));
            }
            if reachable(port).await {
                map.insert(server_id.to_string(), Tunnel { child, port, signature });
                return Ok(port);
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
        let _ = child.start_kill();
        Err(format!("Tunnel SSH vers « {} » : délai dépassé", host.label))
    }

    /// Ferme les tunnels des autres serveurs (changement de serveur actif ou de réglages).
    pub async fn close_except(&self, keep: Option<&str>) {
        let mut map = self.0.lock().await;
        let ids: Vec<String> = map.keys().filter(|id| Some(id.as_str()) != keep).cloned().collect();
        for id in ids {
            if let Some(mut t) = map.remove(&id) {
                let _ = t.child.start_kill();
                let _ = t.child.wait().await;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tunnel vers un vrai serveur SSH (variables MD_SSH_TEST_* et MD_TUNNEL_TEST_PORT :
    /// un port en écoute sur le serveur).
    #[tokio::test]
    async fn tunnel_reaches_the_remote_port() {
        let (Ok(host), Ok(remote)) = (std::env::var("MD_SSH_TEST_HOST"), std::env::var("MD_TUNNEL_TEST_PORT")) else { return };
        let h = SshHost {
            id: "t".into(),
            label: "test".into(),
            host,
            port: std::env::var("MD_SSH_TEST_PORT").ok().and_then(|p| p.parse().ok()),
            user: std::env::var("MD_SSH_TEST_USER").ok(),
            identity_file: std::env::var("MD_SSH_TEST_KEY").ok(),
            system: None,
        };
        let tunnels = Tunnels::default();
        let port = tunnels.ensure("srv", &h, remote.parse().unwrap()).await.unwrap();
        assert!(reachable(port).await);
        // Réutilisé tant qu'il est vivant.
        assert_eq!(tunnels.ensure("srv", &h, remote.parse().unwrap()).await.unwrap(), port);
        // Port distant fermé : le tunnel s'ouvre mais rien ne répond derrière ; un port local reste attribué.
        tunnels.close_except(None).await;
        assert!(!reachable(port).await);
    }
}
