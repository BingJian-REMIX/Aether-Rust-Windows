//! 批量备份编排：目录递归、云端 / 本地冲突校验、结构化结果。
//!
//! `BatchReport` 是后续 UI 消息中心可直接消费的事件源：每一项都带 outcome
//! 与 detail（含冲突原因），不需要额外的事件总线或日志格式。

use crate::alist;
use crate::options::DEFAULT_EXCLUDES;
use crate::resume::{ResumeEntry, ResumeStore};
use clap::ValueEnum;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum, Serialize)]
#[value(rename_all = "lowercase")]
#[serde(rename_all = "lowercase")]
pub enum ConflictPolicy {
    /// 云端同名文件与本地不一致时跳过，绝不覆盖
    Skip,
    /// 直接覆盖云端文件
    Overwrite,
    /// 本地文件加时间戳后缀另存，双方都保留
    Rename,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
    Uploaded,
    Skipped,
    Failed,
}

#[derive(Debug, Serialize)]
pub struct BackupItem {
    pub local: String,
    pub remote: String,
    pub outcome: Outcome,
    pub detail: String,
}

#[derive(Debug, Default, Serialize)]
pub struct BatchReport {
    pub items: Vec<BackupItem>,
    /// 由断点清单直接跳过、完全没发网络请求的项数。
    pub resumed: usize,
}

impl BatchReport {
    /// (uploaded, skipped, failed)
    pub fn counts(&self) -> (usize, usize, usize) {
        let mut uploaded = 0;
        let mut skipped = 0;
        let mut failed = 0;
        for item in &self.items {
            match item.outcome {
                Outcome::Uploaded => uploaded += 1,
                Outcome::Skipped => skipped += 1,
                Outcome::Failed => failed += 1,
            }
        }
        (uploaded, skipped, failed)
    }

    pub fn ok(&self) -> bool {
        self.counts().2 == 0
    }
}

/// 最近一次备份的摘要，`aether backup` 结束时落盘，供 `aether ui` 展示。
///
/// 单独一个文件而不是塞进 `state.json`：备份是独立命令，可能和 `start`
/// 的引擎进程同时存在，写同一份状态会互相覆盖（引擎每 5s 心跳写一次）。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BackupSummary {
    pub finished_at: u64,
    pub local: String,
    pub remote: String,
    pub uploaded: usize,
    pub skipped: usize,
    pub failed: usize,
    /// 命中断点、完全没发网络请求的项数。
    pub resumed: usize,
    /// 计划上传的总字节数。
    pub bytes: u64,
    pub duration_secs: u64,
}

impl BackupSummary {
    pub fn from_report(
        report: &BatchReport,
        local: &Path,
        remote: &str,
        bytes: u64,
        duration_secs: u64,
    ) -> Self {
        let (uploaded, skipped, failed) = report.counts();
        Self {
            finished_at: unix_now(),
            local: local.display().to_string().replace('\\', "/"),
            remote: remote.to_string(),
            uploaded,
            skipped,
            failed,
            resumed: report.resumed,
            bytes,
            duration_secs,
        }
    }

    pub fn path(home: &Path) -> PathBuf {
        home.join("last-backup.json")
    }

    pub fn save(&self, home: &Path) -> std::io::Result<()> {
        let json = serde_json::to_string_pretty(self).map_err(std::io::Error::other)?;
        crate::state::atomic_write(&Self::path(home), json.as_bytes())
    }

    pub fn load(home: &Path) -> Option<Self> {
        let text = fs::read_to_string(Self::path(home)).ok()?;
        serde_json::from_str(&text).ok()
    }
}

#[derive(Debug)]
pub struct PlannedFile {
    pub local: PathBuf,
    pub remote: String,
    pub size: u64,
}

pub fn plan_total(planned: &[PlannedFile]) -> u64 {
    planned.iter().map(|file| file.size).sum()
}

fn is_excluded_dir(name: &str) -> bool {
    DEFAULT_EXCLUDES.iter().any(|excluded| {
        if cfg!(windows) {
            name.eq_ignore_ascii_case(excluded)
        } else {
            name == *excluded
        }
    })
}

