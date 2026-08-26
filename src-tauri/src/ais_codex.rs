//! AIS Switch Codex 本地代理 ↔ dsh 配置。逻辑与 scripts/setup-ais-codex.py 对齐。
//! 蹭 Codex 通道，非官方支持。HTTP 走系统 curl，不新增依赖。

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

const DEFAULT_PROXY: &str = "http://127.0.0.1:15721";
const PROVIDER_ID: &str = "ais-codex";
const API_KEY_ENV: &str = "AIS_CODEX_API_KEY";
const API_KEY_VALUE: &str = "local";
const DISPLAY_NAME: &str = "AIS Switch (Codex)";
const GATEWAY_PREFIX: &str = "llm-gateway--";
const DEFAULT_CONTEXT: &str = "1000000";
const DEFAULT_MAX_TOKENS: &str = "131072";
/// 接入前 `agent-default-model` 原始段落的备份文件名（放 ~/.dsh 下），移除时原样还原。
const DEFAULT_MODEL_BACKUP: &str = ".ais-default-model.bak";

/// 代理基址：可用 `AIS_SWITCH_PROXY` 环境变量覆盖（与命令行脚本一致），默认 127.0.0.1:15721。
fn proxy() -> String {
    std::env::var("AIS_SWITCH_PROXY").unwrap_or_else(|_| DEFAULT_PROXY.to_string())
}

fn base_url() -> String {
    format!("{}/codex/v1", proxy())
}

#[derive(Clone, serde::Serialize)]
pub struct AisStep {
    pub ok: bool,
    pub name: String,
    pub detail: String,
}

#[derive(Clone, serde::Serialize)]
pub struct AisCheckResult {
    pub ok: bool,
    pub steps: Vec<AisStep>,
    pub models: Vec<String>,
    pub has_provider: bool,
    pub has_key: bool,
    pub reply: Option<String>,
    /// 本地 15721 连不上（AIS Switch 未开 / Codex 路由关）
    pub proxy_down: bool,
}

#[derive(Clone, serde::Serialize)]
pub struct AisActionResult {
    pub ok: bool,
    pub message: String,
    pub models: Vec<String>,
}

struct GatewayModel {
    id: String,
    name: String,
}

fn dsh_home() -> Result<PathBuf, String> {
    if let Ok(h) = std::env::var("DSH_HOME") {
        return Ok(PathBuf::from(h));
    }
    let home = std::env::var("HOME").map_err(|_| "无 HOME".to_string())?;
    Ok(PathBuf::from(home).join(".dsh"))
}

fn settings_path() -> Result<PathBuf, String> {
    Ok(dsh_home()?.join("settings.yaml"))
}

/// 当前 dsh 配置目录（用于日志排障），解析失败时返回占位文案。
pub fn config_home_display() -> String {
    match dsh_home() {
        Ok(p) => p.display().to_string(),
        Err(e) => format!("<解析失败: {e}>"),
    }
}

fn credentials_path() -> Result<PathBuf, String> {
    Ok(dsh_home()?.join(".credentials.yaml"))
}

fn read_or_empty(path: &Path) -> String {
    fs::read_to_string(path).unwrap_or_default()
}

/// 跨进程互斥锁（与 Python 脚本共用 ~/.dsh/.ais-codex.lock），
/// 防止 App 与命令行脚本同时读写 settings.yaml / .credentials.yaml。
fn with_config_lock<T>(f: impl FnOnce() -> Result<T, String>) -> Result<T, String> {
    let lock_path = dsh_home()?.join(".ais-codex.lock");
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|e| format!("无法创建配置锁 {}: {e}", lock_path.display()))?;
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_EX);
    }
    let result = f();
    unsafe {
        libc::flock(file.as_raw_fd(), libc::LOCK_UN);
    }
    result
}

fn write_with_backup(path: &Path, content: &str, mode: Option<u32>) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let backup = path.with_file_name(format!(
        "{}.bak-ais-codex",
        path.file_name().and_then(|s| s.to_str()).unwrap_or("file")
    ));
    if path.exists() && !backup.exists() {
        let old = fs::read_to_string(path).unwrap_or_default();
        fs::write(&backup, old).map_err(|e| e.to_string())?;
        if let Some(m) = mode {
            let _ = fs::set_permissions(&backup, fs::Permissions::from_mode(m));
        }
    }
    // 原子写：先写同目录临时文件再 rename，避免 dsh 的文件监听(chokidar)读到半份内容
    let fname = path.file_name().and_then(|s| s.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!("{fname}.tmp-ais-codex"));
    fs::write(&tmp, content).map_err(|e| e.to_string())?;
    if let Some(m) = mode {
        fs::set_permissions(&tmp, fs::Permissions::from_mode(m)).map_err(|e| e.to_string())?;
    } else if let Ok(meta) = fs::metadata(path) {
        // rename 不会继承旧文件权限，覆盖已有文件时保留原权限
        let _ = fs::set_permissions(&tmp, meta.permissions());
    }
    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        e.to_string()
    })?;
    Ok(())
}

