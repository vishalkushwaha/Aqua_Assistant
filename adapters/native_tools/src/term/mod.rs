//! Terminal tools: `term.run` (one command, wait for it) and background jobs
//! (`term.start`, `term.output`, `term.stop`, `term.sessions`).
//!
//! There is no shell. A command is a program name plus a list of arguments, started with a
//! scrubbed environment inside a Job Object, with output, time, memory and process-count limits.

pub mod arity;
pub mod custom;
pub mod exit_codes;
pub mod extended;
#[cfg(feature = "git")]
mod journalled;
pub mod rules;
pub mod shim;

use crate::contain::{build_env, Contained, Limits};
use crate::fs::guard::{display, Op, PathGuard};
use crate::sandbox::{self, Mxc, Tier};
use aqua_tool_host::{
    Annotations, Classification, Decision, ErrorCode, ExecCtx, PermClass, ProgressSink, ProviderFuture, ProviderOutput, ToolError, ToolManifest, ToolProvider,
};
pub use rules::{ExeKind, ExecRules, RuleDecision, TermConfig};
use serde_json::{json, Value};
use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

const RING_BYTES: usize = 512 * 1024;
const MAX_LINE_CHARS: usize = 4096;
/// How much of a one-shot `term.run` is kept for the tool host (card OA5-02, TM-02): a quarter at the start and the rest, up to this, at the end. The host shows the model a few thousand
/// characters of it and keeps the whole redacted text in a file (`@out/...`), so the middle of a build log can be searched; an output longer than this loses its middle, not its end.
const RUN_CAPTURE_BYTES: usize = 8 * 1024 * 1024;
/// The most characters one page of `term.output` holds, whatever `max_chars` says: the host shows the model 4,000 characters of any result of this tool, and a page that is longer
/// than that is cut in its middle, which loses lines without telling the cursor (`after_seq`) it did.
const OUTPUT_PAGE_MAX_CHARS: usize = 3_600;
/// What the header line of a page and the line that says where to continue take out of it.
const PAGE_OVERHEAD_CHARS: usize = 140;
/// How long a `term.run` that was moved to a background job (card OA4-03, TM-04) may run in all: the most `term.start` allows.
const PROMOTED_RUN_SECS: u64 = 3600;

thread_local! {
    static CALL_ENDS_AT: std::cell::Cell<Option<Instant>> = const { std::cell::Cell::new(None) };
}

/// The time the host gives the call that the current thread is working for (card OA4-03, TM-04). A provider starts it on the blocking thread that does the call and holds it until the
/// call is over; every program started on that thread by [`run_command`] is then given no more than what is left of it, less a margin (a fifth of the call's time, at most two seconds)
/// so that the program is ended, and the call says what it printed, before the host gives up on the call. Before, `git fetch` was allowed 240 s under a call the host ended at 120 s,
/// and the host's timeout only abandoned it. Nothing is held outside a call: a program started from a test, a job (`term.start`) or the core itself keeps the time it was given.
pub struct CallClock {
    before: Option<Instant>,
}

impl CallClock {
    /// `timeout_ms` is what `ExecCtx::timeout_ms` says; 0 holds nothing.
    pub fn start(timeout_ms: u64) -> CallClock {
        let before = CALL_ENDS_AT.with(|c| c.get());
        if timeout_ms > 0 {
            let margin = (timeout_ms / 5).min(2_000);
            CALL_ENDS_AT.with(|c| c.set(Some(Instant::now() + Duration::from_millis(timeout_ms - margin))));
        }
        CallClock { before }
    }

    /// What is left of the time of the call this thread works for; `None` when it works for none.
    pub fn left() -> Option<Duration> {
        CALL_ENDS_AT.with(|c| c.get()).map(|t| t.saturating_duration_since(Instant::now()))
    }
}

impl Drop for CallClock {
    fn drop(&mut self) {
        CALL_ENDS_AT.with(|c| c.set(self.before));
    }
}

#[derive(Default)]
struct JobState {
    /// (sequence number, is_stderr, text)
    lines: VecDeque<(u64, bool, String)>,
    bytes: usize,
    next_seq: u64,
    /// Stop appending once the cap is reached (one-shot runs) instead of dropping the oldest lines.
    stop_at_cap: bool,
    cap_bytes: usize,
    capped: bool,
    /// A one-shot `term.run` keeps its end (card OA5-02): the first quarter of `cap_bytes` of lines stays in `head`, everything after it goes into `lines`, which is a ring of the other
    /// three quarters. A line pushed out of the ring is counted in `dropped` (lines of stdout, lines of stderr).
    keep_end: bool,
    head: Vec<(u64, bool, String)>,
    head_bytes: usize,
    head_closed: bool,
    dropped: (u64, u64),
    exit_code: Option<i32>,
    done: bool,
    timed_out: bool,
    killed: bool,
    /// What the Job Object saw when the program had ended: a limit of the terminal tools was reached (card OA7-01, TM-11). Not set for a run that was stopped or timed out.
    limit_note: Option<String>,
}

pub(crate) struct Job {
    id: String,
    pid: u32,
    name: String,
    command_line: String,
    cwd: String,
    started: Instant,
    state: Arc<Mutex<JobState>>,
    kill: Arc<AtomicBool>,
}

impl Job {
    fn push(state: &Mutex<JobState>, is_err: bool, text: String) {
        let mut st = state.lock().unwrap_or_else(PoisonError::into_inner);
        if st.keep_end {
            let seq = st.next_seq;
            st.next_seq += 1;
            let head_cap = st.cap_bytes / 4;
            // (A line is counted with its line end, so that the text the lines make is within the cap.)
            if !st.head_closed && st.head_bytes + text.len() + 1 <= head_cap {
                st.head_bytes += text.len() + 1;
                st.head.push((seq, is_err, text));
                return;
            }
            // The first line that does not fit closes the start: a short line that follows it must not be put before it.
            st.head_closed = true;
            st.bytes += text.len() + 1;
            st.lines.push_back((seq, is_err, text));
            let ring_cap = st.cap_bytes - head_cap;
            // The newest line stays even when it alone is over the ring.
            while st.bytes > ring_cap && st.lines.len() > 1 {
                if let Some((_, was_err, t)) = st.lines.pop_front() {
                    st.bytes -= t.len() + 1;
                    if was_err {
                        st.dropped.1 += 1;
                    } else {
                        st.dropped.0 += 1;
                    }
                }
            }
            return;
        }
        if st.capped {
            return;
        }
        st.bytes += text.len();
        if st.stop_at_cap && st.bytes > st.cap_bytes {
            st.capped = true;
            return;
        }
        let seq = st.next_seq;
        st.next_seq += 1;
        st.lines.push_back((seq, is_err, text));
        while st.bytes > st.cap_bytes.max(RING_BYTES) && !st.stop_at_cap {
            if let Some((_, _, t)) = st.lines.pop_front() {
                st.bytes -= t.len();
            } else {
                break;
            }
        }
    }
}

/// Run the program inside an MXC container instead of starting it directly (card R6-35): the container, and what it is granted.
struct Wrap {
    mxc: Arc<Mxc>,
    /// Read-only folders besides the program's own: the working folder, when it may be shared.
    read: Vec<PathBuf>,
    /// The scratch folder.
    write: Vec<PathBuf>,
    /// Aqua's own files, named as denied.
    denied: Vec<PathBuf>,
}

struct Spec {
    /// Run at Low integrity with a stripped token (Windows): it can write only where it was allowed to.
    isolated: bool,
    /// Run in an MXC container (the tier chosen by `contain`); `isolated` is then false.
    wrap: Option<Wrap>,
    exe: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    stdin: Option<String>,
    limits: Limits,
    max_runtime: Duration,
    cap_bytes: usize,
    stop_at_cap: bool,
    /// Keep the end of the output, and only the start and the end (see `JobState::keep_end`).
    keep_end: bool,
    /// A line of output longer than this many characters is kept cut to it. A job that is read page by page keeps [`MAX_LINE_CHARS`]. A one-shot
    /// run is read whole by the tool that started it (`terraform show -json` prints a plan on one line of many thousands of characters, and cut
    /// at 4096 it could not be read as JSON), so it is only held to the cap of the whole run.
    max_line_chars: usize,
    /// Where each line of the output is told as it arrives (card OA7-04, TM-14): the live output of the call. `None` when the setting is off or nobody is shown it.
    progress: Option<ProgressSink>,
}

/// A running child: an ordinary one, or one started at Low integrity.
enum Proc {
    Std(std::process::Child),
    #[cfg(windows)]
    Low(crate::contain_low::LowChild),
}

type Streams = (Option<Box<dyn Write + Send>>, Option<Box<dyn std::io::Read + Send>>, Option<Box<dyn std::io::Read + Send>>);

impl Proc {
    fn id(&self) -> u32 {
        match self {
            Proc::Std(c) => c.id(),
            #[cfg(windows)]
            Proc::Low(c) => c.pid,
        }
    }

    fn streams(&mut self) -> Streams {
        match self {
            Proc::Std(c) => (
                c.stdin.take().map(|s| Box::new(s) as Box<dyn Write + Send>),
                c.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                c.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
            #[cfg(windows)]
            Proc::Low(c) => (
                c.stdin.take().map(|s| Box::new(s) as Box<dyn Write + Send>),
                c.stdout.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
                c.stderr.take().map(|s| Box::new(s) as Box<dyn std::io::Read + Send>),
            ),
        }
    }

    /// `Some(code)` once it has exited (the code itself is `None` when the system gives none).
    fn try_wait(&mut self) -> Result<Option<Option<i32>>, ()> {
        match self {
            Proc::Std(c) => c.try_wait().map(|o| o.map(|s| s.code())).map_err(|_| ()),
            #[cfg(windows)]
            Proc::Low(c) => Ok(c.try_wait().map(Some)),
        }
    }

    fn kill(&mut self) {
        match self {
            Proc::Std(c) => {
                let _ = c.kill();
            }
            #[cfg(windows)]
            Proc::Low(c) => c.kill(),
        }
    }

    fn wait(&mut self) -> Option<Option<i32>> {
        match self {
            Proc::Std(c) => c.wait().ok().map(|s| s.code()),
            #[cfg(windows)]
            Proc::Low(c) => Some(c.wait()),
        }
    }
}

fn spawn(spec: Spec, id: String, name: String, command_line: String) -> Result<Arc<Job>, ToolError> {
    let max_line_chars = spec.max_line_chars;
    // Where a program may run is not decided here. A comparison with the folder the process started in does not belong in this function: the core runs from its own folder (the release build folder)
    // while the person's project is anywhere. The path guard is the one authority: `parse_command` resolves `cwd` against the configured roots, `git` works in a repository the guard accepted,
    // and the scratch folders of docker, ssh and the cloud tools are Aqua's own (OA1-02).

    // In an MXC container the program is started through the launcher, which is given a policy file (card R6-35).
    let wrapped = match &spec.wrap {
        Some(w) => {
            let launch = sandbox::Launch {
                exe: spec.exe.clone(),
                args: spec.args.clone(),
                cwd: spec.cwd.clone(),
                env: spec.env.clone(),
                read_paths: w.read.clone(),
                write_paths: w.write.clone(),
                denied_paths: w.denied.clone(),
                timeout: Some(spec.max_runtime),
            };
            Some(w.mxc.plan(&launch).map_err(|e| ToolError::new(ErrorCode::SandboxViolation, format!("could not set up the MXC container, so {} was not started: {e}", spec.exe.display())))?)
        }
        None => None,
    };
    let (run_exe, run_args, run_cwd, run_env) = match &wrapped {
        Some(w) => (w.exe.clone(), w.args.clone(), w.cwd.clone(), w.env.clone()),
        None => (spec.exe.clone(), spec.args.clone(), spec.cwd.clone(), spec.env.clone()),
    };
    let (mut child, contained) = if spec.isolated {
        #[cfg(windows)]
        {
            let mut low = crate::contain_low::LowChild::spawn(&spec.exe, &spec.args, &spec.cwd, &spec.env, spec.stdin.is_some()).map_err(|e| {
                let code = if e.contains("ERROR_FILE_NOT_FOUND") || e.contains("cannot find") { ErrorCode::NotFound } else { ErrorCode::SandboxViolation };
                ToolError::new(code, format!("could not start {} isolated: {e}", spec.exe.display()))
            })?;
            // The child is still suspended: it is inside the Job Object before it runs anything.
            let contained = Contained::attach_handle(low.raw_process(), spec.limits).map_err(|e| ToolError::new(ErrorCode::SandboxViolation, format!("could not contain the command, so it was not started: {e}")))?;
            low.resume().map_err(|e| ToolError::new(ErrorCode::SandboxViolation, e))?;
            (Proc::Low(low), Some(contained))
        }
        #[cfg(not(windows))]
        {
            return Err(ToolError::new(ErrorCode::UnsupportedPlatform, "isolated runs are only available on Windows"));
        }
    } else {
        let mut cmd = Command::new(&run_exe);
        cmd.args(&run_args).current_dir(&run_cwd).env_clear().envs(run_env.iter().cloned());
        cmd.stdin(if spec.stdin.is_some() { Stdio::piped() } else { Stdio::null() }).stdout(Stdio::piped()).stderr(Stdio::piped());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
        }
        let mut child = cmd.spawn().map_err(|e| {
            let code = if e.kind() == std::io::ErrorKind::NotFound { ErrorCode::NotFound } else { ErrorCode::BackendError };
            ToolError::new(code, format!("could not start {}: {e}", run_exe.display()))
        })?;
        let contained = match Contained::attach(&child, spec.limits) {
            Ok(c) => Some(c),
            Err(e) => {
                // A command that cannot be contained does not run.
                let _ = child.kill();
                let _ = child.wait();
                return Err(ToolError::new(ErrorCode::SandboxViolation, format!("could not contain the command, so it was stopped: {e}")));
            }
        };
        (Proc::Std(child), contained)
    };
    let cleanup = wrapped.map(|w| w.cleanup);
    let (stdin_pipe, stdout_pipe, stderr_pipe) = child.streams();
    let state = Arc::new(Mutex::new(JobState { stop_at_cap: spec.stop_at_cap, cap_bytes: spec.cap_bytes, keep_end: spec.keep_end, ..Default::default() }));
    let kill = Arc::new(AtomicBool::new(false));
    let job = Arc::new(Job {
        id,
        pid: child.id(),
        name,
        command_line,
        cwd: display(&spec.cwd),
        started: Instant::now(),
        state: state.clone(),
        kill: kill.clone(),
    });

