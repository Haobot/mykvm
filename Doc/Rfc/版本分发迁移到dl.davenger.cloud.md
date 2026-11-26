# 版本分发与宣传页迁移到 dl.davenger.cloud — 实施方案

- 状态：待评审
- 日期：2026-11-26
- 范围：更新检查、安装包分发、网页宣传、发布流水线

## 1. 背景与目标

当前版本检查/分发链路残留大量源头仓库（`github.com/XxMinor/mykvm`）引用，且更新清单与安装包只发布在 GitHub Releases 上——国内网络访问 GitHub 不稳定（应用内已有"无法获取更新清单，请检查网络或代理"的报错文案佐证）。

目标：

1. **版本检查、下载、宣传统一收敛到 `https://dl.davenger.cloud/mykvm/`**（腾讯云 COS 静态网站，HTTPS 已验证可用）。
2. **彻底清理源头仓库 XxMinor 引用**，源码仓库链接指向本仓库 `github.com/Haobot/mykvm`。
3. **GitHub Releases 保留为备用更新源**（双写），保证已安装的旧版本客户端不断更。

## 2. 现状盘点（需改动的引用点）

| 文件 | 现状 | 问题 |
| --- | --- | --- |
| `src-tauri/tauri.conf.json` updater endpoints（stable） | `github.com/Haobot/mykvm/releases/latest/download/latest.json` | 仅 GitHub 一源，国内不稳 |
| `.github/workflows/release.yml` → Apply release version（beta） | 硬编码 `github.com/XxMinor/mykvm/releases/download/beta/latest.json` | 源头仓库（遗留 bug） |
| `.github/workflows/release.yml` → Generate latest.json | 下载 URL = GitHub Releases 直链 | 走 GitHub |
| `src/constants.ts` | `REPOSITORY_URL/RELEASES_URL` = `github.com/XxMinor/mykvm` | 源头仓库 |
| `src-tauri/src/lib.rs:83-84` | 同上两个常量（`open_repository_url`/`open_releases_url`） | 源头仓库 |
| `src/i18n.ts` | "已打开 GitHub Releases 下载页"、"打开 Releases"、更新报错文案 | 文案指向 GitHub |
| `package.json` repository | `github.com/XxMinor/mykvm.git` | 源头仓库 |
| `README.md` / `README.zh-CN.md` | 徽章、下载链接、issue 链接 | 见 §5 PR1 备注 |
| `.github/ISSUE_TEMPLATE/*.yml` | Discussions/README 链接 | 源头仓库 |

更新机制本体：前端 `desktopApi.ts` 的 `checkForAppUpdate()` 与后端 `run_requested_update()`（`lib.rs:11599`）均走 `tauri-plugin-updater`，端点由 `tauri.conf.json` 编译进应用；minisign 签名公钥内嵌于 `plugins.updater.pubkey`。**签名体系与密钥不变，迁移只改分发位置。**

## 3. 已确认决策

1. **更新源**：dl 主 + GitHub 备（updater endpoints 数组 dl 在前、GitHub 作 fallback；CI 双写，latest.json 内安装包 URL 统一指向 dl）。
2. **网页形态**：轻量单页宣传 + 下载页，放仓库 `web/` 目录，随发版同步上传。
3. **发布通道**：CI 自动上传 COS（腾讯云密钥配到 GitHub Secrets）；上传逻辑做成脚本，CI 与本地 bootstrap/应急共用同一实现。

## 4. 目标架构

### 4.1 COS 目录与 URL 布局

`cos://voicestick-dl-1329978361/mykvm/` → `https://dl.davenger.cloud/mykvm/`：

```text
mykvm/
├── index.html            # 宣传+下载单页（内容变更时上传）
├── assets/               # 页面静态资源（截图、图标）
├── downloads.json        # 安装包直链清单（页面渲染下载按钮用，每次发版生成）
├── latest.json           # stable 更新清单（Cache-Control: no-cache）
├── beta/
│   └── latest.json       # beta 更新清单
└── v{tag}/               # 每版本全部产物（含 .sig），tag 如 v0.1.2 / v0.1.3-beta.42
```

- `v{tag}/` 目录不可变（长缓存 `public, max-age=31536000, immutable`），历史版本全部保留（回滚能力 + 旧清单可继续下载）。
- `latest.json` / `downloads.json` 是仅有的可变对象（短缓存/no-cache）。

### 4.2 更新链路

- `tauri.conf.json` stable endpoints：
  ```json
  "endpoints": [
    "https://dl.davenger.cloud/mykvm/latest.json",
    "https://github.com/Haobot/mykvm/releases/latest/download/latest.json"
  ]
  ```
- beta（release.yml 写入）：
  `https://dl.davenger.cloud/mykvm/beta/latest.json` + `https://github.com/Haobot/mykvm/releases/download/beta/latest.json`。
- latest.json 的 `platforms.*.url` 全部指向 `https://dl.davenger.cloud/mykvm/{tag}/...`。同一份清单双写到 GitHub release 资产——**旧版本客户端（endpoint 只有 GitHub）从 GitHub 拿清单、从 dl 下载安装包，无感迁移**。
- 老客户端清单里的下载地址本来就是清单生成时写死的 URL，因此存量版本回填（§6）后旧客户端也立即走 dl。

