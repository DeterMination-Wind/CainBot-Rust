# Rust Port Progress

当前分支：

- `experiment/rust-runtime`

当前已经有的 Rust 模块：

- `rust-src/main.rs`
- `rust-src/config.rs`
- `rust-src/logger.rs`
- `rust-src/state_store.rs`
- `rust-src/runtime_config_store.rs`
- `rust-src/webui_sync_store.rs`
- `rust-src/napcat_client.rs`
- `rust-src/openai_chat_client.rs`
- `rust-src/openai_translator.rs`

当前状态：

- `cargo check` 已通过
- 基础层已经能编译：配置、日志、状态文件、运行时配置、WebUI 同步文件、NapCat HTTP/SSE、OpenAI 客户端
- 主循环目前只接了事件监听和群邀请占位逻辑

还没迁移的原版功能：

- `chat-session-manager.mjs`
- `group-file-download-manager.mjs`
- `msav-map-analyzer.mjs`
- `mod-issue-repair-manager.mjs`
- `codex-bridge-server.mjs`
- `local-rag-retriever.mjs`
- `codex-readonly-tools.mjs`
- `message-attachment-reader.mjs`
- `src/index.mjs` 里的完整消息分流、命令解析、低信息回复拦截、自动入群双保险、topic closure、shutdown vote

兼容原则：

- 配置文件继续读现有 `config.json`
- 状态文件继续兼容 `data/state.json`
- 运行时配置继续兼容 `data/runtime-config.json`
- WebUI 同步继续兼容 `data/webui-sync.json`
- NapCat 侧继续走现有 OneBot HTTP + SSE

已知原版性能瓶颈：

- `Node.js` 运行时常驻内存本身不低，空跑就有明显基线开销
- `src/index.mjs` 初始化时一次性常驻很多模块对象，主进程职责过多
- `replySourceToBotReplies`、`botReplyToSource`、群状态 Map、投票状态 Map 都是长期常驻结构
- `StateStore.save()` / `WebUiSyncStore.save()` 每次直接整份 `JSON.stringify` 落盘，状态越大越吃 CPU 和瞬时内存
- `OpenAiChatClient` 同时维护多传输路径、重试、fallback、熔断逻辑，消息和响应体会产生较多临时字符串/JSON 对象
- `NapCatClient` 事件流在 Node 里按字符串缓冲和 JSON 解析，遇到高频事件时 GC 压力会抬高
- `group-file-download-manager.mjs` 和 `codex-readonly-tools.mjs` 这类大模块把“下载、文件扫描、文本拼接、外部工具调用”都堆在同一个进程里，容易抬高峰值内存

为什么先迁基础层：

- 先把协议边界、配置边界、状态边界固定住，后面逐个迁业务模块时更容易验证行为一致性
- 这些基础层正好也是最能直接降低 Node 常驻开销的部分
