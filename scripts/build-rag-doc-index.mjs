import fs from 'node:fs/promises';
import path from 'node:path';

const MARKDOWN_EXTENSIONS = new Set(['.md', '.mdx', '.markdown']);
const SKIP_DIRS = new Set([
  '.git',
  '.github',
  '.vitepress',
  'node_modules',
  'dist',
  'build',
  'out',
  '.next',
  '.cache',
  'coverage'
]);

function normalizeText(value) {
  return String(value ?? '').replace(/\s+/g, ' ').trim();
}

function truncateText(value, maxChars = 280) {
  const normalized = normalizeText(value);
  if (normalized.length <= maxChars) {
    return normalized;
  }
  return `${normalized.slice(0, maxChars - 1)}…`;
}

function parseArgs(argv) {
  const options = {
    root: '',
    out: '',
    label: '',
    maxChars: 1100,
    minChars: 320
  };

  for (let index = 0; index < argv.length; index += 1) {
    const current = String(argv[index] ?? '').trim();
    const next = String(argv[index + 1] ?? '').trim();
    if (current === '--root' && next) {
      options.root = next;
      index += 1;
      continue;
    }
    if (current === '--out' && next) {
      options.out = next;
      index += 1;
      continue;
    }
    if (current === '--label' && next) {
      options.label = next;
      index += 1;
      continue;
    }
    if (current === '--max-chars' && next) {
      options.maxChars = Math.max(400, Number(next) || options.maxChars);
      index += 1;
      continue;
    }
    if (current === '--min-chars' && next) {
      options.minChars = Math.max(120, Number(next) || options.minChars);
      index += 1;
    }
  }

  return options;
}

function cleanHeadingText(value) {
  return normalizeText(
    String(value ?? '')
      .replace(/\s+\{#.+\}\s*$/u, '')
      .replace(/<[^>]+>/g, ' ')
      .replace(/!\[[^\]]*]\([^)]+\)/g, ' ')
      .replace(/\[([^\]]+)]\([^)]+\)/g, '$1')
      .replace(/`([^`]+)`/g, '$1')
  );
}

function slugifyFragment(value) {
  const normalized = cleanHeadingText(value)
    .normalize('NFKC')
    .toLowerCase()
    .replace(/[^\p{Letter}\p{Number}\s-]/gu, ' ')
    .replace(/\s+/g, '-')
    .replace(/-+/g, '-')
    .replace(/^-|-$/g, '');
  return normalized;
}

function stripFrontmatter(text) {
  const source = String(text ?? '').replace(/^\uFEFF/, '');
  const lines = source.split(/\r?\n/);
  if (lines[0]?.trim() !== '---') {
    return {
      body: source,
      data: {}
    };
  }

  let closingIndex = -1;
  for (let index = 1; index < lines.length; index += 1) {
    if (lines[index].trim() === '---') {
      closingIndex = index;
      break;
    }
  }
  if (closingIndex < 0) {
    return {
      body: source,
      data: {}
    };
  }

  const data = {};
  for (const line of lines.slice(1, closingIndex)) {
    const match = line.match(/^([A-Za-z0-9_-]+)\s*:\s*(.+)$/);
    if (!match) {
      continue;
    }
    data[match[1]] = String(match[2] ?? '').trim().replace(/^['"]|['"]$/g, '');
  }

  return {
    body: lines.slice(closingIndex + 1).join('\n'),
    data
  };
}

function splitBlocks(section) {
  const blocks = [];
  let current = [];
  let inCodeFence = false;

  const flush = () => {
    if (current.length === 0) {
      return;
    }
    blocks.push(current);
    current = [];
  };

  for (const item of section.lines) {
    const trimmed = item.text.trim();
    if (/^(```|~~~)/.test(trimmed)) {
      current.push(item);
      inCodeFence = !inCodeFence;
      continue;
    }
    if (!inCodeFence && !trimmed) {
      flush();
      continue;
    }
    current.push(item);
  }

  flush();
  return blocks;
}

function splitOversizedBlock(block, maxChars, minChars) {
  const segments = [];
  let current = [];

  const flush = () => {
    if (current.length === 0) {
      return;
    }
    segments.push(current);
    current = [];
  };

  for (const line of block) {
    current.push(line);
    const currentText = normalizeText(current.map((item) => item.text).join('\n'));
    if (currentText.length >= maxChars) {
      flush();
    }
  }

  flush();
  if (segments.length > 1) {
    const last = segments.at(-1);
    const previous = segments.at(-2);
    const lastText = normalizeText((last ?? []).map((item) => item.text).join('\n'));
    if (last && previous && lastText.length < minChars) {
      previous.push(...last);
      segments.pop();
    }
  }

  return segments.length > 0 ? segments : [block];
}