    let mut readers = Vec::new();
    for (stream, is_err) in [(stdout_pipe, false), (stderr_pipe, true)] {
        if let Some(s) = stream {
            let st = state.clone();
            let max_line = max_line_chars;
            let live = spec.progress.clone();
            readers.push(std::thread::spawn(move || {
                let mut r = BufReader::new(s);
                let mut buf = Vec::new();
                loop {
                    buf.clear();
                    // At most four bytes a character are kept of a line; the rest of a longer one is read and thrown away, so a program that
                    // never ends a line cannot make this buffer grow until the machine is out of memory.
                    match read_line_bounded(&mut r, &mut buf, max_line.saturating_mul(4)) {
                        Ok((false, _)) | Err(_) => break,
                        Ok((true, was_cut)) => {
                            let mut t = decode_line(&buf, was_cut).trim_end_matches(['\n', '\r']).to_string();
                            if was_cut || t.chars().count() > max_line {
                                t = format!("{}… [line cut]", t.chars().take(max_line).collect::<String>());
                            }
                            // The live output of the call (card OA7-04): the line is told as it is kept, and the sink does the rest (coalesces, cleans, drops the oldest).
                            if let Some(p) = &live {
                                p.line(&t);
                            }
                            Job::push(&st, is_err, t);
                        }
                    }
                }
            }));
        }
    }

    let deadline = Instant::now() + spec.max_runtime;
    let waiter_state = state.clone();
    let limits = spec.limits;
    let live = spec.progress.clone();
    std::thread::spawn(move || {
        let contained = contained;
        let status = loop {
            // The messages the job sends about its limits are read as they come: a program that starts a great many processes would leave them waiting in the job's port for as long as it runs.
            if let Some(c) = &contained {
                c.drain();
            }
            // The lines of the last quarter of a second leave even when the program prints nothing more for a while (card OA7-04).
            if let Some(p) = &live {
                p.tick();
            }
            match child.try_wait() {
                Ok(Some(code)) => break Some(code),
                Ok(None) => {}
                Err(_) => break None,
            }
            let timed_out = Instant::now() >= deadline;
            if kill.load(Ordering::SeqCst) || timed_out {
                {
                    let mut st = waiter_state.lock().unwrap_or_else(PoisonError::into_inner);
                    st.timed_out = timed_out && !kill.load(Ordering::SeqCst);
                    st.killed = true;
                }
                if let Some(c) = &contained {
                    c.kill_tree();
                }
                child.kill();
                break child.wait();
            }
            std::thread::sleep(Duration::from_millis(25));
        };
        // What the job counted, asked before it is closed: a program that was stopped or timed out ended for another reason, and one that failed for lack of memory or of room for
        // another process says nothing about it in its exit code (card OA7-01, TM-11).
        let ended_by_itself = !waiter_state.lock().unwrap_or_else(PoisonError::into_inner).killed;
        if ended_by_itself {
            if let Some(c) = &contained {
                let note = exit_codes::limit_note(c.usage(), limits, status.flatten() != Some(0));
                waiter_state.lock().unwrap_or_else(PoisonError::into_inner).limit_note = note;
            }
        }
        // Whatever is still holding the pipes (a grandchild) is killed with the job.
        drop(contained);
        // An MXC container is deleted before the readers are joined (a contained process that outlived the launcher would hold the pipes open). After a normal end the
        // clean-up, which asks the launcher to delete the container's profile, is left to a thread of its own so that the answer does not wait for it.
        if let Some(c) = cleanup {
            if waiter_state.lock().unwrap_or_else(PoisonError::into_inner).killed {
                drop(c);
            } else {
                std::thread::spawn(move || drop(c));
            }
        }
        for r in readers {
            let _ = r.join();
        }
        // The last lines leave before the end is told. A sink that the call kept for a background job is finished with the job; any other stays open for the programs that follow in the same
        // call (a `term.shell` line) and is finished by the host when the call ends.
        if let Some(p) = &live {
            if p.is_detached() {
                p.finish();
            } else {
                p.flush_now();
            }
        }
        let mut st = waiter_state.lock().unwrap_or_else(PoisonError::into_inner);
        st.exit_code = status.flatten();
        st.done = true;
    });

    // The text for the program's stdin is written on a thread of its own, now that the readers and the waiter exist (card OA7-01, TM-20). Written before them, a program that prints while it
    // still reads (a filter) fills its output pipe, stops, and the write that waits for it never ends: no timeout, no stop, and a program that cannot be killed from here. Written here, the
    // timeout of the waiter ends the program, and its end breaks the pipe and ends the thread. stdin is closed when the thread has written it (that is the end of the input).
    if let (Some(text), Some(mut stdin)) = (spec.stdin, stdin_pipe) {
        std::thread::spawn(move || {
            let _ = stdin.write_all(text.as_bytes());
        });
    }
    Ok(job)
}

struct TermInner {
    guard: Arc<PathGuard>,
    cfg: TermConfig,
    /// The person's own rules (`data/tools/term_rules.json`); they can only make a command ask or refuse.
    rules: ExecRules,
    jobs: Mutex<HashMap<String, Arc<Job>>>,
    counter: AtomicU64,
    /// The tier calls run at when they do not ask for more, and the MXC container behind it when the person chose that tier (cards R6-35, R6-10).
    tier: Tier,
    mxc: Option<Arc<Mxc>>,
    /// The journal of what commands changed (card OA7-03, TM-12), given once by [`TermProvider::set_journal`]. Looked at only while the setting is on.
    #[cfg(feature = "git")]
    journal: std::sync::OnceLock<Arc<crate::git::journal::Journal>>,
}

/// What a call got from `TermInner::contain`: the tier it runs at, how to start it, and what to tell the person.
struct Contain {
    tier: Tier,
    /// Start it at Low integrity.
    isolated: bool,
    wrap: Option<Wrap>,
    scratch: Option<PathBuf>,
    note: Option<String>,
}

/// Result of a command run to completion.
pub(crate) struct RunResult {
    pub exit_code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub truncated: bool,
    pub timed_out: bool,
    pub cancelled: bool,
    pub duration_ms: u64,
    /// The last 2,000 characters of what the program printed, stdout and stderr in the order they came (a stderr line begins `[err] `): what a timeout can still say.
    pub last_output: String,
    /// A limit of the terminal tools the command ran into (see `exit_codes::limit_note`).
    pub limit_note: Option<String>,
    /// The time the command was given, in seconds (the one asked for, or what was left of the call, whichever is less), and whether it is the second (card OA4-03).
    pub limit_s: u64,
    pub limit_is_the_calls: bool,
    /// The command was still running at its time and was kept as a background job instead of being stopped (card OA4-03, TM-04); `timed_out` is false then.
    pub promoted: Option<Arc<Job>>,
}

impl RunResult {
    /// The limit as a sentence says it: `300 s`, or `the 28 s left of this call`.
    pub fn limit_words(&self) -> String {
        if self.limit_is_the_calls {
            format!("the {} s left of this call", self.limit_s)
        } else {
            format!("{} s", self.limit_s)
        }
    }

    /// A command that was not started, because the call it belongs to was already stopped or had no time left (card OA4-03).
    fn not_started(cancelled: bool, limit_s: u64) -> RunResult {
        RunResult { exit_code: None, stdout: String::new(), stderr: String::new(), truncated: false, timed_out: !cancelled, cancelled, duration_ms: 0, last_output: String::new(), limit_note: None, limit_s, limit_is_the_calls: true, promoted: None }
    }
}

/// How much of the end of the output a timeout still shows.
const LAST_OUTPUT_CHARS: usize = 2_000;

/// Read one line into `buf` without its line end, keeping at most `max_bytes` of it: the rest of a longer line is read and dropped. Returns
/// whether a line was read (false at the end of the stream) and whether it was cut.
fn read_line_bounded(r: &mut impl BufRead, buf: &mut Vec<u8>, max_bytes: usize) -> std::io::Result<(bool, bool)> {
    let (mut any, mut cut) = (false, false);
    loop {
        let available = match r.fill_buf() {
            Ok(a) => a,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        if available.is_empty() {
            return Ok((any, cut));
        }
        any = true;
        let (used, ended) = match available.iter().position(|b| *b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        let content = if ended { used - 1 } else { used };
        let room = max_bytes.saturating_sub(buf.len());
        buf.extend_from_slice(&available[..content.min(room)]);
        cut |= content > room;
        r.consume(used);
        if ended {
            return Ok((true, cut));
        }
    }
}

/// One line of a program's output as text (card OA7-01, TM-09). A line that is UTF-8 is kept as it is. One that is not is what a console program of Windows prints in the OEM code page
/// (`ipconfig` and `tasklist` on a computer that is not set to English), and it is converted from that code page, where it used to become a row of U+FFFD. `cut` says the line was cut
/// at a byte limit, so a character unfinished at its end is no code page text: it is dropped.
fn decode_line(buf: &[u8], cut: bool) -> String {
    decode_with(buf, cut, oem_to_string)
}

/// [`decode_line`] with the conversion from the OEM code page given (so that a test can use its own).
fn decode_with(buf: &[u8], cut: bool, oem: impl Fn(&[u8]) -> Option<String>) -> String {
    match std::str::from_utf8(buf) {
        Ok(text) => text.to_string(),
        // `error_len() == None` is a character that is not finished at the end of the bytes.
        Err(e) if cut && e.error_len().is_none() => String::from_utf8_lossy(&buf[..e.valid_up_to()]).into_owned(),
        Err(_) => oem(buf).unwrap_or_else(|| String::from_utf8_lossy(buf).into_owned()),
    }
}

/// The bytes read in the OEM code page of this computer (`MultiByteToWideChar` with `CP_OEMCP`), or `None` where there is no such thing or the conversion failed.
#[cfg(windows)]
fn oem_to_string(bytes: &[u8]) -> Option<String> {
    #[link(name = "kernel32")]
    extern "system" {
        fn MultiByteToWideChar(code_page: u32, flags: u32, multi_byte: *const u8, multi_byte_len: i32, wide: *mut u16, wide_len: i32) -> i32;
    }
    const CP_OEMCP: u32 = 1;
    if bytes.is_empty() || bytes.len() > i32::MAX as usize {
        return None;
    }
    // SAFETY: the pointers and lengths are those of live buffers (the second call is given the size the first one asked for).
    unsafe {
        let n = MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), bytes.len() as i32, std::ptr::null_mut(), 0);
        if n <= 0 {
            return None;
        }
        let mut wide = vec![0u16; n as usize];
        let written = MultiByteToWideChar(CP_OEMCP, 0, bytes.as_ptr(), bytes.len() as i32, wide.as_mut_ptr(), n);
        (written > 0).then(|| String::from_utf16_lossy(&wide[..written as usize]))
    }
}

#[cfg(not(windows))]
fn oem_to_string(_bytes: &[u8]) -> Option<String> {
    None
}

/// Start a program (no shell) inside a Job Object, wait for it, and return what it printed.
/// The whole process tree is killed on timeout or cancellation.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run_command(
    exe: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    stdin: Option<String>,
    limits: Limits,
    timeout: Duration,
    cap_bytes: usize,
    cancel: &AtomicBool,
) -> Result<RunResult, ToolError> {
    run_command_ex(false, None, exe, args, cwd, env, stdin, limits, timeout, cap_bytes, false, cancel, false, None)
}

