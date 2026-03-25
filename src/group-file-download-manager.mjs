import fs from 'node:fs/promises';
import path from 'node:path';
import { execFile } from 'node:child_process';
import { promisify } from 'node:util';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import { ensureDir } from './utils.mjs';

const execFileAsync = promisify(execFile);

const SESSION_TTL_MS = 5 * 60 * 1000;
const MAX_RELEASE_PAGES = 4;
const RELEASES_PER_PAGE = 100;
const GITHUB_API_BASE_URL = 'https://api.github.com';
const MAX_CONCURRENT_DOWNLOADS = 5;
const GITHUB_DOWNLOAD_MIRRORS = [
  'https://github.chenc.dev',
  'https://ghproxy.cfd',
  'https://github.tbedu.top',
  'https://ghproxy.cc',
  'https://gh.monlor.com',
  'https://cdn.akaere.online',
  'https://gh.idayer.com',
  'https://gh.llkk.cc',
  'https://ghpxy.hwinzniej.top',
  'https://github-proxy.memory-echoes.cn',
  'https://git.yylx.win',
  'https://gitproxy.mrhjx.cn',
  'https://gh.fhjhy.top',
  'https://gp.zkitefly.eu.org',
  'https://gh-proxy.com',
  'https://ghfile.geekertao.top',
  'https://j.1lin.dpdns.org',
  'https://ghproxy.imciel.com',
  'https://github-proxy.teach-english.tech',
  'https://gitproxy.click',
  'https://gh.927223.xyz',
  'https://github.ednovas.xyz',
  'https://ghf.xn--eqrr82bzpe.top',
  'https://gh.dpik.top',
  'https://gh.jasonzeng.dev',
  'https://gh.xxooo.cf',
  'https://gh.bugdey.us.kg',
  'https://ghm.078465.xyz',
  'https://j.1win.ggff.net',
  'https://tvv.tw',
  'https://gitproxy.127731.xyz',
  'https://gh.inkchills.cn',
  'https://ghproxy.cxkpro.top',
  'https://gh.sixyin.com',
  'https://github.geekery.cn',
  'https://git.669966.xyz',
  'https://gh.5050net.cn',
  'https://gh.felicity.ac.cn',
  'https://github.dpik.top',
  'https://ghp.keleyaa.com',
  'https://gh.wsmdn.dpdns.org',
  'https://ghproxy.monkeyray.net',
  'https://fastgit.cc',
  'https://gh.catmak.name',
  'https://gh.noki.icu'
];
const DOWNLOAD_TIMEOUT_ABORT_LIMIT = 3;

function clampInteger(value, fallback, min, max) {
  const numeric = Number(value);
  if (!Number.isFinite(numeric)) {
    return fallback;
  }
  return Math.min(max, Math.max(min, Math.trunc(numeric)));
}

function escapeRegExp(value) {
  return String(value ?? '').replace(/[.*+?^${}()|[\]\\]/g, '\\$&');
}

function normalizeText(value) {
  return String(value ?? '').trim();
}

