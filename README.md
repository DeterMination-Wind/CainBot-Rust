# NapCat Cain Bot

Cain 是一个基于 NapCat OneBot HTTP + SSE 的本地 QQ 机器人，主要负责群聊问答、翻译、GitHub release 文件下载转发、模组问题接管修复，以及本地 Codex 文件桥。

当前仓库的运行形态是单进程 Node.js 服务，入口为 `src/index.mjs`。NapCat 负责 QQ 收发，Cain 负责策略、会话、AI 请求、文件处理和运行时配置。

## 当前能力

- 指定群的消息过滤 + 自动 AI 回复
- `@[Bot]Cain` 与 `/chat` 显式问答，复用同一群上下文
- `/e` 修改当前群的过滤 prompt、聊天 prompt、过滤心跳、文件下载开关
- `/tr` 与 `#翻译` 的文本、图片、文本附件翻译
- `.msav` 地图解析和后续追问
- GitHub release 查询、下载、本地缓存、群文件发送
- 检测并自动同意群邀请
- DeterMination 相关模组问题接管修复，并只在 `188709300` 群发布/询问后续
- Codex 本地消息 / 文件桥
- 持久化日志，默认写入 `data/logs/latest.log`

## 当前约束

- 默认不会把 AI 报错直接发到群里，`config.bot.replyErrorsToChat` 默认是 `false`
- 是否要把问题视为“模组问题并继续追修”的跟进询问，只允许发到 `188709300`
- `CC Switch` 作为 OpenAI 兼容代理时，Cain 现在优先走 `/v1/responses`
- `gpt-5-codex-mini` 在当前 CC Switch 下会被代理映射到可用模型，已实测可返回结果

## 群问答流程

仅对 `qa.enabledGroupIds` 或运行时执行过 `/e 启用` 的群生效。

1. 收到普通群消息。
2. 先按当前群的过滤心跳决定这条候选消息是否需要进入 AI 过滤。
3. 若进入过滤，则由过滤模型判断这是不是值得 Cain 回答的问题。
4. 通过后直接调用聊天模型生成回复，不再先发提示语。
5. `@[Bot]Cain` 或 `/chat ...` 会强制进入显式问答，不受主动回复开关影响。
6. 回复发出前会再过一层“低信息回复拦截”，避免把空话、复述话发到群里。
7. 群内明显反对 bot 发言时，会触发关闭投票；达到阈值后关闭该群聊天功能。
8. 群长期空闲后会做话题结束判定，只清上下文，不发结束语。

## 自动入群

- 收到 `request_type=group` 的邀请请求会自动同意
- 启动时和运行中还会轮询 NapCat 的群系统消息，补捞漏掉的邀请
- 相关日志会写入 `data/logs/latest.log`

## 文件下载

Cain 可以识别类似“最新版 release 下载”“某版本 pc 包”“NapCatQQ 最新 release”这类请求。

当前行为：

- 支持 GitHub 仓库 URL 和 `owner/repo`
- 支持读取最新 release 或指定版本/tag
- 下载结果有本地缓存，避免重复下载
- 下载完成后可通过 QQ 群文件发送
- 可配合群文件夹名把文件发到指定群目录

如果某群关闭了“文件下载”功能，则不会自动处理此类消息。

## 模组问题接管修复

当消息被判定为 `../codex` 下相关模组的 bug / 体验问题时，Cain 会进入接管修复流程。

当前规则：

- 群内不直接广播 AI 报错
- 是否继续当作模组问题追问，只在 `188709300` 发出
- 产物同步目标群也是 `188709300`

## 过滤心跳

为降低 token 消耗，当前群可启用“过滤心跳”。

启用后：

- 不是每条候选消息都送进 AI 过滤
- 只有每累计 `N` 条候选消息，才会送一次过滤模型审核
- `@[Bot]Cain` 和 `/chat` 不受这个节流影响

命令：

- `/e 过滤心跳 启用 [N]`
- `/e 过滤心跳 关闭`

## 命令

普通用户：

- `/help`
- `/chat 文本`
- 引用一条消息后发送 `/chat`
- `/tr 文本`
- `#翻译 文本`
- 引用一条消息后发送 `/tr`
- 引用一条消息后发送 `#翻译`

