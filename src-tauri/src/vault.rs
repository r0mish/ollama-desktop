//! Chiffrement des données locales (conversations, projets, configuration MCP) :
//! XChaCha20-Poly1305 avec une clé de 256 bits gardée dans le trousseau du système.
//! Les fichiers non chiffrés restent lisibles (migration transparente).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;

use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};

type R<T> = Result<T, String>;

const MAGIC: &[u8] = b"MDENC1\n";
const NONCE_LEN: usize = 24;
const KEYRING_SERVICE: &str = "dev.r0mish.ollama-desktop";
const KEYRING_USER: &str = "data-key";

/// Chiffrement actif pour les écritures (réglage + clé disponible).
static ENABLED: AtomicBool = AtomicBool::new(false);
/// Clé en mémoire, pour ne pas solliciter le trousseau à chaque écriture.
static KEY: Mutex<Option<[u8; 32]>> = Mutex::new(None);

pub fn is_sealed(data: &[u8]) -> bool {
    data.starts_with(MAGIC)
}

pub fn seal(plain: &[u8], key: &[u8; 32]) -> R<Vec<u8>> {
    let cipher = XChaCha20Poly1305::new(Key::from_slice(key));
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = cipher.encrypt(&nonce, plain).map_err(|_| "Chiffrement impossible".to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

pub fn open(data: &[u8], key: &[u8; 32]) -> R<Vec<u8>> {
    let body = data.strip_prefix(MAGIC).ok_or("Fichier non chiffré")?;
    if body.len() < NONCE_LEN {
        return Err("Fichier chiffré tronqué".into());
    }
    let (nonce, ct) = body.split_at(NONCE_LEN);
    XChaCha20Poly1305::new(Key::from_slice(key))
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| "Déchiffrement impossible : clé incorrecte ou fichier altéré".into())
}

fn entry() -> R<keyring::Entry> {
    keyring::Entry::new(KEYRING_SERVICE, KEYRING_USER).map_err(|e| e.to_string())
}

fn decode_key(b64: &str) -> R<[u8; 32]> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64.trim())
        .map_err(|_| "Clé invalide (base64 attendu)".to_string())?;
    bytes.try_into().map_err(|_| "Clé invalide (256 bits attendus)".into())
}

fn encode_key(key: &[u8; 32]) -> String {
    base64::engine::general_purpose::STANDARD.encode(key)
}

/// Clé du trousseau ; créée si absente et `create`.
pub fn key(create: bool) -> R<Option<[u8; 32]>> {
    if let Some(k) = *KEY.lock().unwrap() {
        return Ok(Some(k));
    }
    let e = entry()?;
    let k = match e.get_password() {
        Ok(s) => decode_key(&s)?,
        Err(keyring::Error::NoEntry) if create => {
            let k: [u8; 32] = XChaCha20Poly1305::generate_key(&mut OsRng).into();
            e.set_password(&encode_key(&k)).map_err(|e| format!("Trousseau du système indisponible : {e}"))?;
            k
        }
        Err(keyring::Error::NoEntry) => return Ok(None),
        Err(err) => return Err(format!("Trousseau du système indisponible : {err}")),
    };
    *KEY.lock().unwrap() = Some(k);
    Ok(Some(k))
}

/// Active ou non le chiffrement des écritures. Renvoie une erreur si la clé ne peut pas être obtenue.
pub fn set_enabled(on: bool) -> R<()> {
    if on {
        key(true)?;
    }
    ENABLED.store(on, Ordering::SeqCst);
    Ok(())
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::SeqCst)
}

/// Dernière erreur d'activation (trousseau indisponible…), affichée dans les réglages.
pub static LAST_ERROR: Mutex<Option<String>> = Mutex::new(None);

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Status {
    pub enabled: bool,
    pub has_key: bool,
    pub error: Option<String>,
}

pub fn status() -> Status {
    Status {
        enabled: enabled(),
        has_key: matches!(key(false), Ok(Some(_))),
        error: LAST_ERROR.lock().unwrap().clone(),
    }
}

/// Clé encodée, pour la sauvegarde de récupération.
pub fn export_key() -> R<String> {
    key(false)?.map(|k| encode_key(&k)).ok_or_else(|| "Aucune clé de chiffrement".into())
}

/// Restaure une clé sauvegardée (nouvel ordinateur, trousseau réinitialisé…).
pub fn import_key(b64: &str) -> R<()> {
    let k = decode_key(b64)?;
    entry()?.set_password(&encode_key(&k)).map_err(|e| format!("Trousseau du système indisponible : {e}"))?;
    *KEY.lock().unwrap() = Some(k);
    Ok(())
}

/// Contenu en clair d'un fichier lu sur disque (chiffré ou non).
pub fn decode(data: Vec<u8>) -> R<Vec<u8>> {
    if !is_sealed(&data) {
        return Ok(data);
    }
    let k = key(false)?.ok_or("Données chiffrées, mais la clé est absente du trousseau : restaurez votre clé de récupération dans les réglages")?;
    open(&data, &k)
}

/// Contenu à écrire sur disque (chiffré si actif).
pub fn encode(plain: &[u8]) -> R<Vec<u8>> {
    if !enabled() {
        return Ok(plain.to_vec());
    }
    let k = key(true)?.ok_or("Clé de chiffrement indisponible")?;
    let sealed = seal(plain, &k)?;
    // Vérification avant écriture : on ne remplace jamais un fichier par un contenu illisible.
    if open(&sealed, &k)? != plain {
        return Err("Vérification du chiffrement échouée".into());
    }
    Ok(sealed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_and_tamper() {
        let k = [7u8; 32];
        let sealed = seal(b"{\"title\":\"secret\"}", &k).unwrap();
        assert!(is_sealed(&sealed));
        assert!(!sealed.windows(6).any(|w| w == b"secret"));
        assert_eq!(open(&sealed, &k).unwrap(), b"{\"title\":\"secret\"}");
        // Deux chiffrements du même contenu diffèrent (nonce aléatoire).
        assert_ne!(seal(b"x", &k).unwrap(), seal(b"x", &k).unwrap());
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(open(&bad, &k).is_err());
        assert!(open(&sealed, &[8u8; 32]).is_err());
        assert!(!is_sealed(b"{\"plain\":true}"));
        assert_eq!(decode(b"{}".to_vec()).unwrap(), b"{}");
    }

    #[test]
    fn key_encoding() {
        let k = [42u8; 32];
        assert_eq!(decode_key(&encode_key(&k)).unwrap(), k);
        assert!(decode_key("abc").is_err());
    }
}
