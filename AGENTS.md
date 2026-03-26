# CainBot Agent Notes

本文件给后续在这个仓库里工作的代理用。目标不是介绍通用编程规范，而是避免把 Cain 当前已经修好的行为改坏。

## 项目定位

Cain 是一个基于 NapCat OneBot HTTP + SSE 的本地 QQ 机器人。

核心职责：

- 群消息过滤和 AI 回复
- 显式 `/chat` / `@Cain` 问答
- GitHub release 查询、下载、群文件发送
- 模组问题接管修复
- 自动入群
- 翻译、地图解析、Codex 本地桥

入口：

- `src/index.mjs`

## 目录约定

- `src/`：运行时代码
- `prompts/`：系统 prompt、审核 prompt、角色图
- `data/`：状态、日志、缓存、下载物、长期记忆
- `config.json`：实际运行配置
- `config.example.json`：配置样例

默认日志目录：

- `data/logs/latest.log`
- `data/logs/YYYY-MM-DD.log`

CC Switch 本地数据目录：

- `C:\Users\华硕\.cc-switch`

## 现在不要改坏的行为

- 不要把 AI 报错直接发到群里，除非用户明确要求
- 不要在 `188709300` 之外询问“这是不是模组问题”“要不要继续修”
- 模组修复产物的同步目标仍是 `188709300`
- 自动入群必须保留事件处理和系统消息轮询双保险
- `gpt-5-codex-mini` + CC Switch 时，不要强制只走 `chat/completions`
- 当前 Cain 依赖 `responses` 路径兼容 CC Switch，别把这个兼容性回退掉

## 关键实现点

### 1. 聊天接口

文件：

- `src/openai-chat-client.mjs`

当前事实：

- `CC Switch` 的 `chat/completions` 在某些模型下可能会直接空回复
- 同样的请求走 `responses` 可正常返回
- `gpt-5-codex-mini` 在 CC Switch 内部会映射成可用模型

如果聊天异常，优先查：

- `data/logs/latest.log`
- `C:\Users\华硕\.cc-switch\logs\cc-switch.log`

不要先假设是 Cain prompt 问题。

### 2. 日志

文件：

- `src/logger.mjs`

要求：

- 保留控制台输出
- 同时落盘到 `bot.logDir`
- 关停时必须 `flush()`

不要再把“只靠 cmd 窗口输出”当作日志方案。

### 3. 自动入群

文件：

- `src/index.mjs`
- `src/napcat-client.mjs`

当前逻辑：

- 收到 `request_type=group` 时直接处理
- 启动后和运行中定时轮询群系统消息，补捞邀请

改这块时必须保留两条路径。

### 4. 过滤心跳

文件：

- `src/chat-session-manager.mjs`
- `src/runtime-config-store.mjs`
- `src/index.mjs`

命令：

- `/e 过滤心跳 启用 [N]`
- `/e 过滤心跳 关闭`

要求：

- 只节流“普通候选消息的 AI 过滤”
- 不要影响显式 `/chat` 和 `@Cain`

### 5. 低信息回复拦截

文件：

- `src/index.mjs`

要求：

- 继续拦截“改对应字段”“看相关对象”这类空话
- 群主动回复场景低信息时优先抑制
- 显式问答场景低信息时可以回退为短句

## 配置修改原则

- 修改 `config.json` 时，同步看一眼 `config.example.json`
- 不要把真实 token 提交进仓库
- `replyErrorsToChat` 默认应保持 `false`
- 如果增加新的运行时字段，优先放进 `runtime-config.json` 或 `config.mjs` 的归一化逻辑

## 文档与 prompt

- 角色设定 prompt 现在已经按 Cain / Blueberry / 管理局体系扩充过
- 更新 prompt 时，保留技术约束，不要只剩人设
- 如果群专属 prompt 已写入 `data/runtime-config.json`，修改默认 prompt 时别误覆盖群特化内容

## 修改前后最低检查

至少做这些：

- `node --check src/<edited-file>.mjs`
- `npm run check`
- 如果改了聊天链路，做一次本地直调 `OpenAiChatClient.complete()`
- 如果改了启动或日志，确认 `data/logs/latest.log` 有新记录
- 如果改了邀请逻辑，至少确认启动日志正常、SSE 已连接

## 不要做的事

- 不要用破坏性 git 命令清理用户改动
- 不要把 `data/` 整体当临时目录直接删
- 不要把 CC Switch 的上游 provider 错误误判成 Cain 自己的 JSON 拼装错误
- 不要恢复成“聊天失败就把完整报错发到群里”

## 当前已知运行命令

启动：

- `npm start`
- `node src/index.mjs`
- `run-cain-bot.bat`
- `run-cain-service.bat`

检查：

- `npm run check`
- `node --check src/index.mjs`
- `Get-Content .\data\logs\latest.log -Tail 100`

## 交接时应说明

- 改了哪些行为
- 是否动了 prompt / 配置结构 / 运行时数据格式
- 做了哪些本地验证
- 还有哪些路径没验证
