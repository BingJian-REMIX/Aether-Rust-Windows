//! 任务级断点续传：把已成功上传的文件清单持久化到 `~/.aether/backups/`。
//!
//! Alist `/api/fs/put` 是整文件 PUT，官方 API 没有 Range / 分片合并接口；
//! `Content-Range` 只存在于第三方 OpenList_Chunk fork，服务端 Chunker 驱动
//! 对客户端透明。因此**字节级断点续传不可实现**，本模块实现的是「任务级断点」：
//! 单个文件上传成功后立刻落盘，进程被中断（断网、Ctrl+C、崩溃）后重跑只补做
//! 未完成 / 已失效的项。
//!
//! 清单绝不能放进被备份（或被调用方监听）的目录树，否则写入会自我触发同步；
//! 默认放 `USERPROFILE/.aether`，可用 `AETHER_HOME` 覆盖。

use crate::state;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: u32 = 1;

/// 一项已完成的文件。`remote` 是**实际写入云端**的路径（`rename` 策略下与
/// 计划路径不同）；size + mtime 组成指纹，本地文件一改动该记录立即失效。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeEntry {
    pub remote: String,
    pub size: u64,
    pub modified_secs: u64,
    pub modified_nanos: u32,
}

impl ResumeEntry {
    pub fn new(remote: &str, size: u64, modified: Option<SystemTime>) -> Self {
        let (modified_secs, modified_nanos) = split_modified(modified);
        Self {
            remote: remote.to_string(),
            size,
            modified_secs,
            modified_nanos,
        }
    }

    pub fn matches(&self, size: u64, modified: Option<SystemTime>) -> bool {
        let (secs, nanos) = split_modified(modified);
        self.size == size && self.modified_secs == secs && self.modified_nanos == nanos
    }
}

/// 把 `SystemTime` 拆成可 JSON 化的秒 + 纳秒（避开 u128 在 serde_json 的精度坑）。
pub fn split_modified(value: Option<SystemTime>) -> (u64, u32) {
    match value.and_then(|time| time.duration_since(UNIX_EPOCH).ok()) {
        Some(duration) => (duration.as_secs(), duration.subsec_nanos()),
        // 某些文件系统 / 平台拿不到 mtime：退化为「只看 size」，仍比无校验安全。
        None => (0, 0),
    }
}

/// 一次备份任务的身份。三者任一变化都换一份清单，避免跨站点 / 跨目录误跳过。
#[derive(Clone, Copy, Debug)]
pub struct JobSpec<'a> {
    pub alist_url: &'a str,
    pub remote_dir: &'a str,
    pub local: &'a str,
}

/// FNV-1a 64 位：不引入新依赖，且跨 Rust 版本 / 平台结果稳定（不像 DefaultHasher）。
pub fn job_key(spec: JobSpec<'_>) -> String {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in format!("{}\0{}\0{}", spec.alist_url, spec.remote_dir, spec.local).as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:016x}")
}

pub fn manifest_path(home: &Path, spec: JobSpec<'_>) -> PathBuf {
    home.join("backups").join(format!("{}.json", job_key(spec)))
}

