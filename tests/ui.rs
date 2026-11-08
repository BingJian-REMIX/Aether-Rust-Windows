//! `aether ui` 的真实 TCP 测试：不 mock HTTP，起服务后直接发请求。

use aether::state::{now, states, State, StateStore};
use aether::ui::UiServer;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;
use tempfile::TempDir;

struct Response {
    status: u16,
    head: String,
    body: String,
}

fn request(server: &UiServer, method: &str, path: &str) -> Response {
    let addr = server.addr();
    let mut stream = TcpStream::connect(addr).expect("connect to ui server");
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .expect("read timeout");
    let raw_request =
        format!("{method} {path} HTTP/1.1\r\nHost: {addr}\r\nConnection: close\r\n\r\n");
    stream
        .write_all(raw_request.as_bytes())
        .expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .map(|(head, body)| (head.to_string(), body.to_string()))
        .unwrap_or((text, String::new()));
    let status = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse().ok())
        .unwrap_or(0);
    Response { status, head, body }
}

fn serve(home: &TempDir) -> UiServer {
    UiServer::start(home.path(), 0).expect("start ui server")
}

fn write_state(home: &TempDir, mutate: impl FnOnce(&mut State)) -> State {
    let store = StateStore::new(home.path());
    let mut state = State::default();
    state.pid = std::process::id();
    state.process_name = "aether".into();
    state.repository = "D:/repo".into();
    state.strategy = "delay".into();
    state.delay = "10m".into();
    state.heartbeat_at = now();
    mutate(&mut state);
    store.save(&state).expect("save state");
    state
}

#[test]
fn serves_dashboard_html() {
    let home = TempDir::new().unwrap();
    let server = serve(&home);
    let response = request(&server, "GET", "/");
    server.stop();

    assert_eq!(response.status, 200);
    assert!(response.head.contains("text/html"), "{}", response.head);
    assert!(response.body.contains("Aether"));
    assert!(response.body.contains("/api/state"));
    // 页面必须离线可用：不引外部字体 / 脚本。
    assert!(!response.body.contains("src=\"http"));
}

#[test]
fn api_state_without_state_file_is_not_running() {
    let home = TempDir::new().unwrap();
    let server = serve(&home);
    let response = request(&server, "GET", "/api/state");
    server.stop();

    assert_eq!(response.status, 200);
    let value: serde_json::Value = serde_json::from_str(&response.body).expect("json");
    assert_eq!(value["running"], serde_json::json!(false));
    assert_eq!(value["state"], serde_json::Value::Null);
    assert_eq!(value["remainingSeconds"], serde_json::json!(0));
}

#[test]
fn api_state_reports_live_countdown() {
    let home = TempDir::new().unwrap();
    write_state(&home, |state| {
        state.state = states::COUNTDOWN.into();
        state.next_sync_at = now() + 600;
        state.pending_changes = 3;
        state.events_seen = 7;
    });
    let server = serve(&home);
    let response = request(&server, "GET", "/api/state");
    server.stop();

    assert_eq!(response.status, 200);
    let value: serde_json::Value = serde_json::from_str(&response.body).expect("json");
    assert_eq!(value["running"], serde_json::json!(true));
    assert_eq!(value["delaySeconds"], serde_json::json!(600));
    let remaining = value["remainingSeconds"].as_u64().expect("remaining");
    assert!((590..=600).contains(&remaining), "remaining={remaining}");
    assert_eq!(value["state"]["pendingChanges"], serde_json::json!(3));
    assert_eq!(value["state"]["repository"], serde_json::json!("D:/repo"));
}

#[test]
fn stale_heartbeat_is_reported_as_stopped() {
    let home = TempDir::new().unwrap();
    write_state(&home, |state| {
        state.state = states::WATCHING.into();
        state.heartbeat_at = now() - 120;
    });
    let server = serve(&home);
    let response = request(&server, "GET", "/api/state");
    server.stop();

    let value: serde_json::Value = serde_json::from_str(&response.body).expect("json");
    assert_eq!(value["running"], serde_json::json!(false));
    assert!(value["heartbeatAgeSeconds"].as_u64().unwrap() >= 120);
}

#[test]
fn unknown_path_is_404_and_post_is_405() {
    let home = TempDir::new().unwrap();
    let server = serve(&home);
    let missing = request(&server, "GET", "/nope");
    let rejected = request(&server, "POST", "/api/state");
    let health = request(&server, "GET", "/healthz");
    server.stop();

    assert_eq!(missing.status, 404);
    assert_eq!(rejected.status, 405);
    assert_eq!(health.status, 200);
    assert_eq!(health.body.trim(), "ok");
}

#[test]
fn serves_concurrent_requests() {
    let home = TempDir::new().unwrap();
    write_state(&home, |state| {
        state.state = states::WATCHING.into();
    });
    let server = serve(&home);
    let addr = server.addr();

    let workers: Vec<_> = (0..8)
        .map(|_| {
            std::thread::spawn(move || {
                let mut stream = TcpStream::connect(addr).expect("connect");
                stream
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                stream
                    .write_all(b"GET /api/state HTTP/1.1\r\nHost: local\r\n\r\n")
                    .unwrap();
                let mut raw = String::new();
                stream.read_to_string(&mut raw).expect("read");
                raw.starts_with("HTTP/1.1 200")
            })
        })
        .collect();

    let results: Vec<bool> = workers.into_iter().map(|w| w.join().unwrap()).collect();
    server.stop();
    assert!(results.iter().all(|ok| *ok), "{results:?}");
}
