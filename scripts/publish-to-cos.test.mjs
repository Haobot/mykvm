// scripts/publish-to-cos 的单元测试。
// 覆盖：上传计划布局/缓存头/顺序、downloads.json 生成、COS 请求签名。
import { test } from 'node:test';
import assert from 'node:assert/strict';
import { mkdtempSync, mkdirSync, writeFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import path from 'node:path';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

import {
  IMMUTABLE_CACHE,
  NO_CACHE,
  buildDownloadsJson,
  buildUploadPlan,
  cosAuthorization,
  hmacSha1Hex,
} from './publish-to-cos.mjs';

const SCRIPT = fileURLToPath(new URL('./publish-to-cos.mjs', import.meta.url));
const PUBLIC_BASE = 'https://dl.davenger.cloud/mykvm';

// 构造一个假的产物目录：安装包 + 签名 + 更新清单。
function makeAssetsDir() {
  const dir = mkdtempSync(path.join(tmpdir(), 'mykvm-assets-'));
  writeFileSync(path.join(dir, 'latest.json'), '{"version":"0.1.2"}');
  writeFileSync(path.join(dir, 'mykvm_0.1.2_x64-setup.exe'), 'exe');
  writeFileSync(path.join(dir, 'mykvm-windows-x64-setup.exe'), 'exe');
  writeFileSync(path.join(dir, 'mykvm-windows-x64-setup.exe.sig'), 'sig');
  writeFileSync(path.join(dir, 'MyKVM_0.1.2_universal.dmg'), 'dmg');
  writeFileSync(path.join(dir, 'mykvm.app.tar.gz'), 'targz');
  writeFileSync(path.join(dir, 'mykvm.app.tar.gz.sig'), 'sig');
  return dir;
}

function makeWebDir() {
  const dir = mkdtempSync(path.join(tmpdir(), 'mykvm-web-'));
  writeFileSync(path.join(dir, 'index.html'), '<html></html>');
  return dir;
}

function makeScreenshotsDir() {
  const dir = mkdtempSync(path.join(tmpdir(), 'mykvm-shots-'));
  writeFileSync(path.join(dir, 'tour.gif'), 'gif');
  writeFileSync(path.join(dir, 'layout.png'), 'png');
  return dir;
}

test('stable 上传计划：版本资产不可变、清单可变、latest.json 最后上传', () => {
  const assetsDir = makeAssetsDir();
  const webDir = makeWebDir();
  const screenshotsDir = makeScreenshotsDir();
  try {
    const plan = buildUploadPlan({
      assetsDir,
      webDir,
      screenshotsDir,
      version: '0.1.2',
      tag: 'v0.1.2',
      channel: 'stable',
      publicBase: PUBLIC_BASE,
    });

    const keys = plan.uploads.map((item) => item.cosKey);
    // 版本化资产目录
    for (const name of [
      'mykvm_0.1.2_x64-setup.exe',
      'mykvm-windows-x64-setup.exe',
      'mykvm-windows-x64-setup.exe.sig',
      'MyKVM_0.1.2_universal.dmg',
      'mykvm.app.tar.gz',
      'mykvm.app.tar.gz.sig',
    ]) {
      assert.ok(keys.includes(`mykvm/v0.1.2/${name}`), `缺少 ${name}`);
    }
    // 页面与清单
    assert.ok(keys.includes('mykvm/index.html'));
    assert.ok(keys.includes('mykvm/assets/tour.gif'));
    assert.ok(keys.includes('mykvm/assets/layout.png'));
    assert.ok(keys.includes('mykvm/downloads.json'));
    assert.ok(keys.includes('mykvm/latest.json'));

    // 缓存头：版本目录不可变，清单不缓存
    const byKey = new Map(plan.uploads.map((item) => [item.cosKey, item]));
    assert.equal(byKey.get('mykvm/v0.1.2/MyKVM_0.1.2_universal.dmg').cacheControl, IMMUTABLE_CACHE);
    assert.equal(byKey.get('mykvm/latest.json').cacheControl, NO_CACHE);
    assert.equal(byKey.get('mykvm/downloads.json').cacheControl, NO_CACHE);

    // 可变清单必须最后上传：先资产后面，中途失败不会让清单指向缺失文件
    assert.equal(keys[keys.length - 1], 'mykvm/latest.json');
  } finally {
    rmSync(assetsDir, { recursive: true, force: true });
    rmSync(webDir, { recursive: true, force: true });
    rmSync(screenshotsDir, { recursive: true, force: true });
  }
});

test('beta 通道只更新 beta 清单，不动 stable 清单与下载清单', () => {
  const assetsDir = makeAssetsDir();
  const webDir = makeWebDir();
  const screenshotsDir = makeScreenshotsDir();
  try {
    const plan = buildUploadPlan({
      assetsDir,
      webDir,
      screenshotsDir,
      version: '0.1.3-beta.7',
      tag: 'v0.1.3-beta.7',
      channel: 'beta',
      publicBase: PUBLIC_BASE,
    });

    const keys = plan.uploads.map((item) => item.cosKey);
    assert.ok(keys.includes('mykvm/beta/latest.json'));
    assert.ok(keys.includes('mykvm/v0.1.3-beta.7/mykvm_0.1.2_x64-setup.exe'));
    assert.ok(!keys.includes('mykvm/latest.json'), 'beta 不应覆盖 stable 清单');
    assert.ok(!keys.includes('mykvm/downloads.json'), 'beta 不应覆盖下载清单');
    assert.equal(keys[keys.length - 1], 'mykvm/beta/latest.json');
  } finally {
    rmSync(assetsDir, { recursive: true, force: true });
    rmSync(webDir, { recursive: true, force: true });
    rmSync(screenshotsDir, { recursive: true, force: true });
  }
});

test('downloads.json 指向 dl 直链并区分 Windows 安装版与 macOS dmg', () => {
  const downloads = buildDownloadsJson({
    assetNames: [
      'mykvm_0.1.2_x64-setup.exe',
      'mykvm-windows-x64-setup.exe',
      'MyKVM_0.1.2_universal.dmg',
      'mykvm.app.tar.gz',
    ],
    version: '0.1.2',
    tag: 'v0.1.2',
    publicBase: PUBLIC_BASE,
    pubDate: '2026-11-26T00:00:00.000Z',
  });

  assert.equal(downloads.version, '0.1.2');
  assert.equal(downloads.pub_date, '2026-11-26T00:00:00.000Z');
  assert.equal(
    downloads.files['windows-setup'],
    `${PUBLIC_BASE}/v0.1.2/mykvm-windows-x64-setup.exe`,
  );
  assert.equal(
    downloads.files['macos-dmg'],
    `${PUBLIC_BASE}/v0.1.2/MyKVM_0.1.2_universal.dmg`,
  );
  // updater 用的 tar.gz 不是给用户下载的入口
  assert.ok(!Object.values(downloads.files).some((url) => url.endsWith('.tar.gz')));
});

test('COS 签名：HMAC-SHA1 符合 RFC 2202 向量', () => {
  // RFC 2202 test case 2
  assert.equal(
    hmacSha1Hex('Jefe', 'what do ya want for nothing?'),
    'effcdf6ae5eb2fa2d27416d5f184df9c259a7c79',
  );
});

test('COS 签名：Authorization 字段完整、稳定、随 KeyTime 变化', () => {
  const first = cosAuthorization({
    method: 'put',
    uri: '/voicestick-dl-1329978361/mykvm/latest.json',
    query: {},
    headers: { host: 'voicestick-dl-1329978361.cos.ap-shanghai.myqcloud.com' },
    secretId: 'AKIDEXAMPLE',
    secretKey: 'SECRETKEYEXAMPLE',
    startTimestamp: 1764100000,
    endTimestamp: 1764100600,
  });

  assert.match(first, /^q-sign-algorithm=sha1&q-ak=AKIDEXAMPLE/);
  assert.match(first, /q-sign-time=1764100000;1764100600/);
  assert.match(first, /q-key-time=1764100000;1764100600/);
  assert.match(first, /q-header-list=host/);
  const signature = /q-signature=([0-9a-f]{40})/.exec(first);
  assert.ok(signature, 'q-signature 应为 40 位小写 hex');

  const again = cosAuthorization({
    method: 'put',
    uri: '/voicestick-dl-1329978361/mykvm/latest.json',
    query: {},
    headers: { host: 'voicestick-dl-1329978361.cos.ap-shanghai.myqcloud.com' },
    secretId: 'AKIDEXAMPLE',
    secretKey: 'SECRETKEYEXAMPLE',
    startTimestamp: 1764100000,
    endTimestamp: 1764100600,
  });
  assert.equal(first, again, '相同输入应产生相同签名');

  const later = cosAuthorization({
    method: 'put',
    uri: '/voicestick-dl-1329978361/mykvm/latest.json',
    query: {},
    headers: { host: 'voicestick-dl-1329978361.cos.ap-shanghai.myqcloud.com' },
    secretId: 'AKIDEXAMPLE',
    secretKey: 'SECRETKEYEXAMPLE',
    startTimestamp: 1764100600,
    endTimestamp: 1764101200,
  });
  assert.notEqual(first, later, 'KeyTime 变化后签名应变化');
});

test('CLI --dry-run 输出上传清单且不需要云密钥', () => {
  const assetsDir = makeAssetsDir();
  const webDir = makeWebDir();
  const screenshotsDir = makeScreenshotsDir();
  try {
    const result = spawnSync(
      process.execPath,
      [
        SCRIPT,
        '--assets', assetsDir,
        '--web', webDir,
        '--screenshots', screenshotsDir,
        '--version', '0.1.2',
        '--tag', 'v0.1.2',
        '--channel', 'stable',
        '--dry-run',
      ],
      { encoding: 'utf8' },
    );
    assert.equal(result.status, 0, result.stderr);
    const output = JSON.parse(result.stdout);
    assert.equal(output.uploads[output.uploads.length - 1].cosKey, 'mykvm/latest.json');
    assert.equal(output.downloads.version, '0.1.2');
  } finally {
    rmSync(assetsDir, { recursive: true, force: true });
    rmSync(webDir, { recursive: true, force: true });
    rmSync(screenshotsDir, { recursive: true, force: true });
  }
});
