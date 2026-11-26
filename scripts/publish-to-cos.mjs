// 发布产物到 dl.davenger.cloud（腾讯云 COS 静态站）的上传脚本。
// 布局见 Doc/Rfc/版本分发迁移到dl.davenger.cloud.md §4.1：
//   mykvm/{tag}/          版本资产（不可变，长缓存）
//   mykvm/latest.json     stable 更新清单（可变，no-cache，最后上传）
//   mykvm/beta/latest.json beta 更新清单
//   mykvm/downloads.json  网页下载按钮用的安装包直链
//   mykvm/index.html、mykvm/assets/*  宣传页与截图
// CI（release.yml publish-cos）与本地 bootstrap 共用本脚本。
import { createReadStream, readdirSync, statSync } from 'node:fs';
import { createHmac, createHash } from 'node:crypto';
import { extname, join } from 'node:path';
import { request as httpsRequest } from 'node:https';
import { pathToFileURL } from 'node:url';

export const IMMUTABLE_CACHE = 'public, max-age=31536000, immutable';
// 截图文件名固定、内容可能随版本更新，给一天缓存而不是 immutable。
export const SHORT_CACHE = 'public, max-age=86400';
export const NO_CACHE = 'no-cache';

const DEFAULT_BUCKET = 'voicestick-dl-1329978361';
const DEFAULT_REGION = 'ap-shanghai';
const DEFAULT_PUBLIC_BASE = 'https://dl.davenger.cloud/mykvm';

const CONTENT_TYPES = {
  '.json': 'application/json',
  '.html': 'text/html; charset=utf-8',
  '.png': 'image/png',
  '.gif': 'image/gif',
  '.svg': 'image/svg+xml',
  '.ico': 'image/x-icon',
};

function contentTypeFor(filePath) {
  return CONTENT_TYPES[extname(filePath).toLowerCase()] ?? 'application/octet-stream';
}

export function hmacSha1Hex(key, data) {
  return createHmac('sha1', key).update(data).digest('hex');
}