/// 拒绝把清单放进被备份的**目录树**（会触发 watcher 自我循环）。
///
/// 只对目录模式生效：单文件模式下 `~/.aether` 位于文件父目录内是正常情况
/// （例如备份 `~/report.pdf`），写清单不会改动被备份文件本身。
pub fn ensure_outside_tree(home: &Path, local: &Path) -> Result<(), String> {
    if !local.is_dir() {
        return Ok(());
    }
    let tree = local.to_path_buf();
    // absolute() 只做词法规范化，不需要路径存在，也不会产生 Windows \\?\ 前缀。
    let tree_abs = std::path::absolute(&tree).unwrap_or(tree);
    let home_abs = std::path::absolute(home).unwrap_or_else(|_| home.to_path_buf());
    if home_abs == tree_abs || home_abs.starts_with(&tree_abs) {
        return Err(format!(
            "resume manifest ({}) would be inside the backed-up tree ({}); \
             set AETHER_HOME to a directory outside it",
            home_abs.display(),
            tree_abs.display()
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResumeManifest {
    pub version: u32,
    pub key: String,
    pub alist_url: String,
    pub remote_dir: String,
    pub local: String,
    pub updated_at: u64,
    #[serde(default)]
    pub entries: HashMap<String, ResumeEntry>,
}

impl ResumeManifest {
    fn fresh(spec: JobSpec<'_>, key: String) -> Self {
        Self {
            version: SCHEMA_VERSION,
            key,
            alist_url: spec.alist_url.to_string(),
            remote_dir: spec.remote_dir.to_string(),
            local: spec.local.to_string(),
            updated_at: state::now(),
            entries: HashMap::new(),
        }
    }

    fn belongs_to(&self, spec: JobSpec<'_>, key: &str) -> bool {
        self.version == SCHEMA_VERSION
            && self.key == key
            && self.alist_url == spec.alist_url
            && self.remote_dir == spec.remote_dir
            && self.local == spec.local
    }
}

/// 单个 CLI 进程内的清单句柄。成功后逐项 `record`（立即落盘），
/// 整批成功再 `clear`，失败时保留供下次续跑。
pub struct ResumeStore {
    path: PathBuf,
    manifest: ResumeManifest,
}

impl ResumeStore {
    /// 读取已有清单；损坏 / 版本不符 / 任务不匹配的清单自动当作新任务（自愈）。
    pub fn open(home: &Path, spec: JobSpec<'_>) -> Result<Self, String> {
        ensure_outside_tree(home, Path::new(spec.local))?;
        let key = job_key(spec);
        let path = manifest_path(home, spec);
        let loaded = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<ResumeManifest>(&text).ok());
        let manifest = match loaded {
            Some(value) if value.belongs_to(spec, &key) => value,
            _ => ResumeManifest::fresh(spec, key),
        };
        Ok(Self { path, manifest })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn len(&self) -> usize {
        self.manifest.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.manifest.entries.is_empty()
    }

    /// 指纹一致才算完成；本地文件被改写后必须重新上传。
    pub fn is_done(&self, planned_remote: &str, size: u64, modified: Option<SystemTime>) -> bool {
        self.manifest
            .entries
            .get(planned_remote)
            .is_some_and(|entry| entry.matches(size, modified))
    }

    pub fn entry_remote(&self, planned_remote: &str) -> Option<&str> {
        self.manifest
            .entries
            .get(planned_remote)
            .map(|entry| entry.remote.as_str())
    }

    pub fn record(&mut self, planned_remote: &str, entry: ResumeEntry) -> Result<(), String> {
        self.manifest
            .entries
            .insert(planned_remote.to_string(), entry);
        self.manifest.updated_at = state::now();
        self.save()
    }

    pub fn save(&self) -> Result<(), String> {
        let json = serde_json::to_string_pretty(&self.manifest)
            .map_err(|error| format!("cannot serialize resume manifest: {error}"))?;
        state::atomic_write(&self.path, json.as_bytes())
            .map_err(|error| format!("cannot write {}: {error}", self.path.display()))
    }

    pub fn clear(&self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(format!("cannot remove {}: {error}", self.path.display())),
        }
    }

    /// `--reset-resume`：磁盘文件与内存快照一起清空，本轮等价于全新任务。
    pub fn reset(&mut self) -> Result<(), String> {
        self.manifest.entries.clear();
        self.manifest.updated_at = state::now();
        self.clear()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    fn spec<'a>(url: &'a str, remote: &'a str, local: &'a str) -> JobSpec<'a> {
        JobSpec {
            alist_url: url,
            remote_dir: remote,
            local,
        }
    }

    #[test]
    fn job_key_is_stable_and_sensitive_to_every_field() {
        let a = spec("https://a", "/backup", "D:/repo");
        assert_eq!(job_key(a), job_key(a));
        assert_eq!(job_key(a).len(), 16);
        assert_ne!(job_key(a), job_key(spec("https://b", "/backup", "D:/repo")));
        assert_ne!(job_key(a), job_key(spec("https://a", "/other", "D:/repo")));
        assert_ne!(job_key(a), job_key(spec("https://a", "/backup", "D:/else")));
    }

    #[test]
    fn manifest_is_under_aether_home_and_not_in_the_tree() {
        let home = TempDir::new().unwrap();
        let path = manifest_path(home.path(), spec("https://a", "/backup", "D:/repo"));
        assert!(path.starts_with(home.path().join("backups")));
        assert!(path.to_string_lossy().ends_with(".json"));
    }

    #[test]
    fn refuses_a_home_inside_the_backed_up_tree() {
        let dir = TempDir::new().unwrap();
        let project = dir.path().join("project");
        fs::create_dir_all(&project).unwrap();
        let inside = project.join(".aether");
        fs::create_dir_all(&inside).unwrap();

        let error = ensure_outside_tree(&inside, &project).unwrap_err();
        assert!(error.contains("inside the backed-up tree"), "{error}");
        assert!(error.contains("AETHER_HOME"), "{error}");
        assert!(ensure_outside_tree(dir.path(), &project).is_ok());
    }

    #[test]
    fn single_file_backup_may_live_next_to_the_manifest() {
        let dir = TempDir::new().unwrap();
        let home = dir.path().join(".aether");
        fs::create_dir_all(&home).unwrap();
        let file = dir.path().join("report.pdf");
        fs::write(&file, b"x").unwrap();

        // 备份 ~/report.pdf 时清单位于 ~/.aether，二者同父目录但互不影响。
        assert!(ensure_outside_tree(&home, &file).is_ok());
    }

    #[test]
    fn roundtrip_and_fingerprint_invalidation() {
        let home = TempDir::new().unwrap();
        let job = spec("https://a", "/backup", "D:/repo");
        let modified = Some(UNIX_EPOCH + std::time::Duration::new(1_700_000_000, 123));
        let mut store = ResumeStore::open(home.path(), job).unwrap();
        assert!(store.is_empty());
        store
            .record(
                "/backup/a.txt",
                ResumeEntry::new("/backup/a.txt", 42, modified),
            )
            .unwrap();
        assert_eq!(store.len(), 1);

        let reopened = ResumeStore::open(home.path(), job).unwrap();
        assert_eq!(reopened.len(), 1);
        assert!(reopened.is_done("/backup/a.txt", 42, modified));
        assert_eq!(reopened.entry_remote("/backup/a.txt"), Some("/backup/a.txt"));
        assert!(!reopened.is_done("/backup/a.txt", 43, modified), "size changed");
        assert!(!reopened.is_done("/backup/a.txt", 42, None), "mtime changed");
        assert!(!reopened.is_done("/backup/other.txt", 42, modified), "wrong key");
    }

    #[test]
    fn stale_or_corrupt_manifest_is_replaced_by_a_fresh_one() {
        let home = TempDir::new().unwrap();
        let job = spec("https://a", "/backup", "D:/repo");
        {
            let mut store = ResumeStore::open(home.path(), job).unwrap();
            store
                .record("/backup/a.txt", ResumeEntry::new("/backup/a.txt", 1, None))
                .unwrap();
        }
        let other = spec("https://a", "/other", "D:/repo");
        assert!(ResumeStore::open(home.path(), other).unwrap().is_empty());

        let broken = spec("https://broken", "/backup", "D:/repo");
        let path = manifest_path(home.path(), broken);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, b"{ this is not json").unwrap();
        assert!(ResumeStore::open(home.path(), broken).unwrap().is_empty());
    }

    #[test]
    fn clear_is_idempotent_and_keeps_other_jobs() {
        let home = TempDir::new().unwrap();
        let a = spec("https://a", "/backup", "D:/repo");
        let b = spec("https://b", "/backup", "D:/repo");
        let store_a = ResumeStore::open(home.path(), a).unwrap();
        let mut store_b = ResumeStore::open(home.path(), b).unwrap();
        store_b
            .record("/backup/a.txt", ResumeEntry::new("/backup/a.txt", 1, None))
            .unwrap();

        store_a.clear().unwrap();
        store_a.clear().unwrap();
        assert_eq!(ResumeStore::open(home.path(), b).unwrap().len(), 1);
        store_b.clear().unwrap();
        assert!(ResumeStore::open(home.path(), b).unwrap().is_empty());
    }
}
