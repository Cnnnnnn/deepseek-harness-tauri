#!/usr/bin/env python3
"""一键把 dsh 接到 AIS Switch 的 Codex 本地代理（公司 LLM Gateway）。

注意：这是蹭 AIS Switch 的 Codex 本地代理，非官方支持；OpenCode 已下线请勿再用。
关 AIS Switch 后选 Gateway 模型会失败，可用 --remove 清理。

前置：本机已安装并登录 AIS Switch，打开 Codex 路由，智能路由池里已添加 LLM Gateway。
用法：
  python3 setup-ais-codex.py           # 写入 ~/.dsh 配置
  python3 setup-ais-codex.py --dry-run
  python3 setup-ais-codex.py --set-default   # 同时把默认模型切到 Gateway Flash
  python3 setup-ais-codex.py --check         # 只体检，不写配置
  python3 setup-ais-codex.py --refresh       # 只刷新 Gateway 模型列表
  python3 setup-ais-codex.py --remove        # 卸掉 ais-codex 配置（关 Switch 后可清）
  python3 setup-ais-codex.py --self-check

同事只需这一份文件。不改 dsh-tauri，不覆盖已有 DEEPSEEK_API_KEY。
"""
from __future__ import annotations

import argparse
import json
import os
import re
import fcntl
import stat
import sys
import tempfile
import urllib.error
import urllib.request
from pathlib import Path

PROXY = os.environ.get("AIS_SWITCH_PROXY", "http://127.0.0.1:15721")
MODELS_URL = f"{PROXY}/codex/v1/models"
CHAT_URL = f"{PROXY}/codex/v1/chat/completions"
HEALTH_URL = f"{PROXY}/health"
STATUS_URL = f"{PROXY}/status"
PROVIDER_ID = "ais-codex"
API_KEY_ENV = "AIS_CODEX_API_KEY"
API_KEY_VALUE = "local"
BASE_URL = f"{PROXY}/codex/v1"
DISPLAY_NAME = "AIS Switch (Codex)"
DEFAULT_CONTEXT = 1_000_000
DEFAULT_MAX_TOKENS = 131_072
GATEWAY_PREFIX = "llm-gateway--"

DSH_HOME = Path(os.environ.get("DSH_HOME", Path.home() / ".dsh"))
SETTINGS = DSH_HOME / "settings.yaml"
CREDENTIALS = DSH_HOME / ".credentials.yaml"
# 接入前 agent-default-model 原始段落备份；--set-default 时写入，--remove 时原样还原
DEFAULT_MODEL_BACKUP = DSH_HOME / ".ais-default-model.bak"


def die(msg: str, code: int = 1) -> None:
    print(f"error: {msg}", file=sys.stderr)
    raise SystemExit(code)


def http_exchange(
    url: str,
    *,
    method: str = "GET",
    payload: dict | None = None,
    timeout: float = 20,
) -> tuple[int | None, object | None, str | None]:
    """返回 (http_status, parsed_json_or_None, error_detail)。"""
    data = None
    headers = {"Authorization": "Bearer dummy"}
    if payload is not None:
        data = json.dumps(payload).encode("utf-8")
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers, method=method)
    try:
        with urllib.request.urlopen(req, timeout=timeout) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
            status = getattr(resp, "status", 200)
    except urllib.error.HTTPError as e:
        raw = e.read().decode("utf-8", errors="replace")
        try:
            return e.code, json.loads(raw), None
        except json.JSONDecodeError:
            return e.code, raw, None
    except urllib.error.URLError as e:
        reason = str(getattr(e, "reason", e))
        if "Connection refused" in reason or "Errno 61" in reason:
            return None, None, "connection_refused"
        return None, None, reason
    except TimeoutError:
        return None, None, "timeout"
    try:
        return status, json.loads(raw) if raw else {}, None
    except json.JSONDecodeError:
        return status, raw, None


