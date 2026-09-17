#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::collections::{BTreeMap, HashMap};
use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use tauri::menu::{Menu, MenuItem, Submenu};
use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

mod ais_codex;

const HOST: &str = "127.0.0.1";
const PORT: u16 = 3080;
const NPM_REGISTRY: &str = "https://registry.npmjs.org";
const NPM_MIRROR: &str = "https://registry.npmmirror.com";
// 固定已验证的 dsh 版本，避免上游发破坏性新版本
// 0.1.6-alpha.1 已实测：首页 200、62/62 前端资源全部 200（对比 0.1.2-alpha.2 曾因 client-runtime 404 导致白屏）
const DSH_PKG: &str = "@deepseek-ai/dsh@0.1.6-alpha.1";
const INSTALL_TIMEOUT_SECS: u64 = 15 * 60;

// ---------- 数据结构 ----------

#[derive(Clone, serde::Serialize)]
struct Problem {
    #[serde(rename = "type")]
    problem_type: String,
    title: String,
    detail: String,
}

#[derive(Clone, serde::Serialize)]
struct DetectResult {
    ok: bool,
    problems: Vec<Problem>,
    install_cmd: String,
}

#[derive(Clone, serde::Serialize)]
struct VersionInfo {
    app_version: String,
    target_dsh: String,
    runtime_dsh: String,
}

struct DshProcess(Mutex<Option<Child>>);
struct InstallProcess(Mutex<Option<i32>>); // 安装进程组 pid
struct QuitFlag(AtomicBool); // 是否正在退出，用于抑制崩溃自动重启
struct DshPort(Mutex<Option<u16>>); // 当前 dsh web 端口（用于手动重启时等待就绪）
struct DshWebUrl(Mutex<Option<String>>); // dsh 当前带认证 token 的 Web URL
struct DshVersion(Mutex<Option<String>>); // 当前 dsh 版本（重启后重注入角标用）
struct LogFile(Mutex<Option<std::fs::File>>); // install.log 常驻句柄，避免每行 open/close

// ---------- 日志 ----------

fn log_to_file(app: &tauri::AppHandle, line: &str) {
    let state = app.state::<LogFile>();
    let mut guard = state.0.lock().unwrap();
    if guard.is_none() {
        if let Ok(dir) = app.path().app_log_dir() {
            if std::fs::create_dir_all(&dir).is_ok() {
                if let Ok(f) = OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(dir.join("install.log"))
                {
                    *guard = Some(f);
                }
            }
        }
    }
    if let Some(f) = guard.as_mut() {
        let _ = writeln!(f, "{}", line);
    }
}

fn emit_log(app: &tauri::AppHandle, line: &str) {
    log_to_file(app, line);
    let _ = app.emit("install_log", format!("{line}\n"));
}

// AIS Switch 操作的排障日志（写到 install.log，前缀 [ais]）
fn ais_log(app: &tauri::AppHandle, line: &str) {
    log_to_file(app, &format!("[ais] {line}"));
}

// ---------- 工具函数 ----------

fn port_in_use(host: &str, port: u16) -> bool {
    TcpStream::connect((host, port)).is_ok()
}

fn wait_for_port(host: &str, port: u16, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if port_in_use(host, port) {
            return true;
        }
        thread::sleep(Duration::from_millis(300));
    }
    false
}

// 从 start 起找一个空闲端口（不再复用 3080 上的陌生进程）
fn find_free_port(start: u16) -> u16 {
    let mut p = start;
    while p < start + 1000 {
        if !port_in_use(HOST, p) {
            return p;
        }
        p += 1;
    }
    start
}

fn current_node_version() -> String {
    if let Ok(out) = Command::new("/bin/bash").arg("-lc").arg("node -v").output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return s;
            }
        }
    }
    "未知".to_string()
}

// 通过 shell 解析 PATH（含 nvm），拿到真正的 dsh 路径，覆盖 volta/fnm/asdf 等
fn find_dsh_via_shell() -> Option<PathBuf> {
    let script = r#"export NVM_DIR="$HOME/.nvm"
[ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh"
command -v dsh"#;
    if let Ok(out) = Command::new("/bin/bash").arg("-lc").arg(script).output() {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout);
            if let Some(last) = s.lines().last() {
                let p = last.trim();
                if !p.is_empty() {
                    return Some(PathBuf::from(p));
                }
            }
        }
    }
    None
}

// 解析 nvm 目录名（如 v22.22.3）为数值，便于按 semver 排序
fn version_key(v: &str) -> (u64, u64, u64) {
    let v = v.trim_start_matches('v');
    let mut it = v.split('.');
    let major = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let minor = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    let patch = it.next().and_then(|s| s.parse().ok()).unwrap_or(0);
    (major, minor, patch)
}

fn find_dsh_candidates() -> Vec<PathBuf> {
    let mut candidates = Vec::new();
    if let Some(via_shell) = find_dsh_via_shell() {
        candidates.push(via_shell);
    }
    if let Some(home) = std::env::var_os("HOME") {
        let home = PathBuf::from(home);
        let nvm_dir = home.join(".nvm").join("versions").join("node");
        if nvm_dir.is_dir() {
            if let Ok(entries) = std::fs::read_dir(&nvm_dir) {
                let mut versions: Vec<PathBuf> = entries
                    .filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .collect();
                // 按 semver 数值排序，避免字符串排序把 v9.x 排在 v22.x 前面
                versions.sort_by_key(|p| {
                    let name = p
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    version_key(&name)
                });
                versions.reverse();
                for v in versions {
                    let p = v.join("bin").join("dsh");
                    if p.is_file() {
                        candidates.push(p);
                    }
                }
            }
        }
        for rel in [".local/bin/dsh", "Library/pnpm/dsh", ".volta/bin/dsh"] {
            let p = home.join(rel);
            if p.is_file() {
                candidates.push(p);
            }
        }
    }
    let extra = [
        "/opt/homebrew/bin/dsh",
        "/opt/homebrew/opt/node@22/bin/dsh",
        "/usr/local/bin/dsh",
        "/usr/local/opt/node@22/bin/dsh",
    ];
    for p in extra {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            candidates.push(pb);
        }
    }
    candidates
}

fn dsh_candidates_cache_lock() -> &'static Mutex<Option<Vec<PathBuf>>> {
    static CACHE: OnceLock<Mutex<Option<Vec<PathBuf>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(None))
}

fn dsh_version_cache_lock() -> &'static Mutex<HashMap<PathBuf, Option<String>>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, Option<String>>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

// 候选列表缓存：一次冷启动里 PATH / nvm 目录基本不变，避免重复 spawn bash 登录 shell。
// 升级 dsh 后必须能失效，故不用 OnceLock 存值。
fn find_dsh_candidates_cached() -> Vec<PathBuf> {
    let cache = dsh_candidates_cache_lock();
    {
        let guard = cache.lock().unwrap();
        if let Some(v) = guard.as_ref() {
            return v.clone();
        }
    }
    let v = find_dsh_candidates();
    *cache.lock().unwrap() = Some(v.clone());
    v
}

