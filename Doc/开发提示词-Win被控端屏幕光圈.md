# 开发提示词 — Windows 被控端「屏幕光圈」构建与验收

> 场景：Mac 控制端在「显示器布局」画布点选 **GT37（Windows 被控端）** 的屏幕磁贴时，
> 被控端应在对应物理显示器上闪一圈蓝色光圈，帮助确认"这块磁贴 = 那块真实屏幕"。
> 服务端（Mac）与被控端（Windows）共用本仓库同一份代码；**被控端需要升级到包含
> `fa5f6fc`（FlashScreen 事件）及之后提交的构建**才能响应光圈请求。
> **推荐直接装最新分支构建（原生光圈）**：Mac 真机验证发现透明 WebView 光圈窗跨显示器
> 移动后 WKWebView 不重绘（外接屏上完全不可见），已换原生窗口实现；macOS 27 上
> Tauri 进程内创建的跨显示器 NSWindow 无法进入当前 Space，Mac 端闪光现由
> `mykvm --flash` 独立子进程执行（`flash-core` crate），已真机验收。Windows 被控端
> 仍为进程内分层窗实现（halo win32 同构，实测可见），不受影响。

## 1. 已实现内容（已合入 `main`，无需重复开发）

- 协议：`shared_input.rs` 新增 `InputEvent::FlashScreen { screen_id }`（wire，camelCase
  `flashScreen`）与 `InputCommand::FlashScreen { x, y, width, height }`。
  旧客户端解码未知 tag 失败按坏包丢弃 → **新旧版本混跑不破坏控制链路**，无需协议版本升级。
- 服务端（Mac）：画布点击远端磁贴 → `flash_peer_screen` 命令 →
  `input::send_flash_screen_to_peer` 在 `build_input_targets` 里找到配对该屏的 target，
  复用 `send_packet` 发送（凭据刷新、对端端点管理全部继承）。
- 被控端（Windows）：`handle_input_datagram` 收包鉴权后，
  `input_event_to_command` 把 wire screen_id 换算成自己 `native_layout` 的逻辑屏矩形 →
  `InputCommand::FlashScreen` → `dispatch_input_command` 顶部分流给
  `dispatch_screen_flash` → lib.rs 在 `.setup` 里用 `install_screen_flash_dispatcher`
  安装的执行器 → `screen_flash::flash_logical_rect`（`src-tauri/src/screen_flash.rs`）：
  `WS_EX_LAYERED` 分层窗 + GDI DIB 像素光圈（halo win32 同构路径），按
  `GetDpiForMonitor` 匹配逻辑矩形选屏，蓝色圆角描边 0.45s×3 脉冲，约 1.4s 自毁。
- Windows 特有注意：
  - FlashScreen **不下发**给锁屏助手服务（`dispatch_input_command` 在 helper 路由之前
    分流）；助手侧 `inject_command_without_tracking` / `worker_can_inject` 已补穷尽匹配。
  - 光圈窗自带消息循环线程，不占输入数据链线程；窗口类 `MyKVMScreenFlash` 复用注册；
    `windows-sys` 已补 `Win32_UI_HiDpi` feature。

## 2. Windows 端构建（二选一）

### 方式 A：GitHub dev-build 工作流（推荐，GT37 上无需装开发环境）

> 功能已合入 `main`（原开发分支 `fix/layout-crossing-overlap`、
> `fix/native-screen-flash` 均已合并删除、不再存在），直接从 `main` 构建即可，
> 无需推送任何分支。

1. 触发：`gh workflow run dev-build --ref main`（或网页 Actions → Dev Build →
   Run workflow 选 `main`）。
2. 等待构建完成，从该次 run 的 Artifacts 下载 `mykvm-windows-x64-setup`。
3. 在 GT37 上运行安装包覆盖安装（先退出旧版 MyKVM）。

### 方式 B：GT37 本地构建

```powershell
# 前置：Node 18+ / npm、Rust stable（MSVC toolchain）、WebView2 运行时
# （2026-11 实测 GT37 尚未安装 Rust/MSVC 工具链，本地构建前需先补齐前置）
git fetch origin
git checkout main
npm install
npm run tauri -- build --ci -b nsis --config '{"bundle":{"createUpdaterArtifacts":false}}'
# ↑ dev 构建必须与 CI 同参关闭 updater 签名（本地无 TAURI_SIGNING_PRIVATE_KEY，
#   裸 `npm run tauri -- build` 会卡死在打包阶段）。产物在
#   src-tauri/target/release/bundle/nsis/
# 网络：tauri 首次打包需从 GitHub 下载 NSIS 工具链；VPN 走系统代理时
#   tauri 下载器不读 WinINET，需先 export http_proxy=<本地代理> https_proxy=<同>
# 安装 nsis 产物后启动
```

开发期快速验证可不起安装包：`npm run tauri dev`。

### 构建前置自检（在 Windows 上先跑，避免白等）

```powershell
cargo check --lib                       # 主库（FlashScreen 穷尽匹配已补齐，应无错误）
cargo check -p mykvm-input-helper       # 锁屏助手包
npm run build                           # tsc -b 全量严格检查 + vite
```

## 3. 验收步骤（GT37 + Mac 两端就位后）

1. 两端 MyKVM 均启动、配对正常（GT37 在 Mac 的设备列表里在线且可穿越）。
2. Mac 打开「显示器布局」画布，点击 **GT37 的屏幕磁贴**。
3. 预期：GT37 对应的物理显示器上出现蓝色圆角边框光圈，脉冲约 1.4s 后自动消失；
   窗口点击穿透，不影响 GT37 正常使用。
4. 连续点击两次 → 光圈叠开两次，各自独立消失，无报错。
5. 点 Mac 本机磁贴 → Mac 每块本机屏（含外接屏）都闪光圈（`494b2e8` 已修复外接屏
   不显示的问题）。
6. 回归：鼠标穿越到 GT37 再返回、键盘输入、剪贴板同步均应正常（确认 FlashScreen
   没有干扰输入管线）。
7. 日志核对：
   - 旧版判别：存量构建对解码失败的输入包是零日志静默丢弃
     （`decode_input_packet` 直接 `.ok()`），`mykvm.log` 不会留痕，只能靠
     现象判：点击远端磁贴无光圈、其余功能（穿越/输入/剪贴板）全部正常 →
     发送端比接收端新。含三 schema 全败后限流 warn
     （`dropped undecodable input datagram … from <addr>`）与计数器的**接收端**
     装上后，即可直接从日志判定混跑与来源地址；时间点恰为 Mac 点击远端磁贴的
     解码失败计数即说明对端还是旧版本（未含 FlashScreen 解码）。
   - Mac 端每次点选应有一条 `screen indicator covering display at cocoa ...`
     （原生路径的选屏日志）。

## 4. 已知边界

- 对端未升级时点击远端磁贴**静默无效果**（请求被旧客户端丢弃），属预期兼容行为；
  旧版可继续正常穿越，仅光圈提示缺失。
- GT37 若有多块屏幕，光圈落在哪块由两端 screen id 映射决定：服务端画布 id 为
  `peer-<host>-<ip>-local-display-N`，wire 上剥成被控端自己的 `local-display-N`，
  被控端按同序枚举映射到物理显示器。若发现光圈落在相邻的错位屏幕上，收集
  被控端 `detect` 相关日志另行排查（枚举顺序跨重启可能变化，与 Mac 端
  `restore_local_screen_layout` 的 id 保持机制是同一类问题）。
- Mac 端外接屏光圈已修复并验收（BUG-005，见 `Doc/Bug与需求记录.md`）。
