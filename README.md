# Aether — Git 自动同步引擎

跨平台命令行工具。监听 Git 仓库工作区的文件变动，按策略自动执行 `git add .` → `git commit -m "Aether Auto-Sync"` → `git push`。

- 监听库：`notify`（Windows ReadDirectoryChangesW / Linux inotify / macOS FSEvents）
- Git 层：`git` CLI 封装
- Alist 备份：`reqwest::blocking` + `serde_json` + `urlencoding`
- 语言：Rust 2021；CLI / 同步引擎不暴露异步生命周期（HTTP 用阻塞式 API），无 unsafe
- 仓库根：本目录（`Aether-Rust/`），crate 直接位于根下

## 环境要求

- Rust ≥ 1.88（`rustup` + `cargo`；依赖树中 `icu_normalizer 2.4` / `clap 4.6` 的 MSRV 决定）
- `git` 在 PATH 中（已配置好 credential / remote 的仓库）
- 本机安装的是 `stable-x86_64-pc-windows-gnu` 工具链（无 MSVC / Windows SDK 时的选择）
- HTTPS 备份依赖系统 TLS：Windows 用 SChannel、Linux/macOS 用 OpenSSL（通过 `reqwest` 的 `native-tls`）；纯 HTTP 无额外要求

### 本机 windows-gnu 的额外配置（已完成）

rustup 的 minimal gnu 发行版自带 GNU `dlltool.exe`，但不含它生成导入库所需的 `as.exe`。`windows-sys >= 0.61` 的 raw-dll 链接会触发 rustc 调用 dlltool，缺 `as.exe` 时报错 `dlltool.exe: CreateProcess`。本机已补齐：

- Rust：`rustup-init.exe -y --default-host x86_64-pc-windows-gnu --default-toolchain stable --profile minimal --no-modify-path`，走 `RUSTUP_DIST_SERVER=https://rsproxy.cn` 镜像（版本 `rustc 1.99.1`）。
- `as.exe`：取自 MSYS2 `mingw-w64-x86_64-binutils`，已放到 `%USERPROFILE%\.cargo\bin\as.exe` 和工具链的 `...\bin\self-contained\as.exe`。
- 因安装时用了 `--no-modify-path`，构建前把 cargo bin 加入 PATH（dlltool 通过 PATH 找到 `as.exe`）：

```bash
# Git Bash
export PATH="$USERPROFILE/.cargo/bin:$PATH"
# PowerShell
$env:PATH = "$env:USERPROFILE\.cargo\bin;$env:PATH"
```

MSVC 工具链或已装完整 MinGW 的机器无此限制；升级 Rust 工具链后若 `as.exe` 被覆盖，重新复制一次即可。

## 构建

```bash
cd Aether-Rust
cargo build --release          # target/release/aether(.exe)
cargo test                     # 单元 + 端到端测试
cargo run -- start --strategy delay
```

加入 PATH（示例）：

```bash
# Windows PowerShell
$env:PATH += ";$PWD\target\release"
# Linux / macOS
export PATH="$PWD/target/release:$PATH"
```

## 命令

```
aether start [--strategy immediate|delay|never] [--delay <duration>] [--path <dir>]
aether status [--json]
aether backup --local <file|dir> --remote <dir> [--on-conflict skip|overwrite|rename] [--resume] [--reset-resume] [--json] [--url <url>] [--token <token>]
```

| 命令 | 说明 |
| --- | --- |
| `aether start` | 前台监听，Ctrl+C 优雅退出；自动定位 `git rev-parse --show-toplevel` 作为监听根 |
| `aether status` | 查看监听状态；进程存活返回 0，未运行返回 1 |
| `aether status --json` | 输出完整状态 JSON，便于脚本集成 |
| `aether backup` | 阻塞式上传文件或整个目录树到 Alist，默认跳过云端冲突，`--resume` 支持断点续跑（详见下方章节），不依赖监听进程 |

`--delay` 支持 `10m`、`30s`、`2h`、`500ms`、`1d`；纯数字按分钟。默认 `10m`。

## 三种策略

