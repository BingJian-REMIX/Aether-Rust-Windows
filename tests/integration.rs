use aether::engine::Engine;
use aether::git::GitClient;
use aether::options::{Options, Strategy, COMMIT_MESSAGE, DEFAULT_EXCLUDES};
use aether::state::{is_running, now, states, SharedState, State, StateStore};
use aether::watcher::RepoWatcher;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;

fn p(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

struct Fixture {
    _dir: TempDir,
    work: PathBuf,
    bare: PathBuf,
}

impl Fixture {
    /// Real work repo + bare remote, with one initial commit already pushed.
    fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let work = dir.path().join("work");
        let bare = dir.path().join("remote.git");

        let bootstrap = GitClient::new(dir.path());
        bootstrap
            .run(&["init", "-b", "main", &p(&work)])
            .unwrap();
        bootstrap
            .run(&["init", "--bare", "-b", "main", &p(&bare)])
            .unwrap();

        let git = GitClient::new(&work);
        git.run(&["config", "user.email", "aether@test.local"])
            .unwrap();
        git.run(&["config", "user.name", "Aether Test"])
            .unwrap();
        git.run(&["remote", "add", "origin", &p(&bare)])
            .unwrap();
        fs::write(work.join(".gitignore"), "/target\nbin/\nobj/\n").unwrap();
        assert!(git.add_all().unwrap().ok());
        assert!(git.commit("initial").unwrap().ok());
        assert!(git.push().unwrap().ok(), "initial push failed");

        Self {
            _dir: dir,
            work,
            bare,
        }
    }

    fn git(&self) -> GitClient {
        GitClient::new(&self.work)
    }

    fn state_home(&self) -> PathBuf {
        // Deliberately outside the watched tree.
        self.work.parent().unwrap().join("aether-state")
    }
}

fn remote_subject(bare: &Path) -> String {
    let gitdir = p(bare);
    let client = GitClient::new(bare.parent().unwrap());
    let result = client
        .run(&["--git-dir", &gitdir, "log", "-1", "--pretty=%s"])
        .unwrap();
    assert!(result.ok(), "{}", result.output());
    result.stdout.trim().to_string()
}

fn remote_count(bare: &Path) -> usize {
    let gitdir = p(bare);
    let client = GitClient::new(bare.parent().unwrap());
    let result = client
        .run(&["--git-dir", &gitdir, "rev-list", "--count", "HEAD"])
        .unwrap();
    assert!(result.ok(), "{}", result.output());
    result.stdout.trim().parse().unwrap_or(0)
}

fn start_engine(
    fixture: &Fixture,
    strategy: Strategy,
    delay: Duration,
) -> (Arc<SharedState>, Arc<AtomicBool>, JoinHandle<()>) {
    let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
    let watcher = RepoWatcher::new(&fixture.work, &excludes).unwrap();
    // Give the OS watch a moment to become active before writing files.
    thread::sleep(Duration::from_millis(250));

    let initial = State {
        pid: std::process::id(),
        process_name: "aether".to_string(),
        started_at: now(),
        heartbeat_at: now(),
        repository: p(&fixture.work),
        strategy: strategy.as_str().to_string(),
        delay: "1s".to_string(),
        excluded: excludes.clone(),
        state: states::STARTING.to_string(),
        ..Default::default()
    };
    let shared = SharedState::new(initial, StateStore::new(fixture.state_home()));
    let stop = Arc::new(AtomicBool::new(false));
    let opts = Options {
        path: fixture.work.clone(),
        strategy,
        delay,
        excludes,
    };
    let engine = Engine::new(opts, fixture.git(), watcher, Arc::clone(&shared));
    let stop_for_thread = Arc::clone(&stop);
    let handle = thread::spawn(move || engine.run(stop_for_thread).unwrap());
    (shared, stop, handle)
}

fn wait_until(
    shared: &Arc<SharedState>,
    predicate: impl Fn(&State) -> bool,
    timeout: Duration,
) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if predicate(&shared.snapshot()) {
            return true;
        }
        thread::sleep(Duration::from_millis(50));
    }
    false
}

fn stop_engine(stop: &Arc<AtomicBool>, handle: JoinHandle<()>) {
    stop.store(true, Ordering::Relaxed);
    handle.join().unwrap();
}