fn walk(
    dir: &Path,
    base: &Path,
    remote_dir: &str,
    out: &mut Vec<PlannedFile>,
) -> Result<(), String> {
    let entries =
        fs::read_dir(dir).map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
    for entry in entries {
        let entry = entry.map_err(|error| format!("cannot read {}: {error}", dir.display()))?;
        let name = entry.file_name().to_string_lossy().into_owned();
        let kind = entry
            .file_type()
            .map_err(|error| format!("cannot stat {}: {error}", entry.path().display()))?;
        let path = entry.path();
        if kind.is_dir() {
            if is_excluded_dir(&name) {
                continue;
            }
            walk(&path, base, remote_dir, out)?;
        } else if kind.is_file() {
            // file_type() does not follow symlinks, so this cannot loop back up the tree.
            let relative = path
                .strip_prefix(base)
                .map_err(|error| format!("relative path failed for {}: {error}", path.display()))?
                .to_string_lossy()
                .replace('\\', "/");
            let remote = alist::join_remote_path(remote_dir, Path::new(&relative))?;
            let size = entry
                .metadata()
                .map_err(|error| format!("cannot stat {}: {error}", path.display()))?
                .len();
            out.push(PlannedFile {
                local: path,
                remote,
                size,
            });
        }
    }
    Ok(())
}

/// 单文件保持原有行为（上传到 `remote_dir/<filename>`）；目录则递归并保留
/// 相对目录结构。排除规则与 watcher 一致：路径中任意层级的 `.git`/`bin`/`obj`。
pub fn plan(local: &Path, remote_dir: &str) -> Result<Vec<PlannedFile>, String> {
    let metadata = fs::metadata(local)
        .map_err(|error| format!("cannot access {}: {error}", local.display()))?;
    let mut files = Vec::new();
    if metadata.is_file() {
        let name = local
            .file_name()
            .ok_or_else(|| format!("cannot determine filename: {}", local.display()))?
            .to_string_lossy()
            .into_owned();
        let remote = alist::build_remote_path(remote_dir, &name)?;
        files.push(PlannedFile {
            local: local.to_path_buf(),
            remote,
            size: metadata.len(),
        });
    } else if metadata.is_dir() {
        walk(local, local, remote_dir, &mut files)?;
    } else {
        return Err(format!(
            "not a regular file or directory: {}",
            local.display()
        ));
    }
    files.sort_by(|a, b| a.remote.cmp(&b.remote));
    Ok(files)
}

pub struct BackupOptions<'a> {
    pub alist_url: &'a str,
    pub token: &'a str,
    pub policy: ConflictPolicy,
}

fn item(local: &Path, remote: &str, outcome: Outcome, detail: impl Into<String>) -> BackupItem {
    BackupItem {
        local: local.display().to_string(),
        remote: remote.to_string(),
        outcome,
        detail: detail.into(),
    }
}

fn snapshot(path: &Path) -> Result<(u64, Option<SystemTime>), String> {
    let metadata = fs::metadata(path)
        .map_err(|error| format!("cannot stat {}: {error}", path.display()))?;
    Ok((metadata.len(), metadata.modified().ok()))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn renamed_path(remote: &str, stamp: u64) -> String {
    let (dir, name) = remote.rsplit_once('/').unwrap_or(("", remote));
    let new_name = match name.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() => format!("{stem}-conflict-{stamp}.{ext}"),
        _ => format!("{name}-conflict-{stamp}"),
    };
    format!("{dir}/{new_name}")
}

/// 进度按「已处理文件字节」校准：上传中途失败也能走到 100%，不会卡住进度条。
fn process_one(
    client: &Client,
    options: &BackupOptions<'_>,
    file: &PlannedFile,
    sent: &Arc<AtomicU64>,
) -> BackupItem {
    let start = sent.load(Ordering::Relaxed);
    let result = attempt(client, options, file, sent);
    sent.store(start + file.size, Ordering::Relaxed);
    result
}

