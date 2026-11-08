//! 用本地 TCP server 模拟 Alist `/api/fs/get` 与 `/api/fs/put`，验证真实 HTTP
//! 请求形状与批量 / 冲突策略。不依赖外网，也不触碰任何真实网盘。

use aether::alist::{encode_file_path, upload_to_alist};
use aether::backup::{plan, plan_total};
use std::collections::HashMap;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::path::PathBuf;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use tempfile::TempDir;

const OK_JSON: &str = r#"{"code":200,"message":"success","data":null}"#;
const NOT_FOUND_JSON: &str =
    r#"{"code":500,"message":"failed get: object not found","data":null}"#;

#[derive(Debug)]
struct Captured {
    method: String,
    target: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

/// 处理 `requests` 个连接后退出；超时返回已捕获的请求，避免测试挂死。
#[derive(Default)]
struct MockConfig {
    requests: usize,
    /// `/api/fs/get` 视为已存在的可读远程路径 -> 云端 size
    cloud: Vec<(String, u64)>,
    /// 这些编码后的 File-Path 在 PUT 时返回 403
    put_fail: Vec<String>,
    /// 覆盖所有 PUT 响应（status line, body）
    put_response: Option<(&'static str, &'static str)>,
    /// 覆盖所有 GET 响应，模拟鉴权 / 服务端故障
    get_failure: Option<(&'static str, &'static str)>,
    /// 按 PUT 的全局序号注入失败，用于「第一次中断、第二次重跑」场景
    fail_put_indices: Vec<usize>,
}

impl MockConfig {
    fn response_for(&self, request: &Captured, put_index: Option<usize>) -> (&'static str, String) {
        if request.target == "/api/fs/get" {
            if let Some((status, body)) = self.get_failure {
                return (status, body.to_string());
            }
            let payload: serde_json::Value =
                serde_json::from_slice(&request.body).unwrap_or_default();
            let path = payload["path"].as_str().unwrap_or("");
            if let Some((_, size)) = self.cloud.iter().find(|(remote, _)| remote == path) {
                return (
                    "200 OK",
                    format!(
                        r#"{{"code":200,"message":"success","data":{{"name":"x","size":{size},"modified":"2026-11-08T10:00:00Z","is_dir":false}}}}"#
                    ),
                );
            }
            return ("200 OK", NOT_FOUND_JSON.to_string());
        }
        if request.target == "/api/fs/put" {
            if let Some((status, body)) = self.put_response {
                return (status, body.to_string());
            }
            let file_path = request
                .headers
                .get("file-path")
                .map(String::as_str)
                .unwrap_or("");
            let denied = self.put_fail.iter().any(|blocked| blocked == file_path)
                || put_index
                    .is_some_and(|index| self.fail_put_indices.contains(&index));
            if denied {
                return (
                    "403 Forbidden",
                    r#"{"code":403,"message":"permission denied","data":null}"#.to_string(),
                );
            }
            return ("200 OK", OK_JSON.to_string());
        }
        (
            "404 Not Found",
            r#"{"code":404,"message":"unexpected endpoint"}"#.to_string(),
        )
    }
}

struct MockAlist {
    url: String,
    handle: JoinHandle<Vec<Captured>>,
}

impl MockAlist {
    fn spawn(config: MockConfig) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
        let handle = thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(20);
            let mut captured = Vec::new();
            let mut put_index = 0usize;
            while captured.len() < config.requests {
                match listener.accept() {
                    Ok((stream, _)) => captured.push(handle_one(stream, &config, &mut put_index)),
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        if Instant::now() >= deadline {
                            break;
                        }
                        thread::sleep(Duration::from_millis(20));
                    }
                    Err(error) => panic!("mock accept failed: {error}"),
                }
            }
            captured
        });
        Self { url, handle }
    }

    fn finish(self) -> Vec<Captured> {
        self.handle.join().unwrap()
    }
}

