# DeepSeek Harness（Tauri 封装）

[DeepSeek Harness (dsh)](https://www.npmjs.com/package/@deepseek-ai/dsh) 的桌面封装，用 Tauri 2 + Rust 打包，启动本地 dsh Web 界面。

功能与 Electron 版一致：环境检测 + 一键安装（npm 缓存重定向 / 镜像降级 / 版本固定 / 超时 / 进程组清理）、单实例、空闲端口、日志落盘、导航限制。

## 开发

```bash
npm install
npx tauri dev                      # 开发
bash scripts/build-universal.sh    # 打包 universal（zip + dmg）
```

## AIS Switch（公司代理）

应用菜单 **AIS Switch**：体检 / 接入 / 刷新模型 / 移除（需本机 AIS Switch 已开 Codex 路由）。  
命令行脚本仍可用：`scripts/setup-ais-codex.py`，说明见 [`scripts/dsh-aisSwitch使用文档.md`](scripts/dsh-aisSwitch使用文档.md)。

## 下载

- **macOS**（Intel + Apple Silicon 通用，推荐 DMG）：[DeepSeek.Harness-universal-mac.dmg](https://github.com/Cnnnnnn/deepseek-harness-tauri/releases/latest/download/DeepSeek.Harness-universal-mac.dmg)（或 [zip](https://github.com/Cnnnnnn/deepseek-harness-tauri/releases/latest/download/DeepSeek.Harness-universal-mac.zip)）

历史版本见 [Releases](https://github.com/Cnnnnnn/deepseek-harness-tauri/releases)。