function sanitizePathSegment(value) {
  return normalizeText(value).replace(/[<>:"/\\|?*\x00-\x1F]/g, '_') || 'unknown';
}

function withUppercaseApkName(fileName) {
  const normalized = normalizeText(fileName);
  if (!normalized) {
    return 'download.bin';
  }
  if (/\.apk$/i.test(normalized)) {
    return normalized.replace(/\.apk$/i, '.APK');
  }
  return normalized;
}

function createRepoInfo(choice) {
  if (choice === 'x') {
    return {
      choice,
      owner: 'TinyLake',
      repo: 'MindustryX',
      fullName: 'TinyLake/MindustryX',
      displayName: 'MindustryX X端'
    };
  }
  return {
    choice: 'vanilla',
    owner: 'Anuken',
    repo: 'Mindustry',
    fullName: 'Anuken/Mindustry',
    displayName: 'Mindustry 原版'
  };
}

function parseRepoChoice(text) {
  const normalized = normalizeText(text).toLowerCase();
  if (!normalized) {
    return '';
  }
  if (/(mindustryx|tinylake|x端)/i.test(normalized)) {
    return 'x';
  }
  if (/(anuken|原版|官版|官方原版|原版mdt|原版mindustry)/i.test(normalized)) {
    return 'vanilla';
  }
  return '';
}

function normalizeRepoChoiceInput(value) {
  const normalized = normalizeText(value).toLowerCase();
  if (!normalized) {
    return '';
  }
  if (normalized === 'x' || /(tinylake\/mindustryx|mindustryx|x端)/i.test(normalized)) {
    return 'x';
  }
  if (normalized === 'vanilla' || /(anuken\/mindustry|原版|官版|官方原版|原版mdt|原版mindustry)/i.test(normalized)) {
    return 'vanilla';
  }
  return '';
}

function parseVersionQuery(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return '';
  }
  if (/(最新版|最新版本|最新的|最新包|最新安装包|最新客户端)/i.test(normalized)) {
    return 'latest';
  }
  const matches = Array.from(
    normalized.matchAll(/(^|[^\d])v?(\d{2,3}(?:\.\d+){0,3}(?:[-._]?(?:rc|beta|alpha|pre)\d*)?)(?=$|[^\d])/ig)
  );
  if (matches.length === 0) {
    return '';
  }
  return String(matches[0]?.[2] ?? '').trim();
}

function parsePlatformHint(text) {
  const normalized = normalizeText(text).toLowerCase();
  if (!normalized) {
    return '';
  }
  if (/(电脑版|pc|desktop|windows|window|win|exe|jar|zip)/i.test(normalized)) {
    return 'pc';
  }
  if (/(安卓|android|apk|手机|移动端)/i.test(normalized)) {
    return 'android';
  }
  return '';
}

function normalizePlatformHintInput(value) {
  const normalized = normalizeText(value).toLowerCase();
  if (!normalized) {
    return '';
  }
  if (normalized === 'pc' || /(电脑版|pc|desktop|windows|window|win|exe|jar|zip)/i.test(normalized)) {
    return 'pc';
  }
  if (normalized === 'android' || /(安卓|android|apk|手机|移动端)/i.test(normalized)) {
    return 'android';
  }
  return '';
}

function normalizeVersionQueryInput(value) {
  const normalized = normalizeText(value);
  if (!normalized) {
    return '';
  }
  if (/^latest$/i.test(normalized)) {
    return 'latest';
  }
  return parseVersionQuery(normalized) || normalized;
}

function looksLikeGameMention(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return false;
  }
  return /(mindustryx|mindustry|mdt|牡丹亭|x端|原版)/i.test(normalized);
}

function looksLikeDownloadRequest(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return false;
  }
  const installLike = /(安装包|安装文件|客户端|下载包|pc安装包|pc包|apk|APK|exe|jar|zip|桌面版|电脑版|电脑版本|pc版本|桌面端|版本包|游戏包|文件包)/i.test(normalized);
  const requestLike = /(有没有|有吗|求|求发|发一下|发个|给我|来个|想要|下载|整一个|能发|有无|有没有人发|谁有|发我|来一份|是哪个|哪个包|哪一个)/i.test(normalized);
  const versionLike = Boolean(parseVersionQuery(normalized));
  const gameLike = looksLikeGameMention(normalized);
  if (installLike && (requestLike || versionLike)) {
    return true;
  }
  if (requestLike && versionLike && gameLike) {
    return true;
  }
  if (requestLike && gameLike && /(电脑|pc|桌面|安卓|android)/i.test(normalized)) {
    return true;
  }
  return false;
}

function parseInitialIntent(text) {
  return {
    matched: looksLikeDownloadRequest(text),
    repoChoice: parseRepoChoice(text),
    versionQuery: parseVersionQuery(text),
    platformHint: parsePlatformHint(text)
  };
}

function parseLatestConfirmation(text) {
  const normalized = normalizeText(text).toLowerCase();
  if (!normalized) {
    return '';
  }
  if (/^(y|yes|是|要|要的|行|可以|那就最新|最新版|最新版本|最新)$/.test(normalized)) {
    return 'yes';
  }
  if (/^(n|no|不是|不要|算了|不用|不)$/.test(normalized)) {
    return 'no';
  }
  return '';
}

function parseNumberSelection(text, max) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return [];
  }
  if (/^全部$/i.test(normalized)) {
    return Array.from({ length: max }, (_, index) => index + 1);
  }
  const matches = Array.from(normalized.matchAll(/(^|[^\p{L}\p{N}_])(\d+)(?=$|[^\p{L}\p{N}_])/gu));
  const numbers = Array.from(new Set(
    matches
      .map((item) => Number(item?.[2] ?? ''))
      .filter((item) => Number.isFinite(item) && item >= 1 && item <= max)
  ));
  return numbers;
}

function isCancelText(text) {
  return /^(取消|算了|不用了|结束|stop|cancel)$/i.test(normalizeText(text));
}