fn handle_one(stream: TcpStream, config: &MockConfig, put_index: &mut usize) -> Captured {
    // Windows 上 accept 会继承监听 socket 的非阻塞属性，读请求头可能立刻 WouldBlock。
    stream.set_nonblocking(false).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    let mut reader = BufReader::new(stream);

    let mut line = String::new();
    reader.read_line(&mut line).unwrap();
    let parts: Vec<&str> = line.trim_end().split_whitespace().collect();
    let method = parts.first().unwrap_or(&"").to_string();
    let target = parts.get(1).unwrap_or(&"").to_string();

    let mut headers = HashMap::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            break;
        }
        if line == "\r\n" || line == "\n" {
            break;
        }
        if let Some((key, value)) = line.split_once(':') {
            headers.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }
    let length: usize = headers
        .get("content-length")
        .and_then(|value| value.parse().ok())
        .unwrap_or(0);
    let mut body = vec![0u8; length];
    if length > 0 {
        reader.read_exact(&mut body).unwrap();
    }

    let request = Captured {
        method,
        target,
        headers,
        body,
    };
    let this_put = if request.target == "/api/fs/put" {
        let index = *put_index;
        *put_index += 1;
        Some(index)
    } else {
        None
    };
    let (status, response) = config.response_for(&request, this_put);
    let reply = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{response}",
        response.len()
    );
    let stream = reader.get_mut();
    stream.write_all(reply.as_bytes()).unwrap();
    stream.flush().unwrap();
    let _ = stream.shutdown(Shutdown::Both);
    request
}

fn fixture_file(dir: &TempDir, name: &str, contents: &[u8]) -> PathBuf {
    let path = dir.path().join(name);
    fs::write(&path, contents).unwrap();
    path
}

fn cli(config: MockConfig, args: &[&str], extra: &[&str]) -> (std::process::Output, Vec<Captured>) {
    let server = MockAlist::spawn(config);
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_aether"));
    command
        .args([
            "backup",
            "--url",
            server.url.as_str(),
            "--token",
            "cli-token",
        ])
        .args(args)
        .args(extra);
    let output = command.output().unwrap();
    (output, server.finish())
}

/// 同一个 mock server 上连续跑多次 CLI（URL 不变 = 同一个断点任务 key）。
/// 通过 `AETHER_HOME` 把清单隔离到临时目录，绝不污染用户目录。
fn run_cli_on(
    server: &MockAlist,
    args: &[&str],
    extra: &[&str],
    home: &std::path::Path,
) -> std::process::Output {
    let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_aether"));
    command
        .args([
            "backup",
            "--url",
            server.url.as_str(),
            "--token",
            "cli-token",
        ])
        .args(args)
        .args(extra)
        .env("AETHER_HOME", home);
    command.output().unwrap()
}

fn stdout(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &std::process::Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn puts(captured: &[Captured]) -> Vec<&Captured> {
    captured
        .iter()
        .filter(|request| request.target == "/api/fs/put")
        .collect()
}

// ---------------------------------------------------------------- 原有单文件链路

#[test]
fn upload_sends_put_with_encoded_headers_and_raw_file_body() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "报告 1.txt", b"aether-alist-payload");
    let server = MockAlist::spawn(MockConfig {
        requests: 1,
        ..Default::default()
    });

    let result = upload_to_alist(&server.url, "test-token", &file, "/备份 文档");
    let captured = server.finish();

    assert!(result.is_ok(), "unexpected error: {result:?}");
    assert_eq!(result.unwrap(), "/备份 文档/报告 1.txt");
    let request = &captured[0];
    assert_eq!(request.method, "PUT");
    assert_eq!(request.target, "/api/fs/put");
    assert_eq!(
        request.headers.get("authorization").map(String::as_str),
        Some("test-token")
    );

    let file_path = request.headers.get("file-path").unwrap();
    assert_eq!(file_path, &encode_file_path("/备份 文档/报告 1.txt"));
    assert!(file_path.is_ascii(), "{file_path}");
    assert!(file_path.contains("%20"), "spaces: {file_path}");
    assert!(!file_path.contains('+'), "Go PathUnescape keeps '+': {file_path}");
    assert_eq!(file_path.matches('/').count(), 2, "path separators: {file_path}");
    assert_eq!(
        request.headers.get("content-length").map(String::as_str),
        Some("20")
    );
    assert_eq!(request.body, b"aether-alist-payload");
}