/// [`run_command`], optionally at Low integrity (see `contain_low`) or in an MXC container (`wrap`).
#[allow(clippy::too_many_arguments)]
fn run_command_ex(
    isolated: bool,
    wrap: Option<Wrap>,
    exe: PathBuf,
    args: Vec<String>,
    cwd: PathBuf,
    env: Vec<(String, String)>,
    stdin: Option<String>,
    limits: Limits,
    timeout: Duration,
    cap_bytes: usize,
    keep_end: bool,
    cancel: &AtomicBool,
    promote: bool,
    progress: Option<ProgressSink>,
) -> Result<RunResult, ToolError> {
    // What is left of the call's time (see `CallClock`) is the most a program gets, whatever the caller asked. A call that was stopped before this program was started starts nothing.
    let (timeout, from_call) = match CallClock::left() {
        Some(left) if left < timeout => (left, true),
        _ => (timeout, false),
    };
    let limit_s = ((timeout.as_millis() + 500) / 1000) as u64;
    if cancel.load(Ordering::SeqCst) {
        return Ok(RunResult::not_started(true, limit_s));
    }
    if timeout < Duration::from_millis(50) {
        return Ok(RunResult::not_started(false, limit_s));
    }
    let line = command_line(&exe, &args);
    // A run that may be moved to a background job at its time is not ended by the clock of the job at that time, but at the longest a job lives.
    let max_runtime = if promote { timeout.max(Duration::from_secs(PROMOTED_RUN_SECS)) } else { timeout };
    let spec = Spec { isolated, wrap, exe, args, cwd, env, stdin, limits, max_runtime, cap_bytes, stop_at_cap: !keep_end, keep_end, max_line_chars: cap_bytes, progress };
    let job = spawn(spec, "run".into(), "run".into(), line)?;
    let mut cancelled = false;
    let mut promoted = None;
    loop {
        if job.state.lock().unwrap_or_else(PoisonError::into_inner).done {
            break;
        }
        if cancel.load(Ordering::SeqCst) {
            cancelled = true;
            job.kill.store(true, Ordering::SeqCst);
        }
        if promote && !cancelled && job.started.elapsed() >= timeout {
            promoted = Some(job.clone());
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let st = job.state.lock().unwrap_or_else(PoisonError::into_inner);
    // The lines in the order they came: the start that was kept, then the ring (a run that keeps no end has no start of its own and its lines are all in the ring).
    let ordered: Vec<&(u64, bool, String)> = st.head.iter().chain(st.lines.iter()).collect();
    let gap_after = st.head.len();
    let join = |err: bool| {
        let dropped = if err { st.dropped.1 } else { st.dropped.0 };
        let marker = format!("[… {dropped} line{} cut …]", if dropped == 1 { "" } else { "s" });
        let mut parts: Vec<&str> = Vec::new();
        let mut marked = dropped == 0;
        for (i, (_, e, t)) in ordered.iter().enumerate() {
            if i == gap_after && !marked {
                parts.push(&marker);
                marked = true;
            }
            if *e == err {
                parts.push(t);
            }
        }
        if !marked {
            parts.push(&marker);
        }
        parts.join("\n")
    };
    let mut last: Vec<String> = Vec::new();
    let mut last_chars = 0usize;
    for (_, e, t) in ordered.iter().rev() {
        let line = if *e { format!("[err] {t}") } else { t.clone() };
        last_chars += line.chars().count() + 1;
        last.push(line);
        if last_chars >= LAST_OUTPUT_CHARS {
            break;
        }
    }
    last.reverse();
    let last_all = last.join("\n");
    let last_output: String = last_all.chars().skip(last_all.chars().count().saturating_sub(LAST_OUTPUT_CHARS)).collect();
    Ok(RunResult {
        exit_code: st.exit_code,
        stdout: join(false),
        stderr: join(true),
        truncated: st.capped || st.dropped.0 + st.dropped.1 > 0,
        timed_out: st.timed_out,
        cancelled: cancelled && st.killed,
        duration_ms: job.started.elapsed().as_millis() as u64,
        last_output,
        limit_note: st.limit_note.clone(),
        limit_s,
        limit_is_the_calls: from_call,
        promoted,
    })
}

/// A command after its program was found and judged.
struct Parsed {
    exe: PathBuf,
    argv: Vec<String>,
    cwd: PathBuf,
    kind: ExeKind,
    /// A build or test command of a project (`cargo test`, `npm test`, `python -m pytest`): see `rules::is_project_check`.
    project_check: bool,
    /// A rule of `term_rules.json` makes this command ask every time (the reason the person wrote, or a plain default).
    rule_prompt: Option<String>,
    /// What the command does beyond running code (it destroys work, it publishes, it changes another computer): see `rules::command_effect`. It makes the command ask every time,
    /// in every mode, like the tool of Aqua that does the same thing.
    effect: Option<rules::Effect>,
    /// What is really started: `argv`, and for git the safety options in front of it and the diff flags after its subcommand (see `rules::git_run_args`). Never shown in the
    /// question, which shows `argv`, the command as written: they only take away.
    spawn_argv: Vec<String>,
}

/// The most characters of a command that a question can show. A longer command is not run: a question that hides part of a command can hide what matters in it.
const MAX_SHOWN_COMMAND: usize = 2000;

/// The terminal tools. State lives behind an `Arc` so a blocking task can own it while it runs. A clone shares the same jobs, rules and configuration: `check.run` (card OA4-05) runs its
/// commands through a clone, so a check is classified, asked about, timed and kept as a background job by the terminal tools exactly as the same `term.run` would be.
#[derive(Clone)]
pub struct TermProvider(Arc<TermInner>);

impl TermProvider {
    pub fn new(guard: Arc<PathGuard>, cfg: TermConfig) -> Self {
        Self::with_rules(guard, cfg, ExecRules::none())
    }

    pub fn with_rules(guard: Arc<PathGuard>, cfg: TermConfig, rules: ExecRules) -> Self {
        // The container is probed here, once, and only when the person chose it (`default_tier` of term.json is `none` unless they did): nothing is started otherwise.
        let mxc = (cfg.default_tier == Tier::Mxc).then(|| Arc::new(Mxc::from_config(&cfg.mxc)));
        if let Some(m) = &mxc {
            match m.available() {
                Ok(()) => tracing::info!("terminal tools: MXC containment is available; programs run in a container"),
                Err(why) => tracing::warn!("terminal tools: default_tier is mxc but it is not usable ({why}); programs run at Low integrity"),
            }
        }
        let tier = cfg.default_tier;
        Self(Arc::new(TermInner {
            guard,
            cfg,
            rules,
            jobs: Mutex::new(HashMap::new()),
            counter: AtomicU64::new(1),
            tier,
            mxc,
            #[cfg(feature = "git")]
            journal: std::sync::OnceLock::new(),
        }))
    }
}

impl TermInner {
    fn limits(&self) -> Limits {
        Limits { memory_mb: self.cfg.memory_mb, max_processes: self.cfg.max_processes }
    }

    fn parse_command(&self, args: &Value) -> Result<Parsed, ToolError> {
        let exe_arg = args.get("exe").and_then(|v| v.as_str()).unwrap_or("");
        let given: Vec<String> = args.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        // `npm` and `npx` start Node.js on a script: run it that way, with no shell. What is judged (and shown) is still the
        // name and the arguments the caller gave.
        let (exe, argv, judged_exe, via_shim) = match rules::resolve_script_host(&self.cfg, exe_arg) {
            Some(found) => {
                let (node, script) = found?;
                let mut v = vec![script.to_string_lossy().into_owned()];
                v.extend(given.iter().cloned());
                (node, v, exe_arg.to_string(), false)
            }
            None => match rules::resolve_exe(&self.cfg, exe_arg) {
                Ok(exe) => {
                    let judged = exe.to_string_lossy().into_owned();
                    (exe, given.clone(), judged, false)
                }
                // A name that is no program may be a Node.js tool whose shim npm wrote (`tsc`, `eslint`, `vite`, `pnpm`...): it runs as `node.exe <script> <arguments>` and the `.cmd` is
                // never started, as for `npm` above (card OA7-01, TM-10). Judged and shown by the name the caller gave.
                Err(e) if e.code == ErrorCode::NotFound || exe_arg.trim().to_ascii_lowercase().ends_with(".cmd") => match shim::resolve(&self.cfg, exe_arg, &self.project_bin_dirs(args)) {
                    Some(found) => {
                        let (node, script) = found?;
                        let mut v = vec![script.to_string_lossy().into_owned()];
                        v.extend(given.iter().cloned());
                        (node, v, exe_arg.to_string(), true)
                    }
                    None => return Err(e),
                },
                Err(e) => return Err(e),
            },
        };
        let kind = rules::classify_exe(&judged_exe)?;
        // A program that runs another is judged by what it runs (`env curl ...` is `curl`).
        let kind = rules::judge_launched(&judged_exe, &given, kind)?;
        // A tool that is ordinary by name can still be told to run code (`git -c core.fsmonitor=...`, `cargo build`):
        // such a call is held to the interpreter rule and asks every time, whatever the mode.
        let kind = if kind == ExeKind::Plain && rules::args_run_code(&judged_exe, &given).is_some() { ExeKind::Interpreter } else { kind };
        // A script of a package that runs under Node.js is code: it asks every time, as `node script.js` does (a project's own `npm test` is the only exception, below).
        let kind = if via_shim { ExeKind::Interpreter } else { kind };
        if argv.iter().any(|a| a.contains('\0')) {
            return Err(ToolError::new(ErrorCode::InvalidArguments, "arguments may not contain NUL characters"));
        }
        let cwd_arg = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
        let cwd = if cwd_arg.is_empty() { self.guard.roots()[0].1.clone() } else { self.guard.resolve(cwd_arg, Op::Read)?.path };
        if !cwd.is_dir() {
            return Err(ToolError::new(ErrorCode::InvalidArguments, format!("cwd {} is not a folder", display(&cwd))));
        }
        if rules::program_name(&exe.to_string_lossy()) == "git" {
            self.check_git_paths(&argv, &cwd)?;
        }
        let project_check = rules::is_project_check(&judged_exe, &given) || (self.extended_checks() && extended::is_extended_check(&judged_exe, &given));
        // The person's own rules come last, so they only ever add to what the built-in rules above already refused or asked about.
        // A rule about a program applies when the program is wrapped in another (`env npm publish`): the strictest answer of any of them counts.
        let mut decided = self.rules.decide(&judged_exe, &given);
        for (inner, inner_args) in rules::launched_chain(&judged_exe, &given) {
            if let Some(d) = self.rules.decide(&inner, &inner_args) {
                if decided.as_ref().is_none_or(|(now, _)| d.0 > *now) {
                    decided = Some(d);
                }
            }
        }
        let rule_prompt = match decided {
            Some((RuleDecision::Forbid, why)) => {
                let why = if why.is_empty() { String::new() } else { format!(": {why}") };
                return Err(ToolError::new(ErrorCode::PermissionDenied, format!("a rule in term_rules.json refuses this command{why}")));
            }
            Some((RuleDecision::Prompt, why)) => Some(if why.is_empty() { "a rule in term_rules.json".to_string() } else { why }),
            None => None,
        };
        let spawn_argv = if rules::program_name(&exe.to_string_lossy()) == "git" { rules::git_run_args(&argv, &self.repo_programs_switched_off(&exe, &argv, &cwd, args)?) } else { argv.clone() };
        // What it does beyond running code (OA8-01, SE-02): `git push`, `git reset --hard`, `cargo publish` and the like. Read for the program and for each program it launches.
        let effect = rules::merge_effects(
            std::iter::once((judged_exe.clone(), given.clone())).chain(rules::launched_chain(&judged_exe, &given)).filter_map(|(program, program_args)| rules::command_effect(&program, &program_args, Some(&cwd))),
        );
        Ok(Parsed { exe, argv, cwd, kind, project_check, rule_prompt, effect, spawn_argv })
    }

    /// The options that switch off every program the repository's own configuration names (a filter, a merge driver, a credential helper, an ssh command...), found by listing that
    /// configuration with `git config` in the repository the command is aimed at. Listing a configuration runs nothing. A folder that is not a repository has none; one whose
    /// configuration cannot be read, or cannot be switched off, is an error.
    fn repo_programs_switched_off(&self, exe: &Path, argv: &[String], cwd: &Path, args: &Value) -> Result<Vec<String>, ToolError> {
        if rules::git_subcommand_index(argv).is_none() {
            return Ok(Vec::new());
        }
        let mut cmd = std::process::Command::new(exe);
        cmd.args(rules::git_config_scan_args(argv)).current_dir(cwd).env_clear().envs(self.env_for(args)?).stdin(std::process::Stdio::null());
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            cmd.creation_flags(0x0800_0000);
        }
        // A configuration that cannot be read is not one that names nothing: git is not run. (Outside a repository the listing still works, with the person's own and the system's
        // configuration only; it fails for a repository that is not safe to read, one that cannot be found, and one whose configuration is broken, none of which git could be run in.)
        let out = cmd.output().map_err(|e| ToolError::new(ErrorCode::PermissionDenied, format!("the repository's own configuration could not be read ({e}): git is not run")))?;
        if !out.status.success() {
            let said = String::from_utf8_lossy(&out.stderr);
            return Err(ToolError::new(ErrorCode::PermissionDenied, format!("the repository's own configuration could not be read ({}): git is not run", said.lines().next().unwrap_or("git config failed").chars().take(160).collect::<String>())));
        }
        rules::switch_off_program_keys(&String::from_utf8_lossy(&out.stdout)).map_err(|why| ToolError::new(ErrorCode::PermissionDenied, why))
    }

    /// The `node_modules\.bin` folders of the working folder of a call and of the folders above it inside its root: where the Node.js tools of a project have their shims (card OA7-01, TM-10).
    /// Empty when the folder cannot be resolved: that is reported where the folder is used.
    fn project_bin_dirs(&self, args: &Value) -> Vec<PathBuf> {
        let cwd_arg = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
        let cwd = if cwd_arg.is_empty() {
            match self.guard.roots().first() {
                Some(r) => r.1.clone(),
                None => return Vec::new(),
            }
        } else {
            match self.guard.resolve(cwd_arg, Op::Read) {
                Ok(r) => r.path,
                Err(_) => return Vec::new(),
            }
        };
        match self.guard.root_path_of(&cwd) {
            Some(root) => shim::project_bin_dirs(&cwd, &root),
            None => Vec::new(),
        }
    }

    /// Is this folder inside a trusted root (the workspace, or the project folder under full access)?
    fn cwd_is_trusted(&self, cwd: &Path) -> bool {
        self.guard.resolve(&cwd.to_string_lossy(), Op::Read).map(|r| r.trusted).unwrap_or(false)
    }

    /// Every file or folder a `git` command names (`-C`, `--output=`, `diff --no-index`, a clone destination, an absolute
    /// operand...) must be one the file tools could use too: inside a configured root, not one of Aqua's own files, and
    /// writable where it is written. Without this, `git log --output=<policy file>` or `git diff --no-index` would reach
    /// anywhere on the computer through a program that is otherwise ordinary.
    fn check_git_paths(&self, argv: &[String], cwd: &Path) -> Result<(), ToolError> {
        let uses = rules::git_path_uses(argv).map_err(|m| ToolError::new(ErrorCode::PermissionDenied, m))?;
        for u in uses {
            let full = if u.path.starts_with('@') || rules::looks_absolute(&u.path) { u.path.clone() } else { cwd.join(&u.path).to_string_lossy().into_owned() };
            self.guard.resolve(&full, if u.write { Op::Write } else { Op::Read }).map_err(|e| {
                ToolError::new(ErrorCode::PermissionDenied, format!("git argument '{}' is not allowed: {}", u.path, e.message))
            })?;
        }
        Ok(())
    }

    fn env_for(&self, args: &Value) -> Result<Vec<(String, String)>, ToolError> {
        let requested: HashMap<String, String> = args
            .get("env")
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.as_str().map(String::from).unwrap_or_else(|| v.to_string()))).collect())
            .unwrap_or_default();
        build_env(std::env::vars(), &requested).map_err(|e| ToolError::new(ErrorCode::InvalidArguments, e))
    }

    /// A fresh scratch folder for an isolated run, under the first read-write root, labelled Low so
    /// the Low-integrity child can write there and nowhere else. Old ones are cleaned up.
    #[cfg(windows)]
    fn sandbox_dir(&self) -> Result<PathBuf, ToolError> {
        let unavailable = |m: String| ToolError::new(ErrorCode::Unavailable, format!("cannot prepare the isolated scratch folder: {m}"));
        let root = self.guard.roots().iter().find(|(_, _, a)| *a == crate::fs::guard::Access::Rw).map(|(_, p, _)| p.clone()).ok_or_else(|| unavailable("no read-write folder root is configured (data/tools/fs.json)".into()))?;
        let base = root.join(".aqua_sandbox");
        std::fs::create_dir_all(&base).map_err(|e| unavailable(format!("{}: {e}", display(&base))))?;
        // Whatever an isolated program writes is its own and never belongs in the repository the folder sits in: the folder keeps itself out of git.
        keep_out_of_git(&base);
        // Labelling once is enough: folders created inside inherit it.
        if !base.join(".labelled").exists() {
            crate::contain_low::label_low(&base).map_err(unavailable)?;
            let _ = std::fs::write(base.join(".labelled"), "");
        }
        // Forget scratch folders older than a day.
        if let Ok(rd) = std::fs::read_dir(&base) {
            for e in rd.flatten() {
                let old = e.metadata().and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok()).is_some_and(|d| d > Duration::from_secs(24 * 3600));
                if old && e.path().is_dir() {
                    let _ = std::fs::remove_dir_all(e.path());
                }
            }
        }
        let n = self.counter.fetch_add(1, Ordering::SeqCst);
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis()).unwrap_or(0);
        let dir = base.join(format!("run_{stamp}_{n}"));
        std::fs::create_dir_all(&dir).map_err(|e| unavailable(format!("{}: {e}", display(&dir))))?;
        Ok(dir)
    }

    #[cfg(not(windows))]
    fn sandbox_dir(&self) -> Result<PathBuf, ToolError> {
        Err(ToolError::new(ErrorCode::UnsupportedPlatform, "isolated runs are only available on Windows"))
    }

    /// The tier a call runs at (what it asked for or the default, whichever is stronger) and what that needs: a scratch folder, the container's grants, a note when the tier
    /// is not the one wanted (cards R6-35 and R6-10, D-5). With the default tier `none` and no `isolated` this does nothing, as before.
    fn contain(&self, asked: Tier, cwd: &mut PathBuf, cwd_given: bool, env: &mut Vec<(String, String)>) -> Result<Contain, ToolError> {
        let choice = sandbox::choose(asked, self.tier, self.mxc.as_deref());
        if choice.tier == Tier::None {
            return Ok(Contain { tier: Tier::None, isolated: false, wrap: None, scratch: None, note: None });
        }
        let dir = self.sandbox_dir()?;
        // Temp files go in the scratch folder too: it is the only place the child can write.
        for k in ["TEMP", "TMP"] {
            env.retain(|(n, _)| !n.eq_ignore_ascii_case(k));
            env.push((k.into(), dir.to_string_lossy().to_string()));
        }
        let mut note = choice.note;
        let mut wrap = None;
        let mut shared = cwd_given;
        if choice.tier == Tier::Mxc {
            let mxc = self.mxc.clone().ok_or_else(|| ToolError::new(ErrorCode::SandboxViolation, "the MXC tier was chosen but is not set up"))?;
            // A working folder that holds Aqua's own data is not shared with the container, not even read-only: the program then works in its scratch folder. (The policy also
            // names the data as denied, but whether a denial beats a grant of the folder above it is for the live measurement to say.)
            let holds_aquas_data = self.guard.protected().iter().any(|p| crate::fs::guard::within(p, cwd));
            if cwd_given && holds_aquas_data {
                shared = false;
                note = Some(match note {
                    Some(n) => format!("{n}; the working folder holds Aqua's own data, so the program worked in its scratch folder"),
                    None => "the working folder holds Aqua's own data, so the program worked in its scratch folder".into(),
                });
            }
            wrap = Some(Wrap { mxc, read: if shared { vec![cwd.clone()] } else { Vec::new() }, write: vec![dir.clone()], denied: self.guard.protected().to_vec() });
        }
        if !shared {
            *cwd = dir.clone();
        }
        Ok(Contain { tier: choice.tier, isolated: choice.tier == Tier::Low, wrap, scratch: Some(dir), note })
    }

    fn get(&self, id: &str) -> Result<Arc<Job>, ToolError> {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner).get(id).cloned().ok_or_else(|| ToolError::new(ErrorCode::NotFound, format!("no job '{id}' (it may have been cleaned up)")))
    }
}

