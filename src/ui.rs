//! 本地状态面板（TODO #4 的零依赖形态）。
//!
//! 不引入托盘 / GUI 框架：一个进程内 HTTP 服务 + 单页看板，把
//! `~/.aether/state.json` 里已有的字段（策略、倒计时、待提交、上次同步结果）
//! 可视化出来。选择 HTTP 而不是原生窗口的理由：
//!
//! 1. 面板与 `aether start` 解耦，引擎被强杀后看板仍能显示「未运行」；
//! 2. 不新增任何 crate，`std::net` 足够，windows-gnu 上不需要额外 C 工具链；
//! 3. 页面本身可被浏览器、手机（局域网另说）或后续 tray 复用同一份 JSON。
//!
//! 只监听 `127.0.0.1`：面板没有鉴权，暴露到 0.0.0.0 等于把仓库路径与提交信息
//! 公开到局域网。

use crate::backup::BackupSummary;
use crate::options::parse_duration;
use crate::state::{is_running, now, State, StateStore};
use serde::Serialize;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

/// 默认端口取 7878，避开常见的 8000/8080 争论。
pub const DEFAULT_PORT: u16 = 7878;

/// 单次请求最多读 8 KiB 头部，防止异常客户端把内存吃满。
const MAX_HEADER_BYTES: usize = 8 * 1024;

/// 面板对外暴露的 JSON 结构。`state` 为 `null` 表示从未启动过。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Snapshot {
    running: bool,
    now: u64,
    /// 距离下次同步的秒数；无倒计时（从未变动 / never 策略）时为 0。
    remaining_seconds: u64,
    /// `delay` 策略的总时长，前端用来画进度环。
    delay_seconds: u64,
    /// 心跳距今秒数，用来判断「运行中但卡住」。
    heartbeat_age_seconds: u64,
    state_path: String,
    state: Option<State>,
    /// 最近一次 `aether backup` 的摘要；从未备份过为 `null`。
    last_backup: Option<BackupSummary>,
}

fn snapshot(store: &StateStore, home: &Path) -> Snapshot {
    let state = store.load();
    let running = state.as_ref().map(is_running).unwrap_or(false);
    let delay_seconds = state
        .as_ref()
        .and_then(|value| parse_duration(&value.delay).ok())
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let remaining_seconds = state
        .as_ref()
        .map(|value| value.next_sync_at.saturating_sub(now()))
        .unwrap_or(0);
    let heartbeat_age_seconds = state
        .as_ref()
        .map(|value| now().saturating_sub(value.heartbeat_at))
        .unwrap_or(0);
    Snapshot {
        running,
        now: now(),
        remaining_seconds,
        delay_seconds,
        heartbeat_age_seconds,
        state_path: store.path().display().to_string().replace('\\', "/"),
        state,
        last_backup: BackupSummary::load(home),
    }
}

/// 一个已绑定端口的看板服务。`stop()` 之后端口释放。
pub struct UiServer {
    addr: SocketAddr,
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<()>>,
}

impl UiServer {
    /// 绑定 `127.0.0.1:port`（`port = 0` 时由系统分配，测试用）。
    pub fn start(home: impl Into<PathBuf>, port: u16) -> Result<Self, String> {
        let home = home.into();
        let listener = TcpListener::bind(("127.0.0.1", port))
            .map_err(|error| format!("cannot bind 127.0.0.1:{port}: {error}"))?;
        listener
            .set_nonblocking(true)
            .map_err(|error| format!("cannot configure listener: {error}"))?;
        let addr = listener
            .local_addr()
            .map_err(|error| format!("cannot read listener address: {error}"))?;

        let stop = Arc::new(AtomicBool::new(false));
        let stop_bg = Arc::clone(&stop);
        let handle = thread::spawn(move || {
            while !stop_bg.load(Ordering::Relaxed) {
                match listener.accept() {
                    Ok((stream, _)) => {
                        // Windows：accept 出来的 socket 会继承监听 socket 的非阻塞属性，
                        // 不显式改回阻塞，读请求头时会随机拿到 WouldBlock 而丢连接。
                        let _ = stream.set_nonblocking(false);
                        let home = home.clone();
                        // 一连接一线程：并发请求互不阻塞，也避免 keep-alive 状态机。
                        thread::spawn(move || handle_connection(stream, &home));
                    }
                    Err(ref error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        thread::sleep(Duration::from_millis(100));
                    }
                    Err(_) => thread::sleep(Duration::from_millis(100)),
                }
            }
        });

        Ok(Self {
            addr,
            stop,
            handle: Some(handle),
        })
    }

    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn url(&self) -> String {
        format!("http://{}", self.addr)
    }

