use aether::alist;
use aether::backup::{self, BackupOptions, ConflictPolicy, Outcome};
use aether::engine::Engine;
use aether::git::GitClient;
use aether::options::{
    format_duration, parse_duration, Options, Strategy, DEFAULT_DELAY, DEFAULT_EXCLUDES,
};
use aether::resume::{self, ResumeStore};
use aether::state::{
    ago, default_home, is_running, now, remaining, states, SharedState, State, StateStore,
};
use aether::ui;
use aether::watcher::RepoWatcher;
use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

#[derive(Parser)]
#[command(name = "aether", version, about = "Cross-platform Git auto-sync engine")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start watching the repository (foreground; Ctrl+C to stop)
    Start {
        /// immediate | delay | never
        #[arg(long, value_enum, default_value = "delay")]
        strategy: Strategy,
        /// Countdown length for `delay`, e.g. 10m, 30s, 2h, 500ms
        #[arg(long, default_value = DEFAULT_DELAY)]
        delay: String,
        /// Repository path (defaults to current directory)
        #[arg(long, default_value = ".")]
        path: PathBuf,
    },
    /// Show the current watcher status
    Status {
        /// Emit raw state JSON for scripting
        #[arg(long)]
        json: bool,
        /// Refresh in place until Ctrl+C (terminal dashboard)
        #[arg(long)]
        watch: bool,
        /// Refresh interval for --watch, e.g. 1s, 500ms
        #[arg(long, default_value = "1s")]
        interval: String,
    },
    /// Serve a local status dashboard at http://127.0.0.1:<port>
    Ui {
        /// TCP port to listen on (0 picks a free port)
        #[arg(long, default_value_t = ui::DEFAULT_PORT)]
        port: u16,
        /// Open the dashboard in the default browser
        #[arg(long)]
        open: bool,
    },
    /// Upload a file or a directory tree to an Alist server (PUT /api/fs/put)
    Backup {
        /// Local file, or a directory to back up recursively
        #[arg(long, value_name = "PATH")]
        local: PathBuf,
        /// Alist remote directory, e.g. /backup
        #[arg(long, value_name = "DIR")]
        remote: String,
        /// Alist site root; overrides AETHER_ALIST_URL / ALIST_URL
        #[arg(long, value_name = "URL")]
        url: Option<String>,
        /// Alist access token; overrides AETHER_ALIST_TOKEN / ALIST_TOKEN
        #[arg(long, value_name = "TOKEN")]
        token: Option<String>,
        /// What to do when the cloud copy differs from the local file
        #[arg(long, value_enum, default_value = "skip")]
        on_conflict: ConflictPolicy,
        /// Emit the structured batch report as JSON (for scripts / future UI)
        #[arg(long)]
        json: bool,
        /// Resume an interrupted batch from ~/.aether/backups (task-level checkpoint)
        #[arg(long)]
        resume: bool,
        /// Discard this job's checkpoint first, then run a full batch
        #[arg(long)]
        reset_resume: bool,
    },
}

fn main() {
    std::process::exit(run());
}

fn run() -> i32 {
    match Cli::parse().command {
        Command::Start {
            strategy,
            delay,
            path,
        } => start(strategy, delay, path),
        Command::Status {
            json,
            watch,
            interval,
        } => status(json, watch, interval),
        Command::Ui { port, open } => match ui::serve(default_home(), port, open) {
            Ok(()) => 0,
            Err(error) => {
                eprintln!("error: {error}");
                1
            }
        },
        Command::Backup {
            local,
            remote,
            url,
            token,
            on_conflict,
            json,
            resume,
            reset_resume,
        } => backup(
            local,
            remote,
            url,
            token,
            on_conflict,
            json,
            resume,
            reset_resume,
        ),
    }
}