fn curl_body(args: &[&str], timeout: Duration) -> Result<(u32, String), String> {
    let secs = timeout.as_secs().max(1).to_string();
    let mut cmd = Command::new("curl");
    cmd.args(["-sS", "-m", &secs, "-w", "\n__HTTP__%{http_code}"]);
    cmd.args(args);
    let out = cmd.output().map_err(|e| format!("无法执行 curl: {e}"))?;
    let raw = String::from_utf8_lossy(&out.stdout);
    let (body, status) = match raw.rfind("\n__HTTP__") {
        Some(i) => {
            let code = raw[i + 9..].trim().parse::<u32>().unwrap_or(0);
            (raw[..i].to_string(), code)
        }
        None => (raw.to_string(), if out.status.success() { 200 } else { 0 }),
    };
    if status == 0 && body.is_empty() && !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        let err_l = err.to_lowercase();
        // curl (7) 文案大小写因版本而异：Couldn't / couldn't / Failed to connect…
        if err_l.contains("connection refused")
            || err_l.contains("couldn't connect")
            || err_l.contains("could not connect")
            || err_l.contains("failed to connect")
        {
            return Err("connection_refused".into());
        }
        return Err(if err.trim().is_empty() {
            "curl 失败".into()
        } else {
            err.trim().to_string()
        });
    }
    Ok((status, body))
}

fn curl_json_get(url: &str, timeout: Duration) -> Result<(u32, serde_json::Value), String> {
    let (status, body) = curl_body(
        &["-H", "Authorization: Bearer dummy", url],
        timeout,
    )?;
    parse_json_body(status, &body)
}

fn curl_json_post(url: &str, payload: &str, timeout: Duration) -> Result<(u32, serde_json::Value), String> {
    let (status, body) = curl_body(
        &[
            "-X",
            "POST",
            "-H",
            "Authorization: Bearer dummy",
            "-H",
            "Content-Type: application/json",
            "-d",
            payload,
            url,
        ],
        timeout,
    )?;
    parse_json_body(status, &body)
}

fn parse_json_body(status: u32, body: &str) -> Result<(u32, serde_json::Value), String> {
    if body.trim().is_empty() {
        return Ok((status, serde_json::Value::Null));
    }
    match serde_json::from_str::<serde_json::Value>(body) {
        Ok(v) => Ok((status, v)),
        Err(_) => Ok((status, serde_json::Value::String(body.chars().take(400).collect()))),
    }
}

fn parse_gateway_models(v: &serde_json::Value) -> (Vec<GatewayModel>, Vec<String>) {
    let mut all = Vec::new();
    let mut gw = Vec::new();
    let Some(rows) = v.get("data").and_then(|d| d.as_array()) else {
        return (gw, all);
    };
    let mut seen = std::collections::HashSet::new();
    for row in rows {
        let Some(id) = row.get("id").and_then(|x| x.as_str()) else {
            continue;
        };
        all.push(id.to_string());
        if !id.starts_with(GATEWAY_PREFIX) || !seen.insert(id.to_string()) {
            continue;
        }
        let name = row
            .get("cc_switch")
            .and_then(|c| c.get("upstream_model"))
            .and_then(|x| x.as_str())
            .unwrap_or_else(|| id.strip_prefix(GATEWAY_PREFIX).unwrap_or(id))
            .to_string();
        gw.push(GatewayModel {
            id: id.to_string(),
            name,
        });
    }
    (gw, all)
}

fn pick_default_model(models: &[GatewayModel]) -> String {
    models
        .iter()
        .find(|m| m.id.ends_with("deepseek-v4-flash"))
        .or(models.first())
        .map(|m| m.id.clone())
        .unwrap_or_default()
}

fn has_provider(settings: &str) -> bool {
    settings.lines().any(|l| l.starts_with(&format!("    {PROVIDER_ID}:")))
}

fn has_key(cred: &str) -> bool {
    cred.lines().any(|l| l.trim_start().starts_with(&format!("{API_KEY_ENV}:")))
}

fn step(ok: bool, name: &str, detail: impl Into<String>) -> AisStep {
    AisStep {
        ok,
        name: name.into(),
        detail: detail.into(),
    }
}