    pub fn stop(mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_connection(mut stream: TcpStream, home: &Path) {
    let Some(request_line) = read_request_line(&mut stream) else {
        return;
    };
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or_default();
    let target = parts.next().unwrap_or("/");
    let path = target.split('?').next().unwrap_or("/");

    if method != "GET" {
        write_response(
            &mut stream,
            "405 Method Not Allowed",
            "text/plain; charset=utf-8",
            format!("method {method} is not allowed\n").as_bytes(),
        );
        return;
    }

    let store = StateStore::new(&home);
    match path {
        "/" | "/index.html" => write_response(
            &mut stream,
            "200 OK",
            "text/html; charset=utf-8",
            INDEX_HTML.as_bytes(),
        ),
        "/api/state" => {
            let body =
                serde_json::to_vec(&snapshot(&store, &home)).unwrap_or_else(|_| b"{}".to_vec());
            write_response(
                &mut stream,
                "200 OK",
                "application/json; charset=utf-8",
                &body,
            );
        }
        "/healthz" => write_response(&mut stream, "200 OK", "text/plain; charset=utf-8", b"ok\n"),
        _ => {
            let body = format!("not found: {path}\n");
            write_response(
                &mut stream,
                "404 Not Found",
                "text/plain; charset=utf-8",
                body.as_bytes(),
            );
        }
    }
}

fn read_request_line(stream: &mut TcpStream) -> Option<String> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::with_capacity(1024);
    let mut chunk = [0u8; 512];
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(read) => {
                buffer.extend_from_slice(&chunk[..read]);
                if buffer.windows(4).any(|window| window == b"\r\n\r\n")
                    || buffer.len() >= MAX_HEADER_BYTES
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    if buffer.is_empty() {
        return None;
    }
    let text = String::from_utf8_lossy(&buffer);
    text.lines().next().map(|line| line.trim().to_string())
}

fn write_response(stream: &mut TcpStream, status: &str, content_type: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nX-Content-Type-Options: nosniff\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body);
    let _ = stream.flush();
}

/// 阻塞式入口：`aether ui` 用它。Ctrl+C 退出。
pub fn serve(home: PathBuf, port: u16, open_browser: bool) -> Result<(), String> {
    let state_path = StateStore::new(&home).path().display().to_string();
    let server = UiServer::start(home, port)?;
    let url = server.url();
    println!("[aether] ui       {url}");
    println!("[aether] state    {state_path}");
    println!("[aether] hint     Ctrl+C to stop");
    if open_browser {
        open_in_browser(&url);
    }

    let stop = Arc::new(AtomicBool::new(false));
    let stop_signal = Arc::clone(&stop);
    if let Err(error) = ctrlc::set_handler(move || stop_signal.store(true, Ordering::Relaxed)) {
        eprintln!("warning: could not install Ctrl+C handler: {error}");
    }
    while !stop.load(Ordering::Relaxed) {
        thread::sleep(Duration::from_millis(120));
    }
    server.stop();
    println!("[aether] ui stopped");
    Ok(())
}

fn open_in_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let (program, args): (&str, Vec<&str>) = ("cmd", vec!["/C", "start", "", url]);
    #[cfg(target_os = "macos")]
    let (program, args): (&str, Vec<&str>) = ("open", vec![url]);
    #[cfg(all(unix, not(target_os = "macos")))]
    let (program, args): (&str, Vec<&str>) = ("xdg-open", vec![url]);

    match std::process::Command::new(program).args(args).spawn() {
        Ok(_) => println!("[aether] opened   default browser"),
        Err(error) => eprintln!("[aether] warning: cannot open browser: {error}"),
    }
}