fn run_dsh_version(dsh_path: &PathBuf) -> Option<String> {
    let dir = dsh_path.parent()?;
    let mut path_env = dir.to_string_lossy().to_string();
    if let Ok(existing) = std::env::var("PATH") {
        path_env = format!("{}:{}", path_env, existing);
    }
    let output = Command::new(dsh_path)
        .arg("--version")
        .env("PATH", path_env)
        .output();
    if let Ok(out) = output {
        if out.status.success() {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    None
}

// 缓存 dsh --version 结果，避免对同一二进制重复 spawn（启动链路会查多遍）
fn run_dsh_version_cached(dsh_path: &PathBuf) -> Option<String> {
    let cache = dsh_version_cache_lock();
    {
        let guard = cache.lock().unwrap();
        if let Some(v) = guard.get(dsh_path) {
            return v.clone();
        }
    }
    let v = run_dsh_version(dsh_path);
    cache.lock().unwrap().insert(dsh_path.clone(), v.clone());
    v
}

/// 升级安装后二进制已原地替换，必须清掉路径/版本缓存再重新探测。
fn clear_dsh_caches() {
    *dsh_candidates_cache_lock().lock().unwrap() = None;
    dsh_version_cache_lock().lock().unwrap().clear();
}

/// DSH_PKG 中的目标版本号（如 0.1.6-alpha.1）
fn target_dsh_version() -> &'static str {
    DSH_PKG.split('@').last().unwrap_or("").trim()
}

fn normalize_version(v: &str) -> &str {
    v.trim().trim_start_matches('v')
}

// alpha 版本的 dsh web 需要使用启动时打印的一次性 token URL。
fn dsh_url_from_output(line: &str, port: u16) -> Option<String> {
    let marker = format!("http://{HOST}:{port}/");
    let start = line.find(&marker)?;
    let url = line[start..].split_whitespace().next()?;
    if url.contains("?token=") {
        Some(url.to_string())
    } else {
        None
    }
}

fn capture_dsh_url(stdout: Option<ChildStdout>, port: u16) -> mpsc::Receiver<String> {
    let (sender, receiver) = mpsc::channel();
    if let Some(stdout) = stdout {
        thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            let mut line = String::new();
            let mut sent = false;
            loop {
                line.clear();
                match reader.read_line(&mut line) {
                    Ok(0) | Err(_) => break,
                    Ok(_) if !sent => {
                        if let Some(url) = dsh_url_from_output(&line, port) {
                            let _ = sender.send(url);
                            sent = true;
                        }
                    }
                    Ok(_) => {}
                }
            }
        });
    }
    receiver
}

fn current_dsh_web_url(app: &tauri::AppHandle) -> String {
    if let Some(url) = app.state::<DshWebUrl>()
        .0
        .lock()
        .unwrap()
        .clone()
    {
        return url;
    }
    let port = app.state::<DshPort>().0.lock().unwrap().unwrap_or(PORT);
    format!("http://{HOST}:{port}")
}

fn navigate_main_to_dsh(app: &tauri::AppHandle) {
    let url = current_dsh_web_url(app);
    let Ok(js_url) = serde_json::to_string(&url) else {
        return;
    };
    let app2 = app.clone();
    let _ = app.run_on_main_thread(move || {
        if let Some(w) = app2.get_webview_window("main") {
            let _ = w.eval(&format!("window.location.href={js_url};"));
        }
    });
}