fn attempt(
    client: &Client,
    options: &BackupOptions<'_>,
    file: &PlannedFile,
    sent: &Arc<AtomicU64>,
) -> BackupItem {
    // 本地占用预检：Windows 独占写锁会让 File::open 直接失败。
    let before = match snapshot(&file.local) {
        Ok(value) => value,
        Err(error) => return item(&file.local, &file.remote, Outcome::Failed, error),
    };
    if let Err(error) = fs::File::open(&file.local) {
        return item(
            &file.local,
            &file.remote,
            Outcome::Failed,
            format!("local file is locked or unreadable: {error}"),
        );
    }

    // 探测失败必须保守跳过：绝不能把鉴权 / 网络错误当成「云端无文件」而覆盖。
    let cloud = match alist::fs_get(client, options.alist_url, options.token, &file.remote) {
        Ok(value) => value,
        Err(error) => {
            return item(
                &file.local,
                &file.remote,
                Outcome::Failed,
                format!("conflict check failed, not uploaded: {error}"),
            )
        }
    };

    let target: String;
    let mut detail = String::new();
    match cloud {
        None => target = file.remote.clone(),
        Some(remote) if remote.size == Some(file.size) => {
            return item(
                &file.local,
                &file.remote,
                Outcome::Skipped,
                format!(
                    "cloud already has same size ({} bytes), skipped",
                    file.size
                ),
            )
        }
        Some(remote) => {
            let cloud_size = remote
                .size
                .map(|value| value.to_string())
                .unwrap_or_else(|| "unknown".to_string());
            let cloud_modified = remote.modified.unwrap_or_else(|| "unknown".to_string());
            let reason = format!(
                "conflict: cloud size {cloud_size} vs local {} bytes (cloud modified {cloud_modified})",
                file.size
            );
            match options.policy {
                ConflictPolicy::Skip => {
                    return item(
                        &file.local,
                        &file.remote,
                        Outcome::Skipped,
                        format!("{reason}; skipped by policy"),
                    )
                }
                ConflictPolicy::Overwrite => {
                    target = file.remote.clone();
                    detail = format!("{reason}; overwritten by policy");
                }
                ConflictPolicy::Rename => {
                    let candidate = renamed_path(&file.remote, unix_now());
                    match alist::fs_get(client, options.alist_url, options.token, &candidate) {
                        Ok(None) => {
                            target = candidate;
                            detail = format!("{reason}; renamed");
                        }
                        Ok(Some(_)) => {
                            return item(
                                &file.local,
                                &candidate,
                                Outcome::Failed,
                                format!("{reason}; rename target already exists"),
                            )
                        }
                        Err(error) => {
                            return item(
                                &file.local,
                                &candidate,
                                Outcome::Failed,
                                format!("{reason}; rename probe failed: {error}"),
                            )
                        }
                    }
                }
            }
        }
    }

    match alist::upload_file_to_path(
        client,
        options.alist_url,
        options.token,
        &file.local,
        &target,
        Some(Arc::clone(sent)),
    ) {
        Err(error) => item(&file.local, &target, Outcome::Failed, error),
        Ok(remote) => match snapshot(&file.local) {
            // 上传期间被外部改写：云端副本可能不一致，明确标记为失败。
            Ok(after) if after != before => item(
                &file.local,
                &remote,
                Outcome::Failed,
                "local file changed during upload (external write / lock); uploaded copy may be inconsistent",
            ),
            _ => item(&file.local, &remote, Outcome::Uploaded, detail),
        },
    }
}

