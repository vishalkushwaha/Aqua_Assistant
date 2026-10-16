use tracing::info;

pub struct SessionWatcher {
    is_locked: bool,
}

impl SessionWatcher {
    pub fn new() -> Self {
        info!("Initializing OS Session & Lock Screen Watcher");
        Self { is_locked: false }
    }

    pub fn is_locked(&self) -> bool {
        self.is_locked
    }

    pub fn set_locked(&mut self, locked: bool) {
        self.is_locked = locked;
        if locked {
            info!("OS Lock screen detected: Pausing microphone sampling.");
        } else {
            info!("OS Lock screen unlocked: Resuming microphone sampling.");
        }
    }
}
