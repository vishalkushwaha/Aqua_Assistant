use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// Official CLI from `vendor/deepfilternet_rust` (libDF `deep-filter` binary).
pub fn default_cli_binary() -> PathBuf {
    let raw = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../vendor/deepfilternet_rust/target/release/deep-filter.exe");
    if let Ok(p) = std::fs::canonicalize(&raw) {
        let p_str = p.to_string_lossy();
        if p_str.starts_with(r"\\?\") {
            return PathBuf::from(&p_str[4..]);
        }
        return p;
    }
    raw
}

pub fn resolve_cli_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AQUA_DEEP_FILTER_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(p) = crate::engines::installed_engine("deep-filter/deep-filter.exe") {
        return Some(p);
    }
    let candidate = default_cli_binary();
    if candidate.is_file() {
        return Some(candidate);
    }
    let alt = candidate.with_extension("");
    if alt.is_file() {
        return Some(alt);
    }
    None
}

pub fn model_tarball(model_dir: &Path) -> PathBuf {
    if model_dir.is_file() {
        return model_dir.to_path_buf();
    }
    let candidates = [
        model_dir.join("DeepFilterNet3_onnx.tar.gz"),
        model_dir.parent().unwrap_or(model_dir).join("DeepFilterNet3_onnx.tar.gz"),
    ];
    for c in candidates {
        if c.is_file() {
            return c;
        }
    }
    model_dir.join("DeepFilterNet3_onnx.tar.gz")
}

pub fn extracted_bundle_dir(model_dir: &Path) -> PathBuf {
    model_dir.join("DeepFilterNet3_onnx")
}

/// Upstream tarball unpacks to `DeepFilterNet3_onnx/tmp/export/*.onnx`; we also support flattened layout.
pub fn enc_onnx_path(model_dir: &Path) -> Option<PathBuf> {
    let base = extracted_bundle_dir(model_dir);
    let flat = base.join("enc.onnx");
    if flat.is_file() {
        return Some(flat);
    }
    let nested = base.join("tmp/export/enc.onnx");
    if nested.is_file() {
        return Some(nested);
    }
    None
}

pub fn model_bundle_valid(model_dir: &Path) -> bool {
    model_tarball(model_dir).is_file() || enc_onnx_path(model_dir).is_some()
}

/// Run upstream `deep-filter` on one WAV (48 kHz mono preferred).
pub fn enhance_wav(cli: &Path, tar_model: &Path, input_wav: &Path, output_dir: &Path) -> Result<()> {
    enhance_wav_with_attenuation(cli, tar_model, input_wav, output_dir, 100.0)
}

pub fn enhance_wav_with_attenuation(
    cli: &Path,
    tar_model: &Path,
    input_wav: &Path,
    output_dir: &Path,
    atten_lim_db: f32,
) -> Result<()> {
    std::fs::create_dir_all(output_dir)?;
    info!(
        "Running deep-filter {:?} with model {:?} atten_lim={}dB",
        cli, tar_model, atten_lim_db
    );
    let atten_str = atten_lim_db.round() as u32;
    let mut child = Command::new(cli)
        .arg(input_wav)
        .arg("-m")
        .arg(tar_model)
        .arg("-o")
        .arg(output_dir)
        .arg("-D")
        .arg("--pf")
        .arg("--pf-beta")
        .arg("0.15")
        .arg("--min-db-thresh")
        .arg("-20")
        .arg("--max-db-erb-thresh")
        .arg("30")
        .arg("--max-db-df-thresh")
        .arg("30")
        .arg("--atten-lim-db")
        .arg(atten_str.to_string())
        .spawn()
        .with_context(|| format!("spawn deep-filter at {}", cli.display()))?;

    let timeout = std::time::Duration::from_secs(60);
    let start = std::time::Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!("deep-filter timed out after {}s", timeout.as_secs());
            }
            Ok(None) => std::thread::sleep(std::time::Duration::from_millis(50)),
            Err(e) => anyhow::bail!("deep-filter wait failed: {e}"),
        }
    };

    if !status.success() {
        anyhow::bail!("deep-filter exited with {status}");
    }
    Ok(())
}

/// Upstream `deep-filter` writes `<input_basename>.wav` into `-o` directory.
pub fn enhanced_output_wav(output_dir: &Path, input_wav: &Path) -> PathBuf {
    let name = input_wav
        .file_name()
        .map(|n| n.to_owned())
        .unwrap_or_else(|| std::ffi::OsStr::new("enhanced.wav").to_owned());
    output_dir.join(name)
}

pub fn cli_available() -> bool {
    resolve_cli_binary().is_some()
}

pub fn warn_if_cli_missing() {
    if !cli_available() {
        warn!(
            "deep-filter CLI not built. Run scripts/build_deepfilter_cli.ps1 (see data/models/deepfilternet/README.md)"
        );
    }
}
