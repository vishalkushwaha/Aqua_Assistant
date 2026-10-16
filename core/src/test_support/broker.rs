//! `BrokerAnswerer`: the person who answers the browser's own questions, as a thread (card OA3-01, AQ-04).
//!
//! The old routes of the browser and the desktop (the ones that do not go through the tool host) put their questions to the browser's broker (`BrowserBackend::broker`), not to the
//! host's approver, and wait for an answer. A test that runs such a route needs somebody to say no, and to know what was asked: this thread polls the broker, answers every question
//! it finds with no, and keeps them in the order they were asked. A call that is answered no never goes on to the browser or the desktop, so a test that uses it cannot start either.

use aqua_mcp_adapters::browser::{BrowserBackend, ConfirmRequest, Decision};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub struct BrokerAnswerer {
    stop: Arc<AtomicBool>,
    answered: Arc<Mutex<Vec<ConfirmRequest>>>,
    thread: std::thread::JoinHandle<()>,
}

impl BrokerAnswerer {
    /// Starts answering every question put to `backend`'s broker with no.
    pub fn denying(backend: Arc<BrowserBackend>) -> Self {
        let (stop, answered) = (Arc::new(AtomicBool::new(false)), Arc::new(Mutex::new(Vec::new())));
        let (flag, kept) = (stop.clone(), answered.clone());
        let thread = std::thread::spawn(move || {
            while !flag.load(Ordering::SeqCst) {
                for open in backend.broker().pending() {
                    backend.broker().resolve_typed(&open.request_id, Decision::Deny);
                    kept.lock().unwrap_or_else(|e| e.into_inner()).push(open);
                }
                std::thread::sleep(Duration::from_millis(2));
            }
        });
        Self { stop, answered, thread }
    }

    /// Stops answering and gives the questions that were put, in order. A question is kept in the same turn of the loop that answers it, so the thread that has ended has kept
    /// every one it answered.
    pub fn finish(self) -> Vec<ConfirmRequest> {
        self.stop.store(true, Ordering::SeqCst);
        self.thread.join().expect("the answering thread ended");
        let answered = self.answered.lock().unwrap_or_else(|e| e.into_inner()).clone();
        answered
    }
}
