# CainBot Rust

[![Rust Edition](https://img.shields.io/badge/rust-edition%202024-orange)](Cargo.toml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

CainBot 的 Rust 运行时仓库。当前主运行链路已经由 Rust 接管，不再是“半 Rust、半 Node worker”的过渡形态。

## 当前状态

2026-04-07 已验证：

- `cargo check` 通过
- `cargo test` 通过：24 个测试，22 通过，2 忽略，0 失败
- Rust 主循环已经接管 NapCat `message` / `notice` / `request` 事件处理
- 聊天、群文件下载、Markdown 回复图、Codex bridge、主动 workflow agent 都运行在 Rust 侧
- 仓库中仍保留少量 `src/*.mjs` 历史脚本与辅助工具，但它们不是当前主运行入口

## 已接入能力

- `/help`
- `/chat`
- `/tr`
- `/agent`
- `/e 状态`
- `/e 启用`
- `/e 禁用`
- `/e 文件下载 启用|关闭`
- `/e 过滤心跳 启用|关闭`
- `/status` 状态截图发送
- `@bot` 显式问答
- 群内疑问句主动回复判定
- 低信息回复复审、回退与群文件补发
- 文本附件读取与消息输入整理
- 自动入群处理与系统消息补捞
- 群名片同步
- issue repair 链路与 Codex bridge 对接
- Markdown 回复图片 Rust 渲染

## 仓库定位

- 这是 CainBot Rust 运行时的独立发布仓库
- 真实运行入口是 Rust 二进制 `cainbot-rs`
- 旧 `src/` 目录只保留为迁移参考或仓库级辅助工具，不是正式运行路径
- 优先保持 Rust 运行时作为单一真实实现，不再继续扩张新的 Node worker 边界

## 兼容性

当前实现继续兼容既有部署文件：

- 配置文件：`config.json`
- 状态文件：`data/state.json`
- 运行时配置：`data/runtime-config.json`
- WebUI 同步：`data/webui-sync.json`
- NapCat 接口：OneBot HTTP + SSE

## 快速开始

### 环境要求

- Rust stable，需支持 `edition = "2024"`
- NapCat OneBot HTTP + SSE
- OpenAI 兼容聊天接口

### 获取源码

```bash
git clone https://github.com/DeterMination-Wind/CainBot-Rust.git
cd CainBot-Rust
```

### 配置

```bash
cp config.example.json config.json
```

然后编辑 `config.json`，至少补齐：

- `napcat.baseUrl`
- `napcat.headers.Authorization`
- `bot.ownerUserId`
- `ai.apiKey`

### 运行

```bash
cargo run --release --bin cainbot-rs
```

或：

```bash
npm start
```

### 检查

```bash
cargo check
cargo test
```

## 目录结构

```text
CainBot-Rust/
├── rust-src/               # Rust 入口与业务模块
├── prompts/                # Prompt 模板
├── scripts/                # 辅助脚本与打包脚本
├── src/                    # 历史 .mjs 参考实现与辅助工具
├── data/                   # 运行时数据（默认不提交）
├── config.example.json     # 配置模板
├── Cargo.toml
├── package.json
└── RUST_PORT_PROGRESS.md
```

## 运行说明

- 二进制入口：`cainbot-rs`
- 入口文件：[rust-src/main.rs](rust-src/main.rs)
- NPM 启动脚本最终也是执行 Rust 入口
- 更细的运行时进度与模块说明见 [RUST_PORT_PROGRESS.md](RUST_PORT_PROGRESS.md)

## 许可证

[AGPL-3.0](LICENSE)