function isCancelSelectionText(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return false;
  }
  return isCancelText(normalized) || /^(0|0\.|0、)$/i.test(normalized);
}

function isTimeoutAbortError(error) {
  const message = String(error?.message ?? error ?? '').trim();
  if (!message) {
    return false;
  }
  return /The operation was aborted due to timeout/i.test(message)
    || /aborted due to timeout/i.test(message)
    || /TimeoutError/i.test(message);
}

function isSourceCodeAsset(asset) {
  const name = normalizeText(asset?.name).toLowerCase();
  if (!name) {
    return true;
  }
  return /^source code/i.test(name);
}

function getDownloadAssets(release) {
  return (Array.isArray(release?.assets) ? release.assets : [])
    .filter((asset) => asset && !isSourceCodeAsset(asset) && normalizeText(asset?.name));
}

function makeVersionPatterns(versionQuery) {
  const raw = normalizeText(versionQuery).toLowerCase();
  if (!raw || raw === 'latest') {
    return [];
  }
  const strippedV = raw.replace(/^v/i, '');
  return Array.from(new Set([
    raw,
    strippedV,
    `v${strippedV}`
  ])).filter(Boolean);
}

function scoreReleaseForVersion(release, versionQuery) {
  const patterns = makeVersionPatterns(versionQuery);
  if (patterns.length === 0) {
    return 0;
  }

  const tag = normalizeText(release?.tag_name).toLowerCase();
  const name = normalizeText(release?.name).toLowerCase();
  const body = normalizeText(release?.body).toLowerCase();
  const fields = [
    { text: tag, base: 260, exact: 520 },
    { text: name, base: 180, exact: 360 },
    { text: body, base: 120, exact: 240 }
  ];

  let score = 0;
  for (const pattern of patterns) {
    const exactRegex = new RegExp(`(^|[^a-z0-9])${escapeRegExp(pattern)}([^a-z0-9]|$)`, 'i');
    for (const field of fields) {
      if (!field.text) {
        continue;
      }
      if (field.text === pattern) {
        score = Math.max(score, field.exact);
        continue;
      }
      if (exactRegex.test(field.text)) {
        score = Math.max(score, field.base);
        continue;
      }
      if (field.text.includes(pattern)) {
        score = Math.max(score, Math.floor(field.base * 0.6));
      }
    }
  }

  if (release?.prerelease === true) {
    score += 12;
  }
  if (release?.draft === true) {
    score -= 1000;
  }
  return score;
}

async function fileExists(targetPath) {
  try {
    await fs.access(targetPath);
    return true;
  } catch {
    return false;
  }
}

function isSimpleUpstreamVersionQuery(versionQuery) {
  const normalized = normalizeText(versionQuery).toLowerCase().replace(/^v/, '');
  if (!normalized || normalized === 'latest') {
    return false;
  }
  return /^\d{2,3}(?:\.\d+){0,2}$/.test(normalized);
}

export class GroupFileDownloadManager {
  constructor(config, runtimeConfigStore, napcatClient, logger, options = {}) {
    this.githubConfig = config?.github ?? {};
    this.runtimeConfigStore = runtimeConfigStore;
    this.napcatClient = napcatClient;
    this.logger = logger;
    this.downloadRoot = path.resolve(options.downloadRoot ?? path.join(process.cwd(), 'data', 'release-downloads'));
    this.sessions = new Map();
    this.githubResolvedToken = null;
  }

  isGroupEnabled(groupId) {
    return this.runtimeConfigStore?.isQaGroupFileDownloadEnabled(groupId) === true;
  }