// 选择 dsh：优先匹配目标版本（DSH_PKG 中的版本号，如 0.1.0-rc.7），
// 避免 PATH 中靠前的旧版（如 rc.6）被误选；无匹配版本时退回第一个能运行的。
fn select_dsh() -> Option<PathBuf> {
    let target = target_dsh_version();
    let candidates = find_dsh_candidates_cached();
    // 并行探测各候选版本，避免串行 spawn 拖慢冷启动
    let versions: Vec<Option<String>> = std::thread::scope(|scope| {
        let handles: Vec<_> = candidates
            .iter()
            .map(|c| scope.spawn(move || run_dsh_version_cached(c)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut first_working: Option<PathBuf> = None;
    for (c, ver) in candidates.iter().zip(versions) {
        if let Some(ver) = ver {
            if first_working.is_none() {
                first_working = Some(c.clone());
            }
            // 版本输出形如 "0.1.0-rc.7"，兼容可能的 "v" 前缀或前后空白
            if normalize_version(&ver) == target {
                return Some(c.clone());
            }
        }
    }
    first_working
}

fn manual_install_cmd() -> String {
    format!(
        r#"export NVM_DIR="$HOME/.nvm"
[ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh"
nvm install 22
nvm use 22
mkdir -p "$HOME/.local/npm-cache"
export npm_config_cache="$HOME/.local/npm-cache"
npm i -g --prefer-online {dsh_pkg} --registry={registry} || npm i -g --prefer-online {dsh_pkg} --registry={mirror}"#,
        dsh_pkg = DSH_PKG,
        registry = NPM_REGISTRY,
        mirror = NPM_MIRROR,
    )
}

fn detect_environment() -> DetectResult {
    let candidates = find_dsh_candidates_cached();
    let mut problems = Vec::new();

    if candidates.is_empty() {
        problems.push(Problem {
            problem_type: "dsh-missing".into(),
            title: "未检测到 DeepSeek Harness (dsh)".into(),
            detail: format!("需要 Node 22.19+（或 Node 24+）以及 {}", DSH_PKG),
        });
        return DetectResult {
            ok: false,
            problems,
            install_cmd: manual_install_cmd(),
        };
    }

    let mut found = false;
    for c in &candidates {
        if run_dsh_version_cached(c).is_some() {
            found = true;
            break;
        }
    }

    if !found {
        problems.push(Problem {
            problem_type: "node-incompatible".into(),
            title: "dsh 已安装但无法运行".into(),
            detail: format!(
                "通常是 Node 版本过低（dsh 需要 Node 22.19+ 或 Node 24+）。当前 Node: {}。建议安装 Node 22。",
                current_node_version()
            ),
        });
    }

    DetectResult {
        ok: problems.is_empty(),
        problems,
        install_cmd: manual_install_cmd(),
    }
}

fn build_install_script() -> String {
    format!(
        r#"set +e
export NVM_DIR="$HOME/.nvm"
export REG="{registry}"
export MIRROR="{mirror}"
export DSH_PKG="{dsh_pkg}"
log() {{ echo "[install] $*"; }}
stage() {{ echo "[install:stage] $1"; }}

stage node
if [ -s "$NVM_DIR/nvm.sh" ]; then
  . "$NVM_DIR/nvm.sh"
  log "检测到 nvm，安装/使用 Node 22 ..."
  nvm install 22
  nvm use 22
elif command -v brew >/dev/null 2>&1; then
  log "未检测到 nvm，使用 Homebrew 安装 node@22 ..."
  brew install node@22
  export PATH="$(brew --prefix node@22)/bin:$PATH"
else
  log "错误：未检测到 nvm 或 Homebrew，无法自动安装 Node 22"
  exit 1
fi

stage install
log "安装 $DSH_PKG ..."

mkdir -p "$HOME/.local/npm-cache" "$HOME/.local"
export npm_config_cache="$HOME/.local/npm-cache"

if [ -d "$HOME/.npm" ] && [ ! -w "$HOME/.npm" ]; then
  log "检测到 ~/.npm 权限受限，尝试 sudo chown 修复..."
  sudo -n chown -R "$(whoami)" "$HOME/.npm" 2>/dev/null && log "sudo chown 成功" || log "sudo 跳过，已使用独立缓存继续安装"
fi

if npm i -g --prefer-online "$DSH_PKG" --registry="$REG" 2>/dev/null; then
  log "官方 registry 安装成功"
elif npm i -g --prefer-online "$DSH_PKG" --registry="$MIRROR" 2>/dev/null; then
  log "npmmirror 镜像安装成功"
else
  log "全局安装失败，fallback 到 --prefix=$HOME/.local（npmmirror）"
  npm i -g --prefix="$HOME/.local" --prefer-online "$DSH_PKG" --registry="$MIRROR"
fi

stage verify
log "验证安装 ..."
export PATH="$HOME/.local/bin:$PATH"
if command -v dsh >/dev/null 2>&1; then
  dsh --version
else
  log "dsh 仍不在 PATH，请复制上方命令在终端中手动安装"
  exit 1
fi
"#,
        registry = NPM_REGISTRY,
        mirror = NPM_MIRROR,
        dsh_pkg = DSH_PKG,
    )
}

fn start_dsh(dsh_path: &PathBuf, port: u16) -> Result<Child, String> {
    let dir = dsh_path.parent().ok_or("无效的 dsh 路径")?;
    let mut path_env = dir.to_string_lossy().to_string();
    if let Ok(existing) = std::env::var("PATH") {
        path_env = format!("{}:{}", path_env, existing);
    }
    Command::new(dsh_path)
        // rc.8 起 dsh web 默认自动打开系统浏览器，封装使用内嵌 webview，必须禁用
        .args(["web", "--host", HOST, "--port", &port.to_string(), "--no-open"])
        .env("PATH", path_env)
        .stdout(Stdio::piped())
        // 保留 stderr：崩溃/启动失败时写入 install.log，便于排障
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| e.to_string())
}

/// 把子进程 stderr 逐行落到日志（前缀 [dsh:err]）
fn pipe_stderr_to_log(app: &tauri::AppHandle, child: &mut Child) {
    let Some(stderr) = child.stderr.take() else {
        return;
    };
    let app = app.clone();
    thread::spawn(move || {
        let reader = BufReader::new(stderr);
        for line in reader.lines() {
            match line {
                Ok(l) if !l.trim().is_empty() => {
                    emit_log(&app, &format!("[dsh:err] {}", l.trim_end()));
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });
}

/// 导航白名单：仅允许访问本机 dsh 的 http + 固定端口，防止跳到同机其它服务。
fn navigation_allowed(scheme: &str, host: Option<&str>, port: Option<u16>, expect_port: u16) -> bool {
    scheme == "http" && host == Some(HOST) && port == Some(expect_port)
}

// 带重试地启动 dsh web（固定端口，失败/超时则重试 attempts 次）
fn start_dsh_with_retry(app: &tauri::AppHandle, dsh_path: &PathBuf, port: u16, attempts: u32) -> bool {
    for i in 1..=attempts {
        match start_dsh(dsh_path, port) {
            Ok(mut child) => {
                *app.state::<DshWebUrl>().0.lock().unwrap() = None;
                let url_receiver = capture_dsh_url(child.stdout.take(), port);
                pipe_stderr_to_log(app, &mut child);
                {
                    let state = app.state::<DshProcess>();
                    *state.0.lock().unwrap() = Some(child);
                }
                if wait_for_port(HOST, port, Duration::from_secs(20)) {
                    if let Ok(url) = url_receiver.recv_timeout(Duration::from_secs(3)) {
                        *app.state::<DshWebUrl>().0.lock().unwrap() = Some(url);
                    }
                    navigate_main_to_dsh(app);
                    return true;
                }
                emit_log(app, &format!("[dsh] 启动超时（第 {i}/{attempts} 次）"));
                if let Some(mut c) = app.state::<DshProcess>().0.lock().unwrap().take() {
                    let _ = c.kill();
                }
            }
            Err(e) => {
                emit_log(app, &format!("[dsh] 启动失败（第 {i}/{attempts} 次）: {e}"));
            }
        }
    }
    false
}

// 监控 dsh 进程，崩溃后自动重启（复用同一端口，窗口无需刷新）
fn monitor_dsh(app: tauri::AppHandle, dsh_path: PathBuf, port: u16) {
    let mut restarts = 0u32;
    let mut window_start = Instant::now();
    loop {
        // 等待当前进程退出（或被退出流程清理）
        loop {
            let state = app.state::<DshProcess>();
            let mut guard = state.0.lock().unwrap();
            let exited = match guard.as_mut() {
                None => true,
                Some(c) => match c.try_wait() {
                    Ok(Some(_)) => true,
                    Ok(None) => false,
                    Err(_) => true,
                },
            };
            drop(guard);
            if exited {
                break;
            }
            thread::sleep(Duration::from_millis(1000));
        }

        {
            let state = app.state::<DshProcess>();
            *state.0.lock().unwrap() = None;
        }

        if app.state::<QuitFlag>().0.load(Ordering::SeqCst) {
            return;
        }

        // 连续崩溃保护：60 秒内最多自动重启 3 次
        if window_start.elapsed() > Duration::from_secs(60) {
            restarts = 0;
            window_start = Instant::now();
        }
        restarts += 1;
        if restarts > 3 {
            emit_log(&app, "[dsh] 连续崩溃过多，停止自动重启，请手动重启应用");
            return;
        }
        emit_log(&app, &format!("[dsh] 进程退出，自动重启（第 {restarts} 次）..."));
        if !start_dsh_with_retry(&app, &dsh_path, port, 1) {
            emit_log(&app, "[dsh] 自动重启失败");
            return;
        }
    }
}

// 终止整个安装进程组（bash + npm 子进程），避免残留孤儿进程
fn kill_install_group(app: &tauri::AppHandle) {
    let state = app.state::<InstallProcess>();
    let mut guard = state.0.lock().unwrap();
    if let Some(pid) = guard.take() {
        unsafe { libc::kill(-pid, libc::SIGKILL); }
    }
}

// ---------- 命令 ----------

#[tauri::command]
fn get_detect_result() -> DetectResult {
    detect_environment()
}

fn version_info() -> VersionInfo {
    VersionInfo {
        app_version: env!("CARGO_PKG_VERSION").to_string(),
        target_dsh: DSH_PKG.to_string(),
        runtime_dsh: "未启动".to_string(),
    }
}

#[tauri::command]
fn get_version_info(app: tauri::AppHandle) -> VersionInfo {
    let mut info = version_info();
    info.runtime_dsh = app
        .state::<DshVersion>()
        .0
        .lock()
        .unwrap()
        .clone()
        .unwrap_or_else(|| "未启动".to_string());
    info
}

#[tauri::command]
fn install_dsh(app: tauri::AppHandle) {
    let script = build_install_script();
    thread::spawn(move || {
        let mut child = match Command::new("/bin/bash")
            .arg("-c")
            .arg(&script)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .process_group(0) // 独立进程组
            .spawn()
        {
            Ok(c) => c,
            Err(e) => {
                emit_log(&app, &format!("[install] 启动安装进程失败: {e}"));
                let _ = app.emit(
                    "install_done",
                    serde_json::json!({ "success": false, "message": format!("启动安装进程失败: {e}") }),
                );
                return;
            }
        };

        let pid = child.id() as i32;
        {
            let state = app.state::<InstallProcess>();
            *state.0.lock().unwrap() = Some(pid);
        }

        let stdout = child.stdout.take();
        let stderr = child.stderr.take();

        let app_stdout = app.clone();
        let t1 = thread::spawn(move || {
            if let Some(out) = stdout {
                let reader = BufReader::new(out);
                for line in reader.lines() {
                    if let Ok(l) = line {
                        emit_log(&app_stdout, &l);
                    }
                }
            }
        });
        let app_stderr = app.clone();
        let t2 = thread::spawn(move || {
            if let Some(err) = stderr {
                let reader = BufReader::new(err);
                for line in reader.lines() {
                    if let Ok(l) = line {
                        emit_log(&app_stderr, &l);
                    }
                }
            }
        });

        // 等待完成（带超时兜底）
        let deadline = Instant::now() + Duration::from_secs(INSTALL_TIMEOUT_SECS);
        let mut success = false;
        loop {
            match child.try_wait() {
                Ok(Some(status)) => {
                    success = status.success();
                    break;
                }
                Ok(None) => {}
                Err(_) => break,
            }
            if Instant::now() > deadline {
                emit_log(&app, "[install] 安装超时，终止进程组");
                unsafe { libc::kill(-pid, libc::SIGKILL); }
                break;
            }
            thread::sleep(Duration::from_millis(200));
        }

        {
            let state = app.state::<InstallProcess>();
            *state.0.lock().unwrap() = None;
        }

        let _ = t1.join();
        let _ = t2.join();

        if success {
            let result = detect_environment();
            let _ = app.emit(
                "install_done",
                serde_json::json!({
                    "success": result.ok,
                    "message": if result.ok { "" } else { "安装命令已执行，但检测仍不通过" }
                }),
            );
            if result.ok {
                let app_for_sleep = app.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(800));
                    let app_for_main = app_for_sleep.clone();
                    let _ = app_for_sleep.run_on_main_thread(move || {
                        if let Some(win) = app_for_main.get_webview_window("precheck") {
                            let _ = win.close();
                        }
                        launch_main(&app_for_main);
                    });
                });
            }
        } else {
            let _ = app.emit(
                "install_done",
                serde_json::json!({ "success": false, "message": "自动安装失败，请使用下方命令在终端中手动安装。" }),
            );
        }
    });
}

#[tauri::command]
fn cancel_precheck(app: tauri::AppHandle) {
    kill_install_group(&app);
    app.exit(0);
}

// ---------- 用量统计（聚合 ~/.dsh/sessions 下的 zstd 压缩会话日志） ----------

#[derive(Clone, serde::Serialize)]
struct TokenBreakdown {
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    total: u64,
}

#[derive(Clone, serde::Serialize)]
struct ModelStat {
    model: String,
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    calls: u64,
}

#[derive(Clone, serde::Serialize)]
struct SessionStat {
    workspace: String,
    session_id: String,
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    calls: u64,
}

#[derive(Clone, serde::Serialize)]
struct BalanceInfo {
    total_balance: String,
    currency: String,
}

#[derive(Clone, serde::Serialize)]
struct UsageStats {
    total: TokenBreakdown,
    by_model: Vec<ModelStat>,
    by_session: Vec<SessionStat>,
    estimated_cost_usd: f64,
    balance: Option<BalanceInfo>,
    session_count: usize,
    scanned_files: usize,
    errors: Vec<String>,
}

#[derive(Clone)]
struct FileAgg {
    input: u64,
    output: u64,
    cache_read: u64,
    reasoning: u64,
    calls: u64,
    by_model: BTreeMap<String, ModelStat>,
}

// 用量统计缓存：按文件（长度 + 修改时间）做增量，余额带 TTL
struct UsageCache {
    files: Mutex<HashMap<PathBuf, CachedScan>>,
    balance: Mutex<Option<(Instant, Option<BalanceInfo>)>>,
}

struct CachedScan {
    len: u64,
    modified: Option<SystemTime>,
    agg: FileAgg,
    cwd: Option<String>,
}

impl Default for UsageCache {
    fn default() -> Self {
        UsageCache {
            files: Mutex::new(HashMap::new()),
            balance: Mutex::new(None),
        }
    }
}


// 从 YAML 行 `KEY: value` 提取（去除首尾引号）
fn extract_yaml_value(content: &str, key: &str) -> Option<String> {
    let pat = format!("{}:", key);
    let line = content.lines().find(|l| l.trim_start().starts_with(&pat))?;
    let after = line.splitn(2, ':').nth(1)?;
    let v = after.trim().trim_matches('"').trim_matches('\'').to_string();
    if v.is_empty() {
        None
    } else {
        Some(v)
    }
}


// 估算费用（美元），参考 DeepSeek 公开定价，仅供参考
fn estimate_cost(model: &str, input: u64, cache_read: u64, output: u64) -> f64 {
    let (input_rate, cache_rate, output_rate) = if model.contains("reasoner") || model.contains("r1") {
        (0.55, 0.14, 2.19)
    } else {
        (0.27, 0.07, 1.10)
    };
    let m = 1_000_000.0;
    (input as f64 / m) * input_rate
        + (cache_read as f64 / m) * cache_rate
        + (output as f64 / m) * output_rate
}

// 最佳努力获取 DeepSeek 账户余额（需联网 + ~/.dsh/.credentials.yaml 中的 API Key）
// API Key 不进进程 argv：写入 0600 临时头文件，用 curl -H @file 读取，结束后立刻删除。
fn fetch_balance() -> Option<BalanceInfo> {
    let home = std::env::var("HOME").ok()?;
    let cred = PathBuf::from(&home).join(".dsh").join(".credentials.yaml");
    let content = std::fs::read_to_string(&cred).ok()?;
    let key = extract_yaml_value(&content, "DEEPSEEK_API_KEY")?;
    if key.is_empty() {
        return None;
    }
    let header_path = std::env::temp_dir().join(format!(
        "dsh-balance-h-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    if std::fs::write(&header_path, format!("Authorization: Bearer {key}\n")).is_err() {
        return None;
    }
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&header_path, std::fs::Permissions::from_mode(0o600));
    }
    let out = Command::new("curl")
        .args([
            "-sS",
            "-m",
            "10",
            "-H",
            &format!("@{}", header_path.display()),
            "https://api.deepseek.com/user/balance",
        ])
        .output();
    let _ = std::fs::remove_file(&header_path);
    let out = out.ok()?;
    let body = String::from_utf8_lossy(&out.stdout);
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let infos = v.get("balance_infos")?.as_array()?;
    let first = infos.first()?;
    let total = first.get("total_balance")?.as_str()?.to_string();
    let currency = first
        .get("currency")
        .and_then(|c| c.as_str())
        .unwrap_or("USD")
        .to_string();
    Some(BalanceInfo {
        total_balance: total,
        currency,
    })
}

fn aggregate_file(path: &PathBuf) -> Result<(FileAgg, Option<String>), String> {
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let buf_reader = std::io::BufReader::new(file);
    let decoder = zstd::stream::read::Decoder::new(buf_reader)
        .map_err(|e| format!("zstd 解压失败: {e}"))?;
    let mut agg = FileAgg {
        input: 0,
        output: 0,
        cache_read: 0,
        reasoning: 0,
        calls: 0,
        by_model: BTreeMap::new(),
    };
    let mut cwd = None;
    for line in std::io::BufReader::new(decoder).lines() {
        let line = line.map_err(|e| e.to_string())?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
            continue;
        };
        if cwd.is_none() {
            cwd = v.get("cwd").and_then(|c| c.as_str()).map(|s| s.to_string());
        }
        // 只统计最终的 assistant/message（流式 assistant/chunk 是同一批 usage 的增量，会计重）
        if v.get("type").and_then(|t| t.as_str()) != Some("assistant/message") {
            continue;
        }
        let Some(usage) = v.pointer("/data/usage") else {
            continue;
        };
        let i = usage
            .get("inputTokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let o = usage
            .get("outputTokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let c = usage
            .get("cacheReadTokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let r = usage
            .get("reasoningTokens")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        agg.input += i;
        agg.output += o;
        agg.cache_read += c;
        agg.reasoning += r;
        agg.calls += 1;
        if let Some(model) = v
            .pointer("/data/message/source/model")
            .and_then(|m| m.as_str())
        {
            let e = agg
                .by_model
                .entry(model.to_string())
                .or_insert_with(|| ModelStat {
                    model: model.to_string(),
                    input: 0,
                    output: 0,
                    cache_read: 0,
                    reasoning: 0,
                    calls: 0,
                });
            e.input += i;
            e.output += o;
            e.cache_read += c;
            e.reasoning += r;
            e.calls += 1;
        }
    }
    Ok((agg, cwd))
}

enum BalanceSource {
    Cached(Option<BalanceInfo>),
    Fetch(std::thread::JoinHandle<Option<BalanceInfo>>),
}

fn scan_sessions(app: &tauri::AppHandle) -> UsageStats {
    const BALANCE_TTL: Duration = Duration::from_secs(5 * 60);

    let mut errors: Vec<String> = Vec::new();
    let mut total = TokenBreakdown {
        input: 0,
        output: 0,
        cache_read: 0,
        reasoning: 0,
        total: 0,
    };
    let mut by_model: BTreeMap<String, ModelStat> = BTreeMap::new();
    let mut by_session: Vec<SessionStat> = Vec::new();
    let mut session_count = 0usize;
    let mut scanned_files = 0usize;

    let cache = app.state::<UsageCache>();

    // 余额：命中 TTL 缓存直接复用；否则后台并行拉取，不阻塞扫描
    let balance_source = {
        let guard = cache.balance.lock().unwrap();
        match guard.as_ref() {
            Some((at, b)) if at.elapsed() < BALANCE_TTL => {
                let cached = b.clone();
                drop(guard);
                BalanceSource::Cached(cached)
            }
            _ => {
                drop(guard);
                BalanceSource::Fetch(std::thread::spawn(fetch_balance))
            }
        }
    };

    let home = std::env::var("HOME").unwrap_or_default();
    let sessions_dir = PathBuf::from(&home).join(".dsh").join("sessions");
    if !sessions_dir.is_dir() {
        errors.push(format!("未找到会话目录: {}", sessions_dir.display()));
    } else if let Ok(ws_entries) = std::fs::read_dir(&sessions_dir) {
        for ws in ws_entries.flatten() {
            let ws_path = ws.path();
            if !ws_path.is_dir() {
                continue;
            }
            let slug = ws_path
                .file_name()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Ok(sess_entries) = std::fs::read_dir(&ws_path) {
                for sess in sess_entries.flatten() {
                    let sess_path = sess.path();
                    if !sess_path.is_dir() {
                        continue;
                    }
                    let session_id = sess_path
                        .file_name()
                        .map(|s| s.to_string_lossy().to_string())
                        .unwrap_or_default();
                    let file = sess_path.join("session.jsonl.zstd");
                    if !file.is_file() {
                        continue;
                    }
                    scanned_files += 1;
                    session_count += 1;

                    // 增量：文件未变化则复用上次聚合结果
                    let meta = std::fs::metadata(&file)
                        .ok()
                        .map(|m| (m.len(), m.modified().ok()));
                    let cached = meta.as_ref().and_then(|(len, modified)| {
                        let guard = cache.files.lock().unwrap();
                        guard
                            .get(&file)
                            .filter(|c| *len == c.len && *modified == c.modified)
                            .map(|c| (c.agg.clone(), c.cwd.clone()))
                    });

                    let (agg, cwd) = if let Some(hit) = cached {
                        hit
                    } else {
                        match aggregate_file(&file) {
                            Ok((agg, cwd)) => {
                                if let Some((len, modified)) = meta {
                                    let mut guard = cache.files.lock().unwrap();
                                    guard.insert(
                                        file.clone(),
                                        CachedScan {
                                            len,
                                            modified,
                                            agg: agg.clone(),
                                            cwd: cwd.clone(),
                                        },
                                    );
                                }
                                (agg, cwd)
                            }
                            Err(e) => {
                                errors.push(format!("{}: {}", file.display(), e));
                                continue;
                            }
                        }
                    };

                    total.input += agg.input;
                    total.output += agg.output;
                    total.cache_read += agg.cache_read;
                    total.reasoning += agg.reasoning;
                    total.total += agg.input + agg.output + agg.cache_read + agg.reasoning;
                    for ms in agg.by_model.values() {
                        let e = by_model.entry(ms.model.clone()).or_insert_with(|| ModelStat {
                            model: ms.model.clone(),
                            input: 0,
                            output: 0,
                            cache_read: 0,
                            reasoning: 0,
                            calls: 0,
                        });
                        e.input += ms.input;
                        e.output += ms.output;
                        e.cache_read += ms.cache_read;
                        e.reasoning += ms.reasoning;
                        e.calls += ms.calls;
                    }
                    by_session.push(SessionStat {
                        workspace: cwd.unwrap_or(slug.clone()),
                        session_id,
                        input: agg.input,
                        output: agg.output,
                        cache_read: agg.cache_read,
                        reasoning: agg.reasoning,
                        calls: agg.calls,
                    });
                }
            }
        }
    }

    let mut cost = 0.0f64;
    for ms in by_model.values() {
        cost += estimate_cost(&ms.model, ms.input, ms.cache_read, ms.output);
    }

    let balance = match balance_source {
        BalanceSource::Cached(b) => b,
        BalanceSource::Fetch(handle) => {
            let b = handle.join().unwrap_or(None);
            let mut guard = cache.balance.lock().unwrap();
            *guard = Some((Instant::now(), b.clone()));
            b
        }
    };

    let mut by_model_vec: Vec<ModelStat> = by_model.into_values().collect();
    by_model_vec.sort_by(|a, b| {
        (b.input + b.output + b.cache_read + b.reasoning)
            .cmp(&(a.input + a.output + a.cache_read + a.reasoning))
    });
    by_session.sort_by(|a, b| {
        (b.input + b.output + b.cache_read + b.reasoning)
            .cmp(&(a.input + a.output + a.cache_read + a.reasoning))
    });

    UsageStats {
        total,
        by_model: by_model_vec,
        by_session,
        estimated_cost_usd: cost,
        balance,
        session_count,
        scanned_files,
        errors,
    }
}

#[tauri::command]
async fn get_usage_stats(app: tauri::AppHandle) -> Result<UsageStats, String> {
    tauri::async_runtime::spawn_blocking(move || scan_sessions(&app))
        .await
        .map_err(|e| format!("用量统计线程异常：{e}"))
}

// AIS 体检最坏要串行跑 health/status/models/chat ping（最长 ~48s），
// Tauri 2 同步命令在主线程内联执行会卡死整个 UI；统一改 async + spawn_blocking
// 把阻塞的 curl / 文件 I/O 挪到线程池，保证窗口与菜单不冻结。
#[tauri::command]
async fn ais_switch_check(app: tauri::AppHandle) -> Result<ais_codex::AisCheckResult, String> {
    let r = tauri::async_runtime::spawn_blocking(ais_codex::check)
        .await
        .map_err(|e| format!("体检线程异常：{e}"))?;
    ais_log(
        &app,
        &format!(
            "体检 ok={} 模型数={} 代理down={} provider={} key={} 配置目录={}",
            r.ok,
            r.models.len(),
            r.proxy_down,
            r.has_provider,
            r.has_key,
            ais_codex::config_home_display()
        ),
    );
    Ok(r)
}

#[tauri::command]
async fn ais_switch_setup(app: tauri::AppHandle, set_default: bool) -> Result<ais_codex::AisActionResult, String> {
    let r = tauri::async_runtime::spawn_blocking(move || ais_codex::setup(set_default))
        .await
        .map_err(|e| format!("接入线程异常：{e}"))?;
    ais_log(
        &app,
        &format!(
            "接入 ok={} set_default={} 模型数={} 配置目录={} msg={}",
            r.ok,
            set_default,
            r.models.len(),
            ais_codex::config_home_display(),
            r.message
        ),
    );
    Ok(r)
}

#[tauri::command]
async fn ais_switch_refresh(app: tauri::AppHandle) -> Result<ais_codex::AisActionResult, String> {
    let r = tauri::async_runtime::spawn_blocking(ais_codex::refresh)
        .await
        .map_err(|e| format!("刷新线程异常：{e}"))?;
    ais_log(
        &app,
        &format!(
            "刷新 ok={} 模型数={} 配置目录={} msg={}",
            r.ok,
            r.models.len(),
            ais_codex::config_home_display(),
            r.message
        ),
    );
    Ok(r)
}

#[tauri::command]
async fn ais_switch_remove(app: tauri::AppHandle) -> Result<ais_codex::AisActionResult, String> {
    let r = tauri::async_runtime::spawn_blocking(ais_codex::remove)
        .await
        .map_err(|e| format!("移除线程异常：{e}"))?;
    ais_log(
        &app,
        &format!(
            "移除 ok={} 配置目录={} msg={}",
            r.ok,
            ais_codex::config_home_display(),
            r.message
        ),
    );
    Ok(r)
}

#[tauri::command]
async fn ais_switch_open_app(app: tauri::AppHandle) -> Result<ais_codex::AisActionResult, String> {
    let r = tauri::async_runtime::spawn_blocking(ais_codex::open_app)
        .await
        .map_err(|e| format!("打开 AIS Switch 线程异常：{e}"))?;
    ais_log(&app, &format!("打开 AIS Switch ok={} msg={}", r.ok, r.message));
    Ok(r)
}

// 打开独立的用量统计窗口（单例）
fn open_usage_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("usage") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "usage", WebviewUrl::App("usage.html".into()))
        .title("用量统计")
        .inner_size(960.0, 700.0)
        .min_inner_size(720.0, 520.0)
        .build();
}

// 打开版本变更窗口（单例）
fn open_changelog_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("changelog") {
        let _ = w.show();
        let _ = w.set_focus();
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "changelog", WebviewUrl::App("changelog.html".into()))
        .title("版本变更")
        .inner_size(760.0, 700.0)
        .min_inner_size(640.0, 560.0)
        .build();
}

// ---------- dsh 版本升级 ----------

#[derive(Clone, serde::Serialize)]
struct DshUpgradeInfo {
    target: String,
    current: Option<String>,
    needs_upgrade: bool,
}

fn current_runtime_dsh_version(app: &tauri::AppHandle) -> Option<String> {
    if let Some(v) = app.state::<DshVersion>().0.lock().unwrap().clone() {
        return Some(v);
    }
    select_dsh().and_then(|p| run_dsh_version_cached(&p))
}

fn dsh_upgrade_info(app: &tauri::AppHandle) -> DshUpgradeInfo {
    let target = target_dsh_version().to_string();
    let current = current_runtime_dsh_version(app);
    let needs_upgrade = match &current {
        Some(c) => normalize_version(c) != target,
        None => true,
    };
    DshUpgradeInfo {
        target,
        current,
        needs_upgrade,
    }
}

// ---------- 升级提示「稍后再说」记忆 ----------

fn upgrade_dismiss_path(app: &tauri::AppHandle) -> Option<PathBuf> {
    app.path()
        .app_config_dir()
        .ok()
        .map(|d| d.join("dsh-upgrade-dismissed"))
}

fn read_dismissed_target(path: &Path) -> Option<String> {
    let s = std::fs::read_to_string(path).ok()?;
    let t = s.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

fn write_dismissed_target(path: &Path, target: &str) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    std::fs::write(path, target).map_err(|e| e.to_string())
}

fn load_dismissed_upgrade_target(app: &tauri::AppHandle) -> Option<String> {
    upgrade_dismiss_path(app).and_then(|p| read_dismissed_target(&p))
}

fn save_dismissed_upgrade_target(app: &tauri::AppHandle, target: &str) {
    if let Some(p) = upgrade_dismiss_path(app) {
        let _ = write_dismissed_target(&p, target);
    }
}

fn clear_dismissed_upgrade_target(app: &tauri::AppHandle) {
    if let Some(p) = upgrade_dismiss_path(app) {
        let _ = std::fs::remove_file(p);
    }
}

/// 仅当需要升级且该目标版本未被「稍后再说」忽略时，才自动弹窗。
/// 目标版本变了（DSH_PKG 更新）会重新提示。
fn should_auto_open_upgrade(app: &tauri::AppHandle) -> bool {
    let info = dsh_upgrade_info(app);
    if !info.needs_upgrade {
        return false;
    }
    load_dismissed_upgrade_target(app).as_deref() != Some(info.target.as_str())
}

#[tauri::command]
fn get_dsh_upgrade_info(app: tauri::AppHandle) -> DshUpgradeInfo {
    dsh_upgrade_info(&app)
}

#[derive(Clone, serde::Serialize)]
struct UpgradeResult {
    ok: bool,
    message: String,
    version: Option<String>,
}

/// 跑一键安装脚本并把输出落到 install.log，返回是否 exit 0。
fn run_install_script_logged(app: &tauri::AppHandle) -> bool {
    let script = build_install_script();
    let mut child = match Command::new("/bin/bash")
        .arg("-c")
        .arg(&script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .process_group(0)
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            emit_log(app, &format!("[upgrade] 启动安装进程失败: {e}"));
            return false;
        }
    };
    let pid = child.id() as i32;
    {
        let state = app.state::<InstallProcess>();
        *state.0.lock().unwrap() = Some(pid);
    }
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let app_out = app.clone();
    let t1 = thread::spawn(move || {
        if let Some(out) = stdout {
            for line in BufReader::new(out).lines().map_while(Result::ok) {
                emit_log(&app_out, &line);
            }
        }
    });
    let app_err = app.clone();
    let t2 = thread::spawn(move || {
        if let Some(err) = stderr {
            for line in BufReader::new(err).lines().map_while(Result::ok) {
                emit_log(&app_err, &line);
            }
        }
    });
    let deadline = Instant::now() + Duration::from_secs(INSTALL_TIMEOUT_SECS);
    let mut success = false;
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                success = status.success();
                break;
            }
            Ok(None) => {}
            Err(_) => break,
        }
        if Instant::now() > deadline {
            emit_log(app, "[upgrade] 安装超时，终止进程组");
            unsafe { libc::kill(-pid, libc::SIGKILL); }
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }
    {
        let state = app.state::<InstallProcess>();
        *state.0.lock().unwrap() = None;
    }
    let _ = t1.join();
    let _ = t2.join();
    success
}

