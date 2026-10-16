//! What the model server is started with and where it is found: the switch and the idle time, the program, the window, the layers on the card, the threads, the slots, the port, the key and the log (move only: this was part of `llama_server.rs`).

use super::*;

pub fn enabled() -> bool {
    !matches!(std::env::var("AQUA_LLAMA_SERVER").ok().as_deref().map(str::to_ascii_lowercase).as_deref(), Some("0" | "off" | "false" | "no"))
}

/// Seconds without a request after which the server (and the model in RAM) is stopped.
pub fn idle_timeout() -> Duration {
    let secs = std::env::var("AQUA_LLAMA_IDLE_SECS").ok().and_then(|s| s.parse::<u64>().ok()).unwrap_or(7_200).clamp(30, 86_400);
    Duration::from_secs(secs)
}

/// The server for the processor: `AQUA_LLAMA_SERVER_BIN`, an installed engine (`llama\llama-server.exe` in an engines folder, or in
/// `data/engines/llama`, where `scripts/fetch_llama_cpu.ps1` puts the official build, which reads prompts about 22 % faster), or
/// the `llama-server.exe` next to `llama-cli.exe`.
pub fn server_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AQUA_LLAMA_SERVER_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(p) = aqua_audio_adapter::engines::installed_engine("llama/llama-server.exe") {
        return Some(p);
    }
    let in_data = crate::runtime::data_dir().join("engines").join("llama").join("llama-server.exe");
    if in_data.is_file() {
        return Some(in_data);
    }
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let mut candidates = vec![manifest.join("../vendor/llama.cpp/build/bin/llama-server.exe"), manifest.join("../vendor/llama.cpp/build/bin/llama-server")];
    if let Some(cli) = super::llama_cli::resolve_cli_binary() {
        if let Some(dir) = cli.parent() {
            candidates.insert(0, dir.join("llama-server.exe"));
            candidates.insert(1, dir.join("llama-server"));
        }
    }
    candidates.into_iter().find(|c| c.is_file())
}

/// The context window the model server is started with (`AQUA_LLAMA_CTX`, default 6144, at least 2048). This is the total `-c`; the server divides it between the slots.
///
/// It was 2048 until card R6-20. The bake-off of R6-31 ran the planner's own prompts (built by the real planner code, with the real list of tools) through a server on this
/// laptop's GTX 1650: 100 goals, the planner right on 77% at 6144 and 62% at 2048, 15 goals right at 6144 that were wrong at 2048 and none the other way. At 2048 the list of
/// tools is also cut to fit, which is part of why it is less often right.
///
/// **The window alone is not safe on this card** (found by running Aqua's own launch, 2026-10-08): with the same 24 layers on the card as at 2048, a cold read of the same
/// 1,394-token prompt took 97 to 105 s at 6144 (13 to 14 tokens a second, the card holding 3.7 of its 4.1 GB) and 20 s at 2048 (70 tokens a second); the server then gave up on
/// the card for the processor. With 22 layers on the card at 6144 the same read took 19.8 s, so the layers follow the window (see [`default_gpu_layers`]).
/// **Not measured:** how much longer a cold read of the planner's real prompt is at 6144 (it is not cut to fit, so it is longer: a median of 1,420 tokens against 806 in the
/// bake-off, which would be about 8 s at 70 tokens a second) when another request has replaced the cached start of the prompt.
pub fn context_tokens() -> u32 {
    std::env::var("AQUA_LLAMA_CTX").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(DEFAULT_CONTEXT).clamp(2048, 32768)
}

/// The window when `AQUA_LLAMA_CTX` is not set (see [`context_tokens`]).
pub(super) const DEFAULT_CONTEXT: u32 = 6144;

/// How many layers of the model go to the card when `AQUA_LLAMA_NGL` is not set and there is a card: 24 at the window of 2048 (measured on the 4 GB GTX 1650), and one fewer
/// for every 2048 tokens more, because the cache of the window grows with it and takes the room (about 8 MB a layer on the card for each 2048 tokens), never fewer than 12.
/// **Measured on one card only:** at 6144 with 24 layers a cold read ran at 13 to 14 tokens a second (the card full, 3.7 of 4.1 GB); with 22 and with 20 it ran at 70, 55 and
/// 53 tokens a second over three rounds (peak video memory 3,585 MiB with 22, 3,317 with 20 and 3,048 with 18). Other windows follow the same slope and were not run.
pub(crate) fn default_gpu_layers(window: u32) -> u32 {
    24u32.saturating_sub(window.saturating_sub(2048) / 2048).max(12)
}

