# AIS Switch → dsh 接入说明

把公司 AIS Switch（Codex 本地代理）接到 DeepSeek Harness（dsh）。  
**非官方支持**：蹭的是 Codex 代理通道；AIS Switch 已下线 OpenCode，请勿再用 OpenCode。

## 应用内（推荐）

看 **屏幕最上方的 macOS 菜单栏**（不是网页左侧「设置」）：

**工具 → AIS Switch…**

- 打开窗口会**自动体检**；连不上代理时会出现 **打开 AIS Switch** 按钮
- **体检**：探测 `127.0.0.1:15721`、列出 Gateway 模型、打一枪 `ping`
- **接入**：写入 `~/.dsh` 的 `ais-codex`（可选「同时设为默认模型」；会先备份接入前的默认模型）。体检未通过时（连不上代理 / 没有 `llm-gateway--` 模型）「接入」会**禁用**，按提示修复后重新体检再接入
- **刷新模型**：只更新模型列表
- **重启 dsh 使生效**：接入 / 刷新后点它让配置立即生效（会中断当前会话并刷新主窗口）
- **移除**：卸掉 `ais-codex` 和 `AIS_CODEX_API_KEY`（不动官方 DeepSeek key）；若接入时设为默认，默认模型会**原样还原**回接入前

然后保持 AIS Switch 开着，**新开一个会话**，模型选 `ais-codex` / `llm-gateway--deepseek-v4-flash`。

## 你需要

1. 已安装并登录 **AIS Switch**
2. 打开 **Codex** 路由总开关，智能路由池里已添加 **LLM Gateway**
3. 本机已能跑 **dsh** / DeepSeek Harness（Node ≥ 22.12，建议 `dsh` = `0.1.1-rc.2`）

命令行脚本仍可用（给没法升级 App 的同事）：`setup-ais-codex.py`


## 常用命令

```bash
# 1. 先体检（不写配置）
python3 setup-ais-codex.py --check

# 2. 通过后再接入（写入 ~/.dsh）
python3 setup-ais-codex.py

# 可选：顺便把默认模型切到 Gateway Flash
python3 setup-ais-codex.py --set-default

# Gateway 模型列表变了，只刷新 models
python3 setup-ais-codex.py --refresh

# 关 AIS Switch / 不用了，清理配置
python3 setup-ais-codex.py --remove
```

先看将要写入的内容：`--dry-run`（可与上面命令组合，例如 `--refresh --dry-run`）。

## 接入后

1. **保持 AIS Switch 开着**（监听 `127.0.0.1:15721`）
2. 重启 dsh / 新开一个会话
3. 模型选 `ais-codex` / `llm-gateway--deepseek-v4-flash`（必须带 `llm-gateway--` 前缀）

脚本默认**不改**官方 DeepSeek 默认模型（除非加了 `--set-default`）。关 Switch 后官方 DeepSeek 仍可用；选 Gateway 模型会失败，属正常。

## 排障

| 现象 | 处理 |
|---|---|
| `--check` 连接被拒绝 | 打开 AIS Switch，打开 Codex 路由 |
| 没有 `llm-gateway--*` | Codex 页把 LLM Gateway 加进路由池并刷新 |
| 打到 ChatGPT / Official | 模型 id 必须用 `llm-gateway--...`，不要用不带前缀的名字 |
| dsh 里看不到 ais-codex | 先 `--check` 通过，再跑无参数 setup，然后重启 dsh |
| 不用了 | `--remove`（不动你的 `DEEPSEEK_API_KEY`） |

**命令行脚本**支持用环境变量覆盖代理基址（一般不用改）：

```bash
export AIS_SWITCH_PROXY=http://127.0.0.1:15721
```

应用内「工具 → AIS Switch…」固定连 `127.0.0.1:15721`，暂不支持改地址；需要改走命令行脚本。
