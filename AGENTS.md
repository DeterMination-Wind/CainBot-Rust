# CainBot Rust Agent Notes

本文件给后续在 `CainBot-Rust` 仓库里工作的代理用。目标是让维护动作贴合当前这条 Rust 运行时分支，而不是继续按旧的 Node 主仓库或旧的兼容 worker 架构来判断实现状态。

## 仓库定位

- 正式运行入口：`rust-src/main.rs`
- 主运行时装配：`rust-src/app.rs`
- 当前聊天、群文件下载、主动工作流、回复图片渲染都已经走 Rust 运行时
- 旧的 `.mjs` 文件如果仍然留在仓库里，默认视为历史兼容脚本或辅助工具，不要再把它们当成主运行链路

不要把这个仓库描述成“只剩基础层占位”，这已经不准确。

## 目录约定

- `rust-src/`：Rust 运行时代码
- `prompts/`：Prompt 与图片资源
- `scripts/`：辅助脚本，不再是聊天/下载主 worker 链路
- `data/`：运行时目录，不应提交运行产物
- `config.example.json`：配置模板
- `RUST_PORT_PROGRESS.md`：当前迁移状态说明

## 不要改坏的行为

- 自动入群必须保留两条路径：
  - `request_type=group` 直接处理
  - 后台轮询系统消息补捞邀请
- 群主动回复的过滤心跳不能影响显式 `/chat`、`@Cain`、`/agent`
- 低信息回复仍要保留复审与回退逻辑
- `replyErrorsToChat` 默认保持 `false`
- 与 CainBot 远端 Linux 部署相关的路径默认仍按 `/Cainbot` 与 `/Wind_Data` 约定
- Markdown 回复图片渲染要保留 Rust 本地图生成功能，不要再退回浏览器 worker 方案

## 关键实现边界

### 1. 聊天会话与回复渲染

文件：

- `rust-src/chat_session_manager.rs`
- `rust-src/reply_markdown_renderer.rs`

现状：

- 聊天入口、主动回复、低信息复审、长期记忆捕获已经在 Rust 内
- Markdown 回复图也已经由 Rust 生成 PNG，不再依赖 Playwright/Node worker

结论：

- 修改聊天链路时，先看 `chat_session_manager.rs`
- 修改回复图片时，直接改 `reply_markdown_renderer.rs`

### 2. 文件下载

文件：

- `rust-src/group_file_download_worker.rs`

现状：

- 群文件下载消息入口、候选选择、镜像探测、下载状态推进都已接在 Rust 运行时

结论：

- 不要再按“Rust 控制层 + Node 下载 worker”理解这块
- 改下载逻辑时，直接以 Rust 实现为准

### 3. 通用主动工作流 Agent

文件：

- `rust-src/workflow_agent_manager.rs`
- `rust-src/codex_bridge_server.rs`

现状：

- 已支持显式 `/agent`
- 已支持聊天链路中途 handoff 到主动工作流
- 已支持阶段性汇报、继续跟进、会话持久化

结论：

- 通用 Agent 相关需求优先落在 `workflow_agent_manager.rs`
- 不要把它收缩回只会修 issue 的专用流程

### 4. Issue repair

文件：

- `rust-src/issue_repair_manager.rs`

现状：

- 这是仍然保留的专用工作流，不等价于通用 Agent

结论：

- 如果用户要的是“主动触发工作流并自由汇报”，优先看通用 Agent，不要只改 issue repair

## 提交前最低检查

至少做这些：

- `cargo check`
- `cargo test`
- 如果改了入口分流、聊天链路、工作流 Agent，再看一眼 `RUST_PORT_PROGRESS.md` 是否需要同步更新
- 如果改了配置字段，同步检查 `config.example.json`

## 不要提交的内容

- `data/` 下运行期生成物
- 下载产物
- 缓存、日志、群表情、临时文件
- 真实 token、私钥、密钥

## 交接时应说明

- 改了哪些行为
- 是否影响通用 Agent、issue repair、群文件下载或聊天链路
- 是否改动配置结构
- 做了哪些本地验证
- 还有哪些路径没验证