def http_json(url: str, timeout: float = 8) -> dict:
    status, body, err = http_exchange(url, timeout=timeout)
    if err == "connection_refused":
        die(
            f"连不上 AIS Switch 代理 {PROXY}（连接被拒绝）。\n"
            "请先打开 AIS Switch，并打开 Codex 路由总开关。"
        )
    if err:
        die(f"请求 {url} 失败：{err}")
    if not isinstance(body, dict):
        die(f"代理返回了非 JSON 对象：{str(body)[:300]}")
    if status is not None and status >= 400:
        die(f"代理 HTTP {status}：{body}")
    return body


def parse_gateway_models(data: dict) -> tuple[list[dict], list[str]]:
    """从 /models 响应抽出 Gateway 模型；第二个返回值是全部 model id。"""
    rows = data.get("data")
    if not isinstance(rows, list):
        return [], []
    all_ids: list[str] = []
    models: list[dict] = []
    seen: set[str] = set()
    for row in rows:
        if not isinstance(row, dict):
            continue
        mid = str(row.get("id") or "")
        if not mid:
            continue
        all_ids.append(mid)
        if not mid.startswith(GATEWAY_PREFIX) or mid in seen:
            continue
        seen.add(mid)
        meta = row.get("cc_switch") if isinstance(row.get("cc_switch"), dict) else {}
        upstream = str(meta.get("upstream_model") or mid.removeprefix(GATEWAY_PREFIX))
        models.append({"id": mid, "name": upstream})
    return models, all_ids


def fetch_gateway_models() -> list[dict]:
    data = http_json(MODELS_URL)
    models, all_ids = parse_gateway_models(data)
    if not all_ids and "data" not in data:
        die("/codex/v1/models 缺少 data 列表；Codex 路由可能没开。")
    if not models:
        tip = (
            "代理在跑，但没有 llm-gateway--* 模型。\n"
            "请在 AIS Switch 的 Codex 页把 LLM Gateway 加进智能路由池并刷新。"
        )
        if all_ids:
            tip += f"\n当前只看到非 Gateway 模型，例如：{', '.join(all_ids[:5])}"
        die(tip)
    return models


def local_config_status() -> tuple[bool, bool]:
    """(settings 是否含 ais-codex, credentials 是否含 AIS_CODEX_API_KEY)。"""
    settings = SETTINGS.read_text() if SETTINGS.exists() else ""
    cred = CREDENTIALS.read_text() if CREDENTIALS.exists() else ""
    has_provider = bool(
        re.search(rf"^\s{{4}}{re.escape(PROVIDER_ID)}\s*:", settings, re.M)
    )
    has_key = bool(
        re.search(rf"^(\s*){re.escape(API_KEY_ENV)}\s*:", cred, re.M)
    )
    return has_provider, has_key