fn start(strategy: Strategy, delay_text: String, path: PathBuf) -> i32 {
    let delay = match parse_duration(&delay_text) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    if !path.exists() {
        eprintln!("error: path does not exist: {}", path.display());
        return 2;
    }

    // Ask git for the real repository root, so running from a subdirectory still
    // watches the whole worktree. Avoid canonicalize(): on Windows it yields a
    // \\?\ verbatim path that git and notify handle inconsistently.
    let root_result = match GitClient::new(&path).repo_root() {
        Ok(result) => result,
        Err(error) => {
            eprintln!("error: cannot run git: {error}");
            return 2;
        }
    };
    if !root_result.ok() {
        eprintln!("error: not a git repository: {}", path.display());
        eprintln!("{}", root_result.output());
        return 2;
    }
    let root = PathBuf::from(root_result.stdout.trim());
    let git = GitClient::new(&root);

    let store = StateStore::from_default_home();
    if let Some(existing) = store.load() {
        if is_running(&existing) {
            eprintln!(
                "error: Aether is already running (PID {}) on {}.",
                existing.pid, existing.repository
            );
            eprintln!("Stop that instance before starting a new one.");
            return 3;
        }
    }

    let excludes: Vec<String> = DEFAULT_EXCLUDES.iter().map(|s| s.to_string()).collect();
    let watcher = match RepoWatcher::new(&root, &excludes) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: cannot start file watcher: {error}");
            return 2;
        }
    };

    match git.remotes() {
        Ok(result) if !result.stdout.trim().is_empty() => {
            let first = result.stdout.lines().next().unwrap_or_default().trim();
            println!("[aether] remote: {first}");
        }
        _ => println!("[aether] warning: no git remote; commits will be local, push will be reported as failed"),
    }

    let initial = State {
        pid: std::process::id(),
        process_name: "aether".to_string(),
        started_at: now(),
        heartbeat_at: now(),
        repository: root.to_string_lossy().into_owned(),
        strategy: strategy.as_str().to_string(),
        delay: format_duration(delay),
        excluded: excludes.clone(),
        state: states::STARTING.to_string(),
        ..Default::default()
    };
    let shared = SharedState::new(initial, store);

    let stop = Arc::new(AtomicBool::new(false));
    let stop_for_signal = Arc::clone(&stop);
    if let Err(error) =
        ctrlc::set_handler(move || stop_for_signal.store(true, Ordering::Relaxed))
    {
        eprintln!("warning: could not install Ctrl+C handler: {error}");
    }

    let opts = Options {
        path: root,
        strategy,
        delay,
        excludes,
    };
    let engine = Engine::new(opts, git, watcher, shared);
    match engine.run(stop) {
        Ok(()) => 0,
        Err(error) => {
            eprintln!("error: engine stopped: {error}");
            1
        }
    }
}

fn env_first(names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        std::env::var(name)
            .ok()
            .filter(|value| !value.trim().is_empty())
    })
}

fn human_size(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KIB {
        format!("{bytes} B")
    } else if value < KIB * KIB {
        format!("{:.1} KiB", value / KIB)
    } else if value < KIB * KIB * KIB {
        format!("{:.1} MiB", value / (KIB * KIB))
    } else {
        format!("{:.2} GiB", value / (KIB * KIB * KIB))
    }
}

/// Git Bash / MSYS2 rewrites a leading `/foo` argument into a Windows path such
/// as `/D:/.../foo`. Alist remote paths are always POSIX-style, so detect the
/// conversion and fail loudly instead of uploading into a nonsense directory.
fn msys_converted_path(remote: &str) -> bool {
    remote
        .replace('\\', "/")
        .split('/')
        .any(|segment| {
            let bytes = segment.as_bytes();
            bytes.len() == 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
        })
}

