# Rust Port Progress

最后核对时间：2026-04-07
当前分支：`sync-rust-publish-main`

## 验证结果

- `cargo check`：通过
- `cargo test`：通过
- 测试结果：24 个测试，22 通过，2 忽略，0 失败
- Rust 主入口已经接管 NapCat 事件循环，而不是早期的“基础层占位”状态
- `message` / `notice` / `request` 三类事件都已进入 Rust 主循环

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

## 启动时初始化的 Rust 组件

- NapCat HTTP + SSE 客户端
- OpenAI 兼容聊天客户端
- 翻译客户端
- 状态文件、运行时配置、WebUI 同步
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
- `/status` 状态截图发送
- `@bot` 显式问答
- 群内疑问句主动回复判定
- 低信息回复复审与回退
- 低信息回复改走群文件下载流程
- 通用主动 workflow agent：
  - 显式 `/agent`
  - 聊天链路中途 handoff
  - 会话持久化、继续跟进、阶段性汇报
- 关闭 bot 的投票链路（shutdown vote）
- 自动入群：
  - 直接处理 `request_type=group`
  - 后台轮询群系统消息补捞邀请
- 群名片同步
- issue repair 入口拦截与 Codex bridge 对接
- Markdown 回复图片 Rust 渲染

## 当前迁移判断

- 就主运行链路而言，当前分支已经可以视为“Rust 版本 CainBot”
- 聊天、群文件下载、回复图片渲染、通用 agent 都已经进入 Rust 运行时
- 保留下来的 `.mjs` 文件主要用于历史参考或仓库级辅助工具，不再承担当前主流程

## 仍待继续整理的部分

- 旧 `src/` 下部分历史 `.mjs` 工具尚未清理
- 一些仓库级辅助脚本仍不是 Rust 实现
- 文档需要随着运行时继续演进同步更新

## 兼容原则

- 配置文件继续读现有 `config.json`
- 状态文件继续兼容 `data/state.json`
- 运行时配置继续兼容 `data/runtime-config.json`
- WebUI 同步继续兼容 `data/webui-sync.json`
- NapCat 侧继续走现有 OneBot HTTP + SSE
- 优先保持 Rust 运行时为单一真实实现，不再新增新的 Node worker 边界