/// Names of the files (recursively) in `dir`, relative, up to `max`.
fn files_in(dir: &std::path::Path, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if out.len() < max {
                let rel = p.strip_prefix(dir).unwrap_or(&p).to_string_lossy().replace('\\', "/");
                out.push(format!("{rel} ({} bytes)", e.metadata().map(|m| m.len()).unwrap_or(0)));
            }
        }
    }
    out.sort();
    out
}

/// Where cargo would write its build output when it is not inside any folder the person allowed: `CARGO_TARGET_DIR` of the environment the program gets (card OA7-01, TM-09). `None` for any
/// other program, when the variable is not set, and when the folder is inside a root (or a folder that is not there yet is under one).
fn build_output_outside_roots(guard: &PathGuard, program: &str, env: &[(String, String)]) -> Option<String> {
    if !(program == "cargo" || program.starts_with("cargo-") || program == "rustc") {
        return None;
    }
    let dir = env.iter().find(|(k, v)| k.eq_ignore_ascii_case("CARGO_TARGET_DIR") && !v.trim().is_empty()).map(|(_, v)| v.trim())?;
    let full = crate::fs::guard::canonical_lenient(Path::new(dir)).unwrap_or_else(|| PathBuf::from(dir));
    guard.root_path_of(&full).is_none().then(|| display(&full))
}

fn command_line(exe: &std::path::Path, argv: &[String]) -> String {
    let name = exe.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let mut parts = vec![name];
    parts.extend(argv.iter().map(|a| if a.is_empty() || a.contains(' ') { format!("\"{a}\"") } else { a.clone() }));
    parts.join(" ")
}

fn spec_manifest(name: &str, description: &str, classes: Vec<PermClass>, schema: Value, default_ms: u64, max_ms: u64, read_only: bool) -> ToolManifest {
    ToolManifest {
        name: name.into(),
        title: name.into(),
        description: description.into(),
        classes,
        default_decision: Decision::Auto,
        input_schema: schema,
        timeout_default_ms: default_ms,
        timeout_max_ms: max_ms,
        annotations: Annotations { read_only, destructive: !read_only, idempotent: false, open_world: true },
        untrusted_output: true,
    }
}

fn cmd_props() -> Value {
    json!({
        "exe": {"type": "string", "minLength": 1, "maxLength": 260},
        "args": {"type": "array", "items": {"type": "string", "maxLength": 4096}, "maxItems": 64},
        "cwd": {"type": "string", "maxLength": 1024},
        "env": {"type": "object", "additionalProperties": true}
    })
}

impl ToolProvider for TermProvider {
    fn id(&self) -> &str {
        "term"
    }

    fn manifests(&self) -> Vec<ToolManifest> {
        let mut run_props = cmd_props();
        run_props["timeout_s"] = json!({"type": "integer", "minimum": 1, "maximum": 300, "description": "How long to wait, in seconds. Left out it is 30 for an ordinary program and 300 for a build or test program (cargo, npm, pnpm, yarn, python -m pytest, go, dotnet, mvn, gradle, make). A longer build: start it with term.start and read it with term.output."});
        run_props["background_on_timeout"] = json!({"type": "boolean", "description": "When the time is up and the command is still running, keep it going as a background job (the answer names its job_id: read it with term.output, end it with term.stop) instead of stopping it. Left out it is true for a build or test program when the person turned that on, and false otherwise."});
        run_props["stdin"] = json!({"type": "string", "maxLength": 65536});
        run_props["max_output_kb"] = json!({"type": "integer", "minimum": 1, "maximum": 512, "description": "Keep at most this much output (its start and its end are kept, the middle is left out). Leave it out to keep everything up to 8 MB: the text you read is cut by the host to its start and its end, and when it is cut the whole output is kept as a file you can search with fs.grep."});
        run_props["isolated"] = json!({"type": "boolean", "default": false, "description": "Run at Low integrity: it can read but can write only in its own scratch folder (@ws/.aqua_sandbox/…). Use it for code you do not trust."});
        let mut start_props = cmd_props();
        start_props["name"] = json!({"type": "string", "maxLength": 60});
        start_props["max_runtime_s"] = json!({"type": "integer", "minimum": 1, "maximum": 3600, "default": 600});
        vec![
            spec_manifest("term.run", "Run one program with arguments (no shell) and wait for it. Returns exit code, stdout, stderr.", vec![PermClass::Execute],
                json!({"type": "object", "properties": run_props, "required": ["exe"]}), 330_000, 330_000, false),
            spec_manifest("term.start", "Start a program in the background (for servers and long builds). Returns a job_id.", vec![PermClass::Execute],
                json!({"type": "object", "properties": start_props, "required": ["exe"]}), 30_000, 60_000, false),
            spec_manifest("term.output", "Read new output of a background job since after_seq, optionally waiting up to wait_ms.", vec![PermClass::Read],
                json!({"type": "object", "properties": {"job_id": {"type": "string", "maxLength": 40}, "after_seq": {"type": "integer", "minimum": -1, "default": -1}, "wait_ms": {"type": "integer", "minimum": 0, "maximum": 10000, "default": 0}, "max_chars": {"type": "integer", "minimum": 200, "maximum": 20000, "default": 4000}}, "required": ["job_id"]}), 20_000, 30_000, true),
            spec_manifest("term.stop", "Stop a background job and everything it started.", vec![PermClass::Write],
                json!({"type": "object", "properties": {"job_id": {"type": "string", "maxLength": 40}}, "required": ["job_id"]}), 15_000, 30_000, false),
            spec_manifest("term.sessions", "List background jobs with their state.", vec![PermClass::Read],
                json!({"type": "object", "properties": {}}), 10_000, 30_000, true),
            spec_manifest("term.shell", "Run up to four programs joined by &&. No pipes, redirects, or nested shells. Asks every time.", vec![PermClass::Execute, PermClass::Destructive],
                json!({"type": "object", "properties": {"command": {"type": "string", "minLength": 1, "maxLength": 2000}, "cwd": {"type": "string", "maxLength": 1024}, "timeout_s": {"type": "integer", "minimum": 1, "maximum": 120, "description": "The time for the whole line, in seconds, not for each program: the programs share it. Left out it is 30 for each program (120 for a build or test program) added up, at most 120."}}, "required": ["command"]}), 150_000, 150_000, false),
            // On offer only while the journal of commands is on (`availability`). It always asks: it puts files back.
            spec_manifest("term.undo", "Put back the files a command changed (call_id is in its result). Files changed since are left. dry_run only lists. Always asks.", vec![PermClass::Write, PermClass::Destructive],
                json!({"type": "object", "properties": {"call_id": {"type": "string", "minLength": 3, "maxLength": 40}, "dry_run": {"type": "boolean", "default": false}, "cwd": {"type": "string", "maxLength": 1024, "description": "The repository, when Aqua was restarted since the command ran."}}, "required": ["call_id"]}), 90_000, 120_000, false),
        ]
    }

