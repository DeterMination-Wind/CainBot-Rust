# CainBot

[![Node.js Version](https://img.shields.io/badge/node-%3E%3D24-brightgreen)](https://nodejs.org/)
[![License: AGPL-3.0](https://img.shields.io/badge/license-AGPL--3.0-blue.svg)](LICENSE)

基于 NapCat OneBot 的 QQ 群聊机器人，支持 AI 问答、翻译、GitHub Release 下载、模组问题修复等功能。

## 功能特性

- **智能群聊问答** - 消息过滤 + AI 自动回复，支持 `@Bot` 和 `/chat` 显式问答
- **多语言翻译** - 支持文本、图片、附件翻译 (`/tr` 或 `#翻译`)
- **GitHub Release 下载** - 识别自然语言请求，自动下载并缓存 Release 文件
- **模组问题修复** - 自动识别并跟进 Mindustry 模组相关问题
- **自动入群** - 自动接受群邀请
- **Codex 文件桥** - 提供 HTTP API 供本地工具调用

## 快速开始

### 环境要求

- Node.js >= 24
- NapCat (OneBot HTTP + SSE)
- AI API (OpenAI 兼容接口)

### 安装

```bash
git clone https://github.com/YOUR_USERNAME/CainBot.git
cd CainBot
npm install
```

### 配置

1. 复制配置模板：

```bash
cp config.example.json config.json
```

2. 编辑 `config.json`，填写必要信息：

```json
{
  "napcat": {
    "baseUrl": "http://127.0.0.1:3000",
    "headers": {
      "Authorization": "Bearer YOUR_ONEBOT_TOKEN"
    }
  },
  "bot": {
    "ownerUserId": "YOUR_QQ_NUMBER"
  },
  "ai": {
    "baseUrl": "http://127.0.0.1:15721/v1",
    "apiKey": "YOUR_API_KEY"
  }
}
```

### 运行

```bash
npm start
```

或使用脚本：

```bash
# Windows
run-cain-bot.bat

# Linux
./run-cain-bot.sh
```

## 命令列表

### 普通用户

| 命令 | 说明 |
|------|------|
| `/help` | 显示帮助 |
| `/chat <文本>` | 与 Bot 对话 |
| `/tr <文本>` | 翻译文本 |
| `#翻译 <文本>` | 翻译文本（备选） |

### 群管理

| 命令 | 说明 |
|------|------|
| `/e 状态` | 查看群配置状态 |
| `/e 过滤 <要求>` | 修改过滤规则 |
| `/e 聊天 <要求>` | 修改聊天规则 |
| `/e 过滤心跳 启用 [N]` | 启用消息节流 |
| `/e 文件下载 启用 [目录]` | 启用文件下载 |

### Bot 主人

| 命令 | 说明 |
|------|------|
| `/e 启用` | 启用群功能 |
| `/e 禁用` | 禁用群功能 |

## 项目结构

```
CainBot/
├── src/                    # 主程序代码
│   ├── index.mjs           # 入口文件
│   ├── napcat-client.mjs   # NapCat 客户端
│   ├── openai-chat-client.mjs
│   └── ...
├── prompts/                # Prompt 模板
├── scripts/                # 辅助脚本
├── data/                   # 运行时数据 (gitignore)
├── config.example.json     # 配置模板
└── package.json
```

## 配置说明

主要配置项：

| 字段 | 说明 |
|------|------|
| `napcat.baseUrl` | NapCat OneBot 地址 |
| `bot.ownerUserId` | Bot 主人 QQ 号 |
| `ai.baseUrl` | AI API 地址 |
| `qa.enabledGroupIds` | 启用群聊的群号列表 |

完整配置见 `config.example.json`。

## 开发

```bash
# 语法检查
npm run check

# 查看日志
cat data/logs/latest.log
```

## 许可证

[AGPL-3.0](LICENSE) - 修改并部署本项目需公开源码。

## 致谢

- [NapCat](https://github.com/NapCatQQ/NapCat) - OneBot 实现
