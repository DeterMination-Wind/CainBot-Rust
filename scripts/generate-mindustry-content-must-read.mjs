import fs from 'node:fs/promises';
import path from 'node:path';

const defaultPaths = {
  blocks: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\src\\mindustry\\content\\Blocks.java',
  units: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\src\\mindustry\\content\\UnitTypes.java',
  items: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\src\\mindustry\\content\\Items.java',
  liquids: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\src\\mindustry\\content\\Liquids.java',
  statuses: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\src\\mindustry\\content\\StatusEffects.java',
  planets: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\src\\mindustry\\content\\Planets.java',
  weathers: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\src\\mindustry\\content\\Weathers.java',
  bundle: 'C:\\Users\\华硕\\Documents\\codex\\MindustryX-main\\work\\core\\assets\\bundles\\bundle_zh_CN.properties',
  output: 'C:\\Users\\华硕\\Documents\\codex\\mindustryx-content(MustRead_if_the_questions_are_about_mindustry_instances).json'
};

const fieldNotes = {
  contentKind: '内容类别；用于和结构定义 JSON 分工，避免重复类型字段说明。',
  sourceGroup: '源文件中的注释分组，仅用于帮助 AI 粗分内容，不是游戏运行时字段。',
  localizedNameZhCN: '从 bundle_zh_CN.properties 提取的中文名称。',
  internalName: '游戏内部名称。',
  sourceFile: '该内容来自的源码文件。'
};

const kindSpecs = [
  {
    kind: 'block',
    bundlePrefix: 'block',
    fileKey: 'blocks',
    sourceFile: 'mindustry/content/Blocks.java',
    allowedAssignments: ['size', 'health', 'alwaysUnlocked', 'hidden', 'placeablePlayer', 'itemDrop', 'liquidDrop', 'status', 'isLiquid'],
    extraParsers: [parseBlockRequirements]
  },
  {
    kind: 'unit',
    bundlePrefix: 'unit',
    fileKey: 'units',
    sourceFile: 'mindustry/content/UnitTypes.java',
    allowedAssignments: ['speed', 'health', 'armor', 'hitSize', 'flying', 'naval', 'lowAltitude', 'targetAir', 'targetGround', 'mineTier', 'alwaysUnlocked', 'hidden', 'itemCapacity'],
    extraParsers: []
  },
  {
    kind: 'item',
    bundlePrefix: 'item',
    fileKey: 'items',
    sourceFile: 'mindustry/content/Items.java',
    allowedAssignments: ['alwaysUnlocked', 'hidden', 'hardness', 'flammability', 'explosiveness', 'charge', 'radioactivity', 'cost', 'healthScaling', 'buildable'],
    extraParsers: []
  },
  {
    kind: 'liquid',
    bundlePrefix: 'liquid',
    fileKey: 'liquids',
    sourceFile: 'mindustry/content/Liquids.java',
    allowedAssignments: ['gas', 'hidden', 'alwaysUnlocked', 'coolant', 'flammability', 'explosiveness', 'temperature', 'heatCapacity', 'viscosity', 'effect', 'boilPoint'],
    extraParsers: []
  },
  {
    kind: 'status',
    bundlePrefix: 'status',
    fileKey: 'statuses',
    sourceFile: 'mindustry/content/StatusEffects.java',
    allowedAssignments: ['speedMultiplier', 'healthMultiplier', 'damageMultiplier', 'reloadMultiplier', 'damage', 'intervalDamage', 'permanent', 'reactive', 'disarm', 'show', 'dynamic'],
    extraParsers: []
  },
  {
    kind: 'planet',
    bundlePrefix: 'planet',
    fileKey: 'planets',
    sourceFile: 'mindustry/content/Planets.java',
    allowedAssignments: ['accessible', 'visible', 'alwaysUnlocked', 'startSector', 'defaultEnv', 'defaultCore', 'allowWaves', 'allowSelfSectorLaunch', 'allowLegacyLaunchPads', 'allowLaunchSchematics', 'allowLaunchLoadout', 'allowSectorInvasion', 'loadPlanetData', 'tidalLock'],
    extraParsers: []
  },
  {
    kind: 'weather',
    bundlePrefix: 'weather',
    fileKey: 'weathers',
    sourceFile: 'mindustry/content/Weathers.java',
    allowedAssignments: ['hidden', 'duration', 'status', 'statusAir', 'statusGround', 'sound', 'opacityMultiplier', 'density', 'baseSpeed'],
    extraParsers: [parseAttributeSets]
  }
];

