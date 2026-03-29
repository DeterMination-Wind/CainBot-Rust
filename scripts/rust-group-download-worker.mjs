import process from 'node:process';
import readline from 'node:readline';
import { fileURLToPath } from 'node:url';

import { loadConfig } from '../src/config.mjs';
import { RuntimeConfigStore } from '../src/runtime-config-store.mjs';
import { NapCatClient } from '../src/napcat-client.mjs';
import { OpenAiChatClient } from '../src/openai-chat-client.mjs';
import { GroupFileDownloadManager } from '../src/group-file-download-manager.mjs';

function writeStderr(...args) {
  const line = args.map((item) => {
    if (typeof item === 'string') {
      return item;
    }
    try {
      return JSON.stringify(item);
    } catch {
      return String(item);
    }
  }).join(' ');
  process.stderr.write(`${line}\n`);
}

function createWorkerLogger() {
  const log = (level, ...args) => {
    writeStderr(`[group-download-worker] [${level}]`, ...args);
  };
  return {
    debug: (...args) => log('debug', ...args),
    info: (...args) => log('info', ...args),
    warn: (...args) => log('warn', ...args),
    error: (...args) => log('error', ...args),
    flush: async () => {},
    setNonInfoNotifier: () => {}
  };
}

async function main() {
  const configPath = String(process.argv[2] ?? '').trim();
  if (!configPath) {
    throw new Error('缺少配置文件路径参数');
  }

  const workerLogger = createWorkerLogger();
  const loaded = await loadConfig(configPath);
  const runtimeConfigStore = new RuntimeConfigStore(
    loaded.config.bot.runtimeConfigFile,
    loaded.configDir,
    {
      qaExternalExclusiveGroupsFile: loaded.config.qa.externalExclusiveGroupsFile,
      qaExternalExclusiveGroupsRefreshMs: loaded.config.qa.externalExclusiveGroupsRefreshMs
    },
    workerLogger
  );
  await runtimeConfigStore.load();

  const napcatClient = new NapCatClient(loaded.config.napcat, workerLogger);
  const qaClient = new OpenAiChatClient(loaded.config.qa.client, workerLogger);
  const manager = new GroupFileDownloadManager(
    loaded.config.qa.answer,
    runtimeConfigStore,
    napcatClient,
    workerLogger,
    {
      downloadRoot: fileURLToPath(new URL('../data/release-downloads/', import.meta.url)),
      chatClient: qaClient,
      platformClassifyModel: loaded.config.qa.platformClassifyModel
    }
  );

  const rl = readline.createInterface({
    input: process.stdin,
    crlfDelay: Infinity
  });

  for await (const line of rl) {
    const trimmed = String(line ?? '').trim();
    if (!trimmed) {
      continue;
    }

    let request = null;
    try {
      request = JSON.parse(trimmed);
      if (request?.action === 'shutdown') {
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, stopped: true })}\n`);
        break;
      }

      // 每次请求前重载运行时配置，保证群开关/文件夹名修改后无需重启 worker。
      await runtimeConfigStore.load().catch((error) => {
        workerLogger.warn(`重载运行时配置失败：${error?.message ?? error}`);
      });

      if (request?.action === 'handle_group_message') {
        const handled = await manager.handleGroupMessage(
          request.payload?.context ?? {},
          request.payload?.event ?? {},
          String(request.payload?.text ?? '')
        );
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, handled })}\n`);
        continue;
      }

      if (request?.action === 'start_group_download_flow_from_tool') {
        const handled = await manager.startGroupDownloadFlowFromTool(
          request.payload?.context ?? {},
          request.payload?.event ?? {},
          request.payload?.request ?? {}
        );
        process.stdout.write(`${JSON.stringify({ id: request.id ?? null, ok: true, handled })}\n`);
        continue;
      }

      process.stdout.write(`${JSON.stringify({ id: request?.id ?? null, ok: false, error: `unknown action: ${request?.action ?? ''}` })}\n`);
    } catch (error) {
      writeStderr(`[group-download-worker] [error]`, error?.stack || error?.message || error);
      process.stdout.write(`${JSON.stringify({
        id: request?.id ?? null,
        ok: false,
        error: String(error?.message ?? error ?? 'unknown error')
      })}\n`);
    }
  }
}

await main().catch((error) => {
  writeStderr('[group-download-worker] [fatal]', error?.stack || error?.message || error);
  process.exitCode = 1;
});