const INDEX_HTML: &str = r##"<!doctype html>
<html lang="zh-CN">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width,initial-scale=1">
<title>Aether · 同步状态</title>
<style>
  :root{
    --bg:#f6f7f9; --panel:#ffffff; --ink:#15181d; --muted:#6b7280;
    --line:#e5e7eb; --accent:#2563eb; --ok:#16a34a; --warn:#d97706; --bad:#dc2626;
    --shadow:0 1px 2px rgba(16,24,40,.06),0 8px 24px -12px rgba(16,24,40,.18);
  }
  @media (prefers-color-scheme: dark){
    :root{
      --bg:#0b0d10; --panel:#14171c; --ink:#e8eaed; --muted:#9aa3af;
      --line:#232830; --accent:#5b9dff; --ok:#4ade80; --warn:#fbbf24; --bad:#f87171;
      --shadow:0 1px 2px rgba(0,0,0,.4),0 12px 32px -16px rgba(0,0,0,.8);
    }
  }
  *{box-sizing:border-box}
  html,body{height:100%}
  body{
    margin:0; background:var(--bg); color:var(--ink);
    font:14px/1.55 ui-sans-serif,-apple-system,"Segoe UI","PingFang SC","Microsoft YaHei",sans-serif;
    display:flex; align-items:center; justify-content:center; padding:28px;
  }
  .shell{width:min(880px,100%); min-width:0; display:flex; flex-direction:column; gap:18px}
  header{display:flex; align-items:center; justify-content:space-between; gap:16px; flex-wrap:wrap}
  .brand{display:flex; align-items:center; gap:10px; min-width:0}
  .brand h1{font-size:18px; margin:0; letter-spacing:-.01em}
  .dot{width:9px; height:9px; border-radius:50%; background:var(--muted); box-shadow:0 0 0 4px color-mix(in srgb,var(--muted) 18%,transparent)}
  body[data-running="on"] .dot{background:var(--ok); box-shadow:0 0 0 4px color-mix(in srgb,var(--ok) 20%,transparent)}
  .pill{font-size:12px; padding:3px 10px; border-radius:999px; border:1px solid var(--line); color:var(--muted); background:var(--panel)}
  body[data-running="on"] .pill{color:var(--ok); border-color:color-mix(in srgb,var(--ok) 35%,var(--line))}
  /* 仓库路径可能很长：允许收缩并省略号截断，否则 nowrap 的 max-content 会把整页撑宽。 */
  .repo{font-size:12px; color:var(--muted); flex:1 1 auto; min-width:0; max-width:52ch; overflow:hidden; text-overflow:ellipsis; white-space:nowrap}
  .hero{display:grid; grid-template-columns:190px 1fr; gap:18px; align-items:stretch}
  @media (max-width:640px){ .hero{grid-template-columns:1fr} }
  .ring{background:var(--panel); border:1px solid var(--line); border-radius:18px; box-shadow:var(--shadow); display:grid; place-items:center; padding:14px}
  .ring svg{width:158px; height:158px; transform:rotate(-90deg)}
  .ring circle{fill:none; stroke-width:8; stroke-linecap:round}
  .track{stroke:var(--line)}
  .bar{stroke:var(--accent); transition:stroke-dashoffset .28s linear, stroke .3s}
  .ring .label{position:absolute; display:flex; flex-direction:column; align-items:center; gap:2px; text-align:center}
  .ring-wrap{position:relative; display:grid; place-items:center}
  .count{display:block; font-size:26px; font-weight:650; letter-spacing:-.02em; font-variant-numeric:tabular-nums; line-height:1.1}
  /* small 与 .count 是兄弟节点，选择器不能写成 `.count small`（子代），否则整行挤在一起。 */
  .ring .label small{display:block; font-size:11px; font-weight:500; color:var(--muted); letter-spacing:0; line-height:1.2}
  .grid{display:grid; grid-template-columns:repeat(2,minmax(0,1fr)); gap:12px}
  /* 手机宽度下两列会挤成豆腐块，改成单列。必须写在 `.grid` 之后才生效。 */
  @media (max-width:520px){ .grid{grid-template-columns:1fr} }
  .card{background:var(--panel); border:1px solid var(--line); border-radius:14px; padding:12px 14px; box-shadow:var(--shadow); min-width:0}
  .card span{display:block; font-size:11px; color:var(--muted); letter-spacing:.04em; text-transform:uppercase}
  .card strong{display:block; font-size:16px; font-weight:600; margin-top:2px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap}
  .card em{display:block; font-style:normal; font-size:12px; color:var(--muted); margin-top:2px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap}
  .flow{display:flex; gap:8px; flex-wrap:wrap; background:var(--panel); border:1px solid var(--line); border-radius:14px; padding:12px 14px; box-shadow:var(--shadow)}
  .step{display:flex; align-items:center; gap:8px; font-size:12px; color:var(--muted); padding:4px 10px; border-radius:999px; border:1px dashed var(--line)}
  .step.on{color:var(--ink); border-style:solid; border-color:color-mix(in srgb,var(--accent) 45%,var(--line)); background:color-mix(in srgb,var(--accent) 8%,transparent)}
  .step i{width:6px; height:6px; border-radius:50%; background:currentColor; opacity:.5}
  .step.on i{opacity:1; background:var(--accent)}
  /* 备份摘要：一行式，和上面两张面板区分开。 */
  .strip{display:flex; align-items:center; gap:10px 14px; flex-wrap:wrap; background:var(--panel); border:1px solid var(--line); border-radius:14px; padding:11px 14px; box-shadow:var(--shadow)}
  .strip .k{font-size:11px; color:var(--muted); letter-spacing:.04em; text-transform:uppercase}
  .strip strong{font-size:13px; font-weight:600}
  .strip .meta{font-size:11px; color:var(--muted); min-width:0; overflow:hidden; text-overflow:ellipsis; white-space:nowrap}
  .strip .ok{color:var(--ok)} .strip .warn{color:var(--warn)} .strip .bad{color:var(--bad)}
  footer{display:flex; justify-content:space-between; gap:12px; font-size:11px; color:var(--muted); flex-wrap:wrap}
  .mono{font-family:ui-monospace,SFMono-Regular,Menlo,Consolas,monospace; font-size:11px; overflow:hidden; text-overflow:ellipsis; white-space:nowrap; max-width:60ch}
  .offline{color:var(--bad)}