function parseBlockRequirements(statement) {
  const match = statement.match(/\brequirements\(\s*Category\.([A-Za-z0-9_]+)/);
  if (!match) {
    return null;
  }
  return ['category', `Category.${match[1]}`];
}

function parseAttributeSets(statement, context) {
  if (context.kind !== 'weather') {
    return null;
  }
  const match = statement.match(/\battrs\.set\(\s*Attribute\.([A-Za-z0-9_]+)\s*,\s*([^)]+)\)/);
  if (!match) {
    return null;
  }
  return ['attributeMods', `${match[1]}=${normalizeValue(match[2])}`, true];
}

function normalizeValue(value) {
  return String(value ?? '')
    .replace(/\s+/g, ' ')
    .replace(/\s*([(),;{}])\s*/g, '$1')
    .trim();
}

function inferValueType(value) {
  if (value === 'true' || value === 'false') {
    return 'boolean';
  }
  if (/^-?\d+(?:\.\d+)?f?$/i.test(value)) {
    return 'number';
  }
  if (/^[A-Za-z_][A-Za-z0-9_]*(?:\.[A-Za-z_][A-Za-z0-9_]*)+$/.test(value)) {
    return 'reference';
  }
  if (/^[A-Za-z_][A-Za-z0-9_]*$/.test(value)) {
    return 'identifier';
  }
  if (/^".*"$/.test(value)) {
    return 'string';
  }
  if (/[()]/.test(value) || /\bnew\b/.test(value) || /->/.test(value) || /[*\/|&]/.test(value) || /\s[+-]\s/.test(value)) {
    return 'expression';
  }
  if (/^[\p{L}\p{N}_\- ,:+]+$/u.test(value)) {
    return 'string';
  }
  return 'expression';
}

function makeField(name, value, notes = '') {
  return {
    name,
    type: inferValueType(value),
    defaultValue: value,
    notes: notes || fieldNotes[name] || ''
  };
}

function parsePropertiesFile(text) {
  const entries = new Map();
  const lines = text.split(/\r?\n/);
  for (const rawLine of lines) {
    const line = rawLine.trim();
    if (!line || line.startsWith('#') || line.startsWith('!')) {
      continue;
    }
    const separatorIndex = line.search(/[:=]/);
    if (separatorIndex <= 0) {
      continue;
    }
    const key = line.slice(0, separatorIndex).trim();
    const value = line.slice(separatorIndex + 1).trim();
    if (key) {
      entries.set(key, value);
    }
  }
  return entries;
}

function findMethodBody(source, methodSignature) {
  const signatureIndex = source.indexOf(methodSignature);
  if (signatureIndex < 0) {
    throw new Error(`未找到方法：${methodSignature}`);
  }
  const openBraceIndex = source.indexOf('{', signatureIndex);
  if (openBraceIndex < 0) {
    throw new Error(`未找到方法体开始：${methodSignature}`);
  }
  const closeBraceIndex = findMatchingDelimiter(source, openBraceIndex, '{', '}');
  return source.slice(openBraceIndex + 1, closeBraceIndex);
}

function findMatchingDelimiter(text, startIndex, openChar, closeChar) {
  let depth = 0;
  let inString = false;
  let escaped = false;
  let inLineComment = false;
  let inBlockComment = false;
  for (let index = startIndex; index < text.length; index += 1) {
    const char = text[index];
    const next = text[index + 1];
    if (inLineComment) {
      if (char === '\n') {
        inLineComment = false;
      }
      continue;
    }
    if (inBlockComment) {
      if (char === '*' && next === '/') {
        inBlockComment = false;
        index += 1;
      }
      continue;
    }
    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (char === '\\') {
        escaped = true;
      } else if (char === '"') {
        inString = false;
      }
      continue;
    }
    if (char === '/' && next === '/') {
      inLineComment = true;
      index += 1;
      continue;
    }
    if (char === '/' && next === '*') {
      inBlockComment = true;
      index += 1;
      continue;
    }
    if (char === '"') {
      inString = true;
      continue;
    }
    if (char === openChar) {
      depth += 1;
      continue;
    }
    if (char === closeChar) {
      depth -= 1;
      if (depth === 0) {
        return index;
      }
    }
  }
  throw new Error(`未找到匹配的 ${closeChar}`);
}

