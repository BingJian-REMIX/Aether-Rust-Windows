use crate::git::GitClient;
use crate::options::{Options, Strategy, COMMIT_MESSAGE, REMINDER_THROTTLE};
use crate::state::{now, states, SharedState};
use crate::watcher::{RepoWatcher, WatchEvent};
use std::io;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::RecvTimeoutError;
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);
const POLL_SLICE: Duration = Duration::from_millis(200);

/// Strategy state machine: coalesces a burst of file events into one sync.
pub struct Engine {
    opts: Options,
    git: GitClient,
    watcher: RepoWatcher,
    shared: Arc<SharedState>,
    last_reminder: Mutex<Instant>,
}

impl Engine {
    pub fn new(
        opts: Options,
        git: GitClient,
        watcher: RepoWatcher,
        shared: Arc<SharedState>,
    ) -> Self {
        Self {
            opts,
            git,
            watcher,
            shared,
            last_reminder: Mutex::new(Instant::now() - REMINDER_THROTTLE),
        }
    }

    pub fn shared(&self) -> Arc<SharedState> {
        Arc::clone(&self.shared)
    }

    pub fn run(&self, stop: Arc<AtomicBool>) -> io::Result<()> {
        let heartbeat = self.spawn_heartbeat(Arc::clone(&stop));
        self.publish(states::WATCHING, 0);
        Self::log(&format!("watching {}", self.git.workdir().display()));
        Self::log(&format!(
            "strategy {}; excluded: {}",
            self.opts.strategy_label(),
            self.opts.excludes.join(", ")
        ));
        Self::log(&format!("state file: {}", self.shared.store_path().display()));

        while !stop.load(Ordering::Relaxed) {
            let first = match self.watcher.events.recv_timeout(POLL_SLICE) {
                Ok(event) => event,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            let mut count = self.handle(first);
            count += self.drain();
            self.register(count);

            if self.opts.strategy == Strategy::Never {
                self.remind();
                continue;
            }

            let window = self.opts.trigger_window();
            let mut deadline = Instant::now() + window;
            self.publish(states::COUNTDOWN, now() + window.as_secs());
            Self::log(&format!(
                "change detected; sync in {}",
                crate::options::format_duration(window)
            ));

            if !self.wait_window(&mut deadline, window, &stop) {
                break;
            }
            self.sync();
            self.publish(states::WATCHING, 0);
        }

        self.publish(states::STOPPED, 0);
        Self::log("watcher stopped");
        let _ = heartbeat.join();
        Ok(())
    }

    /// Returns true when the countdown elapsed, false on shutdown/disconnect.
    /// Any new change restarts the countdown, so active editing never gets committed mid-burst.
    fn wait_window(
        &self,
        deadline: &mut Instant,
        window: Duration,
        stop: &AtomicBool,
    ) -> bool {
        loop {
            if stop.load(Ordering::Relaxed) {
                return false;
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return true;
            }
            match self.watcher.events.recv_timeout(remaining) {
                Ok(event) => {
                    let mut count = self.handle(event);
                    count += self.drain();
                    self.register(count);
                    *deadline = Instant::now() + window;
                    self.publish(states::COUNTDOWN, now() + window.as_secs());
                }
                Err(RecvTimeoutError::Timeout) => return true,
                Err(RecvTimeoutError::Disconnected) => return false,
            }
        }
    }

    fn drain(&self) -> u64 {
        let mut count = 0;
        while let Ok(event) = self.watcher.events.try_recv() {
            count += self.handle(event);
        }
        count
    }

    fn handle(&self, event: WatchEvent) -> u64 {
        match event {
            WatchEvent::Changed(_) => 1,
            WatchEvent::Error(message) => {
                Self::log(&format!("warning: watcher error: {message}; treating as a change"));
                1
            }
        }
    }

    fn register(&self, count: u64) {
        if count == 0 {
            return;
        }
        self.shared.update(|state| {
            state.events_seen += count;
            state.last_change_at = now();
        });
    }

    fn remind(&self) {
        {
            let mut last = self
                .last_reminder
                .lock()
                .unwrap_or_else(|error| error.into_inner());
            if last.elapsed() < REMINDER_THROTTLE {
                self.publish(states::REMINDED, 0);
                return;
            }
            *last = Instant::now();
        }

        let pending = self
            .git
            .pending()
            .map(|result| {
                if result.ok() {
                    GitClient::count_lines(&result.stdout)
                } else {
                    0
                }
            })
            .unwrap_or(0);
        self.shared.update(|state| state.pending_changes = pending);
        self.publish(states::REMINDED, 0);
        Self::log(&format!(
            "[never] {pending} pending change(s). No commit/push performed. Sync manually when ready."
        ));
    }

    fn sync(&self) {
        self.publish(states::SYNCING, 0);
        Self::log("sync started: git add . -> git commit -m \"Aether Auto-Sync\" -> git push");

        let (result, message) = self.execute_sync();
        self.shared.update(|state| {
            state.last_sync_at = now();
            state.last_sync_result = result.clone();
            state.last_sync_message = message.clone();
            state.pending_changes = 0;
        });
        self.publish(states::WATCHING, 0);
        Self::log(&format!("sync {}: {message}", result.to_uppercase()));
    }

    fn execute_sync(&self) -> (String, String) {
        let add = match self.git.add_all() {
            Ok(result) => result,
            Err(error) => return ("failed".into(), error.to_string()),
        };
        if !add.ok() {
            return ("failed".into(), format!("{}: {}", add.cmd, add.output()));
        }

        let staged = match self.git.check_staged() {
            Ok(result) => result,
            Err(error) => return ("failed".into(), error.to_string()),
        };
        if staged.code == 0 {
            return ("skipped".into(), "no changes to commit".into());
        }
        if staged.code != 1 {
            return ("failed".into(), format!("{}: {}", staged.cmd, staged.output()));
        }

        let commit = match self.git.commit(COMMIT_MESSAGE) {
            Ok(result) => result,
            Err(error) => return ("failed".into(), error.to_string()),
        };
        if !commit.ok() {
            return ("failed".into(), format!("{}: {}", commit.cmd, commit.output()));
        }

        let push = match self.git.push() {
            Ok(result) => result,
            Err(error) => return ("failed".into(), error.to_string()),
        };
        if !push.ok() {
            return (
                "partial".into(),
                format!("commit created but push failed: {}", push.output()),
            );
        }
        let summary = commit
            .stdout
            .lines()
            .next()
            .unwrap_or("commit created")
            .trim()
            .to_string();
        ("ok".into(), format!("pushed: {summary}"))
    }

    fn publish(&self, state: &str, next_sync_at: u64) {
        self.shared.update(|snapshot| {
            snapshot.state = state.to_string();
            snapshot.next_sync_at = next_sync_at;
            snapshot.heartbeat_at = now();
        });
    }

    fn spawn_heartbeat(&self, stop: Arc<AtomicBool>) -> JoinHandle<()> {
        let shared = Arc::clone(&self.shared);
        thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                shared.update(|state| state.heartbeat_at = now());
                // Sleep in small slices so shutdown is immediate.
                let mut waited = Duration::ZERO;
                while waited < HEARTBEAT_INTERVAL && !stop.load(Ordering::Relaxed) {
                    thread::sleep(Duration::from_millis(100));
                    waited += Duration::from_millis(100);
                }
            }
        })
    }

    fn log(message: &str) {
        let secs = now() % 86_400;
        println!("[{:02}:{:02}:{:02}Z] {message}", secs / 3_600, (secs % 3_600) / 60, secs % 60);
    }
}