function buildSectionChunks(section, relativePath, options) {
  const blocks = splitBlocks(section).flatMap((block) => {
    const text = normalizeText(block.map((item) => item.text).join('\n'));
    if (text.length <= options.maxChars) {
      return [block];
    }
    return splitOversizedBlock(block, options.maxChars, options.minChars);
  });

  const chunks = [];
  let currentBlocks = [];

  const emit = () => {
    if (currentBlocks.length === 0) {
      return;
    }
    const flattened = currentBlocks.flat();
    const rawText = flattened.map((item) => item.text).join('\n').trim();
    const normalizedBody = normalizeText(rawText);
    if (!normalizedBody) {
      currentBlocks = [];
      return;
    }

    const headingTrail = section.headings.filter(Boolean);
    const sectionTitle = headingTrail.at(-1) || section.title || path.basename(relativePath, path.extname(relativePath));
    const anchor = slugifyFragment(sectionTitle);
    const startLine = Number(flattened[0]?.line ?? section.startLine) || section.startLine;
    const endLine = Number(flattened.at(-1)?.line ?? startLine) || startLine;
    chunks.push({
      id: `${relativePath}:${startLine}`,
      path: relativePath.replace(/\\/g, '/'),
      title: section.title || path.basename(relativePath, path.extname(relativePath)),
      section: sectionTitle,
      headings: headingTrail,
      anchor,
      startLine,
      endLine,
      text: rawText,
      summary: truncateText(rawText)
    });
    currentBlocks = [];
  };

  for (const block of blocks) {
    const blockText = normalizeText(block.map((item) => item.text).join('\n'));
    const currentText = normalizeText(currentBlocks.flat().map((item) => item.text).join('\n'));
    const combinedText = normalizeText([currentText, blockText].filter(Boolean).join('\n\n'));
    if (currentBlocks.length > 0 && combinedText.length > options.maxChars && currentText.length >= options.minChars) {
      emit();
    }
    currentBlocks.push(block);
  }

  emit();
  return chunks;
}

function buildSections(relativePath, text, metadata) {
  const titleFromMeta = cleanHeadingText(metadata?.title ?? '');
  const baseTitle = titleFromMeta || path.basename(relativePath, path.extname(relativePath));
  const lines = String(text ?? '').split(/\r?\n/);
  const sections = [];
  let headingTrail = [];
  let current = {
    title: baseTitle,
    headings: [],
    startLine: 1,
    lines: []
  };
  let inCodeFence = false;

  const flush = () => {
    if (current.lines.length === 0) {
      return;
    }
    sections.push({
      title: current.title,
      headings: current.headings.slice(),
      startLine: current.startLine,
      lines: current.lines.slice()
    });
  };

  for (let index = 0; index < lines.length; index += 1) {
    const textLine = lines[index];
    const trimmed = textLine.trim();
    if (/^(```|~~~)/.test(trimmed)) {
      inCodeFence = !inCodeFence;
      current.lines.push({ line: index + 1, text: textLine });
      continue;
    }

    const headingMatch = !inCodeFence ? trimmed.match(/^(#{1,6})\s+(.+?)\s*$/u) : null;
    if (headingMatch) {
      flush();
      const level = headingMatch[1].length;
      headingTrail = headingTrail.slice(0, level - 1);
      headingTrail[level - 1] = cleanHeadingText(headingMatch[2]);
      current = {
        title: baseTitle,
        headings: headingTrail.slice(),
        startLine: index + 1,
        lines: [{ line: index + 1, text: textLine }]
      };
      continue;
    }

    current.lines.push({ line: index + 1, text: textLine });
  }

  flush();
  return sections;
}

async function collectMarkdownFiles(rootPath, currentDir = rootPath, results = []) {
  const entries = await fs.readdir(currentDir, { withFileTypes: true });
  for (const entry of entries) {
    const fullPath = path.join(currentDir, entry.name);
    if (entry.isDirectory()) {
      if (SKIP_DIRS.has(entry.name)) {
        continue;
      }
      await collectMarkdownFiles(rootPath, fullPath, results);
      continue;
    }
    if (!entry.isFile()) {
      continue;
    }
    if (MARKDOWN_EXTENSIONS.has(path.extname(entry.name).toLowerCase())) {
      results.push(fullPath);
    }
  }
  return results;
}

async function buildIndex(rootPath, options) {
  const files = await collectMarkdownFiles(rootPath);
  const chunks = [];

  for (const filePath of files.sort((left, right) => left.localeCompare(right, 'zh-CN'))) {
    const relativePath = path.relative(rootPath, filePath).replace(/\\/g, '/');
    const rawText = await fs.readFile(filePath, 'utf8');
    const { body, data } = stripFrontmatter(rawText);
    const sections = buildSections(relativePath, body, data);
    for (const section of sections) {
      chunks.push(...buildSectionChunks(section, relativePath, options));
    }
  }

  return {
    version: 1,
    generatedAt: new Date().toISOString(),
    label: options.label || path.basename(rootPath),
    rootPath: rootPath.replace(/\\/g, '/'),
    fileCount: files.length,
    chunkCount: chunks.length,
    chunks
  };
}

async function main() {
  const options = parseArgs(process.argv.slice(2));
  if (!options.root) {
    throw new Error('缺少 --root');
  }

  const rootPath = path.resolve(options.root);
  const outputFile = path.resolve(options.out || path.join(rootPath, '.cain-rag-index.json'));
  const index = await buildIndex(rootPath, options);
  await fs.mkdir(path.dirname(outputFile), { recursive: true });
  await fs.writeFile(outputFile, `${JSON.stringify(index, null, 2)}\n`, 'utf8');
  console.log(`Indexed ${index.fileCount} files into ${index.chunkCount} chunks -> ${outputFile}`);
}

main().catch((error) => {
  console.error(`build-rag-doc-index failed: ${error.message}`);
  process.exitCode = 1;
});
