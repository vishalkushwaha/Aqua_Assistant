use anyhow::{Context, Result};
use hound::{SampleFormat, WavReader};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};
use tracing::{info, warn};

/// Official release assets (nazdridoy/kokoro-tts README — Download Model Files).
pub const MODEL_FILENAME: &str = "kokoro-v1.0.onnx";
pub const VOICES_FILENAME: &str = "voices-v1.0.bin";

const LEGACY_MODEL: &str = "kokoro-v0_19.onnx";
const LEGACY_VOICES: &str = "voices.bin";

pub struct KokoroCli {
    pub program: PathBuf,
    pub prefix_args: Vec<String>,
}

fn vendor_root() -> PathBuf {
    // `AQUA_KOKORO_ROOT`, then an installed `engines/kokoro`, then the development checkout.
    if let Some(dir) = std::env::var_os("AQUA_KOKORO_ROOT").map(PathBuf::from).filter(|p| p.is_dir()) {
        return dir;
    }
    if let Some(dir) = crate::engines::installed_engine_dir("kokoro") {
        return dir;
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../vendor/kokoro-tts")
}

pub fn resolve_model_file(model_dir: &Path) -> Option<PathBuf> {
    for name in [MODEL_FILENAME, LEGACY_MODEL] {
        let p = model_dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn resolve_voices_file(model_dir: &Path) -> Option<PathBuf> {
    for name in [VOICES_FILENAME, LEGACY_VOICES] {
        let p = model_dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

pub fn model_bundle_valid(model_dir: &Path) -> bool {
    resolve_model_file(model_dir).is_some() && resolve_voices_file(model_dir).is_some()
}

pub fn resolve_cli() -> Option<KokoroCli> {
    if let Ok(p) = std::env::var("AQUA_KOKORO_CLI_BIN") {
        let path = PathBuf::from(&p);
        if path.is_file() {
            return Some(KokoroCli {
                program: path,
                prefix_args: Vec::new(),
            });
        }
    }

    let vendor = vendor_root();
    let candidates: [(PathBuf, Vec<String>); 5] = [
        (
            vendor.join(".venv/Scripts/kokoro-tts.exe"),
            Vec::new(),
        ),
        (
            vendor.join(".venv/bin/kokoro-tts"),
            Vec::new(),
        ),
        (
            vendor.join(".venv/Scripts/python.exe"),
            vec!["-m".into(), "kokoro_tts".into()],
        ),
        (
            vendor.join(".venv/bin/python"),
            vec!["-m".into(), "kokoro_tts".into()],
        ),
        // An installed copy carries its own Python.
        (
            vendor.join("python/python.exe"),
            vec!["-m".into(), "kokoro_tts".into()],
        ),
    ];

    for (program, prefix_args) in candidates {
        if program.is_file() {
            return Some(KokoroCli {
                program,
                prefix_args,
            });
        }
    }
    None
}

pub fn cli_available() -> bool {
    resolve_cli().is_some()
}

/// How long to wait for one daemon response. Short lines stay near 45 s; longer text gets more
/// time so a passage of a few hundred characters is not abandoned and redone.
fn daemon_read_timeout(text: &str) -> std::time::Duration {
    let chars = text.chars().count() as u64;
    let secs = 45u64.saturating_add(chars / 8).min(180);
    std::time::Duration::from_secs(secs)
}

fn try_synthesize_daemon(text: &str, voice: &str, lang: &str) -> Option<Vec<u8>> {
    use std::io::{Read, Write};
    use std::net::TcpStream;
    use std::time::Duration;

    let payload = serde_json::json!({
        "text": text,
        "voice": voice,
        "lang": lang,
        "speed": 1.0
    }).to_string();

    let addr = "127.0.0.1:8766".parse().ok()?;
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    // The daemon writes the WAV only when the whole request is done, so a fixed 30 s read timeout
    // looks like a dead socket on a long passage and the same text is synthesized again by the CLI.
    // A daemon that is not there still fails the 300 ms connect above.
    let read_secs = daemon_read_timeout(text).as_secs();
    stream.set_read_timeout(Some(Duration::from_secs(read_secs))).ok()?;
    stream.set_write_timeout(Some(Duration::from_secs(3))).ok()?;

    let req = format!(
        "POST /synthesize HTTP/1.1\r\nHost: 127.0.0.1:8766\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        payload.len(),
        payload
    );

    if stream.write_all(req.as_bytes()).is_err() {
        return None;
    }

    let mut response_bytes = Vec::new();
    if stream.read_to_end(&mut response_bytes).is_err() {
        return None;
    }

    // Split headers and binary body at \r\n\r\n
    let header_end = response_bytes.windows(4).position(|w| w == b"\r\n\r\n")?;
    let status_line = std::str::from_utf8(&response_bytes[..header_end]).ok()?;
    if !status_line.contains("200 OK") {
        return None;
    }

    let body = response_bytes[header_end + 4..].to_vec();
    if body.is_empty() {
        None
    } else {
        Some(body)
    }
}

pub fn synthesize_to_wav(
    cli: &KokoroCli,
    model: &Path,
    voices: &Path,
    text: &str,
    voice: &str,
    lang: &str,
) -> Result<Vec<u8>> {
    // 1. Fast Path: Use in-memory persistent Kokoro daemon if available
    if let Some(wav_bytes) = try_synthesize_daemon(text, voice, lang) {
        info!("Synthesized WAV via persistent Kokoro daemon ({} bytes)", wav_bytes.len());
        return Ok(wav_bytes);
    }

    // 2. Fallback Path: Spawning CLI process with unique temporary files
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let tmp = std::env::temp_dir();
    let txt_path = tmp.join(format!("aqua_kokoro_{stamp}.txt"));
    let wav_path = tmp.join(format!("aqua_kokoro_{stamp}.wav"));

    fs::write(&txt_path, text).with_context(|| format!("write temp text {:?}", txt_path))?;

    info!(
        "Running Kokoro CLI fallback {:?} model {:?} voice {}",
        cli.program, model, voice
    );

    let mut cmd = Command::new(&cli.program);
    cmd.args(&cli.prefix_args)
        .arg(&txt_path)
        .arg(&wav_path)
        .arg("--model")
        .arg(model)
        .arg("--voices")
        .arg(voices)
        .arg("--voice")
        .arg(voice)
        .arg("--lang")
        .arg(lang)
        .arg("--speed")
        .arg("1.0")
        .arg("--format")
        .arg("wav");

    let ran = crate::process::run_with_deadline(cmd, None, std::time::Duration::from_secs(120));
    // The text of the reply goes away whether or not the program ran.
    let _ = fs::remove_file(&txt_path);
    let output = ran.with_context(|| format!("Kokoro CLI at {}", cli.program.display())).map_err(|e| {
        let _ = fs::remove_file(&wav_path);
        e
    })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let _ = fs::remove_file(&wav_path);
        anyhow::bail!("kokoro-tts failed: {stderr}{stdout}");
    }

    let wav_bytes = fs::read(&wav_path).with_context(|| format!("read output {:?}", wav_path))?;
    let _ = fs::remove_file(&wav_path);
    Ok(wav_bytes)
}

pub fn wav_sample_rate(wav: &[u8]) -> Option<u32> {
    let cursor = std::io::Cursor::new(wav);
    WavReader::new(cursor)
        .ok()
        .map(|r| r.spec().sample_rate)
}

pub fn wav_bytes_to_f32_mono(wav: &[u8]) -> Result<(Vec<f32>, u32)> {
    let cursor = std::io::Cursor::new(wav);
    let mut reader = WavReader::new(cursor).context("parse kokoro wav")?;
    let spec = reader.spec();
    let sample_rate = spec.sample_rate;

    let samples: Vec<f32> = match spec.sample_format {
        SampleFormat::Float => reader
            .samples::<f32>()
            .collect::<Result<Vec<_>, _>>()
            .context("read float wav")?,
        SampleFormat::Int => reader
            .samples::<i16>()
            .map(|s| s.map(|v| v as f32 / i16::MAX as f32))
            .collect::<Result<Vec<_>, _>>()
            .context("read int16 wav")?,
    };

    Ok((samples, sample_rate))
}

pub fn warn_if_cli_missing() {
    if !cli_available() {
        warn!(
            "kokoro-tts CLI not installed. Run scripts/setup_kokoro_cli.ps1 (see data/models/kokoro/README.md)"
        );
    }
}
