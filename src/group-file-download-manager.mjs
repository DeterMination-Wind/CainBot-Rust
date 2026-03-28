import fs from 'node:fs/promises';
import path from 'node:path';
import { execFile, spawn } from 'node:child_process';
import readline from 'node:readline';
import { promisify } from 'node:util';
import { Readable } from 'node:stream';
import { pipeline } from 'node:stream/promises';

import { ensureDir } from './utils.mjs';

const execFileAsync = promisify(execFile);

const SESSION_TTL_MS = 5 * 60 * 1000;
const MAX_RELEASE_PAGES = 4;
const RELEASES_PER_PAGE = 100;
const COMMITS_PER_PAGE = 100;
const GITHUB_API_BASE_URL = 'https://api.github.com';
const MAX_CONCURRENT_DOWNLOADS = 5;
const PREFERRED_MIRROR_TTL_MS = 8 * 60 * 60 * 1000;
const BUILD_TIMEOUT_MS = 60 * 60 * 1000;
const PLATFORM_CLASSIFY_MODEL = null;
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
const LOCAL_RELEASE_SPECS = Object.freeze([
  {
    choice: 'local:neon',
    displayName: 'Neon',
    folderName: 'Neon',
    matchers: [/neon/i, /氖/],
    filePattern: /^Neon[-_].+\.(zip|jar)$/i,
    preferredExts: ['.zip', '.jar']
  },
  {
    choice: 'local:determination',
    displayName: 'DeterMination 服务器插件',
    folderName: 'DeterMination',
    matchers: [/determination/i, /决心/],
    filePattern: /^DeterMination-modules(?:-\d{8}(?:-\d{4,6})?)?\.zip$/i,
    preferredExts: ['.zip']
  }
]);
const LOCAL_RELEASE_SPEC_BY_CHOICE = new Map(
  LOCAL_RELEASE_SPECS.map((item) => [item.choice, item])
);

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

function uniqueStrings(values) {
  return Array.from(new Set((Array.isArray(values) ? values : []).map((item) => normalizeText(item)).filter(Boolean)));
}

function parseGithubRepoSpecifier(input) {
  const source = normalizeText(input);
  if (!source) {
    throw new Error('repo 不能为空');
  }

  const normalized = source
    .replace(/^git\+/i, '')
    .replace(/\.git$/i, '')
    .trim();

  let owner = '';
  let repo = '';
  const directMatch = normalized.match(/^([A-Za-z0-9_.-]+)\/([A-Za-z0-9_.-]+)$/);
  if (directMatch) {
    owner = directMatch[1];
    repo = directMatch[2];
  } else {
    try {
      const parsed = new URL(/^https?:\/\//i.test(normalized) ? normalized : `https://${normalized}`);
      const parts = parsed.pathname.split('/').map((item) => item.trim()).filter(Boolean);
      if ((parsed.hostname === 'github.com' || parsed.hostname === 'www.github.com') && parts.length >= 2) {
        [owner, repo] = parts;
      }
    } catch {
    }
  }

  owner = normalizeText(owner);
  repo = normalizeText(repo).replace(/\.git$/i, '');
  if (!owner || !repo) {
    throw new Error(`无法解析 GitHub 仓库：${source}`);
  }

  return {
    owner,
    repo,
    fullName: `${owner}/${repo}`,
    htmlUrl: `https://github.com/${owner}/${repo}`
  };
}

function tryParseGithubRepoSpecifier(input) {
  try {
    return parseGithubRepoSpecifier(input);
  } catch {
    return null;
  }
}

function extractGithubRepoSpecifier(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return null;
  }

  const urlMatch = normalized.match(/https?:\/\/github\.com\/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+(?:\/|(?:\.git)?)?/i);
  if (urlMatch?.[0]) {
    const parsedFromUrl = tryParseGithubRepoSpecifier(urlMatch[0]);
    if (parsedFromUrl) {
      return parsedFromUrl;
    }
  }

  const directMatches = normalized.match(/[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+/g) ?? [];
  for (const candidate of directMatches) {
    const parsed = tryParseGithubRepoSpecifier(candidate);
    if (parsed) {
      return parsed;
    }
  }

  return null;
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

function getLocalReleaseSpec(choice) {
  return LOCAL_RELEASE_SPEC_BY_CHOICE.get(normalizeText(choice).toLowerCase()) ?? null;
}

function detectLocalReleaseChoices(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return [];
  }
  return uniqueStrings(
    LOCAL_RELEASE_SPECS
      .filter((spec) => spec.matchers.some((matcher) => matcher.test(normalized)))
      .map((spec) => spec.choice)
  );
}

function normalizeRepoChoiceInput(value) {
  const normalized = normalizeText(value).toLowerCase();
  if (!normalized) {
    return '';
  }
  const localChoices = detectLocalReleaseChoices(value);
  if (localChoices.length === 1) {
    return localChoices[0];
  }
  if (normalized === 'x' || /(tinylake\/mindustryx|mindustryx|x端)/i.test(normalized)) {
    return 'x';
  }
  if (normalized === 'vanilla' || /(anuken\/mindustry|原版|官版|官方原版|原版mdt|原版mindustry)/i.test(normalized)) {
    return 'vanilla';
  }
  if (/(scriptagent4mindustryext(?:-8\.0)?|scriptagent\s*4\s*mindustry\s*ext|script\s*agent\s*4\s*mindustry\s*ext|sa插件|sa plugin)/i.test(normalized)) {
    return 'way-zer/ScriptAgent4MindustryExt';
  }
  return tryParseGithubRepoSpecifier(value)?.fullName ?? '';
}

function createRepoInfo(choice) {
  if (choice === 'x') {
    return {
      kind: 'builtin',
      choice,
      owner: 'TinyLake',
      repo: 'MindustryX',
      fullName: 'TinyLake/MindustryX',
      displayName: 'MindustryX X端'
    };
  }
  if (choice === 'vanilla') {
    return {
      kind: 'builtin',
      choice: 'vanilla',
      owner: 'Anuken',
      repo: 'Mindustry',
      fullName: 'Anuken/Mindustry',
      displayName: 'Mindustry 原版'
    };
  }
  if (/^way-zer\/scriptagent4mindustryext$/i.test(normalizeText(choice))) {
    return {
      kind: 'github',
      choice: 'way-zer/ScriptAgent4MindustryExt',
      owner: 'way-zer',
      repo: 'ScriptAgent4MindustryExt',
      fullName: 'way-zer/ScriptAgent4MindustryExt',
      displayName: 'ScriptAgent4MindustryExt 8.0',
      htmlUrl: 'https://github.com/way-zer/ScriptAgent4MindustryExt'
    };
  }
  const parsed = parseGithubRepoSpecifier(choice);
  return {
    kind: 'github',
    choice: parsed.fullName,
    owner: parsed.owner,
    repo: parsed.repo,
    fullName: parsed.fullName,
    displayName: parsed.fullName,
    htmlUrl: parsed.htmlUrl
  };
}

function parseRepoChoice(text) {
  const normalized = normalizeText(text).toLowerCase();
  if (!normalized) {
    return '';
  }
  const localChoices = detectLocalReleaseChoices(text);
  if (localChoices.length === 1) {
    return localChoices[0];
  }
  if (/(mindustryx|tinylake|x端)/i.test(normalized)) {
    return 'x';
  }
  if (/(anuken|原版|官版|官方原版|原版mdt|原版mindustry)/i.test(normalized)) {
    return 'vanilla';
  }
  if (/(scriptagent4mindustryext(?:-8\.0)?|scriptagent\s*4\s*mindustry\s*ext|script\s*agent\s*4\s*mindustry\s*ext|sa插件|sa plugin)/i.test(normalized)) {
    return 'way-zer/ScriptAgent4MindustryExt';
  }
  return extractGithubRepoSpecifier(text)?.fullName ?? '';
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

function parseCommitHash(text) {
  const normalized = normalizeText(text).toLowerCase();
  if (!normalized) {
    return '';
  }
  const match = normalized.match(/\b([0-9a-f]{7,40})\b/);
  const value = String(match?.[1] ?? '').trim();
  if (!value) {
    return '';
  }
  if (/^\d+$/.test(value) && !/(commit|hash|sha|提交|编译|构建|build)/i.test(normalized)) {
    return '';
  }
  return value;
}

function looksLikeCommitBuildRequest(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return false;
  }
  const hasCommitHash = Boolean(parseCommitHash(normalized));
  const buildLike = /(commit|hash|提交|编译|构建|build|源码)/i.test(normalized);
  const artifactLike = /(jar|服务端|服务器|server|desktop|电脑版|pc|安卓|android|apk)/i.test(normalized);
  if (hasCommitHash && (artifactLike || buildLike || looksLikeGameMention(normalized))) {
    return true;
  }
  return looksLikeGameMention(normalized) && buildLike && artifactLike;
}

function wantsExactCommitBuild(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return false;
  }
  return /(一定要|就要|精确编译|本地编译|原样编译|该commit|这个commit|exact)/i.test(normalized);
}

