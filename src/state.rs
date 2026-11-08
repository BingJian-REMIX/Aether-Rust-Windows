use serde::{Deserialize, Serialize};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{SystemTime, UNIX_EPOCH};

pub const HEARTBEAT_TIMEOUT_SECS: u64 = 15;

pub mod states {
    pub const STARTING: &str = "starting";
    pub const WATCHING: &str = "watching";
    pub const COUNTDOWN: &str = "countdown";
    pub const SYNCING: &str = "syncing";
    pub const REMINDED: &str = "reminded";
    pub const STOPPED: &str = "stopped";
}

pub fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// Compact human-readable duration, e.g. `09:42` / `1h02m10s`.
pub fn compact(secs: u64) -> String {
    if secs >= 3_600 {
        format!("{}h{:02}m{:02}s", secs / 3_600, (secs % 3_600) / 60, secs % 60)
    } else if secs >= 60 {
        format!("{}m{:02}s", secs / 60, secs % 60)
    } else {
        format!("{secs}s")
    }
}

pub fn ago(timestamp: u64) -> String {
    if timestamp == 0 {
        return "-".to_string();
    }
    format!("{} ago", compact(now().saturating_sub(timestamp)))
}

pub fn remaining(timestamp: u64) -> String {
    if timestamp == 0 {
        return "-".to_string();
    }
    let left = timestamp.saturating_sub(now());
    if left == 0 {
        "due now".to_string()
    } else {
        compact(left)
    }
}

/// Snapshot consumed by `aether status`. Stored outside the watched tree on purpose:
/// writing the state file inside the repo would trigger the watcher forever.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct State {
    pub pid: u32,
    pub process_name: String,
    pub started_at: u64,
    pub heartbeat_at: u64,
    pub repository: String,
    pub strategy: String,
    pub delay: String,
    pub excluded: Vec<String>,
    pub state: String,
    pub events_seen: u64,
    pub last_change_at: u64,
    pub next_sync_at: u64,
    pub last_sync_at: u64,
    pub last_sync_result: String,
    pub last_sync_message: String,
    pub pending_changes: usize,
}

pub fn default_home() -> PathBuf {
    if let Ok(custom) = std::env::var("AETHER_HOME") {
        if !custom.trim().is_empty() {
            return PathBuf::from(custom);
        }
    }
    let base = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if base.is_empty() {
        PathBuf::from(".aether")
    } else {
        PathBuf::from(base).join(".aether")
    }
}

pub struct StateStore {
    path: PathBuf,
}

impl StateStore {
    pub fn new(home: impl AsRef<Path>) -> Self {
        Self {
            path: home.as_ref().join("state.json"),
        }
    }

    pub fn from_default_home() -> Self {
        Self::new(default_home())
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn load(&self) -> Option<State> {
        let text = fs::read_to_string(&self.path).ok()?;
        serde_json::from_str(&text).ok()
    }

    /// Atomic replace, so `status` never reads a half-written file.
    pub fn save(&self, state: &State) -> io::Result<()> {
        let json = serde_json::to_string_pretty(state).map_err(io::Error::other)?;
        atomic_write(&self.path, json.as_bytes())
    }
}

/// 写临时文件再 rename，供 `StateStore` 与断点清单共用。
///
/// 临时文件名由目标文件名派生，避免不同文件在同一目录下抢同一个 `.tmp`
/// （心跳线程与备份进程并发写时曾出现过这种竞态）。
pub fn atomic_write(path: &Path, bytes: &[u8]) -> io::Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let mut temp_name = path.file_name().unwrap_or_default().to_os_string();
    temp_name.push(".tmp");
    let temp = path.with_file_name(temp_name);
    fs::write(&temp, bytes)?;
    fs::rename(&temp, path)?;
    Ok(())
}

/// Thread-safe state snapshot shared between the engine loop and the heartbeat thread.
pub struct SharedState {
    inner: Mutex<State>,
    store: StateStore,
}

impl SharedState {
    pub fn new(initial: State, store: StateStore) -> Arc<Self> {
        let shared = Arc::new(Self {
            inner: Mutex::new(initial),
            store,
        });
        shared.update(|_| {}); // persist the initial snapshot immediately
        shared
    }

    pub fn update(&self, change: impl FnOnce(&mut State)) {
        // Keep the write inside the lock: the heartbeat thread and the engine
        // loop would otherwise race on the same .tmp file during atomic replace.
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        change(&mut guard);
        if let Err(error) = self.store.save(&guard) {
            eprintln!("[aether] warning: state write failed: {error}");
        }
    }

    pub fn snapshot(&self) -> State {
        self.inner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn store_path(&self) -> &Path {
        self.store.path()
    }
}

/// A running instance is one that stopped recently OR kept its heartbeat fresh.
pub fn is_running(state: &State) -> bool {
    state.state != states::STOPPED
        && now().saturating_sub(state.heartbeat_at) <= HEARTBEAT_TIMEOUT_SECS
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn save_and_load_roundtrip() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::new(dir.path());
        let mut state = State::default();
        state.pid = 1234;
        state.repository = "D:/repo".to_string();
        state.state = states::COUNTDOWN.into();
        state.next_sync_at = now() + 600;
        store.save(&state).unwrap();

        let loaded = store.load().unwrap();
        assert_eq!(loaded.pid, 1234);
        assert_eq!(loaded.repository, "D:/repo");
        assert_eq!(loaded.state, states::COUNTDOWN);
        assert_eq!(loaded.next_sync_at, state.next_sync_at);
    }

    #[test]
    fn repeated_saves_overwrite_atomically() {
        let dir = TempDir::new().unwrap();
        let store = StateStore::new(dir.path());
        for i in 0..20 {
            let mut state = State::default();
            state.events_seen = i;
            store.save(&state).unwrap();
            assert_eq!(store.load().unwrap().events_seen, i);
        }
    }

    #[test]
    fn heartbeat_determines_running() {
        let mut state = State::default();
        state.state = states::WATCHING.into();
        state.heartbeat_at = now();
        assert!(is_running(&state));

        state.heartbeat_at = now() - HEARTBEAT_TIMEOUT_SECS - 10;
        assert!(!is_running(&state));

        state.heartbeat_at = now();
        state.state = states::STOPPED.into();
        assert!(!is_running(&state));
    }

    #[test]
    fn missing_state_file_loads_none() {
        let dir = TempDir::new().unwrap();
        assert!(StateStore::new(dir.path()).load().is_none());
    }
}
