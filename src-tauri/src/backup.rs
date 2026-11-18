//! Sauvegarde chiffrée de toutes les données (conversations, projets, assistants, réglages)
//! dans un seul fichier, à déposer dans un dossier synchronisé (Nextcloud, Syncthing…).
//! Chiffrement XChaCha20-Poly1305 avec une clé dérivée d'une phrase secrète (Argon2id) :
//! le fichier se restaure sur n'importe quelle machine, sans dépendre du trousseau.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use argon2::{Algorithm, Argon2, Params, Version};
use base64::Engine;
use chacha20poly1305::aead::{Aead, AeadCore, KeyInit, OsRng};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use serde::{Deserialize, Serialize};

type R<T> = Result<T, String>;

const MAGIC: &[u8] = b"ODBAK1\n";
const SALT_LEN: usize = 16;
const NONCE_LEN: usize = 24;
pub const EXTENSION: &str = "odbak";
const PREFIX: &str = "ollama-desktop-";

#[derive(Serialize, Deserialize)]
struct Entry {
    name: String,
    /// Contenu en base64 (texte clair : le fichier entier est chiffré ensuite).
    data: String,
}

#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Bundle {
    format: u32,
    app_version: String,
    created_at: u64,
    files: Vec<Entry>,
}

/// Clé de 256 bits dérivée de la phrase secrète (Argon2id, 64 Mio, 3 passes).
fn derive_key(passphrase: &str, salt: &[u8]) -> R<[u8; 32]> {
    let params = Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| e.to_string())?;
    let mut key = [0u8; 32];
    Argon2::new(Algorithm::Argon2id, Version::V0x13, params)
        .hash_password_into(passphrase.as_bytes(), salt, &mut key)
        .map_err(|e| e.to_string())?;
    Ok(key)
}