## 5. 改动清单

### PR1 清理源头仓库引用（纯改常量/文案，先行合入）

- `src/constants.ts`、`src-tauri/src/lib.rs:83-84`：
  - `REPOSITORY_URL` → `https://github.com/Haobot/mykvm`
  - `RELEASES_URL`（建议更名 `DOWNLOAD_PAGE_URL`）→ `https://dl.davenger.cloud/mykvm/`
- `src/i18n.ts` 中英两份文案："打开 Releases"→"打开下载页"；"已打开 GitHub Releases 下载页"→"已打开下载页"；更新清单获取失败文案中的 "Releases"→"下载页"。
- `package.json` repository → Haobot。
- README / README.zh-CN：下载与徽章链接 → `dl.davenger.cloud/mykvm`（下载）与 Haobot（仓库/Stars/Forks）。**注意**：正文里 `#11`、`#31` issue 链接是源头仓库的既有 issue，保留指向 XxMinor 的原链接（外部引用，改到 Haobot 会 404）。
- `.github/ISSUE_TEMPLATE/config.yml`、`setup_help.yml`：Discussions/README 链接 → Haobot。
- 测试：`npm test` + `cargo test --lib` 回归；如 i18n/常量有既有测试则同步更新断言。

### PR2 宣传单页 + 发布脚本

- `web/index.html`：单文件静态页（内联 CSS + 少量 JS），内容大纲见 §7。
- 页面 JS：fetch 同源 `downloads.json` 渲染下载按钮（Windows 安装版 / macOS dmg）与当前版本号；fetch 失败时降级显示 GitHub Releases（Haobot）链接。
- `downloads.json` 由发布脚本生成：
  ```json
  {
    "version": "0.1.2",
    "pub_date": "…",
    "files": {
      "windows-setup": "https://dl.davenger.cloud/mykvm/v0.1.2/mykvm-windows-x64-setup.exe",
      "macos-dmg": "https://dl.davenger.cloud/mykvm/v0.1.2/…_universal.dmg"
    }
  }
  ```
  （macOS 下载按钮必须指向 `.dmg` 而非 updater 用的 `.app.tar.gz`，故不能直接复用 latest.json 的 platforms。）
- `scripts/publish-to-cos`（Node 或 PowerShell，实现形式不限）：输入 = 产物目录 + 版本号，职责 = 生成 downloads.json、按 §4.1 布局上传、打缓存头。提供 `--dry-run` 输出将上传清单供测试断言。CI 与本地 bootstrap 共用。
- 素材：截图从 `docs/screenshots/` 上传到 `mykvm/assets/`（不在 web/ 下复制一份）。
- 测试：dry-run 断言上传清单/缓存头正确；页面本地起静态服务人工验收。

### PR3 CI 接入 COS + updater 端点切换

- `tauri.conf.json` endpoints 按 §4.2 切换（GitHub fallback 在数组第二位，切换先行合入也安全）。
- `release.yml`：
  1. "Apply release version" 中 beta endpoints 改为 §4.2 的双源。
  2. "Generate latest.json" 的 `baseUrl` 改为 `https://dl.davenger.cloud/mykvm/{tag}`。
  3. 新增 `publish-cos` job（needs: assemble-latest-json）：调用 `scripts/publish-to-cos` 上传 `v{tag}/` 全部资产、latest.json（stable→`mykvm/latest.json`，beta→`mykvm/beta/latest.json`）、web 页面与 assets。
  4. **"Publish release"（draft 翻正）移到 publish-cos 成功之后**——保证 GitHub 的 "latest" 可见时 dl 已就绪，且上传失败时发布可整体重跑。
  5. assemble job 通过 workflow artifact 把 `dl/` 目录传给 publish-cos，避免重复下载。
- 需要配置的 GitHub Secrets：`TENCENT_SECRET_ID`、`TENCENT_SECRET_KEY`。
- 测试：`workflow_dispatch` 发一版（或走 beta 通道）验证全链路，见 §10。

### 存量回填（一次性 bootstrap，PR3 合入后、下次发版前）

1. 下载当前最新 release 全部资产 → 传 `mykvm/v{tag}/`。
2. 用该 tag 资产重新生成 url 指向 dl 的 latest.json → 传 `mykvm/latest.json` 并 clobber 到 GitHub release 资产。
3. 效果：已装客户端下次检查更新即从 dl 下载；旧版清单 URL 指向 GitHub 的历史版本仍可经 GitHub 资产继续下载。

## 6. 发布流水线（目标时序）

```text
prepare（版本号/tag/draft release）
  → build ×3（macOS/Windows/Linux，上传 GitHub draft 资产 + .sig）
  → assemble-latest-json（生成 latest.json → GitHub 资产 + workflow artifact）
  → publish-cos（v{tag}/ + latest.json + downloads.json + 页面）
  → publish release（draft 翻正，GitHub "latest" 前进）
```

