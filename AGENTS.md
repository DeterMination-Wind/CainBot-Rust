# CainBot Rust Agent Notes

本文件给后续在 `CainBot-Rust` 仓库里工作的代理用。目标是让维护动作贴合当前 Rust 运行时，而不是继续按旧的 Node 主仓库习惯操作。

## 仓库定位

- 正式运行入口：`rust-src/main.rs`
- 主运行时装配：`rust-src/app.rs`
- 本仓库已经由 Rust 接管主事件循环
- 但当前仍保留一部分 Node 兼容 worker：
  - `scripts/rust-qa-session-worker.mjs`
  - `scripts/rust-group-download-worker.mjs`

不要把这个仓库描述成“只剩基础层占位”，这已经不准确。

## 目录约定

- `rust-src/`：Rust 运行时代码
- `prompts/`：Prompt 与图片资源
- `scripts/`：兼容 worker 与辅助脚本
- `data/`：运行时目录，不应提交运行产物
- `config.example.json`：配置模板
- `RUST_PORT_PROGRESS.md`：当前迁移状态说明

## 不要改坏的行为

- 自动入群必须保留两条路径：
  - `request_type=group` 直接处理
  - 后台轮询系统消息补捞邀请
- 群主动回复的过滤心跳不能影响显式 `/chat` 和 `@Cain`
- 低信息回复仍要保留复审与回退逻辑
- `replyErrorsToChat` 默认保持 `false`
- 与 CainBot 远端 Linux 部署相关的路径默认仍按 `/Cainbot` 与 `/Wind_Data` 约定

## 关键实现边界

### 1. 聊天会话

文件：

- `rust-src/chat_session_manager.rs`
- `rust-src/qa_session_worker.rs`

现状：

- Rust 侧已接入聊天入口、主动回复、低信息复审、长期记忆捕获
- 但实际会话处理仍部分委托给 `rust-qa-session-worker.mjs`

结论：

- 不要误以为这里已经完全纯 Rust 化
- 如果修改聊天链路，优先同时检查 Rust 控制层和 Node worker 边界

### 2. 文件下载

文件：

- `rust-src/group_file_download_worker.rs`
- `scripts/rust-group-download-worker.mjs`

现状：

- Rust 侧已经接线，消息入口和低信息回退都可能触发它
- 但大体量下载/构建逻辑主体仍在 Node worker

结论：

- 不要把这块当成“未接入”
- 也不要把它当成“已经完全 Rust 重写”

### 3. Issue repair 与 Codex bridge

文件：

- `rust-src/issue_repair_manager.rs`
- `rust-src/codex_bridge_server.rs`

现状：

- 启动时会初始化并接到消息分流
- 相关行为已经是运行中路径，不是占位文件

## 提交前最低检查

至少做这些：

- `cargo check`
- `cargo test`
- 如果改了入口分流或聊天链路，再看一眼 `RUST_PORT_PROGRESS.md` 是否需要同步更新
- 如果改了配置字段，同步检查 `config.example.json`

## 不要提交的内容

- `data/` 下运行期生成物
- 下载产物
- 缓存、日志、群表情、临时文件
- 真实 token、私钥、密钥

## 交接时应说明

- 改了哪些行为
- 是否影响 Rust/Node 兼容 worker 边界
- 是否改动配置结构
- 做了哪些本地验证
- 还有哪些路径没验证
