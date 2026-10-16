use anyhow::{Context, Result};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::process::Command;
use tracing::{info, warn};

/// Preferred default when present under `data/models/llm/`.
pub const MODEL_FILENAME: &str = "qwen-0.5b-instruct-q4_k_m.gguf";

#[derive(Debug, Clone, Serialize)]
pub struct GgufModelInfo {
    pub filename: String,
    pub display_name: String,
    pub size_bytes: u64,
    pub active: bool,
    /// What the file's header and the person's profile say about it: family, layers, trained context, quantisation, what a window of this size costs (card OA9-03). Only the engine's
    /// own listing fills it (reading a header is not free); left out of the answer when it is not there.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub info: Option<crate::model::profile::ModelFacts>,
}

pub fn default_cli_binary() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../vendor/llama.cpp/build/bin/Release/llama-cli.exe")
}

pub fn resolve_cli_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AQUA_LLAMA_CLI_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(p) = aqua_audio_adapter::engines::installed_engine("llama/llama-cli.exe") {
        return Some(p);
    }
    let candidates = [
        default_cli_binary(),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vendor/llama.cpp/build/bin/llama-cli.exe"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vendor/llama.cpp/build/bin/llama-simple.exe"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vendor/llama.cpp/build/bin/Release/llama-simple.exe"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vendor/llama.cpp/build/bin/Release/llama-cli"),
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../vendor/llama.cpp/build/bin/llama-cli"),
    ];
    for c in candidates {
        if c.is_file() {
            return Some(c);
        }
    }
    None
}

pub fn display_name_from_filename(filename: &str) -> String {
    filename
        .trim_end_matches(".gguf")
        .trim_end_matches(".GGUF")
        .replace('_', "-")
}

/// List all `*.gguf` files in the LLM model directory (sorted by name).
pub fn list_gguf_models(model_dir: &Path) -> Vec<GgufModelInfo> {
    let mut models = Vec::new();
    let Ok(entries) = std::fs::read_dir(model_dir) else {
        return models;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        if !name.to_ascii_lowercase().ends_with(".gguf") {
            continue;
        }
        let size_bytes = entry.metadata().map(|m| m.len()).unwrap_or(0);
        models.push(GgufModelInfo {
            filename: name.to_string(),
            display_name: display_name_from_filename(name),
            size_bytes,
            active: false,
            info: None,
        });
    }
    models.sort_by(|a, b| a.filename.to_ascii_lowercase().cmp(&b.filename.to_ascii_lowercase()));
    models
}

/// The length of the start of a GGUF file: the marker `GGUF`, the version (u32), the number of tensors (u64) and the number of key-value pairs (u64), little-endian.
const GGUF_HEADER_BYTES: usize = 24;

/// Does this file start like a GGUF model file that llama.cpp reads? It checks the marker, a version the loader reads (2 or 3; version 1 is no longer read) and that the counts
/// that follow are not random bytes. It cannot say that the file is a sound model: the loader still parses the rest. What it keeps out is a file that only has the name.
pub fn check_gguf_header(path: &Path) -> anyhow::Result<()> {
    use std::io::Read;
    let mut head = [0u8; GGUF_HEADER_BYTES];
    let mut file = std::fs::File::open(path).map_err(|e| anyhow::anyhow!("could not open {}: {e}", path.display()))?;
    file.read_exact(&mut head).map_err(|_| anyhow::anyhow!("{} is too short to be a GGUF model file", path.display()))?;
    if &head[..4] != b"GGUF" {
        anyhow::bail!("{} is not a GGUF model file: it does not start with the GGUF marker", path.display());
    }
    let version = u32::from_le_bytes([head[4], head[5], head[6], head[7]]);
    if !(2..=3).contains(&version) {
        anyhow::bail!("{} is GGUF version {version}, which the model server does not read (versions 2 and 3 are read)", path.display());
    }
    let word = |at: usize| u64::from_le_bytes([head[at], head[at + 1], head[at + 2], head[at + 3], head[at + 4], head[at + 5], head[at + 6], head[at + 7]]);
    // A model has a few thousand tensors at the most and a few hundred pairs; a count beyond a million is not a count.
    if word(8) > 1_000_000 || word(16) > 1_000_000 {
        anyhow::bail!("{} does not have a sound GGUF header (its counts are out of range)", path.display());
    }
    Ok(())
}