</style>
</head>
<body data-running="off">
<main class="shell">
  <header>
    <div class="brand"><span class="dot"></span><h1>Aether</h1><span class="pill" id="status">连接中</span></div>
    <div class="repo mono" id="repo">-</div>
  </header>

  <section class="hero">
    <div class="ring">
      <div class="ring-wrap">
        <svg viewBox="0 0 120 120" aria-hidden="true">
          <circle class="track" cx="60" cy="60" r="52"></circle>
          <circle class="bar" id="bar" cx="60" cy="60" r="52"
                  stroke-dasharray="326.7" stroke-dashoffset="326.7"></circle>
        </svg>
        <div class="label"><span class="count" id="count">--:--</span><small id="countLabel">倒计时</small></div>
      </div>
    </div>
    <div class="grid">
      <div class="card"><span>策略</span><strong id="strategy">-</strong><em id="excluded"></em></div>
      <div class="card"><span>待提交</span><strong id="pending">-</strong><em id="events"></em></div>
      <div class="card"><span>上次同步</span><strong id="lastResult">-</strong><em id="lastMessage"></em></div>
      <div class="card"><span>心跳</span><strong id="heartbeat">-</strong><em id="pid"></em></div>
    </div>
  </section>

  <section class="flow" id="flow">
    <div class="step" data-step="watching"><i></i>监听变动</div>
    <div class="step" data-step="countdown"><i></i>倒计时</div>
    <div class="step" data-step="syncing"><i></i>提交</div>
    <div class="step" data-step="syncing"><i></i>推送</div>
  </section>

  <section class="strip" id="backupStrip">
    <span class="k">上次备份</span>
    <strong id="bkCounts">尚无记录</strong>
    <span class="meta" id="bkMeta">运行 aether backup 后显示</span>
  </section>

  <footer>
    <span class="mono" id="statePath">-</span>
    <span id="updated">等待数据…</span>
  </footer>
</main>
<script>
const el = (id) => document.getElementById(id);
const CIRC = 326.7;
let deadline = 0, delaySeconds = 0, last = 0;

function pad(n){ return String(n).padStart(2, '0'); }
function fmt(sec){
  sec = Math.max(0, Math.round(sec));
  const h = Math.floor(sec / 3600), m = Math.floor((sec % 3600) / 60), s = sec % 60;
  return h > 0 ? h + ':' + pad(m) + ':' + pad(s) : pad(m) + ':' + pad(s);
}
const LABEL = { starting: '启动中', watching: '监听中', countdown: '倒计时', syncing: '同步中', reminded: '已提醒', stopped: '已停止' };

