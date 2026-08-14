#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::fs::OpenOptions;
use std::io::{BufRead, BufReader, Write};
use std::net::TcpStream;
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use tauri::{Emitter, Manager, WebviewUrl, WebviewWindowBuilder};

const HOST: &str = "127.0.0.1";
const PORT: u16 = 3080;
const NPM_REGISTRY: &str = "https://registry.npmjs.org";
const NPM_MIRROR: &str = "https://registry.npmmirror.com";
// 固定已验证的 dsh 版本，避免上游发破坏性新版本
const DSH_PKG: &str = "@deepseek-ai/dsh@0.1.0-rc.6";
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

struct DshProcess(Mutex<Option<Child>>);
struct InstallProcess(Mutex<Option<i32>>); // 安装进程组 pid
struct QuitFlag(AtomicBool); // 是否正在退出，用于抑制崩溃自动重启

// ---------- 日志 ----------

fn log_to_file(app: &tauri::AppHandle, line: &str) {
    if let Ok(dir) = app.path().app_log_dir() {
        if std::fs::create_dir_all(&dir).is_ok() {
            if let Ok(mut f) = OpenOptions::new()
                .create(true)
                .append(true)
                .open(dir.join("install.log"))
            {
                let _ = writeln!(f, "{}", line);
            }
        }
    }
}

fn emit_log(app: &tauri::AppHandle, line: &str) {
    log_to_file(app, line);
    let _ = app.emit("install_log", format!("{line}\n"));
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

fn manual_install_cmd() -> String {
    format!(
        r#"export NVM_DIR="$HOME/.nvm"
[ -s "$NVM_DIR/nvm.sh" ] && . "$NVM_DIR/nvm.sh"
nvm install 22
nvm use 22
mkdir -p "$HOME/.local/npm-cache"
export npm_config_cache="$HOME/.local/npm-cache"
npm i -g {dsh_pkg} --registry={registry}"#,
        dsh_pkg = DSH_PKG,
        registry = NPM_REGISTRY,
    )
}

fn detect_environment() -> DetectResult {
    let candidates = find_dsh_candidates();
    let mut problems = Vec::new();

    if candidates.is_empty() {
        problems.push(Problem {
            problem_type: "dsh-missing".into(),
            title: "未检测到 DeepSeek Harness (dsh)".into(),
            detail: format!("需要 Node ≥ 22.12 以及 {}", DSH_PKG),
        });
        return DetectResult {
            ok: false,
            problems,
            install_cmd: manual_install_cmd(),
        };
    }

    let mut found = false;
    for c in &candidates {
        if run_dsh_version(c).is_some() {
            found = true;
            break;
        }
    }

    if !found {
        problems.push(Problem {
            problem_type: "node-incompatible".into(),
            title: "dsh 已安装但无法运行".into(),
            detail: format!(
                "通常是 Node 版本过低（dsh 需要 Node ≥ 20.19 / 22.12）。当前 Node: {}。建议安装 Node 22。",
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

if npm i -g "$DSH_PKG" --registry="$REG" 2>/dev/null; then
  log "官方 registry 安装成功"
elif npm i -g "$DSH_PKG" --registry="$MIRROR" 2>/dev/null; then
  log "npmmirror 镜像安装成功"
else
  log "全局安装失败，fallback 到 --prefix=$HOME/.local（npmmirror）"
  npm i -g --prefix="$HOME/.local" "$DSH_PKG" --registry="$MIRROR"
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
        .args(["web", "--host", HOST, "--port", &port.to_string()])
        .env("PATH", path_env)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| e.to_string())
}

// 带重试地启动 dsh web（固定端口，失败/超时则重试 attempts 次）
fn start_dsh_with_retry(app: &tauri::AppHandle, dsh_path: &PathBuf, port: u16, attempts: u32) -> bool {
    for i in 1..=attempts {
        match start_dsh(dsh_path, port) {
            Ok(child) => {
                {
                    let state = app.state::<DshProcess>();
                    *state.0.lock().unwrap() = Some(child);
                }
                if wait_for_port(HOST, port, Duration::from_secs(20)) {
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
            thread::sleep(Duration::from_millis(300));
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

fn launch_main(app: &tauri::AppHandle) {
    let dsh_path = find_dsh_candidates()
        .into_iter()
        .find(|c| run_dsh_version(c).is_some());

    let mut port = PORT;
    if let Some(path) = dsh_path {
        port = find_free_port(PORT);
        if start_dsh_with_retry(app, &path, port, 3) {
            let app_clone = app.clone();
            let path_clone = path.clone();
            thread::spawn(move || monitor_dsh(app_clone, path_clone, port));
        } else {
            emit_log(app, "[dsh] 多次启动失败，dsh web 可能无法正常使用");
        }
    }

    let _ = WebviewWindowBuilder::new(
        app,
        "main",
        WebviewUrl::External(format!("http://{HOST}:{port}").parse().unwrap()),
    )
    .title("DeepSeek Harness")
    .inner_size(1280.0, 820.0)
    .on_navigation(|url| url.host_str() == Some(HOST))
    .build();
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
        .invoke_handler(tauri::generate_handler![
            get_detect_result,
            install_dsh,
            cancel_precheck
        ])
        .setup(|app| {
            let result = detect_environment();
            if result.ok {
                launch_main(app.handle());
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