/// Is `source` a file that [`import_gguf`] would take? The checks that need no copy, so that a refused file is refused before the model server is stopped for it.
pub fn check_importable(source: &Path) -> anyhow::Result<()> {
    if !source.is_file() {
        anyhow::bail!("not a file: {}", source.display());
    }
    let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
    if name.is_empty() || name.contains("..") || !name.to_ascii_lowercase().ends_with(".gguf") {
        anyhow::bail!("the file must be a .gguf");
    }
    check_gguf_header(source)
}

/// Copy a `.gguf` into the model folder. The name is the file's own name, so the destination cannot leave that folder. The file must start like a GGUF model file
/// ([`check_gguf_header`]): a text file or a download that stopped, renamed to `.gguf`, is refused here and not handed to the model server's parser.
pub fn import_gguf(model_dir: &Path, source: &Path) -> anyhow::Result<String> {
    check_importable(source)?;
    let name = source.file_name().and_then(|n| n.to_str()).unwrap_or("");
    std::fs::create_dir_all(model_dir)?;
    let dest = model_dir.join(name);
    if source != dest.as_path() {
        std::fs::copy(source, &dest).map_err(|e| anyhow::anyhow!("could not copy into {}: {e}", dest.display()))?;
    }
    Ok(name.to_string())
}

/// Resolve a GGUF under `model_dir` by basename. Rejects path traversal.
pub fn resolve_named_model(model_dir: &Path, filename: &str) -> Option<PathBuf> {
    let name = filename.trim();
    if name.is_empty() || name.contains("..") || name.contains('/') || name.contains('\\') {
        return None;
    }
    if !name.to_ascii_lowercase().ends_with(".gguf") {
        return None;
    }
    let p = model_dir.join(name);
    if p.is_file() {
        Some(p)
    } else {
        None
    }
}

/// Pick the active GGUF: env override → preferred filename → first available.
pub fn resolve_model_file(model_dir: &Path) -> Option<PathBuf> {
    if let Ok(override_name) = std::env::var("AQUA_LLAMA_MODEL") {
        let trimmed = override_name.trim();
        if !trimmed.is_empty() {
            let as_path = PathBuf::from(trimmed);
            if as_path.is_file() {
                return Some(as_path);
            }
            if let Some(p) = resolve_named_model(model_dir, trimmed) {
                return Some(p);
            }
        }
    }

    let preferred = model_dir.join(MODEL_FILENAME);
    if preferred.is_file() {
        return Some(preferred);
    }

    list_gguf_models(model_dir)
        .into_iter()
        .next()
        .map(|m| model_dir.join(m.filename))
}

pub fn model_bundle_valid(model_dir: &Path) -> bool {
    !list_gguf_models(model_dir).is_empty()
}

/// Current `llama-cli` is an interactive chat UI. Answer one turn and exit, with the model's
/// chat template applied and thinking disabled (stdin is also closed by callers).
const CHAT_CLI_ARGS: &[&str] = &["-st", "--reasoning", "off", "--log-disable", "--no-display-prompt"];

/// What the chat CLI prints in place of the middle of a long prompt it echoes.
const ECHO_TRUNCATED: &str = "... (truncated)";

/// Marker of the per-turn stats line the chat CLI prints right after the answer.
const CHAT_CLI_STATS_MARKER: &str = "\n[ Prompt:";

/// The chat `llama-cli` writes: banner, `> <prompt>` echo, the answer, then
/// `[ Prompt: … | Generation: … ]`. Return only the answer; `None` if the output has no stats
/// line (llama-simple / older llama-cli print the bare completion instead).
pub fn extract_chat_cli_answer(raw: &str, prompt: &str) -> Option<String> {
    let raw = raw.replace("\r\n", "\n");
    let stats = raw.rfind(CHAT_CLI_STATS_MARKER)?;
    let head = &raw[..stats];
    let prompt = prompt.trim().replace("\r\n", "\n");
    let echo = format!("> {prompt}");
    let start = match head.rfind(&echo) {
        Some(i) => i + echo.len(),
        // A long prompt is echoed cut short ("The use ... (truncated)"), so the end of it is that marker.
        None if head.contains(ECHO_TRUNCATED) => head.rfind(ECHO_TRUNCATED)? + ECHO_TRUNCATED.len(),
        None => {
            // Echo may be re-wrapped; fall back to the end of the prompt's last line.
            let last_line = prompt.lines().last()?.trim();
            head.rfind(last_line)? + last_line.len()
        }
    };
    Some(head[start..].trim().to_string())
}

