//! Recherche textuelle locale, sans service tiers :
//! plein texte dans les conversations, et classement BM25 de morceaux de documents (projets).

use std::collections::HashMap;

/// Minuscule sans accents, caractère par caractère (les positions restent alignées sur l'original).
pub fn fold_char(c: char) -> char {
    let c = c.to_lowercase().next().unwrap_or(c);
    match c {
        'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
        'ç' => 'c',
        'è' | 'é' | 'ê' | 'ë' => 'e',
        'ì' | 'í' | 'î' | 'ï' => 'i',
        'ñ' => 'n',
        'ò' | 'ó' | 'ô' | 'õ' | 'ö' => 'o',
        'ù' | 'ú' | 'û' | 'ü' => 'u',
        'ý' | 'ÿ' => 'y',
        'œ' => 'o',
        'æ' => 'a',
        c => c,
    }
}

pub fn fold(s: &str) -> Vec<char> {
    s.chars().map(fold_char).collect()
}

fn find(hay: &[char], needle: &[char]) -> Option<usize> {
    if needle.is_empty() || needle.len() > hay.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

/// Extrait autour de la première occurrence de `query` dans `text` (insensible à la casse et aux accents).
pub fn snippet(text: &str, query: &str, radius: usize) -> Option<String> {
    let chars: Vec<char> = text.chars().collect();
    let pos = find(&fold(text), &fold(query.trim()))?;
    let start = pos.saturating_sub(radius);
    let end = (pos + query.trim().chars().count() + radius).min(chars.len());
    let mut s: String = chars[start..end].iter().collect::<String>().split_whitespace().collect::<Vec<_>>().join(" ");
    if start > 0 {
        s.insert(0, '…');
    }
    if end < chars.len() {
        s.push('…');
    }
    Some(s)
}

// ---------------------------------------------------------------- BM25

fn tokens(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for c in s.chars().map(fold_char) {
        if c.is_alphanumeric() {
            cur.push(c);
        } else if !cur.is_empty() {
            out.push(std::mem::take(&mut cur));
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    // Mots vides français et anglais très fréquents : n'apportent rien au classement.
    const STOP: [&str; 44] = [
        "le", "la", "les", "un", "une", "des", "de", "du", "et", "ou", "a", "au", "aux", "en", "est", "que", "qui", "pour",
        "dans", "sur", "par", "ce", "cette", "il", "elle", "on", "je", "tu", "nous", "vous", "the", "of", "and", "to", "in", "is",
        "for", "on", "with", "an", "it", "that", "this", "be",
    ];
    out.retain(|t| t.chars().count() > 1 && !STOP.contains(&t.as_str()));
    // Racinisation minimale : pluriels en -s / -x.
    for t in out.iter_mut() {
        if t.chars().count() > 3 && (t.ends_with('s') || t.ends_with('x')) {
            t.pop();
        }
    }
    out
}

pub struct Chunk {
    pub source: String,
    pub text: String,
}

/// Découpe un document en morceaux d'environ `size` caractères, coupés sur les fins de paragraphe ou de ligne.
pub fn chunk(source: &str, text: &str, size: usize) -> Vec<Chunk> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for line in text.split_inclusive('\n') {
        if cur.chars().count() + line.chars().count() > size && !cur.trim().is_empty() {
            out.push(Chunk { source: source.to_string(), text: std::mem::take(&mut cur) });
        }
        if line.chars().count() > size {
            // Ligne géante (texte sans retours) : découpe brute.
            let chars: Vec<char> = line.chars().collect();
            for part in chars.chunks(size) {
                out.push(Chunk { source: source.to_string(), text: part.iter().collect() });
            }
            continue;
        }
        cur.push_str(line);
    }
    if !cur.trim().is_empty() {
        out.push(Chunk { source: source.to_string(), text: cur });
    }
    out
}

/// Indices des morceaux les plus pertinents pour `query` (BM25, k1 = 1.2, b = 0.75), meilleurs d'abord.
pub fn bm25_rank(chunks: &[Chunk], query: &str, limit: usize) -> Vec<(usize, f64)> {
    let q: Vec<String> = {
        let mut t = tokens(query);
        t.sort();
        t.dedup();
        t
    };
    if q.is_empty() || chunks.is_empty() {
        return Vec::new();
    }
    let docs: Vec<Vec<String>> = chunks.iter().map(|c| tokens(&format!("{} {}", c.source, c.text))).collect();
    let n = docs.len() as f64;
    let avg = docs.iter().map(Vec::len).sum::<usize>() as f64 / n;
    let df: HashMap<&str, usize> = q
        .iter()
        .map(|t| (t.as_str(), docs.iter().filter(|d| d.iter().any(|w| w == t)).count()))
        .collect();
    let mut scored: Vec<(usize, f64)> = docs
        .iter()
        .enumerate()
        .map(|(i, d)| {
            let len = d.len() as f64;
            let score = q
                .iter()
                .map(|t| {
                    let f = d.iter().filter(|w| *w == t).count() as f64;
                    if f == 0.0 {
                        return 0.0;
                    }
                    let df = df[t.as_str()] as f64;
                    let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
                    idf * f * 2.2 / (f + 1.2 * (0.25 + 0.75 * len / avg.max(1.0)))
                })
                .sum::<f64>();
            (i, score)
        })
        .filter(|(_, s)| *s > 0.0)
        .collect();
    scored.sort_by(|a, b| b.1.total_cmp(&a.1));
    scored.truncate(limit);
    scored
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snippet_ignores_case_and_accents() {
        let t = "Bonjour, voici la Configuration de Fail2Ban pour le serveur.";
        assert_eq!(snippet(t, "configuration", 8).unwrap(), "…oici la Configuration de Fail…");
        assert!(snippet(t, "CONFIGURÂTION", 3).is_some());
        assert!(snippet(t, "nginx", 3).is_none());
    }

    #[test]
    fn chunking_and_ranking() {
        let doc = "# Réseau\nLe VLAN 20 sert aux caméras.\n\n# Stockage\nLe pool ZFS tank utilise 4 disques en RAIDZ2.\n\n# Sauvegardes\nProxmox Backup Server tourne sur pbs1.\n";
        let chunks = chunk("infra.md", doc, 60);
        assert!(chunks.len() >= 3, "{}", chunks.len());
        let best = bm25_rank(&chunks, "combien de disques dans le pool ZFS ?", 2);
        assert!(chunks[best[0].0].text.contains("RAIDZ2"));
        let best = bm25_rank(&chunks, "sauvegarde proxmox", 1);
        assert!(chunks[best[0].0].text.contains("pbs1"));
        assert!(bm25_rank(&chunks, "le la de", 3).is_empty());
        // Ligne sans retour plus longue que la taille : découpée.
        assert_eq!(chunk("x", &"a".repeat(250), 100).len(), 3);
    }
}