/// 一键把本机 dsh 升到 DSH_PKG 固定版本，并重启子进程使其生效。
fn upgrade_dsh_impl(app: &tauri::AppHandle) -> UpgradeResult {
    let target = target_dsh_version().to_string();
    emit_log(app, &format!("[upgrade] 开始升级 dsh 到 {target} ..."));
    if !run_install_script_logged(app) {
        return UpgradeResult {
            ok: false,
            message: "升级安装失败，请到「环境检查 / 安装日志」查看，或手动执行安装命令。".into(),
            version: None,
        };
    }
    clear_dsh_caches();
    let Some(path) = select_dsh() else {
        return UpgradeResult {
            ok: false,
            message: "升级后未找到可用的 dsh，请检查 PATH / nvm。".into(),
            version: None,
        };
    };
    let Some(ver) = run_dsh_version_cached(&path) else {
        return UpgradeResult {
            ok: false,
            message: "升级后 dsh 无法运行（可能是 Node 版本不兼容）。".into(),
            version: None,
        };
    };
    if normalize_version(&ver) != target {
        return UpgradeResult {
            ok: false,
            message: format!(
                "升级后版本为 {ver}，仍不等于目标 {target}。可能是候选路径里旧版优先，请手动 npm i -g 到当前 Node 的 prefix。"
            ),
            version: Some(ver),
        };
    }

    // 杀掉当前子进程；monitor_dsh 会用同一路径（npm -g 原地覆盖）在同端口拉起新版本
    {
        let state = app.state::<DshProcess>();
        let mut guard = state.0.lock().unwrap();
        if let Some(c) = guard.as_mut() {
            let _ = c.kill();
        }
    }
    let port = app
        .state::<DshPort>()
        .0
        .lock()
        .unwrap()
        .unwrap_or(PORT);
    if !wait_for_port_free_then_up(app, port) {
        return UpgradeResult {
            ok: false,
            message: format!("dsh {ver} 已安装，但进程重启超时，请手动重启应用。"),
            version: Some(ver),
        };
    }
    *app.state::<DshVersion>().0.lock().unwrap() = Some(ver.clone());
    inject_version_badge(app, &ver);
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.set_title(&format!("DeepSeek Harness · dsh {ver}"));
    }
    clear_dismissed_upgrade_target(app);
    emit_log(app, &format!("[upgrade] 升级完成，运行版本 {ver}"));
    UpgradeResult {
        ok: true,
        message: format!("dsh 已升级到 {ver}，并已重启生效。"),
        version: Some(ver),
    }
}