def run_check() -> None:
    """体检代理 + 本地 dsh 配置，不写文件。失败 exit 1。"""
    ok = True
    print(f"代理基址: {PROXY}")

    # 1) health
    status, body, err = http_exchange(HEALTH_URL, timeout=5)
    if err == "connection_refused":
        die(
            f"连不上 {PROXY}（连接被拒绝）。\n"
            "→ 打开 AIS Switch，打开 Codex 路由总开关，确认监听 15721。"
        )
    if err or status != 200:
        print(f"[fail] /health：{err or body}")
        ok = False
    else:
        print("[ok]   /health")

    # 2) status
    status, body, err = http_exchange(STATUS_URL, timeout=5)
    if err or not isinstance(body, dict):
        print(f"[fail] /status：{err or body}")
        ok = False
    else:
        running = body.get("running")
        port = body.get("port")
        print(f"[ok]   /status  running={running} port={port}")
        if running is False:
            print("       → Codex 代理未处于 running，请在 AIS Switch 打开 Codex 路由")
            ok = False

    # 3) models
    status, body, err = http_exchange(MODELS_URL, timeout=8)
    if err or not isinstance(body, dict):
        print(f"[fail] /codex/v1/models：{err or body}")
        ok = False
        models: list[dict] = []
    else:
        models, all_ids = parse_gateway_models(body)
        print(f"[ok]   /codex/v1/models  共 {len(all_ids)} 个，其中 Gateway {len(models)} 个")
        if not models:
            print("       → 没有 llm-gateway--*：请在 Codex 页把 LLM Gateway 加入路由池并刷新")
            if all_ids:
                sample = ", ".join(all_ids[:6])
                print(f"       → 当前模型样例：{sample}")
                if any("npt.sg--" in i or "@" in i for i in all_ids):
                    print("       → 看起来只有个人 ChatGPT 路由，请求会打到 Official 而非公司 Gateway")
            ok = False
        else:
            for m in models:
                print(f"         - {m['id']}")

    # 4) chat/completions 探活
    if models:
        mid = pick_default_model(models)
        print(f"探活 POST /codex/v1/chat/completions  model={mid} ...")
        status, body, err = http_exchange(
            CHAT_URL,
            method="POST",
            payload={
                "model": mid,
                "messages": [{"role": "user", "content": "ping"}],
                "stream": False,
            },
            timeout=30,
        )
        if err:
            print(f"[fail] chat/completions：{err}")
            ok = False
        elif status != 200:
            msg = body
            if isinstance(body, dict):
                err_obj = body.get("error") if isinstance(body.get("error"), dict) else {}
                msg = err_obj.get("message") or body
                provider = err_obj.get("provider")
                if provider == "default":
                    print(
                        "[fail] 请求打到了 Codex default（ChatGPT 账号），不是 LLM Gateway。\n"
                        "       → 必须使用带 llm-gateway-- 前缀的模型 id"
                    )
                    ok = False
                else:
                    print(f"[fail] HTTP {status}：{msg}")
                    ok = False
            else:
                print(f"[fail] HTTP {status}：{msg}")
                ok = False
        else:
            snippet = ""
            if isinstance(body, dict):
                choices = body.get("choices")
                if isinstance(choices, list) and choices:
                    msg = choices[0].get("message") if isinstance(choices[0], dict) else {}
                    if isinstance(msg, dict):
                        snippet = str(msg.get("content") or "")[:80]
            print(f"[ok]   chat/completions  HTTP 200  reply={snippet!r}")

    # 5) 本地 dsh 配置
    has_provider, has_key = local_config_status()
    print(f"本地配置: {SETTINGS}")
    print(f"  {PROVIDER_ID} provider: {'已配置' if has_provider else '未配置（可先跑本脚本写入）'}")
    print(f"  {API_KEY_ENV}: {'已配置' if has_key else '未配置'}")
    if not has_provider or not has_key:
        print("  → 代理通了但 dsh 未接入时，执行：python3 setup-ais-codex.py")

    if ok:
        print("\ncheck passed")
        return
    print("\ncheck failed", file=sys.stderr)
    raise SystemExit(1)


def yaml_models(models: list[dict]) -> str:
    lines = []
    for m in models:
        lines.append(f"        - id: {m['id']}")
        lines.append(f"          name: {m['name']}")
    return "\n".join(lines)


def provider_yaml(models: list[dict]) -> str:
    return (
        f"    {PROVIDER_ID}:\n"
        f"      displayName: {DISPLAY_NAME}\n"
        f"      api: openai-completions\n"
        f"      baseURL: {BASE_URL}\n"
        f"      apiKeyEnv: {API_KEY_ENV}\n"
        f"      defaultContextWindow: {DEFAULT_CONTEXT}\n"
        f"      defaultMaxTokens: {DEFAULT_MAX_TOKENS}\n"
        f"      models:\n"
        f"{yaml_models(models)}\n"
    )


def llm_pi_ai_section(models: list[dict]) -> str:
    return "llm-pi-ai:\n  providers:\n" + provider_yaml(models)


def split_top_level(text: str) -> list[tuple[str, str]]:
    """[(key, full_block_including_key_line), ...] 按出现顺序。"""
    if not text.strip():
        return []
    lines = text.splitlines(keepends=True)
    starts: list[tuple[int, str]] = []
    for i, line in enumerate(lines):
        m = re.match(r"^([A-Za-z0-9_-]+):", line)
        if m:
            starts.append((i, m.group(1)))
    if not starts:
        return [("", text)]
    blocks = []
    prefix = "".join(lines[: starts[0][0]])
    if prefix.strip():
        blocks.append(("", prefix))
    for idx, (start, key) in enumerate(starts):
        end = starts[idx + 1][0] if idx + 1 < len(starts) else len(lines)
        blocks.append((key, "".join(lines[start:end])))
    return blocks