| 策略 | 行为 |
| --- | --- |
| `immediate` | 变动后等待 **2s 静默窗口** 立即同步（合并一次保存产生的多个事件） |
| `delay` | 变动后启动 **10m 倒计时**；期间有新变动则**重置**倒计时，等人停手再提交 |
| `never` | 完全不碰 git，仅打印提醒并用 `git status --porcelain` 记录待提交文件数（提醒节流 10s） |

示例：

```bash
aether start --strategy immediate
aether start --strategy delay --delay 10m     # 默认
aether start --strategy delay --delay 30s
aether start --strategy never
aether status
```

同步结果记录在状态里：`ok`（已 push）/ `skipped`（`git add .` 后无改动）/ `partial`（本地提交成功、push 失败）/ `failed`。

## 排除规则

监听时排除路径中任意层级的 `.git`、`bin`、`obj` 目录。排除 `.git` 是关键：git 自身提交/推送会大量写 `.git`，不过滤会造成无限自我触发循环。

> 排除**只影响触发**，`git add .` 仍按仓库 `.gitignore` 执行。若不希望 `bin/`、`obj/` 被提交，请在仓库 `.gitignore` 中加入它们。

## 状态文件

默认 `~/.aether/state.json`（可用 `AETHER_HOME` 覆盖）。**绝不能放在被监听目录内**，否则写入状态会自我触发。

- 引擎每 5s 写一次心跳；`status` 判定 `state != stopped` 且心跳在 15s 内即视为运行中。进程被强杀后最多 15s 自动显示未运行，无需平台相关的进程查询 API。
- 状态写入采用临时文件 + 原子替换，`status` 不会读到半截 JSON。
- 倒计时显示基于状态中的 `nextSyncAt`（Unix 秒），无需引擎持续刷新。

`aether status` 输出字段：PID、启动/心跳时间、仓库路径、策略、排除项、状态、事件计数、待提交数、下次同步倒计时、上次同步结果、状态文件路径。

## Alist 备份（backup）

直连 Alist 网盘聚合平台，用阻塞 HTTP 把本地文件 `PUT` 到 `/api/fs/put`。不需要先启动 `start`，也不写入 Aether 状态文件。

```bash
export AETHER_ALIST_URL="https://alist.example.com"
export AETHER_ALIST_TOKEN="alist-xxxxxxxx"   # 建议用环境变量，避免 token 进入 shell history / 进程列表
aether backup --local ./report.pdf --remote /backup/2026

# 也可显式传参（优先级高于环境变量）
aether backup --local ./报告.pdf --remote /备份 文档 \
  --url http://127.0.0.1:5244 --token alist-xxxxxxxx
```

| 配置 | 取值优先级（高 → 低） |
| --- | --- |
| Alist 站点根 | `--url` → `AETHER_ALIST_URL` → `ALIST_URL` |
| 访问令牌 | `--token` → `AETHER_ALIST_TOKEN` → `ALIST_TOKEN` |

URL 传**站点根**（末尾斜杠自动去掉），模块自行拼 `/api/fs/put`；token 是 Alist 控制台生成的原始值，**不加 `Bearer` 前缀**。未提供 token 时仍尝试匿名上传（需 Alist 开启游客写权限）。

请求细节：

- `PUT`，headers：`Authorization: {token}`、`File-Path: {URL 编码后的远程绝对路径}`、`Content-Type: application/octet-stream`、`Content-Length: {文件大小}`。
- Body 直接是文件流 `reqwest::blocking::Body::from(File)`，不把整文件读进内存；CLI 的进度版用 `Body::sized` 包一层带计数的 Reader（其余语义完全一致）。
- 中文 / 空格按**路径段**编码并保留 `/` 分隔。`urlencoding` 会把空格写成 `+`，但 Alist 服务端用 Go 的 `url.PathUnescape` 解码，`+` 不会被还原，因此统一替换为 `%20`。
- 响应解析为 JSON，只有 `code == 200` 视为成功；非 200、非 JSON、网络错误都会带上 HTTP 状态与服务端 message 报错。
- 单次请求总超时 300s（`alist::UPLOAD_TIMEOUT`）。

`--remote` 会规范化成绝对路径：`backup`、`/backup/`、`a//b` 分别归一为 `/backup`、`/backup`、`/a/b`；含 `..` 的路径会被拒绝以防越权写入。单文件模式上传到 `/remote/<文件名>`；目录模式保留相对目录结构（`src/main.rs` → `/backup/src/main.rs`）。`--remote` 请传**原始路径**，模块内部负责编码（传入已编码字符串会被二次编码）。