群管理：

- `/e 状态`
- `/e 过滤 <要求>`
- `/e 聊天 <要求>`
- `/e 过滤心跳 启用 [N]`
- `/e 过滤心跳 关闭`
- `/e 文件下载 启用 [群文件夹名]`
- `/e 文件下载 关闭`

主人命令：

- `/e 启用`
- `/e 禁用`

说明：

- `/e 过滤`、`/e 聊天`、`/e 过滤心跳`、`/e 文件下载` 仅当前群群主、管理员或 bot 主人可用
- `/e 启用` 与 `/e 禁用` 仅 bot 主人可用
- prompt 修改会先经过 AI 审核与润色，再落盘到运行时配置

## 配置文件

主配置文件为 `config.json`，样例见 `config.example.json`。

常用字段：

- `napcat.baseUrl`
- `napcat.eventBaseUrl`
- `napcat.headers.Authorization`
- `bot.ownerUserId`
- `bot.replyErrorsToChat`
- `bot.logDir`
- `ai.baseUrl`
- `ai.apiKey`
- `qa.enabledGroupIds`
- `qa.answer.model`
- `issueRepair.publishGroupId`
- `issueRepair.offerGroupIds`
- `issueRepair.codexRoot`

当前默认日志目录：

- `data/logs/latest.log`
- `data/logs/YYYY-MM-DD.log`

运行时配置文件：

- `data/runtime-config.json`

状态文件：

- `data/state.json`

## AI / CC Switch 说明

当前常用模型配置已经切到 `gpt-5-codex-mini`。

如果 `ai.baseUrl` 指向本机 `CC Switch`，当前建议：

- 使用 `http://127.0.0.1:15721/v1`
- 优先走 `/v1/responses`
- 不要强制只走 `/v1/chat/completions`

这次排查中确认到：

- `gpt-5-codex-mini` 通过 CC Switch 的 responses 路径可正常返回结果
- 某些 provider 链路仍可能出现 `401 无效的API Key` 或上游 `502`
- 这些上游错误会记在 `C:\Users\华硕\.cc-switch\logs\cc-switch.log`

## Codex 文件桥

保留接口：

- `GET /codex/health`
- `POST /codex/send-group-file`
- `POST /codex/send-group-file-to-folder`
- `POST /codex/send-group-message`
- `POST /codex/send-private-message`
- `POST /codex/read-group-messages`
- `POST /codex/read-private-messages`
- `POST /codex/read-file`

所有接口只允许本机访问；如果配置了 `codexBridge.token`，需要附带：

- `Authorization: Bearer <token>`

## 目录说明

- `src/`：主程序代码
- `prompts/`：系统 prompt、审核 prompt、角色图
- `data/`：状态、缓存、日志、下载文件、长期记忆
- `scripts/`：辅助脚本
- `run-cain-bot.bat`：直接启动 Cain
- `run-cain-service.bat`：先拉起 NapCat，再拉起 Cain

## 启动

PowerShell：

```powershell
cd C:\Users\华硕\Documents\NapCatCainBot
npm start
```

只启动 Cain：

- 双击 `run-cain-bot.bat`

连同 NapCat 一起启动：

- 双击 `run-cain-service.bat`

## 检查

语法检查：

```powershell
npm run check
node --check src\openai-chat-client.mjs
```

查看日志：

```powershell
Get-Content .\data\logs\latest.log -Tail 100
```

## 排障

如果“聊天接口又不能用了”，优先检查：

1. `config.json` 中的 `ai.baseUrl` 和 `ai.apiKey`
2. `data/logs/latest.log`
3. `C:\Users\华硕\.cc-switch\logs\cc-switch.log`
4. 本机 `127.0.0.1:15721` 是否仍由 CC Switch 监听

如果群里没有自动接受邀请，优先检查：

1. NapCat 是否正常返回 `request_type=group`
2. Cain 是否已连接 SSE
3. `data/logs/latest.log` 里是否有邀请处理日志

## 维护备注

- `data/` 下内容默认视为运行态数据，不要随意清空
- 不要把真实 token、QQ 号授权或 API Key 提交到仓库
- 修改 prompt 或群级运行时配置前，先确认是否会影响 `188709300` 的修复流程限制
