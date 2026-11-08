//! Alist 网盘聚合平台直连上传（阻塞式 HTTP，不引入异步生命周期）。
//!
//! Alist `/api/fs/put` 接口约定：
//!
//! ```text
//! PUT {alist_url}/api/fs/put
//! Authorization: {token}
//! File-Path:     {URL 编码后的远程绝对路径}
//! Content-Length: {文件大小}
//! Body:          原始文件字节流
//! ```
//!
//! 响应形如 `{"code":200,"message":"success","data":null}`，`code == 200` 视为成功。
//! `File-Path` 必须是 ASCII，中文/空格等字符需要 URL 编码，否则 HTTP header 非法。

use reqwest::blocking::{Body, Client};
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_LENGTH, CONTENT_TYPE};
use serde::Deserialize;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

/// 单次请求的总超时，避免网络异常时无限挂起。
pub const UPLOAD_TIMEOUT: Duration = Duration::from_secs(300);

/// 批量上传复用同一个 Client，避免每个文件重复建连 / TLS 握手。
pub fn new_client() -> Result<Client, String> {
    Client::builder()
        .timeout(UPLOAD_TIMEOUT)
        .build()
        .map_err(|error| format!("cannot build HTTP client: {error}"))
}

/// Alist 通用响应的最小子集。`data` 因接口而异，保留但忽略内容。
#[derive(Debug, Deserialize)]
pub struct AlistResponse {
    pub code: i64,
    #[serde(default)]
    pub message: Option<String>,
    #[serde(default)]
    pub data: Option<serde_json::Value>,
}

/// 把用户传入的目录规范化成 Alist 绝对路径：`/` 或 `/a/b`。
///
/// 同时接受 `/backup`、`backup`、`/backup/`、`a//b` 以及 Windows 反斜杠写法。
pub fn normalize_remote_dir(remote_dir: &str) -> String {
    let unified = remote_dir.replace('\\', "/");
    let segments: Vec<&str> = unified
        .split('/')
        .filter(|segment| !segment.is_empty() && *segment != ".")
        .collect();
    if segments.is_empty() {
        "/".to_string()
    } else {
        format!("/{}", segments.join("/"))
    }
}

/// 拼出可读的远程绝对路径，供返回值与终端提示使用（不做 URL 编码）。
pub fn build_remote_path(remote_dir: &str, filename: &str) -> Result<String, String> {
    let name = filename.trim();
    if name.is_empty() {
        return Err("remote filename is empty".to_string());
    }
    if name.contains('/') || name.contains('\\') {
        return Err(format!("filename must not contain a path separator: {name}"));
    }
    let dir = normalize_remote_dir(remote_dir);
    if dir.split('/').any(|segment| segment == "..") {
        return Err(format!("remote directory must not contain '..': {remote_dir}"));
    }
    Ok(if dir == "/" {
        format!("/{name}")
    } else {
        format!("{dir}/{name}")
    })
}

/// 对 `File-Path` 分段 URL 编码：保留 `/` 作为路径分隔，段内字符全部编码。
///
/// `urlencoding` 采用 form 编码，空格会变成 `+`；而 Alist 服务端用 Go 的
/// `url.PathUnescape` 解码，`+` 不会被还原成空格。`%20` 对 PathUnescape 和
/// QueryUnescape 都正确，因此这里统一替换。
pub fn encode_file_path(remote_path: &str) -> String {
    remote_path
        .split('/')
        .map(|segment| urlencoding::encode(segment).replace('+', "%20"))
        .collect::<Vec<_>>()
        .join("/")
}

struct Prepared {
    file: File,
    size: u64,
    remote_path: String,
}

fn prepare(local_path: &Path, remote_dir: &str) -> Result<Prepared, String> {
    let metadata = fs::metadata(local_path)
        .map_err(|error| format!("cannot access {}: {error}", local_path.display()))?;
    if !metadata.is_file() {
        return Err(format!("not a regular file: {}", local_path.display()));
    }
    let filename = local_path
        .file_name()
        .ok_or_else(|| format!("cannot determine filename: {}", local_path.display()))?
        .to_string_lossy()
        .into_owned();
    let remote_path = build_remote_path(remote_dir, &filename)?;
    let file = File::open(local_path)
        .map_err(|error| format!("cannot open {}: {error}", local_path.display()))?;
    Ok(Prepared {
        file,
        size: metadata.len(),
        remote_path,
    })
}

fn clip(text: &str, limit: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= limit {
        return trimmed.to_string();
    }
    format!("{}...", trimmed.chars().take(limit).collect::<String>())
}