function parsePlatformHint(text) {
  const normalized = normalizeText(text).toLowerCase();
  if (!normalized) {
    return '';
  }
  if (/(sa插件|sa plugin|scriptagent4mindustryext|scriptagent|script agent)/i.test(normalized)) {
    return 'server';
  }
  if (/(服务端|服务器|server)/i.test(normalized)) {
    return 'server';
  }
  if (/(电脑版|电脑端|电脑|pc|desktop|windows|window|win|exe|jar|zip)/i.test(normalized)) {
    return 'pc';
  }
  if (/(安卓|android|apk|手机|移动端|手机版)/i.test(normalized)) {
    return 'android';
  }
  return '';
}

function normalizePlatformHintInput(value) {
  const normalized = normalizeText(value).toLowerCase();
  if (!normalized) {
    return '';
  }
  if (normalized === 'server' || /(服务端|服务器|server)/i.test(normalized)) {
    return 'server';
  }
  if (normalized === 'pc' || /(电脑版|电脑端|电脑|pc|desktop|windows|window|win|exe|jar|zip)/i.test(normalized)) {
    return 'pc';
  }
  if (normalized === 'android' || /(安卓|android|apk|手机|移动端|手机版)/i.test(normalized)) {
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
  const localReleaseChoices = detectLocalReleaseChoices(normalized);
  const githubRepo = extractGithubRepoSpecifier(normalized);
  if (githubRepo && /(release|releases|最新版|最新版本|latest|安装包|客户端下载|下载|下载包|文件|资产|asset|apk|exe|zip|jar)/i.test(normalized)) {
    return true;
  }
  const installLike = /(安装包|安装文件|客户端|下载包|pc安装包|pc包|apk|APK|exe|jar|zip|桌面版|电脑版|电脑版本|pc版本|桌面端|版本包|游戏包|文件包|插件|服务器插件|服务端插件|脚本包)/i.test(normalized);
  const pluginLike = /(sa插件|sa plugin|scriptagent|script agent|scriptagent4mindustryext|neon|determination|服务器插件|服务端插件)/i.test(normalized) || localReleaseChoices.length > 0;
  const requestLike = /(有没有|有吗|求|求发|发一下|发个|给我|来个|想要|下载|整一个|能发|有无|有没有人发|谁有|发我|来一份|是哪个|哪个包|哪一个)/i.test(normalized);
  const versionLike = Boolean(parseVersionQuery(normalized));
  const gameLike = looksLikeGameMention(normalized);
  if (installLike && (requestLike || versionLike)) {
    return true;
  }
  if (pluginLike && (requestLike || versionLike)) {
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

function parseFolderNameHint(text) {
  const normalized = normalizeText(text);
  if (!normalized) {
    return '';
  }
  const explicitMatch = normalized.match(/(?:群文件夹|文件夹|目录|路径)\s*[：: ]\s*[“"'`]?([^"'`”’\n，。,；;]{1,80})/i);
  if (explicitMatch?.[1]) {
    return normalizeText(explicitMatch[1]);
  }
  const targetMatch = normalized.match(/(?:发|传|上传|保存|放|丢|下载)(?:到)?\s*[“"'`]?([^"'`”’\n，。,；;]{1,40})[”"'`]?/i);
  if (!targetMatch?.[1]) {
    return '';
  }
  const candidate = normalizeText(targetMatch[1]);
  if (!candidate || /^(我|你|这里|那边|本地|群里|群文件|最新|最新版)$/i.test(candidate)) {
    return '';
  }
  return candidate;
}

function parseInitialIntent(text) {
  const commitMode = looksLikeCommitBuildRequest(text);
  return {
    matched: looksLikeDownloadRequest(text) || commitMode,
    mode: commitMode ? 'commit-build' : 'release',
    repoChoice: parseRepoChoice(text),
    versionQuery: parseVersionQuery(text),
    platformHint: parsePlatformHint(text),
    commitHash: parseCommitHash(text),
    exactCommitBuild: wantsExactCommitBuild(text),
    folderName: parseFolderNameHint(text)
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

async function resolvePreferredBashPath() {
  const candidates = [
    'C:\\Program Files\\Git\\bin\\bash.exe',
    'C:\\Program Files\\Git\\usr\\bin\\bash.exe',
    'C:\\Program Files (x86)\\Git\\bin\\bash.exe',
    'C:\\Program Files (x86)\\Git\\usr\\bin\\bash.exe'
  ];
  for (const candidate of candidates) {
    if (await fileExists(candidate)) {
      return candidate;
    }
  }
  return 'bash';
}

function isSimpleUpstreamVersionQuery(versionQuery) {
  const normalized = normalizeText(versionQuery).toLowerCase().replace(/^v/, '');
  if (!normalized || normalized === 'latest') {
    return false;
  }
  return /^\d{2,3}(?:\.\d+){0,2}$/.test(normalized);
}

function releaseSortValue(release) {
  return String(release?.published_at ?? release?.created_at ?? '');
}

function pickLatestRelease(releases) {
  const normalized = [...(Array.isArray(releases) ? releases : [])]
    .filter((item) => item?.draft !== true)
    .sort((left, right) => releaseSortValue(right).localeCompare(releaseSortValue(left)));
  const latestPreRelease = normalized.find((item) => item?.prerelease === true);
  return latestPreRelease ?? normalized[0] ?? null;
}

function scoreAssetForPlatform(assetName, platformHint) {
  const normalized = normalizeText(assetName).toLowerCase();
  if (!normalized || !platformHint) {
    return 0;
  }

  let score = 0;
  switch (platformHint) {
    case 'pc':
      if (/desktop/i.test(normalized)) {
        score += 300;
      }
      if (/\.jar$/i.test(normalized)) {
        score += 90;
      }
      if (/\.(exe|zip)$/i.test(normalized)) {
        score += 70;
      }
      if (/loader/i.test(normalized)) {
        score -= 220;
      }
      if (/server/i.test(normalized)) {
        score -= 260;
      }
      if (/\.apk$/i.test(normalized) || /android/i.test(normalized)) {
        score -= 320;
      }
      break;
    case 'android':
      if (/\.apk$/i.test(normalized)) {
        score += 320;
      }
      if (/android/i.test(normalized)) {
        score += 120;
      }
      if (/\.jar$/i.test(normalized)) {
        score -= 220;
      }
      if (/server/i.test(normalized) || /loader/i.test(normalized)) {
        score -= 260;
      }
      break;
    case 'server':
      if (/(^|[-_.])server/i.test(normalized)) {
        score += 320;
      }
      if (/\.jar$/i.test(normalized)) {
        score += 100;
      }
      if (/desktop/i.test(normalized) || /loader/i.test(normalized)) {
        score -= 260;
      }
      if (/\.apk$/i.test(normalized) || /android/i.test(normalized)) {
        score -= 320;
      }
      break;
    default:
      break;
  }

  return score;
}

function isGithubHttpsUrl(url) {
  return /^https?:\/\/github\.com\//i.test(normalizeText(url));
}

function buildGitMirrorRewritePrefix(base) {
  const normalizedBase = normalizeText(base).replace(/\/+$/g, '');
  if (!normalizedBase) {
    return '';
  }
  return `${normalizedBase}/https://github.com/`;
}

function buildGitMirrorConfigArgs(base) {
  const rewritePrefix = buildGitMirrorRewritePrefix(base);
  if (!rewritePrefix) {
    return [];
  }
  return ['-c', `url.${rewritePrefix}.insteadOf=https://github.com/`];
}

function buildGitMirrorProbeUrl(base, sourceUrl) {
  const normalizedBase = normalizeText(base).replace(/\/+$/g, '');
  const normalizedSourceUrl = normalizeText(sourceUrl).replace(/\/+$/g, '');
  if (!normalizedBase || !normalizedSourceUrl) {
    return '';
  }
  return `${normalizedBase}/${normalizedSourceUrl}/info/refs?service=git-upload-pack`;
}

export class GroupFileDownloadManager {
  constructor(config, runtimeConfigStore, napcatClient, logger, options = {}) {
    this.githubConfig = config?.github ?? {};
    this.platformClassifyModel = String(options.platformClassifyModel ?? config?.platformClassifyModel ?? '').trim() || null;
    this.runtimeConfigStore = runtimeConfigStore;
    this.napcatClient = napcatClient;
    this.logger = logger;
    this.chatClient = options.chatClient ?? null;
    this.codexRoot = path.resolve(options.codexRoot ?? config?.codexRoot ?? path.join(process.cwd(), '..', 'codex'));
    this.localBuildRoot = path.resolve(options.localBuildRoot ?? config?.localBuildRoot ?? path.join(this.codexRoot, 'builds'));
    this.downloadRoot = path.resolve(options.downloadRoot ?? path.join(process.cwd(), 'data', 'release-downloads'));
    this.vanillaRepoRoot = path.resolve(options.vanillaRepoRoot ?? config?.vanillaRepoRoot ?? path.join(this.codexRoot, 'Mindustry-master'));
    this.xRepoRoot = path.resolve(options.xRepoRoot ?? config?.xRepoRoot ?? path.join(this.codexRoot, 'MindustryX-main'));
    this.sessions = new Map();
    this.githubResolvedToken = null;
    this.mirrorCache = null;
    this.mirrorCacheFile = path.join(this.downloadRoot, '_meta', 'preferred-mirror.json');
    this.groupFolderCache = null;
    this.groupFolderCacheFile = path.join(this.downloadRoot, '_meta', 'group-folder-cache.json');
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
      mode: intent.mode,
      repoChoice: intent.repoChoice,
      versionQuery: intent.versionQuery,
      platformHint: await this.#resolvePlatformHint(text),
      commitHash: intent.commitHash,
      exactCommitBuild: intent.exactCommitBuild === true,
      folderName: intent.folderName || this.runtimeConfigStore?.getQaGroupFileDownloadFolderName(groupId) || '',
      localReleaseChoices: detectLocalReleaseChoices(text),
      state: ''
    };

    this.logger.info(
      `群文件下载请求：group=${groupId} user=${userId} mode=${session.mode} repoHint=${session.repoChoice || '(ask)'} version=${session.versionQuery || '(ask)'} commit=${session.commitHash || '(ask)'} platform=${session.platformHint || '-'}`
    );

    try {
      if (session.mode !== 'commit-build' && session.localReleaseChoices.length > 0) {
        return await this.#resolveLocalReleasesAndSend(session, context, event);
      }
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
    const requestedMode = normalizeText(request?.mode ?? request?.download_mode ?? '').toLowerCase();
    const repoChoice = normalizeRepoChoiceInput(request?.repo_choice ?? request?.repo ?? '') || parseRepoChoice(rawText);
    const versionQuery = normalizeVersionQueryInput(request?.version_query ?? request?.version ?? '') || parseVersionQuery(rawText);
    const explicitPlatformHint = normalizePlatformHintInput(request?.platform_hint ?? request?.platform ?? '');
    const platformHint = explicitPlatformHint || await this.#resolvePlatformHint(rawText);
    const commitHash = normalizeText(
      request?.commit_hash
      ?? request?.commit
      ?? request?.sha
      ?? request?.hash
      ?? ''
    ).toLowerCase() || parseCommitHash(rawText);
    const inferredIntent = parseInitialIntent(rawText);
    const mode = requestedMode === 'commit-build' || requestedMode === 'commit'
      ? 'commit-build'
      : commitHash
        ? 'commit-build'
        : inferredIntent.mode;
    const exactCommitBuild = request?.exact_commit_build === true
      || request?.exactCommitBuild === true
      || wantsExactCommitBuild(rawText);
    const sessionKey = `${groupId}:${userId}`;
    const session = {
      key: sessionKey,
      groupId,
      userId,
      startedAt: Date.now(),
      expiresAt: Date.now() + SESSION_TTL_MS,
      requestText: rawText || [repoChoice, versionQuery, platformHint].filter(Boolean).join(' '),
      mode,
      repoChoice,
      versionQuery,
      platformHint,
      commitHash,
      exactCommitBuild,
      folderName: normalizeText(request?.folder_name ?? request?.folderName ?? '')
        || parseFolderNameHint(rawText)
        || this.runtimeConfigStore?.getQaGroupFileDownloadFolderName(groupId)
        || '',
      localReleaseChoices: uniqueStrings([
        ...detectLocalReleaseChoices(request?.repo_choice ?? request?.repo ?? ''),
        ...detectLocalReleaseChoices(rawText)
      ]),
      state: ''
    };

    this.logger.info(
      `群文件下载工具触发：group=${groupId} user=${userId} mode=${session.mode} repoHint=${session.repoChoice || '(ask)'} version=${session.versionQuery || '(ask)'} commit=${session.commitHash || '(ask)'} platform=${session.platformHint || '-'}`
    );

    try {
      if (session.mode !== 'commit-build' && session.localReleaseChoices.length > 0) {
        await this.#resolveLocalReleasesAndSend(session, context, event);
        return {
          tool: 'start_group_file_download',
          started: true,
          state: 'sent-local-release',
          repo_choice: session.repoChoice || null,
          version_query: session.versionQuery || null,
          platform_hint: session.platformHint || null,
          handled_directly: true
        };
      }
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
    await this.#mergeSessionHintsFromText(session, text);
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
      case 'awaiting_commit_choice': {
        if (isCancelSelectionText(text)) {
          this.sessions.delete(session.key);
          await this.#reply(context, event, '已退出这次 commit 构建查询。');
          return true;
        }
        const directHash = parseCommitHash(text);
        if (directHash) {
          session.commitHash = directHash;
          return await this.#resolveCommitBuildAndSend(session, context, event);
        }
        const choices = parseNumberSelection(text, Array.isArray(session.commitCandidates) ? session.commitCandidates.length : 0);
        if (choices.length !== 1) {
          return true;
        }
        const commit = session.commitCandidates[choices[0] - 1];
        if (!commit?.sha) {
          return true;
        }
        session.commitHash = commit.sha;
        return await this.#resolveCommitBuildAndSend(session, context, event);
      }
      case 'awaiting_commit_release_choice': {
        if (isCancelSelectionText(text)) {
          this.sessions.delete(session.key);
          await this.#reply(context, event, '已退出这次 commit 构建查询。');
          return true;
        }
        if (wantsExactCommitBuild(text)) {
          session.exactCommitBuild = true;
          return await this.#resolveCommitBuildAndSend(session, context, event);
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
    if (session.mode === 'commit-build') {
      if (!session.commitHash) {
        return await this.#presentCommitChoices(session, context, event);
      }
      return await this.#resolveCommitBuildAndSend(session, context, event);
    }

    if (!session.versionQuery) {
      session.state = 'awaiting_version_query';
      this.sessions.set(session.key, session);
      await this.#reply(context, event, `按 ${createRepoInfo(session.repoChoice).displayName} 来找。你要哪个版本？直接回版本号；要最新版回“最新版”；回 Cancel 退出。`);
      return true;
    }
    return await this.#resolveReleaseAndContinue(session, context, event);
  }

  async #mergeSessionHintsFromText(session, text) {
    if (!session || !text) {
      return;
    }
    session.localReleaseChoices = uniqueStrings([
      ...(Array.isArray(session.localReleaseChoices) ? session.localReleaseChoices : []),
      ...detectLocalReleaseChoices(text)
    ]);
    if (!session.repoChoice) {
      session.repoChoice = parseRepoChoice(text) || session.repoChoice;
    }
    if (!session.platformHint) {
      session.platformHint = await this.#resolvePlatformHint(text) || session.platformHint;
    }
    if (session.mode === 'commit-build' && !session.commitHash) {
      session.commitHash = parseCommitHash(text) || session.commitHash;
    }
    if (session.mode === 'commit-build' && wantsExactCommitBuild(text)) {
      session.exactCommitBuild = true;
    }
    if (session.mode !== 'commit-build' && !session.versionQuery) {
      session.versionQuery = parseVersionQuery(text) || session.versionQuery;
    }
  }

  async #resolveLocalReleasesAndSend(session, context, event) {
    const choices = uniqueStrings(session?.localReleaseChoices);
    const artifacts = [];
    const missing = [];

    for (const choice of choices) {
      const artifact = await this.#findLatestLocalReleaseArtifact(choice, session);
      if (artifact) {
        artifacts.push(artifact);
      } else {
        missing.push(getLocalReleaseSpec(choice)?.displayName ?? choice);
      }
    }

    if (artifacts.length === 0) {
      this.sessions.delete(session.key);
      await this.#reply(context, event, '本地构建目录里还没找到对应的可发发布文件。');
      return true;
    }

    await this.#reply(context, event, artifacts.length > 1 ? '开始发送本地最新发布。' : '开始发送本地最新发布文件。');
    const targetFolderName = await this.#getSessionTargetFolderName(session);
    for (const artifact of artifacts) {
      await this.napcatClient.sendLocalFileToGroup({
        groupId: context.groupId,
        filePath: artifact.filePath,
        fileName: artifact.fileName,
        folderName: targetFolderName || artifact.folderName || ''
      });
    }
    this.sessions.delete(session.key);

    if (missing.length > 0) {
      await this.#reply(context, event, `这些资源本地没找到可发文件：${missing.join('、')}`);
    }
    return true;
  }

  async #findLatestLocalReleaseArtifact(choice, session) {
    const spec = getLocalReleaseSpec(choice);
    if (!spec) {
      return null;
    }
    const versionQuery = normalizeText(session?.versionQuery).toLowerCase();
    let candidates = await this.#listLocalReleaseCandidates(spec);
    if (candidates.length === 0) {
      return null;
    }

    if (versionQuery && versionQuery !== 'latest') {
      candidates = candidates.filter((item) => normalizeText(item.fileName).toLowerCase().includes(versionQuery));
      if (candidates.length === 0) {
        return null;
      }
    }

    const platformHint = normalizeText(session?.platformHint).toLowerCase();
    const scored = candidates
      .map((item) => ({
        ...item,
        score: this.#scoreLocalReleaseCandidate(item, spec, versionQuery, platformHint)
      }))
      .sort((left, right) => right.score - left.score || right.mtimeMs - left.mtimeMs || left.fileName.localeCompare(right.fileName, 'zh-CN'));

    const best = scored[0];
    if (!best || best.score <= 0) {
      return null;
    }
    return {
      filePath: best.filePath,
      fileName: best.fileName,
      folderName: spec.folderName
    };
  }

  async #listLocalReleaseCandidates(spec) {
    const roots = this.#getLocalReleaseSearchRoots(spec?.choice);
    const results = [];
    const seen = new Set();

    for (const root of roots) {
      const files = await this.#collectFilesRecursively(root, 4);
      for (const filePath of files) {
        const fileName = path.basename(filePath);
        if (!spec.filePattern.test(fileName)) {
          continue;
        }
        const normalizedPath = path.resolve(filePath).toLowerCase();
        if (seen.has(normalizedPath)) {
          continue;
        }
        seen.add(normalizedPath);
        const stat = await fs.stat(filePath).catch(() => null);
        if (!stat?.isFile?.()) {
          continue;
        }
        results.push({
          filePath,
          fileName,
          ext: path.extname(fileName).toLowerCase(),
          mtimeMs: Number(stat.mtimeMs ?? 0)
        });
      }
    }

    return results;
  }

  #getLocalReleaseSearchRoots(choice) {
    switch (normalizeText(choice).toLowerCase()) {
      case 'local:neon':
        return [
          path.join(this.localBuildRoot, 'Neon'),
          path.join(this.localBuildRoot, 'release-assets', 'Neon')
        ];
      case 'local:determination':
        return [
          path.join(this.codexRoot, 'anonymous'),
          path.join(this.localBuildRoot, 'DeterMination'),
          path.join(this.localBuildRoot, 'release-assets', 'DeterMination')
        ];
      default:
        return [];
    }
  }

  async #collectFilesRecursively(rootPath, maxDepth = 4) {
    const normalizedRoot = normalizeText(rootPath);
    if (!normalizedRoot || !(await fileExists(normalizedRoot))) {
      return [];
    }

    const queue = [{ currentPath: normalizedRoot, depth: 0 }];
    const files = [];
    while (queue.length > 0) {
      const current = queue.shift();
      const currentPath = normalizeText(current?.currentPath);
      const depth = Number(current?.depth ?? 0);
      if (!currentPath) {
        continue;
      }
      const entries = await fs.readdir(currentPath, { withFileTypes: true }).catch(() => []);
      for (const entry of entries) {
        const entryPath = path.join(currentPath, entry.name);
        if (entry.isDirectory()) {
          if (depth < maxDepth) {
            queue.push({ currentPath: entryPath, depth: depth + 1 });
          }
          continue;
        }
        if (entry.isFile()) {
          files.push(entryPath);
        }
      }
    }
    return files;
  }

  #scoreLocalReleaseCandidate(candidate, spec, versionQuery, platformHint) {
    if (!candidate || !spec) {
      return 0;
    }
    const fileName = normalizeText(candidate.fileName);
    const lowerName = fileName.toLowerCase();
    let score = 100;

    if (versionQuery && versionQuery !== 'latest') {
      if (lowerName.includes(versionQuery)) {
        score += 200;
      } else {
        score -= 120;
      }
    }

    if (platformHint === 'android') {
      score += candidate.ext === '.jar' ? 20 : -10;
    } else if (platformHint === 'pc') {
      score += candidate.ext === '.zip' ? 20 : 10;
    } else if (platformHint === 'server') {
      score += candidate.ext === '.zip' || candidate.ext === '.jar' ? 20 : 0;
    }

    const preferredIndex = spec.preferredExts.indexOf(candidate.ext);
    if (preferredIndex >= 0) {
      score += (spec.preferredExts.length - preferredIndex) * 10;
    }

    if (/^DeterMination-modules\.zip$/i.test(fileName)) {
      score += 25;
    }

    score += Math.min(30, Math.floor(candidate.mtimeMs / (60 * 1000)));
    return score;
  }

  async #resolvePlatformHint(text) {
    const regexHint = parsePlatformHint(text);
    const normalizedText = normalizeText(text);
    if (!normalizedText) {
      return regexHint;
    }
    if (!this.chatClient?.complete) {
      return regexHint;
    }
    try {
      const raw = await this.chatClient.complete([
        {
          role: 'system',
          content: [
            '你负责判断一段中文资源请求更偏向哪个平台。',
            '只输出 JSON：{"platform":"pc|android|server|unknown","reason":"简短原因"}。',
            '规则：',
            '1. 电脑版、电脑、PC、桌面版、desktop、Windows、jar、exe、zip -> pc',
            '2. 安卓、Android、手机、APK、移动端 -> android',
            '3. 服务端、服务器、server -> server',
            '4. 如果文本没有明确平台，就输出 unknown。'
          ].join('\n')
        },
        {
          role: 'user',
          content: normalizedText
        }
      ], {
        model: this.platformClassifyModel || PLATFORM_CLASSIFY_MODEL || 'gpt-5.4-mini',
        temperature: 0.1
      });
      const match = String(raw ?? '').match(/\{[\s\S]*\}/);
      const parsed = JSON.parse(match?.[0] ?? '{}');
      const platform = normalizePlatformHintInput(parsed?.platform ?? '');
      if (platform) {
        return platform;
      }
    } catch (error) {
      this.logger.warn(`平台识别模型判定失败，回退正则：${error.message}`);
    }
    return regexHint;
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
      session.release = pickLatestRelease(releases);
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

  async #presentCommitChoices(session, context, event) {
    const repo = createRepoInfo(session.repoChoice);
    session.repo = repo;
    const commits = await this.#listCommits(repo, 100);
    if (commits.length === 0) {
      this.sessions.delete(session.key);
      await this.#reply(context, event, `${repo.displayName} 这边暂时没读到最近 commit。`);
      return true;
    }

    session.commitCandidates = commits;
    session.state = 'awaiting_commit_choice';
    this.sessions.set(session.key, session);
    const lines = commits.map((commit, index) => {
      const shaShort = String(commit.sha ?? '').slice(0, 7);
      const title = normalizeText(commit.title) || '(无标题)';
      return `${index + 1}. ${shaShort} ${title}`;
    });
    await this.#reply(context, event, [
      `这是 ${repo.displayName} 最近 100 个 commit：`,
      ...lines,
      '0. Cancel',
      '回序号，或直接回 commit hash。'
    ].join('\n'));
    return true;
  }

  async #resolveCommitBuildAndSend(session, context, event) {
    const repo = createRepoInfo(session.repoChoice);
    session.repo = repo;
    const commitHash = normalizeText(session.commitHash).toLowerCase();
    if (!commitHash) {
      return await this.#presentCommitChoices(session, context, event);
    }

    const platformHint = session.platformHint || 'pc';
    const artifactLabel = platformHint === 'server' ? 'server jar' : platformHint === 'android' ? 'apk' : 'desktop jar';

    let artifact = null;
    try {
      if (repo.choice === 'x') {
        const exactArtifact = await this.#resolveExactXCommitArtifact(session, platformHint);
        if (!exactArtifact) {
          if (session.exactCommitBuild === true) {
            await this.#reply(context, event, `开始本地精确编译 ${commitHash.slice(0, 7)} 的 ${artifactLabel}。`);
            artifact = await this.#buildExactXCommitArtifact(session, platformHint);
          } else {
            return await this.#presentXCommitReleaseChoices(session, context, event);
          }
        } else {
          artifact = exactArtifact;
        }
      } else {
        artifact = await this.#buildVanillaCommitArtifact(session, platformHint);
      }
      const targetFolderName = await this.#getSessionTargetFolderName(session);
      await this.napcatClient.sendLocalFileToGroup({
        groupId: context.groupId,
        filePath: artifact.filePath,
        fileName: artifact.fileName,
        folderName: targetFolderName
      });
      this.sessions.delete(session.key);
      return true;
    } finally {
      if (artifact?.cleanup) {
        await artifact.cleanup().catch((error) => {
          this.logger.warn(`清理 commit 构建临时文件失败：${error.message}`);
        });
      }
    }
  }

  async #presentAssets(session, context, event) {
    const release = session.release;
    const assets = getDownloadAssets(release);
    if (assets.length === 0) {
      this.sessions.delete(session.key);
      await this.#reply(context, event, `找到了 ${normalizeText(release?.tag_name) || '(无 tag)'}，但这个 release 没有可发的文件资产。`);
      return true;
    }

    const matchedAssets = this.#matchAssetsByPlatform(assets, session.platformHint);
    if (matchedAssets.length > 0) {
      await this.#downloadAndSendAssets(session, context, event, matchedAssets);
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
    const targetFolderName = await this.#getSessionTargetFolderName(session);

    for (const asset of assets) {
      this.logger.info(`群文件下载开始：group=${context.groupId} asset=${asset.name} tag=${releaseTag}`);
      const downloaded = await this.#downloadAsset(repo, releaseTag, asset);
      await this.napcatClient.sendLocalFileToGroup({
        groupId: context.groupId,
        filePath: downloaded.filePath,
        fileName: downloaded.fileName,
        folderName: targetFolderName
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
    let lastError = null;
    let timeoutCount = 0;

    const preferredMirror = await this.#getPreferredMirrorBase();
    if (preferredMirror) {
      this.logger.info(`优先尝试最近成功镜像：${preferredMirror}`);
      const preferredBatch = [{
        label: `preferred:${preferredMirror}`,
        url: `${preferredMirror.replace(/\/+$/g, '')}/${sourceUrl}`,
        useAuth: false
      }];
      const preferredResult = await this.#downloadCandidateBatch(preferredBatch, filePath);
      if (preferredResult.success) {
        await this.#rememberPreferredMirrorBase(preferredMirror);
        return { filePath, fileName };
      }
      if (preferredResult.lastError) {
        lastError = preferredResult.lastError;
      }
      timeoutCount += preferredResult.timeoutCount;
      if (timeoutCount >= DOWNLOAD_TIMEOUT_ABORT_LIMIT) {
        throw new Error(`The operation was aborted due to timeout（累计 ${timeoutCount} 次）`);
      }
    }

    const candidates = await this.#buildDownloadCandidates(sourceUrl, preferredMirror);
    for (let startIndex = 0; startIndex < candidates.length; startIndex += MAX_CONCURRENT_DOWNLOADS) {
      const batch = candidates.slice(startIndex, startIndex + MAX_CONCURRENT_DOWNLOADS);
      if (batch.length === 0) {
        break;
      }
      this.logger.info(`开始并发下载批次：${batch.map((item) => item.label).join(', ')}`);
      const batchResult = await this.#downloadCandidateBatch(batch, filePath);
      if (batchResult.success) {
        if (batchResult.winnerLabel?.startsWith('mirror:')) {
          const mirrorBase = batchResult.winnerLabel.slice('mirror:'.length);
          await this.#rememberPreferredMirrorBase(mirrorBase);
        }
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

  async #listCommits(repo, maxCommits = 100) {
    const commits = [];
    for (let page = 1; commits.length < maxCommits; page += 1) {
      const remaining = maxCommits - commits.length;
      const perPage = Math.min(COMMITS_PER_PAGE, remaining);
      const payload = await this.#githubApiGet(`/repos/${repo.owner}/${repo.repo}/commits`, {
        per_page: perPage,
        page
      });
      const current = Array.isArray(payload) ? payload : [];
      if (current.length === 0) {
        break;
      }
      commits.push(...current.map((commit) => {
        const message = String(commit?.commit?.message ?? '').replace(/\r\n/g, '\n').trim();
        const [title, ...rest] = message.split('\n');
        return {
          sha: normalizeText(commit?.sha),
          title: normalizeText(title),
          body: normalizeText(rest.join('\n')),
          htmlUrl: normalizeText(commit?.html_url),
          date: normalizeText(commit?.commit?.author?.date)
        };
      }));
      if (current.length < perPage) {
        break;
      }
    }
    return commits.slice(0, maxCommits);
  }

  #matchAssetsByPlatform(assets, platformHint) {
    const normalizedPlatform = normalizeText(platformHint);
    if (!normalizedPlatform) {
      return [];
    }
    const scored = (Array.isArray(assets) ? assets : [])
      .map((asset) => ({ asset, score: scoreAssetForPlatform(asset?.name, normalizedPlatform) }))
      .filter((item) => item.score > 0)
      .sort((left, right) => right.score - left.score || normalizeText(left.asset?.name).localeCompare(normalizeText(right.asset?.name), 'zh-CN'));
    if (scored.length === 0) {
      return [];
    }
    return [scored[0].asset];
  }

  async #resolveExactXCommitArtifact(session, platformHint) {
    const repo = session.repo;
    const commitHash = normalizeText(session.commitHash).toLowerCase();
    const releases = await this.#listReleases(repo);
    const matchedRelease = releases
      .filter((release) => {
        const target = normalizeText(release?.target_commitish).toLowerCase();
        return target && (target.startsWith(commitHash) || commitHash.startsWith(target));
      })
      .sort((left, right) => releaseSortValue(right).localeCompare(releaseSortValue(left)))[0] ?? null;

    if (!matchedRelease) {
      return null;
    }

    const assets = getDownloadAssets(matchedRelease);
    const selected = this.#matchAssetsByPlatform(assets, platformHint || 'pc');
    if (selected.length === 0) {
      throw new Error(`commit ${commitHash.slice(0, 7)} 的 pre-release 没有匹配 ${platformHint || 'pc'} 的文件`);
    }

    const artifact = await this.#downloadAsset(repo, normalizeText(matchedRelease.tag_name) || commitHash.slice(0, 7), selected[0]);
    return {
      ...artifact,
      cleanup: null
    };
  }

  async #presentXCommitReleaseChoices(session, context, event) {
    const candidates = await this.#findXCommitReleaseCandidates(session.commitHash, 6);
    if (candidates.length === 0) {
      throw new Error(`X端这边没找到 commit ${normalizeText(session.commitHash).slice(0, 7)} 对应或后续包含它的 Actions pre-release`);
    }

    session.releaseCandidates = candidates.map((item) => item.release);
    session.state = 'awaiting_commit_release_choice';
    this.sessions.set(session.key, session);
    const lines = candidates.map((item, index) => {
      const tag = normalizeText(item.release?.tag_name) || '(无 tag)';
      const distance = Number(item.aheadBy ?? 0);
      return `${index + 1}. ${tag} | 后续 ${distance} 提交`;
    });
    await this.#reply(context, event, [
      `这个 commit 没有精确预编译，我找到后续最近的几个 X端预编译：`,
      ...lines,
      '0. Cancel',
      '回序号选择；回“精确编译”则本地编译该 commit。'
    ].join('\n'));
    return true;
  }

  async #findXCommitReleaseCandidates(commitHash, maxCandidates = 6) {
    const repo = createRepoInfo('x');
    const releases = (await this.#listReleases(repo))
      .filter((release) => release?.prerelease === true || release?.draft !== true)
      .slice(0, 16);
    const normalizedCommit = normalizeText(commitHash).toLowerCase();
    const repoRoot = this.xRepoRoot;
    if (!(await fileExists(path.join(repoRoot, '.git')))) {
      throw new Error(`X端源码仓库不存在：${repoRoot}`);
    }

    const refMap = await this.#resolveGitRefsWithSingleFetch(repoRoot, [
      normalizedCommit,
      ...releases.map((release) => normalizeText(release?.target_commitish)).filter(Boolean)
    ]);
    const resolvedCommit = refMap.get(normalizedCommit);
    if (!resolvedCommit) {
      throw new Error(`本地 X端仓库里找不到 commit ${normalizedCommit.slice(0, 7)}`);
    }

    const candidates = [];

    for (const release of releases) {
      const target = normalizeText(release?.target_commitish).toLowerCase();
      const resolvedTarget = refMap.get(target);
      if (!resolvedTarget) {
        continue;
      }
      try {
        const isAncestor = await this.#isGitAncestor(repoRoot, resolvedCommit, resolvedTarget);
        if (!isAncestor) {
          continue;
        }
        const aheadBy = await this.#countGitRevisionDistance(repoRoot, resolvedCommit, resolvedTarget);
        candidates.push({
          release,
          aheadBy
        });
      } catch (error) {
        this.logger.warn(`比较 X端 commit 与 release 失败：commit=${normalizedCommit.slice(0, 7)} target=${resolvedTarget.slice(0, 7)} error=${error.message}`);
      }
    }

    return candidates
      .sort((left, right) => left.aheadBy - right.aheadBy || releaseSortValue(right.release).localeCompare(releaseSortValue(left.release)))
      .slice(0, maxCandidates);
  }

  async #buildVanillaCommitArtifact(session, platformHint) {
    const normalizedPlatform = platformHint || 'pc';
    if (normalizedPlatform === 'android') {
      throw new Error('原版 commit 构建目前只支持 pc 或 server');
    }

    const repoRoot = this.vanillaRepoRoot;
    if (!(await fileExists(path.join(repoRoot, 'gradlew.bat')))) {
      throw new Error(`原版源码仓库不存在：${repoRoot}`);
    }

    const fullCommit = await this.#resolveGitCommit(repoRoot, session.commitHash);
    const tempParent = path.join(this.downloadRoot, '_tmp-builds');
    const worktreeDir = path.join(tempParent, `mindustry-${fullCommit.slice(0, 12)}-${Date.now()}`);
    await ensureDir(tempParent);
    this.logger.info(`原版 commit 编译准备 worktree：commit=${fullCommit.slice(0, 7)} path=${worktreeDir}`);

    await execFileAsync('git', ['-C', repoRoot, 'worktree', 'add', '--detach', worktreeDir, fullCommit], {
      windowsHide: true,
      timeout: 120000,
      maxBuffer: 8 * 1024 * 1024
    });

    let artifactPath = '';
    let artifactName = '';
    const cleanup = async () => {
      await execFileAsync('git', ['-C', repoRoot, 'worktree', 'remove', worktreeDir, '--force'], {
        windowsHide: true,
        timeout: 120000,
        maxBuffer: 8 * 1024 * 1024
      }).catch(() => {});
      await fs.rm(worktreeDir, { recursive: true, force: true }).catch(() => {});
    };

    try {
      const taskArgs = normalizedPlatform === 'server'
        ? [':server:dist']
        : [':desktop:dist', '-x', 'test'];
      this.logger.info(`原版 commit 开始构建：commit=${fullCommit.slice(0, 7)} platform=${normalizedPlatform} task=${taskArgs.join(' ')}`);
      await this.#runLoggedCommand(
        path.join(worktreeDir, 'gradlew.bat'),
        taskArgs,
        {
          cwd: worktreeDir,
          timeout: BUILD_TIMEOUT_MS,
          label: `vanilla-build:${fullCommit.slice(0, 7)}`
        }
      );

      artifactPath = normalizedPlatform === 'server'
        ? path.join(worktreeDir, 'server', 'build', 'libs', 'server-release.jar')
        : path.join(worktreeDir, 'desktop', 'build', 'libs', 'Mindustry.jar');
      const exists = await fileExists(artifactPath);
      if (!exists) {
        throw new Error(`构建成功但没找到产物：${artifactPath}`);
      }
      artifactName = normalizedPlatform === 'server'
        ? `Mindustry-server-${fullCommit.slice(0, 7)}.jar`
        : `Mindustry-desktop-${fullCommit.slice(0, 7)}.jar`;
      this.logger.info(`原版 commit 构建完成：commit=${fullCommit.slice(0, 7)} artifact=${artifactPath}`);
      return {
        filePath: artifactPath,
        fileName: artifactName,
        cleanup
      };
    } catch (error) {
      await cleanup();
      throw error;
    }
  }

  async #buildExactXCommitArtifact(session, platformHint) {
    const normalizedPlatform = platformHint || 'pc';
    if (normalizedPlatform === 'android') {
      throw new Error('X端本地精确编译暂不支持安卓包');
    }

    const repoRoot = this.xRepoRoot;
    if (!(await fileExists(path.join(repoRoot, 'scripts', 'applyPatches.sh')))) {
      throw new Error(`X端源码仓库不存在：${repoRoot}`);
    }

    const fullCommit = await this.#resolveGitCommit(repoRoot, session.commitHash);
    const tempParent = path.join(this.downloadRoot, '_tmp-builds');
    const worktreeDir = path.join(tempParent, `mindustryx-${fullCommit.slice(0, 12)}-${Date.now()}`);
    await ensureDir(tempParent);
    this.logger.info(`X端精确编译准备 worktree：commit=${fullCommit.slice(0, 7)} path=${worktreeDir}`);

    await execFileAsync('git', ['-C', repoRoot, 'worktree', 'add', '--detach', worktreeDir, fullCommit], {
      windowsHide: true,
      timeout: 120000,
      maxBuffer: 8 * 1024 * 1024
    });

    const cleanup = async () => {
      await execFileAsync('git', ['-C', repoRoot, 'worktree', 'remove', worktreeDir, '--force'], {
        windowsHide: true,
        timeout: 120000,
        maxBuffer: 8 * 1024 * 1024
      }).catch(() => {});
      await fs.rm(worktreeDir, { recursive: true, force: true }).catch(() => {});
    };

    try {
      let usedLocalSubmodules = false;
      try {
        const localSubmoduleResult = await this.#prepareExactXLocalSubmodules(worktreeDir);
        if (localSubmoduleResult.prepared === true) {
          usedLocalSubmodules = true;
          this.logger.info(`X端精确编译已复用本地子模块：commit=${fullCommit.slice(0, 7)} ${localSubmoduleResult.items.map((item) => `${item.path}@${item.commit.slice(0, 7)}`).join(' ')}`);
        } else {
          this.logger.info(`X端精确编译无法复用本地子模块，回退网络初始化：commit=${fullCommit.slice(0, 7)} reason=${localSubmoduleResult.reason || 'unknown'}`);
        }
      } catch (error) {
        this.logger.warn(`X端精确编译复用本地子模块失败，回退网络初始化：commit=${fullCommit.slice(0, 7)} error=${error.message}`);
      }

      if (!usedLocalSubmodules) {
        this.logger.info(`X端精确编译开始同步子模块配置：commit=${fullCommit.slice(0, 7)}`);
        await this.#runLoggedCommand('git', ['-C', worktreeDir, 'submodule', 'sync', '--recursive'], {
          timeout: 10 * 60 * 1000,
          label: `mindustryx-submodule-sync:${fullCommit.slice(0, 7)}`
        });
        this.logger.info(`X端精确编译开始初始化子模块：commit=${fullCommit.slice(0, 7)}`);
        await this.#runGitCommandWithMirrorFallback(
          ['-C', worktreeDir, 'submodule', 'update', '--init', '--recursive', '--jobs', '4'],
          {
            sourceUrlHint: 'https://github.com/Anuken/Arc',
            timeout: 30 * 60 * 1000,
            maxBuffer: 8 * 1024 * 1024,
            label: `mindustryx-submodule-update:${fullCommit.slice(0, 7)}`,
            mirrorLimit: MAX_CONCURRENT_DOWNLOADS
          }
        );
      }
      this.logger.info(`X端精确编译开始应用补丁：commit=${fullCommit.slice(0, 7)}`);
      const bashPath = await resolvePreferredBashPath();
      this.logger.info(`X端精确编译补丁脚本 shell：${bashPath}`);
      await this.#runLoggedCommand(bashPath, ['./scripts/applyPatches.sh'], {
        cwd: worktreeDir,
        timeout: 20 * 60 * 1000,
        label: `mindustryx-patch:${fullCommit.slice(0, 7)}`
      });

      const taskArgs = normalizedPlatform === 'server'
        ? [':server:dist']
        : [':desktop:dist', '-x', 'test'];
      this.logger.info(`X端精确编译开始构建：commit=${fullCommit.slice(0, 7)} platform=${normalizedPlatform} task=${taskArgs.join(' ')}`);
      await this.#runLoggedCommand(path.join(worktreeDir, 'work', 'gradlew.bat'), taskArgs, {
        cwd: path.join(worktreeDir, 'work'),
        timeout: BUILD_TIMEOUT_MS,
        label: `mindustryx-build:${fullCommit.slice(0, 7)}`
      });

      const artifactPath = normalizedPlatform === 'server'
        ? path.join(worktreeDir, 'work', 'server', 'build', 'libs', 'server-release.jar')
        : path.join(worktreeDir, 'work', 'desktop', 'build', 'libs', 'Mindustry.jar');
      if (!(await fileExists(artifactPath))) {
        throw new Error(`X端精确编译成功但没找到产物：${artifactPath}`);
      }

      const artifactName = normalizedPlatform === 'server'
        ? `MindustryX-server-${fullCommit.slice(0, 7)}.jar`
        : `MindustryX-desktop-${fullCommit.slice(0, 7)}.jar`;
      this.logger.info(`X端精确编译完成：commit=${fullCommit.slice(0, 7)} artifact=${artifactPath}`);
      return {
        filePath: artifactPath,
        fileName: artifactName,
        cleanup
      };
    } catch (error) {
      await cleanup();
      throw error;
    }
  }

  async #prepareExactXLocalSubmodules(worktreeDir) {
    const submodules = [
      { path: 'Arc', sourcePath: path.join(this.xRepoRoot, 'Arc') },
      { path: 'work', sourcePath: path.join(this.xRepoRoot, 'work') }
    ];
    const preparedItems = [];

    for (const submodule of submodules) {
      const commit = await this.#readGitTreeEntryCommit(worktreeDir, 'HEAD', submodule.path);
      if (!commit) {
        return {
          prepared: false,
          reason: `未找到子模块 gitlink：${submodule.path}`
        };
      }
      if (!(await fileExists(path.join(submodule.sourcePath, '.git')))) {
        return {
          prepared: false,
          reason: `本地子模块不存在：${submodule.path}`
        };
      }
      if (!(await this.#tryResolveGitCommit(submodule.sourcePath, commit))) {
        return {
          prepared: false,
          reason: `本地子模块缺少目标 commit：${submodule.path}@${commit.slice(0, 7)}`
        };
      }

      const targetPath = path.join(worktreeDir, submodule.path);
      await fs.rm(targetPath, { recursive: true, force: true }).catch(() => {});
      await this.#runLoggedCommand('git', ['clone', '--no-checkout', submodule.sourcePath, targetPath], {
        timeout: 10 * 60 * 1000,
        label: `mindustryx-local-submodule-clone:${submodule.path}:${commit.slice(0, 7)}`
      });
      await this.#runLoggedCommand('git', ['-C', targetPath, 'checkout', '--force', commit], {
        timeout: 10 * 60 * 1000,
        label: `mindustryx-local-submodule-checkout:${submodule.path}:${commit.slice(0, 7)}`
      });
      preparedItems.push({
        path: submodule.path,
        commit
      });
    }

    return {
      prepared: true,
      items: preparedItems
    };
  }

  async #readGitTreeEntryCommit(repoRoot, ref, entryPath) {
    try {
      const result = await execFileAsync('git', ['-C', repoRoot, 'ls-tree', ref, entryPath], {
        windowsHide: true,
        timeout: 20000,
        maxBuffer: 1024 * 1024
      });
      const line = String(result?.stdout ?? '')
        .split(/\r?\n/g)
        .map((item) => normalizeText(item))
        .find(Boolean);
      const match = line?.match(/^160000 commit ([0-9a-f]{40})\t/);
      return String(match?.[1] ?? '').toLowerCase();
    } catch {
      return '';
    }
  }

  async #resolveGitCommit(repoRoot, commitHash) {
    const normalized = normalizeText(commitHash).toLowerCase();
    if (!normalized) {
      throw new Error('commit hash 不能为空');
    }

    const tryResolve = async () => this.#tryResolveGitCommit(repoRoot, normalized);

    try {
      const resolved = await tryResolve();
      if (resolved) {
        return resolved;
      }
      throw new Error('unresolved');
    } catch {
      await this.#fetchGitRepo(repoRoot);
      const resolved = await tryResolve();
      if (resolved) {
        return resolved;
      }
      throw new Error(`找不到 commit ${normalized.slice(0, 7)}`);
    }
  }

  async #resolveGitRefsWithSingleFetch(repoRoot, refs) {
    const uniqueRefs = Array.from(new Set(
      (Array.isArray(refs) ? refs : [])
        .map((item) => normalizeText(item).toLowerCase())
        .filter(Boolean)
    ));
    const resolved = new Map();
    const unresolved = [];

    for (const ref of uniqueRefs) {
      const fullHash = await this.#tryResolveGitCommit(repoRoot, ref);
      if (fullHash) {
        resolved.set(ref, fullHash);
      } else {
        unresolved.push(ref);
      }
    }

    if (unresolved.length === 0) {
      return resolved;
    }

    await this.#fetchGitRepo(repoRoot);

    for (const ref of unresolved) {
      const fullHash = await this.#tryResolveGitCommit(repoRoot, ref);
      if (fullHash) {
        resolved.set(ref, fullHash);
      }
    }

    return resolved;
  }

  async #tryResolveGitCommit(repoRoot, commitish) {
    const normalized = normalizeText(commitish).toLowerCase();
    if (!normalized) {
      return '';
    }
    try {
      const result = await execFileAsync('git', ['-C', repoRoot, 'rev-parse', '--verify', `${normalized}^{commit}`], {
        windowsHide: true,
        timeout: 20000,
        maxBuffer: 4 * 1024 * 1024
      });
      return normalizeText(result?.stdout);
    } catch {
      return '';
    }
  }

  async #runLoggedCommand(command, args = [], options = {}) {
    const label = normalizeText(options?.label) || path.basename(String(command ?? 'command'));
    const cwd = normalizeText(options?.cwd);
    const timeout = clampInteger(options?.timeout, 120000, 1000, BUILD_TIMEOUT_MS);
    const maxLineLength = clampInteger(options?.maxLineLength, 500, 80, 4000);
    this.logger.info(`[${label}] start: ${command} ${(Array.isArray(args) ? args : []).join(' ')}`.trim());

    return await new Promise((resolve, reject) => {
      const child = spawn(command, Array.isArray(args) ? args : [], {
        cwd: cwd || undefined,
        windowsHide: true,
        stdio: ['ignore', 'pipe', 'pipe']
      });

      let finished = false;
      const finish = (handler, value) => {
        if (finished) {
          return;
        }
        finished = true;
        clearTimeout(timer);
        handler(value);
      };

      const bindStream = (stream, level) => {
        if (!stream) {
          return;
        }
        const lineReader = readline.createInterface({ input: stream });
        lineReader.on('line', (line) => {
          const text = normalizeText(line);
          if (!text) {
            return;
          }
          this.logger[level](`[${label}] ${text.slice(0, maxLineLength)}`);
        });
      };

      bindStream(child.stdout, 'info');
      bindStream(child.stderr, 'warn');

      child.on('error', (error) => {
        finish(reject, error);
      });

      child.on('close', (code, signal) => {
        if (code === 0) {
          this.logger.info(`[${label}] done`);
          finish(resolve, { code, signal });
          return;
        }
        finish(reject, new Error(`[${label}] 退出失败：code=${code ?? 'null'} signal=${signal ?? 'null'}`));
      });

      const timer = setTimeout(() => {
        child.kill('SIGTERM');
        finish(reject, new Error(`[${label}] 超时：${timeout}ms`));
      }, timeout);
    });
  }

  async #fetchGitRepo(repoRoot) {
    const orderedRemotes = await this.#getOrderedGitRemotes(repoRoot);
    if (orderedRemotes.length === 0) {
      await execFileAsync('git', ['-C', repoRoot, 'fetch', '--all', '--tags', '--prune'], {
        windowsHide: true,
        timeout: 120000,
        maxBuffer: 8 * 1024 * 1024
      });
      return;
    }

    let lastError = null;
    for (const remote of orderedRemotes) {
      try {
        await this.#runGitCommandWithMirrorFallback(
          ['-C', repoRoot, 'fetch', remote, '--tags', '--prune'],
          {
            sourceUrlHint: await this.#getGitRemoteUrl(repoRoot, remote),
            timeout: 120000,
            maxBuffer: 8 * 1024 * 1024,
            label: `git-fetch:${path.basename(repoRoot)}:${remote}`
          }
        );
        return;
      } catch (error) {
        lastError = error;
        this.logger.warn(`git fetch 远端失败：repo=${repoRoot} remote=${remote} error=${error.message}`);
      }
    }

    if (lastError) {
      throw lastError;
    }
  }

  async #getOrderedGitRemotes(repoRoot) {
    const result = await execFileAsync('git', ['-C', repoRoot, 'remote'], {
      windowsHide: true,
      timeout: 10000,
      maxBuffer: 1024 * 1024
    }).catch(() => null);
    const remotes = String(result?.stdout ?? '')
      .split(/\r?\n/g)
      .map((item) => normalizeText(item))
      .filter(Boolean);
    if (remotes.length === 0) {
      return [];
    }

    const preferred = this.#getPreferredGitFetchRemotes(repoRoot);
    const ordered = [
      ...preferred.filter((item) => remotes.includes(item)),
      ...remotes.filter((item) => !preferred.includes(item))
    ];
    return Array.from(new Set(ordered));
  }

  async #getGitRemoteUrl(repoRoot, remote) {
    const normalizedRemote = normalizeText(remote);
    if (!normalizedRemote) {
      return '';
    }
    try {
      const result = await execFileAsync('git', ['-C', repoRoot, 'remote', 'get-url', normalizedRemote], {
        windowsHide: true,
        timeout: 10000,
        maxBuffer: 1024 * 1024
      });
      return normalizeText(result?.stdout);
    } catch {
      return '';
    }
  }

  #getPreferredGitFetchRemotes(repoRoot) {
    const normalizedRepoRoot = path.resolve(repoRoot);
    if (normalizedRepoRoot === this.xRepoRoot) {
      return ['upstream', 'origin'];
    }
    if (normalizedRepoRoot === this.vanillaRepoRoot) {
      return ['origin', 'upstream', 'mindustry-upstream'];
    }
    return ['origin', 'upstream'];
  }

  async #isGitAncestor(repoRoot, ancestorRef, descendantRef) {
    try {
      await execFileAsync('git', ['-C', repoRoot, 'merge-base', '--is-ancestor', ancestorRef, descendantRef], {
        windowsHide: true,
        timeout: 20000,
        maxBuffer: 4 * 1024 * 1024
      });
      return true;
    } catch (error) {
      if (Number(error?.code) === 1) {
        return false;
      }
      throw error;
    }
  }

  async #countGitRevisionDistance(repoRoot, baseRef, targetRef) {
    const result = await execFileAsync('git', ['-C', repoRoot, 'rev-list', '--count', `${baseRef}..${targetRef}`], {
      windowsHide: true,
      timeout: 20000,
      maxBuffer: 4 * 1024 * 1024
    });
    return Math.max(0, Number.parseInt(normalizeText(result?.stdout), 10) || 0);
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

    try {
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
    } catch (error) {
      this.logger.warn(`GitHub fetch 失败，改用 gh api 重试：${error.message}`);
      return await this.#githubApiGetViaGh(parsed);
    }
  }

  async #githubApiGetViaGh(url) {
    const apiBase = normalizeText(this.githubConfig?.apiBaseUrl) || GITHUB_API_BASE_URL;
    const parsed = new URL(String(url), `${apiBase.replace(/\/+$/g, '')}/`);
    const pathWithQuery = `${parsed.pathname}${parsed.search}`;
    const result = await execFileAsync('gh', ['api', pathWithQuery], {
      windowsHide: true,
      timeout: clampInteger(this.githubConfig?.requestTimeoutMs, 15000, 3000, 120000),
      maxBuffer: 8 * 1024 * 1024
    });
    return JSON.parse(String(result?.stdout ?? '').trim() || 'null');
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

  async #loadMirrorCache() {
    if (this.mirrorCache) {
      return this.mirrorCache;
    }
    try {
      const raw = JSON.parse(await fs.readFile(this.mirrorCacheFile, 'utf8'));
      this.mirrorCache = raw && typeof raw === 'object' ? raw : {};
    } catch {
      this.mirrorCache = {};
    }
    return this.mirrorCache;
  }

  async #loadGroupFolderCache() {
    if (this.groupFolderCache) {
      return this.groupFolderCache;
    }
    try {
      const raw = JSON.parse(await fs.readFile(this.groupFolderCacheFile, 'utf8'));
      this.groupFolderCache = raw && typeof raw === 'object' ? raw : {};
    } catch {
      this.groupFolderCache = {};
    }
    return this.groupFolderCache;
  }

  #buildGroupFolderCacheKey(groupId, repoChoice) {
    return `${normalizeText(groupId)}:${normalizeText(repoChoice) || 'unknown'}`;
  }

  async #getCachedGroupFolderName(groupId, repoChoice) {
    const cache = await this.#loadGroupFolderCache();
    return normalizeText(cache?.[this.#buildGroupFolderCacheKey(groupId, repoChoice)]?.folderName);
  }

  async #rememberGroupFolderName(groupId, repoChoice, folderName) {
    const normalizedFolderName = normalizeText(folderName);
    if (!normalizedFolderName) {
      return;
    }
    const cache = await this.#loadGroupFolderCache();
    cache[this.#buildGroupFolderCacheKey(groupId, repoChoice)] = {
      folderName: normalizedFolderName,
      updatedAt: new Date().toISOString()
    };
    this.groupFolderCache = cache;
    await ensureDir(path.dirname(this.groupFolderCacheFile));
    await fs.writeFile(this.groupFolderCacheFile, JSON.stringify(cache, null, 2), 'utf8');
  }

  #pickExistingMindustryFolder(folders, repoChoice) {
    const normalizedFolders = (Array.isArray(folders) ? folders : [])
      .map((folder) => ({
        folderName: normalizeText(folder?.folder_name ?? folder?.name),
        lowered: normalizeText(folder?.folder_name ?? folder?.name).toLowerCase()
      }))
      .filter((folder) => folder.folderName);

    if (repoChoice === 'x') {
      const exact = normalizedFolders.find((folder) => folder.lowered === 'mindustryx');
      if (exact) {
        return exact.folderName;
      }
      const fuzzy = normalizedFolders.find((folder) => folder.lowered.includes('mindustryx'));
      return fuzzy?.folderName ?? '';
    }

    const exact = normalizedFolders.find((folder) => folder.lowered === 'mindustry');
    if (exact) {
      return exact.folderName;
    }
    const fuzzy = normalizedFolders.find((folder) => folder.lowered.includes('mindustry') && !folder.lowered.includes('mindustryx'));
    return fuzzy?.folderName ?? '';
  }

  async #getSessionTargetFolderName(session) {
    const resolved = normalizeText(session?.resolvedFolderName);
    if (resolved) {
      return resolved;
    }

    const explicit = normalizeText(session?.folderName);
    if (explicit) {
      session.resolvedFolderName = explicit;
      return explicit;
    }

    const groupId = normalizeText(session?.groupId);
    const repoChoice = normalizeText(session?.repo?.choice ?? session?.repoChoice);
    if (!groupId) {
      session.resolvedFolderName = '';
      return '';
    }

    const cached = await this.#getCachedGroupFolderName(groupId, repoChoice);
    if (cached) {
      session.resolvedFolderName = cached;
      this.logger.info(`群文件下载使用缓存目录：group=${groupId} repo=${repoChoice || '-'} folder=${cached}`);
      return cached;
    }

    try {
      const root = await this.napcatClient.call('get_group_root_files', {
        group_id: groupId,
        file_count: 500
      });
      const matched = this.#pickExistingMindustryFolder(root?.folders, repoChoice);
      if (matched) {
        await this.#rememberGroupFolderName(groupId, repoChoice, matched);
        session.resolvedFolderName = matched;
        this.logger.info(`群文件下载自动匹配目录：group=${groupId} repo=${repoChoice || '-'} folder=${matched}`);
        return matched;
      }
    } catch (error) {
      this.logger.warn(`扫描群文件根目录失败，回退根目录上传：group=${groupId} error=${error.message}`);
    }

    session.resolvedFolderName = '';
    return '';
  }

  async #getPreferredMirrorBase() {
    const cache = await this.#loadMirrorCache();
    const entry = cache?.githubReleaseMirror;
    const base = normalizeText(entry?.base);
    const expiresAt = Number(entry?.expiresAt ?? 0);
    if (!base || !expiresAt || expiresAt <= Date.now()) {
      return '';
    }
    return base;
  }

  async #rememberPreferredMirrorBase(base) {
    const normalizedBase = normalizeText(base);
    if (!normalizedBase) {
      return;
    }
    const cache = await this.#loadMirrorCache();
    cache.githubReleaseMirror = {
      base: normalizedBase,
      expiresAt: Date.now() + PREFERRED_MIRROR_TTL_MS
    };
    this.mirrorCache = cache;
    await ensureDir(path.dirname(this.mirrorCacheFile));
    await fs.writeFile(this.mirrorCacheFile, JSON.stringify(cache, null, 2), 'utf8');
  }

  async #getPreferredGitMirrorBase() {
    const cache = await this.#loadMirrorCache();
    const entry = cache?.githubGitMirror;
    const base = normalizeText(entry?.base);
    const expiresAt = Number(entry?.expiresAt ?? 0);
    if (!base || !expiresAt || expiresAt <= Date.now()) {
      return '';
    }
    return base;
  }

  async #rememberPreferredGitMirrorBase(base) {
    const normalizedBase = normalizeText(base);
    if (!normalizedBase) {
      return;
    }
    const cache = await this.#loadMirrorCache();
    cache.githubGitMirror = {
      base: normalizedBase,
      expiresAt: Date.now() + PREFERRED_MIRROR_TTL_MS
    };
    this.mirrorCache = cache;
    await ensureDir(path.dirname(this.mirrorCacheFile));
    await fs.writeFile(this.mirrorCacheFile, JSON.stringify(cache, null, 2), 'utf8');
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

  async #buildDownloadCandidates(sourceUrl, excludedMirror = '') {
    const rankedMirrors = await this.#probeMirrorLatencies(sourceUrl);
    this.logger.info(`GitHub 镜像前五：${rankedMirrors.slice(0, MAX_CONCURRENT_DOWNLOADS).map((item) => `${item.base}(${item.latencyMs}ms)`).join(' | ') || '(none)'}`);
    return [
      ...rankedMirrors
        .filter((item) => item.base !== excludedMirror)
        .slice(0, MAX_CONCURRENT_DOWNLOADS)
        .map((item) => ({
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

  async #buildRankedGitMirrorBases(sourceUrl, limit = MAX_CONCURRENT_DOWNLOADS) {
    if (!isGithubHttpsUrl(sourceUrl)) {
      return [];
    }
    const preferredGitMirror = await this.#getPreferredGitMirrorBase();
    const preferredDownloadMirror = await this.#getPreferredMirrorBase();
    const rankedMirrors = await this.#probeGitMirrorLatencies(sourceUrl);
    const orderedBases = [
      ...[preferredGitMirror, preferredDownloadMirror].filter(Boolean),
      ...rankedMirrors.map((item) => item.base)
    ];
    return Array.from(new Set(orderedBases))
      .slice(0, clampInteger(limit, MAX_CONCURRENT_DOWNLOADS, 1, 10));
  }

  async #probeGitMirrorLatencies(sourceUrl) {
    const results = await Promise.all(
      GITHUB_DOWNLOAD_MIRRORS.map(async (base) => ({
        base,
        ...(await this.#probeSingleGitMirror(base, sourceUrl))
      }))
    );
    const successful = results
      .filter((item) => item.ok)
      .sort((left, right) => left.latencyMs - right.latencyMs);
    const summary = results
      .map((item) => `${item.base}=${item.ok ? `${item.latencyMs}ms` : `error:${item.error}`}`)
      .join(' | ');
    this.logger.info(`Git 镜像测速：${summary}`);
    return successful;
  }

  async #probeSingleGitMirror(base, sourceUrl) {
    const probeUrl = buildGitMirrorProbeUrl(base, sourceUrl);
    if (!probeUrl) {
      return {
        ok: false,
        error: 'empty-probe-url'
      };
    }

    const startedAt = Date.now();
    try {
      const response = await fetch(probeUrl, {
        method: 'GET',
        headers: {
          Accept: '*/*',
          'User-Agent': 'NapCatCainBot/0.1'
        },
        redirect: 'follow',
        signal: AbortSignal.timeout(8000)
      });
      if (response.body && typeof response.body.cancel === 'function') {
        response.body.cancel().catch(() => {});
      }
      if (!response.ok) {
        return {
          ok: false,
          error: `HTTP ${response.status}`
        };
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

  async #runGitCommandWithMirrorFallback(args = [], options = {}) {
    const sourceUrl = normalizeText(options?.sourceUrlHint);
    const timeout = clampInteger(options?.timeout, 120000, 1000, BUILD_TIMEOUT_MS);
    const maxBuffer = clampInteger(options?.maxBuffer, 8 * 1024 * 1024, 1024 * 1024, 64 * 1024 * 1024);
    const label = normalizeText(options?.label) || 'git-command';
    const mirrorLimit = clampInteger(options?.mirrorLimit, MAX_CONCURRENT_DOWNLOADS, 1, 10);
    const commandArgs = Array.isArray(args) ? args : [];

    const runOnce = async (prefixArgs, routeLabel, mirrorBase = '') => {
      this.logger.info(`[${label}] start via ${routeLabel}: git ${[...prefixArgs, ...commandArgs].join(' ')}`.trim());
      await execFileAsync('git', [...prefixArgs, ...commandArgs], {
        windowsHide: true,
        timeout,
        maxBuffer
      });
      this.logger.info(`[${label}] done via ${routeLabel}`);
      if (mirrorBase) {
        await this.#rememberPreferredGitMirrorBase(mirrorBase);
      }
    };

    if (!isGithubHttpsUrl(sourceUrl)) {
      await runOnce([], 'source');
      return;
    }

    const mirrorBases = await this.#buildRankedGitMirrorBases(sourceUrl, mirrorLimit);
    let lastMirrorError = null;
    for (const mirrorBase of mirrorBases) {
      try {
        await runOnce(buildGitMirrorConfigArgs(mirrorBase), `mirror:${mirrorBase}`, mirrorBase);
        return;
      } catch (error) {
        lastMirrorError = error;
        this.logger.warn(`[${label}] git 镜像失败：base=${mirrorBase} error=${error.message}`);
      }
    }

    try {
      await runOnce([], 'source');
    } catch (error) {
      if (lastMirrorError) {
        this.logger.warn(`[${label}] 全部镜像失败后原链也失败：mirrorError=${lastMirrorError.message} sourceError=${error.message}`);
      }
      throw error;
    }
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
        lastError: null,
        winnerLabel: candidates[winner.index]?.label ?? ''
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
        lastError: lastError ?? aggregateError,
        winnerLabel: ''
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
