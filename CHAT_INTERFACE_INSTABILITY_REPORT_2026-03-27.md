# Cain 聊天接口不稳定排查报告

日期：2026-03-27  
项目：`C:\Users\华硕\Documents\NapCatCainBot`

## 结论

这次“聊天接口不稳定”不是单一故障，而是三层问题叠加：

1. Cain 之前一度被强制走 `CC Switch /v1/chat/completions`，而这条兼容路径在当前环境下会出现空回复或直接断连接。
2. CC Switch 自己的 `codex` provider 池里存在失效上游，包含 `401 无效的API Key`、`502`、以及错误的请求格式回退尝试，导致 failover 抖动。
3. OpenAI 兼容 `responses` 路径在不同代理上的输入格式兼容性并不一致，错误格式会触发 `400`，进而把本来可用的链路拖进不必要的失败切换。

当前最稳定的路径是：

- Cain → `http://127.0.0.1:15721/v1/responses`
- `input` 使用列表格式
- 内容类型使用 `input_text`

本地已验证该路径可连续成功返回结果。

## 现象与证据

### 1. Cain 本地日志

文件：

- `C:\Users\华硕\Documents\NapCatCainBot\data\logs\latest.log`

在 2026-03-27 01:17 前后，Cain 记录到的典型错误是：

- `fetch failed (UND_ERR_SOCKET: other side closed)`
- 调用栈落在 `src/openai-chat-client.mjs` 的 `#completeViaChatCompletions`

这说明 Cain 已经把请求发到本地代理，但本地代理没有返回完整响应体。

### 2. CC Switch 日志

文件：

- `C:\Users\华硕\.cc-switch\logs\cc-switch.log`

日志显示了几类不同失败：

- `https://right.codes/codex/v1/chat/completions (model=gpt-4o-mini)` 返回 `401 无效的API Key`
- 同一路径下还出现 `400: 端点/codex未配置模型gpt-4o-mini`
- `https://laofoye.helpaio.com/v1/chat/completions` 返回 `502`
- 某些 `responses` 请求使用了错误载荷时，返回：
  - `Input must be a list`
  - `Invalid value: 'text'. Supported values are: 'input_text' ...`

同时也能看到成功请求：

- `https://right.codes/codex/v1/responses (model=gpt-5-codex-mini)`

这说明：

- CC Switch 本身是通的
- 不是所有上游都坏
- 问题集中在“路径选择 + 请求格式 + 失效 provider 池”

### 3. CC Switch 数据库

数据库：

- `C:\Users\华硕\.cc-switch\cc-switch.db`

确认到的事实：

- 当前 `codex` live provider 是 `RightCode`
- `proxy_config` 中 `codex` 的 `auto_failover_enabled = 1`
- `proxy_request_logs` 里，`request_model = gpt-5-codex-mini` 的成功记录会被落成：
  - `model = gpt-5.1-codex-mini`
  - `status_code = 200`

这说明当前 CC Switch 会把 `gpt-5-codex-mini` 映射到一个可工作的实际模型。

同一数据库里也能看到失败记录：

- `request_model = gpt-4o-mini` 时多次 `401`
- provider health 里还有 `AIO` 的 `502`、若干 `RightCode` 的 `401`

因此“模型完全不可用”这个判断是不成立的，真正的问题是：

- 有的路径可用
- 有的路径不可用
- failover 池不干净

## 本地复现结果

### 直接打 `chat/completions`

对 `http://127.0.0.1:15721/v1/chat/completions` 直接请求 `gpt-5-codex-mini` 时，曾复现：

- `Empty reply from server`

这与 Cain 日志中的 `UND_ERR_SOCKET: other side closed` 一致。

### 直接打 `responses`

使用不同 `input` 结构时表现不同：

- `input: "ping"`：可复现空回复
- `input` 列表但内容类型是 `text`：会触发 `400`
- `input` 列表且内容类型是 `input_text`：成功，返回 SSE，并输出 `pong`

这条路径也已经通过 Cain 自己的 `OpenAiChatClient.complete()` 连续 5 次验证成功。

## 根因拆分

### 根因 1：错误地依赖了不稳定的 `chat/completions` 路径

Cain 曾为 CC Switch 特判成优先甚至只走 `chat/completions`。  
在当前这台机器、当前这版 CC Switch、当前这组 provider 下，这条路径并不稳定，会直接断连接。

### 根因 2：CC Switch 的 codex failover 池含有坏上游

已确认的坏上游行为：

- `RightCode` 某些分支返回 `401 无效的API Key`
- `AIO` 返回 `502`
- 某些模型与端点组合不兼容