def upsert_ais_codex_provider(settings: str, models: list[dict]) -> str:
    section = llm_pi_ai_section(models)
    blocks = split_top_level(settings)
    if not any(k == "llm-pi-ai" for k, _ in blocks):
        body = settings.rstrip()
        extra = ("\n\n" if body else "") + section
        return (body + extra).rstrip() + "\n"
    out = []
    for key, block in blocks:
        if key != "llm-pi-ai":
            out.append(block if block.endswith("\n") else block + "\n")
            continue
        out.append(_upsert_provider_inside_llm_pi_ai(block, models))
    text = "".join(out)
    return text if text.endswith("\n") else text + "\n"


def _upsert_provider_inside_llm_pi_ai(block: str, models: list[dict]) -> str:
    """保留同事已有的其它 pi-ai provider，只替换 ais-codex。"""
    lines = block.splitlines(keepends=True)
    # 找 `  providers:`（两空格）
    prov_idx = next(
        (i for i, ln in enumerate(lines) if re.match(r"^  providers:\s*$", ln)),
        None,
    )
    new_prov = provider_yaml(models)
    if prov_idx is None:
        # 畸形分节：整段重写，避免静默丢配置
        return llm_pi_ai_section(models)
    # providers 子键：四空格 + id + :
    i = prov_idx + 1
    children: list[tuple[str, int, int]] = []
    while i < len(lines):
        m = re.match(r"^    ([A-Za-z0-9_-]+):", lines[i])
        if m:
            children.append((m.group(1), i, -1))
        elif lines[i].startswith(" ") is False and lines[i].strip():
            break
        i += 1
    for n, (name, start, _) in enumerate(children):
        end = children[n + 1][1] if n + 1 < len(children) else i
        children[n] = (name, start, end)
    # 删掉旧 ais-codex
    keep = [c for c in children if c[0] != PROVIDER_ID]
    head = "".join(lines[: prov_idx + 1])
    middle = "".join("".join(lines[s:e]) for _, s, e in keep)
    tail = "".join(lines[i:])
    return head + new_prov + middle + tail


def upsert_default_model(settings: str, model_id: str) -> str:
    block = (
        "agent-default-model:\n"
        f"  provider: {PROVIDER_ID}\n"
        f"  model: {model_id}\n"
        "  reasoningEffort: high\n"
    )
    blocks = split_top_level(settings)
    if not any(k == "agent-default-model" for k, _ in blocks):
        return settings.rstrip() + "\n\n" + block
    out = []
    for key, b in blocks:
        if key == "agent-default-model":
            out.append(block)
        else:
            out.append(b if b.endswith("\n") else b + "\n")
    text = "".join(out)
    return text if text.endswith("\n") else text + "\n"


def upsert_credential(text: str) -> str:
    if re.search(rf"^{re.escape(API_KEY_ENV)}\s*:", text, re.M) or re.search(
        rf"^\s+{re.escape(API_KEY_ENV)}\s*:", text, re.M
    ):
        return text if text.endswith("\n") else text + "\n"
    if re.search(r"^refs:\s*$", text, re.M):
        return re.sub(
            r"^refs:\s*$",
            f"refs:\n  {API_KEY_ENV}: {API_KEY_VALUE}",
            text,
            count=1,
            flags=re.M,
        )
    body = text.rstrip()
    extra = f"\n{API_KEY_ENV}: {API_KEY_VALUE}\n"
    return (body + extra) if body else f"version: 1\nrefs:\n  {API_KEY_ENV}: {API_KEY_VALUE}\n"


