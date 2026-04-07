# CainBot Rust

[![Rust Edition](https://img.shields.io/badge/rust-edition%202024-orange)](Cargo.toml)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

CainBot 的 Rust 运行时仓库。这个仓库用于独立发布 CainBot 的 Rust 版入口，和旧的 Node.js 主仓库分开维护。

## 当前状态

已接入并可编译运行的部分：

- NapCat HTTP + SSE 客户端
- OpenAI 兼容聊天客户端与翻译客户端
- 配置、日志、状态文件、运行时配置、WebUI 同步文件
- `/help`、`/chat`、`/tr`
- `/e 状态`
- `/e 启用`、`/e 禁用`
- `/e 文件下载 启用|关闭`
- `/e 过滤心跳 启用|关闭`
- `@bot` / `@他人` 检测、疑问句检测、文本附件读取骨架

尚未完成迁移的能力：

- 完整 `chat-session-manager.mjs`
- 完整 `group-file-download-manager.mjs`
- `msav-map-analyzer.mjs`
- `mod-issue-repair-manager.mjs`
- `codex-bridge-server.mjs`
- `local-rag-retriever.mjs`
- `codex-readonly-tools.mjs`
- 完整图片/联网混合输入路径
- `src/index.mjs` 里的完整消息分流、低信息回复拦截、自动入群双保险、topic closure、shutdown vote
- `/e` 的 prompt 审核链路和 prompt 生成/改写逻辑

更细的迁移记录见 [RUST_PORT_PROGRESS.md](RUST_PORT_PROGRESS.md)。

## 仓库定位

- 这是 Rust 版 CainBot 的独立仓库
- 配置文件继续兼容现有 `config.json`
- 状态文件继续兼容 `data/state.json`
- 运行时配置继续兼容 `data/runtime-config.json`
- WebUI 同步继续兼容 `data/webui-sync.json`
- NapCat 侧继续走 OneBot HTTP + SSE

## 快速开始

### 环境要求

- Rust stable，需支持 `edition = "2024"`
- NapCat OneBot HTTP + SSE
- OpenAI 兼容接口

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

### 开发运行

```bash
cargo run --bin cainbot-rs
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
├── scripts/                # 辅助脚本
├── data/                   # 运行时数据（默认不提交）
├── config.example.json     # 配置模板
├── Cargo.toml
└── RUST_PORT_PROGRESS.md
```

## 运行说明

- 二进制入口：`cainbot-rs`
- 入口文件：`rust-src/main.rs`
- 当前仓库仍保留部分旧版 `src/*.mjs` 文件，作为迁移参考，不是正式运行入口

## 许可证

[AGPL-3.0](LICENSE)
