# NapCat Cain Bot

这是一个基于 NapCat OneBot HTTP + SSE 的本地 QQ 机器人。当前版本只保留以下能力：

- 指定群内的问题过滤 + 自动 AI 回复 + 显式问答
- DeterMination 名下模组的 bug / 体验问题自动接管修复跟踪
- `/chat` 与 `@[Bot]Cain` 共享同一群上下文
- `/e` 修改当前群的过滤 prompt 和聊天 prompt
- `/tr` 与 `#翻译` 文本/图片翻译
- `.msav` 地图文件解析与后续追问
- Codex 本地消息 / 文件桥
- 独立的 GitHub release 安装包查询 / 下载 / 发群文件流程
- NapCat WebUI 里的 Cain 监控面板，可查看长期记忆、群时间线和相关文件

不再保留：

- GitHub Release 跟踪
- GitHub 自动化与 `$` / `$$` 指令
- 本地文件夹同步到群文件
- 伪装模式
- 每日群聊日报
- `/chatFull`、`/chatClear`
- 旧的“每个用户一个上下文”
- 表情归档/识别

## 现在的群问答流程

仅对 `qa.enabledGroupIds` 或运行时 `/e 启用` 过的群生效。

1. 群友发送一条普通消息
2. `ai-1 (gpt-5.4-mini)` 根据当前群的过滤 prompt 判断这是不是值得 Cain 直接回答的问题
3. 如果通过，Cain 直接调用聊天模型回复，不再先发 `[自动回复]...` 提示
4. 群里 `@[Bot]Cain` 或发送 `/chat ...` 仍然会强制进入显式问答
5. `@[Bot]Cain` 与 `/chat` 共用同一份群上下文
6. 如果群里有人明显反对 bot 发言，例如“这是谁的机器人”“这机器人好吵”，会由 `gpt-5.4-mini` 判定是否发起关闭投票；当至少 3 个不同成员回复投票消息且内容含独立 `Y` 时，会关闭该群聊天功能
7. 如果群里 15 分钟没人发消息，就把最后 30 条消息交给 `ai-5 (gpt-5.4-mini)` 判断话题是否结束；若结束，只清上下文，不发言
8. 收到 `request_type=group` 且 `sub_type=invite` 的邀请请求时，会自动同意入群
9. 如果某群启用了“文件下载”功能，Cain 会独立检测类似“有没有156的pc安装包”这类消息；它会先问你是 X端 还是 原版，再按 tag/body 查 release，列出该 tag 下可用文件让你选，随后下载到本地并通过 QQ 群文件发出
10. 无论群主动回复有没有启用，只要消息被判定为在反馈 `../codex` 下作者包含 `DeterMination` 的模组 bug / 体验问题，Cain 都会先问要不要直接跟进修；确认后会启动 `gpt-5.4-high` 的 Codex 修复会话，按需改代码、构建本地产物、发给提问者测试，直到 AI 判断用户已经确认“可以了”，再把当前产物同步到 `188709300` 群文件对应文件夹并关闭会话

默认过滤标准：

- 与 Mindustry / mdt / 牡丹亭游戏有关的问题

默认回答 prompt：

- 优先通过只读查阅本地文件、配置、代码和文档来回答

## QQ 命令

普通用户：

- `/help`
- `/chat 文本`
- 引用一条消息后发送 `/chat`
- `/tr 文本`
- `#翻译 文本`
- 引用一条消息后发送 `/tr`
- 引用一条消息后发送 `#翻译`

群管理命令：

- `/e 状态`
- `/e 过滤 <要求>`
- `/e 聊天 <要求>`
- `/e 文件下载 启用 [群文件夹名]`
- `/e 文件下载 关闭`

主人命令：

- `/e 启用`
- `/e 禁用`

说明：

- `/e 过滤 ...` 和 `/e 聊天 ...` 仅当前群群主、管理员或 bot 主人可用
- `/e 文件下载 启用 [群文件夹名]|关闭` 仅当前群群主、管理员或 bot 主人可用
- 修改请求会先交给 `ai-2 (gpt-5.4-mini)` 审核和润色，再落盘保存
- `/tr` 与 `#翻译` 支持当前消息图片、引用消息图片、文本附件
- 发送 `.msav` 文件后，Cain 会自动解析地图；回复那条介绍消息时会继续沿同一地图上下文对话
- 文件下载流程独立于 `/chat`、群问答总开关和主动聊天开关
- `.apk` 资产在下载转发时会以 `.APK` 文件名上传
- 如果某个 tag 的某个文件已经下载过且本地缓存大小匹配，会直接复用本地文件，不重复下载
- 下载 GitHub 资产前会读取 NapCatShell 的镜像名单并测速，按延迟选出前 5 个镜像并发下载；谁先下完就终止其余下载并清理临时文件。若累计 3 次超时仍未成功，才退出并回退失败

## 配置

最重要的是这几个字段：

- `napcat.baseUrl`
- `napcat.headers.Authorization`
- `ai.baseUrl`
- `ai.apiKey`
- `qa.enabledGroupIds`
- `issueRepair.publishGroupId`
- `issueRepair.codexRoot`

`config.example.json` 已经是当前结构。

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

所有接口都只允许本机访问；如果配置了 `codexBridge.token`，还需要带 `Authorization: Bearer <token>`。

发群文件示例：

```json
{
  "groupId": "123456789",
  "filePath": "C:\\Users\\华硕\\Downloads\\example.zip",
  "fileName": "example.zip",
  "folderName": "发布文件",
  "notifyText": "已上传新文件"
}
```

发群文件到指定目录示例：

```json
{
  "groupId": "123456789",
  "filePath": "C:\\Users\\华硕\\Downloads\\example.zip",
  "fileName": "example.zip",
  "folderName": "发布文件"
}
```

发群消息示例，支持 `atUserIds` 和引用：

```json
{
  "groupId": "123456789",
  "text": "帮你看过了，这是最新版。",
  "atUserIds": ["2712706502", "12345678"],
  "replyToMessageId": "987654321"
}
```

读群消息示例：

```json
{
  "groupId": "123456789",
  "count": 30
}
```

读私聊消息示例：

```json
{
  "userId": "2712706502",
  "count": 20
}
```

读本地文件示例：

```json
{
  "path": "C:\\Users\\华硕\\Documents\\NapCatCainBot\\README.md",
  "startLine": 1,
  "endLine": 80,
  "maxChars": 12000
}
```

## NapCat WebUI 面板

如果 `napcat-plugin-cain-monitor` 已启用，可在 NapCat WebUI 打开：

- `/plugin/napcat-plugin-cain-monitor/page/dashboard`

现在面板里可以直接查看：

- 长期记忆文件
- QA 群开关
- 群级 prompt 覆盖
- 每个群的聊天时间线
- 每个群对应的运行时配置和日报文件预览
- `.msav` 解析任务状态

## 启动

```powershell
cd C:\Users\华硕\Documents\NapCatCainBot
npm start
```

或者双击：

- `run-cain-bot.bat`
- `run-cain-service.bat`
