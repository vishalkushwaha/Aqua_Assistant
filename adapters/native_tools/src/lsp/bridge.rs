//! The servers of the runs that are going on (card OA12-04): at most one process for a language in a run, started when a call first needs it, ended when it has been idle, when its run ends, when
//! it did not answer, or when the host goes away.
//!
//! * A server is started only for a call that was classified as a launch (the tool door asked, or the policy said no question was needed): the bridge records the project folders a launch
//!   was made for in a run, and the provider classifies a call for a folder not in the record as one that runs a program. The record only grows during a run, so a call classified as a read
//!   never finds itself starting a process.
//! * A server that did not answer in time, wrote what is not the protocol, ended, or could not be started marks its language **unavailable for the rest of the run**: the model is told so and
//!   falls back to `code.symbols`, `code.refs` and `fs.grep`; a new run tries again (and asks again).
//! * A run may make at most `max_requests_per_run` requests to servers.
//! * A thread looks every quarter of a second for a server that has been idle for its `idle_secs`, or that ended by itself, and ends it (asking it to shut down first, for a moment).
//! * The whole process tree of a server is ended when its run ends ([`Bridge::end_run`], which the host calls), when it is stopped, and when the bridge is dropped; on Windows the Job Object
//!   of each process ends the tree whatever else happens to this process.

use super::client::{verify_program, LspError, Server};
use super::config::{LspConfig, ServerSpec};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

type Key = (String, String);

struct Live {
    server: Arc<Server>,
    root: PathBuf,
    last_used: Instant,
    busy: u32,
    idle: Duration,
}

#[derive(Default)]
struct State {
    live: HashMap<Key, Live>,
    /// The project folders (as [`root_key`] writes them) a server was started for, by run and language.
    launched: HashMap<Key, HashSet<String>>,
    broken: HashMap<Key, String>,
    spent: HashMap<String, u32>,
}

struct Shared {
    config: LspConfig,
    may_write: Box<dyn Fn(&Path) -> bool + Send + Sync>,
    state: Mutex<State>,
    start_lock: Mutex<()>,
    reaper_started: AtomicBool,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A project folder, written so that two spellings of it are the same text.
pub fn root_key(root: &Path) -> String {
    let s = root.to_string_lossy().replace('\\', "/");
    let s = s.trim_end_matches('/');
    if cfg!(windows) {
        s.to_lowercase()
    } else {
        s.to_string()
    }
}

/// The bridge: what the provider asks for a server and gives back.
#[derive(Clone)]
pub struct Bridge {
    shared: Arc<Shared>,
}

/// A server in use by one call. While it is held the server is not idle.
pub struct Lease {
    shared: Arc<Shared>,
    key: Key,
    server: Arc<Server>,
    /// This call started the process.
    pub launched_now: bool,
}

impl Lease {
    pub fn server(&self) -> &Server {
        &self.server
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        let mut st = lock(&self.shared.state);
        if let Some(l) = st.live.get_mut(&self.key) {
            if Arc::ptr_eq(&l.server, &self.server) {
                l.busy = l.busy.saturating_sub(1);
                l.last_used = Instant::now();
            }
        }
    }
}

impl Bridge {
    /// `may_write` says whether the file tools may write to a path: a program there is never started.
    pub fn new(config: LspConfig, may_write: impl Fn(&Path) -> bool + Send + Sync + 'static) -> Bridge {
        Bridge { shared: Arc::new(Shared { config, may_write: Box::new(may_write), state: Mutex::new(State::default()), start_lock: Mutex::new(()), reaper_started: AtomicBool::new(false) }) }
    }

    pub fn config(&self) -> &LspConfig {
        &self.shared.config
    }

    /// Was a server of this language started in this run for this project folder? (If not, a call that needs one is a launch.)
    pub fn has_launched(&self, run: &str, language: &str, root: &Path) -> bool {
        lock(&self.shared.state).launched.get(&(run.to_string(), language.to_string())).is_some_and(|roots| roots.contains(&root_key(root)))
    }

    /// Why the server of this language is no longer used in this run, when it is not.
    pub fn broken_reason(&self, run: &str, language: &str) -> Option<String> {
        lock(&self.shared.state).broken.get(&(run.to_string(), language.to_string())).cloned()
    }

