//! Running the language model on the graphics card.
//!
//! Aqua's own model server (`vendor/llama.cpp`, built with MinGW) is a program for the processor. The official llama.cpp build for
//! graphics cards (Vulkan; `scripts/fetch_llama_gpu.ps1` installs it in `data/engines/llama-gpu`) can keep a share of the model's
//! layers in video memory, and the card does that share of the work several times faster than the processor. The server fits the
//! share to the free video memory by itself (`-ngl auto`, with a margin left for the desktop); what is left to decide is:
//!
//! * whether there is such a build and a card worth using ([`plan`]);
//! * which card: the discrete one. The integrated graphics share the processor's memory, and offloading to them made writing
//!   slower, not faster;
//! * that a card which fails is not tried again straight away ([`note_failure`]): the model then runs on the processor, as it
//!   always did, for half an hour, then two hours and then eight if it keeps failing (it is tried again: Aqua often runs for days).
//!
//! Nothing changes when the build is not installed: [`plan`] is `None` and the processor build starts as before.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::{Duration, Instant};
use tracing::{info, warn};

/// A graphics device a llama.cpp build can offload to, as `llama-server --list-devices` lists it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Device {
    /// The name the `-dev` option takes ("Vulkan1").
    pub id: String,
    pub name: String,
    pub total_mib: u64,
    pub free_mib: u64,
}

impl Device {
    /// "NVIDIA GeForce GTX 1650 (Vulkan1, 3561 MiB free)".
    pub fn describe(&self) -> String {
        format!("{} ({}, {} MiB free)", self.name, self.id, self.free_mib)
    }
}

/// A card with less memory than this free is left alone. With little room the server puts no layers on the card and still uses it
/// to read prompts (measured: 74 tokens a second against 28 on the processor, for 130 MiB of video memory), so even a card that is
/// nearly full is worth using; below this there is not enough left for that.
pub const MIN_FREE_MIB: u64 = 700;

/// What to start: the GPU build of the server, and the card to put the model on.
#[derive(Debug, Clone)]
pub struct Plan {
    pub binary: PathBuf,
    pub device: Device,
}

/// `AQUA_LLAMA_GPU=0` (or off, false, no) keeps the model on the processor whatever is installed.
pub fn enabled() -> bool {
    !matches!(std::env::var("AQUA_LLAMA_GPU").ok().as_deref().map(str::to_ascii_lowercase).as_deref(), Some("0" | "off" | "false" | "no"))
}

/// The GPU build of `llama-server`: `AQUA_LLAMA_GPU_SERVER_BIN`, an installed engine (`llama-gpu\llama-server.exe` in an engines
/// folder), or `data/engines/llama-gpu` (where `scripts/fetch_llama_gpu.ps1` puts it).
pub fn gpu_server_binary() -> Option<PathBuf> {
    if let Ok(p) = std::env::var("AQUA_LLAMA_GPU_SERVER_BIN") {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(p) = aqua_audio_adapter::engines::installed_engine("llama-gpu/llama-server.exe") {
        return Some(p);
    }
    let in_data = crate::runtime::data_dir().join("engines").join("llama-gpu").join("llama-server.exe");
    in_data.is_file().then_some(in_data)
}

/// Is this a graphics card with memory of its own? Integrated graphics (Intel UHD and Iris Xe, AMD "Radeon(TM) Graphics") share
/// the processor's memory and are left out; the name is all the listing gives.
pub fn is_discrete(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    let words: Vec<&str> = lower.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
    let has = |w: &str| words.contains(&w);
    has("nvidia") || has("geforce") || has("rtx") || has("gtx") || has("quadro") || has("tesla") || has("instinct") || has("arc") || (has("radeon") && (has("rx") || has("pro") || has("vii")))
}

/// The devices in the text `llama-server --list-devices` prints:
/// `  Vulkan1: NVIDIA GeForce GTX 1650 (4152 MiB, 3561 MiB free)`. Any other line is skipped.
pub fn parse_devices(listing: &str) -> Vec<Device> {
    listing
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            let (id, rest) = line.split_once(':')?;
            if id.is_empty() || id.contains(' ') {
                return None;
            }
            let open = rest.rfind(" (")?;
            let name = rest[..open].trim().to_string();
            let numbers = rest[open + 2..].trim_end_matches(')');
            let mut parts = numbers.split(',');
            let total_mib = parts.next()?.trim().strip_suffix("MiB")?.trim().parse().ok()?;
            let free_mib = parts.next()?.trim().strip_suffix("MiB free")?.trim().parse().ok()?;
            (!name.is_empty()).then(|| Device { id: id.to_string(), name, total_mib, free_mib })
        })
        .collect()
}