  async handleGroupMessage(context, event, text) {
    const groupId = normalizeText(context?.groupId);
    const userId = normalizeText(context?.userId);
    if (!groupId || !userId || !this.isGroupEnabled(groupId)) {
      return false;
    }

    this.#cleanupExpiredSessions();
    const sessionKey = `${groupId}:${userId}`;
    const existing = this.sessions.get(sessionKey);
    if (existing) {
      if (isCancelText(text)) {
        this.sessions.delete(sessionKey);
        await this.#reply(context, event, '已取消这次安装包查询。');
        return true;
      }
      try {
        return await this.#handleSessionReply(existing, context, event, text);
      } catch (error) {
        return await this.#handleFlowError(existing.key, context, event, error);
      }
    }

    const intent = parseInitialIntent(text);
    if (!intent.matched) {
      return false;
    }

    const session = {
      key: sessionKey,
      groupId,
      userId,
      startedAt: Date.now(),
      expiresAt: Date.now() + SESSION_TTL_MS,
      requestText: normalizeText(text),
      repoChoice: intent.repoChoice,
      versionQuery: intent.versionQuery,
      platformHint: intent.platformHint,
      folderName: this.runtimeConfigStore?.getQaGroupFileDownloadFolderName(groupId) || '',
      state: ''
    };

    this.logger.info(
      `群文件下载请求：group=${groupId} user=${userId} repoHint=${session.repoChoice || '(ask)'} version=${session.versionQuery || '(ask)'} platform=${session.platformHint || '-'}`
    );

    try {
      if (!session.repoChoice) {
        session.state = 'awaiting_repo_choice';
        this.sessions.set(sessionKey, session);
        await this.#reply(context, event, '你要的是 MindustryX 的 X端(TinyLake) 还是原版(Anuken)？直接回“X端”或“原版”；回 Cancel 退出。');
        return true;
      }

      return await this.#continueAfterRepoResolved(session, context, event);
    } catch (error) {
      return await this.#handleFlowError(session.key, context, event, error);
    }
  }

  async startGroupDownloadFlowFromTool(context, event, request = {}) {
    const groupId = normalizeText(context?.groupId);
    const userId = normalizeText(context?.userId);
    if (!groupId || !userId) {
      return {
        tool: 'start_group_file_download',
        started: false,
        reason: '仅群聊可用'
      };
    }
    if (!this.isGroupEnabled(groupId)) {
      return {
        tool: 'start_group_file_download',
        started: false,
        reason: '本群未启用文件下载'
      };
    }

    this.#cleanupExpiredSessions();
    const rawText = normalizeText(
      request?.request_text
      ?? request?.text
      ?? request?.query
      ?? request?.message
      ?? event?.raw_message
      ?? ''
    );
    const repoChoice = normalizeRepoChoiceInput(request?.repo_choice ?? request?.repo ?? '') || parseRepoChoice(rawText);
    const versionQuery = normalizeVersionQueryInput(request?.version_query ?? request?.version ?? '') || parseVersionQuery(rawText);
    const platformHint = normalizePlatformHintInput(request?.platform_hint ?? request?.platform ?? '') || parsePlatformHint(rawText);
    const sessionKey = `${groupId}:${userId}`;
    const session = {
      key: sessionKey,
      groupId,
      userId,
      startedAt: Date.now(),
      expiresAt: Date.now() + SESSION_TTL_MS,
      requestText: rawText || [repoChoice, versionQuery, platformHint].filter(Boolean).join(' '),
      repoChoice,
      versionQuery,
      platformHint,
      folderName: normalizeText(request?.folder_name ?? request?.folderName ?? '')
        || this.runtimeConfigStore?.getQaGroupFileDownloadFolderName(groupId)
        || '',
      state: ''
    };

    this.logger.info(
      `群文件下载工具触发：group=${groupId} user=${userId} repoHint=${session.repoChoice || '(ask)'} version=${session.versionQuery || '(ask)'} platform=${session.platformHint || '-'}`
    );

    try {
      if (!session.repoChoice) {
        session.state = 'awaiting_repo_choice';
        this.sessions.set(sessionKey, session);
        await this.#reply(context, event, '你要的是 MindustryX 的 X端(TinyLake) 还是原版(Anuken)？直接回“X端”或“原版”；回 Cancel 退出。');
        return {
          tool: 'start_group_file_download',
          started: true,
          state: session.state,
          repo_choice: null,
          version_query: session.versionQuery || null,
          platform_hint: session.platformHint || null,
          handled_directly: true
        };
      }

      await this.#continueAfterRepoResolved(session, context, event);
      return {
        tool: 'start_group_file_download',
        started: true,
        state: session.state || '',
        repo_choice: session.repoChoice || null,
        version_query: session.versionQuery || null,
        platform_hint: session.platformHint || null,
        release_tag: normalizeText(session.release?.tag_name) || null,
        handled_directly: true
      };
    } catch (error) {
      await this.#handleFlowError(session.key, context, event, error);
      return {
        tool: 'start_group_file_download',
        started: true,
        state: 'aborted',
        handled_directly: true,
        reason: isTimeoutAbortError(error) ? 'timeout' : 'error'
      };
    }
  }