function stripLineComment(line) {
  let inString = false;
  let escaped = false;
  for (let index = 0; index < line.length; index += 1) {
    const char = line[index];
    const next = line[index + 1];
    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (char === '\\') {
        escaped = true;
      } else if (char === '"') {
        inString = false;
      }
      continue;
    }
    if (char === '"') {
      inString = true;
      continue;
    }
    if (char === '/' && next === '/') {
      return line.slice(0, index);
    }
  }
  return line;
}

function updateBraceDepth(line, depth) {
  const cleanLine = stripLineComment(line);
  let inString = false;
  let escaped = false;
  let result = depth;
  for (let index = 0; index < cleanLine.length; index += 1) {
    const char = cleanLine[index];
    if (inString) {
      if (escaped) {
        escaped = false;
      } else if (char === '\\') {
        escaped = true;
      } else if (char === '"') {
        inString = false;
      }
      continue;
    }
    if (char === '"') {
      inString = true;
      continue;
    }
    if (char === '{') {
      result += 1;
    } else if (char === '}') {
      result -= 1;
    }
  }
  return result;
}

function normalizeGroupComment(line) {
  const comment = line.trim();
  if (!comment.startsWith('//')) {
    return null;
  }
  const normalized = comment
    .replace(/^\/\/\s*region\s*/i, '')
    .replace(/^\/\//, '')
    .trim();
  if (!normalized || /^end(region)?$/i.test(normalized)) {
    return '';
  }
  if (/register|looked up|needed here|todo/i.test(normalized)) {
    return null;
  }
  if (normalized.length > 48 && /\s/.test(normalized)) {
    return null;
  }
  return normalized;
}

function collectGroupedStatements(body) {
  const lines = body.split(/\r?\n/);
  const statements = [];
  let currentGroup = '';
  let currentStatement = [];
  let statementGroup = '';
  let braceDepth = 0;

  const flushIfComplete = (trimmedLine) => {
    if (currentStatement.length === 0 || braceDepth !== 0) {
      return;
    }
    if (!/[;}]\s*$/.test(trimmedLine)) {
      return;
    }
    const text = currentStatement.join('\n').trim();
    if (text) {
      statements.push({ text, group: statementGroup });
    }
    currentStatement = [];
    statementGroup = '';
  };

  for (const line of lines) {
    const trimmed = line.trim();
    if (braceDepth === 0 && currentStatement.length === 0) {
      const maybeGroup = normalizeGroupComment(trimmed);
      if (maybeGroup !== null) {
        currentGroup = maybeGroup;
        continue;
      }
      if (!trimmed) {
        continue;
      }
      statementGroup = currentGroup;
    }

    currentStatement.push(line);
    braceDepth = updateBraceDepth(line, braceDepth);
    flushIfComplete(stripLineComment(line).trim());
  }

  if (currentStatement.length > 0) {
    const text = currentStatement.join('\n').trim();
    if (text) {
      statements.push({ text, group: statementGroup });
    }
  }

  return statements;
}

function extractInnerInitializerBody(statement, closeParenIndex) {
  const rest = statement.slice(closeParenIndex + 1).trimStart();
  if (!rest.startsWith('{{')) {
    return '';
  }
  const firstBraceIndex = statement.indexOf('{', closeParenIndex);
  const endBraceIndex = findMatchingDelimiter(statement, firstBraceIndex, '{', '}');
  const blockText = statement.slice(firstBraceIndex, endBraceIndex + 1);
  if (!blockText.startsWith('{{') || !blockText.endsWith('}}')) {
    return '';
  }
  return blockText.slice(2, -2);
}