/// Assemble, compresse et chiffre les fichiers `(nom, contenu)`.
pub fn seal(files: Vec<(String, Vec<u8>)>, passphrase: &str, created_at: u64) -> R<Vec<u8>> {
    if passphrase.chars().count() < 8 {
        return Err("La phrase secrète doit compter au moins 8 caractères".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    let bundle = Bundle {
        format: 1,
        app_version: env!("CARGO_PKG_VERSION").into(),
        created_at,
        files: files.into_iter().map(|(name, data)| Entry { name, data: b64.encode(data) }).collect(),
    };
    let json = serde_json::to_vec(&bundle).map_err(|e| e.to_string())?;
    let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    gz.write_all(&json).map_err(|e| e.to_string())?;
    let plain = gz.finish().map_err(|e| e.to_string())?;

    let mut salt = [0u8; SALT_LEN];
    salt.copy_from_slice(&XChaCha20Poly1305::generate_nonce(&mut OsRng)[..SALT_LEN]);
    let key = derive_key(passphrase, &salt)?;
    let nonce = XChaCha20Poly1305::generate_nonce(&mut OsRng);
    let ct = XChaCha20Poly1305::new(Key::from_slice(&key)).encrypt(&nonce, plain.as_slice()).map_err(|_| "Chiffrement impossible".to_string())?;
    let mut out = Vec::with_capacity(MAGIC.len() + SALT_LEN + NONCE_LEN + ct.len());
    out.extend_from_slice(MAGIC);
    out.extend_from_slice(&salt);
    out.extend_from_slice(&nonce);
    out.extend_from_slice(&ct);
    Ok(out)
}

/// Déchiffre une sauvegarde et renvoie ses fichiers `(nom, contenu)`.
pub fn open(data: &[u8], passphrase: &str) -> R<Vec<(String, Vec<u8>)>> {
    let body = data.strip_prefix(MAGIC).ok_or("Ce fichier n'est pas une sauvegarde Ollama Desktop")?;
    if body.len() < SALT_LEN + NONCE_LEN {
        return Err("Sauvegarde tronquée".into());
    }
    let (salt, rest) = body.split_at(SALT_LEN);
    let (nonce, ct) = rest.split_at(NONCE_LEN);
    let key = derive_key(passphrase, salt)?;
    let plain = XChaCha20Poly1305::new(Key::from_slice(&key))
        .decrypt(XNonce::from_slice(nonce), ct)
        .map_err(|_| "Phrase secrète incorrecte, ou sauvegarde altérée".to_string())?;
    let mut json = Vec::new();
    flate2::read::GzDecoder::new(plain.as_slice()).read_to_end(&mut json).map_err(|e| format!("Sauvegarde illisible : {e}"))?;
    let bundle: Bundle = serde_json::from_slice(&json).map_err(|e| format!("Sauvegarde illisible : {e}"))?;
    if bundle.format != 1 {
        return Err("Sauvegarde créée par une version plus récente de l'application".into());
    }
    let b64 = base64::engine::general_purpose::STANDARD;
    bundle.files.into_iter().map(|e| Ok((e.name, b64.decode(e.data).map_err(|e| e.to_string())?))).collect()
}

/// `AAAAMMJJ-HHMMSS` (UTC) pour un horodatage en secondes.
fn stamp(secs: u64) -> String {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    // Date civile à partir du nombre de jours depuis 1970 (algorithme de Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Écrit la sauvegarde dans `dir` et supprime les plus anciennes au-delà de `keep`.
pub fn write(dir: &Path, sealed: &[u8], now_secs: u64, keep: usize) -> R<PathBuf> {
    if !dir.is_dir() {
        return Err(format!("Dossier de sauvegarde introuvable : {}", dir.display()));
    }
    let path = dir.join(format!("{PREFIX}{}.{EXTENSION}", stamp(now_secs)));
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, sealed).map_err(|e| format!("{}: {e}", tmp.display()))?;
    std::fs::rename(&tmp, &path).map_err(|e| format!("{}: {e}", path.display()))?;
    // Les noms horodatés se trient par ancienneté.
    let mut old: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| e.to_string())?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some(EXTENSION) && p.file_name().and_then(|n| n.to_str()).is_some_and(|n| n.starts_with(PREFIX)))
        .collect();
    old.sort();
    for p in old.iter().rev().skip(keep.max(1)) {
        let _ = std::fs::remove_file(p);
    }
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn backup_round_trips_and_rejects_wrong_passphrase() {
        let files = vec![("conversations/a1.json".to_string(), b"{\"id\":\"a1\"}".to_vec()), ("settings.json".to_string(), vec![0u8, 255, 10])];
        let sealed = seal(files.clone(), "phrase secrete longue", 1_700_000_000_000).unwrap();
        assert!(sealed.starts_with(MAGIC));
        // Rien de lisible dans le fichier.
        assert!(!sealed.windows(4).any(|w| w == b"a1.j"));
        assert_eq!(open(&sealed, "phrase secrete longue").unwrap(), files);
        assert!(open(&sealed, "autre phrase secrete").unwrap_err().contains("incorrecte"));
        let mut tampered = sealed.clone();
        *tampered.last_mut().unwrap() ^= 1;
        assert!(open(&tampered, "phrase secrete longue").is_err());
        assert!(open(b"pas une sauvegarde", "x").is_err());
        assert!(seal(files, "court", 0).is_err());
    }

    #[test]
    fn files_are_stamped_and_pruned() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_794_441_600 + 3_661), "20261112-010101");
        let dir = std::env::temp_dir().join(format!("md-bak-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("notes.txt"), "x").unwrap();
        for day in 0..5u64 {
            write(&dir, b"x", 1_794_441_600 + day * 86_400, 3).unwrap();
        }
        let mut names: Vec<String> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names, ["notes.txt", "ollama-desktop-20261114-000000.odbak", "ollama-desktop-20261115-000000.odbak", "ollama-desktop-20261116-000000.odbak"]);
        assert!(write(&dir.join("absent"), b"x", 0, 3).is_err());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