fn backup(
    local: PathBuf,
    remote: String,
    url: Option<String>,
    token: Option<String>,
    policy: ConflictPolicy,
    as_json: bool,
    use_resume: bool,
    reset_resume: bool,
) -> i32 {
    let alist_url = match url.or_else(|| env_first(&["AETHER_ALIST_URL", "ALIST_URL"])) {
        Some(value) => value.trim().to_string(),
        None => {
            eprintln!("error: Alist URL is missing; pass --url or set AETHER_ALIST_URL");
            return 2;
        }
    };
    let token = token
        .or_else(|| env_first(&["AETHER_ALIST_TOKEN", "ALIST_TOKEN"]))
        .unwrap_or_default();
    if token.is_empty() && !as_json {
        eprintln!("[aether] warning: no Alist token; attempting anonymous upload");
    }
    if msys_converted_path(&remote) {
        eprintln!("error: --remote looks like a Windows path rewritten by Git Bash: {remote}");
        eprintln!("hint: retry with MSYS_NO_PATHCONV=1, or use PowerShell / CMD");
        return 2;
    }
    if let Err(error) = std::fs::metadata(&local) {
        eprintln!("error: {}: {error}", local.display());
        return 2;
    }

    let planned = match backup::plan(&local, &remote) {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    if planned.is_empty() {
        eprintln!("error: no files to back up under {}", local.display());
        return 2;
    }
    let total = backup::plan_total(&planned);
    let client = match alist::new_client() {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };

    // 断点清单放 ~/.aether/backups/<job>.json（AETHER_HOME 可覆盖），
    // 绝不放进被备份的目录树；任务身份由站点 + 远程目录 + 本地绝对路径决定。
    let local_abs = std::path::absolute(&local).unwrap_or_else(|_| local.clone());
    let local_key = local_abs.to_string_lossy().into_owned();
    let site = alist_url.trim_end_matches('/');
    let remote_key = alist::normalize_remote_dir(&remote);
    let job = resume::JobSpec {
        alist_url: site,
        remote_dir: &remote_key,
        local: &local_key,
    };
    let mut checkpoint: Option<ResumeStore> = if use_resume || reset_resume {
        match ResumeStore::open(&default_home(), job) {
            Ok(value) => Some(value),
            Err(error) => {
                eprintln!("error: {error}");
                return 2;
            }
        }
    } else {
        None
    };
    if reset_resume {
        if let Some(store) = checkpoint.as_mut() {
            if let Err(error) = store.reset() {
                eprintln!("error: {error}");
                return 2;
            }
        }
    }

    if !as_json {
        println!("[aether] backup  {}", local.display());
        println!(
            "[aether] files   {} ({}, {total} bytes)",
            planned.len(),
            human_size(total)
        );
        println!("[aether] target  {}", alist::normalize_remote_dir(&remote));
        println!(
            "[aether] policy  {}",
            format!("{policy:?}").to_ascii_lowercase()
        );
        println!("[aether] uploading to {alist_url}/api/fs/put ...");
        println!(
            "[aether] token   {}",
            if token.is_empty() { "none" } else { "set" }
        );
        if let Some(store) = checkpoint.as_ref() {
            println!(
                "[aether] resume  {} ({} completed, {})",
                store.path().display(),
                store.len(),
                if reset_resume { "reset" } else { "enabled" }
            );
        }
    }

    let sent = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));
    let (sent_bg, stop_bg) = (Arc::clone(&sent), Arc::clone(&stop));
    let show_progress = !as_json;
    let progress = thread::spawn(move || {
        // Tiny uploads finish before the first tick: don't flash a stray "0%".
        thread::sleep(Duration::from_millis(200));
        let mut last: Option<u64> = None;
        while !stop_bg.load(Ordering::Relaxed) {
            let done = sent_bg.load(Ordering::Relaxed);
            let percent = if total == 0 {
                100
            } else {
                (done.saturating_mul(100) / total).min(100)
            };
            if show_progress && last != Some(percent) && percent < 100 {
                eprintln!("[aether] {percent}%  {}/{}", human_size(done), human_size(total));
                last = Some(percent);
            }
            thread::sleep(Duration::from_millis(120));
        }
    });

    let options = BackupOptions {
        alist_url: &alist_url,
        token: &token,
        policy,
    };
    let result = backup::run_backup(
        &client,
        &local,
        &remote,
        &options,
        Arc::clone(&sent),
        if use_resume { checkpoint.as_mut() } else { None },
    );
    stop.store(true, Ordering::Relaxed);
    let _ = progress.join();

    let report = match result {
        Ok(value) => value,
        Err(error) => {
            eprintln!("error: {error}");
            return 2;
        }
    };
    if as_json {
        println!("{}", serde_json::to_string_pretty(&report).unwrap_or_default());
        return if report.ok() { 0 } else { 1 };
    }

    for entry in &report.items {
        let label = match entry.outcome {
            Outcome::Uploaded => "OK: uploaded to",
            Outcome::Skipped => "skipped",
            Outcome::Failed => "failed",
        };
        println!("[aether] {label} {}", entry.remote);
        if !entry.detail.is_empty() {
            if entry.outcome == Outcome::Failed {
                eprintln!("           {}", entry.detail);
            } else {
                println!("           {}", entry.detail);
            }
        }
    }
    let (uploaded, skipped, failed) = report.counts();
    println!("[aether] done: {uploaded} uploaded, {skipped} skipped, {failed} failed");
    if report.resumed > 0 {
        println!(
            "[aether]          ({} restored from checkpoint, no network request)",
            report.resumed
        );
    }
    if report.ok() { 0 } else { 1 }
}