> **Git Bash / MSYS2 注意**：MSYS 会把以 `/` 开头的命令行参数自动转换成 Windows 路径，`--remote /backup` 可能实际变成 `/D:/.../backup`。Aether 检测到这种转换会直接报错退出（码 2），不会静默上传到错误目录。请加前缀 `MSYS_NO_PATHCONV=1 aether backup ...`，或改用 PowerShell / CMD。

终端会输出文件数、总大小、目标路径、上传百分比（每 120ms 采样、只在百分比变化时刷新）、逐项结果与汇总。退出码：`0` 全部成功（含跳过），`1` 有文件失败，`2` 缺少 URL、本地路径不存在或路径非法。

### 批量备份与冲突校验

`--local` 传**目录**即递归上传整个目录树，复用 watcher 的排除规则（任意层级的 `.git` / `bin` / `obj`），符号链接不跟随。整批复用同一个 HTTP Client；单文件失败不中断，末尾汇总 `N uploaded, M skipped, K failed`。进度按整体字节数统计，跳过的文件也计入「已处理」。

上传前逐个校验：

1. **本地占用**：`File::open` 预检（Windows 独占写锁会直接失败）；上传前后比对 `len + modified`，期间被外部改写则标记失败并提示云端副本可能不一致。
2. **云端冲突**：`POST /api/fs/get` 探测同名文件。云端不存在 → 上传；存在且 `size` 与本地相同 → 跳过（视为已是最新）；`size` 不同 → 冲突。
3. **探测失败**（鉴权 / 网络 / 非预期响应）一律记失败且**不上传**，绝不把错误当成「云端无文件」而覆盖。

`--on-conflict` 决定冲突处理，默认 `skip`：

| 策略 | 行为 |
| --- | --- |
| `skip`（默认） | 跳过，detail 记录云端 / 本地 size 与云端 modified，绝不覆盖 |
| `overwrite` | 覆盖云端同名文件，detail 明确记录 |
| `rename` | 上传为 `<name>-conflict-<unix秒>.<ext>`，两边都保留；新名已存在则记失败 |

`--json` 输出结构化 `BatchReport`（每项含 `local` / `remote` / `outcome` / `detail`）。冲突原因就在 `detail` 里，**后续 UI 消息中心可直接消费该结构**，无需额外事件总线。

> 冲突判定目前基于「云端是否存在 + size」，未做内容 hash 或时间戳大小比较。风险：本地与云端同时改动但字节数恰好相同时会被当作「已是最新」而跳过。
>
> 目录模式假定 Alist `PUT` 会创建缺失的父目录；若目标服务端不自动建目录，需补 `/api/fs/mkdir`（尚未实机验证，见 TODO #1）。

### 任务级断点续传

`--resume` 启用断点清单：每个文件上传成功后立刻写入 `~/.aether/backups/<job>.json`（`AETHER_HOME` 可覆盖），进程被 Ctrl+C / 断网 / 崩溃中断后，重跑同一条命令会自动跳过已完成项，**连云端探测请求都不再发**。

```bash
aether backup --local ./project --remote /backup --resume
# 中断后重跑同一条命令即可续传
aether backup --local ./project --remote /backup --resume
# 云端被清空 / 想强制全量重传
aether backup --local ./project --remote /backup --resume --reset-resume
```

| 规则 | 说明 |
| --- | --- |
| 任务身份 | `站点根 + 规范化远程目录 + 本地绝对路径` 的 FNV-1a 哈希；换任一参数即换清单，不会串任务 |
| 完成判定 | `size + mtime` 指纹；本地文件改动后该条记录自动失效并重新上传 |
| 记账范围 | **只有上传成功才记完成**；冲突跳过 / 失败项下次仍会重新探测，绝不把「没传上去」记成已完成 |
| 清理 | 整批无失败（含按策略跳过）时自动删除清单；有失败则保留供续跑 |
| 位置保护 | 默认 `~/.aether/backups/`；**目录模式**下若 `AETHER_HOME` 落在被备份目录树内，直接报错退出（码 2），避免 watcher 自我触发。单文件模式不限制（备份 `~/report.pdf` 时清单在 `~/.aether` 属正常） |
| 可观测 | `--json` 报告含 `resumed` 字段；续跑项 `outcome` 为 `skipped`、`detail` 以 `resumed:` 开头，UI 可直接渲染 |