function parseConstructorStatement(statement) {
  const newMatch = statement.match(/^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*new\s+([A-Za-z0-9_$.<>]+)\s*\(/);
  if (newMatch) {
    const variableName = newMatch[1];
    const constructorName = newMatch[2].replace(/<.*$/, '');
    const parenIndex = statement.indexOf('(', newMatch.index + newMatch[0].length - 1);
    const closeParenIndex = findMatchingDelimiter(statement, parenIndex, '(', ')');
    const args = statement.slice(parenIndex + 1, closeParenIndex);
    const body = extractInnerInitializerBody(statement, closeParenIndex);
    return {
      variableName,
      constructorName,
      args,
      body,
      factory: 'new'
    };
  }

  const asteroidMatch = statement.match(/^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*makeAsteroid\s*\(/);
  if (!asteroidMatch) {
    return null;
  }
  const variableName = asteroidMatch[1];
  const parenIndex = statement.indexOf('(', asteroidMatch.index + asteroidMatch[0].length - 1);
  const closeParenIndex = findMatchingDelimiter(statement, parenIndex, '(', ')');
  const args = statement.slice(parenIndex + 1, closeParenIndex);
  return {
    variableName,
    constructorName: 'Planet',
    args,
    body: '',
    factory: 'makeAsteroid'
  };
}

function firstQuotedString(text) {
  const match = text.match(/"([^"]+)"/);
  return match ? match[1] : '';
}

function collectTopLevelStatements(body) {
  return collectGroupedStatements(body).map((item) => item.text);
}

function appendField(fieldMap, name, value, notes = '', append = false) {
  if (!name || !value) {
    return;
  }
  if (append) {
    const previous = fieldMap.get(name);
    const nextValue = previous ? `${previous.defaultValue}, ${value}` : value;
    fieldMap.set(name, makeField(name, nextValue, notes));
    return;
  }
  if (!fieldMap.has(name)) {
    fieldMap.set(name, makeField(name, value, notes));
  }
}

function parseEntryFields(entry, spec, localizedName) {
  const fieldMap = new Map();
  appendField(fieldMap, 'contentKind', spec.kind, fieldNotes.contentKind);
  appendField(fieldMap, 'internalName', entry.internalName, fieldNotes.internalName);
  if (localizedName) {
    appendField(fieldMap, 'localizedNameZhCN', localizedName, fieldNotes.localizedNameZhCN);
  }
  if (entry.group) {
    appendField(fieldMap, 'sourceGroup', entry.group, fieldNotes.sourceGroup);
  }
  appendField(fieldMap, 'sourceFile', spec.sourceFile, fieldNotes.sourceFile);

  if (spec.kind === 'planet') {
    const argsTokens = entry.args.split(',').map((item) => normalizeValue(item));
    if (argsTokens.length >= 2 && argsTokens[1] && argsTokens[1] !== 'null') {
      appendField(fieldMap, 'parent', argsTokens[1], '该星球在源码中的父级天体。');
    }
    if (entry.factory === 'makeAsteroid') {
      appendField(fieldMap, 'factory', 'makeAsteroid', '通过 Planets.makeAsteroid 构造的星体。');
    }
  }

  const statements = collectTopLevelStatements(entry.body);
  for (const statement of statements) {
    const normalized = normalizeValue(statement);
    const directAssignment = normalized.match(/^([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.+);$/);
    if (directAssignment) {
      const fieldName = directAssignment[1];
      const value = normalizeValue(directAssignment[2]);
      if (spec.allowedAssignments.includes(fieldName)) {
        appendField(fieldMap, fieldName, value);
      }
    }

    for (const parser of spec.extraParsers) {
      const parsed = parser(normalized, spec);
      if (!parsed) {
        continue;
      }
      const [fieldName, value, append] = parsed;
      appendField(fieldMap, fieldName, value, '', append === true);
    }
  }

  return Array.from(fieldMap.values());
}

function parseEntriesFromSource(source, spec, bundle) {
  const loadBody = findMethodBody(source, 'public static void load(){');
  const groupedStatements = collectGroupedStatements(loadBody);
  const entries = [];
  for (const item of groupedStatements) {
    const parsed = parseConstructorStatement(item.text);
    if (!parsed) {
      continue;
    }
    const internalName = firstQuotedString(parsed.args) || parsed.variableName;
    const localizedName = bundle.get(`${spec.bundlePrefix}.${internalName}.name`) || '';
    entries.push({
      variableName: parsed.variableName,
      internalName,
      constructorName: parsed.constructorName,
      args: parsed.args,
      body: parsed.body,
      group: item.group,
      factory: parsed.factory,
      localizedName
    });
  }
  return entries;
}

function parseItemPlanetSets(source) {
  const result = new Map();
  const assignSet = (collectionName, label) => {
    const match = source.match(new RegExp(`${collectionName}\\.addAll\\(([\\s\\S]*?)\\);`));
    if (!match) {
      return;
    }
    const tokens = match[1]
      .split(',')
      .map((item) => normalizeValue(item))
      .filter((item) => /^[A-Za-z_][A-Za-z0-9_]*$/.test(item));
    for (const token of tokens) {
      const current = result.get(token) ?? [];
      if (!current.includes(label)) {
        current.push(label);
      }
      result.set(token, current);
    }
  };

  assignSet('serpuloItems', 'serpulo');
  assignSet('erekirItems', 'erekir');
  assignSet('erekirOnlyItems', 'erekir-only');
  return result;
}

function sortFields(fields) {
  const preferredOrder = [
    'contentKind',
    'internalName',
    'localizedNameZhCN',
    'sourceGroup',
    'sourceFile',
    'parent',
    'factory',
    'category',
    'size',
    'health',
    'armor',
    'speed',
    'hitSize',
    'hardness',
    'itemDrop',
    'liquidDrop',
    'status',
    'targetAir',
    'targetGround',
    'flying',
    'naval',
    'lowAltitude',
    'mineTier',
    'gas',
    'coolant',
    'alwaysUnlocked',
    'hidden',
    'placeablePlayer',
    'isLiquid',
    'accessible',
    'visible',
    'startSector',
    'defaultEnv',
    'defaultCore',
    'planetSets',
    'attributeMods'
  ];
  return [...fields].sort((left, right) => {
    const leftOrder = preferredOrder.indexOf(left.name);
    const rightOrder = preferredOrder.indexOf(right.name);
    if (leftOrder !== rightOrder) {
      return (leftOrder < 0 ? Number.MAX_SAFE_INTEGER : leftOrder) - (rightOrder < 0 ? Number.MAX_SAFE_INTEGER : rightOrder);
    }
    return left.name.localeCompare(right.name, 'zh-CN');
  });
}

async function main() {
  const bundleText = await fs.readFile(defaultPaths.bundle, 'utf8');
  const bundle = parsePropertiesFile(bundleText);
  const outputs = [];

  for (const spec of kindSpecs) {
    const source = await fs.readFile(defaultPaths[spec.fileKey], 'utf8');
    const entries = parseEntriesFromSource(source, spec, bundle);
    const itemPlanetSets = spec.kind === 'item' ? parseItemPlanetSets(source) : null;

    for (const entry of entries) {
      const fields = parseEntryFields(entry, spec, entry.localizedName);
      if (itemPlanetSets?.has(entry.variableName)) {
        fields.push(makeField('planetSets', itemPlanetSets.get(entry.variableName).join(', '), '该物品在源码静态列表中的星球集合。'));
      }
      outputs.push({
        type: entry.internalName,
        extends: entry.constructorName,
        fields: sortFields(fields)
      });
    }
  }

  outputs.sort((left, right) => {
    const leftKind = left.fields.find((field) => field.name === 'contentKind')?.defaultValue ?? '';
    const rightKind = right.fields.find((field) => field.name === 'contentKind')?.defaultValue ?? '';
    return leftKind.localeCompare(rightKind, 'zh-CN') || left.type.localeCompare(right.type, 'zh-CN');
  });

  await fs.writeFile(defaultPaths.output, `${JSON.stringify(outputs, null, 2)}\n`, 'utf8');
  console.log(`generated ${outputs.length} entries -> ${defaultPaths.output}`);
}

await main();
