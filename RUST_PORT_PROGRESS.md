# Rust Port Progress

当前分支：

- `sync-rust-publish-main`

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
- `rust-src/reply_markdown_renderer.rs`
- `rust-src/group_file_download_worker.rs`
- `rust-src/workflow_agent_manager.rs`
- `rust-src/issue_repair_manager.rs`
- `rust-src/codex_bridge_server.rs`
- `rust-src/status_dashboard.rs`
- `rust-src/utils.rs`

## 当前状态

- `cargo check` 已通过
- `cargo test` 已通过（当前 21 个单测）
- Rust 主入口已经接管 NapCat 事件循环，而不是只有“基础层占位”
- `request` / `notice` / `message` 三类事件都已经进入 Rust 主循环
- 启动时会初始化：
  - NapCat HTTP + SSE 客户端
  - OpenAI 兼容聊天客户端
  - 翻译客户端
  - 运行时配置、状态文件、WebUI 同步
  - Codex Bridge Server
  - WorkflowAgentManager
  - IssueRepairManager
  - Rust 群文件下载工作流
  - Rust 聊天会话与 Markdown 回复图渲染

## 已接入的业务能力

- `/help`
- `/chat`
- `/tr`
- `/agent`
- `/e 状态`
- `/e 启用`
- `/e 禁用`
- `/e 文件下载 启用|关闭`
- `/e 过滤心跳 启用|关闭`
- `@bot` 显式问答
- 群内疑问句主动回复判定
- 低信息回复复审与回退
- 低信息回复改走群文件下载流程
- 通用主动工作流 Agent：
  - 显式 `/agent`
  - 由聊天链路中途 handoff
  - 会话持久化、继续跟进、阶段性汇报
- 关闭 bot 的投票链路（shutdown vote）
- 自动入群：
  - 直接处理 `request_type=group`
  - 后台轮询群系统消息补捞邀请
- 群名片同步
- Issue repair 入口拦截与 Codex bridge 对接
- Markdown 回复图片 Rust 渲染

## 当前运行时迁移判断

- 就主运行链路而言，当前分支已经不是“Rust 主控 + Node 兼容 worker”
- 聊天、群文件下载、回复图片渲染、通用 Agent 都已经进入 Rust 运行时
- 仓库里仍可能保留少量 `.mjs` 辅助脚本或历史工具，但它们不再是当前主运行路径

## 仍未 Rust 化或仍待继续整理的部分

- 旧 `src/` 下部分历史 `.mjs` 工具尚未完全清理
- 一些仓库级辅助工具仍不是 Rust 实现
- 文档与配置说明需要继续随着运行时演进同步

## 已完成但文档过去写错的地方

以下内容以前被写成“还未接入”或“只有占位”，现在已经不是：

- 主循环不再只是事件监听和群邀请占位
- `codex_bridge_server.rs` 已启动并接线
- `workflow_agent_manager.rs` 已初始化并参与消息分流
- `issue_repair_manager.rs` 已初始化并参与消息分流
- `group_file_download_worker.rs` 已接线并可从消息入口/低信息回退链路触发
- `chat_session_manager.rs` 已接入主动回复、过滤心跳、低信息复审、长期记忆捕获等业务入口
- `reply_markdown_renderer.rs` 已承担 Markdown 回复图片生成
- `shutdown vote` 已接到 notice/message 处理链路

## 兼容原则

- 配置文件继续读现有 `config.json`
- 状态文件继续兼容 `data/state.json`
- 运行时配置继续兼容 `data/runtime-config.json`
- WebUI 同步继续兼容 `data/webui-sync.json`
- NapCat 侧继续走现有 OneBot HTTP + SSE
- 优先保持 Rust 运行时为单一真实实现，不再新增新的 Node worker 边界