fn put(
    client: &Client,
    alist_url: &str,
    token: &str,
    remote_path: String,
    body: Body,
    size: u64,
) -> Result<String, String> {
    let base = alist_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Alist URL is empty (set AETHER_ALIST_URL or pass --url)".to_string());
    }
    let url = format!("{base}/api/fs/put");
    let encoded = encode_file_path(&remote_path);

    // HeaderValue 拒绝非 ASCII / 控制字符。提前校验，避免请求构造阶段 panic。
    let auth = HeaderValue::from_str(token.trim())
        .map_err(|error| format!("invalid Authorization token: {error}"))?;
    let file_path = HeaderValue::from_str(&encoded)
        .map_err(|error| format!("invalid File-Path header: {error}"))?;
    let length = HeaderValue::from_str(&size.to_string())
        .map_err(|error| format!("invalid Content-Length: {error}"))?;

    let response = client
        .put(&url)
        .header(AUTHORIZATION, auth)
        .header("File-Path", file_path)
        .header(CONTENT_LENGTH, length)
        .header(CONTENT_TYPE, "application/octet-stream")
        .body(body)
        .send()
        .map_err(|error| format!("PUT {url} failed: {error}"))?;

    let status = response.status();
    let text = response
        .text()
        .map_err(|error| format!("cannot read Alist response: {error}"))?;
    let parsed: AlistResponse = serde_json::from_str(&text).map_err(|error| {
        format!(
            "Alist returned invalid JSON (HTTP {}): {error}: {}",
            status.as_u16(),
            clip(&text, 300)
        )
    })?;

    // Alist 的业务状态码是最终判据；HTTP 状态写进错误信息便于排障。
    if parsed.code != 200 {
        return Err(format!(
            "Alist upload failed (HTTP {}, code {}): {}",
            status.as_u16(),
            parsed.code,
            parsed.message
                .unwrap_or_else(|| clip(&text, 300))
        ));
    }
    Ok(remote_path)
}

/// 上传本地文件到 Alist。
///
/// 成功时返回上传后的**可读远程路径**（未 URL 编码，如 `/backup/报告.txt`），
/// 方便调用方继续拼接或记录；失败时返回带上下文的错误字符串。
///
/// `alist_url` 传站点根（如 `https://alist.example.com`），`token` 传 Alist
/// 控制台生成的访问令牌原值（不加 `Bearer` 前缀）。
pub fn upload_to_alist(
    alist_url: &str,
    token: &str,
    local_path: &Path,
    remote_dir: &str,
) -> Result<String, String> {
    let client = new_client()?;
    let prepared = prepare(local_path, remote_dir)?;
    put(
        &client,
        alist_url,
        token,
        prepared.remote_path,
        Body::from(prepared.file),
        prepared.size,
    )
}

/// 为读取文件流套一层计数器，供终端显示真实上传进度。
struct ProgressReader {
    inner: File,
    sent: Arc<AtomicU64>,
}

impl Read for ProgressReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let read = self.inner.read(buffer)?;
        if read > 0 {
            self.sent.fetch_add(read as u64, Ordering::Relaxed);
        }
        Ok(read)
    }
}

/// [`upload_to_alist`] 的进度版：`sent` 会在请求过程中持续累加已发送字节数。
///
/// 语义、headers、判定规则与 [`upload_to_alist`] 完全一致，仅 body 换成带计数的
/// 文件流。`Body` 只为 `File` 实现了 `From`，包一层 Reader 后改用 `Body::sized`
/// 显式给出长度（等价地让 reqwest 生成 Content-Length，不使用 chunked）。
/// CLI 用它驱动进度输出，未来 GUI / daemon 复用同一入口即可。
pub fn upload_to_alist_with_progress(
    alist_url: &str,
    token: &str,
    local_path: &Path,
    remote_dir: &str,
    sent: Arc<AtomicU64>,
) -> Result<String, String> {
    let client = new_client()?;
    let prepared = prepare(local_path, remote_dir)?;
    sent.store(0, Ordering::Relaxed);
    let reader = ProgressReader {
        inner: prepared.file,
        sent,
    };
    put(
        &client,
        alist_url,
        token,
        prepared.remote_path,
        Body::sized(reader, prepared.size),
        prepared.size,
    )
}

