//! Key Management & Encryption Utilities

#[cfg(feature = "unwired")]
pub mod key_rotation;

#[cfg(feature = "unwired")]
pub use key_rotation::{KeyRotationEngine, Vault, VaultPayload, DEFAULT_ITERATIONS};

pub fn status() -> &'static str {
    "Crypto Engine Ready"
}