def remove_ais_codex_provider(settings: str) -> str:
    """删掉 ais-codex；若 llm-pi-ai 下没有其它 provider，整段去掉。"""
    blocks = split_top_level(settings)
    if not any(k == "llm-pi-ai" for k, _ in blocks):
        return settings if not settings or settings.endswith("\n") else settings + "\n"
    out = []
    for key, block in blocks:
        if key != "llm-pi-ai":
            out.append(block if block.endswith("\n") else block + "\n")
            continue
        trimmed = _remove_provider_inside_llm_pi_ai(block)
        if trimmed:
            out.append(trimmed if trimmed.endswith("\n") else trimmed + "\n")
    text = "".join(out).rstrip() + ("\n" if settings.strip() else "")
    return text


def _remove_provider_inside_llm_pi_ai(block: str) -> str:
    lines = block.splitlines(keepends=True)
    prov_idx = next(
        (i for i, ln in enumerate(lines) if re.match(r"^  providers:\s*$", ln)),
        None,
    )
    if prov_idx is None:
        return ""  # 无法解析则整段丢掉，避免留下半残配置
    i = prov_idx + 1
    children: list[tuple[str, int, int]] = []
    while i < len(lines):
        m = re.match(r"^    ([A-Za-z0-9_-]+):", lines[i])
        if m:
            children.append((m.group(1), i, -1))
        elif lines[i].startswith(" ") is False and lines[i].strip():
            break
        i += 1
    for n, (name, start, _) in enumerate(children):
        end = children[n + 1][1] if n + 1 < len(children) else i
        children[n] = (name, start, end)
    keep = [c for c in children if c[0] != PROVIDER_ID]
    if not keep:
        return ""
    head = "".join(lines[: prov_idx + 1])
    middle = "".join("".join(lines[s:e]) for _, s, e in keep)
    tail = "".join(lines[i:])
    return head + middle + tail


def remove_credential(text: str) -> str:
    """删掉 AIS_CODEX_API_KEY，保留其它凭据。"""
    lines = text.splitlines(keepends=True)
    kept = [
        ln
        for ln in lines
        if not re.match(rf"^(\s*){re.escape(API_KEY_ENV)}\s*:", ln)
    ]
    text = "".join(kept)
    return text if text.endswith("\n") or not text else text + "\n"


def clear_ais_default_model(settings: str) -> str:
    """若默认模型指向 ais-codex，改回官方 DeepSeek。"""
    blocks = split_top_level(settings)
    out = []
    changed = False
    for key, block in blocks:
        if key != "agent-default-model":
            out.append(block if block.endswith("\n") else block + "\n")
            continue
        if re.search(rf"^\s+provider:\s*{re.escape(PROVIDER_ID)}\s*$", block, re.M):
            out.append(
                "agent-default-model:\n"
                "  provider: deepseek-official\n"
                "  model: deepseek-v4-flash\n"
                "  reasoningEffort: high\n"
            )
            changed = True
        else:
            out.append(block if block.endswith("\n") else block + "\n")
    if not changed:
        return settings if not settings or settings.endswith("\n") else settings + "\n"
    text = "".join(out)
    return text if text.endswith("\n") else text + "\n"

def _agent_default_provider(settings: str) -> str | None:
    """settings 里 agent-default-model 段的 provider 值（无则 None）。"""
    for key, block in split_top_level(settings):
        if key == "agent-default-model":
            for line in block.splitlines():
                t = line.strip()
                if t.startswith("provider:"):
                    return t.split(":", 1)[1].strip()
    return None


def save_default_model_backup_at(path: Path, settings: str) -> None:
    """把当前 agent-default-model 整段备份到 path；原本没有则存空文件。
    重复接入时保留第一份（原始）快照，避免二次接入把备份覆盖成 ais-codex。"""
    if path.exists():
        return
    block = ""
    for key, b in split_top_level(settings):
        if key == "agent-default-model":
            block = b
            break
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(block)
    os.chmod(path, stat.S_IRUSR | stat.S_IWUSR)


def remove_top_level_block(settings: str, key: str) -> str:
    out = []
    for k, b in split_top_level(settings):
        if k != key:
            out.append(b if b.endswith("\n") else b + "\n")
    if not settings.strip():
        return ""
    return "".join(out).rstrip() + "\n"