/// Streaming counterpart of [`extract_chat_cli_answer`]: swallows the banner and prompt echo,
/// forwards the answer, stops at the stats line. Pass-through when `enabled` is false.
struct ChatCliStreamFilter {
    enabled: bool,
    prompt_tail: String,
    buf: String,
    started: bool,
    emitted_any: bool,
    done: bool,
}

impl ChatCliStreamFilter {
    fn new(prompt: &str, enabled: bool) -> Self {
        Self {
            enabled,
            prompt_tail: prompt.trim().lines().last().unwrap_or("").trim().to_string(),
            buf: String::new(),
            started: false,
            emitted_any: false,
            done: false,
        }
    }

    fn push(&mut self, chunk: &str) -> String {
        if !self.enabled {
            return chunk.to_string();
        }
        if self.done {
            return String::new();
        }
        self.buf.push_str(&chunk.replace('\r', ""));
        if !self.started {
            let Some(echo_at) = self.buf.find("\n> ") else {
                return String::new();
            };
            let found = self.buf[echo_at..]
                .find(&self.prompt_tail)
                .map(|t| t + self.prompt_tail.len())
                .or_else(|| self.buf[echo_at..].find(ECHO_TRUNCATED).map(|t| t + ECHO_TRUNCATED.len()));
            let Some(tail_end) = found else {
                return String::new();
            };
            self.buf.drain(..echo_at + tail_end);
            self.started = true;
        }
        let out = if let Some(i) = self.buf.find(CHAT_CLI_STATS_MARKER) {
            self.done = true;
            let out = self.buf[..i].to_string();
            self.buf.clear();
            out
        } else {
            // Hold back enough to recognise a stats marker split across chunks.
            let mut cut = self.buf.len().saturating_sub(CHAT_CLI_STATS_MARKER.len());
            while !self.buf.is_char_boundary(cut) {
                cut -= 1;
            }
            self.buf.drain(..cut).collect()
        };
        self.trim_first(out)
    }

    fn finish(&mut self) -> String {
        if !self.enabled || self.done {
            return String::new();
        }
        self.done = true;
        let rest = std::mem::take(&mut self.buf);
        if self.started {
            self.trim_first(rest)
        } else {
            // Not the chat CLI format after all — emit what we got rather than nothing.
            rest.trim().to_string()
        }
    }

    fn is_done(&self) -> bool {
        self.done
    }

    fn trim_first(&mut self, s: String) -> String {
        if self.emitted_any {
            return s;
        }
        let t = s.trim_start().to_string();
        if !t.is_empty() {
            self.emitted_any = true;
        }
        t
    }
}

pub fn generate_prompt(cli: &Path, model: &Path, prompt: &str, max_tokens: u32) -> Result<String> {
    generate_prompt_with_progress(cli, model, prompt, max_tokens, agent_llama_timeout(), |_| {})
}