pub fn check() -> AisCheckResult {
    let mut steps = Vec::new();
    let mut ok = true;
    let mut models = Vec::new();
    let mut reply = None;
    let mut proxy_down = false;

    match curl_json_get(&format!("{}/health", proxy()), Duration::from_secs(5)) {
        Ok((200, _)) => steps.push(step(true, "/health", "ok")),
        Ok((s, v)) => {
            ok = false;
            steps.push(step(false, "/health", format!("HTTP {s}: {v}")));
        }
        Err(e) if e == "connection_refused" => {
            return AisCheckResult {
                ok: false,
                steps: vec![step(
                    false,
                    "/health",
                    format!("连不上 {}。请打开 AIS Switch，并打开 Codex 路由总开关。", proxy()),
                )],
                models,
                has_provider: has_provider(&read_or_empty(&settings_path().unwrap_or_default())),
                has_key: has_key(&read_or_empty(&credentials_path().unwrap_or_default())),
                reply: None,
                proxy_down: true,
            };
        }
        Err(e) => {
            ok = false;
            // 超时 / DNS / 其它 curl 错误也算连不上本地代理，引导用户打开 AIS Switch
            proxy_down = true;
            steps.push(step(false, "/health", e));
        }
    }

    match curl_json_get(&format!("{}/status", proxy()), Duration::from_secs(5)) {
        Ok((200, v)) => {
            let running = v.get("running").and_then(|x| x.as_bool()).unwrap_or(false);
            let port = v.get("port").and_then(|x| x.as_u64()).unwrap_or(0);
            if running {
                steps.push(step(true, "/status", format!("running port={port}")));
            } else {
                ok = false;
                steps.push(step(false, "/status", "Codex 代理未 running，请打开 Codex 路由"));
            }
        }
        Ok((s, v)) => {
            ok = false;
            steps.push(step(false, "/status", format!("HTTP {s}: {v}")));
        }
        Err(e) => {
            ok = false;
            steps.push(step(false, "/status", e));
        }
    }

    let gw = match curl_json_get(&format!("{}/codex/v1/models", proxy()), Duration::from_secs(8)) {
        Ok((200, v)) => {
            let (gw, all) = parse_gateway_models(&v);
            if gw.is_empty() {
                ok = false;
                let mut d = format!("共 {} 个模型，没有 llm-gateway--*。请把 LLM Gateway 加入 Codex 路由池并刷新。", all.len());
                if all.iter().any(|i| i.contains('@') || i.contains("npt.sg--")) {
                    d.push_str(" 当前像是只有个人 ChatGPT 路由。");
                }
                steps.push(step(false, "/models", d));
            } else {
                steps.push(step(true, "/models", format!("Gateway {} / 共 {}", gw.len(), all.len())));
                models = gw.iter().map(|m| m.id.clone()).collect();
            }
            gw
        }
        Ok((s, v)) => {
            ok = false;
            steps.push(step(false, "/models", format!("HTTP {s}: {v}")));
            Vec::new()
        }
        Err(e) => {
            ok = false;
            steps.push(step(false, "/models", e));
            Vec::new()
        }
    };

    if !gw.is_empty() {
        let mid = pick_default_model(&gw);
        let payload = format!(
            r#"{{"model":"{mid}","messages":[{{"role":"user","content":"ping"}}],"stream":false}}"#
        );
        match curl_json_post(
            &format!("{}/codex/v1/chat/completions", proxy()),
            &payload,
            Duration::from_secs(30),
        ) {
            Ok((200, v)) => {
                let text = v
                    .get("choices")
                    .and_then(|c| c.get(0))
                    .and_then(|c| c.get("message"))
                    .and_then(|m| m.get("content"))
                    .and_then(|c| c.as_str())
                    .unwrap_or("")
                    .chars()
                    .take(80)
                    .collect::<String>();
                reply = Some(text.clone());
                steps.push(step(true, "chat/completions", format!("HTTP 200 reply={text:?}")));
            }
            Ok((s, v)) => {
                ok = false;
                let provider = v
                    .pointer("/error/provider")
                    .and_then(|x| x.as_str())
                    .unwrap_or("");
                let msg = v
                    .pointer("/error/message")
                    .and_then(|x| x.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| v.to_string());
                let detail = if provider == "default" {
                    "请求打到了 Codex default（ChatGPT），不是 LLM Gateway。必须用 llm-gateway-- 前缀的模型 id。".to_string()
                } else {
                    format!("HTTP {s}: {msg}")
                };
                steps.push(step(false, "chat/completions", detail));
            }
            Err(e) => {
                ok = false;
                steps.push(step(false, "chat/completions", e));
            }
        }
    }

    let settings = read_or_empty(&settings_path().unwrap_or_default());
    let cred = read_or_empty(&credentials_path().unwrap_or_default());
    AisCheckResult {
        ok,
        steps,
        models,
        has_provider: has_provider(&settings),
        has_key: has_key(&cred),
        reply,
        proxy_down,
    }
}

/// 打开本机 `/Applications/AIS Switch.app`（macOS `open -a`）。
pub fn open_app() -> AisActionResult {
    let out = Command::new("open")
        .args(["-a", "AIS Switch"])
        .output();
    match out {
        Ok(o) if o.status.success() => AisActionResult {
            ok: true,
            message: "已打开 AIS Switch。请确认 Codex 路由已开，然后点「体检」。".into(),
            models: vec![],
        },
        Ok(o) => {
            let err = String::from_utf8_lossy(&o.stderr);
            AisActionResult {
                ok: false,
                message: if err.trim().is_empty() {
                    "打不开 AIS Switch，请确认已安装到 /Applications。".into()
                } else {
                    format!("打不开 AIS Switch：{}", err.trim())
                },
                models: vec![],
            }
        }
        Err(e) => AisActionResult {
            ok: false,
            message: format!("无法执行 open: {e}"),
            models: vec![],
        },
    }
}

pub fn setup(set_default: bool) -> AisActionResult {
    with_config_lock(|| {
        let gw = match fetch_gateway_or_err() {
            Ok(g) => g,
            Err(e) => return Ok(AisActionResult { ok: false, message: e, models: vec![] }),
        };
        let ids: Vec<String> = gw.iter().map(|m| m.id.clone()).collect();
        if let Err(e) = apply_setup(&gw, set_default) {
            return Ok(AisActionResult { ok: false, message: e, models: ids });
        }
        let extra = if set_default {
            " 已把默认模型切到 Gateway Flash。"
        } else {
            " 未改默认模型。"
        };
        Ok(AisActionResult {
            ok: true,
            message: format!(
                "已写入 ais-codex（{} 个模型）。保持 AIS Switch 开着，新开会话后选 llm-gateway--deepseek-v4-flash。{extra}",
                ids.len()
            ),
            models: ids,
        })
    })
    .unwrap_or_else(|e| AisActionResult { ok: false, message: e, models: vec![] })
}

pub fn refresh() -> AisActionResult {
    with_config_lock(|| {
        let settings = read_or_empty(&settings_path().unwrap_or_default());
        if !has_provider(&settings) {
            return Ok(AisActionResult {
                ok: false,
                message: "本地还没有 ais-codex，请先点「接入」。".into(),
                models: vec![],
            });
        }
        let gw = match fetch_gateway_or_err() {
            Ok(g) => g,
            Err(e) => return Ok(AisActionResult { ok: false, message: e, models: vec![] }),
        };
        let ids: Vec<String> = gw.iter().map(|m| m.id.clone()).collect();
        let next = upsert_ais_codex_provider(&settings, &gw);
        let sp = match settings_path() {
            Ok(p) => p,
            Err(e) => return Ok(AisActionResult { ok: false, message: e, models: ids }),
        };
        if let Err(e) = write_with_backup(&sp, &next, None) {
            return Ok(AisActionResult { ok: false, message: e, models: ids });
        }
        Ok(AisActionResult {
            ok: true,
            message: format!("已刷新 {} 个 Gateway 模型（未改默认模型 / 凭据）。新开会话后生效。", ids.len()),
            models: ids,
        })
    })
    .unwrap_or_else(|e| AisActionResult { ok: false, message: e, models: vec![] })
}