/// The layers a person's profile asks for on the card for this model (card OA9-03): the `gpu_layers` of the entry of `profiles.json` (in the folder above the model folder) that
/// matches the file, and only with `AQUA_MODEL_PROFILES` on. `None` in every other case, and the launch is then what it has always been. The file is read here and not remembered
/// (the server starts rarely, and a change to the file is then seen without a restart of Aqua). **A plan that is not the measured one (22 layers at a window of 6144 for the 8B on the
/// 4 GB card) has to pass `python scripts/eval/speed_gate.py` with Aqua closed before it is relied on:** the person who writes `gpu_layers` takes that on, and nothing ships with one.
pub(crate) fn profile_gpu_layers(model: &Path) -> Option<u32> {
    if !crate::flags::is_on(&crate::flags::MODEL_PROFILES) {
        return None;
    }
    let name = model.file_name()?.to_str()?;
    let root = model.parent()?.parent()?;
    crate::model::profile::Profiles::load(&root.join(crate::model::profile::FILE)).for_file(name)?.gpu_layers
}

/// The `-ngl` the launch passes: `AQUA_LLAMA_NGL` when it is set, else on a card the profile's layers or the measured default for the window ([`default_gpu_layers`]), else 0 on the
/// processor. With no profile this is what the launch has always passed (a test holds the whole command line of the baseline model to it).
pub(crate) fn layers_for_launch(card: bool, env_layers: Option<String>, window: u32, profile: Option<u32>) -> String {
    match (card, env_layers) {
        (_, Some(n)) => n,
        (true, None) => profile.unwrap_or_else(|| default_gpu_layers(window)).to_string(),
        (false, None) => "0".to_string(),
    }
}

/// Where the model server writes its errors.
pub(super) fn server_log_path() -> PathBuf {
    crate::runtime::data_dir().join("logs").join("llama-server.log")
}

/// The end of the server's log as one line, for an error message (empty when there is none).
pub(in crate::modules::llm_router) fn log_tail(path: &Path) -> String {
    let Ok(text) = std::fs::read_to_string(path) else { return String::new() };
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    let skip = text.chars().count().saturating_sub(300);
    let tail: String = text.chars().skip(skip).collect();
    if tail.is_empty() { String::new() } else { format!(": {tail}") }
}

pub(in crate::modules::llm_router) fn free_port() -> Result<u16> {
    let l = std::net::TcpListener::bind("127.0.0.1:0").context("no free local port")?;
    Ok(l.local_addr()?.port())
}

pub(in crate::modules::llm_router) fn random_key() -> String {
    let mut b = [0u8; 24];
    if getrandom::getrandom(&mut b).is_err() {
        // Not expected; a key derived from the clock is still unguessable enough for a loopback port.
        let n = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(1);
        b.iter_mut().enumerate().for_each(|(i, x)| *x = (n >> (i % 16)) as u8 ^ (i as u8).wrapping_mul(31));
    }
    b.iter().map(|x| format!("{x:02x}")).collect()
}

/// Cache slots in the model server (`AQUA_LLAMA_SLOTS`, default 1). The server picks, for each request, the slot whose
/// cached prompt it shares most with. One slot is the voice default; set this to 2 only together with a larger `AQUA_LLAMA_CTX`.
pub(crate) fn slots() -> u32 {
    std::env::var("AQUA_LLAMA_SLOTS").ok().and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).clamp(1, 4)
}

/// The thread count that was measured to be the fastest, and the most that is used however many processors there are. Measured on one laptop (4 physical
/// cores, 8 logical processors) with Qwen3-8B Q4_K_M partly on the GTX 1650: 4 threads generated at 6.6 tokens a second and 6 threads at 7.3, with 8 threads
/// no faster than 6. It has not been measured on any other machine.
pub(super) const MEASURED_THREADS: usize = 6;

/// Threads for a machine with `logical` processors: the measured count, but never more than the machine has (more threads than processors only make each
/// other wait, and a two-processor machine must not be given six) and never fewer than two.
pub(super) fn threads_for(logical: usize) -> usize {
    MEASURED_THREADS.min(logical).max(2)
}

/// Threads for the model on this machine. `AQUA_LLAMA_THREADS` overrides.
pub(in crate::modules::llm_router) fn default_threads() -> usize {
    threads_for(aqua_audio_adapter::cpu::logical_processors())
}

pub(in crate::modules::llm_router) fn threads() -> String {
    std::env::var("AQUA_LLAMA_THREADS").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or_else(default_threads).to_string()
}

/// Threads for reading the prompt (`-tb`), the same as for writing unless `AQUA_LLAMA_THREADS_BATCH` says otherwise.
pub(super) fn threads_batch() -> String {
    std::env::var("AQUA_LLAMA_THREADS_BATCH").ok().and_then(|s| s.parse::<usize>().ok()).unwrap_or_else(|| threads().parse().unwrap_or_else(|_| default_threads())).to_string()
}
