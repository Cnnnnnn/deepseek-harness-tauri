# DeepSeek Harness（Tauri 封装）

[DeepSeek Harness (dsh)](https://www.npmjs.com/package/@deepseek-ai/dsh) 的桌面封装，用 Tauri 2 + Rust 打包，启动本地 dsh Web 界面。

功能与 Electron 版一致：环境检测 + 一键安装（npm 缓存重定向 / 镜像降级 / 版本固定 / 超时 / 进程组清理）、单实例、空闲端口、日志落盘、导航限制。

## 开发

```bash
npm install
npx tauri dev                                      # 开发
npx tauri build                                    # 打包当前架构
npx tauri build --target x86_64-apple-darwin       # Intel
```

## 下载

预编译产物见 [Releases](../../releases)。