pub fn remove() -> AisActionResult {
    with_config_lock(|| {
        let sp = match settings_path() {
            Ok(p) => p,
            Err(e) => return Ok(AisActionResult { ok: false, message: e, models: vec![] }),
        };
        let cp = match credentials_path() {
            Ok(p) => p,
            Err(e) => return Ok(AisActionResult { ok: false, message: e, models: vec![] }),
        };
        let settings = restore_default_model(&remove_ais_codex_provider(&read_or_empty(&sp)));
        let cred = remove_credential(&read_or_empty(&cp));
        if sp.exists() || !settings.trim().is_empty() {
            if let Err(e) = write_with_backup(&sp, &settings, None) {
                return Ok(AisActionResult { ok: false, message: e, models: vec![] });
            }
        }
        if cp.exists() {
            let body = if cred.trim().is_empty() {
                "version: 1\nrefs:\n".to_string()
            } else {
                cred
            };
            if let Err(e) = write_with_backup(&cp, &body, Some(0o600)) {
                return Ok(AisActionResult { ok: false, message: e, models: vec![] });
            }
        }
        // 还原成功后再清掉备份，避免下次接入误用旧快照
        if let Ok(p) = default_model_backup_path() {
            let _ = fs::remove_file(p);
        }
        Ok(AisActionResult {
            ok: true,
            message: "已移除 ais-codex 与 AIS_CODEX_API_KEY。官方 DEEPSEEK_API_KEY 未动；默认模型已还原（若接入时设为默认）。新开会话后生效。".into(),
            models: vec![],
        })
    })
    .unwrap_or_else(|e| AisActionResult { ok: false, message: e, models: vec![] })
}

fn fetch_gateway_or_err() -> Result<Vec<GatewayModel>, String> {
    let (status, v) = curl_json_get(&format!("{}/codex/v1/models", proxy()), Duration::from_secs(8))
        .map_err(|e| {
            if e == "connection_refused" {
                format!("连不上 AIS Switch（{}）。请打开 AIS Switch 并打开 Codex 路由。", proxy()).into()
            } else {
                e
            }
        })?;
    if status != 200 {
        return Err(format!("/models HTTP {status}"));
    }
    let (gw, all) = parse_gateway_models(&v);
    if gw.is_empty() {
        return Err(format!(
            "没有 llm-gateway--* 模型（共 {} 个）。请在 Codex 页把 LLM Gateway 加入路由池并刷新。",
            all.len()
        ));
    }
    Ok(gw)
}

fn apply_setup(models: &[GatewayModel], set_default: bool) -> Result<(), String> {
    let sp = settings_path()?;
    let cp = credentials_path()?;
    let mut settings = upsert_ais_codex_provider(&read_or_empty(&sp), models);
    if set_default {
        // 先备份当前 agent-default-model，移除时原样还原
        save_default_model_backup(&settings)?;
        settings = upsert_default_model(&settings, &pick_default_model(models));
    }
    let cred = upsert_credential(&if cp.exists() {
        read_or_empty(&cp)
    } else {
        "version: 1\nrefs:\n".into()
    });
    write_with_backup(&sp, &settings, None)?;
    write_with_backup(&cp, &cred, Some(0o600))?;
    Ok(())
}

// ---------- YAML（与 Python 脚本同一套约定）----------

fn provider_yaml(models: &[GatewayModel]) -> String {
    let mut s = format!(
        "    {PROVIDER_ID}:\n      displayName: {DISPLAY_NAME}\n      api: openai-completions\n      baseURL: {}\n      apiKeyEnv: {API_KEY_ENV}\n      defaultContextWindow: {DEFAULT_CONTEXT}\n      defaultMaxTokens: {DEFAULT_MAX_TOKENS}\n      models:\n",
        base_url(),
    );
    for m in models {
        s.push_str(&format!("        - id: {}\n          name: {}\n", m.id, m.name));
    }
    s
}

fn llm_pi_ai_section(models: &[GatewayModel]) -> String {
    format!("llm-pi-ai:\n  providers:\n{}", provider_yaml(models))
}

fn split_top_level(text: &str) -> Vec<(String, String)> {
    if text.trim().is_empty() {
        return vec![];
    }
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    let mut starts: Vec<(usize, String)> = Vec::new();
    for (i, line) in lines.iter().enumerate() {
        if let Some(key) = top_level_key(line) {
            starts.push((i, key));
        }
    }
    if starts.is_empty() {
        return vec![("".into(), text.to_string())];
    }
    let mut blocks = Vec::new();
    if starts[0].0 > 0 {
        let prefix: String = lines[..starts[0].0].concat();
        if prefix.trim().len() > 0 {
            blocks.push(("".into(), prefix));
        }
    }
    for (n, (start, key)) in starts.iter().enumerate() {
        let end = if n + 1 < starts.len() {
            starts[n + 1].0
        } else {
            lines.len()
        };
        blocks.push((key.clone(), lines[*start..end].concat()));
    }
    blocks
}

fn top_level_key(line: &str) -> Option<String> {
    let line = line.trim_end_matches(['\n', '\r']);
    if line.starts_with(' ') || line.starts_with('\t') || line.is_empty() {
        return None;
    }
    let k = line.split(':').next()?.to_string();
    if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && line.contains(':') {
        Some(k)
    } else {
        None
    }
}