/// Env `AQUA_LLAMA_TIMEOUT_SECS` (default 300 for Qwen3-8B CPU planning: a step reads the prompt at
/// about 35 tokens a second and then writes the answer at about 4, and other programs share the CPU).
pub fn agent_llama_timeout() -> std::time::Duration {
    let secs = std::env::var("AQUA_LLAMA_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(300)
        .clamp(10, 900);
    std::time::Duration::from_secs(secs)
}

pub fn generate_prompt_with_progress(
    cli: &Path,
    model: &Path,
    prompt: &str,
    max_tokens: u32,
    timeout: std::time::Duration,
    mut on_tick: impl FnMut(u64),
) -> Result<String> {
    info!(
        "Running llama binary {:?} with model {:?} (prompt_chars={} max_tokens={} timeout={}s)",
        cli,
        model,
        prompt.len(),
        max_tokens,
        timeout.as_secs()
    );
    let cli_str = cli.to_string_lossy();
    let is_simple = cli_str.contains("llama-simple");

    let threads = std::env::var("AQUA_LLAMA_THREADS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or_else(|| aqua_audio_adapter::cpu::physical_cores().clamp(2, 16))
        .to_string();
    let ngl = std::env::var("AQUA_LLAMA_NGL")
        .unwrap_or_else(|_| "0".into());

    let mut child = if is_simple {
        Command::new(cli)
            .arg("-m")
            .arg(model)
            .arg("-p")
            .arg(prompt)
            .arg("-n")
            .arg(max_tokens.to_string())
            .arg("-t")
            .arg(&threads)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn llama-simple at {}", cli.display()))?
    } else {
        Command::new(cli)
            .arg("-m")
            .arg(model)
            .arg("-p")
            .arg(prompt)
            .arg("-n")
            .arg(max_tokens.to_string())
            .arg("-t")
            .arg(&threads)
            .arg("-ngl")
            .arg(&ngl)
            .args(CHAT_CLI_ARGS)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn llama-cli at {}", cli.display()))?
    };

    // Drain both pipes while the child runs; reading only after exit deadlocks once
    // llama writes more than the OS pipe buffer (~4 KB on Windows).
    fn drain<R: std::io::Read + Send + 'static>(pipe: Option<R>) -> std::thread::JoinHandle<Vec<u8>> {
        std::thread::spawn(move || {
            let mut buf = Vec::new();
            if let Some(mut p) = pipe {
                let _ = p.read_to_end(&mut buf);
            }
            buf
        })
    }
    let stdout_reader = drain(child.stdout.take());
    let stderr_reader = drain(child.stderr.take());

    let start = std::time::Instant::now();
    let mut last_tick = 0u64;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if start.elapsed() > timeout => {
                let _ = child.kill();
                let _ = child.wait();
                anyhow::bail!(
                    "llama timed out after {}s (model={}, prompt was {} chars). Other programs using the CPU slow this down: close heavy ones, prefer a smaller agent GGUF, or raise AQUA_LLAMA_TIMEOUT_SECS.",
                    timeout.as_secs(),
                    model.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                    prompt.len()
                );
            }
            Ok(None) => {
                let secs = start.elapsed().as_secs();
                if secs >= last_tick + 2 {
                    last_tick = secs;
                    on_tick(secs);
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            Err(e) => anyhow::bail!("llama wait failed: {e}"),
        }
    };

    let stdout = stdout_reader.join().unwrap_or_default();
    let stderr = stderr_reader.join().unwrap_or_default();

    if !status.success() {
        let stderr = String::from_utf8_lossy(&stderr);
        anyhow::bail!("llama execution failed: {stderr}");
    }

    let raw_text = String::from_utf8_lossy(&stdout);
    let result_str = match (!is_simple)
        .then(|| extract_chat_cli_answer(&raw_text, prompt))
        .flatten()
    {
        Some(answer) => answer,
        // llama-simple / older llama-cli print the bare completion (plus log lines).
        None => raw_text
            .lines()
            .filter(|l| {
                let line = l.trim();
                !line.starts_with("llama_")
                    && !line.starts_with("main:")
                    && !line.starts_with("sched_reserve:")
                    && !line.starts_with("~llama_")
                    && !line.starts_with("srv")
            })
            .collect::<Vec<_>>()
            .join("\n")
            .trim()
            .to_string(),
    };
    if result_str.is_empty() {
        // Surface the failure; echoing the prompt back looked like a real answer.
        let label = model
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("GGUF");
        let stderr = String::from_utf8_lossy(&stderr);
        let tail: String = stderr.trim().chars().rev().take(400).collect::<Vec<_>>().into_iter().rev().collect();
        anyhow::bail!("llama produced no output (model={label}). stderr tail: {tail}");
    } else {
        Ok(result_str)
    }
}

pub async fn generate_prompt_stream_async(
    cli: &Path,
    model: &Path,
    prompt: &str,
    max_tokens: u32,
    tx: tokio::sync::mpsc::Sender<String>,
) -> Result<()> {
    use tokio::io::AsyncReadExt;
    use std::process::Stdio;

    info!("Streaming async llama binary {:?} with model {:?}", cli, model);
    let cli_str = cli.to_string_lossy();
    let is_simple = cli_str.contains("llama-simple");

    let child = if is_simple {
        tokio::process::Command::new(cli)
            .arg("-m")
            .arg(model)
            .arg("-p")
            .arg(prompt)
            .arg("-n")
            .arg(max_tokens.to_string())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn llama-simple at {}", cli.display()))?
    } else {
        tokio::process::Command::new(cli)
            .arg("-m")
            .arg(model)
            .arg("-p")
            .arg(prompt)
            .arg("-n")
            .arg(max_tokens.to_string())
            .arg("-t")
            .arg(std::env::var("AQUA_LLAMA_THREADS").ok().filter(|s| s.parse::<usize>().is_ok()).unwrap_or_else(|| aqua_audio_adapter::cpu::physical_cores().clamp(2, 16).to_string()))
            .arg("-ngl")
            .arg("0")
            .args(CHAT_CLI_ARGS)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .with_context(|| format!("spawn llama-cli at {}", cli.display()))?
    };

    struct ChildGuard(Option<tokio::process::Child>);
    impl ChildGuard {
        async fn reap(&mut self) {
            if let Some(mut child) = self.0.take() {
                let _ = child.kill().await;
                let _ = child.wait().await;
            }
        }
    }

    let mut child_guard = ChildGuard(Some(child));
    let mut filter = ChatCliStreamFilter::new(prompt, !is_simple);

    let res: Result<()> = async {
        let child_ref = child_guard.0.as_mut().unwrap();
        if let Some(mut stdout) = child_ref.stdout.take() {
            let mut read_buf = [0u8; 128];
            let mut utf8_buffer: Vec<u8> = Vec::new();

            loop {
                tokio::select! {
                    res = stdout.read(&mut read_buf) => {
                        match res {
                            Ok(0) => break,
                            Ok(n) => {
                                utf8_buffer.extend_from_slice(&read_buf[..n]);

                                let valid_up_to = match std::str::from_utf8(&utf8_buffer) {
                                    Ok(s) => s.len(),
                                    Err(e) => e.valid_up_to(),
                                };
                                if valid_up_to > 0 {
                                    let text: String = String::from_utf8_lossy(&utf8_buffer[..valid_up_to]).into_owned();
                                    utf8_buffer.drain(..valid_up_to);
                                    let out = filter.push(&text);
                                    if !out.is_empty() && tx.send(out).await.is_err() {
                                        info!("Client channel closed; stopping llama process.");
                                        return Ok(());
                                    }
                                    if filter.is_done() {
                                        break;
                                    }
                                }
                            }
                            Err(_) => break,
                        }
                    }
                    _ = tx.closed() => {
                        info!("MPSC channel closed by receiver; stopping llama process.");
                        return Ok(());
                    }
                }
            }

            let mut tail = String::new();
            if !utf8_buffer.is_empty() {
                tail = filter.push(&String::from_utf8_lossy(&utf8_buffer));
            }
            tail.push_str(&filter.finish());
            if !tail.is_empty() {
                let _ = tx.send(tail).await;
            }
        }
        Ok(())
    }.await;

    child_guard.reap().await;
    res
}

pub fn warn_if_cli_missing() {
    if resolve_cli_binary().is_none() {
        warn!(
            "llama-cli not built. Run scripts/build_llama_cli.ps1 (see data/models/llm/README.md)"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captured from the vendored llama-cli (b11205) run with CHAT_CLI_ARGS.
    const CHAT_CLI_STDOUT: &str = "\r\n\r\nLoading model... \r\n\r\n▄▄ ▄▄\r\n██ ██\r\n\r\nbuild      : b11205-95887577a\r\nmodel      : qwen.gguf\r\n\r\navailable commands:\r\n  /exit or Ctrl+C     stop or exit\r\n\r\n\r\n\r\n> Say hello in 5 words.\r\n/no_think\r\nHello there, nice to meet!\r\n\r\n[ Prompt: 206.7 t/s | Generation: 18.7 t/s ]\r\n\r\n\r\nExiting...\r\n";
    const PROMPT: &str = "Say hello in 5 words.\n/no_think";

    #[test]
    fn extracts_only_the_answer_from_chat_cli_output() {
        assert_eq!(
            extract_chat_cli_answer(CHAT_CLI_STDOUT, PROMPT).as_deref(),
            Some("Hello there, nice to meet!")
        );
    }

    #[test]
    fn bare_completion_is_not_treated_as_chat_output() {
        assert_eq!(extract_chat_cli_answer("Hello there!\n", PROMPT), None);
    }

    #[test]
    fn stream_filter_emits_only_the_answer_across_tiny_chunks() {
        let mut f = ChatCliStreamFilter::new(PROMPT, true);
        let mut out = String::new();
        let chars: Vec<char> = CHAT_CLI_STDOUT.chars().collect();
        for chunk in chars.chunks(3) {
            out.push_str(&f.push(&chunk.iter().collect::<String>()));
        }
        out.push_str(&f.finish());
        assert_eq!(out.trim_end(), "Hello there, nice to meet!");
    }

    #[test]
    fn stream_filter_passes_through_when_disabled() {
        let mut f = ChatCliStreamFilter::new(PROMPT, false);
        assert_eq!(f.push("raw tokens"), "raw tokens");
        assert_eq!(f.finish(), "");
    }
    use std::fs;

    #[test]
    fn list_and_resolve_named_models() {
        let dir = std::env::temp_dir().join(format!(
            "aqua_llama_models_{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("alpha.gguf"), b"a").unwrap();
        fs::write(dir.join("beta.gguf"), b"b").unwrap();
        fs::write(dir.join("notes.txt"), b"x").unwrap();

        let listed = list_gguf_models(&dir);
        assert_eq!(listed.len(), 2);
        assert_eq!(listed[0].filename, "alpha.gguf");
        assert_eq!(listed[1].filename, "beta.gguf");
        assert!(model_bundle_valid(&dir));

        assert!(resolve_named_model(&dir, "beta.gguf").is_some());
        assert!(resolve_named_model(&dir, "../beta.gguf").is_none());
        assert!(resolve_named_model(&dir, "missing.gguf").is_none());

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_long_prompt_that_the_cli_echoes_cut_short_still_gives_only_the_answer() {
        let long = format!("{}\nThe user said: tell me a joke\n/no_think", "You are Aqua, a private assistant. ".repeat(20));
        let raw = "\r\nLoading model...\r\nbuild : b1\r\n\r\n> You are Aqua, a private assistant. You are Aqua, a private assistant. The use ... (truncated)\r\nWhy did the computer go to the doctor? It had a virus.\r\n\r\n[ Prompt: 29.0 t/s | Generation: 3.2 t/s ]\r\n\r\nExiting...\r\n";
        assert_eq!(extract_chat_cli_answer(raw, &long).as_deref(), Some("Why did the computer go to the doctor? It had a virus."));
        let mut f = ChatCliStreamFilter::new(&long, true);
        let streamed: String = raw.as_bytes().chunks(17).map(|c| f.push(&String::from_utf8_lossy(c))).collect();
        assert!(streamed.contains("Why did the computer go to the doctor?") && !streamed.contains("Loading model") && !streamed.contains("[ Prompt"), "{streamed:?}");
    }
}

#[cfg(test)]
mod import_tests {
    use super::*;
    use crate::test_support::TempWorld;

    fn header(version: u32, tensors: u64, pairs: u64) -> Vec<u8> {
        let mut h = b"GGUF".to_vec();
        h.extend(version.to_le_bytes());
        h.extend(tensors.to_le_bytes());
        h.extend(pairs.to_le_bytes());
        h
    }

    #[test]
    fn a_gguf_is_imported_only_when_it_starts_like_one_and_a_refused_file_leaves_nothing_in_the_model_folder() {
        let world = TempWorld::new("gguf_import");
        let models = world.path("models");
        let good = world.write("in/good.gguf", [header(3, 291, 24), vec![0u8; 64]].concat());
        assert_eq!(import_gguf(&models, &good).unwrap(), "good.gguf");
        assert!(models.join("good.gguf").is_file());
        let v2 = world.write("in/older.GGUF", [header(2, 10, 5), vec![1u8; 8]].concat());
        assert_eq!(import_gguf(&models, &v2).unwrap(), "older.GGUF");
        // A file that only has the name, a download that stopped, a version nobody reads and counts that are random bytes: none is copied.
        for (name, bytes, why) in [
            ("text.gguf", b"this is not a model, only a note".to_vec(), "marker"),
            ("empty.gguf", Vec::new(), "too short"),
            ("short.gguf", b"GGUF\x03\x00\x00\x00".to_vec(), "too short"),
            ("v1.gguf", [header(1, 3, 3), vec![0u8; 8]].concat(), "version 1"),
            ("v99.gguf", [header(99, 3, 3), vec![0u8; 8]].concat(), "version 99"),
            ("counts.gguf", [header(3, u64::MAX, 3), vec![0u8; 8]].concat(), "counts"),
            ("swapped.gguf", { let mut h = header(3, 1, 1); h[..4].copy_from_slice(b"FUGG"); h }, "marker"),
        ] {
            let path = world.write(&format!("in/{name}"), bytes);
            let err = import_gguf(&models, &path).expect_err(name).to_string();
            assert!(err.contains(why), "{name}: {err}");
            assert!(!models.join(name).exists(), "{name} was copied although it was refused");
            assert!(check_importable(&path).is_err(), "{name}: the check that runs before the server is stopped says the same");
        }
        // The old checks still hold.
        assert!(import_gguf(&models, &world.write("in/model.bin", header(3, 1, 1))).unwrap_err().to_string().contains("must be a .gguf"));
        assert!(import_gguf(&models, &world.path("in/missing.gguf")).unwrap_err().to_string().contains("not a file"));
        let kept: Vec<String> = list_gguf_models(&models).into_iter().map(|m| m.filename).collect();
        assert_eq!(kept, vec!["good.gguf".to_string(), "older.GGUF".to_string()]);
    }
}