/// 把相对路径（可含多级目录）拼到远程目录下，保留目录结构。
///
/// `..` 和盘符前缀会被拒绝，防止越权写入；非空路径段原样保留，编码由
/// [`encode_file_path`] 在发请求时统一处理。
pub fn join_remote_path(remote_dir: &str, relative: &Path) -> Result<String, String> {
    let mut segments: Vec<String> = Vec::new();
    for component in relative.components() {
        match component {
            std::path::Component::Normal(part) => {
                let text = part.to_string_lossy();
                if text.is_empty() {
                    return Err(format!("empty path segment in {}", relative.display()));
                }
                segments.push(text.into_owned());
            }
            std::path::Component::ParentDir => {
                return Err(format!("relative path must not contain '..': {}", relative.display()));
            }
            std::path::Component::CurDir | std::path::Component::RootDir
            | std::path::Component::Prefix(_) => {}
        }
    }
    if segments.is_empty() {
        return Err(format!("empty relative path: {}", relative.display()));
    }
    let dir = normalize_remote_dir(remote_dir);
    if dir.split('/').any(|segment| segment == "..") {
        return Err(format!("remote directory must not contain '..': {remote_dir}"));
    }
    Ok(if dir == "/" {
        format!("/{}", segments.join("/"))
    } else {
        format!("{dir}/{}", segments.join("/"))
    })
}

/// `/api/fs/get` 返回的云端文件信息。只取冲突判定需要的字段；不同 Alist
/// 版本字段不一致，缺失时均为 `None`。
#[derive(Debug, Default, Deserialize)]
pub struct RemoteFile {
    #[serde(default)]
    pub size: Option<u64>,
    #[serde(default)]
    pub modified: Option<String>,
}

fn looks_not_found(parsed: &AlistResponse) -> bool {
    let Some(message) = &parsed.message else {
        return false;
    };
    let lower = message.to_ascii_lowercase();
    lower.contains("not found")
        || lower.contains("no such")
        || lower.contains("does not exist")
        || lower.contains("不存在")
}

/// 探测云端同名文件。`Ok(None)` 表示云端不存在；`Ok(Some(_))` 表示存在。
///
/// 探测失败（鉴权、网络、非预期响应）一律返回 `Err`，调用方必须**保守跳过**
/// 而不是当作「云端无文件」直接覆盖。
pub fn fs_get(
    client: &Client,
    alist_url: &str,
    token: &str,
    remote_path: &str,
) -> Result<Option<RemoteFile>, String> {
    let base = alist_url.trim().trim_end_matches('/');
    if base.is_empty() {
        return Err("Alist URL is empty".to_string());
    }
    let url = format!("{base}/api/fs/get");
    let auth = HeaderValue::from_str(token.trim())
        .map_err(|error| format!("invalid Authorization token: {error}"))?;
    let request = serde_json::json!({ "path": remote_path, "password": "" });

    let response = client
        .post(&url)
        .header(AUTHORIZATION, auth)
        .json(&request)
        .send()
        .map_err(|error| format!("POST {url} failed: {error}"))?;
    let status = response.status();
    let text = response
        .text()
        .map_err(|error| format!("cannot read Alist response: {error}"))?;
    let parsed: AlistResponse = serde_json::from_str(&text).map_err(|error| {
        format!(
            "Alist /api/fs/get returned invalid JSON (HTTP {}): {error}: {}",
            status.as_u16(),
            clip(&text, 300)
        )
    })?;

    if parsed.code == 200 && status.is_success() {
        let file = match parsed.data {
            Some(value) if !value.is_null() => serde_json::from_value::<RemoteFile>(value)
                .unwrap_or_else(|_| RemoteFile::default()),
            _ => RemoteFile::default(),
        };
        return Ok(Some(file));
    }
    if status.as_u16() == 404 || looks_not_found(&parsed) {
        return Ok(None);
    }
    Err(format!(
        "Alist /api/fs/get failed (HTTP {}, code {}): {}",
        status.as_u16(),
        parsed.code,
        parsed.message.unwrap_or_else(|| clip(&text, 300))
    ))
}