// 腾讯云 COS 请求签名（q-sign-algorithm=sha1）。
// 编码规则：仅保留字母数字与 - _ . *，其余字符百分号编码。
function cosUriEncode(value) {
  return encodeURIComponent(value).replace(
    /[!'()~]/g,
    (char) => `%${char.charCodeAt(0).toString(16).toUpperCase()}`,
  );
}

function sortedParamString(query) {
  return Object.keys(query)
    .sort()
    .map((key) => `${cosUriEncode(key)}=${cosUriEncode(String(query[key]))}`)
    .join('&');
}

function sortedHeaderString(headers) {
  const normalized = {};
  for (const [key, value] of Object.entries(headers)) {
    normalized[key.toLowerCase()] = String(value).trim();
  }
  return Object.keys(normalized)
    .sort()
    .map((key) => `${key}=${cosUriEncode(normalized[key])}`)
    .join('&');
}

export function cosAuthorization({
  method,
  uri,
  query = {},
  headers,
  secretId,
  secretKey,
  startTimestamp,
  endTimestamp,
}) {
  const keyTime = `${startTimestamp};${endTimestamp}`;
  const signKey = hmacSha1Hex(secretKey, keyTime);
  const httpString = [
    method.toLowerCase(),
    uri,
    sortedParamString(query),
    sortedHeaderString(headers),
    '',
  ].join('\n');
  const stringToSign = ['sha1', keyTime, createHash('sha1').update(httpString).digest('hex'), ''].join('\n');
  const signature = hmacSha1Hex(signKey, stringToSign);
  const headerList = Object.keys(headers)
    .map((key) => key.toLowerCase())
    .sort()
    .join(';');
  const paramList = Object.keys(query).sort().join(';');
  return [
    'q-sign-algorithm=sha1',
    `q-ak=${secretId}`,
    `q-sign-time=${keyTime}`,
    `q-key-time=${keyTime}`,
    `q-header-list=${headerList}`,
    `q-url-param-list=${paramList}`,
    `q-signature=${signature}`,
  ].join('&');
}

// 网页下载按钮用的安装包直链清单。macOS 必须给 .dmg（updater 用的
// .app.tar.gz 是自动更新专用，不是给用户下载的入口）。
export function buildDownloadsJson({ assetNames, version, tag, publicBase, pubDate }) {
  const find = (predicate) => assetNames.find(predicate);
  const windowsSetup =
    find((name) => name === 'mykvm-windows-x64-setup.exe') ??
    find((name) => /\.exe$/i.test(name) && /setup/i.test(name));
  const macDmg = find((name) => /\.dmg$/i.test(name));
  const linuxAppImage = find((name) => /\.AppImage$/i.test(name));

  const files = {};
  if (windowsSetup) files['windows-setup'] = `${publicBase}/${tag}/${windowsSetup}`;
  if (macDmg) files['macos-dmg'] = `${publicBase}/${tag}/${macDmg}`;
  if (linuxAppImage) files['linux-appimage'] = `${publicBase}/${tag}/${linuxAppImage}`;

  return {
    version,
    pub_date: pubDate ?? new Date().toISOString(),
    files,
  };
}

// 生成完整上传计划（数组顺序即上传顺序：可变清单永远最后）。
export function buildUploadPlan({
  assetsDir,
  webDir,
  screenshotsDir,
  version,
  tag,
  channel,
  publicBase = DEFAULT_PUBLIC_BASE,
  pubDate,
}) {
  const uploads = [];

  // 1. 版本资产（跳过更新清单本身，它按可变对象单独处理）
  const assetNames = readdirSync(assetsDir).filter((name) => name !== 'latest.json');
  for (const name of assetNames) {
    uploads.push({
      localPath: join(assetsDir, name),
      cosKey: `mykvm/${tag}/${name}`,
      cacheControl: IMMUTABLE_CACHE,
      contentType: contentTypeFor(name),
    });
  }

  // 2. 宣传页与截图（幂等同步，页面内容变更时自然生效）
  for (const name of readdirSync(webDir)) {
    const localPath = join(webDir, name);
    if (!statSync(localPath).isFile()) continue;
    uploads.push({
      localPath,
      cosKey: `mykvm/${name}`,
      cacheControl: NO_CACHE,
      contentType: contentTypeFor(name),
    });
  }
  for (const name of readdirSync(screenshotsDir)) {
    const localPath = join(screenshotsDir, name);
    if (!statSync(localPath).isFile()) continue;
    uploads.push({
      localPath,
      cosKey: `mykvm/assets/${name}`,
      cacheControl: SHORT_CACHE,
      contentType: contentTypeFor(name),
    });
  }

  let downloads = null;
  if (channel === 'stable') {
    // 3. 下载清单（网页读取）
    downloads = buildDownloadsJson({
      assetNames: readdirSync(assetsDir),
      version,
      tag,
      publicBase,
      pubDate,
    });
    uploads.push({
      cosKey: 'mykvm/downloads.json',
      cacheControl: NO_CACHE,
      contentType: CONTENT_TYPES['.json'],
      content: `${JSON.stringify(downloads, null, 2)}\n`,
    });
    // 4. 更新清单最后上传：中途失败不会出现指向缺失文件的清单
    uploads.push({
      localPath: join(assetsDir, 'latest.json'),
      cosKey: 'mykvm/latest.json',
      cacheControl: NO_CACHE,
      contentType: CONTENT_TYPES['.json'],
    });
  } else {
    uploads.push({
      localPath: join(assetsDir, 'latest.json'),
      cosKey: 'mykvm/beta/latest.json',
      cacheControl: NO_CACHE,
      contentType: CONTENT_TYPES['.json'],
    });
  }

  return { uploads, downloads };
}

function putObject({ bucket, region, cosKey, item, secretId, secretKey }) {
  return new Promise((resolve, reject) => {
    const host = `${bucket}.cos.${region}.myqcloud.com`;
    const uri = `/${bucket}/${cosKey}`;
    const now = Math.floor(Date.now() / 1000);
    const headers = {
      host,
      'content-type': item.contentType,
      'cache-control': item.cacheControl,
    };
    const body = item.content ?? null;
    if (body !== null) {
      headers['content-length'] = Buffer.byteLength(body);
    } else {
      headers['content-length'] = statSync(item.localPath).size;
    }
    headers.authorization = cosAuthorization({
      method: 'put',
      uri,
      query: {},
      headers,
      secretId,
      secretKey,
      startTimestamp: now - 60,
      endTimestamp: now + 3600,
    });

    const req = httpsRequest(
      { host, path: uri, method: 'PUT', headers },
      (res) => {
        const chunks = [];
        res.on('data', (chunk) => chunks.push(chunk));
        res.on('end', () => {
          const text = Buffer.concat(chunks).toString('utf8');
          if (res.statusCode >= 200 && res.statusCode < 300) {
            resolve();
          } else {
            reject(new Error(`PUT ${cosKey} -> HTTP ${res.statusCode}: ${text}`));
          }
        });
      },
    );
    req.on('error', reject);
    if (body !== null) {
      req.end(body);
    } else {
      createReadStream(item.localPath).pipe(req);
    }
  });
}

function parseArgs(argv) {
  const args = {};
  for (let i = 0; i < argv.length; i += 1) {
    const key = argv[i];
    if (!key.startsWith('--')) continue;
    const name = key.slice(2);
    if (name === 'dry-run') {
      args.dryRun = true;
      continue;
    }
    args[name] = argv[i + 1];
    i += 1;
  }
  return args;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  for (const required of ['assets', 'web', 'screenshots', 'version', 'tag']) {
    if (!args[required]) {
      console.error(`缺少参数 --${required}`);
      process.exit(2);
    }
  }
  const channel = args.channel ?? 'stable';
  if (channel !== 'stable' && channel !== 'beta') {
    console.error(`--channel 只支持 stable/beta，收到 ${channel}`);
    process.exit(2);
  }

  const plan = buildUploadPlan({
    assetsDir: args.assets,
    webDir: args.web,
    screenshotsDir: args.screenshots,
    version: args.version,
    tag: args.tag,
    channel,
    publicBase: args['public-base'] ?? process.env.PUBLIC_BASE ?? DEFAULT_PUBLIC_BASE,
  });

  if (args.dryRun) {
    const printable = plan.uploads.map(({ localPath, ...rest }) => ({
      ...rest,
      ...(localPath ? { localPath } : {}),
    }));
    console.log(JSON.stringify({ uploads: printable, downloads: plan.downloads }, null, 2));
    return;
  }

  const secretId = process.env.TENCENT_SECRET_ID;
  const secretKey = process.env.TENCENT_SECRET_KEY;
  if (!secretId || !secretKey) {
    console.error('需要环境变量 TENCENT_SECRET_ID 与 TENCENT_SECRET_KEY（或改用 --dry-run）');
    process.exit(2);
  }
  const bucket = process.env.COS_BUCKET ?? DEFAULT_BUCKET;
  const region = process.env.COS_REGION ?? DEFAULT_REGION;

  for (const item of plan.uploads) {
    const label = item.cosKey;
    process.stdout.write(`上传 ${label} ... `);
    await putObject({ bucket, region, cosKey: label, item, secretId, secretKey });
    console.log('ok');
  }
  console.log(`完成：${plan.uploads.length} 个对象 → cos://${bucket}/mykvm/`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  main().catch((error) => {
    console.error(error.message ?? error);
    process.exit(1);
  });
}