function ago(ts){
  const sec = Math.max(0, Math.floor(Date.now() / 1000) - ts);
  if (sec < 60) return sec + 's 前';
  if (sec < 3600) return Math.floor(sec / 60) + 'm 前';
  if (sec < 86400) return Math.floor(sec / 3600) + 'h 前';
  return Math.floor(sec / 86400) + 'd 前';
}
function humanSize(bytes){
  const units = ['B', 'KB', 'MB', 'GB'];
  let value = bytes || 0, index = 0;
  while (value >= 1024 && index < units.length - 1) { value /= 1024; index++; }
  return (index === 0 ? value : value.toFixed(1)) + ' ' + units[index];
}
function paintBackup(bk){
  const counts = el('bkCounts');
  if (!bk) {
    counts.textContent = '尚无记录';
    counts.className = '';
    el('bkMeta').textContent = '运行 aether backup 后显示';
    el('bkMeta').title = '';
    return;
  }
  const parts = [bk.uploaded + ' 上传', bk.skipped + ' 跳过'];
  if (bk.failed) parts.push(bk.failed + ' 失败');
  counts.textContent = parts.join(' · ');
  counts.className = bk.failed ? 'bad' : (bk.uploaded ? 'ok' : '');
  const bits = [ago(bk.finishedAt), bk.remote, humanSize(bk.bytes)];
  if (bk.resumed) bits.push(bk.resumed + ' 命中续传');
  if (bk.durationSecs >= 1) bits.push('耗时 ' + bk.durationSecs + 's');
  el('bkMeta').textContent = bits.join(' · ');
  el('bkMeta').title = bk.local + ' → ' + bk.remote;
}

function paintCountdown(){
  const nowSec = Math.floor(Date.now() / 1000);
  if (!deadline) {
    el('count').textContent = '--:--';
    el('countLabel').textContent = '无倒计时';
    el('bar').style.strokeDashoffset = CIRC;
    return;
  }
  const left = Math.max(0, deadline - nowSec);
  el('count').textContent = fmt(left);
  el('countLabel').textContent = left > 0 ? '后自动提交' : '即将同步';
  const total = delaySeconds || Math.max(left, 1);
  const progress = Math.min(1, Math.max(0, 1 - left / total));
  el('bar').style.strokeDashoffset = CIRC * (1 - progress);
}

function render(data){
  const st = data.state || {};  document.body.dataset.running = data.running ? 'on' : 'off';
  el('status').textContent = data.running ? (LABEL[st.state] || st.state || '运行中') : '未运行';
  el('repo').textContent = st.repository || '（尚无状态文件）';
  el('repo').title = st.repository || '';

  const strategy = st.strategy === 'delay' ? 'delay · ' + st.delay : (st.strategy || '-');
  el('strategy').textContent = strategy;
  el('excluded').textContent = st.excluded && st.excluded.length ? '排除 ' + st.excluded.join(' / ') : '';
  el('pending').textContent = (st.pendingChanges || 0) + ' 个文件';
  el('events').textContent = (st.eventsSeen || 0) + ' 次变动';

  const result = (st.lastSyncResult || '').toUpperCase();
  el('lastResult').textContent = result || '-';
  el('lastResult').style.color = result === 'OK' ? 'var(--ok)' : (result === 'FAILED' || result === 'PARTIAL' ? 'var(--warn)' : '');
  el('lastMessage').textContent = st.lastSyncMessage || '';
  el('lastMessage').title = st.lastSyncMessage || '';

  el('heartbeat').textContent = data.running ? data.heartbeatAgeSeconds + 's 前' : '已停止';
  el('pid').textContent = st.pid ? 'PID ' + st.pid : '';

  deadline = st.nextSyncAt || 0;
  delaySeconds = data.delaySeconds || 0;
  paintCountdown();

  const steps = document.querySelectorAll('.step');
  steps.forEach((node) => {
    const key = node.dataset.step;
    node.classList.toggle('on', st.state === key || (key === 'watching' && st.state === 'starting'));
  });

  paintBackup(data.lastBackup);

  el('statePath').textContent = data.statePath || '';
  el('statePath').title = data.statePath || '';
  el('updated').textContent = '更新于 ' + new Date().toLocaleTimeString();
  el('updated').classList.remove('offline');
}