/// 批量模式入口：上传到**指定的完整远程路径**（可含子目录 / 冲突重命名）。
///
/// `sent` 累加已发送字节数，供整体进度使用；传入 `None` 则不带计数。
/// [`upload_to_alist`] 的签名与行为保持不变。
pub fn upload_file_to_path(
    client: &Client,
    alist_url: &str,
    token: &str,
    local_path: &Path,
    remote_path: &str,
    sent: Option<Arc<AtomicU64>>,
) -> Result<String, String> {
    let metadata = fs::metadata(local_path)
        .map_err(|error| format!("cannot access {}: {error}", local_path.display()))?;
    if !metadata.is_file() {
        return Err(format!("not a regular file: {}", local_path.display()));
    }
    let file = File::open(local_path)
        .map_err(|error| format!("cannot open {}: {error}", local_path.display()))?;
    let size = metadata.len();
    let body = match sent {
        Some(counter) => Body::sized(
            ProgressReader {
                inner: file,
                sent: counter,
            },
            size,
        ),
        None => Body::from(file),
    };
    put(client, alist_url, token, remote_path.to_string(), body, size)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_remote_dirs() {
        assert_eq!(normalize_remote_dir(""), "/");
        assert_eq!(normalize_remote_dir("/"), "/");
        assert_eq!(normalize_remote_dir("backup"), "/backup");
        assert_eq!(normalize_remote_dir("/backup/"), "/backup");
        assert_eq!(normalize_remote_dir("a//b"), "/a/b");
        assert_eq!(normalize_remote_dir(r"\backup\sub"), "/backup/sub");
        assert_eq!(normalize_remote_dir("/a/./b"), "/a/b");
    }

    #[test]
    fn builds_remote_paths() {
        assert_eq!(
            build_remote_path("/backup", "notes.txt").unwrap(),
            "/backup/notes.txt"
        );
        assert_eq!(
            build_remote_path("backup/", "报告.txt").unwrap(),
            "/backup/报告.txt"
        );
        assert_eq!(
            build_remote_path("/", "a.txt").unwrap(),
            "/a.txt"
        );
    }

    #[test]
    fn rejects_unsafe_remote_paths() {
        assert!(build_remote_path("/../etc", "a.txt").is_err());
        assert!(build_remote_path("/backup", "").is_err());
        assert!(build_remote_path("/backup", "a/b.txt").is_err());
    }

    #[test]
    fn encodes_non_ascii_and_spaces_without_plus() {
        let encoded = encode_file_path("/备份 文档/报告 1.txt");
        assert!(encoded.starts_with('/'), "{encoded}");
        assert_eq!(encoded.split('/').count(), 3, "path separators must survive");
        assert!(encoded.is_ascii(), "{encoded}");
        assert!(encoded.contains("%20"), "spaces must be %20: {encoded}");
        assert!(!encoded.contains('+'), "Go PathUnescape keeps '+': {encoded}");
        assert!(!encoded.contains("备份"), "{encoded}");
        assert_eq!(encode_file_path("/plain/file.txt"), "/plain/file.txt");
    }

    #[test]
    fn parses_alist_response() {
        let parsed: AlistResponse =
            serde_json::from_str(r#"{"code":200,"message":"success","data":null}"#).unwrap();
        assert_eq!(parsed.code, 200);
        assert_eq!(parsed.message.as_deref(), Some("success"));
        let parsed: AlistResponse = serde_json::from_str(r#"{"code":401,"message":"bad token"}"#).unwrap();
        assert_eq!(parsed.code, 401);
        assert!(parsed.data.is_none());
    }

    #[test]
    fn clips_long_bodies() {
        assert_eq!(clip("  ok  ", 300), "ok");
        assert!(clip(&"x".repeat(500), 300).ends_with("..."));
        assert_eq!(clip(&"x".repeat(500), 300).chars().count(), 303);
    }

    #[test]
    fn joins_relative_paths_preserving_directories() {
        assert_eq!(
            join_remote_path("/backup", Path::new("src/lib/a.rs")).unwrap(),
            "/backup/src/lib/a.rs"
        );
        assert_eq!(
            join_remote_path("backup/", Path::new("报告 1.txt")).unwrap(),
            "/backup/报告 1.txt"
        );
        assert_eq!(join_remote_path("/", Path::new("a.txt")).unwrap(), "/a.txt");
    }

    #[test]
    fn join_rejects_traversal_and_empty_paths() {
        assert!(join_remote_path("/backup", Path::new("../secret")).is_err());
        assert!(join_remote_path("/../etc", Path::new("a.txt")).is_err());
        assert!(join_remote_path("/backup", Path::new("")).is_err());
    }

    #[test]
    fn detects_not_found_responses() {
        let response: AlistResponse =
            serde_json::from_str(r#"{"code":500,"message":"failed get: object not found"}"#)
                .unwrap();
        assert!(looks_not_found(&response));
        let denied: AlistResponse =
            serde_json::from_str(r#"{"code":401,"message":"token invalid"}"#).unwrap();
        assert!(!looks_not_found(&denied));
    }

    #[test]
    fn parses_remote_file_with_missing_fields() {
        let full: RemoteFile =
            serde_json::from_str(r#"{"size":42,"modified":"2026-11-08T10:00:00Z"}"#).unwrap();
        assert_eq!(full.size, Some(42));
        assert!(full.modified.is_some());
        let sparse: RemoteFile = serde_json::from_str(r#"{"name":"a.txt"}"#).unwrap();
        assert!(sparse.size.is_none());
    }
}