/// 顺序处理，单个文件失败不中断整批；结果全部记入 `BatchReport`。
///
/// 传入 `resume_store` 时启用任务级断点：
/// - 指纹（size + mtime）与清单一致的文件直接跳过，**零网络请求**；
/// - 每个上传成功的文件立刻写入清单，进程中途被杀也能从断点继续；
/// - 只有 `Uploaded` 才记完成，冲突跳过 / 失败项下次仍会重新校验；
/// - 整批无失败时自动清理清单，避免陈旧缓存长期生效。
pub fn run_backup(
    client: &Client,
    local: &Path,
    remote_dir: &str,
    options: &BackupOptions<'_>,
    sent: Arc<AtomicU64>,
    mut resume_store: Option<&mut ResumeStore>,
) -> Result<BatchReport, String> {
    let planned = plan(local, remote_dir)?;
    let mut report = BatchReport::default();
    for file in &planned {
        // 本地文件拿不到 metadata 时绝不认作已完成，让常规链路给出明确失败原因。
        let resumed_remote = snapshot(&file.local).ok().and_then(|(size, modified)| {
            resume_store.as_ref().and_then(|store| {
                store.is_done(&file.remote, size, modified).then(|| {
                    store
                        .entry_remote(&file.remote)
                        .unwrap_or(&file.remote)
                        .to_string()
                })
            })
        });

        let result = if let Some(remote) = resumed_remote {
            report.resumed += 1;
            let start = sent.load(Ordering::Relaxed);
            sent.store(start + file.size, Ordering::Relaxed);
            item(
                &file.local,
                &remote,
                Outcome::Skipped,
                "resumed: completed in a previous run; use --reset-resume to force a full run",
            )
        } else {
            process_one(client, options, file, &sent)
        };

        if result.outcome == Outcome::Uploaded {
            if let Some(store) = resume_store.as_deref_mut() {
                let (size, modified) = snapshot(&file.local)
                    .map(|(size, modified)| (size, modified))
                    .unwrap_or((file.size, None));
                if let Err(error) =
                    store.record(&file.remote, ResumeEntry::new(&result.remote, size, modified))
                {
                    eprintln!("[aether] warning: resume checkpoint not saved: {error}");
                }
            }
        }
        report.items.push(result);
    }

    // 全部成功（含按策略跳过）后清掉清单：下次仍会走云端探测，防止云端被外部
    // 删除却因本地缓存而永久漏传。失败时保留，供 --resume 续跑。
    if report.ok() {
        if let Some(store) = resume_store.as_deref() {
            if let Err(error) = store.clear() {
                eprintln!("[aether] warning: cannot clear resume manifest: {error}");
            }
        }
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn summary_maps_report_counts_and_roundtrips() {
        let report = BatchReport {
            items: vec![
                BackupItem {
                    local: "a.txt".into(),
                    remote: "/r/a.txt".into(),
                    outcome: Outcome::Uploaded,
                    detail: String::new(),
                },
                BackupItem {
                    local: "b.txt".into(),
                    remote: "/r/b.txt".into(),
                    outcome: Outcome::Skipped,
                    detail: "same size".into(),
                },
            ],
            resumed: 4,
        };

        let summary = BackupSummary::from_report(&report, Path::new("C:/src"), "/r", 1536, 12);
        assert_eq!(summary.uploaded, 1);
        assert_eq!(summary.skipped, 1);
        assert_eq!(summary.failed, 0);
        assert_eq!(summary.resumed, 4);
        assert_eq!(summary.local, "C:/src");
        assert_eq!(summary.bytes, 1536);

        let dir = TempDir::new().unwrap();
        summary.save(dir.path()).unwrap();
        let loaded = BackupSummary::load(dir.path()).expect("summary should roundtrip");
        assert_eq!(loaded.remote, "/r");
        assert_eq!(loaded.duration_secs, 12);
    }

    #[test]
    fn summary_load_returns_none_when_absent() {
        let dir = TempDir::new().unwrap();
        assert!(BackupSummary::load(dir.path()).is_none());
    }

    #[test]
    fn plans_single_file_exactly_like_before() {
        let dir = TempDir::new().unwrap();
        let file = dir.path().join("a.txt");
        fs::write(&file, b"abc").unwrap();
        let planned = plan(&file, "/backup").unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].remote, "/backup/a.txt");
        assert_eq!(planned[0].size, 3);
    }

    #[test]
    fn plans_directory_recursively_preserving_layout_and_excluding_build_dirs() {
        let dir = TempDir::new().unwrap();
        let root = dir.path().join("project");
        fs::create_dir_all(root.join("src")).unwrap();
        fs::create_dir_all(root.join(".git/objects")).unwrap();
        fs::create_dir_all(root.join("bin")).unwrap();
        fs::create_dir_all(root.join("obj")).unwrap();
        fs::write(root.join("README.md"), b"r").unwrap();
        fs::write(root.join("src/main.rs"), b"m").unwrap();
        fs::write(root.join(".git/config"), b"g").unwrap();
        fs::write(root.join("bin/app.dll"), b"b").unwrap();
        fs::write(root.join("obj/app.o"), b"o").unwrap();

        let planned = plan(&root, "/backup").unwrap();
        let remotes: Vec<&str> = planned.iter().map(|f| f.remote.as_str()).collect();
        assert_eq!(remotes, ["/backup/README.md", "/backup/src/main.rs"]);
        assert_eq!(plan_total(&planned), 2);
    }

    #[test]
    fn rejects_missing_local_path() {
        let dir = TempDir::new().unwrap();
        assert!(plan(&dir.path().join("nope"), "/backup").is_err());
    }

    #[test]
    fn renamed_path_keeps_extension_and_directory() {
        assert_eq!(
            renamed_path("/backup/sub/a.txt", 1700000000),
            "/backup/sub/a-conflict-1700000000.txt"
        );
        assert_eq!(renamed_path("/a", 7), "/a-conflict-7");
        assert_eq!(renamed_path("/backup/.gitignore", 7), "/backup/.gitignore-conflict-7");
    }

    #[test]
    fn report_counts_and_ok() {
        let mut report = BatchReport::default();
        let path = Path::new("a.txt");
        report.items.push(item(path, "/a", Outcome::Uploaded, ""));
        report.items.push(item(path, "/b", Outcome::Skipped, "conflict"));
        assert_eq!(report.counts(), (1, 1, 0));
        assert!(report.ok());
        report.items.push(item(path, "/c", Outcome::Failed, "denied"));
        assert_eq!(report.counts(), (1, 1, 1));
        assert!(!report.ok());
    }
}
