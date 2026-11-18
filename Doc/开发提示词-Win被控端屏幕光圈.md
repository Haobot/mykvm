# 开发提示词 — Windows 被控端「屏幕光圈」构建与验收

> 场景：Mac 控制端在「显示器布局」画布点选 **GT37（Windows 被控端）** 的屏幕磁贴时，
> 被控端应在对应物理显示器上闪一圈蓝色光圈，帮助确认"这块磁贴 = 那块真实屏幕"。
> 服务端（Mac）与被控端（Windows）共用本仓库同一份代码；**被控端需要升级到包含
> `fa5f6fc`（FlashScreen 事件）及之后提交的构建**才能响应光圈请求。

## 1. 已实现内容（代码已在分支上，无需重复开发）

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
  安装的执行器（捕获 AppHandle）→ `run_screen_flash_window` 在对应显示器上创建
  透明置顶点击穿透窗口（加载 `public/screen-indicator.html` 纯 CSS 光圈），约 1.4s 自关。
- Windows 特有注意：
  - FlashScreen **不下发**给锁屏助手服务（`dispatch_input_command` 在 helper 路由之前
    分流）；助手侧 `inject_command_without_tracking` / `worker_can_inject` 已补穷尽匹配。
  - Windows 无 tao 的 Cocoa 翻转问题，`builder.position`（逻辑坐标）直接可用。

## 2. Windows 端构建（二选一）

### 方式 A：GitHub dev-build 工作流（推荐，GT37 上无需装开发环境）

1. 推送分支：`git push origin fix/layout-crossing-overlap`。
2. 触发：`gh workflow run dev-build --ref fix/layout-crossing-overlap`（或网页 Actions →
   Dev Build → Run workflow 选分支）。
3. 等待构建完成，从该次 run 的 Artifacts 下载 `mykvm-windows-x64-setup`。
4. 在 GT37 上运行安装包覆盖安装（先退出旧版 MyKVM）。

### 方式 B：GT37 本地构建

```powershell
# 前置：Node 18+ / npm、Rust stable（MSVC toolchain）、WebView2 运行时
git fetch origin
git checkout fix/layout-crossing-overlap
npm install
npm run tauri -- build        # 产物在 src-tauri/target/release/bundle/nsis/
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
5. 点 Mac 本机磁贴 → Mac 本机屏闪光圈（不受本次改动影响）。
6. 回归：鼠标穿越到 GT37 再返回、键盘输入、剪贴板同步均应正常（确认 FlashScreen
   没有干扰输入管线）。
7. 日志核对：GT37 端 `mykvm.log` 若出现输入解码失败相关计数，且时间点恰为 Mac 点击
   远端磁贴的时刻，说明对端还是旧版本（未含 FlashScreen 解码）。

## 4. 已知边界

- 对端未升级时点击远端磁贴**静默无效果**（请求被旧客户端丢弃），属预期兼容行为；
  旧版可继续正常穿越，仅光圈提示缺失。
- GT37 若有多块屏幕，光圈落在哪块由两端 screen id 映射决定：服务端画布 id 为
  `peer-<host>-<ip>-local-display-N`，wire 上剥成被控端自己的 `local-display-N`，
  被控端按同序枚举映射到物理显示器。若发现光圈落在相邻的错位屏幕上，收集
  被控端 `detect` 相关日志另行排查（枚举顺序跨重启可能变化，与 Mac 端
  `restore_local_screen_layout` 的 id 保持机制是同一类问题）。
- 本轮 Mac 端外接屏光圈仍在排查中（`screen indicator` 日志已带 frame/level 回读）。