/// kill 后等端口释放再被 monitor 拉起：先等旧端口空出，再等新进程监听上。
fn wait_for_port_free_then_up(_app: &tauri::AppHandle, port: u16) -> bool {
    let start = Instant::now();
    // 等旧进程真正退出（端口释放），最多 5s
    while start.elapsed() < Duration::from_secs(5) {
        if !port_in_use(HOST, port) {
            break;
        }
        thread::sleep(Duration::from_millis(150));
    }
    // monitor_dsh 轮询间隔 1s + 启动最多 20s
    wait_for_port(HOST, port, Duration::from_secs(25))
}

#[tauri::command]
async fn upgrade_dsh(app: tauri::AppHandle) -> Result<UpgradeResult, String> {
    tauri::async_runtime::spawn_blocking(move || upgrade_dsh_impl(&app))
        .await
        .map_err(|e| format!("升级线程异常：{e}"))
}

#[tauri::command]
fn close_upgrade_window(app: tauri::AppHandle, dismiss: bool) {
    if dismiss {
        let target = target_dsh_version().to_string();
        save_dismissed_upgrade_target(&app, &target);
        emit_log(&app, &format!("[upgrade] 已忽略对目标版本 {target} 的自动升级提示（菜单仍可手动打开）"));
    }
    if let Some(w) = app.get_webview_window("upgrade") {
        let _ = w.close();
    }
}

