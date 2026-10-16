pub mod document_engine;

pub use document_engine::{DocumentSummary, UniversalDocumentEngine};

pub fn status() -> &'static str {
    "Document Reader Ready"
}