#[test]
fn git_client_adds_commits_and_pushes() {
    let fixture = Fixture::new();
    fs::write(fixture.work.join("hello.txt"), "hello").unwrap();

    assert!(fixture.git().add_all().unwrap().ok());
    assert_eq!(fixture.git().check_staged().unwrap().code, 1);
    assert!(fixture.git().commit(COMMIT_MESSAGE).unwrap().ok());
    assert!(fixture.git().push().unwrap().ok());

    assert_eq!(remote_subject(&fixture.bare), COMMIT_MESSAGE);
    assert_eq!(remote_count(&fixture.bare), 2);
    assert_eq!(fixture.git().check_staged().unwrap().code, 0);
    assert_eq!(GitClient::count_lines("?? a\n M b\n\n"), 2);
}

#[test]
fn delay_strategy_syncs_once_after_countdown_resets() {
    let fixture = Fixture::new();
    let (shared, stop, handle) =
        start_engine(&fixture, Strategy::Delay, Duration::from_secs(1));

    fs::write(fixture.work.join("a.txt"), "a").unwrap();
    thread::sleep(Duration::from_millis(400));
    fs::write(fixture.work.join("b.txt"), "b").unwrap();

    let synced = wait_until(
        &shared,
        |state| state.last_sync_result == "ok",
        Duration::from_secs(20),
    );
    stop_engine(&stop, handle);

    assert!(synced, "no successful sync: {:?}", shared.snapshot());
    assert_eq!(remote_subject(&fixture.bare), COMMIT_MESSAGE);
    assert_eq!(
        remote_count(&fixture.bare),
        2,
        "a new change must reset the countdown, not create a second commit"
    );
    assert!(shared.snapshot().events_seen >= 2);
    assert!(!is_running(&shared.snapshot()));
}

#[test]
fn never_strategy_reminds_without_touching_git() {
    let fixture = Fixture::new();
    let (shared, stop, handle) =
        start_engine(&fixture, Strategy::Never, Duration::from_secs(1));

    fs::write(fixture.work.join("pending.txt"), "x").unwrap();
    let reminded = wait_until(
        &shared,
        |state| state.state == states::REMINDED && state.pending_changes > 0,
        Duration::from_secs(10),
    );
    thread::sleep(Duration::from_millis(300));
    stop_engine(&stop, handle);

    assert!(reminded, "no reminder recorded: {:?}", shared.snapshot());
    assert_eq!(remote_count(&fixture.bare), 1, "never must not commit");
    assert_eq!(shared.snapshot().last_sync_result, "");
    assert!(shared.snapshot().pending_changes > 0);
}

#[test]
fn excluded_directories_do_not_trigger_sync() {
    let fixture = Fixture::new();
    let (shared, stop, handle) =
        start_engine(&fixture, Strategy::Delay, Duration::from_millis(600));

    fs::create_dir_all(fixture.work.join("bin")).unwrap();
    fs::write(fixture.work.join("bin/app.dll"), "x").unwrap();
    fs::create_dir_all(fixture.work.join("obj")).unwrap();
    fs::write(fixture.work.join("obj/app.o"), "x").unwrap();
    fs::write(fixture.work.join(".git/AETHER_PROBE"), "x").unwrap();
    thread::sleep(Duration::from_millis(1_500));

    assert_eq!(
        shared.snapshot().events_seen,
        0,
        ".git/bin/obj changes must be filtered out"
    );
    assert_eq!(remote_count(&fixture.bare), 1);

    fs::write(fixture.work.join("real.txt"), "x").unwrap();
    let synced = wait_until(
        &shared,
        |state| state.last_sync_result == "ok",
        Duration::from_secs(15),
    );
    stop_engine(&stop, handle);

    assert!(synced, "real change was not synced: {:?}", shared.snapshot());
    assert_eq!(remote_subject(&fixture.bare), COMMIT_MESSAGE);
}

#[test]
fn immediate_strategy_uses_short_quiet_window() {
    let fixture = Fixture::new();
    let (shared, stop, handle) =
        start_engine(&fixture, Strategy::Immediate, Duration::from_secs(600));

    let started = Instant::now();
    fs::write(fixture.work.join("fast.txt"), "x").unwrap();
    let synced = wait_until(
        &shared,
        |state| state.last_sync_result == "ok",
        Duration::from_secs(15),
    );
    let elapsed = started.elapsed();
    stop_engine(&stop, handle);

    assert!(synced, "immediate sync failed: {:?}", shared.snapshot());
    assert!(elapsed < Duration::from_secs(8), "immediate took {elapsed:?}");
    assert_eq!(remote_subject(&fixture.bare), COMMIT_MESSAGE);
}