/// The card to use: the discrete one with the most free memory, and at least [`MIN_FREE_MIB`] of it.
pub fn pick(devices: &[Device]) -> Option<&Device> {
    devices.iter().filter(|d| is_discrete(&d.name) && d.free_mib >= MIN_FREE_MIB).max_by_key(|d| d.free_mib)
}

/// Ask a build which devices it can see. A listing that does not come back within 20 seconds is no listing.
fn list_devices(binary: &Path) -> Vec<Device> {
    parse_devices(&text_of(binary, "--list-devices", 20))
}

/// What a build prints (on either pipe) when it is run with one option such as `--list-devices` or `--help`, which load no model. Empty when it cannot be started or
/// does not finish within `seconds` (it is then stopped).
pub(crate) fn text_of(binary: &Path, option: &str, seconds: u64) -> String {
    let mut cmd = Command::new(binary);
    cmd.arg(option).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    let Ok(mut child) = cmd.spawn() else { return String::new() };
    // Both pipes are read while the program runs, so it can never block on a full pipe; the build prints the list on either.
    let stdout = child.stdout.take().map(|mut p| std::thread::spawn(move || read_all(&mut p)));
    let stderr = child.stderr.take().map(|mut p| std::thread::spawn(move || read_all(&mut p)));
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
        }
    }
    let mut text = String::new();
    for reader in [stdout, stderr].into_iter().flatten() {
        text.push_str(&reader.join().unwrap_or_default());
        text.push('\n');
    }
    text
}

/// Everything the pipe gives. A byte that is not UTF-8 becomes a replacement character instead of throwing the whole text away.
fn read_all(pipe: &mut impl Read) -> String {
    let mut bytes = Vec::new();
    let _ = pipe.read_to_end(&mut bytes);
    String::from_utf8_lossy(&bytes).into_owned()
}

/// How much video memory the server keeps free (`--fit-target`, in MiB), by the size of the card. The server fits the model to the
/// free memory the Vulkan driver reports and keeps this much of it back; its default of 1 GiB measured on this laptop's 4 GB card
/// (the driver says 3561 MiB free where the card has about 2850) left only 300 MiB free after the model loaded. The desktop and the
/// speech engine, which also uses the card, need room, and a card that runs out makes everything stutter. 40 % of the card, at least
/// 1 GiB and at most 1700 MiB.
pub fn fit_target_mib(total_mib: u64) -> u64 {
    (total_mib * 40 / 100).clamp(1024, 1700)
}

/// The options a server on the card is started with, beyond the card itself (`-dev`).
pub fn server_flags(device: &Device) -> Vec<String> {
    vec!["--fit-target".to_string(), fit_target_mib(device.total_mib).to_string()]
}

/// What to start, or `None` to run the model on the processor: there is a GPU build, a card worth using (`AQUA_LLAMA_DEVICE`
/// names one by its id, "Vulkan1", to override the choice), and no recent failure of the card.
pub fn plan() -> Option<Plan> {
    if !enabled() || cooling_down() {
        return None;
    }
    let binary = gpu_server_binary()?;
    let devices = list_devices(&binary);
    let device = match std::env::var("AQUA_LLAMA_DEVICE").ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        Some(id) => devices.iter().find(|d| d.id.eq_ignore_ascii_case(&id)).cloned().unwrap_or(Device { id: id.clone(), name: id, total_mib: 0, free_mib: 0 }),
        None => pick(&devices).cloned().or_else(|| {
            info!("the GPU build is installed but no graphics card with enough free memory was found ({} device(s) seen); the model runs on the processor", devices.len());
            None
        })?,
    };
    Some(Plan { binary, device })
}

/// Unix seconds before which the card is not tried.
static OFF_UNTIL: AtomicU64 = AtomicU64::new(0);
static FAILURES: AtomicU32 = AtomicU32::new(0);
/// What the last failure said (for [`off_status`]).
static LAST_REASON: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());

/// The card is being left alone: why, and until when. What the status route shows, so that a model that runs on the processor (three to five times slower) is not a
/// thing only the log knows (card R3-07).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Off {
    /// What went wrong, as it was noted ("prompt reading on the graphics card fell to 13 tokens a second").
    pub reason: String,
    /// Unix seconds at which the card is tried again.
    pub until_unix: u64,
    /// Failures in a row with no answer from the card in between (it decides how long the wait is).
    pub failures: u32,
}