fn open_upgrade_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("upgrade") {
        let _ = w.show();
        let _ = w.set_focus();
        let _ = w.eval("window.__upgradeRefresh && window.__upgradeRefresh()");
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "upgrade", WebviewUrl::App("upgrade.html".into()))
        .title("升级 dsh")
        .inner_size(520.0, 420.0)
        .min_inner_size(440.0, 360.0)
        .build();
}

// 一键重启 dsh 使配置生效：杀掉子进程，等 monitor_dsh 在同端口自动拉起，并刷新主窗口
#[tauri::command]
async fn ais_switch_restart_dsh(app: tauri::AppHandle) -> Result<ais_codex::AisActionResult, String> {
    tauri::async_runtime::spawn_blocking(move || restart_dsh(&app))
        .await
        .map_err(|e| format!("重启线程异常：{e}"))
}

fn restart_dsh(app: &tauri::AppHandle) -> ais_codex::AisActionResult {
    let result = restart_dsh_inner(app);
    ais_log(
        app,
        &format!(
            "重启 dsh ok={} 配置目录={} msg={}",
            result.ok,
            ais_codex::config_home_display(),
            result.message
        ),
    );
    result
}

fn restart_dsh_inner(app: &tauri::AppHandle) -> ais_codex::AisActionResult {
    let port = match *app.state::<DshPort>().0.lock().unwrap() {
        Some(p) => p,
        None => {
            return ais_codex::AisActionResult {
                ok: false,
                message: "dsh 尚未启动，无需重启。".into(),
                models: vec![],
            }
        }
    };
    let had_child = {
        let state = app.state::<DshProcess>();
        let mut guard = state.0.lock().unwrap();
        match guard.as_mut() {
            Some(c) => {
                let _ = c.kill();
                true
            }
            None => false,
        }
    };
    if !had_child {
        return ais_codex::AisActionResult {
            ok: false,
            message: "未找到运行中的 dsh 进程（可能在重启中），稍后再试。".into(),
            models: vec![],
        };
    }
    if !wait_for_port(HOST, port, Duration::from_secs(25)) {
        return ais_codex::AisActionResult {
            ok: false,
            message: "dsh 重启超时，请到「环境检查 / 安装日志」查看。".into(),
            models: vec![],
        };
    }
    // 新版 dsh 每次启动都会生成新 token，start_dsh_with_retry 已导航到新 URL。
    // 导航会清掉版本角标，等页面加载后重注入。
    if let Some(ver) = app.state::<DshVersion>().0.lock().unwrap().clone() {
        inject_version_badge(app, &ver);
    }
    ais_codex::AisActionResult {
        ok: true,
        message: "dsh 已重启，新配置已生效。".into(),
        models: vec![],
    }
}