  #cleanupExpiredSessions() {
    const now = Date.now();
    for (const [key, session] of this.sessions.entries()) {
      if (Number(session?.expiresAt ?? 0) <= now) {
        this.sessions.delete(key);
      }
    }
  }

  async #handleSessionReply(session, context, event, text) {
    session.expiresAt = Date.now() + SESSION_TTL_MS;
    switch (session.state) {
      case 'awaiting_repo_choice': {
        const repoChoice = parseRepoChoice(text);
        if (!repoChoice) {
          return true;
        }
        session.repoChoice = repoChoice;
        return await this.#continueAfterRepoResolved(session, context, event);
      }
      case 'awaiting_version_query': {
        const versionQuery = parseVersionQuery(text);
        if (!versionQuery) {
          return true;
        }
        session.versionQuery = versionQuery;
        return await this.#resolveReleaseAndContinue(session, context, event);
      }
      case 'awaiting_latest_confirm': {
        const confirm = parseLatestConfirmation(text);
        if (confirm === 'no') {
          this.sessions.delete(session.key);
          await this.#reply(context, event, '那这次先不查了。之后你直接发更具体的版本号给我。');
          return true;
        }
        if (confirm !== 'yes') {
          return true;
        }
        session.versionQuery = 'latest';
        return await this.#resolveReleaseAndContinue(session, context, event);
      }
      case 'awaiting_release_choice': {
        if (isCancelSelectionText(text)) {
          this.sessions.delete(session.key);
          await this.#reply(context, event, '已退出这次安装包查询。');
          return true;
        }
        const choices = parseNumberSelection(text, Array.isArray(session.releaseCandidates) ? session.releaseCandidates.length : 0);
        if (choices.length !== 1) {
          return true;
        }
        const release = session.releaseCandidates[choices[0] - 1];
        if (!release) {
          return true;
        }
        session.release = release;
        return await this.#presentAssets(session, context, event);
      }
      case 'awaiting_asset_choice': {
        if (isCancelSelectionText(text)) {
          this.sessions.delete(session.key);
          await this.#reply(context, event, '已退出这次安装包查询。');
          return true;
        }
        const assets = Array.isArray(session.assets) ? session.assets : [];
        const selections = parseNumberSelection(text, assets.length);
        if (selections.length === 0) {
          return true;
        }
        const selectedAssets = selections
          .map((index) => assets[index - 1])
          .filter(Boolean);
        if (selectedAssets.length === 0) {
          return true;
        }
        await this.#downloadAndSendAssets(session, context, event, selectedAssets);
        return true;
      }
      default:
        this.sessions.delete(session.key);
        return false;
    }
  }

  async #continueAfterRepoResolved(session, context, event) {
    if (!session.versionQuery) {
      session.state = 'awaiting_version_query';
      this.sessions.set(session.key, session);
      await this.#reply(context, event, `按 ${createRepoInfo(session.repoChoice).displayName} 来找。你要哪个版本？直接回版本号；要最新版回“最新版”；回 Cancel 退出。`);
      return true;
    }
    return await this.#resolveReleaseAndContinue(session, context, event);
  }

  async #resolveReleaseAndContinue(session, context, event) {
    const repo = createRepoInfo(session.repoChoice);
    session.repo = repo;
    const releases = await this.#listReleases(repo);
    if (releases.length === 0) {
      this.sessions.delete(session.key);
      await this.#reply(context, event, `${repo.displayName} 这边暂时没读到可用的 release。`);
      return true;
    }

    if (session.versionQuery === 'latest') {
      session.release = releases[0];
      this.logger.info(`群文件下载匹配到最新 release：repo=${repo.fullName} tag=${normalizeText(session.release?.tag_name) || '(none)'}`);
      return await this.#presentAssets(session, context, event);
    }

    const scored = releases
      .map((release) => ({ release, score: scoreReleaseForVersion(release, session.versionQuery) }))
      .filter((item) => item.score > 0)
      .sort((left, right) => right.score - left.score || String(right.release?.published_at ?? '').localeCompare(String(left.release?.published_at ?? '')));

    if (scored.length === 0) {
      session.state = 'awaiting_latest_confirm';
      this.sessions.set(session.key, session);
      await this.#reply(context, event, `我在 ${repo.displayName} 的 release 里没找到和 ${session.versionQuery} 明显对应的 tag 或内容。你是不是要最新版？回“Y”或“最新版”确认。`);
      return true;
    }

    if (scored.length > 1 && scored[0].score === scored[1].score) {
      if (repo.choice === 'x' && isSimpleUpstreamVersionQuery(session.versionQuery)) {
        session.release = scored[0].release;
        this.logger.info(`群文件下载按 X端上游版本自动取最新候选：repo=${repo.fullName} version=${session.versionQuery} tag=${normalizeText(session.release?.tag_name) || '(none)'}`);
        return await this.#presentAssets(session, context, event);
      }
      session.state = 'awaiting_release_choice';
      session.releaseCandidates = scored.slice(0, 6).map((item) => item.release);
      this.sessions.set(session.key, session);
      const lines = session.releaseCandidates.map((release, index) => {
        const name = normalizeText(release?.name) || '(无标题)';
        const tag = normalizeText(release?.tag_name) || '(无 tag)';
        return `${index + 1}. ${tag} | ${name}`;
      });
      await this.#reply(context, event, [
        `我找到多个可能对应 ${session.versionQuery} 的 release，你选一个 tag：`,
        ...lines,
        '0. Cancel',
        '回复一个序号即可。'
      ].join('\n'));
      return true;
    }

    session.release = scored[0].release;
    this.logger.info(`群文件下载匹配到 release：repo=${repo.fullName} version=${session.versionQuery} tag=${normalizeText(session.release?.tag_name) || '(none)'}`);
    return await this.#presentAssets(session, context, event);
  }

  async #presentAssets(session, context, event) {
    const release = session.release;
    const assets = getDownloadAssets(release);
    if (assets.length === 0) {
      this.sessions.delete(session.key);
      await this.#reply(context, event, `找到了 ${normalizeText(release?.tag_name) || '(无 tag)'}，但这个 release 没有可发的文件资产。`);
      return true;
    }

    session.assets = assets;
    session.state = 'awaiting_asset_choice';
    this.sessions.set(session.key, session);
    const lines = assets.map((asset, index) => `${index + 1}. ${asset.name}`);
    await this.#reply(context, event, [
      `按 ${session.repo.displayName} 的 ${normalizeText(release?.tag_name) || '(无 tag)'} 找到了这些文件：`,
      ...lines,
      '0. Cancel',
      '回复序号选择要发哪个；如果想一次发多个，也可以回“1 2”或“1,2”。'
    ].join('\n'));
    return true;
  }

  async #downloadAndSendAssets(session, context, event, assets) {
    const releaseTag = normalizeText(session?.release?.tag_name) || 'unknown-tag';
    const repo = session.repo;
    const total = assets.length;
    await this.#reply(context, event, total > 1 ? `开始下载并发送这 ${total} 个文件。` : '开始下载并发送文件。');

    for (const asset of assets) {
      this.logger.info(`群文件下载开始：group=${context.groupId} asset=${asset.name} tag=${releaseTag}`);
      const downloaded = await this.#downloadAsset(repo, releaseTag, asset);
      await this.napcatClient.sendLocalFileToGroup({
        groupId: context.groupId,
        filePath: downloaded.filePath,
        fileName: downloaded.fileName,
        folderName: normalizeText(session.folderName)
      });
      this.logger.info(`群文件下载完成：group=${context.groupId} file=${downloaded.fileName} tag=${releaseTag}`);
    }

    this.sessions.delete(session.key);
  }

  async #downloadAsset(repo, releaseTag, asset) {
    const fileName = withUppercaseApkName(asset?.name);
    const targetDir = path.join(
      this.downloadRoot,
      sanitizePathSegment(repo.fullName),
      sanitizePathSegment(releaseTag)
    );
    await ensureDir(targetDir);
    const filePath = path.join(targetDir, sanitizePathSegment(fileName));
    const expectedSize = Number(asset?.size ?? 0);

    if (await fileExists(filePath)) {
      const stat = await fs.stat(filePath).catch(() => null);
      if (stat?.isFile?.() && (!expectedSize || stat.size === expectedSize)) {
        this.logger.info(`群文件下载复用本地缓存：repo=${repo.fullName} tag=${releaseTag} file=${fileName}`);
        return { filePath, fileName };
      }
    }

    const sourceUrl = normalizeText(asset?.browser_download_url ?? asset?.url);
    if (!sourceUrl) {
      throw new Error(`资产 ${fileName} 缺少下载地址`);
    }
    const candidates = await this.#buildDownloadCandidates(sourceUrl);
    let lastError = null;
    let timeoutCount = 0;
    for (let startIndex = 0; startIndex < candidates.length; startIndex += MAX_CONCURRENT_DOWNLOADS) {
      const batch = candidates.slice(startIndex, startIndex + MAX_CONCURRENT_DOWNLOADS);
      if (batch.length === 0) {
        break;
      }
      this.logger.info(`开始并发下载批次：${batch.map((item) => item.label).join(', ')}`);
      const batchResult = await this.#downloadCandidateBatch(batch, filePath);
      if (batchResult.success) {
        return { filePath, fileName };
      }
      timeoutCount += batchResult.timeoutCount;
      if (timeoutCount >= DOWNLOAD_TIMEOUT_ABORT_LIMIT) {
        throw new Error(`The operation was aborted due to timeout（累计 ${timeoutCount} 次）`);
      }
      if (batchResult.lastError) {
        lastError = batchResult.lastError;
      }
    }
    throw lastError ?? new Error(`下载 ${fileName} 失败`);
  }

  async #listReleases(repo) {
    const releases = [];
    for (let page = 1; page <= MAX_RELEASE_PAGES; page += 1) {
      const payload = await this.#githubApiGet(`/repos/${repo.owner}/${repo.repo}/releases`, {
        per_page: RELEASES_PER_PAGE,
        page
      });
      const current = Array.isArray(payload) ? payload : [];
      if (current.length === 0) {
        break;
      }
      releases.push(...current.filter((item) => item?.draft !== true));
      if (current.length < RELEASES_PER_PAGE) {
        break;
      }
    }
    return releases;
  }

  async #githubApiGet(apiPath, searchParams = {}) {
    const baseUrl = normalizeText(this.githubConfig?.apiBaseUrl) || GITHUB_API_BASE_URL;
    const parsed = new URL(apiPath, `${baseUrl.replace(/\/+$/g, '')}/`);
    for (const [key, value] of Object.entries(searchParams)) {
      if (value == null || value === '') {
        continue;
      }
      parsed.searchParams.set(key, String(value));
    }

    const headers = {
      Accept: 'application/vnd.github+json',
      'User-Agent': 'NapCatCainBot/0.1'
    };
    const token = await this.#resolveGithubToken();
    if (token) {
      headers.Authorization = `Bearer ${token}`;
    }

    const response = await fetch(parsed, {
      method: 'GET',
      headers,
      signal: AbortSignal.timeout(clampInteger(this.githubConfig?.requestTimeoutMs, 15000, 3000, 120000))
    });
    if (!response.ok) {
      const body = normalizeText(await response.text());
      throw new Error(body ? `GitHub API ${response.status}: ${body}` : `GitHub API ${response.status}`);
    }
    return await response.json();
  }

  async #resolveGithubToken() {
    const configured = normalizeText(this.githubConfig?.token ?? process.env.GITHUB_TOKEN);
    if (configured) {
      return configured;
    }
    if (this.githubResolvedToken !== null) {
      return this.githubResolvedToken;
    }
    try {
      const result = await execFileAsync('gh', ['auth', 'token'], {
        windowsHide: true,
        timeout: 8000
      });
      this.githubResolvedToken = normalizeText(result?.stdout);
      return this.githubResolvedToken;
    } catch {
      this.githubResolvedToken = '';
      return '';
    }
  }

  async #reply(context, event, text) {
    await this.napcatClient.replyText(context, event?.message_id, text);
  }

  async #handleFlowError(sessionKey, context, event, error) {
    this.sessions.delete(sessionKey);
    this.logger.warn(`群文件下载流程失败：group=${context?.groupId || '-'} user=${context?.userId || '-'} error=${error?.message || error}`);
    if (isTimeoutAbortError(error)) {
      await this.#reply(context, event, '下载超时，已退出本次流程。');
      return true;
    }
    await this.#reply(context, event, `文件下载失败：${String(error?.message ?? error).slice(0, 120)}`);
    return true;
  }

  async #buildDownloadCandidates(sourceUrl) {
    const rankedMirrors = await this.#probeMirrorLatencies(sourceUrl);
    this.logger.info(`GitHub 镜像前五：${rankedMirrors.slice(0, MAX_CONCURRENT_DOWNLOADS).map((item) => `${item.base}(${item.latencyMs}ms)`).join(' | ') || '(none)'}`);
    return [
      ...rankedMirrors.map((item) => ({
        label: `mirror:${item.base}`,
        url: `${item.base.replace(/\/+$/g, '')}/${sourceUrl}`,
        useAuth: false
      })),
      {
        label: 'source:github',
        url: sourceUrl,
        useAuth: true
      }
    ];
  }

  async #probeMirrorLatencies(sourceUrl) {
    const results = await Promise.all(
      GITHUB_DOWNLOAD_MIRRORS.map(async (base) => ({
        base,
        ...(await this.#probeSingleMirror(base, sourceUrl))
      }))
    );
    const successful = results
      .filter((item) => item.ok)
      .sort((left, right) => left.latencyMs - right.latencyMs);
    const summary = results.map((item) => `${item.base}=${item.ok ? `${item.latencyMs}ms` : `error:${item.error}`}`).join(' | ');
    this.logger.info(`GitHub 镜像测速：${summary}`);
    return successful;
  }

  async #probeSingleMirror(base, sourceUrl) {
    const startedAt = Date.now();
    const url = `${base.replace(/\/+$/g, '')}/${sourceUrl}`;
    try {
      const response = await fetch(url, {
        method: 'GET',
        headers: {
          Range: 'bytes=0-0',
          'User-Agent': 'NapCatCainBot/0.1'
        },
        redirect: 'follow',
        signal: AbortSignal.timeout(8000)
      });
      if (response.body && typeof response.body.cancel === 'function') {
        response.body.cancel().catch(() => {});
      }
      if (!response.ok && response.status !== 206) {
        return { ok: false, error: `HTTP ${response.status}` };
      }
      return {
        ok: true,
        latencyMs: Date.now() - startedAt
      };
    } catch (error) {
      return {
        ok: false,
        error: error.message
      };
    }
  }

  async #downloadCandidateBatch(candidates, filePath) {
    const controllers = candidates.map(() => new AbortController());
    const tasks = candidates.map((candidate, index) => this.#downloadUrlToTempFile(
      candidate,
      `${filePath}.part-${index}`,
      index,
      controllers[index].signal
    ));

    try {
      const winner = await Promise.any(tasks);
      controllers.forEach((controller, index) => {
        if (index !== winner.index) {
          controller.abort();
        }
      });
      await Promise.allSettled(tasks);
      await fs.rm(filePath, { force: true }).catch(() => {});
      await fs.rename(winner.tempPath, filePath);
      await Promise.allSettled(candidates.map(async (_candidate, index) => {
        if (index === winner.index) {
          return;
        }
        await fs.rm(`${filePath}.part-${index}`, { force: true }).catch(() => {});
      }));
      return {
        success: true,
        timeoutCount: 0,
        lastError: null
      };
    } catch (aggregateError) {
      const settled = await Promise.allSettled(tasks);
      let timeoutCount = 0;
      let lastError = null;
      for (let index = 0; index < settled.length; index += 1) {
        await fs.rm(`${filePath}.part-${index}`, { force: true }).catch(() => {});
        const result = settled[index];
        if (result.status !== 'rejected') {
          continue;
        }
        const error = result.reason;
        if (isTimeoutAbortError(error)) {
          timeoutCount += 1;
          this.logger.warn(`下载候选超时：label=${candidates[index]?.label} url=${candidates[index]?.url} timeoutCount=${timeoutCount}/${DOWNLOAD_TIMEOUT_ABORT_LIMIT}`);
          continue;
        }
        if (String(error?.name ?? '') === 'AbortError') {
          continue;
        }
        lastError = error;
        this.logger.warn(`下载候选失败：label=${candidates[index]?.label} url=${candidates[index]?.url} error=${error.message}`);
      }
      return {
        success: false,
        timeoutCount,
        lastError: lastError ?? aggregateError
      };
    }
  }

  async #downloadUrlToTempFile(candidate, tempPath, index, externalAbortSignal) {
    const timeoutSignal = AbortSignal.timeout(clampInteger(this.githubConfig?.requestTimeoutMs, 60000, 5000, 300000));
    const signal = AbortSignal.any([externalAbortSignal, timeoutSignal]);
    const headers = {
      Accept: 'application/octet-stream',
      'User-Agent': 'NapCatCainBot/0.1'
    };
    if (candidate.useAuth) {
      const token = await this.#resolveGithubToken();
      if (token) {
        headers.Authorization = `Bearer ${token}`;
      }
    }

    this.logger.info(`开始下载候选：${candidate.label}`);
    try {
      const response = await fetch(candidate.url, {
        method: 'GET',
        headers,
        redirect: 'follow',
        signal
      });
      if (!response.ok || !response.body) {
        throw new Error(`HTTP ${response.status}`);
      }

      await pipeline(Readable.fromWeb(response.body), (await import('node:fs')).createWriteStream(tempPath));
      return {
        index,
        tempPath
      };
    } catch (error) {
      await fs.rm(tempPath, { force: true }).catch(() => {});
      throw error;
    }
  }
}