fn ensure_nl(s: String) -> String {
    if s.ends_with('\n') || s.is_empty() {
        s
    } else {
        s + "\n"
    }
}

fn upsert_ais_codex_provider(settings: &str, models: &[GatewayModel]) -> String {
    let section = llm_pi_ai_section(models);
    let blocks = split_top_level(settings);
    if !blocks.iter().any(|(k, _)| k == "llm-pi-ai") {
        let body = settings.trim_end();
        let extra = if body.is_empty() { section } else { format!("\n\n{section}") };
        return format!("{}\n", format!("{body}{extra}").trim_end());
    }
    let mut out = String::new();
    for (key, block) in blocks {
        if key != "llm-pi-ai" {
            out.push_str(&ensure_nl(block));
            continue;
        }
        out.push_str(&upsert_provider_inside_llm_pi_ai(&block, models));
    }
    ensure_nl(out)
}

fn upsert_provider_inside_llm_pi_ai(block: &str, models: &[GatewayModel]) -> String {
    let lines: Vec<&str> = block.split_inclusive('\n').collect();
    let Some(prov_idx) = lines.iter().position(|l| l.trim_end() == "  providers:") else {
        return llm_pi_ai_section(models);
    };
    let new_prov = provider_yaml(models);
    let mut children: Vec<(String, usize)> = Vec::new();
    let mut i = prov_idx + 1;
    while i < lines.len() {
        let l = lines[i];
        if let Some(name) = child_provider_key(l) {
            children.push((name, i));
        } else if !l.starts_with(' ') && !l.trim().is_empty() {
            break;
        }
        i += 1;
    }
    let mut ranges: Vec<(String, usize, usize)> = Vec::new();
    for n in 0..children.len() {
        let end = if n + 1 < children.len() {
            children[n + 1].1
        } else {
            i
        };
        ranges.push((children[n].0.clone(), children[n].1, end));
    }
    let mut out = lines[..=prov_idx].concat();
    out.push_str(&new_prov);
    for (name, s, e) in ranges {
        if name != PROVIDER_ID {
            out.push_str(&lines[s..e].concat());
        }
    }
    out.push_str(&lines[i..].concat());
    out
}

fn child_provider_key(line: &str) -> Option<String> {
    let line = line.trim_end_matches(['\n', '\r']);
    if !line.starts_with("    ") || line.starts_with("     ") {
        return None;
    }
    let rest = &line[4..];
    let k = rest.split(':').next()?.to_string();
    if k.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') && rest.contains(':') {
        Some(k)
    } else {
        None
    }
}

fn upsert_default_model(settings: &str, model_id: &str) -> String {
    let block = format!(
        "agent-default-model:\n  provider: {PROVIDER_ID}\n  model: {model_id}\n  reasoningEffort: high\n"
    );
    let blocks = split_top_level(settings);
    if !blocks.iter().any(|(k, _)| k == "agent-default-model") {
        return format!("{}\n\n{block}", settings.trim_end());
    }
    let mut out = String::new();
    for (key, b) in blocks {
        if key == "agent-default-model" {
            out.push_str(&block);
        } else {
            out.push_str(&ensure_nl(b));
        }
    }
    ensure_nl(out)
}

fn upsert_credential(text: &str) -> String {
    if text.lines().any(|l| l.trim_start().starts_with(&format!("{API_KEY_ENV}:"))) {
        return ensure_nl(text.to_string());
    }
    let mut out = String::new();
    let mut inserted = false;
    for line in text.split_inclusive('\n') {
        out.push_str(line);
        if !inserted && line.trim_end() == "refs:" {
            out.push_str(&format!("  {API_KEY_ENV}: {API_KEY_VALUE}\n"));
            inserted = true;
        }
    }
    if !inserted {
        let body = text.trim_end();
        if body.is_empty() {
            return format!("version: 1\nrefs:\n  {API_KEY_ENV}: {API_KEY_VALUE}\n");
        }
        return format!("{body}\n{API_KEY_ENV}: {API_KEY_VALUE}\n");
    }
    ensure_nl(out)
}

fn remove_ais_codex_provider(settings: &str) -> String {
    let blocks = split_top_level(settings);
    if !blocks.iter().any(|(k, _)| k == "llm-pi-ai") {
        return ensure_nl(settings.to_string());
    }
    let mut out = String::new();
    for (key, block) in blocks {
        if key != "llm-pi-ai" {
            out.push_str(&ensure_nl(block));
            continue;
        }
        let trimmed = remove_provider_inside_llm_pi_ai(&block);
        if !trimmed.is_empty() {
            out.push_str(&ensure_nl(trimmed));
        }
    }
    if settings.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n", out.trim_end())
    }
}

fn remove_provider_inside_llm_pi_ai(block: &str) -> String {
    let lines: Vec<&str> = block.split_inclusive('\n').collect();
    let Some(prov_idx) = lines.iter().position(|l| l.trim_end() == "  providers:") else {
        return String::new();
    };
    let mut children: Vec<(String, usize)> = Vec::new();
    let mut i = prov_idx + 1;
    while i < lines.len() {
        let l = lines[i];
        if let Some(name) = child_provider_key(l) {
            children.push((name, i));
        } else if !l.starts_with(' ') && !l.trim().is_empty() {
            break;
        }
        i += 1;
    }
    let keep: Vec<(usize, usize)> = children
        .iter()
        .enumerate()
        .filter(|(_, (name, _))| name != PROVIDER_ID)
        .map(|(n, (_, start))| {
            let end = if n + 1 < children.len() {
                children[n + 1].1
            } else {
                i
            };
            (*start, end)
        })
        .collect();
    if keep.is_empty() {
        return String::new();
    }
    let mut out = lines[..=prov_idx].concat();
    for (s, e) in keep {
        out.push_str(&lines[s..e].concat());
    }
    out.push_str(&lines[i..].concat());
    out
}

