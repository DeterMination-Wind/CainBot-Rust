# Rust Port Progress

当前分支：

- `experiment/rust-runtime`

## 当前 Rust 模块

- `rust-src/main.rs`
- `rust-src/app.rs`
- `rust-src/commands.rs`
- `rust-src/config.rs`
- `rust-src/logger.rs`
- `rust-src/state_store.rs`
- `rust-src/runtime_config_store.rs`
- `rust-src/webui_sync_store.rs`
- `rust-src/event_utils.rs`
- `rust-src/message_input.rs`
- `rust-src/message_attachment_reader.rs`
- `rust-src/napcat_client.rs`
- `rust-src/openai_chat_client.rs`
- `rust-src/openai_translator.rs`
- `rust-src/chat_session_manager.rs`
- `rust-src/group_file_download_worker.rs`
- `rust-src/issue_repair_manager.rs`
- `rust-src/codex_bridge_server.rs`
- `rust-src/qa_session_worker.rs`
- `rust-src/worker_process.rs`
- `rust-src/utils.rs`

## 当前状态

- `cargo check` 已通过
- `cargo test` 已通过（当前 11 个消息入口/工具层单测）
- Rust 主入口已经接管 NapCat 事件循环，而不是只有“基础层占位”
- `request` / `notice` / `message` 三类事件都已经进入 Rust 主循环
- 启动时会拉起：
  - NapCat HTTP + SSE 客户端
  - OpenAI 兼容聊天客户端
  - 翻译客户端
  - 运行时配置、状态文件、WebUI 同步
  - Codex Bridge Server
  - IssueRepairManager
  - 群文件下载兼容 worker
  - QA session 兼容 worker（按需懒启动）

## 已接入的业务能力

- `/help`
- `/chat`
- `/tr`
- `/e 状态`
- `/e 启用`
- `/e 禁用`
- `/e 文件下载 启用|关闭`
- `/e 过滤心跳 启用|关闭`
- `@bot` 显式问答
- 群内疑问句主动回复判定
- 低信息回复复审与回退
- 低信息回复改走群文件下载流程
- 关闭 bot 的投票链路（shutdown vote）
- 自动入群：
  - 直接处理 `request_type=group`
  - 后台轮询群系统消息补捞邀请
- 群名片同步
- Issue repair 入口拦截与 Codex bridge 对接

## 目前仍是“Rust 主控 + Node 兼容 worker”的部分

- `chat_session_manager.rs`
  - 仍通过 `scripts/rust-qa-session-worker.mjs` 托管一部分会话/审核链路
- `group_file_download_worker.rs`
  - 仍通过 `scripts/rust-group-download-worker.mjs` 托管原版超重下载/构建逻辑

这意味着当前仓库已经不是“只迁基础层”，但也还不是“100% 纯 Rust 业务层”。

## 还没完成纯 Rust 化的部分

- `msav-map-analyzer.mjs`
- 群文件下载的大体量 Node 逻辑本体还没重写到 Rust
- QA session worker 背后的 Node 会话逻辑还没彻底拔掉
- `codex-readonly-tools.mjs`
- `local-rag-retriever.mjs`
- `message_attachment_reader.mjs` 的完整联网/图片混合输入路径
- `/e` 的 prompt 审核链路和 prompt 生成/改写逻辑

## 已完成但文档过去写错的地方

以下内容以前被写成“还未接入”或“只有占位”，现在已经不是：

- 主循环不再只是事件监听和群邀请占位
- `codex_bridge_server.rs` 已启动并接线
- `issue_repair_manager.rs` 已初始化并参与消息分流
- `group_file_download_worker.rs` 已接线并可从消息入口/低信息回退链路触发
- `chat_session_manager.rs` 已接入主动回复、过滤心跳、低信息复审、长期记忆捕获等业务入口
- `shutdown vote` 已接到 notice/message 处理链路

## 兼容原则

- 配置文件继续读现有 `config.json`
- 状态文件继续兼容 `data/state.json`
- 运行时配置继续兼容 `data/runtime-config.json`
- WebUI 同步继续兼容 `data/webui-sync.json`
- NapCat 侧继续走现有 OneBot HTTP + SSE
- 在纯 Rust 重写完成前，允许局部保留 Node worker 作为兼容桥

## 当前迁移判断

如果只按“入口是否由 Rust 接管”来算，这个分支已经进入“Rust 主运行时”阶段。

如果按“业务逻辑是否完全摆脱 Node 兼容 worker”来算，这个分支仍处于“半迁移完成”阶段：

- Rust 负责主进程、协议、分流、状态和大部分控制逻辑
- 一部分重量级业务实现仍挂在 Node worker 后面