fn open_ais_window(app: &tauri::AppHandle) {
    if let Some(w) = app.get_webview_window("ais") {
        let _ = w.show();
        let _ = w.set_focus();
        // 再次打开时自动复检
        let _ = w.eval("window.__aisRecheck && window.__aisRecheck()");
        return;
    }
    let _ = WebviewWindowBuilder::new(app, "ais", WebviewUrl::App("ais.html".into()))
        .title("AIS Switch")
        .inner_size(560.0, 640.0)
        .min_inner_size(480.0, 480.0)
        .build();
}

fn build_app_menu(app: &tauri::App) -> Result<tauri::menu::Menu<tauri::Wry>, Box<dyn std::error::Error>> {
    // macOS 顶栏菜单；不要往 Menu 根上直接塞 MenuItem（不容易看见）
    let changelog_item = MenuItem::with_id(app, "version_changelog", "版本变更…", true, None::<&str>)?;
    let usage_item = MenuItem::with_id(app, "usage_stats", "用量统计", true, None::<&str>)?;
    let ais_item = MenuItem::with_id(app, "ais_switch", "AIS Switch…", true, None::<&str>)?;
    let upgrade_item = MenuItem::with_id(app, "upgrade_dsh", "升级 dsh…", true, None::<&str>)?;
    let tools = Submenu::with_id_and_items(
        app,
        "tools",
        "工具",
        true,
        &[&changelog_item, &usage_item, &ais_item, &upgrade_item],
    )?;
    let menu = Menu::default(app.handle())?;
    menu.append(&tools)?;
    Ok(menu)
}

// 在 dsh web 页面注入常驻版本角标（自修复：应对 SPA 重渲染；reload 后需重新注入）
fn inject_version_badge(app: &tauri::AppHandle, ver: &str) {
    let ver = ver.to_string();
    let app2 = app.clone();
    std::thread::spawn(move || {
        // 给首次窗口一个缓冲；注入脚本内部会等页面 ready 后再落 DOM，
        // 并用递归 setTimeout 持续维持，reload 后重新调用也能可靠补上
        std::thread::sleep(Duration::from_millis(1200));
        let app_for_closure = app2.clone();
        let _ = app2.run_on_main_thread(move || {
            if let Some(w) = app_for_closure.get_webview_window("main") {
                let badge_js = [
                    "(function(){function step(){",
                    "var b=document.getElementById('dsh-version-badge');",
                    "if(!b){",
                    "if(!document.body||document.readyState==='loading'){setTimeout(step,400);return;}",
                    "b=document.createElement('div');b.id='dsh-version-badge';",
                    "b.textContent='dsh ", ver.as_str(), "';",
                    "b.style.cssText='position:fixed;right:10px;bottom:10px;z-index:2147483647;",
                    "background:rgba(15,23,42,.82);color:#e2e8f0;",
                    "font:12px -apple-system,BlinkMacSystemFont,sans-serif;padding:4px 9px;",
                    "border-radius:6px;pointer-events:none;box-shadow:0 1px 4px rgba(0,0,0,.35)';",
                    "document.body.appendChild(b);}",
                    "setTimeout(step,5000);}",
                    "step();})();",
                ]
                .concat();
                let _ = w.eval(badge_js);
            }
        });
    });
}