    /// The server of `language` for `root` in `run`: the one that is running, or one started now. Starting checks the program's hash first (see [`verify_program`]).
    pub fn lease(&self, run: &str, language: &str, root: &Path, cancel: &AtomicBool) -> Result<Lease, LspError> {
        let key: Key = (run.to_string(), language.to_string());
        let spec = self.shared.config.server_for(language).ok_or_else(|| LspError::Refused(format!("no language server is configured for {language} in data/tools/lsp.json")))?;
        if let Some(why) = self.broken_reason(run, language) {
            return Err(LspError::Refused(format!("the {language} language server stopped working earlier in this run ({why}), so it is not started again: use code.symbols, code.refs or fs.grep")));
        }
        if let Some(l) = self.take_live(&key, root) {
            return Ok(l);
        }
        let _one_start = lock(&self.shared.start_lock);
        // Another call may have started it while this one waited.
        if let Some(l) = self.take_live(&key, root) {
            return Ok(l);
        }
        // One process for a language in a run: one for another folder, or one that ended, goes first.
        let old = lock(&self.shared.state).live.remove(&key);
        if let Some(old) = old {
            let died = if old.server.is_alive() { None } else { Some(old.server.why_over().unwrap_or_else(|| "it ended".into())) };
            old.server.kill();
            if let Some(why) = died {
                // It ended by itself since its last request: that is a server that stopped working, not one to start again.
                lock(&self.shared.state).broken.insert(key.clone(), why.clone());
                return Err(LspError::Refused(format!("the {language} language server stopped working earlier in this run ({why}), so it is not started again: use code.symbols, code.refs or fs.grep")));
            }
        }
        let started = verify_program(spec, &*self.shared.may_write).and_then(|program| Server::start(spec, program, root, cancel));
        let server = match started {
            Ok(s) => Arc::new(s),
            Err(e) => {
                // A run that was stopped while the server started has not found the server broken.
                if e != LspError::Cancelled {
                    lock(&self.shared.state).broken.insert(key.clone(), match &e {
                        LspError::Refused(_) => "its program was refused: its hash, its place or its file".to_string(),
                        other => other.sentence(),
                    });
                }
                return Err(e);
            }
        };
        {
            let mut st = lock(&self.shared.state);
            st.launched.entry(key.clone()).or_default().insert(root_key(root));
            st.live.insert(key.clone(), Live { server: server.clone(), root: root.to_path_buf(), last_used: Instant::now(), busy: 1, idle: Duration::from_secs(spec.idle_secs) });
        }
        self.start_reaper();
        Ok(Lease { shared: self.shared.clone(), key, server, launched_now: true })
    }

    fn take_live(&self, key: &Key, root: &Path) -> Option<Lease> {
        let mut st = lock(&self.shared.state);
        let l = st.live.get_mut(key)?;
        if l.server.is_alive() && root_key(&l.root) == root_key(root) {
            l.busy += 1;
            l.last_used = Instant::now();
            return Some(Lease { shared: self.shared.clone(), key: key.clone(), server: l.server.clone(), launched_now: false });
        }
        None
    }

    /// Count one request against the run's limit.
    pub fn spend(&self, run: &str, spec: &ServerSpec) -> Result<(), LspError> {
        let mut st = lock(&self.shared.state);
        let n = st.spent.entry(run.to_string()).or_insert(0);
        if *n >= spec.max_requests_per_run {
            return Err(LspError::Refused(format!("this run has made its {} language-server requests (the limit in data/tools/lsp.json): use code.symbols, code.refs or fs.grep", spec.max_requests_per_run)));
        }
        *n += 1;
        Ok(())
    }

    /// A request failed: when the failure ends the server, it is ended and its language is unavailable for the rest of the run.
    pub fn fail(&self, run: &str, language: &str, error: &LspError) {
        if !error.ends_the_server() {
            return;
        }
        let key: Key = (run.to_string(), language.to_string());
        let old = {
            let mut st = lock(&self.shared.state);
            st.broken.insert(key.clone(), error.sentence());
            st.live.remove(&key)
        };
        if let Some(old) = old {
            old.server.kill();
        }
    }

    /// The run is over: its servers are ended (the whole tree of each), and what was kept for it is let go.
    pub fn end_run(&self, run: &str) {
        let ended: Vec<Live> = {
            let mut st = lock(&self.shared.state);
            let keys: Vec<Key> = st.live.keys().filter(|k| k.0 == run).cloned().collect();
            let ended = keys.iter().filter_map(|k| st.live.remove(k)).collect();
            st.launched.retain(|k, _| k.0 != run);
            st.broken.retain(|k, _| k.0 != run);
            st.spent.remove(run);
            ended
        };
        for l in ended {
            l.server.kill();
        }
    }

    /// Every server ended.
    pub fn shutdown_all(&self) {
        let ended: Vec<Live> = lock(&self.shared.state).live.drain().map(|(_, l)| l).collect();
        for l in ended {
            l.server.kill();
        }
    }

    /// How many servers are running (for a test).
    pub fn live_count(&self) -> usize {
        lock(&self.shared.state).live.len()
    }

    /// The process ids of the servers that are running (for a test).
    pub fn live_pids(&self) -> Vec<u32> {
        lock(&self.shared.state).live.values().map(|l| l.server.pid).collect()
    }

    fn start_reaper(&self) {
        if self.shared.reaper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(&self.shared);
        let spawned = std::thread::Builder::new().name("lsp-reaper".into()).spawn(move || loop {
            std::thread::sleep(Duration::from_millis(250));
            let Some(shared) = weak.upgrade() else { return };
            reap(&shared);
        });
        if spawned.is_err() {
            self.shared.reaper_started.store(false, Ordering::SeqCst);
        }
    }
}

/// End the servers that have been idle for their time, and note the ones that ended by themselves.
fn reap(shared: &Shared) {
    let retired: Vec<(Key, Live)> = {
        let mut st = lock(&shared.state);
        let keys: Vec<Key> = st.live.iter().filter(|(_, l)| !l.server.is_alive() || (l.busy == 0 && l.last_used.elapsed() >= l.idle)).map(|(k, _)| k.clone()).collect();
        keys.into_iter().filter_map(|k| st.live.remove(&k).map(|l| (k, l))).collect()
    };
    for (key, live) in retired {
        if live.server.is_alive() {
            live.server.shutdown(Duration::from_millis(800));
        } else {
            // It ended by itself, between two requests.
            lock(&shared.state).broken.insert(key, live.server.why_over().unwrap_or_else(|| "it ended".into()));
            live.server.kill();
        }
    }
}