#[test]
fn non_200_code_is_an_error_with_server_message() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x");
    let server = MockAlist::spawn(MockConfig {
        requests: 1,
        put_response: Some((
            "401 Unauthorized",
            r#"{"code":401,"message":"token invalid","data":null}"#,
        )),
        ..Default::default()
    });

    let error = upload_to_alist(&server.url, "bad-token", &file, "/backup").unwrap_err();
    let _ = server.finish();

    assert!(error.contains("401"), "{error}");
    assert!(error.contains("token invalid"), "{error}");
}

#[test]
fn invalid_json_response_is_an_error() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x");
    let server = MockAlist::spawn(MockConfig {
        requests: 1,
        put_response: Some(("200 OK", "not json at all")),
        ..Default::default()
    });

    let error = upload_to_alist(&server.url, "token", &file, "/backup").unwrap_err();
    let _ = server.finish();

    assert!(error.contains("invalid JSON"), "{error}");
}

#[test]
fn rejects_missing_files_and_traversal_before_sending() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x");

    let missing = dir.path().join("missing.txt");
    let error = upload_to_alist("http://127.0.0.1:1", "token", &missing, "/backup").unwrap_err();
    assert!(error.contains("cannot access"), "{error}");

    let error = upload_to_alist("http://127.0.0.1:1", "token", &file, "/../etc").unwrap_err();
    assert!(error.contains("'..'"), "{error}");
}

#[test]
fn backup_command_uploads_and_prints_success() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "backup.txt", b"cli-payload");
    let (output, captured) = cli(
        MockConfig {
            requests: 2, // POST /api/fs/get (not found) + PUT
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &[],
    );

    let out = stdout(&output);
    assert!(
        output.status.success(),
        "backup failed: stdout={out} stderr={}",
        stderr(&output)
    );
    assert!(out.contains("OK: uploaded to /backup/backup.txt"), "{out}");
    assert!(out.contains("uploading"), "{out}");
    assert_eq!(captured.len(), 2);
    assert_eq!(captured[0].target, "/api/fs/get");
    let put = &captured[1];
    assert_eq!(put.method, "PUT");
    assert_eq!(put.body, b"cli-payload");
    assert_eq!(
        put.headers.get("file-path").map(String::as_str),
        Some("/backup/backup.txt")
    );
}

#[test]
fn backup_rejects_msys_rewritten_remote_paths_before_sending() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aether"))
        .args(["backup", "--local"])
        .arg(&file)
        .args([
            "--remote",
            "/D:/msys-converted/backup",
            "--url",
            "http://127.0.0.1:1",
            "--token",
            "t",
        ])
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(stderr(&output).contains("MSYS_NO_PATHCONV"));
}

#[test]
fn backup_command_fails_with_nonzero_exit_on_alist_error() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "backup.txt", b"cli-payload");
    let (output, captured) = cli(
        MockConfig {
            requests: 2,
            put_response: Some((
                "403 Forbidden",
                r#"{"code":403,"message":"permission denied","data":null}"#,
            )),
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &[],
    );

    assert!(!output.status.success());
    assert!(stderr(&output).contains("permission denied"));
    assert_eq!(puts(&captured).len(), 1);
}

// ---------------------------------------------------------------- 冲突策略

#[test]
fn skip_policy_does_not_upload_when_cloud_differs() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x"); // local size 1
    let (output, captured) = cli(
        MockConfig {
            requests: 1, // only the probe
            cloud: vec![("/backup/a.txt".to_string(), 999)],
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &[],
    );

    let out = stdout(&output);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(out.contains("skipped"), "{out}");
    assert!(out.contains("conflict"), "{out}");
    assert!(out.contains("0 uploaded, 1 skipped, 0 failed"), "{out}");
    assert!(puts(&captured).is_empty(), "skip must never overwrite");
}