async function poll(){
  try {
    const response = await fetch('/api/state', { cache: 'no-store' });
    if (!response.ok) throw new Error('HTTP ' + response.status);
    last = Date.now();
    render(await response.json());
  } catch (error) {
    el('updated').textContent = '面板已断开（' + error.message + '）';
    el('updated').classList.add('offline');
    document.body.dataset.running = 'off';
  }
}

setInterval(paintCountdown, 250);
setInterval(poll, 1000);
poll();
</script>
</body>
</html>
"##;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{now, states};
    use tempfile::TempDir;

    fn snapshot_for(state: Option<State>) -> Snapshot {
        let dir = TempDir::new().unwrap();
        let store = StateStore::new(dir.path());
        if let Some(value) = state {
            store.save(&value).unwrap();
        }
        snapshot(&store, dir.path())
    }

    #[test]
    fn missing_state_file_is_not_running() {
        let value = snapshot_for(None);
        assert!(!value.running);
        assert_eq!(value.remaining_seconds, 0);
        assert!(value.state.is_none());
        assert!(value.last_backup.is_none());
    }

    #[test]
    fn backup_summary_is_exposed_when_present() {
        let dir = TempDir::new().unwrap();
        let summary = BackupSummary {
            finished_at: now(),
            local: "C:/src".into(),
            remote: "/backup".into(),
            uploaded: 3,
            skipped: 1,
            failed: 0,
            resumed: 2,
            bytes: 2048,
            duration_secs: 7,
        };
        summary.save(dir.path()).unwrap();

        let value = snapshot(&StateStore::new(dir.path()), dir.path());
        let exposed = value.last_backup.expect("summary should be loaded");
        assert_eq!(exposed.uploaded, 3);
        assert_eq!(exposed.remote, "/backup");
        assert_eq!(exposed.duration_secs, 7);
    }

    #[test]
    fn corrupt_backup_summary_is_ignored_instead_of_failing() {
        let dir = TempDir::new().unwrap();
        std::fs::write(BackupSummary::path(dir.path()), b"{not json").unwrap();
        let value = snapshot(&StateStore::new(dir.path()), dir.path());
        assert!(value.last_backup.is_none());
    }

    #[test]
    fn countdown_reports_remaining_and_delay() {
        let mut state = State::default();
        state.strategy = "delay".into();
        state.delay = "10m".into();
        state.state = states::COUNTDOWN.into();
        state.heartbeat_at = now();
        state.next_sync_at = now() + 600;

        let value = snapshot_for(Some(state));
        assert!(value.running);
        assert_eq!(value.delay_seconds, 600);
        assert!(value.remaining_seconds > 590 && value.remaining_seconds <= 600);
    }

    #[test]
    fn due_countdown_never_underflows() {
        let mut state = State::default();
        state.state = states::COUNTDOWN.into();
        state.heartbeat_at = now();
        state.next_sync_at = now() - 30;

        let value = snapshot_for(Some(state));
        assert_eq!(value.remaining_seconds, 0);
    }

    #[test]
    fn snapshot_is_camel_case_json() {
        let value = snapshot_for(None);
        let json = serde_json::to_string(&value).unwrap();
        assert!(json.contains("\"remainingSeconds\""));
        assert!(json.contains("\"delaySeconds\""));
        assert!(json.contains("\"statePath\""));
        assert!(json.contains("\"state\":null"));
    }

    #[test]
    fn page_carries_no_external_assets() {
        // 离线可用：不得出现 http(s) 外链脚本 / 样式。
        assert!(!INDEX_HTML.contains("src=\"http"));
        assert!(!INDEX_HTML.contains("href=\"http"));
        assert!(INDEX_HTML.contains("/api/state"));
    }

    #[test]
    fn countdown_label_stacks_under_the_digits() {
        // 回归：`<small>` 是 `<span class="count">` 的兄弟节点，
        // 写成 `.count small` 匹配不到，文字会和数字挤在同一行。
        assert!(INDEX_HTML.contains("<span class=\"count\" id=\"count\">"));
        assert!(INDEX_HTML.contains("<small id=\"countLabel\">"));
        assert!(!INDEX_HTML.contains(".count small{"));
        assert!(INDEX_HTML.contains(".ring .label small{display:block"));
    }
}
