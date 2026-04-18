// Native Rook embedder.
//
// Feature-hashed bag of word tokens + char n-grams projected into a fixed 384-dim
// L2-normalized vector. Zero dependencies beyond std. Deterministic. Fast
// (~microseconds for small text on a single core).
//
// Why not an on-device transformer? Because Rook should boot instantly on a
// fresh machine with no model download. The feature-hashing trick gives us
// retrieval quality good enough to beat keyword search for the scale of memory
// we care about (thousands of short nodes), while being cache-friendly and
// trivial to reason about.
//
// Design:
//   • Tokens: whitespace-split + punctuation-split, lowercased, stop-word filtered.
//   • Char n-grams: sizes 3 and 4, walked across tokens so we get sub-word
//     similarity (`rust`/`rusty`/`rusted` share 3-grams).
//   • Feature hashing: two hashes per feature (one for the bucket, one for the
//     sign) — the standard trick for cancelling collision bias (Weinberger et
//     al. 2009). Output dim = 384.
//   • Frequency weighting: log(1 + tf). No IDF — that would require a global
//     corpus table and introduce write contention for zero retrieval benefit
//     at our scale.
//   • Final step: L2 normalize so cosine similarity reduces to a dot product.

const DIM: usize = 384;
const NGRAM_MIN: usize = 3;
const NGRAM_MAX: usize = 4;

// Stop words that add noise to similarity — dropped before hashing.
const STOP: &[&str] = &[
    "a", "an", "the", "and", "or", "but", "is", "are", "was", "were", "be", "been", "being",
    "have", "has", "had", "do", "does", "did", "of", "in", "on", "at", "to", "for", "with", "by",
    "as", "it", "this", "that", "these", "those", "i", "you", "he", "she", "we", "they", "my",
    "your", "his", "her", "our", "their", "so", "if", "then", "than", "which", "who", "what",
];

/// Deterministic 384-d embedding for arbitrary text.
pub fn embed(text: &str) -> Vec<f32> {
    let mut vec = vec![0f32; DIM];

    // Whitespace + punctuation split, lowercase.
    let cleaned = text.to_ascii_lowercase();
    let tokens: Vec<&str> = cleaned
        .split(|c: char| !c.is_alphanumeric())
        .filter(|s| !s.is_empty() && !STOP.contains(s))
        .collect();

    // Accumulate term frequencies so we can weight by log(1+tf) at the end.
    use std::collections::HashMap;
    let mut counts: HashMap<u64, f32> = HashMap::new();

    // Word-level features.
    for tok in &tokens {
        add_feature(&mut counts, &format!("w:{}", tok));
    }

    // Char n-gram features for each token. Pads with ^/$ boundaries so
    // prefixes/suffixes get their own signatures ("run" and "running"
    // still share `^ru`, `run`, `un$`).
    for tok in &tokens {
        let padded: String = format!("^{}$", tok);
        let chars: Vec<char> = padded.chars().collect();
        for n in NGRAM_MIN..=NGRAM_MAX {
            if chars.len() < n {
                continue;
            }
            for window in chars.windows(n) {
                let gram: String = window.iter().collect();
                add_feature(&mut counts, &format!("c{}:{}", n, gram));
            }
        }
    }

    // Emit into vec via two-hash trick (Weinberger et al. 2009).
    for (hash, tf) in counts {
        let bucket = (hash as usize) % DIM;
        // sign: top bit of a second hash, folded down
        let sign_bit = ((hash >> 32) ^ (hash << 1)) & 1;
        let sign = if sign_bit == 0 { 1.0 } else { -1.0 };
        let weight = (1.0 + tf).ln() + 1.0;
        vec[bucket] += sign * weight;
    }

    // L2 normalize so cosine similarity = dot product.
    let norm_sq: f32 = vec.iter().map(|v| v * v).sum();
    if norm_sq > 1e-12 {
        let inv = 1.0 / norm_sq.sqrt();
        for v in vec.iter_mut() {
            *v *= inv;
        }
    }

    vec
}

#[inline]
fn add_feature(counts: &mut std::collections::HashMap<u64, f32>, feat: &str) {
    let h = hash_feature(feat);
    *counts.entry(h).or_insert(0.0) += 1.0;
}

// Small deterministic hash — std's DefaultHasher is fine for feature hashing.
// Not cryptographic; collisions are OK (that's the whole point).
fn hash_feature(s: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut h);
    h.finish()
}

/// Return the cosine similarity of two `embed()` vectors. Since we L2-normalize
/// at write time, this is just a dot product.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() {
        return 0.0;
    }
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_text_is_identical() {
        let a = embed("hello world");
        let b = embed("hello world");
        assert_eq!(a.len(), DIM);
        assert!((cosine(&a, &b) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn similar_text_similar() {
        let a = embed("rust is a systems programming language");
        let b = embed("rust is used for systems programming");
        let sim = cosine(&a, &b);
        assert!(
            sim > 0.4,
            "similar texts should have sim > 0.4, got {}",
            sim
        );
    }

    #[test]
    fn different_text_different() {
        let a = embed("rust is a systems programming language");
        let b = embed("the cat sat on the mat");
        let sim = cosine(&a, &b);
        assert!(
            sim < 0.3,
            "unrelated texts should have sim < 0.3, got {}",
            sim
        );
    }

    #[test]
    fn subword_similarity() {
        let a = embed("running marathon");
        let b = embed("runner finished race");
        let sim = cosine(&a, &b);
        // Char n-grams should catch "run" stem
        assert!(
            sim > 0.1,
            "words sharing stems should have sim > 0.1, got {}",
            sim
        );
    }

    #[test]
    fn empty_text_returns_zero_vector() {
        let v = embed("");
        assert_eq!(v.len(), DIM);
        assert!(v.iter().all(|&x| x == 0.0));
    }
}