fn remove_credential(text: &str) -> String {
    let kept: String = text
        .split_inclusive('\n')
        .filter(|l| !l.trim_start().starts_with(&format!("{API_KEY_ENV}:")))
        .collect();
    ensure_nl(kept)
}

fn clear_ais_default_model(settings: &str) -> String {
    let blocks = split_top_level(settings);
    let mut out = String::new();
    let mut changed = false;
    for (key, block) in blocks {
        if key != "agent-default-model" {
            out.push_str(&ensure_nl(block));
            continue;
        }
        if block.lines().any(|l| l.trim() == format!("provider: {PROVIDER_ID}")) {
            out.push_str(
                "agent-default-model:\n  provider: deepseek-official\n  model: deepseek-v4-flash\n  reasoningEffort: high\n",
            );
            changed = true;
        } else {
            out.push_str(&ensure_nl(block));
        }
    }
    if !changed {
        return ensure_nl(settings.to_string());
    }
    ensure_nl(out)
}

fn default_model_backup_path() -> Result<PathBuf, String> {
    Ok(dsh_home()?.join(DEFAULT_MODEL_BACKUP))
}

fn remove_top_level_block(settings: &str, key: &str) -> String {
    let blocks = split_top_level(settings);
    let mut out = String::new();
    for (k, b) in blocks {
        if k != key {
            out.push_str(&ensure_nl(b));
        }
    }
    if settings.trim().is_empty() {
        String::new()
    } else {
        format!("{}\n", out.trim_end())
    }
}

fn replace_top_level_block(settings: &str, key: &str, new_block: &str) -> String {
    let blocks = split_top_level(settings);
    if !blocks.iter().any(|(k, _)| k == key) {
        let body = settings.trim_end();
        let extra = if body.is_empty() {
            new_block.trim_end().to_string()
        } else {
            format!("\n\n{}", new_block.trim_end())
        };
        return format!("{}\n", format!("{body}{extra}").trim_end());
    }
    let mut out = String::new();
    for (k, b) in blocks {
        if k == key {
            out.push_str(new_block);
        } else {
            out.push_str(&ensure_nl(b));
        }
    }
    ensure_nl(out)
}

/// 把当前 `agent-default-model` 整段（若存在）备份到指定路径；不存在则存空文件（表示"原本没有"）。
/// 重复接入时保留第一份（原始）快照，避免二次接入把备份覆盖成 ais-codex。
fn save_default_model_backup_at(path: &Path, settings: &str) -> Result<(), String> {
    if path.exists() {
        return Ok(());
    }
    let block = split_top_level(settings)
        .into_iter()
        .find(|(k, _)| k == "agent-default-model")
        .map(|(_, b)| b);
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    fs::write(path, block.unwrap_or_default()).map_err(|e| e.to_string())?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|e| e.to_string())?;
    Ok(())
}

/// 还原接入前的默认模型：备份非空就整段还原；备份为空说明原本没有，删掉接入时加的那段。
/// 仅当当前默认模型确实指向 ais-codex 时才动手，避免覆盖用户后来的手动修改。
fn restore_default_model_at(path: &Path, settings: &str) -> String {
    let current_provider = split_top_level(settings)
        .into_iter()
        .find(|(k, _)| k == "agent-default-model")
        .and_then(|(_, b)| {
            b.lines().find_map(|l| {
                l.trim()
                    .strip_prefix("provider:")
                    .map(|s| s.trim().to_string())
            })
        });
    if current_provider.as_deref() != Some(PROVIDER_ID) {
        return ensure_nl(settings.to_string());
    }
    if let Ok(saved) = fs::read_to_string(path) {
        if !saved.trim().is_empty() {
            return replace_top_level_block(settings, "agent-default-model", &saved);
        }
        return remove_top_level_block(settings, "agent-default-model");
    }
    // 没有备份：退回硬编码官方 DeepSeek
    clear_ais_default_model(settings)
}

fn save_default_model_backup(settings: &str) -> Result<(), String> {
    save_default_model_backup_at(&default_model_backup_path()?, settings)
}

fn restore_default_model(settings: &str) -> String {
    match default_model_backup_path() {
        Ok(p) => restore_default_model_at(&p, settings),
        Err(_) => clear_ais_default_model(settings),
    }
}


