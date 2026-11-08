use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

pub struct CmdResult {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
    pub cmd: String,
}

impl CmdResult {
    pub fn ok(&self) -> bool {
        self.code == 0
    }

    pub fn output(&self) -> String {
        let out = self.stdout.trim();
        let err = self.stderr.trim();
        match (out.is_empty(), err.is_empty()) {
            (true, true) => String::new(),
            (false, true) => out.to_string(),
            (true, false) => err.to_string(),
            (false, false) => format!("{out}\n{err}"),
        }
    }
}

/// Thin wrapper over the git CLI. Deliberately not libgit2: the CLI inherits the
/// user's credential helpers, SSH agent, hooks, GPG signing, LFS and proxy config.
pub struct GitClient {
    workdir: PathBuf,
}

impl GitClient {
    pub fn new(workdir: impl AsRef<Path>) -> Self {
        Self {
            workdir: workdir.as_ref().to_path_buf(),
        }
    }

    pub fn workdir(&self) -> &Path {
        &self.workdir
    }

    pub fn run(&self, args: &[&str]) -> io::Result<CmdResult> {
        let output = Command::new("git")
            .args(args)
            .current_dir(&self.workdir)
            // Never block a background sync on an interactive credential prompt.
            .env("GIT_TERMINAL_PROMPT", "0")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()?;
        Ok(CmdResult {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            cmd: format!("git {}", args.join(" ")),
        })
    }

    pub fn repo_root(&self) -> io::Result<CmdResult> {
        self.run(&["rev-parse", "--show-toplevel"])
    }

    pub fn remotes(&self) -> io::Result<CmdResult> {
        self.run(&["remote", "-v"])
    }

    pub fn add_all(&self) -> io::Result<CmdResult> {
        self.run(&["add", "."])
    }

    /// Exit 0 = nothing staged, 1 = staged changes present, other = real failure.
    pub fn check_staged(&self) -> io::Result<CmdResult> {
        self.run(&["diff", "--cached", "--quiet", "--exit-code"])
    }

    pub fn commit(&self, message: &str) -> io::Result<CmdResult> {
        self.run(&["commit", "-m", message])
    }

    pub fn pending(&self) -> io::Result<CmdResult> {
        self.run(&["status", "--porcelain"])
    }

    /// Pushes to the configured upstream, or establishes one on the first push.
    pub fn push(&self) -> io::Result<CmdResult> {
        let upstream = self.run(&["rev-parse", "--abbrev-ref", "--symbolic-full-name", "@{u}"])?;
        if upstream.ok() {
            return self.run(&["push"]);
        }
        let branch = self.run(&["rev-parse", "--abbrev-ref", "HEAD"])?;
        if !branch.ok() {
            return Ok(branch);
        }
        let name = branch.stdout.trim().to_string();
        self.run(&["push", "-u", "origin", &name])
    }

    pub fn count_lines(porcelain: &str) -> usize {
        porcelain.lines().filter(|line| !line.trim().is_empty()).count()
    }
}
