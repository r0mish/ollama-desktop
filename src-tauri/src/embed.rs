//! Recherche sémantique dans les fichiers de référence d'un projet. Les embeddings sont
//! calculés par le serveur Ollama de l'utilisateur (`/api/embed`) et gardés dans un index
//! local chiffré : rien ne part chez un tiers. Seuls les extraits nouveaux ou modifiés sont
//! recalculés.

use std::collections::HashMap;
use std::path::Path;

use crate::textsearch::Chunk;

type R<T> = Result<T, String>;

/// Serveur et modèle d'embedding à utiliser.
#[derive(Clone)]
pub struct EmbedConn {
    pub base: String,
    pub key: String,
    pub model: String,
}

const MAGIC: &[u8; 8] = b"MDEMB1\n\0";
/// Extraits envoyés par requête.
const BATCH: usize = 16;

/// Empreinte stable d'un extrait (FNV-1a 64 bits) : clé de l'index.
fn fingerprint(text: &str) -> u64 {
    text.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x0000_0100_0000_01b3))
}

/// Texte indexé pour un extrait : le nom du fichier aide à situer le passage.
fn indexed_text(c: &Chunk) -> String {
    format!("{}\n{}", c.source, c.text)
}

fn normalize(mut v: Vec<f32>) -> Vec<f32> {
    let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    if norm > 0.0 {
        v.iter_mut().for_each(|x| *x /= norm);
    }
    v
}

#[derive(Default)]
struct Index {
    model: String,
    /// Vecteurs normalisés, par empreinte d'extrait.
    vectors: HashMap<u64, Vec<f32>>,
}

impl Index {
    /// Format : en-tête, nom du modèle, dimension, puis (empreinte, vecteur) pour chaque extrait.
    fn to_bytes(&self) -> Vec<u8> {
        let dims = self.vectors.values().next().map(Vec::len).unwrap_or(0);
        let mut out = Vec::with_capacity(32 + self.model.len() + self.vectors.len() * (8 + dims * 4));
        out.extend_from_slice(MAGIC);
        out.extend_from_slice(&(self.model.len() as u32).to_le_bytes());
        out.extend_from_slice(self.model.as_bytes());
        out.extend_from_slice(&(dims as u32).to_le_bytes());
        out.extend_from_slice(&(self.vectors.len() as u32).to_le_bytes());
        for (hash, v) in &self.vectors {
            out.extend_from_slice(&hash.to_le_bytes());
            v.iter().for_each(|x| out.extend_from_slice(&x.to_le_bytes()));
        }
        out
    }

    fn from_bytes(b: &[u8]) -> Option<Index> {
        let mut pos = 0;
        let mut take = |n: usize| {
            let s = b.get(pos..pos + n)?;
            pos += n;
            Some(s)
        };
        if take(8)? != MAGIC {
            return None;
        }
        let name_len = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let model = String::from_utf8(take(name_len)?.to_vec()).ok()?;
        let dims = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let count = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
        let mut vectors = HashMap::with_capacity(count);
        for _ in 0..count {
            let hash = u64::from_le_bytes(take(8)?.try_into().ok()?);
            let raw = take(dims * 4)?;
            vectors.insert(hash, raw.as_chunks::<4>().0.iter().map(|c| f32::from_le_bytes(*c)).collect());
        }
        Some(Index { model, vectors })
    }
}

fn load(path: &Path, model: &str) -> Index {
    std::fs::read(path)
        .ok()
        .and_then(|raw| crate::vault::decode(raw).ok())
        .and_then(|b| Index::from_bytes(&b))
        // Autre modèle : les vecteurs ne sont pas comparables, l'index est reconstruit.
        .filter(|i| i.model == model)
        .unwrap_or_else(|| Index { model: model.to_string(), vectors: HashMap::new() })
}

fn save(path: &Path, index: &Index) -> R<()> {
    let sealed = crate::vault::encode(&index.to_bytes())?;
    let tmp = path.with_extension("emb.tmp");
    std::fs::write(&tmp, sealed).map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, path).map_err(|e| e.to_string())
}