#[test]
fn same_size_on_cloud_is_skipped_without_upload() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"abc"); // size 3
    let (output, captured) = cli(
        MockConfig {
            requests: 1,
            cloud: vec![("/backup/a.txt".to_string(), 3)],
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &[],
    );

    assert!(stdout(&output).contains("same size"));
    assert!(puts(&captured).is_empty());
}

#[test]
fn overwrite_policy_uploads_when_cloud_differs() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x");
    let (output, captured) = cli(
        MockConfig {
            requests: 2,
            cloud: vec![("/backup/a.txt".to_string(), 999)],
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &["--on-conflict", "overwrite"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("overwritten by policy"));
    assert_eq!(puts(&captured).len(), 1);
}

#[test]
fn rename_policy_keeps_both_copies() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x");
    let (output, captured) = cli(
        MockConfig {
            requests: 3, // probe original + probe renamed + PUT
            cloud: vec![("/backup/a.txt".to_string(), 999)],
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &["--on-conflict", "rename"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    assert!(stdout(&output).contains("renamed"));
    let put = puts(&captured);
    assert_eq!(put.len(), 1);
    let target = put[0].headers.get("file-path").unwrap();
    assert!(target.contains("-conflict-"), "{target}");
    assert!(target.ends_with(".txt"), "{target}");
}

#[test]
fn conflict_probe_failure_never_uploads() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"x");
    let (output, captured) = cli(
        MockConfig {
            requests: 1,
            get_failure: Some((
                "401 Unauthorized",
                r#"{"code":401,"message":"token invalid","data":null}"#,
            )),
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &[],
    );

    assert!(!output.status.success());
    assert!(stderr(&output).contains("token invalid"));
    assert!(puts(&captured).is_empty(), "probe errors must not be treated as 'not found'");
}

// ---------------------------------------------------------------- 目录批量

fn project_tree(dir: &TempDir) -> PathBuf {
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
    root
}

#[test]
fn batch_uploads_tree_preserving_layout_and_excluding_build_dirs() {
    let dir = TempDir::new().unwrap();
    let root = project_tree(&dir);
    let planned = plan(&root, "/backup").unwrap();
    assert_eq!(planned.len(), 2);
    assert_eq!(plan_total(&planned), 2);

    let (output, captured) = cli(
        MockConfig {
            requests: 4, // 2 files x (probe + put)
            ..Default::default()
        },
        &["--local", root.to_str().unwrap(), "--remote", "/backup"],
        &[],
    );

    let out = stdout(&output);
    assert!(output.status.success(), "{}", stderr(&output));
    assert!(out.contains("2 uploaded, 0 skipped, 0 failed"), "{out}");
    let mut targets: Vec<&str> = puts(&captured)
        .iter()
        .map(|request| request.headers.get("file-path").unwrap().as_str())
        .collect();
    targets.sort();
    assert_eq!(targets, ["/backup/README.md", "/backup/src/main.rs"]);
}

#[test]
fn batch_continues_after_a_failed_file() {
    let dir = TempDir::new().unwrap();
    let root = dir.path().join("batch");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();
    fs::write(root.join("b.txt"), b"b").unwrap();

    let (output, captured) = cli(
        MockConfig {
            requests: 4,
            put_fail: vec!["/backup/a.txt".to_string()],
            ..Default::default()
        },
        &["--local", root.to_str().unwrap(), "--remote", "/backup"],
        &[],
    );

    let (out, err) = (stdout(&output), stderr(&output));
    assert!(!output.status.success());
    assert!(out.contains("1 uploaded, 0 skipped, 1 failed"), "{out}");
    assert!(err.contains("permission denied"), "{err}");
    assert_eq!(puts(&captured).len(), 2, "b.txt must still be attempted");
}

// ---------------------------------------------------------------- 任务级断点续传

fn two_file_tree(dir: &TempDir) -> PathBuf {
    let root = dir.path().join("batch");
    fs::create_dir_all(&root).unwrap();
    fs::write(root.join("a.txt"), b"a").unwrap();
    fs::write(root.join("b.txt"), b"b").unwrap();
    root
}

fn resume_args<'a>(root: &'a PathBuf) -> [&'a str; 4] {
    ["--local", root.to_str().unwrap(), "--remote", "/backup"]
}

#[test]
fn resume_skips_completed_files_after_interrupted_batch() {
    let dir = TempDir::new().unwrap();
    let root = two_file_tree(&dir);
    let home = dir.path().join("home");
    let server = MockAlist::spawn(MockConfig {
        requests: 6, // 首次 4 次（含 1 次失败），续跑 2 次
        fail_put_indices: vec![1], // 首次运行的 b.txt 上传失败
        ..Default::default()
    });
    let args = resume_args(&root);

    let first = run_cli_on(&server, &args, &["--resume"], &home);
    let out1 = stdout(&first);
    assert!(!first.status.success(), "first run must report the failure");
    assert!(out1.contains("1 uploaded, 0 skipped, 1 failed"), "{out1}");
    assert!(stderr(&first).contains("permission denied"));

    let second = run_cli_on(&server, &args, &["--resume"], &home);
    let out2 = stdout(&second);
    assert!(second.status.success(), "stderr={}", stderr(&second));
    assert!(out2.contains("1 uploaded, 1 skipped, 0 failed"), "{out2}");
    assert!(out2.contains("resumed: completed in a previous run"), "{out2}");
    assert!(out2.contains("restored from checkpoint"), "{out2}");

    let captured = server.finish();
    assert_eq!(captured.len(), 6, "a.txt must not be probed again");
    assert_eq!(puts(&captured).len(), 3);
    assert_eq!(captured[4].target, "/api/fs/get");
    assert_eq!(captured[5].target, "/api/fs/put");
    assert_eq!(
        captured[5].headers.get("file-path").map(String::as_str),
        Some("/backup/b.txt")
    );
}

#[test]
fn resume_invalidates_when_local_file_changed() {
    let dir = TempDir::new().unwrap();
    let root = two_file_tree(&dir);
    let home = dir.path().join("home");
    let server = MockAlist::spawn(MockConfig {
        requests: 8,
        fail_put_indices: vec![1],
        ..Default::default()
    });
    let args = resume_args(&root);

    let first = run_cli_on(&server, &args, &["--resume"], &home);
    assert!(!first.status.success());
    fs::write(root.join("a.txt"), b"a-changed-significantly").unwrap();

    let second = run_cli_on(&server, &args, &["--resume"], &home);
    let out2 = stdout(&second);
    assert!(second.status.success(), "stderr={}", stderr(&second));
    assert!(out2.contains("2 uploaded, 0 skipped, 0 failed"), "{out2}");
    assert!(
        !out2.contains("restored from checkpoint"),
        "stale fingerprint must not skip: {out2}"
    );
    assert_eq!(puts(&server.finish()).len(), 4);
}

#[test]
fn without_resume_flag_behaviour_is_unchanged() {
    let dir = TempDir::new().unwrap();
    let root = two_file_tree(&dir);
    let home = dir.path().join("home");
    let server = MockAlist::spawn(MockConfig {
        requests: 8,
        fail_put_indices: vec![1],
        ..Default::default()
    });
    let args = resume_args(&root);

    let first = run_cli_on(&server, &args, &["--resume"], &home);
    assert!(!first.status.success());
    let second = run_cli_on(&server, &args, &[], &home);
    let out2 = stdout(&second);
    assert!(second.status.success(), "stderr={}", stderr(&second));
    assert!(out2.contains("2 uploaded, 0 skipped, 0 failed"), "{out2}");
    assert!(!out2.contains("restored from checkpoint"), "{out2}");
    assert_eq!(server.finish().len(), 8);
}

#[test]
fn successful_resume_run_removes_its_checkpoint() {
    let dir = TempDir::new().unwrap();
    let root = two_file_tree(&dir);
    let home = dir.path().join("home");
    let server = MockAlist::spawn(MockConfig {
        requests: 4,
        ..Default::default()
    });
    let args = resume_args(&root);

    let output = run_cli_on(&server, &args, &["--resume"], &home);
    assert!(output.status.success(), "{}", stderr(&output));
    let backups = home.join("backups");
    let leftovers: Vec<_> = fs::read_dir(&backups)
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(leftovers.is_empty(), "checkpoint should self-clean: {leftovers:?}");
    let _ = server.finish();
}

#[test]
fn reset_resume_forces_a_full_run() {
    let dir = TempDir::new().unwrap();
    let root = two_file_tree(&dir);
    let home = dir.path().join("home");
    let server = MockAlist::spawn(MockConfig {
        requests: 8,
        fail_put_indices: vec![1],
        ..Default::default()
    });
    let args = resume_args(&root);

    let first = run_cli_on(&server, &args, &["--resume"], &home);
    assert!(!first.status.success());
    let second = run_cli_on(&server, &args, &["--resume", "--reset-resume"], &home);
    let out2 = stdout(&second);
    assert!(second.status.success(), "stderr={}", stderr(&second));
    assert!(out2.contains("2 uploaded, 0 skipped, 0 failed"), "{out2}");
    assert!(out2.contains("(0 completed, reset)"), "{out2}");
    assert!(!out2.contains("restored from checkpoint"), "{out2}");
    assert_eq!(server.finish().len(), 8);
}

#[test]
fn json_report_exposes_resumed_count_and_reason() {
    let dir = TempDir::new().unwrap();
    let root = two_file_tree(&dir);
    let home = dir.path().join("home");
    let server = MockAlist::spawn(MockConfig {
        requests: 6,
        fail_put_indices: vec![1],
        ..Default::default()
    });
    let args = resume_args(&root);

    let first = run_cli_on(&server, &args, &["--resume"], &home);
    assert!(!first.status.success());
    let second = run_cli_on(&server, &args, &["--resume", "--json"], &home);
    assert!(second.status.success(), "{}", stderr(&second));
    let report: serde_json::Value = serde_json::from_slice(&second.stdout).unwrap();
    assert_eq!(report["resumed"], 1);
    let items = report["items"].as_array().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0]["outcome"], "skipped");
    assert!(items[0]["detail"].as_str().unwrap().contains("resumed"), "{items:?}");
    let _ = server.finish();
}