    fn availability(&self, tool: &str) -> aqua_tool_host::Availability {
        // `term.undo` is a new tool: it is on offer only while the journal of commands is on (card OA7-03; new behaviour is off until the person turns it on).
        if tool == "term.undo" {
            #[cfg(feature = "git")]
            {
                if self.0.journal_on() && self.0.journal.get().is_some() {
                    return aqua_tool_host::Availability::Available;
                }
            }
            return aqua_tool_host::Availability::Unavailable("term.undo is off (AQUA_TERM_JOURNAL=on, or \"journal\": true in term.json, turns the journal of commands and this tool on)".into());
        }
        aqua_tool_host::Availability::Available
    }

    fn grant_scope(&self, tool: &str, args: &Value) -> Option<String> {
        self.0.grant_scope_of(tool, args)
    }

    fn project_check(&self, tool: &str, args: &Value) -> Option<aqua_tool_host::ProjectCheck> {
        self.0.project_check_of(tool, args)
    }

    fn classify(&self, tool: &str, args: &Value) -> Result<Classification, ToolError> {
        match tool {
            "term.run" | "term.start" => {
                let Parsed { exe, argv, cwd, kind, project_check, rule_prompt, effect, .. } = self.0.parse_command(args)?;
                let env = self.0.env_for(args)?;
                let line = command_line(&exe, &argv);
                let mut classes = vec![PermClass::Execute];
                // A project's own build or test command in a trusted folder is an ordinary "runs code" call: it asks once for the
                // exact command and remembers the answer for the session. Anywhere else, and for every other interpreter call
                // (a script, `cargo run`, `npm install`), it runs whatever its arguments or files say, so it asks every time.
                let project_check_here = kind == ExeKind::Interpreter && project_check && rule_prompt.is_none() && self.0.cwd_is_trusted(&cwd);
                if (kind == ExeKind::Interpreter && !project_check_here) || rule_prompt.is_some() {
                    classes.push(PermClass::Destructive);
                }
                // A push, a hard reset, a clean, a publish: the classes of the tool of Aqua that does the same thing, which ask every time in every mode (Developer mode and full access
                // let a plain `Execute` through unasked). The question says what the command does and names the safer tool.
                if let Some(effect) = &effect {
                    for class in &effect.classes {
                        if !classes.contains(class) {
                            classes.push(*class);
                        }
                    }
                }
                let effect_note = effect.as_ref().map(rules::Effect::note).unwrap_or_default();
                let rule_note = format!("{effect_note}{}", rule_prompt.as_deref().map(|why| format!("[rule: {why}] ")).unwrap_or_default());
                // The question shows the whole command or the command is not run: what a question hides is where a harmful argument would be put.
                let shown = aqua_tool_host::shown_whole(&line, MAX_SHOWN_COMMAND).map_err(|n| {
                    ToolError::new(
                        ErrorCode::InvalidArguments,
                        format!("this command is {n} characters long and a question can show {MAX_SHOWN_COMMAND}: it is not run on a question that would hide part of it. Give it shorter, or put the long part in a file."),
                    )
                })?;
                let asked_low = tool == "term.run" && args.get("isolated").and_then(|v| v.as_bool()).unwrap_or(false);
                let tier = if asked_low { Tier::Low.max(self.0.tier) } else { self.0.tier };
                if tier != Tier::None && !cfg!(windows) {
                    return Err(ToolError::new(ErrorCode::UnsupportedPlatform, "isolated runs are only available on Windows"));
                }
                // The question says how contained the program will be; if MXC cannot be used here the program runs at Low integrity and the answer says so.
                let prefix = match tier {
                    Tier::None => "",
                    Tier::Low => "[isolated: can write only in its scratch folder] ",
                    Tier::Mxc => "[in an MXC container: no network, reads only its program's folder and the working folder, writes only its scratch folder] ",
                };
                // cargo writes its build output where the person pointed `CARGO_TARGET_DIR` (it comes from Aqua's own environment, never from the call): outside the folders the person allowed, the question says so.
                let target_note = build_output_outside_roots(&self.0.guard, &rules::program_name(&exe.to_string_lossy()), &env).map(|dir| format!(" (cargo writes its build output to {dir}, which is outside the folders you allowed)")).unwrap_or_default();
                Ok(Classification { classes, target: display(&exe), summary: format!("{rule_note}{prefix}Run `{shown}` in {}{target_note}", display(&cwd)), target_allowed: false })
            }
            "term.shell" => {
                let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let segs = rules::shell_segments(command).map_err(|m| ToolError::new(ErrorCode::InvalidArguments, m))?;
                let shown = segs.iter().map(|(e, a)| command_line(std::path::Path::new(e), a)).collect::<Vec<_>>().join(" && ");
                // The same effects as `term.run` reads, for each program of the line (the folder is the one the line runs in, when it can be told here; a wrong one is an error where the line runs).
                let cwd_arg = args.get("cwd").and_then(|v| v.as_str()).unwrap_or("");
                let cwd = if cwd_arg.is_empty() { self.0.guard.roots().first().map(|r| r.1.clone()) } else { self.0.guard.resolve(cwd_arg, Op::Read).ok().map(|r| r.path) };
                let effect = rules::merge_effects(segs.iter().flat_map(|(e, a)| std::iter::once((e.clone(), a.clone())).chain(rules::launched_chain(e, a))).filter_map(|(e, a)| rules::command_effect(&e, &a, cwd.as_deref())));
                let mut classes = vec![PermClass::Execute, PermClass::Destructive];
                if let Some(effect) = &effect {
                    for class in &effect.classes {
                        if !classes.contains(class) {
                            classes.push(*class);
                        }
                    }
                }
                let effect_note = effect.as_ref().map(rules::Effect::note).unwrap_or_default();
                Ok(Classification { classes, target: shown.clone(), summary: format!("{effect_note}Run shell `{shown}`"), target_allowed: false })
            }
            #[cfg(feature = "git")]
            "term.undo" => self.0.classify_undo(args),
            "term.output" | "term.sessions" => Ok(Classification { classes: vec![PermClass::Read], target: "background jobs".into(), summary: "Read a background job".into(), target_allowed: true }),
            "term.stop" => {
                let id = args.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
                let job = self.0.get(id)?;
                Ok(Classification { classes: vec![PermClass::Write], target: job.command_line.clone(), summary: format!("Stop job {id}: {}", job.command_line), target_allowed: true })
            }
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    fn call<'a>(&'a self, tool: &'a str, args: Value, ctx: &'a ExecCtx) -> ProviderFuture<'a, Result<ProviderOutput, ToolError>> {
        let cancel = ctx.cancel.clone();
        let inner = self.0.clone();
        let tool = tool.to_string();
        let timeout_ms = ctx.timeout_ms;
        let run_id = ctx.agent_run_id.clone();
        // The live output of the call (card OA7-04, TM-14): the host's sink, when the setting is on.
        let progress = if self.0.live_output() { ctx.progress.clone() } else { None };
        Box::pin(async move {
            // Everything here blocks (process waits, sleeps), so it runs off the async workers. The programs it starts get no more than what is left of the host's time for this call.
            tokio::task::spawn_blocking(move || {
                let _clock = CallClock::start(timeout_ms);
                inner.run_blocking(&tool, &args, &run_id, &cancel, progress)
            })
                .await
                .map_err(|e| ToolError::new(ErrorCode::BackendError, format!("terminal task failed: {e}")))?.map(ProviderOutput::marked_private)
        })
    }
}

fn out(data: Value, text: String) -> Result<ProviderOutput, ToolError> {
    Ok(ProviderOutput { data, text, ..Default::default() })
}

/// Puts a `.gitignore` of one line (`*`) in the scratch folder, once, so that git ignores the folder and everything in it (the file itself too). A folder that cannot be written
/// to is left as it was: the scratch folder still works, the person's `git status` just shows it.
#[cfg_attr(not(windows), allow(dead_code))]
fn keep_out_of_git(base: &std::path::Path) {
    let file = base.join(".gitignore");
    if !file.exists() {
        let _ = std::fs::write(&file, "# Scratch folder of the isolated runs of Aqua: never commit it.\n*\n");
    }
}

impl TermInner {
    fn run_blocking(&self, tool: &str, args: &Value, run_id: &str, cancel: &Arc<AtomicBool>, progress: Option<ProgressSink>) -> Result<ProviderOutput, ToolError> {
        // With the journal of commands on, a command that may change files is looked at before and after it (card OA7-03, TM-12); the programs of a `term.shell` line are one command.
        #[cfg(feature = "git")]
        if matches!(tool, "term.run" | "term.shell") {
            return self.journalled(tool, args, run_id, cancel, || if tool == "term.run" { self.do_run(args, cancel, progress) } else { self.do_shell(args, cancel, progress) });
        }
        #[cfg(not(feature = "git"))]
        let _ = run_id;
        match tool {
            "term.run" => self.do_run(args, cancel, progress),
            "term.shell" => self.do_shell(args, cancel, progress),
            #[cfg(feature = "git")]
            "term.undo" => self.do_undo(args, cancel),
            "term.start" => self.do_start(args, progress),
            "term.output" => self.do_output(args, cancel),
            "term.stop" => self.do_stop(args),
            "term.sessions" => self.do_sessions(),
            other => Err(ToolError::new(ErrorCode::NotFound, format!("unknown tool '{other}'"))),
        }
    }

    fn do_shell(&self, args: &Value, cancel: &Arc<AtomicBool>, progress: Option<ProgressSink>) -> Result<ProviderOutput, ToolError> {
        let command = args.get("command").and_then(|v| v.as_str()).unwrap_or("");
        let segs = rules::shell_segments(command).map_err(|m| ToolError::new(ErrorCode::InvalidArguments, m))?;
        // (Card OA4-03, TM-04.) `timeout_s` is the time of the whole line, which its programs share: before, each of up to four programs had all of it, so a line could run for four times
        // what the call allowed, and went on running after the host had ended the call. Left out it is 30 s for a program (120 s for a build or test program), added up, at most 120.
        let default_total: u64 = segs.iter().map(|(exe, argv)| if rules::long_running_program(exe, argv) { 120 } else { 30 }).sum();
        let total_s = args.get("timeout_s").and_then(|v| v.as_u64()).unwrap_or(default_total).clamp(1, 120);
        let started = Instant::now();
        let cwd = args.get("cwd").cloned().unwrap_or(Value::Null);
        let mut text = String::new();
        let mut last_code = 0i64;
        for (at, (exe, argv)) in segs.iter().enumerate() {
            // A call that was stopped, or whose time is used up, starts no more programs.
            let before = if text.is_empty() { String::new() } else { format!("\n--- what the programs before it wrote ---\n{text}") };
            if cancel.load(Ordering::SeqCst) {
                return Err(ToolError::new(ErrorCode::Cancelled, format!("the call was stopped before program {} of {} ({exe}) was started{before}", at + 1, segs.len())));
            }
            let left = Duration::from_secs(total_s).saturating_sub(started.elapsed());
            if left < Duration::from_millis(500) {
                return Err(ToolError::new(ErrorCode::Timeout, format!("the line used its {total_s} s before program {} of {} ({exe}) was started{before}", at + 1, segs.len())));
            }
            // The programs of a line are never kept as jobs: a line has no way to say which one would have been.
            let one = json!({"exe": exe, "args": argv, "cwd": cwd, "timeout_s": (left.as_millis() as u64).div_ceil(1000), "background_on_timeout": false});
            let ran = match self.do_run(&one, cancel, progress.clone()) {
                Ok(ran) => ran,
                // What the programs before it already did and wrote is not lost with the one that could not finish (it timed out, was stopped or would not start).
                Err(mut e) if at > 0 => {
                    e.message = format!("program {} of {} ({}) did not finish: {}\n--- what the programs before it wrote ---\n{}", at + 1, segs.len(), exe, e.message, text);
                    return Err(e);
                }
                Err(e) => return Err(e),
            };
            last_code = ran.data.get("exit_code").and_then(|v| v.as_i64()).unwrap_or(-1);
            if !text.is_empty() {
                text.push_str("\n");
            }
            text.push_str(&ran.text);
            if last_code != 0 {
                text.push_str("\nstopped because the previous program did not exit 0");
                break;
            }
        }
        out(json!({"exit_code": last_code, "segments": segs.len(), "command": command}), text)
    }