/// Classe les extraits du plus proche au plus éloigné du sens de `query`.
/// `progress(faits, total)` est appelé pendant l'indexation des extraits manquants.
pub async fn rank(index_path: &Path, conn: &EmbedConn, chunks: &[Chunk], query: &str, progress: &(dyn Fn(usize, usize) + Send + Sync)) -> R<Vec<usize>> {
    if chunks.is_empty() || query.trim().is_empty() {
        return Ok(Vec::new());
    }
    let texts: Vec<String> = chunks.iter().map(indexed_text).collect();
    let hashes: Vec<u64> = texts.iter().map(|t| fingerprint(t)).collect();
    let mut index = load(index_path, &conn.model);

    let mut missing: Vec<usize> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for (i, h) in hashes.iter().enumerate() {
        if !index.vectors.contains_key(h) && seen.insert(*h) {
            missing.push(i);
        }
    }
    let stale = index.vectors.len() > hashes.iter().collect::<std::collections::HashSet<_>>().len();
    if !missing.is_empty() {
        let total = missing.len();
        for (n, batch) in missing.chunks(BATCH).enumerate() {
            progress(n * BATCH, total);
            let inputs: Vec<String> = batch.iter().map(|&i| texts[i].clone()).collect();
            let vectors = crate::ollama::embed(&conn.base, &conn.key, &conn.model, &inputs).await?;
            for (&i, v) in batch.iter().zip(vectors) {
                index.vectors.insert(hashes[i], normalize(v));
            }
        }
        progress(total, total);
    }
    if !missing.is_empty() || stale {
        // Les extraits supprimés ou modifiés quittent l'index.
        let keep: std::collections::HashSet<u64> = hashes.iter().copied().collect();
        index.vectors.retain(|h, _| keep.contains(h));
        save(index_path, &index)?;
    }

    let q = normalize(crate::ollama::embed(&conn.base, &conn.key, &conn.model, &[query.to_string()]).await?.pop().ok_or("embedding de la question absent")?);
    let mut scored: Vec<(usize, f32)> = hashes
        .iter()
        .enumerate()
        .filter_map(|(i, h)| {
            let v = index.vectors.get(h)?;
            (v.len() == q.len()).then(|| (i, v.iter().zip(&q).map(|(a, b)| a * b).sum::<f32>()))
        })
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    Ok(scored.into_iter().map(|(i, _)| i).collect())
}

/// Fusion de deux classements (reciprocal rank fusion) : un extrait bien placé par les
/// mots-clés ou par le sens remonte, les deux à la fois encore plus.
pub fn fuse(keyword: &[usize], semantic: &[usize], limit: usize) -> Vec<usize> {
    let mut score: HashMap<usize, f64> = HashMap::new();
    for list in [keyword, semantic] {
        for (rank, &i) in list.iter().enumerate() {
            *score.entry(i).or_default() += 1.0 / (60.0 + rank as f64);
        }
    }
    let mut out: Vec<(usize, f64)> = score.into_iter().collect();
    out.sort_by(|a, b| b.1.total_cmp(&a.1).then(a.0.cmp(&b.0)));
    out.into_iter().take(limit).map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn index_round_trips() {
        let mut index = Index { model: "bge-m3".into(), vectors: HashMap::new() };
        index.vectors.insert(fingerprint("a"), vec![0.5, -0.25, 1.0]);
        index.vectors.insert(fingerprint("b"), vec![0.0, 2.0, -1.0]);
        let back = Index::from_bytes(&index.to_bytes()).unwrap();
        assert_eq!(back.model, "bge-m3");
        assert_eq!(back.vectors, index.vectors);
        assert!(Index::from_bytes(b"autre chose").is_none());
        assert_ne!(fingerprint("a"), fingerprint("b"));
    }

    #[test]
    fn fusion_rewards_agreement() {
        // 7 est premier pour le sens et second pour les mots-clés : il passe devant 3 (premier d'un seul classement).
        assert_eq!(fuse(&[3, 7, 1], &[7, 9], 3), [7, 3, 9]);
        assert_eq!(fuse(&[], &[4, 2], 5), [4, 2]);
    }
}