/// `Some` while the card is being left alone after a failure, `None` when it is in use or was never tried.
pub fn off_status() -> Option<Off> {
    let until = OFF_UNTIL.load(Ordering::SeqCst);
    (now_secs() < until).then(|| Off { reason: LAST_REASON.lock().unwrap_or_else(|e| e.into_inner()).clone(), until_unix: until, failures: FAILURES.load(Ordering::Relaxed) })
}

/// How long the card is left alone after its `failures`-th failure with no answer from it in between: half an hour, then two hours, then
/// eight. A card that keeps failing is not tried every half hour (each try is a start of the server that fails after 15 to 40 seconds, and
/// the request waits for it), but it is tried again: a game that was using its memory is closed, a driver recovers. Before, the second
/// failure turned the card off until Aqua was restarted, and a card that failed twice in one evening cost every answer of the next days
/// three to five times the time.
fn cooldown_secs(failures: u32) -> u64 {
    match failures {
        0 | 1 => 30 * 60,
        2 => 2 * 60 * 60,
        _ => 8 * 60 * 60,
    }
}

fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

fn cooling_down() -> bool {
    now_secs() < OFF_UNTIL.load(Ordering::SeqCst)
}

/// The card could not be used (the server would not load the model, or died while it ran on the card, which is what a lost
/// device or an out-of-memory error looks like). The model runs on the processor for half an hour; each failure after that, with no
/// answer from the card in between, doubles and more the wait (see [`cooldown_secs`]).
pub fn note_failure(why: &str) {
    *LAST_REASON.lock().unwrap_or_else(|e| e.into_inner()) = why.chars().take(200).collect();
    let failures = FAILURES.fetch_add(1, Ordering::SeqCst) + 1;
    let wait = cooldown_secs(failures);
    OFF_UNTIL.store(now_secs() + wait, Ordering::SeqCst);
    let again = if failures >= 2 { "failed again" } else { "could not be used" };
    warn!("the GPU {again} ({why}): the model runs on the processor for the next {}", if wait >= 3600 { format!("{} hours", wait / 3600) } else { format!("{} minutes", wait / 60) });
}

/// Reading a prompt on the processor takes about 30 tokens a second on this laptop, on a card with room about 80. When the card's
/// memory is nearly full (another program started using it) reading does not slow down a little: it collapses (measured: 13 tokens a
/// second, with the card at 3917 of 4096 MiB, for a prompt that took 42 s with room to spare). Below this the card is worse than the
/// processor.
pub const SLOWEST_USEFUL_READING: f64 = 25.0;

/// Did the server read this prompt so slowly that the card is doing more harm than good? Judged only on a prompt long enough to
/// measure (a few tokens read in a fraction of a second say nothing).
pub fn reading_is_too_slow(tokens_read: u64, tokens_per_second: f64) -> bool {
    tokens_read >= 300 && tokens_per_second < SLOWEST_USEFUL_READING
}

/// A request was answered on the card: the failures so far were not a pattern.
pub fn note_success() {
    if FAILURES.load(Ordering::Relaxed) != 0 {
        FAILURES.store(0, Ordering::Relaxed);
    }
}

/// Forget every failure (tests).
#[cfg(test)]
pub(crate) fn forget_failures() {
    FAILURES.store(0, Ordering::SeqCst);
    OFF_UNTIL.store(0, Ordering::SeqCst);
    LAST_REASON.lock().unwrap_or_else(|e| e.into_inner()).clear();
}

#[cfg(test)]
pub(crate) fn is_off() -> bool {
    cooling_down()
}

