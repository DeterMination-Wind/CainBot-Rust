const LEVELS = {
  debug: 10,
  info: 20,
  warn: 30,
  error: 40
};

export class Logger {
  constructor(level = 'info') {
    this.level = LEVELS[level] ?? LEVELS.info;
    this.nonInfoNotifier = null;
    this.notifyQueue = Promise.resolve();
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

  #write(label, numericLevel, args) {
    if (numericLevel < this.level) {
      return;
    }
    const prefix = `[${new Date().toISOString()}] [${label}]`;
    console.log(prefix, ...args);
    if (numericLevel <= LEVELS.info || !this.nonInfoNotifier) {
      return;
    }

    const text = `${prefix} ${args.map((item) => this.#formatArg(item)).join(' ')}`.trim();
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