#[test]
fn refuses_a_checkpoint_inside_the_backed_up_tree() {
    let dir = TempDir::new().unwrap();
    let root = two_file_tree(&dir);
    let inside = root.join(".aether");
    fs::create_dir_all(&inside).unwrap();

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_aether"))
        .args(["backup", "--local"])
        .arg(&root)
        .args([
            "--remote",
            "/backup",
            "--url",
            "http://127.0.0.1:1",
            "--token",
            "t",
            "--resume",
        ])
        .env("AETHER_HOME", &inside)
        .output()
        .unwrap();

    assert!(!output.status.success());
    assert!(stderr(&output).contains("inside the backed-up tree"), "{}", stderr(&output));
}

#[test]
fn json_report_is_machine_readable_for_future_ui() {
    let dir = TempDir::new().unwrap();
    let file = fixture_file(&dir, "a.txt", b"abc");
    let (output, _) = cli(
        MockConfig {
            requests: 2,
            ..Default::default()
        },
        &["--local", file.to_str().unwrap(), "--remote", "/backup"],
        &["--json"],
    );

    assert!(output.status.success(), "{}", stderr(&output));
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let items = report["items"].as_array().unwrap();
    assert_eq!(items.len(), 1);
    assert_eq!(items[0]["outcome"], "uploaded");
    assert_eq!(items[0]["remote"], "/backup/a.txt");
}