上传顺序保证：dl 先就绪，GitHub 后可见。COS 上传失败 → release 保持 draft，修复后整条流水线重跑（资产覆盖上传幂等）。

## 7. 网页内容大纲（web/index.html）

1. Hero：产品名 + 一句话（"一套键盘、一只鼠标、一份剪贴板——同一局域网内 Mac/Windows 共享"）。
2. 核心特性：跨屏切换（推出屏幕边缘）、剪贴板文本/图片双向、文件拖拽、免 KVM 硬件/插线、局域网直连。
3. 演示与截图：`tour.gif` + layout/devices/settings 三图。
4. 下载区：Windows 安装版 / macOS dmg 按钮（downloads.json 渲染）+ 版本号；macOS 自签名首次打开提示（右键→打开）；Linux 按钮从简或注明键鼠未实现。
5. 快速开始：README 的 5 步精简版。
6. 权限说明摘要（macOS 辅助功能/输入监控）。
7. 页脚：开源仓库链接（Haobot/mykvm）、更新日志（CHANGELOG.md 链接或内嵌最近几版）。

## 8. 迁移兼容性

| 客户端 | 行为 |
| --- | --- |
| 现有 stable（endpoint=GitHub） | 从 GitHub 拿新 latest.json（URL 已指向 dl）→ dl 下载，无感迁移 |
| 现有 beta（endpoint=XxMinor！） | 走 PR1/PR3 后的 beta 双源；XxMinor 端点彻底废弃 |
| 新版本（endpoint=dl 主 + GitHub 备） | 正常走 dl；dl 故障时自动 fallback GitHub 清单（清单 URL 仍指 dl，此时需依赖 dl 恢复——可接受，因 GitHub 清单与 dl 清单同源同内容） |

注意：双写期间 GitHub 清单里的安装包 URL 指向 dl，意味着"GitHub fallback"只覆盖**清单获取**失败场景，不覆盖 dl 下载失败场景。若后续要求下载也有双源，可让 GitHub 清单使用 GitHub 资产 URL（两份清单分别生成）——本期不做，记录为可选演进。

## 9. 安全

- 腾讯云子账号密钥，最小权限：仅 `cos:PutObject` 作用于 `voicestick-dl-1329978361` 桶 `mykvm/*` 前缀（覆盖 latest.json 是 PutObject，无需 Delete）；不授桶管理/删除权限。密钥只存 GitHub Secrets。
- 更新包签名不变：`TAURI_SIGNING_PRIVATE_KEY` 仍只在 GitHub Secrets，dl 只分发已签名产物 + `.sig`；客户端 minisign 公钥校验与分发域名无关。
- HTTPS：dl.davenger.cloud 现状已可用（COS 静态网站 + 自定义域名证书）。

## 10. 验证清单

1. `cd src-tauri && cargo test --lib` 全绿；`npm test`、`npm run lint` 全绿；`cargo fmt` 只检查改动文件。
2. `workflow_dispatch` 实发一版后：
   - `https://dl.davenger.cloud/mykvm/latest.json` 200，`platforms.*.url` 均指向 dl，且直链可下载（含 .sig）；
   - `mykvm/{tag}/` 资产齐全；`downloads.json` 的 dmg/setup 链接有效；
   - GitHub release 双写存在、draft 翻正在 COS 上传之后（看流水线时序）；
   - `https://dl.davenger.cloud/mykvm/` 页面下载按钮/版本号渲染正确；
   - 连续两次 GET latest.json 验证 Cache-Control 头（no-cache）生效。
3. 真机：
   - **旧版本客户端**（改动前装的）能收到更新提示并完成更新（观察下载走 dl）；
   - 新版本"检查更新"正常（dl 主源）；断网 dl 时 fallback 不报错前置清单；
   - "打开下载页"→ dl 页面；"打开仓库"→ Haobot；
   - beta 通道同理（如继续使用 bate 分支）。

## 11. 风险与对策

| 风险 | 对策 |
| --- | --- |
| 云密钥泄露 | 子账号最小权限 + 只写不删 + 定期轮换 |
| COS 上传中断导致 latest.json 先于资产可见 | publish-cos 先传 `v{tag}/` 资产并校验清单完整，最后才覆盖 latest.json；draft 翻正置于其后 |
| 误删/误覆盖历史版本 | CI 不含 Delete 权限；`v{tag}/` 不可变；仅 latest.json/downloads.json 可变 |
| 页面与 app 脱节（下载链接失效） | 页面读 downloads.json 而非硬编码版本；fetch 失败降级 GitHub 链接 |
| 源头仓库 issue 链接（#11/#31）改坏 | 保留指向 XxMinor 的原 issue 链接不动 |

## 12. 实施顺序与提交约定

1. PR1（`fix(分发):` 或 `refactor(分发):`）→ 2. PR2（`feat(分发):`）→ 3. PR3（`feat(ci):`）→ 4. bootstrap 回填 + 实发验证。
- 每 PR 独立过测试；遵循 Conventional Commits 中文提交信息。