> **为什么不是字节级续传**：Alist 官方 `/api/fs/put` 是整文件 PUT，没有 Range / 分片合并接口（`Content-Range` 只存在于第三方 OpenList_Chunk fork；服务端 Chunker 驱动对客户端透明，也不提供断点）。因此**单个大文件中断后仍需整文件重传**，本功能保证的是「已完成的文件不重复传、已完成的进度不丢」。

库 API（供后续功能复用）：

```rust
aether::alist::upload_to_alist(url, token, local_path, remote_dir) -> Result<String, String>
```

成功返回**可读的远程路径**（如 `/backup/报告.pdf`）；`upload_to_alist_with_progress` 额外接收 `Arc<AtomicU64>`，供 GUI / daemon 显示真实进度。签名与行为保持不变。

批量编排在 `aether::backup`：`plan(local, remote_dir)` 产出待传文件清单（含相对目录结构），`run_backup(&client, local, remote_dir, &BackupOptions, sent, Option<&mut ResumeStore>)` 顺序执行并返回 `BatchReport`；`alist::new_client()` / `alist::fs_get()` / `alist::upload_file_to_path()` 是可单独复用的网络层入口，断点清单在 `aether::resume`（`ResumeStore::open` / `is_done` / `record` / `reset`）。

## 设计权衡

**为什么用 git CLI 而不是 libgit2 / git2-rs**：CLI 自动继承用户的 credential helper、SSH agent、git hooks、GPG 签名、LFS 过滤器、代理配置和 `insteadOf` 重写；libgit2 在这些场景常出现「能 commit 但 push 失败」。封装层 `src/git.rs` 仍提供库式 API（`add_all` / `check_staged` / `commit` / `push`），后续若需替换后端只需改这一个模块。命令通过 `ArgumentList` 式的 `args()` 传参，无 shell 拼接注入风险，并设 `GIT_TERMINAL_PROMPT=0` 避免后台同步卡在交互输入。

**为什么 delay 会重置倒计时**：一次编辑往往产生多次文件写入，固定 10m 会在编辑中途提交。重置语义保证只在静默后提交。

## 模块结构（便于后续迭代）

| 文件 | 职责 |
| --- | --- |
| `src/main.rs` | CLI 解析、启动校验、`status` 渲染、`backup` 批量进度 / 汇总 / `--json` 与环境变量 |
| `src/options.rs` | 策略枚举、时长解析、窗口常量 |
| `src/filter.rs` | 路径段排除规则 |
| `src/watcher.rs` | `notify` 递归监听 → mpsc 事件流 |
| `src/git.rs` | git 命令封装 |
| `src/engine.rs` | 三策略状态机、倒计时、心跳、同步执行 |
| `src/state.rs` | 状态模型、原子持久化、心跳判定 |
| `src/alist.rs` | Alist `/api/fs/put` / `/api/fs/get` 阻塞请求、路径编码、响应判定、进度计数 |
| `src/backup.rs` | 目录递归枚举、冲突策略、本地占用检测、断点接入、结构化 `BatchReport` |
| `src/resume.rs` | 任务身份哈希、断点清单持久化与指纹失效、清单位置保护 |
| `tests/integration.rs` | 真实临时仓库 + bare remote 端到端测试 |
| `tests/alist.rs` | 本地多请求 TCP mock server，验证 PUT/POST 请求形状、批量、排除、三策略与 CLI 退出码 |

## 已知限制

- 单实例：同一用户目录下只允许一个 watcher（`start` 检测到运行中会退出，码 3）。
- `git push` 依赖已有 remote；首次推送自动 `push -u origin <branch>`，无 remote 时标记为 `partial`。
- `never` 策略的提醒输出到启动终端，不写系统通知。
- 排除目录名固定为 `.git` / `bin` / `obj`，尚未开放 CLI 自定义（扩展点已留在 `Options.excludes`）。
- `backup` 的断点续传是**任务级**：已完成的文件不重传；但 Alist 没有 Range / 分片接口，单个大文件中断后仍需整文件重传。冲突判定只看云端存在性 + size，未做 hash / 时间戳比对。
- 批量为顺序上传、未并发；大目录耗时较长。目录模式依赖 Alist 自动创建父目录，尚未实机验证。
- `backup` 尚未接入 `start` 的自动同步流程（不会在 commit 后自动备份），两者相互独立。