def replace_top_level_block(settings: str, key: str, new_block: str) -> str:
    blocks = split_top_level(settings)
    if not any(k == key for k, _ in blocks):
        body = settings.rstrip()
        extra = ("\n\n" if body else "") + new_block.rstrip()
        return (body + extra).rstrip() + "\n"
    out = []
    for k, b in blocks:
        if k == key:
            out.append(new_block if new_block.endswith("\n") else new_block + "\n")
        else:
            out.append(b if b.endswith("\n") else b + "\n")
    text = "".join(out)
    return text if text.endswith("\n") else text + "\n"


def restore_default_model_at(path: Path, settings: str) -> str:
    """还原接入前的默认模型：备份非空就整段还原；为空说明原本没有，删掉加的那段。
    仅当当前默认模型确实指向 ais-codex 时才动手。"""
    if _agent_default_provider(settings) != PROVIDER_ID:
        return settings if not settings or settings.endswith("\n") else settings + "\n"
    if path.exists():
        saved = path.read_text()
        if saved.strip():
            return replace_top_level_block(settings, "agent-default-model", saved)
        return remove_top_level_block(settings, "agent-default-model")
    return clear_ais_default_model(settings)


def save_default_model_backup(settings: str) -> None:
    save_default_model_backup_at(DEFAULT_MODEL_BACKUP, settings)


def restore_default_model(settings: str) -> str:
    return restore_default_model_at(DEFAULT_MODEL_BACKUP, settings)



def write_file(path: Path, content: str, mode: int | None = None) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    backup = path.with_name(path.name + ".bak-ais-codex")
    if path.exists() and not backup.exists():
        backup.write_text(path.read_text())
        if mode is not None:
            os.chmod(backup, mode)
    path.write_text(content)
    if mode is not None:
        os.chmod(path, mode)


class _ConfigLock:
    """跨进程互斥锁（与 App 共用 ~/.dsh/.ais-codex.lock），
    防止命令行脚本与 App 同时读写 settings.yaml / .credentials.yaml。"""

    def __enter__(self):
        DSH_HOME.mkdir(parents=True, exist_ok=True)
        self._f = open(DSH_HOME / ".ais-codex.lock", "w")
        fcntl.flock(self._f, fcntl.LOCK_EX)
        return self

    def __exit__(self, *exc):
        try:
            fcntl.flock(self._f, fcntl.LOCK_UN)
        finally:
            self._f.close()
        return False


def pick_default_model(models: list[dict]) -> str:
    for m in models:
        if m["id"].endswith("deepseek-v4-flash"):
            return m["id"]
    return models[0]["id"]