#[derive(Serialize)]
struct StatusEnvelope {
    #[serde(flatten)]
    state: State,
    running: bool,
}

fn render_status(state: &State, running: bool, state_path: &std::path::Path) -> String {
    let mut out = String::new();
    out.push_str(&format!(
        "Aether: {}\n",
        if running {
            "RUNNING"
        } else {
            "NOT RUNNING (stopped or stale state)"
        }
    ));
    out.push_str(&format!("  PID          {} ({})\n", state.pid, state.process_name));
    out.push_str(&format!("  Started      {}\n", ago(state.started_at)));
    out.push_str(&format!("  Heartbeat    {}\n", ago(state.heartbeat_at)));
    out.push_str(&format!("  Repository   {}\n", state.repository));
    let strategy = if state.strategy == "delay" {
        format!("delay ({})", state.delay)
    } else {
        state.strategy.clone()
    };
    out.push_str(&format!("  Strategy     {strategy}\n"));
    out.push_str(&format!("  Excluded     {}\n", state.excluded.join(", ")));
    out.push_str(&format!("  State        {}\n", state.state));
    out.push_str(&format!(
        "  Events       {} change(s), last {}\n",
        state.events_seen,
        ago(state.last_change_at)
    ));
    out.push_str(&format!("  Pending      {} file(s)\n", state.pending_changes));
    if state.next_sync_at != 0 {
        out.push_str(&format!("  Next sync    in {}\n", remaining(state.next_sync_at)));
    }
    if state.last_sync_at != 0 {
        out.push_str(&format!(
            "  Last sync    {}  {}  {}\n",
            ago(state.last_sync_at),
            state.last_sync_result.to_uppercase(),
            state.last_sync_message
        ));
    }
    out.push_str(&format!("  State file   {}\n", state_path.display()));
    out
}

/// `status --watch`：原地刷新，Ctrl+C 退出。返回最后一次读到的运行状态。
fn watch_status(interval: Duration) -> i32 {
    let store = StateStore::from_default_home();
    let stop = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::clone(&stop);
    if let Err(error) = ctrlc::set_handler(move || stop_signal.store(true, Ordering::Relaxed)) {
        eprintln!("warning: could not install Ctrl+C handler: {error}");
    }

    let tick = Duration::from_millis(100);
    let mut code = 1;
    while !stop.load(Ordering::Relaxed) {
        let (body, running) = match store.load() {
            Some(state) => {
                let running = is_running(&state);
                (render_status(&state, running, store.path()), running)
            }
            None => (
                format!(
                    "Aether: NOT RUNNING (no state file at {})\n",
                    store.path().display()
                ),
                false,
            ),
        };
        code = if running { 0 } else { 1 };
        // 清屏 + 回原点；Windows 10+ 终端默认支持 VT 序列。
        print!("\x1b[2J\x1b[H{body}\x1b[2m  refresh {}\x1b[0m\n", format_duration(interval));
        let _ = std::io::stdout().flush();

        let mut slept = Duration::ZERO;
        while slept < interval && !stop.load(Ordering::Relaxed) {
            thread::sleep(tick);
            slept += tick;
        }
    }
    println!();
    code
}

fn status(as_json: bool, watch: bool, interval_text: String) -> i32 {
    if watch {
        if as_json {
            eprintln!("error: --watch cannot be combined with --json");
            return 2;
        }
        let interval = match parse_duration(&interval_text) {
            Ok(value) if value >= Duration::from_millis(100) => value,
            Ok(_) => {
                eprintln!("error: --interval must be at least 100ms");
                return 2;
            }
            Err(error) => {
                eprintln!("error: {error}");
                return 2;
            }
        };
        return watch_status(interval);
    }

    let store = StateStore::from_default_home();
    let Some(state) = store.load() else {
        if as_json {
            println!("null");
        } else {
            println!(
                "Aether: NOT RUNNING (no state file at {})",
                store.path().display()
            );
        }
        return 1;
    };

    let running = is_running(&state);
    if as_json {
        let envelope = StatusEnvelope { state, running };
        println!(
            "{}",
            serde_json::to_string_pretty(&envelope).unwrap_or_default()
        );
        return if running { 0 } else { 1 };
    }

    print!("{}", render_status(&state, running, store.path()));
    if running {
        0
    } else {
        1
    }
}
