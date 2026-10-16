//! The seam for text embeddings (card R3-06): something that turns a text into a vector, so that two texts that mean the same come out close.
//!
//! Aqua has had one embedding, [`HashEmbedding`] (`intent_embed`: a bag of hashed words, bigrams and letter triples, no model). It finds a text that
//! shares words with another and almost nothing else. The measurement of card R6-36 (`fixtures/fact_retrieval_cases.json`) found that a request put in
//! other words than the fact it asks for is found 3 times in 21 by the keywords and the hash, and about 16 times in 21 by a small static model; so a
//! second provider, [`super::static_embed::StaticEmbedder`], reads such a model from a folder, and this is the trait both implement.
//!
//! **What the trait is not for.** The vectors that decide whether a stored recipe may be replayed (`SolutionsDb`, the 0.92 and 0.95 of cosine) stay the
//! hash: a better embedding would put "the weather in Delhi" and "the weather in Goa" much closer, and a replay would then answer for the wrong city.
//! Only retrieval of facts about the person uses a ranker, and only when the person switched it on (`AQUA_FACT_RANK=on`, off by default).
//!
//! The model is not downloaded by the program. It is read from `data/models/embedding/` (or the folder in `AQUA_EMBED_DIR`) when the switch is on; without
//! the files the switch does nothing and the log says so once.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};

/// Something that turns a text into a vector. The vectors it makes are comparable with each other (the same provider, the same length) and have a length
/// of 1, or are all zeros when the text holds nothing the provider knows.
pub trait EmbeddingProvider: Send + Sync {
    /// A short name for logs and measurements.
    fn name(&self) -> String;
    /// How many numbers a vector has.
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Vec<f32>;
    /// Several texts at once. The default does one after the other; a provider that can do better says so.
    fn embed_many(&self, texts: &[&str]) -> Vec<Vec<f32>> {
        texts.iter().map(|t| self.embed(t)).collect()
    }
}

/// The cosine of two vectors: 1 for the same direction, 0 for unrelated, negative for opposed. `0.0` when either is all zeros or the lengths differ.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let (na, nb) = (a.iter().map(|x| x * x).sum::<f32>().sqrt(), b.iter().map(|x| x * x).sum::<f32>().sqrt());
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na * nb)
    }
}

/// The texts in the order of how well they fit `query`: `(index in texts, cosine)`, the best first, ties in the order the texts came.
pub fn rank_by_similarity(provider: &dyn EmbeddingProvider, query: &str, texts: &[&str]) -> Vec<(usize, f32)> {
    let q = provider.embed(query);
    let mut ranked: Vec<(usize, f32)> = provider.embed_many(texts).iter().enumerate().map(|(i, e)| (i, cosine(&q, e))).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal).then(a.0.cmp(&b.0)));
    ranked
}

/// The embedding Aqua has always had (a hash of words, no model), behind the trait so that a measurement can put it beside another.
pub struct HashEmbedding;

impl EmbeddingProvider for HashEmbedding {
    fn name(&self) -> String {
        "hash-384".to_string()
    }

    fn dim(&self) -> usize {
        super::intent_embed::EMBED_DIM
    }

    fn embed(&self, text: &str) -> Vec<f32> {
        super::intent_embed::intent_embedding(text)
    }
}

/// The switch for ranking facts with a static model. Off unless it is `on`.
pub const RANK_FLAG: &str = "AQUA_FACT_RANK";
/// The folder the static model is read from, when it is not the default one.
pub const DIR_ENV: &str = "AQUA_EMBED_DIR";

/// Is ranking facts with a static model switched on?
pub fn ranking_enabled() -> bool {
    std::env::var(RANK_FLAG).is_ok_and(|v| v.trim().eq_ignore_ascii_case("on"))
}

/// Where the static model's files are: `model.safetensors`, `tokenizer.json` and (optionally) `config.json`.
pub fn model_dir() -> PathBuf {
    match std::env::var_os(DIR_ENV).map(PathBuf::from).filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir,
        None => crate::runtime::default_models_dir().join("embedding"),
    }
}

/// The model in `dir` as a provider, or why there is none. (No switch is looked at here: [`ranker`] does that.)
pub fn load_ranker(dir: &Path) -> Result<Arc<dyn EmbeddingProvider>, String> {
    let model = super::static_embed::StaticEmbedder::load(dir)?;
    Ok(Arc::new(model))
}

static RANKER: OnceLock<Option<Arc<dyn EmbeddingProvider>>> = OnceLock::new();

/// The provider that ranks facts: `None` while the switch is off (the model is not even read), and `None` when it is on and the model cannot be read
/// (said once in the log; the facts are then found as they were). The model is read the first time it is needed and kept; a model put in the folder
/// later is read at the next start.
pub fn ranker() -> Option<Arc<dyn EmbeddingProvider>> {
    if !ranking_enabled() {
        return None;
    }
    RANKER
        .get_or_init(|| {
            let dir = model_dir();
            match load_ranker(&dir) {
                Ok(model) => {
                    tracing::info!("fact ranking: the static embedding model {} is loaded from {} ({} numbers a text)", model.name(), dir.display(), model.dim());
                    Some(model)
                }
                Err(why) => {
                    tracing::warn!("fact ranking is on (AQUA_FACT_RANK) but there is no model to rank with, so facts are found as before: {why}");
                    None
                }
            }
        })
        .clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_cosine_is_one_for_the_same_direction_zero_for_unrelated_and_zero_when_it_cannot_be_told() {
        assert!((cosine(&[1.0, 0.0], &[2.0, 0.0]) - 1.0).abs() < 1e-6);
        assert!(cosine(&[1.0, 0.0], &[0.0, 3.0]).abs() < 1e-6);
        assert!((cosine(&[1.0, 0.0], &[-1.0, 0.0]) + 1.0).abs() < 1e-6);
        assert_eq!(cosine(&[0.0, 0.0], &[1.0, 0.0]), 0.0, "an all-zero vector is related to nothing");
        assert_eq!(cosine(&[1.0], &[1.0, 0.0]), 0.0, "vectors of different lengths are not compared");
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn the_hash_embedding_behind_the_trait_is_the_one_aqua_has_always_had() {
        let p = HashEmbedding;
        assert_eq!(p.dim(), super::super::EMBED_DIM);
        assert_eq!(p.embed("tell me today's weather"), crate::memory::intent_embedding("tell me today's weather"));
        let ranked = rank_by_similarity(&p, "weather today", &["open calculator and multiply 12 by 8", "what is the weather today", "the weather"]);
        assert_eq!(ranked[0].0, 1, "{ranked:?}");
        assert_eq!(ranked.len(), 3);
        assert!(ranked.windows(2).all(|w| w[0].1 >= w[1].1), "best first: {ranked:?}");
    }

    #[test]
    fn a_tie_keeps_the_order_the_texts_came_in() {
        struct Flat;
        impl EmbeddingProvider for Flat {
            fn name(&self) -> String {
                "flat".into()
            }
            fn dim(&self) -> usize {
                2
            }
            fn embed(&self, _text: &str) -> Vec<f32> {
                vec![1.0, 0.0]
            }
        }
        let ranked = rank_by_similarity(&Flat, "q", &["a", "b", "c"]);
        assert_eq!(ranked.iter().map(|r| r.0).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    #[test]
    fn a_folder_with_no_model_is_a_reason_not_a_provider() {
        let dir = std::env::temp_dir().join(format!("aqua_embed_none_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let why = load_ranker(&dir).err().expect("there is no model in an empty folder");
        assert!(why.contains("model.safetensors"), "the reason names the missing file: {why}");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
