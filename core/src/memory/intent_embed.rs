//! Cheap deterministic intent embeddings for SolutionsDb (no external model).

/// Embedding dimensionality expected by SolutionsDb.
pub const EMBED_DIM: usize = 384;

/// Hash bag-of-tokens (+ bigrams) into a fixed L2-normalized vector.
pub fn intent_embedding(text: &str) -> Vec<f32> {
    let mut v = vec![0.0f32; EMBED_DIM];
    let normalized = text.to_ascii_lowercase();
    let tokens: Vec<&str> = normalized
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() > 1)
        .collect();

    for token in &tokens {
        let idx = fnv1a(token) % EMBED_DIM as u32;
        v[idx as usize] += 1.0;
    }
    for window in tokens.windows(2) {
        let bigram = format!("{}_{}", window[0], window[1]);
        let idx = fnv1a(&bigram) % EMBED_DIM as u32;
        v[idx as usize] += 0.75;
    }

    // Character trigrams catch paraphrases with shared substrings.
    let chars: Vec<char> = normalized.chars().filter(|c| c.is_alphanumeric()).collect();
    if chars.len() >= 3 {
        for i in 0..chars.len() - 2 {
            let tri: String = chars[i..i + 3].iter().collect();
            let idx = fnv1a(&tri) % EMBED_DIM as u32;
            v[idx as usize] += 0.35;
        }
    }

    let mut norm = 0.0f32;
    for x in &v {
        norm += x * x;
    }
    if norm > 0.0 {
        let inv = 1.0 / norm.sqrt();
        for x in &mut v {
            *x *= inv;
        }
    }
    v
}

fn fnv1a(s: &str) -> u32 {
    let mut hash: u32 = 0x811c_9dc5;
    for b in s.bytes() {
        hash ^= u32::from(b);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn similar_intents_are_close() {
        let a = intent_embedding("Tell me today's weather");
        let b = intent_embedding("what is the weather today");
        let mut dot = 0.0f32;
        for i in 0..EMBED_DIM {
            dot += a[i] * b[i];
        }
        assert!(dot > 0.35, "expected related weather intents, got {dot}");
    }

    #[test]
    fn unrelated_intents_differ() {
        let a = intent_embedding("Tell me today's weather");
        let b = intent_embedding("open calculator and multiply 12 by 8");
        let mut dot = 0.0f32;
        for i in 0..EMBED_DIM {
            dot += a[i] * b[i];
        }
        assert!(dot < 0.5, "expected low similarity, got {dot}");
    }
}