    fn do_run(&self, args: &Value, cancel: &Arc<AtomicBool>, progress: Option<ProgressSink>) -> Result<ProviderOutput, ToolError> {
        let Parsed { exe, argv, cwd, spawn_argv, .. } = self.parse_command(args)?;
        // (Card OA4-03, WF-08.) A build or test program is given 300 s when the call names no time, any other program 30 s. The class is read from the program as the caller wrote it.
        let given_args: Vec<String> = args.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let long = rules::long_running_program(args.get("exe").and_then(|v| v.as_str()).unwrap_or(""), &given_args);
        let timeout_s = args.get("timeout_s").and_then(|v| v.as_u64()).unwrap_or(if long { 300 } else { 30 }).clamp(1, 300);
        // At its time a command that is still running may be kept as a background job (card OA4-03, TM-04): when the call says so, or for a build or test program when the person turned it on.
        // With as many jobs running as may run, it is stopped at its time as it always was, and the answer says why it was not kept.
        let mut promote = args.get("background_on_timeout").and_then(|v| v.as_bool()).unwrap_or(long && self.auto_background());
        let mut not_kept = String::new();
        if promote {
            let running = self.running_jobs();
            if running >= self.cfg.max_sessions {
                promote = false;
                not_kept = format!(" It could not be kept as a background job: {running} are already running (the limit is {}); end one with term.stop first.", self.cfg.max_sessions);
            }
        }
        // A caller that sets `max_output_kb` gets that much (its start and its end); one that does not gets everything up to `RUN_CAPTURE_BYTES` in the text, for the host to cut and keep in a file,
        // and 64 KB of it in the data.
        let explicit_kb = args.get("max_output_kb").and_then(|v| v.as_u64());
        let cap_kb = explicit_kb.unwrap_or(64).clamp(1, 512) as usize;
        let capture = if explicit_kb.is_some() { cap_kb * 1024 } else { RUN_CAPTURE_BYTES };
        let line = command_line(&exe, &argv);
        let argv = spawn_argv;
        let asked = if args.get("isolated").and_then(|v| v.as_bool()).unwrap_or(false) { Tier::Low } else { Tier::None };
        let mut env = self.env_for(args)?;
        let mut cwd = cwd;
        let cwd_given = !args.get("cwd").and_then(|v| v.as_str()).unwrap_or("").is_empty();
        let contained = self.contain(asked, &mut cwd, cwd_given, &mut env)?;
        let isolated = contained.tier != Tier::None;
        let scratch = contained.scratch.clone();
        let cwd_shown = display(&cwd);
        let r = run_command_ex(
            contained.isolated,
            contained.wrap,
            exe,
            argv,
            cwd,
            env,
            args.get("stdin").and_then(|v| v.as_str()).map(String::from),
            self.limits(),
            Duration::from_secs(timeout_s),
            capture,
            true,
            cancel,
            promote,
            progress.clone(),
        )?;
        // What it had printed is not lost with a stop or a time limit: the last part is in the message the model reads (TM-02), the rest in the details.
        let printed = if r.last_output.is_empty() { String::new() } else { format!("\n--- the last of what it printed ---\n{}", r.last_output) };
        if r.cancelled {
            return Err(ToolError::new(ErrorCode::Cancelled, format!("the run was cancelled and the command was stopped{printed}")));
        }
        if let Some(job) = r.promoted.clone() {
            // The program goes on after this call: its live output is finished with it, not with the call (card OA7-04).
            if let Some(p) = &progress {
                p.detach();
            }
            return self.keep_as_job(job, &line, &r.limit_words(), &r.last_output);
        }
        if r.timed_out {
            return Err(ToolError::new(
                ErrorCode::Timeout,
                format!("the command ran longer than {} and was stopped (with everything it started).{not_kept} For a build or test that needs longer, run it with term.start and read it with term.output.{printed}", r.limit_words()),
            )
            .with_details(json!({"stdout": r.stdout, "stderr": r.stderr, "limit_s": r.limit_s})));
        }
        let mut text = format!("exit code {}", r.exit_code.map(|c| c.to_string()).unwrap_or_else(|| "unknown".into()));
        // A Windows error status is named on the line after the number (card OA7-01, TM-11); the number stays where the readers of the result look for it.
        if let Some(line) = r.exit_code.and_then(exit_codes::status_line) {
            text.push_str(&format!("
{line}"));
        }
        if let Some(note) = &r.limit_note {
            text.push_str(&format!("
{note}"));
        }
        if !r.stdout.is_empty() {
            text.push_str(&format!("\n--- stdout ---\n{}", r.stdout));
        }
        if !r.stderr.is_empty() {
            text.push_str(&format!("\n--- stderr ---\n{}", r.stderr));
        }
        if explicit_kb.is_some() && r.truncated {
            text.push_str(&format!("\n… output cut at {cap_kb} KB (the start and the end are kept)"));
        }
        if let Some(note) = &contained.note {
            text.push_str(&format!("\n[{note}]"));
        }
        let mut made: Vec<String> = Vec::new();
        if let Some(dir) = &scratch {
            made = files_in(dir, 20);
            if !made.is_empty() {
                text.push_str(&format!("
--- isolated: files it wrote in {} ---
{}", display(dir), made.join("
")));
            }
        }
        // The data holds the start and the end of each stream down to what `max_output_kb` says; the text above holds all that was kept, and the host cuts it for the model and keeps the rest in a file.
        let shorten = |s: &str| aqua_tool_host::output::head_tail(s, cap_kb * 1024).0;
        let mut data = json!({"exit_code": r.exit_code, "stdout": shorten(&r.stdout), "stderr": shorten(&r.stderr), "truncated": r.truncated, "duration_ms": r.duration_ms, "command": line, "cwd": cwd_shown, "isolated": isolated, "tier": contained.tier.as_str(), "scratch": scratch.as_ref().map(|d| display(d)), "files": made});
        if let Some(name) = r.exit_code.and_then(exit_codes::short) {
            data["exit_status"] = json!(name);
        }
        if let Some(note) = &r.limit_note {
            data["limit_note"] = json!(note);
        }
        out(data, text)
    }

    /// Background jobs that have not ended.
    fn running_jobs(&self) -> usize {
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner).values().filter(|j| !j.state.lock().unwrap_or_else(PoisonError::into_inner).done).count()
    }

    /// Whether the lines a program prints are shown while it runs (card OA7-04, TM-14): `live_output` of term.json, or `AQUA_TERM_LIVE_OUTPUT=on`. They go to the app's event stream of the run (the host
    /// cleans them like a result) and never to the model.
    fn live_output(&self) -> bool {
        self.cfg.live_output || matches!(std::env::var("AQUA_TERM_LIVE_OUTPUT").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }

    /// Whether the wider list of project checks counts (card OA4-05, D-07): `extended_project_checks` of term.json, or `AQUA_TERM_PROJECT_CHECKS=extended`.
    fn extended_checks(&self) -> bool {
        self.cfg.extended_project_checks || matches!(std::env::var("AQUA_TERM_PROJECT_CHECKS").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("extended"))
    }

    /// Whether one answer of "approve for this session" covers a project check's program and subcommand (card OA4-05, TM-16): `check_grants` of term.json, or `AQUA_TERM_CHECK_GRANTS=on`.
    fn check_grants_on(&self) -> bool {
        self.cfg.check_grants || matches!(std::env::var("AQUA_TERM_CHECK_GRANTS").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }

    /// The scope of the answer "approve for this session" for this call, when it is a project check in a trusted folder and the person turned `check_grants` on (card OA4-05, TM-16).
    fn grant_scope_of(&self, tool: &str, args: &Value) -> Option<String> {
        if !matches!(tool, "term.run" | "term.start") || !self.check_grants_on() {
            return None;
        }
        let parsed = self.parse_command(args).ok()?;
        // A command that a rule of term_rules.json asks about, or that does more than run code (a push, a publish), keeps its own question: only a plain project check is covered.
        if !parsed.project_check || parsed.rule_prompt.is_some() || parsed.effect.is_some() || !self.cwd_is_trusted(&parsed.cwd) {
            return None;
        }
        let given: Vec<String> = args.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        arity::scope_of(args.get("exe").and_then(|v| v.as_str()).unwrap_or(""), &given, &display(&parsed.cwd))
    }

    /// What the call is when it is a project's own check that a pre-approval of the person may answer (card OA10-05, WF-14): `term.run` or `term.start` of a command line that is a project
    /// check (`rules::is_project_check`, or the wider list when that is on), that no rule of `term_rules.json` asks about, that does nothing beyond running it (no push, no publish), run
    /// in a folder the file guard trusts. The kind is read from the command line and the folder is the one the command runs in. Anything else is `None`: it asks as it always did.
    fn project_check_of(&self, tool: &str, args: &Value) -> Option<aqua_tool_host::ProjectCheck> {
        if !matches!(tool, "term.run" | "term.start") {
            return None;
        }
        let parsed = self.parse_command(args).ok()?;
        if !parsed.project_check || parsed.rule_prompt.is_some() || parsed.effect.is_some() || !self.cwd_is_trusted(&parsed.cwd) {
            return None;
        }
        let given: Vec<String> = args.get("args").and_then(|v| v.as_array()).map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect()).unwrap_or_default();
        let kind = arity::check_kind(args.get("exe").and_then(|v| v.as_str()).unwrap_or(""), &given)?;
        Some(aqua_tool_host::ProjectCheck { kind, folder: display(&parsed.cwd) })
    }

    /// Whether a build or test run is kept as a job at its time when the call says nothing: `auto_background` of term.json, or `AQUA_TERM_AUTO_BACKGROUND=on`.
    fn auto_background(&self) -> bool {
        self.cfg.auto_background || matches!(std::env::var("AQUA_TERM_AUTO_BACKGROUND").ok().as_deref().map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("on" | "1" | "true"))
    }

    /// A `term.run` that was still running at its time becomes a background job (card OA4-03, TM-04): the program is not stopped, its output so far stays and goes on being
    /// kept, `term.output` pages it and `term.stop` ends it, and it is in `term.sessions`. It lives as long as a job does (3,600 s at most), under the same limits and tier it started with.
    fn keep_as_job(&self, job: Arc<Job>, line: &str, limit: &str, last_output: &str) -> Result<ProviderOutput, ToolError> {
        let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        let running = jobs.values().filter(|j| !j.state.lock().unwrap_or_else(PoisonError::into_inner).done).count();
        let printed = if last_output.is_empty() { String::new() } else { format!("\n--- the last of what it printed ---\n{last_output}") };
        if running >= self.cfg.max_sessions {
            drop(jobs);
            // The room was taken while it ran: it is ended, and the answer says what it printed.
            job.kill.store(true, Ordering::SeqCst);
            let until = Instant::now() + Duration::from_secs(5);
            while Instant::now() < until && !job.state.lock().unwrap_or_else(PoisonError::into_inner).done {
                std::thread::sleep(Duration::from_millis(25));
            }
            return Err(ToolError::new(
                ErrorCode::Timeout,
                format!("the command ran longer than {limit} and was stopped (with everything it started). It could not be kept as a background job: {running} are already running (the limit is {}); end one with term.stop first.{printed}", self.cfg.max_sessions),
            ));
        }
        {
            let mut st = job.state.lock().unwrap_or_else(PoisonError::into_inner);
            // A one-shot run keeps the start of its output apart from the rest; a job is read page by page from the first line it still holds.
            if st.keep_end {
                let mut all: VecDeque<(u64, bool, String)> = st.head.drain(..).collect();
                all.extend(st.lines.drain(..));
                st.bytes = all.iter().map(|(_, _, t)| t.len() + 1).sum();
                st.lines = all;
                st.head_bytes = 0;
                st.keep_end = false;
                st.stop_at_cap = false;
                st.cap_bytes = st.cap_bytes.max(RING_BYTES);
            }
        }
        let id = format!("job_{}", self.counter.fetch_add(1, Ordering::SeqCst));
        let kept = Arc::new(Job { id: id.clone(), pid: job.pid, name: line.to_string(), command_line: job.command_line.clone(), cwd: job.cwd.clone(), started: job.started, state: job.state.clone(), kill: job.kill.clone() });
        let ran_s = job.started.elapsed().as_secs();
        jobs.insert(id.clone(), kept);
        out(
            json!({"job_id": id, "state": "running", "promoted": true, "pid": job.pid, "command": line, "ran_s": ran_s, "max_runtime_s": PROMOTED_RUN_SECS}),
            format!(
                "{id} is still running: `{line}` had not finished in {limit}, so it was kept as a background job and was NOT stopped. Read what it prints with term.output job_id={id} (wait_ms up to 10000 waits for new lines), end it with term.stop job_id={id}; it ends by itself after {PROMOTED_RUN_SECS} s at the most.{printed}"
            ),
        )
    }

    fn do_start(&self, args: &Value, progress: Option<ProgressSink>) -> Result<ProviderOutput, ToolError> {
        {
            let mut jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
            // Finished jobs are dropped after their output has been available for a while.
            jobs.retain(|_, j| {
                let st = j.state.lock().unwrap_or_else(PoisonError::into_inner);
                !(st.done && j.started.elapsed() > Duration::from_secs(3600))
            });
            let running = jobs.values().filter(|j| !j.state.lock().unwrap_or_else(PoisonError::into_inner).done).count();
            if running >= self.cfg.max_sessions {
                return Err(ToolError::new(ErrorCode::ResourceLimit, format!("{running} background jobs are already running (limit {}); stop one first", self.cfg.max_sessions)));
            }
        }
        let Parsed { exe, argv, cwd, spawn_argv, .. } = self.parse_command(args)?;
        let max_runtime = args.get("max_runtime_s").and_then(|v| v.as_u64()).unwrap_or(600).clamp(1, 3600);
        let line = command_line(&exe, &argv);
        let id = format!("job_{}", self.counter.fetch_add(1, Ordering::SeqCst));
        let name = args.get("name").and_then(|v| v.as_str()).filter(|n| !n.is_empty()).unwrap_or(&line).to_string();
        let mut env = self.env_for(args)?;
        let mut cwd = cwd;
        let cwd_given = !args.get("cwd").and_then(|v| v.as_str()).unwrap_or("").is_empty();
        // Only the person's default tier applies to a background job (a call cannot ask for `isolated` on `term.start`).
        let contained = self.contain(Tier::None, &mut cwd, cwd_given, &mut env)?;
        // The job goes on after this call has answered: its live output is finished with the job, not with the call (card OA7-04).
        if let Some(p) = &progress {
            p.detach();
        }
        let spec = Spec { isolated: contained.isolated, wrap: contained.wrap, exe, args: spawn_argv, cwd, env, stdin: None, limits: self.limits(), max_runtime: Duration::from_secs(max_runtime), cap_bytes: RING_BYTES, stop_at_cap: false, keep_end: false, max_line_chars: MAX_LINE_CHARS, progress };
        let job = spawn(spec, id.clone(), name, line.clone())?;
        let job_pid = job.pid;
        self.jobs.lock().unwrap_or_else(PoisonError::into_inner).insert(id.clone(), job);
        let note = contained.note.as_ref().map(|n| format!(" [{n}]")).unwrap_or_default();
        out(
            json!({"job_id": id, "pid": job_pid, "command": line, "max_runtime_s": max_runtime, "tier": contained.tier.as_str()}),
            format!("Started {id} (pid {job_pid}): {line}. Read it with term.output job_id={id}; stop it with term.stop.{note}"),
        )
    }

    fn do_output(&self, args: &Value, cancel: &Arc<AtomicBool>) -> Result<ProviderOutput, ToolError> {
        let id = args.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
        let job = self.get(id)?;
        let after = args.get("after_seq").and_then(|v| v.as_i64()).unwrap_or(-1);
        let wait = Duration::from_millis(args.get("wait_ms").and_then(|v| v.as_u64()).unwrap_or(0).min(10_000));
        // A page never holds more than the host shows (`OUTPUT_PAGE_MAX_CHARS`), and the header line and the line that says where to continue are paid for out of it: a page that is cut by
        // the host is cut in its middle, and the cursor has already moved past the lines that were lost.
        let page_chars = (args.get("max_chars").and_then(|v| v.as_u64()).unwrap_or(4000) as usize).min(OUTPUT_PAGE_MAX_CHARS);
        let max_chars = page_chars.saturating_sub(PAGE_OVERHEAD_CHARS).max(80);
        let until = Instant::now() + wait;
        loop {
            let st = job.state.lock().unwrap_or_else(PoisonError::into_inner);
            let fresh = st.lines.iter().any(|(s, _, _)| *s as i64 > after);
            if fresh || st.done || Instant::now() >= until || cancel.load(Ordering::SeqCst) {
                break;
            }
            drop(st);
            std::thread::sleep(Duration::from_millis(50));
        }
        let st = job.state.lock().unwrap_or_else(PoisonError::into_inner);
        let mut lines = Vec::new();
        let mut used = 0usize;
        let mut last = after;
        let mut cut = false;
        for (seq, is_err, t) in st.lines.iter().filter(|(s, _, _)| *s as i64 > after) {
            let l = if *is_err { format!("[err] {t}") } else { t.clone() };
            if used + l.chars().count() > max_chars {
                if lines.is_empty() {
                    // One line longer than a whole page: it is given cut to the page and the cursor moves past it. It used to be held back, so
                    // the answer was an empty page that said to continue from where it already was, for ever.
                    let shown: String = t.chars().take(max_chars.saturating_sub(7 + 6).max(1)).collect();
                    used += shown.chars().count() + 1;
                    last = *seq as i64;
                    lines.push(json!({"seq": seq, "stderr": is_err, "text": format!("{shown}… [cut to fit the page]")}));
                    continue;
                }
                cut = true;
                break;
            }
            used += l.chars().count() + 1;
            last = *seq as i64;
            lines.push(json!({"seq": seq, "stderr": is_err, "text": t}));
        }
        let state = if st.done { if st.timed_out { "timed_out" } else if st.killed { "stopped" } else { "exited" } } else { "running" };
        let body = lines.iter().map(|l| format!("{}{}", if l["stderr"].as_bool().unwrap_or(false) { "[err] " } else { "" }, l["text"].as_str().unwrap_or(""))).collect::<Vec<_>>().join("\n");
        let mut text = format!("job {id} is {state}{}", st.exit_code.map(|c| format!(" (exit code {c}{})", exit_codes::short(c).map(|n| format!(", {n}")).unwrap_or_default())).unwrap_or_default());
        if let Some(note) = &st.limit_note {
            text.push_str(&format!("
{note}"));
        }
        if !body.is_empty() {
            text.push_str(&format!("\n{body}"));
        }
        if cut {
            text.push_str(&format!("\n… more output; continue with after_seq={last}"));
        }
        out(json!({"job_id": id, "state": state, "exit_code": st.exit_code, "lines": lines, "last_seq": last, "more": cut}), text)
    }

    fn do_stop(&self, args: &Value) -> Result<ProviderOutput, ToolError> {
        let id = args.get("job_id").and_then(|v| v.as_str()).unwrap_or("");
        let job = self.get(id)?;
        job.kill.store(true, Ordering::SeqCst);
        let until = Instant::now() + Duration::from_secs(5);
        while Instant::now() < until && !job.state.lock().unwrap_or_else(PoisonError::into_inner).done {
            std::thread::sleep(Duration::from_millis(25));
        }
        let done = job.state.lock().unwrap_or_else(PoisonError::into_inner).done;
        out(json!({"job_id": id, "stopped": done}), if done { format!("Stopped {id} and everything it started.") } else { format!("{id} did not stop within 5 s; try again.") })
    }

    fn do_sessions(&self) -> Result<ProviderOutput, ToolError> {
        let jobs = self.jobs.lock().unwrap_or_else(PoisonError::into_inner);
        let mut rows: Vec<Value> = jobs
            .values()
            .map(|j| {
                let st = j.state.lock().unwrap_or_else(PoisonError::into_inner);
                json!({"job_id": j.id, "pid": j.pid, "name": j.name, "command": j.command_line, "cwd": j.cwd, "state": if st.done { "exited" } else { "running" }, "exit_code": st.exit_code, "age_s": j.started.elapsed().as_secs()})
            })
            .collect();
        rows.sort_by(|a, b| a["job_id"].as_str().cmp(&b["job_id"].as_str()));
        let text = rows.iter().map(|r| format!("{} {} {} [{}]", r["job_id"].as_str().unwrap_or(""), r["state"].as_str().unwrap_or(""), r["command"].as_str().unwrap_or(""), r["age_s"])).collect::<Vec<_>>().join("\n");
        out(json!({ "sessions": rows }), if text.is_empty() { "No background jobs.".into() } else { text })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn lines_of(input: &[u8], max_bytes: usize) -> Vec<(String, bool)> {
        let mut r = BufReader::with_capacity(8, Cursor::new(input.to_vec()));
        let mut out = Vec::new();
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match read_line_bounded(&mut r, &mut buf, max_bytes).unwrap() {
                (false, _) => return out,
                (true, cut) => out.push((String::from_utf8_lossy(&buf).to_string(), cut)),
            }
        }
    }

    #[test]
    fn lines_are_read_whole_however_the_stream_is_cut_up_and_a_long_one_is_kept_to_its_start() {
        // A reader with an 8-byte buffer: every line is read in several pieces.
        assert_eq!(lines_of(b"one\ntwo\r\n\nlast without an end", 100), [("one".into(), false), ("two\r".into(), false), ("".into(), false), ("last without an end".into(), false)]);
        assert_eq!(lines_of(b"", 100), []);
        assert_eq!(lines_of(b"\n", 100), [("".into(), false)]);
        // Exactly the limit is not a cut; one more is, and what follows the long line is intact.
        assert_eq!(lines_of(b"12345678\nabc\n", 8), [("12345678".into(), false), ("abc".into(), false)]);
        assert_eq!(lines_of(b"123456789\nabc\n", 8), [("12345678".into(), true), ("abc".into(), false)]);
        assert_eq!(lines_of(b"123456789", 8), [("12345678".into(), true)], "a long last line with no end");
        // A line of a million bytes keeps its first 16 and no more of the buffer.
        let mut huge = vec![b'x'; 1_000_000];
        huge.extend_from_slice(b"\nnext\n");
        assert_eq!(lines_of(&huge, 16), [("x".repeat(16), true), ("next".into(), false)]);
    }

    /// A filter that writes back what it reads, at once: it stops when its output is not read, and then it stops reading its input.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper program: the stdin tests below start this test binary with --ignored --exact"]
    fn helper_echo_stdin() {
        use std::io::{Read, Write};
        println!();
        let mut stdin = std::io::stdin().lock();
        let mut stdout = std::io::stdout().lock();
        let mut buf = [0u8; 4096];
        loop {
            let n = stdin.read(&mut buf).unwrap();
            if n == 0 {
                break;
            }
            stdout.write_all(&buf[..n]).unwrap();
            stdout.flush().unwrap();
        }
    }

    /// A program that is given input and never reads it.
    #[cfg(windows)]
    #[test]
    #[ignore = "helper program: the stdin tests below start this test binary with --ignored --exact"]
    fn helper_hang_without_reading() {
        println!("STARTED");
        std::thread::sleep(Duration::from_secs(120));
    }

    /// Start the helper of this test binary with `stdin` on a thread of its own and wait `secs` for it: the call that is tested could not be stopped at all, and a test that waits for it would not end.
    #[cfg(windows)]
    fn run_helper_within(secs: u64, helper: &str, stdin: String, timeout: Duration) -> RunResult {
        static NEVER: AtomicBool = AtomicBool::new(false);
        let (tx, rx) = std::sync::mpsc::channel();
        let args = vec!["--ignored".to_string(), "--exact".to_string(), format!("term::tests::{helper}"), "--nocapture".to_string()];
        let name = helper.to_string();
        std::thread::spawn(move || {
            let ran = run_command_ex(
                false,
                None,
                std::env::current_exe().unwrap(),
                args,
                std::env::temp_dir(),
                std::env::vars().collect(),
                Some(stdin),
                Limits { memory_mb: 512, max_processes: 8 },
                timeout,
                RUN_CAPTURE_BYTES,
                true,
                &NEVER,
                false,
                None,
            );
            let _ = tx.send(ran);
        });
        match rx.recv_timeout(Duration::from_secs(secs)) {
            Ok(ran) => ran.unwrap_or_else(|e| panic!("{name} did not start: {} {}", e.code, e.message)),
            Err(_) => panic!("{name} did not return in {secs} s: the input was written before anything read the output, and nothing could stop it"),
        }
    }

    /// OA7-01 (TM-20). More input than a pipe holds, written to a program that writes while it reads. The input used to be written before the readers and the waiter were started, so the
    /// output pipe filled, the program stopped reading, the write never ended, and there was no timeout and no way to stop it.
    #[cfg(windows)]
    #[test]
    fn a_filter_that_echoes_its_input_while_it_is_still_written_finishes() {
        let text = "0123456789abcdef\n".repeat(250_000);
        let started = Instant::now();
        let ran = run_helper_within(60, "helper_echo_stdin", text.clone(), Duration::from_secs(50));
        assert!(!ran.timed_out && ran.exit_code == Some(0), "{:?} {:?}", ran.exit_code, ran.stderr);
        assert_eq!(ran.stdout.matches("0123456789abcdef").count(), 250_000, "every line came back once ({} bytes)", ran.stdout.len());
        assert!(started.elapsed() < Duration::from_secs(30), "{:?}", started.elapsed());
    }

    /// OA7-01 (TM-20). A program that never reads its input is stopped at its timeout, with the whole tree, and the call returns.
    #[cfg(windows)]
    #[test]
    fn a_hung_program_that_is_given_input_it_does_not_read_is_killed_at_its_timeout() {
        let started = Instant::now();
        let ran = run_helper_within(60, "helper_hang_without_reading", "x".repeat(4 * 1024 * 1024), Duration::from_secs(2));
        assert!(ran.timed_out, "{:?}", ran.exit_code);
        assert!(started.elapsed() < Duration::from_secs(15), "{:?}", started.elapsed());
    }

    /// A few bytes of the OEM code page 437 (and 850, which agrees on these): what a stand-in for the conversion of Windows says, so that this test needs no particular computer.
    fn oem437(bytes: &[u8]) -> Option<String> {
        Some(bytes.iter().map(|b| match b {
            0x82 => 'é',
            0x85 => 'à',
            0x87 => 'ç',
            0x81 => 'ü',
            b if b.is_ascii() => *b as char,
            _ => '?',
        }).collect())
    }

    /// OA7-01 (TM-09): `CARGO_TARGET_DIR` that points outside the folders the person allowed is said in the question, for cargo and no other program.
    #[test]
    fn cargo_output_that_goes_outside_the_roots_is_named_in_the_question() {
        let dir = std::env::temp_dir().join(format!("aqua_term_target_note_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("ws").join("target")).unwrap();
        std::fs::create_dir_all(dir.join("elsewhere")).unwrap();
        let guard = PathGuard::new(crate::fs::FsConfig::with_workspace(dir.join("ws").to_string_lossy().to_string()), vec![]).unwrap();
        let env = |v: &str| vec![("PATH".to_string(), "p".to_string()), ("CARGO_TARGET_DIR".to_string(), v.to_string())];
        let outside = dir.join("elsewhere").join("build");
        let said = build_output_outside_roots(&guard, "cargo", &env(&outside.to_string_lossy())).expect("a folder outside the roots is named");
        assert!(said.to_lowercase().replace('\\', "/").ends_with("elsewhere/build"), "{said}");
        // Inside a root (a folder that is there, or one that is not there yet), nothing is said.
        assert_eq!(build_output_outside_roots(&guard, "cargo", &env(&dir.join("ws").join("target").to_string_lossy())), None);
        assert_eq!(build_output_outside_roots(&guard, "cargo", &env(&dir.join("ws").join("not_yet").join("deeper").to_string_lossy())), None);
        // Another program, an empty value, and a call with no such variable.
        assert_eq!(build_output_outside_roots(&guard, "git", &env(&outside.to_string_lossy())), None);
        assert_eq!(build_output_outside_roots(&guard, "cargo", &env("  ")), None);
        assert_eq!(build_output_outside_roots(&guard, "cargo", &[("PATH".to_string(), "p".to_string())]), None);
        assert!(build_output_outside_roots(&guard, "cargo-clippy", &env(&outside.to_string_lossy())).is_some());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OA7-01 (TM-09): what is UTF-8 stays UTF-8 (a Devanagari line must not be read as a code page), a line that is not is read in the OEM code page, and a line cut in the middle of a
    /// character loses that character and not the whole line.
    #[test]
    fn oem_bytes_are_decoded() {
        let hindi = "हिन्दी";
        assert_eq!(decode_with(hindi.as_bytes(), false, oem437), hindi);
        assert_eq!(decode_with(b"plain ascii", false, oem437), "plain ascii");
        assert_eq!(decode_with(b"", false, oem437), "");
        // `ipconfig` and `tasklist` on a computer that is not set to English: bytes that are no UTF-8.
        assert_eq!(decode_with(b"caf\x82 \x85 la carte \x87a", false, oem437), "café à la carte ça");
        assert_eq!(decode_with(&[0x82], false, oem437), "é");
        // The conversion that fails gives what the lossy one would have given, not nothing.
        assert_eq!(decode_with(b"a\x82b", false, |_| None), "a\u{FFFD}b");
        // A line cut at a byte limit in the middle of a three-byte character: the unfinished bytes go, the rest is UTF-8, and nothing is read as a code page.
        let mut cut = format!("start {hindi}").into_bytes();
        cut.truncate(cut.len() - 2);
        let text = decode_with(&cut, true, |_| panic!("a cut UTF-8 line was read as the OEM code page"));
        assert!(text.starts_with("start ह") && !text.contains('\u{FFFD}'), "{text}");
        // The same bytes in a line that was not cut are not UTF-8 and are a code page text.
        assert!(decode_with(&cut, false, oem437).contains('?'));
        // A cut line that is wrong in its middle is not UTF-8 at all.
        assert_eq!(decode_with(b"x\x82y\xE0\xA4", true, oem437), "xéy??");
    }

    /// The same on this computer, with the real conversion of Windows: the bytes of a console program that prints `é à ç` in the OEM code page.
    #[cfg(windows)]
    #[test]
    fn the_oem_code_page_of_this_computer_reads_its_own_bytes() {
        #[link(name = "kernel32")]
        extern "system" {
            fn GetOEMCP() -> u32;
        }
        let page = unsafe { GetOEMCP() };
        let text = decode_line(&[b'O', b'K', b' ', 0x82, 0x85, 0x87], false);
        assert!(text.starts_with("OK ") && !text.contains('\u{FFFD}'), "code page {page}: {text:?}");
        if matches!(page, 437 | 850 | 858) {
            assert_eq!(text, "OK éàç", "code page {page}");
        }
        assert_eq!(decode_line("हिन्दी".as_bytes(), false), "हिन्दी");
    }

    #[test]
    fn the_scratch_folder_of_isolated_runs_ignores_itself_in_git_once_and_never_overwrites_a_file_the_person_changed() {
        let dir = std::env::temp_dir().join(format!("aqua_term_sandbox_ignore_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let base = dir.join(".aqua_sandbox");
        std::fs::create_dir_all(&base).unwrap();
        keep_out_of_git(&base);
        let text = std::fs::read_to_string(base.join(".gitignore")).unwrap();
        assert!(text.lines().any(|l| l == "*"), "{text}");
        // A file the person wrote there is theirs.
        std::fs::write(base.join(".gitignore"), "mine\n").unwrap();
        keep_out_of_git(&base);
        assert_eq!(std::fs::read_to_string(base.join(".gitignore")).unwrap(), "mine\n");
        // A folder that is not there is not made by it and is no error.
        keep_out_of_git(&dir.join("nowhere"));
        assert!(!dir.join("nowhere").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Found in a review: every line was cut at 4096 characters, so `terraform show -json` (the whole plan on one line) could not be read and no
    /// plan of any size could be applied; the same cut made a job's page stall.
    #[cfg(windows)]
    #[test]
    fn a_one_shot_run_keeps_a_long_line_whole_and_a_job_page_moves_past_one() {
        let dir = std::env::temp_dir().join(format!("aqua_term_long_line_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("one_line.json");
        let long = format!("{{\"plan\":\"{}\"}}", "p".repeat(60_000));
        std::fs::write(&file, &long).unwrap();
        let run = |cap: usize| {
            run_command(
                PathBuf::from("cmd.exe"),
                vec!["/c".into(), "type".into(), file.to_string_lossy().to_string()],
                dir.clone(),
                std::env::vars().collect(),
                None,
                Limits { memory_mb: 512, max_processes: 8 },
                Duration::from_secs(30),
                cap,
                &AtomicBool::new(false),
            )
            .unwrap()
        };
        let whole = run(1024 * 1024);
        assert_eq!(whole.exit_code, Some(0));
        assert_eq!(whole.stdout, long, "the line is as the program printed it");
        assert!(!whole.truncated);
        serde_json::from_str::<Value>(&whole.stdout).expect("a plan on one line can be read as JSON");
        // A run whose whole output is over its cap says so, rather than handing over half a line.
        let small = run(20_000);
        assert!(small.truncated && small.stdout.is_empty(), "{} bytes kept", small.stdout.len());
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// OA1-02 (TM-01). `spawn()` once refused any folder outside the folder the process started in. The core runs from its own folder (`target\release`), git works in the person's
    /// repository and docker, ssh and the cloud tools in scratch folders of Aqua's own: none of them is under it. Who may run where is decided before `spawn()` (the path guard resolves the
    /// folder in `parse_command`), so a folder `spawn()` is handed is started in, and one that vanished since is an error, not a panic.
    #[cfg(windows)]
    #[test]
    fn a_program_starts_in_the_folder_it_is_given_wherever_that_is_and_a_vanished_folder_is_an_error() {
        let dir = std::env::temp_dir().join(format!("aqua_term_any_cwd_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let process_dir = std::env::current_dir().unwrap().canonicalize().unwrap();
        assert!(!dir.canonicalize().unwrap().starts_with(&process_dir), "the test needs a folder outside the process folder");
        let run = |cwd: &Path| {
            run_command(PathBuf::from("cmd.exe"), vec!["/c".into(), "cd".into()], cwd.to_path_buf(), std::env::vars().collect(), None, Limits { memory_mb: 512, max_processes: 8 }, Duration::from_secs(30), 64 * 1024, &AtomicBool::new(false))
        };
        let ran = match run(&dir) {
            Ok(r) => r,
            Err(e) => panic!("a folder outside the process folder was refused: {} {}", e.code, e.message),
        };
        assert_eq!(ran.exit_code, Some(0));
        let name = dir.file_name().unwrap().to_string_lossy().to_lowercase();
        assert!(ran.stdout.trim().to_lowercase().ends_with(&name), "it did not run in the folder it was given: {}", ran.stdout);
        // The folder is gone between the check and the start.
        std::fs::remove_dir_all(&dir).unwrap();
        match run(&dir) {
            Ok(r) => panic!("started in a folder that is not there: {}", r.stdout),
            Err(e) => assert!(matches!(e.code, ErrorCode::NotFound | ErrorCode::BackendError), "{} {}", e.code, e.message),
        }
    }

    /// Card OA5-02 (TM-02): a one-shot run that prints more than it keeps loses its middle, not its end. (`run_command`, which the other tools use, still keeps the start and drops the rest.)
    #[cfg(windows)]
    #[test]
    fn a_one_shot_run_that_prints_more_than_it_keeps_loses_its_middle_and_not_its_end() {
        let dir = std::env::temp_dir().join(format!("aqua_term_keep_end_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("big.txt");
        let mut text: String = (0..20_000).map(|i| format!("line {i:05} of the output\n")).collect();
        text.push_str("THE LAST LINE");
        std::fs::write(&file, &text).unwrap();
        let run = |cap: usize, keep_end: bool| {
            run_command_ex(
                false,
                None,
                PathBuf::from("cmd.exe"),
                vec!["/c".into(), "type".into(), file.to_string_lossy().to_string()],
                dir.clone(),
                std::env::vars().collect(),
                None,
                Limits { memory_mb: 512, max_processes: 8 },
                Duration::from_secs(30),
                cap,
                keep_end,
                &AtomicBool::new(false),
                false,
                None,
            )
            .unwrap()
        };
        let kept = run(8 * 1024, true);
        assert_eq!(kept.exit_code, Some(0));
        assert!(kept.truncated);
        assert!(kept.stdout.starts_with("line 00000 of the output\n"), "{}", &kept.stdout[..40]);
        assert!(kept.stdout.ends_with("line 19999 of the output\nTHE LAST LINE"), "...{}", &kept.stdout[kept.stdout.len() - 60..]);
        assert!(kept.stdout.len() < 8 * 1024 + 800, "{} bytes kept", kept.stdout.len());
        // It says how many lines are missing, once, between the start and the end; the lines on either side are in order and none is twice.
        let marker = kept.stdout.lines().filter(|l| l.starts_with("[\u{2026} ") && l.ends_with(" lines cut \u{2026}]")).collect::<Vec<_>>();
        assert_eq!(marker.len(), 1, "{marker:?}");
        let hidden: usize = marker[0].trim_start_matches("[\u{2026} ").split(' ').next().unwrap().parse().unwrap();
        let shown = kept.stdout.lines().filter(|l| l.starts_with("line ")).count() + 1;
        assert_eq!(shown + hidden, 20_001, "{shown} shown and {hidden} cut are every line of the output once");
        let numbers: Vec<u32> = kept.stdout.lines().filter_map(|l| l.strip_prefix("line ")?.split(' ').next()?.parse().ok()).collect();
        assert!(numbers.windows(2).all(|w| w[0] < w[1]), "in order, none twice");
        // The last of the output, as a timeout shows it.
        assert!(kept.last_output.ends_with("THE LAST LINE") && kept.last_output.chars().count() <= LAST_OUTPUT_CHARS, "{}", kept.last_output.chars().count());
        // An output that fits is whole, with no marker.
        let whole = run(8 * 1024 * 1024, true);
        assert!(!whole.truncated && !whole.stdout.contains("lines cut") && whole.stdout.lines().count() == 20_001, "{}", whole.stdout.lines().count());
        assert_eq!(whole.stdout, text);
        // Without the flag the end is what is lost: this is how the other tools that run a program still behave.
        let head_only = run(8 * 1024, false);
        assert!(head_only.truncated && head_only.stdout.starts_with("line 00000") && !head_only.stdout.contains("THE LAST LINE"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