## 测试覆盖

`cargo test` 包含时长解析、路径过滤、状态原子读写/心跳、git add-commit-push 全链路、delay 倒计时重置只产生一次提交、never 不产生提交、`.git`/`bin`/`obj` 不触发、immediate 快速同步。集成测试使用真实临时 Git 仓库和 bare remote，不联网、不改动用户仓库。

Alist 部分另外覆盖：远程路径归一化与 `..` 拒绝、中文/空格路径编码（`%20` 且无 `+`）、`AlistResponse` / `RemoteFile` 解析、以及用**本地多请求 TCP mock server** 接住真实 HTTP 请求，断言 `PUT /api/fs/put`、`POST /api/fs/get`、`Authorization`、编码后的 `File-Path`、`Content-Length`、原始文件字节流、`code != 200` / 非 JSON 的报错。

批量与冲突部分覆盖：单文件兼容、目录递归保留相对结构、`.git`/`bin`/`obj` 排除、单文件失败后继续、`skip`（不覆盖）/ `overwrite` / `rename`（新名带时间戳）三种策略、云端同 size 跳过、探测失败保守不上传、`--json` 结构化报告、`upload_to_alist` 原签名行为不变。全程不访问真实 Alist 服务。

断点续传部分覆盖：任务 key 对站点 / 远程目录 / 本地路径三者敏感且稳定、清单原子读写与往返、损坏或跨任务清单自愈、`size`/`mtime` 变化即失效、`clear` 幂等且不影响其他任务、拒绝 `AETHER_HOME` 落在被备份树内；CLI 侧用同一个 mock server 连跑两次，验证「中断后只补传失败项、已完成项零请求」、本地文件改动后重新上传、不带 `--resume` 行为不变、整批成功后清单自动清理、`--reset-resume` 强制全量、`--json` 的 `resumed` 与 `detail` 可被 UI 消费。

## 后续路线（TODO）

状态：**#1、#2、#3 已实现**；#4 未开始。

| # | 待办 | 状态 | 关键约束 / 落点 |
| --- | --- | --- | --- |
| 1 | 目录递归与批量备份 | ✅ 已实现 | `src/backup.rs::plan` 递归枚举，排除 `.git` / `bin` / `obj`，失败继续并汇总，进度按整体字节。**遗留**：Alist `PUT` 是否自动创建多级父目录尚未用真实服务端验证，若不支持需补 `/api/fs/mkdir`。 |
| 2 | 断点续传与分片 | ✅ 已实现（任务级） | **字节级续传不可行**：Alist `/api/fs/put` 是整文件 PUT，官方无分片合并 / Range 接口（`Content-Range` 仅第三方 OpenList_Chunk fork）。已实现任务级断点：`src/resume.rs` 持久化已完成文件指纹到 `~/.aether/backups/`，逐项落盘、失败保留、成功后自清理，`--resume` / `--reset-resume` 控制。**遗留**：单文件中断仍需整文件重传。 |
| 3 | 同步防冲突校验 | ✅ 已实现 | `POST /api/fs/get` 探测 + size 比对，默认 `skip`，另有 `overwrite` / `rename`；本地占用用 open 预检 + 上传前后 metadata 一致性。**遗留**：未做 hash / 时间戳大小比对；`--json` 的 `BatchReport.detail` 已结构化保留冲突原因，待 #4 的 UI 消息中心直接消费。 |
| 4 | 系统托盘 / UI | 未开始 | 架构级改动：tray-icon + 事件循环与 `notify` watcher 线程整合；状态可复用 `~/.aether/state.json` 的 5s 心跳。**冲突消息**接 `BatchReport`（跳过 / 失败原因）展示在 UI 消息中心。建议先做 `status --watch` 或本地 HTML 状态页验证需求，再上原生托盘。 |

关联现状：`backup` 已支持目录批量，但仍未接入 `start` 的 commit 后自动同步流程；详见「已知限制」。