/// The failure memory is process-wide, so the tests that change it, or that start a server and so read it, take turns.
#[cfg(test)]
pub(crate) static STATE_TESTS: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    /// The listing this machine's Vulkan build printed (llama.cpp b11371).
    const LISTING: &str = "Available devices:\n  Vulkan0: Intel(R) Iris(R) Xe Graphics (12179 MiB, 11411 MiB free)\n  Vulkan1: NVIDIA GeForce GTX 1650 (4152 MiB, 3561 MiB free)\n";

    #[test]
    fn the_device_listing_is_read_line_by_line() {
        let devices = parse_devices(LISTING);
        assert_eq!(
            devices,
            vec![
                Device { id: "Vulkan0".into(), name: "Intel(R) Iris(R) Xe Graphics".into(), total_mib: 12179, free_mib: 11411 },
                Device { id: "Vulkan1".into(), name: "NVIDIA GeForce GTX 1650".into(), total_mib: 4152, free_mib: 3561 },
            ]
        );
        assert_eq!(devices[1].describe(), "NVIDIA GeForce GTX 1650 (Vulkan1, 3561 MiB free)");
    }

    #[test]
    fn lines_that_are_not_devices_are_skipped() {
        let noisy = "ggml_vulkan: Found 2 Vulkan devices:\nwarning: something odd (really)\nAvailable devices:\n\n  CUDA0: NVIDIA GeForce RTX 3060 (12288 MiB, 11000 MiB free)\nnot a device: (MiB)\n";
        let devices = parse_devices(noisy);
        assert_eq!(devices.len(), 1, "{devices:?}");
        assert_eq!(devices[0].id, "CUDA0");
        assert!(parse_devices("").is_empty());
        assert!(parse_devices("Available devices:\n").is_empty());
        assert!(parse_devices("error: no devices found").is_empty());
    }

    #[test]
    fn only_cards_with_memory_of_their_own_count() {
        for card in ["NVIDIA GeForce GTX 1650", "NVIDIA GeForce RTX 4090", "NVIDIA Quadro T1000", "AMD Radeon RX 7600", "AMD Radeon Pro W6800", "Intel(R) Arc(TM) A770 Graphics", "NVIDIA GeForce MX450"] {
            assert!(is_discrete(card), "{card}");
        }
        for shared in ["Intel(R) Iris(R) Xe Graphics", "Intel(R) UHD Graphics 620", "AMD Radeon(TM) Graphics", "Microsoft Basic Render Driver", "llvmpipe (LLVM 15.0.7, 256 bits)"] {
            assert!(!is_discrete(shared), "{shared}");
        }
    }

    #[test]
    fn the_card_with_the_most_free_memory_wins_and_a_nearly_full_one_is_left_alone() {
        let devices = parse_devices(LISTING);
        assert_eq!(pick(&devices).map(|d| d.id.as_str()), Some("Vulkan1"), "not the integrated graphics, even with more memory");
        let two = parse_devices("  Vulkan0: NVIDIA GeForce GTX 1650 (4152 MiB, 2000 MiB free)\n  Vulkan1: NVIDIA GeForce RTX 3060 (12288 MiB, 9000 MiB free)\n");
        assert_eq!(pick(&two).map(|d| d.id.as_str()), Some("Vulkan1"));
        let little = parse_devices("  Vulkan0: NVIDIA GeForce GTX 1650 (4152 MiB, 900 MiB free)\n");
        assert_eq!(pick(&little).map(|d| d.id.as_str()), Some("Vulkan0"), "little room still lets the card read prompts");
        let full = parse_devices("  Vulkan0: NVIDIA GeForce GTX 1650 (4152 MiB, 500 MiB free)\n");
        assert_eq!(pick(&full), None, "less than {MIN_FREE_MIB} MiB free is not worth it");
        assert_eq!(pick(&[]), None);
        assert_eq!(pick(&parse_devices("  Vulkan0: Intel(R) Iris(R) Xe Graphics (12179 MiB, 11411 MiB free)\n")), None, "integrated graphics alone: processor");
    }

    #[test]
    fn a_card_that_reads_slower_than_the_processor_is_recognised_but_a_short_prompt_proves_nothing() {
        assert!(reading_is_too_slow(3415, 13.4), "the measured collapse");
        assert!(!reading_is_too_slow(3415, 81.0), "a healthy card");
        assert!(!reading_is_too_slow(3415, 28.0), "as fast as the processor: fine");
        assert!(!reading_is_too_slow(40, 3.0), "40 tokens is too few to judge by");
        assert!(reading_is_too_slow(300, 24.9) && !reading_is_too_slow(299, 1.0));
    }

    #[test]
    fn the_margin_left_on_the_card_grows_with_the_card_up_to_a_limit() {
        assert_eq!(fit_target_mib(4152), 1660, "this laptop's card: 40 % of it");
        assert_eq!(fit_target_mib(2048), 1024, "never less than the server's own default");
        assert_eq!(fit_target_mib(8192), 1700, "and never more than 1700 MiB");
        assert_eq!(fit_target_mib(24576), 1700);
        assert_eq!(fit_target_mib(0), 1024, "a card whose size is not known");
        let device = Device { id: "Vulkan1".into(), name: "NVIDIA GeForce GTX 1650".into(), total_mib: 4152, free_mib: 3561 };
        assert_eq!(server_flags(&device), ["--fit-target", "1660"]);
    }


    #[test]
    fn a_failed_card_is_left_alone_for_a_while_and_each_failure_after_that_waits_longer_but_never_for_good() {
        let _turn = STATE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        forget_failures();
        assert!(!is_off());
        let wait_set = || OFF_UNTIL.load(Ordering::SeqCst).saturating_sub(now_secs());
        note_failure("the server would not load the model");
        assert!(is_off(), "off for half an hour");
        assert!((30 * 60 - 5..=30 * 60).contains(&wait_set()), "{}", wait_set());
        // The half hour passes (the clock is not touched: the deadline is moved into the past).
        OFF_UNTIL.store(1, Ordering::SeqCst);
        assert!(!is_off(), "tried again after the cool-down");
        note_failure("it died again");
        assert!(is_off());
        assert!((2 * 3600 - 5..=2 * 3600).contains(&wait_set()), "two hours, and not for the rest of the run: {}", wait_set());
        OFF_UNTIL.store(1, Ordering::SeqCst);
        assert!(!is_off(), "tried again after two hours");
        note_failure("and again");
        assert!((8 * 3600 - 5..=8 * 3600).contains(&wait_set()), "{}", wait_set());
        OFF_UNTIL.store(1, Ordering::SeqCst);
        note_failure("and again");
        assert!(wait_set() <= 8 * 3600, "eight hours is the longest: {}", wait_set());
        forget_failures();
    }

    #[test]
    fn the_status_says_why_the_card_is_left_alone_and_until_when_and_says_nothing_otherwise() {
        let _turn = STATE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        forget_failures();
        assert_eq!(off_status(), None, "never tried, nothing to say");
        let before = now_secs();
        note_failure("prompt reading on the graphics card fell to 13 tokens a second");
        let off = off_status().expect("left alone now");
        assert_eq!((off.failures, off.reason.as_str()), (1, "prompt reading on the graphics card fell to 13 tokens a second"));
        assert!(off.until_unix >= before + 30 * 60 - 2 && off.until_unix <= now_secs() + 30 * 60, "half an hour: {}", off.until_unix - before);
        // A second failure, and a very long reason that is cut.
        OFF_UNTIL.store(1, Ordering::SeqCst);
        note_failure(&"x".repeat(500));
        let again = off_status().unwrap();
        assert_eq!((again.failures, again.reason.chars().count()), (2, 200));
        assert!(again.until_unix > off.until_unix, "the second wait is longer");
        // The time is over: the card is tried again and the status says nothing.
        OFF_UNTIL.store(1, Ordering::SeqCst);
        assert_eq!(off_status(), None);
        let text = serde_json::to_string(&Off { reason: "r".into(), until_unix: 5, failures: 2 }).unwrap();
        assert_eq!(text, r#"{"reason":"r","until_unix":5,"failures":2}"#);
        forget_failures();
    }

    #[test]
    fn a_request_answered_on_the_card_clears_the_count_so_a_one_off_is_forgiven() {
        let _turn = STATE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        forget_failures();
        note_failure("one-off");
        OFF_UNTIL.store(1, Ordering::SeqCst); // the half hour is over
        note_success(); // the card answered a request
        note_failure("a month later");
        assert!(OFF_UNTIL.load(Ordering::SeqCst) - now_secs() <= 30 * 60, "the second failure was far enough from the first to count as a first one");
        forget_failures();
    }

    #[test]
    fn it_can_be_turned_off_and_a_missing_build_means_the_processor() {
        let _turn = STATE_TESTS.lock().unwrap_or_else(|e| e.into_inner());
        std::env::set_var("AQUA_LLAMA_GPU", "off");
        assert!(!enabled());
        assert!(plan().is_none());
        std::env::set_var("AQUA_LLAMA_GPU", "1");
        assert!(enabled());
        std::env::remove_var("AQUA_LLAMA_GPU");
        std::env::set_var("AQUA_LLAMA_GPU_SERVER_BIN", "C:/definitely/not/llama-server.exe");
        let found = gpu_server_binary();
        std::env::remove_var("AQUA_LLAMA_GPU_SERVER_BIN");
        // Either nothing is installed, or the one that is installed (never the path that does not exist).
        assert!(found.as_ref().is_none_or(|p| p.is_file()), "{found:?}");
    }
}