#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    // 集成测试与 fixture 测试都会改 AIS_SWITCH_PROXY / DSH_HOME 环境变量，
    // Rust 测试默认并行跑，用这把锁串行化这两个环境敏感的测试。
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn sample_models() -> Vec<GatewayModel> {
        vec![
            GatewayModel {
                id: "llm-gateway--deepseek-v4-flash".into(),
                name: "deepseek-v4-flash".into(),
            },
            GatewayModel {
                id: "llm-gateway--glm-5.2".into(),
                name: "glm-5.2".into(),
            },
        ]
    }

    #[test]
    fn yaml_upsert_remove_roundtrip() {
        let models = sample_models();
        let empty = upsert_ais_codex_provider("", &models);
        assert!(empty.contains("llm-pi-ai:"));
        assert!(empty.contains(PROVIDER_ID));

        let existing = "ui-theme:\n  preference: system\nllm-pi-ai:\n  providers:\n    other:\n      api: openai-completions\n      baseURL: http://example.invalid/v1\n";
        let merged = upsert_ais_codex_provider(existing, &models);
        assert!(merged.contains("    other:"));
        assert_eq!(merged.matches(&format!("    {PROVIDER_ID}:")).count(), 1);
        let twice = upsert_ais_codex_provider(&merged, &models);
        assert_eq!(twice.matches(&format!("    {PROVIDER_ID}:")).count(), 1);

        let removed = remove_ais_codex_provider(&merged);
        assert!(!removed.contains(PROVIDER_ID));
        assert!(removed.contains("    other:"));

        let only = upsert_ais_codex_provider("ui-theme:\n  preference: system\n", &models);
        assert!(!remove_ais_codex_provider(&only).contains("llm-pi-ai:"));

        let cred = upsert_credential("version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-test\n");
        assert!(cred.contains(API_KEY_ENV));
        assert!(cred.contains("sk-test"));
        assert_eq!(upsert_credential(&cred).matches(API_KEY_ENV).count(), 1);
        let cred_rm = remove_credential(&cred);
        assert!(!cred_rm.contains(API_KEY_ENV));
        assert!(cred_rm.contains("sk-test"));

        let defaulted = upsert_default_model("agent-default-model:\n  provider: x\n  model: y\n", &models[0].id);
        let cleared = clear_ais_default_model(&defaulted);
        assert!(cleared.contains("deepseek-official"));
        assert!(!cleared.contains(&format!("provider: {PROVIDER_ID}")));
    }

    #[test]
    fn parse_models_filters_gateway() {
        let v: serde_json::Value = serde_json::json!({
            "data": [
                {"id": "llm-gateway--glm-5.2", "cc_switch": {"upstream_model": "glm-5.2"}},
                {"id": "user@npt.sg--gpt-5.4"}
            ]
        });
        let (gw, all) = parse_gateway_models(&v);
        assert_eq!(all.len(), 2);
        assert_eq!(gw.len(), 1);
        assert_eq!(gw[0].name, "glm-5.2");
    }

    #[test]
    fn default_model_backup_restore_roundtrip() {
        let dir = std::env::temp_dir().join("dsh-ais-codex-test-default");
        let _ = fs::create_dir_all(&dir);
        let backup = dir.join(DEFAULT_MODEL_BACKUP);
        let _ = fs::remove_file(&backup);

        // 原本就有默认模型：移除后应原样还原，而不是硬编码回 deepseek-official
        let original = "agent-default-model:\n  provider: deepseek-official\n  model: deepseek-v4-flash\n  reasoningEffort: high\n";
        let with_other = format!("ui-theme:\n  preference: system\n\n{original}");
        save_default_model_backup_at(&backup, &with_other).unwrap();

        let overridden = upsert_default_model(&with_other, &sample_models()[0].id);
        assert!(overridden.contains(&format!("provider: {PROVIDER_ID}")));

        let restored = restore_default_model_at(&backup, &overridden);
        assert!(restored.contains("provider: deepseek-official"));
        assert!(restored.contains("model: deepseek-v4-flash"));
        assert!(!restored.contains(&format!("provider: {PROVIDER_ID}")));
        assert!(restored.contains("ui-theme:"));

        // 原本没有默认模型：移除后应删掉接入时加的那整段
        let _ = fs::remove_file(&backup);
        save_default_model_backup_at(&backup, "ui-theme:\n  preference: system\n").unwrap();
        let overridden2 = upsert_default_model("ui-theme:\n  preference: system\n", &sample_models()[0].id);
        assert!(overridden2.contains("agent-default-model:"));
        let restored2 = restore_default_model_at(&backup, &overridden2);
        assert!(!restored2.contains("agent-default-model"));
        assert!(restored2.contains("ui-theme:"));

        // 当前默认模型不是 ais-codex 时，还原不动手
        let other_default = "agent-default-model:\n  provider: something-else\n  model: x\n";
        assert_eq!(
            restore_default_model_at(&backup, other_default),
            ensure_nl(other_default.to_string())
        );

        let _ = fs::remove_file(&backup);
    }

    // ---- mock AIS Switch（本地 HTTP server，纯 std 实现，不引新依赖）----

    fn spawn_mock_ais() -> String {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind mock");
        let addr = listener.local_addr().expect("addr");
        let base = format!("http://{addr}");
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                std::thread::spawn(move || {
                    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
                    let mut buf = [0u8; 8192];
                    let n = stream.read(&mut buf).unwrap_or(0);
                    let req = String::from_utf8_lossy(&buf[..n]).to_string();
                    let path = req.split_whitespace().nth(1).unwrap_or("/").split('?').next().unwrap_or("/");
                    let (status, body) = match path {
                        "/health" => (200, r#"{"ok":true}"#),
                        "/status" => (200, r#"{"running":true,"port":15721}"#),
                        "/codex/v1/models" => (
                            200,
                            r#"{"data":[{"id":"llm-gateway--deepseek-v4-flash","cc_switch":{"upstream_model":"deepseek-v4-flash"}},{"id":"llm-gateway--glm-5.2","cc_switch":{"upstream_model":"glm-5.2"}}]}"#,
                        ),
                        "/codex/v1/chat/completions" => (200, r#"{"choices":[{"message":{"content":"pong"}}]}"#),
                        _ => (404, "{}"),
                    };
                    let resp = format!(
                        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    );
                    let _ = stream.write_all(resp.as_bytes());
                });
            }
        });
        base
    }

    #[test]
    fn integration_check_setup_refresh_remove() {
        let _guard = ENV_LOCK.lock().unwrap();
        let base = spawn_mock_ais();
        let tmp = std::env::temp_dir().join(format!("dsh-ais-it-{}", std::process::id()));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(&tmp).unwrap();
        unsafe {
            std::env::set_var("AIS_SWITCH_PROXY", &base);
            std::env::set_var("DSH_HOME", &tmp);
        }

        // 预置：其它 provider + 官方默认模型，验证 remove 时其它 provider 保留、默认模型还原
        let before = concat!(
            "ui-theme:\n",
            "  preference: system\n",
            "\n",
            "llm-pi-ai:\n",
            "  providers:\n",
            "    other:\n",
            "      api: openai-completions\n",
            "      baseURL: http://example.invalid/v1\n",
            "\n",
            "agent-default-model:\n",
            "  provider: deepseek-official\n",
            "  model: deepseek-v4-flash\n",
            "  reasoningEffort: high\n",
        );
        let sp = tmp.join("settings.yaml");
        fs::write(&sp, before).unwrap();
        fs::write(
            tmp.join(".credentials.yaml"),
            "version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-test\n",
        )
        .unwrap();

        // 1) check：全通过，拿到 2 个 gateway 模型，尚未接入
        let r = check();
        assert!(r.ok, "check 应通过: {:?}", r.steps.iter().map(|s| (&s.name, &s.detail)).collect::<Vec<_>>());
        assert_eq!(r.models.len(), 2);
        assert!(!r.proxy_down);
        assert_eq!(r.reply.as_deref(), Some("pong"));
        assert!(!r.has_provider);

        // 2) setup(true)：写入 provider + 默认模型 + 凭据；备份默认模型
        let s = setup(true);
        assert!(s.ok, "setup: {}", s.message);
        let after = fs::read_to_string(&sp).unwrap();
        assert_eq!(after.matches(&format!("    {PROVIDER_ID}:")).count(), 1, "只有一份 ais-codex");
        assert!(after.contains("provider: ais-codex"));
        assert!(after.contains("    other:"), "其它 provider 保留");
        let cred = fs::read_to_string(tmp.join(".credentials.yaml")).unwrap();
        assert!(cred.contains("AIS_CODEX_API_KEY"));
        let backup = tmp.join(DEFAULT_MODEL_BACKUP);
        assert!(backup.exists());
        assert!(fs::read_to_string(&backup).unwrap().contains("deepseek-official"));

        // 3) setup(true) 再来一次：幂等，不重复
        let s2 = setup(true);
        assert!(s2.ok, "setup2: {}", s2.message);
        let after2 = fs::read_to_string(&sp).unwrap();
        assert_eq!(after2.matches(&format!("    {PROVIDER_ID}:")).count(), 1);

        // 4) refresh：只刷模型，默认模型 / 凭据不动
        let r = refresh();
        assert!(r.ok, "refresh: {}", r.message);
        let after3 = fs::read_to_string(&sp).unwrap();
        assert_eq!(after3.matches(&format!("    {PROVIDER_ID}:")).count(), 1);
        assert!(after3.contains("llm-gateway--glm-5.2"));

        // 5) remove：ais-codex 消失、其它 provider 保留、默认模型还原、凭据清理、备份删除
        let r = remove();
        assert!(r.ok, "remove: {}", r.message);
        let after4 = fs::read_to_string(&sp).unwrap();
        assert!(!after4.contains(PROVIDER_ID));
        assert!(after4.contains("    other:"));
        assert!(after4.contains("provider: deepseek-official"));
        let cred4 = fs::read_to_string(tmp.join(".credentials.yaml")).unwrap();
        assert!(!cred4.contains("AIS_CODEX_API_KEY"));
        assert!(cred4.contains("sk-test"));
        assert!(!backup.exists(), "备份应已清理");

        let _ = fs::remove_dir_all(&tmp);
    }

    /// 共享 fixture 契约测试：Rust 与 Python 脚本对同一组 settings.yaml 的变换必须一致。
    /// fixture 位于 scripts/tests/fixtures/（baseURL 用 __BASE_URL__ 占位，运行时替换，
    /// 与 AIS_SWITCH_PROXY 环境变量解耦）。
    #[test]
    fn fixture_contract_matches() {
        let _guard = ENV_LOCK.lock().unwrap();
        // 保证两次 base_url() 调用（变换 + 期望替换）看到同一代理基址
        unsafe {
            std::env::remove_var("AIS_SWITCH_PROXY");
        }
        let fixtures = concat!(env!("CARGO_MANIFEST_DIR"), "/../scripts/tests/fixtures");
        let read = |name: &str| -> String {
            fs::read_to_string(format!("{fixtures}/{name}")).unwrap_or_else(|e| panic!("读取 fixture {name}: {e}"))
        };
        let models = sample_models();

        let before = read("settings-before.yaml");
        let after_setup = upsert_ais_codex_provider(&before, &models);
        let after_setup = upsert_default_model(&after_setup, &pick_default_model(&models));
        let expected_setup = read("settings-after-setup.yaml").replace("__BASE_URL__", &base_url());
        assert_eq!(after_setup, expected_setup, "settings-after-setup 与共享 fixture 不一致（Rust↔Python 漂移？）");

        let after_remove = restore_default_model(&remove_ais_codex_provider(&after_setup));
        let expected_remove = read("settings-after-remove.yaml");
        assert_eq!(after_remove, expected_remove, "settings-after-remove 与共享 fixture 不一致（Rust↔Python 漂移？）");

        let cred_before = read("credentials-before.yaml");
        let cred_after_setup = upsert_credential(&cred_before);
        let expected_cred_setup = read("credentials-after-setup.yaml");
        assert_eq!(cred_after_setup, expected_cred_setup, "credentials-after-setup 与共享 fixture 不一致");

        let cred_after_remove = remove_credential(&cred_after_setup);
        let expected_cred_remove = read("credentials-after-remove.yaml");
        assert_eq!(cred_after_remove, expected_cred_remove, "credentials-after-remove 与共享 fixture 不一致");
    }
}