def run_self_check() -> None:
    models = [
        {"id": "llm-gateway--deepseek-v4-flash", "name": "deepseek-v4-flash"},
        {"id": "llm-gateway--glm-5.2", "name": "glm-5.2"},
    ]
    empty = upsert_ais_codex_provider("", models)
    assert "llm-pi-ai:" in empty and PROVIDER_ID in empty
    existing = (
        "ui-theme:\n  preference: system\n"
        "llm-pi-ai:\n  providers:\n    other:\n      api: openai-completions\n"
        "      baseURL: http://example.invalid/v1\n"
    )
    merged = upsert_ais_codex_provider(existing, models)
    assert "    other:" in merged
    assert merged.count(f"    {PROVIDER_ID}:") == 1
    twice = upsert_ais_codex_provider(merged, models)
    assert twice.count(f"    {PROVIDER_ID}:") == 1
    cred = upsert_credential("version: 1\nrefs:\n  DEEPSEEK_API_KEY: sk-test\n")
    assert API_KEY_ENV in cred and "sk-test" in cred
    cred2 = upsert_credential(cred)
    assert cred2.count(API_KEY_ENV) == 1
    removed = remove_ais_codex_provider(merged)
    assert PROVIDER_ID not in removed and "    other:" in removed
    only = upsert_ais_codex_provider("ui-theme:\n  preference: system\n", models)
    assert "llm-pi-ai:" not in remove_ais_codex_provider(only)
    cred_rm = remove_credential(cred2)
    assert API_KEY_ENV not in cred_rm and "sk-test" in cred_rm
    defaulted = upsert_default_model("agent-default-model:\n  provider: x\n  model: y\n", models[0]["id"])
    cleared = clear_ais_default_model(defaulted)
    assert "deepseek-official" in cleared and PROVIDER_ID not in cleared

    # 默认模型备份/还原
    tmp_default = Path(tempfile.mkdtemp()) / ".ais-default-model.bak"
    original = "agent-default-model:\n  provider: deepseek-official\n  model: deepseek-v4-flash\n  reasoningEffort: high\n"
    with_other = "ui-theme:\n  preference: system\n\n" + original
    save_default_model_backup_at(tmp_default, with_other)
    overridden = upsert_default_model(with_other, models[0]["id"])
    assert PROVIDER_ID in overridden
    restored = restore_default_model_at(tmp_default, overridden)
    assert "deepseek-official" in restored and "deepseek-v4-flash" in restored
    assert PROVIDER_ID not in restored and "ui-theme:" in restored
    # 原本没有默认模型：还原为删掉整段（先删旧备份，模拟从未接入过）
    if tmp_default.exists():
        tmp_default.unlink()
    save_default_model_backup_at(tmp_default, "ui-theme:\n  preference: system\n")
    overridden2 = upsert_default_model("ui-theme:\n  preference: system\n", models[0]["id"])
    assert "agent-default-model" in overridden2
    restored2 = restore_default_model_at(tmp_default, overridden2)
    assert "agent-default-model" not in restored2 and "ui-theme:" in restored2
    # 当前默认不是 ais-codex 时不动手
    other_default = "agent-default-model:\n  provider: something-else\n  model: x\n"
    assert restore_default_model_at(tmp_default, other_default) == (other_default if other_default.endswith("\n") else other_default + "\n")
    tmp_default.unlink()

    # 共享 fixture 契约检查：与 Rust 侧（cargo test fixture_contract_matches）共用
    # scripts/tests/fixtures/，任何一边改动 YAML 输出都会让另一边失败。
    fixtures = Path(__file__).resolve().parent / "tests" / "fixtures"
    read_fixture = lambda name: (fixtures / name).read_text()
    before = read_fixture("settings-before.yaml")
    after_setup = upsert_ais_codex_provider(before, models)
    after_setup = upsert_default_model(after_setup, pick_default_model(models))
    expected_setup = read_fixture("settings-after-setup.yaml").replace("__BASE_URL__", BASE_URL)
    assert after_setup == expected_setup, "settings-after-setup 与共享 fixture 不一致（Rust↔Python 漂移？）"
    after_remove = restore_default_model(remove_ais_codex_provider(after_setup))
    assert after_remove == read_fixture("settings-after-remove.yaml"), "settings-after-remove 与共享 fixture 不一致"
    cred_before = read_fixture("credentials-before.yaml")
    cred_after_setup = upsert_credential(cred_before)
    assert cred_after_setup == read_fixture("credentials-after-setup.yaml"), "credentials-after-setup 与共享 fixture 不一致"
    assert remove_credential(cred_after_setup) == read_fixture("credentials-after-remove.yaml"), "credentials-after-remove 与共享 fixture 不一致"

    print("self-check ok")


def apply_remove(*, dry_run: bool) -> None:
    with _ConfigLock():
        settings = SETTINGS.read_text() if SETTINGS.exists() else ""
        cred = CREDENTIALS.read_text() if CREDENTIALS.exists() else ""
        settings = restore_default_model(remove_ais_codex_provider(settings))
        cred = remove_credential(cred) if cred else cred

        if dry_run:
            print(f"--- {SETTINGS} ---")
            print(settings or "(empty)")
            print(f"--- {CREDENTIALS} (是否仍含 {API_KEY_ENV}) ---")
            print("present" if API_KEY_ENV in cred else "absent")
            return

        if SETTINGS.exists() or settings.strip():
            write_file(SETTINGS, settings if settings.strip() else "")
        if CREDENTIALS.exists():
            write_file(CREDENTIALS, cred if cred.strip() else "version: 1\nrefs:\n", mode=stat.S_IRUSR | stat.S_IWUSR)
        if DEFAULT_MODEL_BACKUP.exists():
            DEFAULT_MODEL_BACKUP.unlink()
        print(f"已移除 {PROVIDER_ID} 与 {API_KEY_ENV}")
        print("重启 dsh / 新开会话后生效。官方 deepseek-official 不受影响；默认模型已还原（若 --set-default 过）。")


