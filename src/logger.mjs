import fs from 'node:fs/promises';
import path from 'node:path';

const LEVELS = {
  debug: 10,
  info: 20,
  warn: 30,
  error: 40
};

export class Logger {
  constructor(level = 'info', options = {}) {
    this.level = LEVELS[level] ?? LEVELS.info;
    this.nonInfoNotifier = null;
    this.notifyQueue = Promise.resolve();
    this.logDir = typeof options?.logDir === 'string' ? options.logDir.trim() : '';
    this.fileWriteQueue = Promise.resolve();
    this.logDirReady = this.logDir
      ? fs.mkdir(this.logDir, { recursive: true })
      : Promise.resolve();
  }

  debug(...args) {
    this.#write('DEBUG', LEVELS.debug, args);
  }

  info(...args) {
    this.#write('INFO', LEVELS.info, args);
  }

  warn(...args) {
    this.#write('WARN', LEVELS.warn, args);
  }

  error(...args) {
    this.#write('ERROR', LEVELS.error, args);
  }

  setNonInfoNotifier(notifier) {
    this.nonInfoNotifier = typeof notifier === 'function' ? notifier : null;
  }

  async flush() {
    await Promise.allSettled([
      this.notifyQueue,
      this.fileWriteQueue
    ]);
  }

  #write(label, numericLevel, args) {
    if (numericLevel < this.level) {
      return;
    }
    const prefix = `[${new Date().toISOString()}] [${label}]`;
    const text = `${prefix} ${args.map((item) => this.#formatArg(item)).join(' ')}`.trim();
    console.log(prefix, ...args);
    this.#queueFileWrite(text);
    if (numericLevel <= LEVELS.info || !this.nonInfoNotifier) {
      return;
    }

    this.notifyQueue = this.notifyQueue
      .catch(() => {})
      .then(() => this.nonInfoNotifier({
        label: label.toLowerCase(),
        numericLevel,
        prefix,
        args,
        text
      }))
      .catch((error) => {
        const notifyPrefix = `[${new Date().toISOString()}] [WARN]`;
        console.error(notifyPrefix, `非 info 日志通知失败：${error?.stack || error?.message || error}`);
      });
  }

  #queueFileWrite(text) {
    if (!this.logDir) {
      return;
    }
    this.fileWriteQueue = this.fileWriteQueue
      .catch(() => {})
      .then(async () => {
        await this.logDirReady;
        const latestLogPath = path.join(this.logDir, 'latest.log');
        const dailyLogPath = path.join(this.logDir, `${this.#formatLocalDate(new Date())}.log`);
        const line = `${text}\n`;
        await fs.appendFile(latestLogPath, line, 'utf8');
        if (dailyLogPath !== latestLogPath) {
          await fs.appendFile(dailyLogPath, line, 'utf8');
        }
      })
      .catch((error) => {
        const notifyPrefix = `[${new Date().toISOString()}] [WARN]`;
        console.error(notifyPrefix, `日志写入文件失败：${error?.stack || error?.message || error}`);
      });
  }

  #formatLocalDate(date) {
    const year = String(date.getFullYear());
    const month = String(date.getMonth() + 1).padStart(2, '0');
    const day = String(date.getDate()).padStart(2, '0');
    return `${year}-${month}-${day}`;
  }

  #formatArg(value) {
    if (typeof value === 'string') {
      return value;
    }
    if (value instanceof Error) {
      return value.stack || value.message || String(value);
    }
    try {
      return JSON.stringify(value);
    } catch {
      return String(value);
    }
  }
}
