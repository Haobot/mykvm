# 托盘退出确认 + 修复无人值守顶班导致的光圈消失

## Context

两个问题同源：

1. **托盘退出后仍可被控**：GUI 退出是完整的，但 `mykvm-input-helper` 无人值守服务按设计接管端口继续接收键鼠。用户缺少一个显式的「退出并暂停被控」出口（今天只能去任务管理器杀服务，且服务有失败自动重启）。
2. **光圈消失**：换装新版本时，新实例的端口接管握手（单实例阻塞命名管道，`WaitNamedPipeW` 只等 2s）撞上旧实例退出瞬间 → `start_discovery` 在 [lib.rs:1080](C:\Dev\DVG\mykvm\src-tauri\src\lib.rs#L1080) 直接 Err，且：错误不落日志、无重试、`started` 照样置 true → GUI 整个会话网络瘫痪，helper 顶班掩盖问题（键鼠正常），只有光圈等 GUI 专属功能暴露。

已确认方案：暂停用 layout.json 持久标记（`headlessPaused`），**持续到下次打开主程序**（含重启电脑期间），GUI 启动接管时自动清除。

## Part A：托盘退出确认对话框

### 后端（src-tauri/src/lib.rs）

1. `LayoutState`（lib.rs:293）新增 `#[serde(default)] headless_paused: bool`；在 `merge_runtime_owned_layout_fields`（lib.rs:2857）把它列为后端持有字段（前端 save_layout 不得覆写）。
2. [headless_client.rs:216](C:\Dev\DVG\mykvm\src-tauri\src\headless_client.rs#L216) `headless_receive_enabled` 加 `&& !layout.headless_paused`，可见性提为 `pub(crate)`。
3. 新事件常量 `QUIT_CONFIRMATION_EVENT = "quit-confirmation-requested"`（放 lib.rs:132 旁）。
4. 托盘 `quit` 分支（lib.rs:5613）：改为调新函数 `handle_tray_quit(app)` —— 纯函数判定 `headless_takeover_would_continue(layout, service_running)`（= `service_running && headless_receive_enabled(layout)`，用 `layout_snapshot()` + `current_input_service_status().running` 喂入）：
   - true → `show_main_window_handle(app)` + `app.emit(QUIT_CONFIRMATION_EVENT, ())`
   - false → 维持 `request_app_quit(app)`
   - `InstanceEvent::Quit`（4903）与 `restart_as_admin` 保持直接退出（更新流程不能被弹窗卡住）。
5. 新命令并注册进 `generate_handler!`（lib.rs:5495）：
   - `quit_app` → `request_app_quit(&app)`
   - `quit_with_headless_pause` → 内存 layout 置 `headless_paused=true` + `write_layout_to_disk`（复用 lib.rs:5800 写盘函数；顺序保证在进程退出前落盘，helper 的 `LeaseReleased → start_receiver` 重读磁盘即可见）+ `log::info!` + `request_app_quit(&app)`
6. 清除标记：`start_runtime_inner`（lib.rs:3033）与 `.setup` 内联启动块（lib.rs:5268-5294）开头调 `AppRuntime::clear_headless_pause()`（检测到 true 才写盘+log info）。

### 前端（src/）

1. `src/desktopApi.ts`：`quitApp()`、`quitWithHeadlessPause()` 两个 invoke 包装（模式同 386-391 的 `hideMainWindow`）。
2. `src/App.tsx`：
   - 状态 `quitConfirmVisible` + `isQuitPausePending`；`listen(QUIT_CONFIRMATION_EVENT)` 置 true（模式同 588 的 runtime-state-changed 监听）。
   - `renderQuitConfirmation()` 完全照抄 `renderInputServicePrompt`（App.tsx:2600-2666）的 `pairing-modal-backdrop`/`pairing-modal`/`pairing-actions` 外壳：按钮「退出并暂停被控」（danger）/「直接退出」（primary）/「取消」；挂载点在 2980 旁；加入 `hasBlockingOverlay`（1023-1027）。
   - 暂停失败时 `setErrorMessage`（新错误文案），成功则进程退出无需善后。
3. `src/i18n.ts`：cn/en 各加 `quitDialog` 小节（title/body/pauseAndExit/directExit/pausing），复用 `ui.common.cancel`；`as const` + `AppText` 保证双语形状受 TS 约束。

## Part B：修光圈消失根因（接管竞态）

### 1. 幂等检查前置 + 握手重试（lib.rs `start_discovery`）

- 把「`discovery_stop.is_some()` 即返回 Ok」的检查改为**短锁先行**（当前租约获取在幂等检查之前，健康会话遇到管道瞬时繁忙也会 Err，[lib.rs:1079-1089](C:\Dev\DVG\mykvm\src-tauri\src\lib.rs#L1079)）。
- Windows 租约获取改为退避重试（在 `start_discovery` 调用点包循环，**不放进** `acquire_input_service_network_lease` —— 那里持 lease Mutex，重试睡眠会卡 install/uninstall 命令）：首试 + `LEASE_TAKEOVER_RETRY_DELAYS = [1s, 3s]` 两次重试；每次失败 `log::warn!("network takeover attempt {n} failed: {error}")`，最终失败 `log::error!` 后返回 Err。长尾交给自愈看门狗。
- 重试结束后**重新长持** `discovery_stop` 锁并复查 `is_some()`（防看门狗与用户命令并发双启动、双双绑到相邻端口——现有代码本就是持锁到函数尾，保持该结构）。

### 2. 静默失败落日志

- [lib.rs:3035](C:\Dev\DVG\mykvm\src-tauri\src\lib.rs#L3035) `start_runtime_inner`：`discovery_error` 存在时 `log::error!("discovery failed to start: {error}")`。
- [lib.rs:5272](C:\Dev\DVG\mykvm\src-tauri\src\lib.rs#L5272) `.setup` 吞错改 `if let Err(error) = ... { log::error!(...) }`；lib.rs:9690 更新安装失败路径同样补日志。

### 3. 状态诚实（错误不被 2s 轮询冲掉）

- `AppRuntime` 加 `last_discovery_error: Mutex<Option<String>>`：上述失败点写入、`start_discovery` 成功路径清除。
- `runtime_status_for_layout`（lib.rs:822）：`runtime.started && discovery.state=="idle"` 时置 `state="error"` + detail 取 `last_discovery_error`（无则 "not running; retrying"）。抽纯函数 `surfaced_discovery_status(...)` 便于单测。

### 4. 自愈看门狗

- 新 `spawn_discovery_recovery_watcher(app)`，完全照 `spawn_lock_sync_watcher`（lib.rs:4042）模板：命名线程 `mykvm-discovery-retry`，每 5s 检查：
  - `upgrading` 置位 → 跳过（更新安装窗口不抢端口）
  - `runtime.started == false` 或 `discovery_stop.is_some()` → 跳过
  - 否则调 `start_discovery()`：成功 `log::info!("discovery recovered...")` 并清 `last_discovery_error`；失败限频 `log::warn!`（至多 60s 一条，防刷日志）。
- 在 `.setup` 里 `spawn_lock_sync_watcher` 调用点（lib.rs:5182）旁 spawn。

## 测试

- Rust（`src-tauri`）：
  - headless_client.rs 测试模块：`headless_receiver_skips_when_paused_for_exit`（eligible_layout + `headlessPaused:true` → false）。
  - lib.rs 测试模块（用现有 `test_layout()`/`test_peer()` 构造器）：`headless_takeover_would_continue_*` 2-3 例；`surfaced_discovery_status_*` 2 例（started+idle+错误 → error；!started+idle → idle）。
- 前端：i18n 双语形状由 `as const`+TS 编译期保证；App 组件无既有测试基建，不新增。

## 验证

1. `cd src-tauri && cargo test --lib` 全绿（Windows 现 415+）；`cargo fmt` 只碰改动文件。
2. `npm test` 全绿。
3. 构建 `npm run tauri -- build --ci -b nsis --config '{"bundle":{"createUpdaterArtifacts":false}}'` 并安装，真机矩阵：
   - 托盘退出（client+receive+已配对+服务运行）→ 弹窗出现；「直接退出」→ 退出后 Mac 仍可移入（helper 顶班，端口归 helper）。
   - 重新打开 → 自动恢复；再退出选「退出并暂停被控」→ 退出后 Mac 移不进来（helper 不接管，netstat 47833/47834 无人持有），layout.json 含 `headlessPaused:true`。
   - 再打开 → 标记清除、被控恢复；Mac 布局点按屏幕 → **蓝色光圈出现**。
   - 正常启动日志仍有 `discovery started ... quic_port=47834`，无新增 takeover 告警。