fn launch_main(app: &tauri::AppHandle) {
    let dsh_path = select_dsh();
    let mut nav_port = PORT;

    if let Some(path) = &dsh_path {
        let port = find_free_port(PORT);
        nav_port = port;
        *app.state::<DshPort>().0.lock().unwrap() = Some(port);
        if start_dsh_with_retry(app, path, port, 3) {
            let app_clone = app.clone();
            let path_clone = path.clone();
            thread::spawn(move || monitor_dsh(app_clone, path_clone, port));
        } else {
            emit_log(app, "[dsh] 多次启动失败，dsh web 可能无法正常使用");
        }
    }

    // 捕获真实运行的 dsh 版本，用于在界面上显示，方便确认当前版本
    let dsh_version = dsh_path
        .as_ref()
        .and_then(run_dsh_version_cached)
        .unwrap_or_else(|| "未知".to_string());

    let win = WebviewWindowBuilder::new(
        app,
        "main",
        WebviewUrl::External(current_dsh_web_url(app).parse().unwrap()),
    )
    .title(format!("DeepSeek Harness · dsh {dsh_version}"))
    .inner_size(1280.0, 820.0)
    .on_navigation(move |url| {
        navigation_allowed(url.scheme(), url.host_str(), url.port(), nav_port)
    })
    .build();

    // 在 dsh web 页面注入常驻角标，显示当前 dsh 版本（自修复：应对 SPA 重渲染）
    *app.state::<DshVersion>().0.lock().unwrap() = Some(dsh_version.clone());
    if let Ok(_win) = win {
        inject_version_badge(app, &dsh_version);
    }
}

// ---------- 主流程 ----------

fn main() {
    tauri::Builder::default()
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.unminimize();
                let _ = win.set_focus();
            } else if let Some(win) = app.get_webview_window("precheck") {
                let _ = win.set_focus();
            }
        }))
        .manage(DshProcess(Mutex::new(None)))
        .manage(InstallProcess(Mutex::new(None)))
        .manage(QuitFlag(AtomicBool::new(false)))
        .manage(DshPort(Mutex::new(None)))
        .manage(DshWebUrl(Mutex::new(None)))
        .manage(DshVersion(Mutex::new(None)))
        .manage(UsageCache::default())
        .manage(LogFile(Mutex::new(None)))
        .invoke_handler(tauri::generate_handler![
            get_detect_result,
            get_version_info,
            install_dsh,
            cancel_precheck,
            get_usage_stats,
            ais_switch_check,
            ais_switch_setup,
            ais_switch_refresh,
            ais_switch_remove,
            ais_switch_open_app,
            ais_switch_restart_dsh,
            get_dsh_upgrade_info,
            upgrade_dsh,
            close_upgrade_window
        ])
        .setup(|app| {
            // 菜单栏加入「用量统计」入口
            if let Ok(menu) = build_app_menu(app) {
                let _ = app.set_menu(menu);
            }
            app.on_menu_event(|app, event| {
                match event.id().as_ref() {
                    "version_changelog" => open_changelog_window(app),
                    "usage_stats" => open_usage_window(app),
                    "ais_switch" => open_ais_window(app),
                    "upgrade_dsh" => open_upgrade_window(app),
                    _ => {}
                }
            });

            let result = detect_environment();
            if result.ok {
                launch_main(app.handle());
                // 版本不一致且该目标未被「稍后再说」时才自动弹；菜单入口不受影响
                if should_auto_open_upgrade(app.handle()) {
                    open_upgrade_window(app.handle());
                }
            } else {
                let _ = WebviewWindowBuilder::new(app, "precheck", WebviewUrl::App("index.html".into()))
                    .title("DeepSeek Harness 环境检查")
                    .inner_size(680.0, 560.0)
                    .build();
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app_handle, event| {
            if let tauri::RunEvent::Exit = event {
                app_handle.state::<QuitFlag>().0.store(true, Ordering::SeqCst);
                kill_install_group(app_handle);
                let state = app_handle.state::<DshProcess>();
                let child = state.0.lock().unwrap().take();
                if let Some(mut child) = child {
                    let _ = child.kill();
                }
            }
        });
}

#[cfg(test)]
mod tests {
    use super::{
        dsh_url_from_output, navigation_allowed, normalize_version, read_dismissed_target,
        target_dsh_version, version_info, write_dismissed_target,
    };

    #[test]
    fn extracts_tokenized_web_url_for_expected_port() {
        let line = "dsh web: http://127.0.0.1:3080/?token=abc-123\n";
        assert_eq!(
            dsh_url_from_output(line, 3080),
            Some("http://127.0.0.1:3080/?token=abc-123".to_string())
        );
    }

    #[test]
    fn ignores_bare_or_wrong_port_urls() {
        assert_eq!(dsh_url_from_output("http://127.0.0.1:3080/", 3080), None);
        assert_eq!(
            dsh_url_from_output("dsh web: http://127.0.0.1:3081/?token=abc", 3080),
            None
        );
    }

    #[test]
    fn navigation_only_allows_same_http_port() {
        assert!(navigation_allowed("http", Some("127.0.0.1"), Some(3080), 3080));
        // 其它端口 / 协议 / 主机一律拒绝，避免跳到同机其它服务
        assert!(!navigation_allowed("http", Some("127.0.0.1"), Some(3081), 3080));
        assert!(!navigation_allowed("http", Some("127.0.0.1"), Some(80), 3080));
        assert!(!navigation_allowed("https", Some("127.0.0.1"), Some(3080), 3080));
        assert!(!navigation_allowed("http", Some("example.com"), Some(3080), 3080));
        assert!(!navigation_allowed("http", None, Some(3080), 3080));
        assert!(!navigation_allowed("http", Some("127.0.0.1"), None, 3080));
    }

    #[test]
    fn target_version_is_plain_semver_without_pkg_prefix() {
        let t = target_dsh_version();
        assert!(!t.contains('@'), "target 不应含包名分隔符: {t}");
        assert!(t.contains('.'), "target 应是版本号: {t}");
        assert_eq!(normalize_version("  v0.1.6-alpha.1\n"), "0.1.6-alpha.1");
        assert_eq!(normalize_version(t), t);
    }

    #[test]
    fn dismissed_upgrade_target_roundtrip() {
        let dir = std::env::temp_dir().join(format!("dsh-dismiss-test-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("dsh-upgrade-dismissed");
        let _ = std::fs::remove_file(&path);

        assert_eq!(read_dismissed_target(&path), None);

        write_dismissed_target(&path, "0.1.6-alpha.1").unwrap();
        assert_eq!(
            read_dismissed_target(&path).as_deref(),
            Some("0.1.6-alpha.1")
        );

        // 目标版本变化后旧忽略记录不应命中
        assert_ne!(
            read_dismissed_target(&path).as_deref(),
            Some("0.1.7-alpha.1")
        );

        let _ = std::fs::remove_file(&path);
        assert_eq!(read_dismissed_target(&path), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn version_change_page_documents_current_versions() {
        let page = include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/../precheck/changelog.html"));
        let info = version_info();
        assert!(page.contains("版本变更"));
        assert!(page.contains(&info.target_dsh));
        assert!(page.contains("DeepSeek Harness 上游更新"));
        assert!(page.contains("0.1.6-alpha.1 新增能力"));
        assert!(page.contains("0.1.5-rc.2"));
        assert!(page.contains("DeepSeek-V41-Flash"));
        assert!(page.contains("通用文件"));
        assert!(page.contains("在应用中打开"));
        assert!(page.contains("Session V3"));
        assert!(page.contains("get_version_info"));
        // 1.0.15：一键升级 + stderr/导航收紧
        assert!(page.contains("一键升级"));
        assert!(page.contains("stderr"));
        // 1.0.16：升级提示可忽略 + 余额 key 不进 argv
        assert!(page.contains("稍后再说"));
        assert!(page.contains("不进进程参数"));
    }
}
