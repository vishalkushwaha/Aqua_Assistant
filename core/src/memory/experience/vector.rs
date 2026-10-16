//! Embeddings and semantic search, behind traits so either can be replaced.
//!
//! Aqua has no text-embedding model of its own, so the default provider is the deterministic hash
//! embedding already used by the recipe store. The index is an in-process cosine scan: experiences
//! number in the hundreds or thousands, so it is fast and needs no extra service. Both sit behind
//! `EmbeddingProvider` / `VectorIndex`; a real model or an external vector database only has to
//! implement them.

use super::model::Experience;
use super::store::ExperienceStore;

/// Turns text into a vector. `model` and `version` are stored with every experience so vectors from
/// different providers are never compared with each other.
pub trait EmbeddingProvider: Send + Sync {
    fn model(&self) -> &str;
    fn version(&self) -> u32;
    fn dim(&self) -> usize;
    fn embed(&self, text: &str) -> Result<Vec<f32>, String>;
}

/// The built-in provider: hashed tokens, bigrams and character trigrams into 384 dimensions.
pub struct HashEmbedding;

impl EmbeddingProvider for HashEmbedding {
    fn model(&self) -> &str {
        "aqua-hash-384"
    }
    fn version(&self) -> u32 {
        1
    }
    fn dim(&self) -> usize {
        crate::memory::intent_embed::EMBED_DIM
    }
    fn embed(&self, text: &str) -> Result<Vec<f32>, String> {
        Ok(crate::memory::intent_embed::intent_embedding(text))
    }
}

/// The text that represents an experience in the index: the problem it solves. Setting, task type
/// and technology are matched from metadata when ranking; putting them in the embedded text only
/// dilutes the match between a new task and the problem it resembles.
pub fn embedding_text(e: &Experience) -> String {
    e.problem_summary.trim().to_string()
}

pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..a.len() {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        0.0
    } else {
        dot / (na.sqrt() * nb.sqrt())
    }
}

/// Semantic search over stored vectors.
pub trait VectorIndex {
    /// The `k` most similar ids with their similarity, best first.
    fn search(&self, query: &[f32], k: usize) -> Vec<(i64, f32)>;
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Cosine scan over the vectors of one embedding model and version.
pub struct BruteForceIndex {
    entries: Vec<(i64, Vec<f32>)>,
    /// Experiences whose vector came from a different model or has the wrong size. They are left
    /// out of results instead of being compared with incompatible numbers.
    pub skipped_incompatible: usize,
}

impl BruteForceIndex {
    pub fn build(rows: &[(Experience, Vec<f32>)], provider: &dyn EmbeddingProvider) -> Self {
        let mut entries = Vec::with_capacity(rows.len());
        let mut skipped = 0;
        for (e, v) in rows {
            if e.embedding_model == provider.model() && e.embedding_version == provider.version() && v.len() == provider.dim() {
                entries.push((e.id, v.clone()));
            } else {
                skipped += 1;
            }
        }
        Self { entries, skipped_incompatible: skipped }
    }
}

impl VectorIndex for BruteForceIndex {
    fn search(&self, query: &[f32], k: usize) -> Vec<(i64, f32)> {
        let mut scored: Vec<(i64, f32)> = self.entries.iter().map(|(id, v)| (*id, cosine(query, v))).collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(k);
        scored
    }
    fn len(&self) -> usize {
        self.entries.len()
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ReindexReport {
    pub total: usize,
    pub reindexed: usize,
    pub failed: usize,
    pub model: String,
    pub version: u32,
}

/// Re-embed every experience with `provider`. Vectors are computed for all rows first and only
/// written when every one succeeded, so a failing provider cannot leave the memory half old and
/// half new (which would silently mix incompatible vectors).
pub fn reindex(store: &ExperienceStore, provider: &dyn EmbeddingProvider) -> Result<ReindexReport, String> {
    let all = store.all_with_embeddings()?;
    let mut report = ReindexReport { total: all.len(), model: provider.model().into(), version: provider.version(), ..Default::default() };
    let mut fresh: Vec<(Experience, Vec<f32>)> = Vec::with_capacity(all.len());
    for (mut e, _) in all {
        match provider.embed(&embedding_text(&e)) {
            Ok(v) if v.len() == provider.dim() => {
                e.embedding_model = provider.model().into();
                e.embedding_version = provider.version();
                fresh.push((e, v));
            }
            _ => report.failed += 1,
        }
    }
    if report.failed > 0 {
        return Ok(report); // nothing written
    }
    for (e, v) in &fresh {
        store.update(e, Some(v))?;
        report.reindexed += 1;
    }
    Ok(report)
}