这会导致：

- 本地代理抖动
- 频繁熔断 / 半开 / 切换
- 某些错误会被包装成“本地空回复”

### 根因 3：`responses` 请求格式必须命中代理支持的那一档

当前环境里真正可用的是：

- `input` 为数组
- 文本内容类型为 `input_text`

错误格式会被上游立刻拒绝，进而干扰整个 failover。

## 已做修复

### 1. 恢复并固定 CC Switch 优先走 `responses`

文件：

- `C:\Users\华硕\Documents\NapCatCainBot\src\openai-chat-client.mjs`

当前逻辑：

- 如果 base URL 是本机 CC Switch `127.0.0.1:15721/v1`
- 优先顺序为 `responses -> chat`

### 2. 增加模型别名兜底

为 codex 模型增加了本地候选别名：

- `gpt-5-codex-mini -> gpt-5.1-codex-mini`
- `gpt-5-codex -> gpt-5.1-codex`

用途：

- 当代理或上游对别名解析不稳定时，Cain 可以本地回退到可用候选模型

### 3. 增加传输层本地熔断

当某个 transport 连续表现出：

- socket 断开
- 空响应
- 无法解析的无效响应

Cain 会临时熔断该 transport，避免每次请求都继续撞同一条坏链路。

当前主要保护对象：

- `chat`

### 4. 增加 `responses` 载荷层兜底

Cain 现在会区分不同 `responses` 载荷策略，并在必要时回退，而不是把所有失败都当成同一类网络故障。

当前策略：

- 优先：结构化 `input_text` 列表
- 非 CC Switch 场景下，必要时才尝试退化为扁平字符串输入

这样做的目的：

- 对 CC Switch 保持当前已验证的最稳格式
- 对其他兼容代理保留一条低保真兜底路

### 5. 保留已有的重试、传输切换和冷却逻辑

仍然保留：

- 单 transport 重试
- `responses` / `chat` 之间切换
- 非代理场景下的本地冷却

但现在这些机制不再是“盲重试”，而是带有本地熔断和模型/载荷兜底。

## 当前验证结果

验证时间：2026-03-27 01:36 至 01:37

### 代码检查

- `node --check src/openai-chat-client.mjs` 通过

### 直调验证

用 `OpenAiChatClient.complete()` 对当前配置连续请求 5 次，全部成功返回：

- `RUN1=pong`
- `RUN2=pong`
- `RUN3=pong`
- `RUN4=pong`
- `RUN5=pong`

### CC Switch 侧验证

在同一时间段，CC Switch 日志出现的是：

- `https://right.codes/codex/v1/responses (model=gpt-5-codex-mini)`

没有再次出现本次直调对应的 `chat/completions` 空回复链路。

## 仍然存在的外部风险

这次已经把 Cain 侧能兜的都补上了，但 CC Switch 外部风险仍存在：

1. `RightCode` 的部分 provider 记录仍会返回 `401`
2. `AIO` provider 仍会返回 `502`
3. CC Switch 的 `codex` provider 池里存在历史失败节点
4. 某些模型的定价信息缺失，只影响统计，不影响实际可用性

这些问题属于代理或上游配置层，不是 Cain 代码本身能彻底修复的。

## 建议的后续动作

### 建议 1：清理 CC Switch 的 codex provider 池

优先保留当前已验证可用的 live provider，清掉反复出现：

- `401`
- `502`
- 模型配置不兼容

的备用节点。

### 建议 2：继续观察 Cain 日志

重点看：

- `data/logs/latest.log`
- 是否再次出现 `UND_ERR_SOCKET: other side closed`

如果只剩上游 `401/502` 而 Cain 本地不再报 socket 断链，说明 Cain 侧兜底已生效。

### 建议 3：遇到失败先分层定位

以后排障建议按这个顺序查：

1. Cain 日志是否是 `fetch failed`
2. CC Switch 日志里实际转发到了哪个 URL
3. 失败发生在：
   - `chat/completions`
   - `responses`
   - provider 上游
4. 再决定是改 Cain、改 CC Switch、还是改 provider 配置

## 相关文件

- `C:\Users\华硕\Documents\NapCatCainBot\src\openai-chat-client.mjs`
- `C:\Users\华硕\Documents\NapCatCainBot\data\logs\latest.log`
- `C:\Users\华硕\.cc-switch\logs\cc-switch.log`
- `C:\Users\华硕\.cc-switch\cc-switch.db`

## 参考链接

- CC Switch 仓库：<https://github.com/farion1231/cc-switch>
- OpenAI Responses API 文档：<https://platform.openai.com/docs/api-reference/responses>