def apply_refresh(*, dry_run: bool) -> None:
    """只刷新 ais-codex 的模型列表；未配置过则提示先跑 setup。"""
    has_provider, _has_key = local_config_status()
    if not has_provider:
        die(
            f"本地还没有 {PROVIDER_ID} provider。\n"
            "→ 先执行：python3 setup-ais-codex.py"
        )
    print(f"探测 {MODELS_URL} ...")
    models = fetch_gateway_models()
    print(f"将刷新为 {len(models)} 个 Gateway 模型：")
    for m in models:
        print(f"  - {m['id']}")

    with _ConfigLock():
        settings = SETTINGS.read_text() if SETTINGS.exists() else ""
        settings = upsert_ais_codex_provider(settings, models)

        if dry_run:
            print(f"\n--- {SETTINGS} ---")
            print(settings)
            return

        write_file(SETTINGS, settings)
        print(f"\n已刷新 {SETTINGS} 中的模型列表（未改默认模型 / 凭据）")
        print("重启 dsh / 新开会话后生效。")


def main() -> None:
    p = argparse.ArgumentParser(description="把 dsh 接到 AIS Switch Codex 本地代理")
    p.add_argument("--dry-run", action="store_true", help="只打印，不写文件")
    p.add_argument("--set-default", action="store_true", help="把默认模型切到 Gateway Flash")
    p.add_argument("--check", action="store_true", help="体检代理与本地配置，不写文件")
    p.add_argument("--refresh", action="store_true", help="只刷新 Gateway 模型列表")
    p.add_argument("--remove", action="store_true", help="卸掉 ais-codex 配置与本地 key")
    p.add_argument("--self-check", action="store_true", help="跑内置合并逻辑检查后退出")
    args = p.parse_args()
    if args.self_check:
        run_self_check()
        return
    if args.check:
        run_check()
        return
    if args.refresh:
        apply_refresh(dry_run=args.dry_run)
        return
    if args.remove:
        apply_remove(dry_run=args.dry_run)
        return

    print(f"探测 {MODELS_URL} ...")
    models = fetch_gateway_models()
    print(f"找到 {len(models)} 个 LLM Gateway 模型：")
    for m in models:
        print(f"  - {m['id']}")

    with _ConfigLock():
        settings = SETTINGS.read_text() if SETTINGS.exists() else ""
        settings = upsert_ais_codex_provider(settings, models)
        if args.set_default:
            if not args.dry_run:
                # 先备份当前 agent-default-model，--remove 时原样还原
                save_default_model_backup(settings)
            settings = upsert_default_model(settings, pick_default_model(models))

        cred = CREDENTIALS.read_text() if CREDENTIALS.exists() else "version: 1\nrefs:\n"
        cred = upsert_credential(cred)

        if args.dry_run:
            print(f"\n--- {SETTINGS} ---")
            print(settings)
            print(f"--- {CREDENTIALS} (只显示是否包含 {API_KEY_ENV}) ---")
            print("present" if API_KEY_ENV in cred else "missing")
            return

        write_file(SETTINGS, settings)
        write_file(CREDENTIALS, cred, mode=stat.S_IRUSR | stat.S_IWUSR)
        print(f"\n已写入 {SETTINGS}")
        print(f"已写入 {CREDENTIALS}（{API_KEY_ENV}=local，本地代理不校验真 key）")
        print("下一步：保持 AIS Switch 开着，重启 dsh / 新开会话，模型选")
        print(f"  {PROVIDER_ID} / {pick_default_model(models)}")
        if not args.set_default:
            print("（未改默认模型；需要的话再跑一次并加 --set-default）")


if __name__ == "__main__":
    main()
