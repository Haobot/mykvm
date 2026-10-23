# MyKVM — Agent 记忆与工程约定

本文件供后续 AI/自动化会话快速进入项目。详细复盘见 [Doc/lessons/](Doc/lessons/)。

## 项目速览

- Tauri 2 + Rust 后端（`src-tauri/`）+ React 前端（`src/`）。服务端/客户端模型：UDP 47833 发现，47834 QUIC 输入/剪贴板。
- 关键模块：[`src-tauri/src/input.rs`](src-tauri/src/input.rs)（捕获/注入/输入协议）、[`src-tauri/src/lib.rs`](src-tauri/src/lib.rs)（编排/发现/配对/剪贴板）。
- 判方向：`~/Library/Application Support/com.xzhpl.mykvm/layout.json` 里的 `machineRole`；**跨平台改键由发送输入的一端应用**，被控端（client）的 `modifierMap` 对接收方向无效。

## 构建与运行（macOS 开发机）

- Rust 不在默认 PATH：`export PATH="$HOME/.cargo/bin:$PATH"`。
- 检查：`cd src-tauri && cargo test --lib`（当前 218 个测试）；前端 `npm test`。
- 构建：`npm run tauri -- build --target aarch64-apple-darwin --bundles app --no-sign`。
- 安装并运行：`sh scripts/install-mac-app.sh`（退出旧应用 → ditto → 本地自签名 → open）。
- 日志：`~/Library/Logs/com.xzhpl.mykvm/mykvm.log`；配置：`~/Library/Application Support/com.xzhpl.mykvm/`。

## 已知系统坑（务必牢记）

1. **重装/重签名会扰动 macOS 权限与会话**：TCC（辅助功能/输入监控）可能需 `tccutil reset` 后重新授权；替换 `.app` 还会让 macOS 重启本地网络隐私会话，启动瞬间的阻塞 `sendto` 曾把发现线程永久卡死（应用已加 500ms 写超时 + 首次 announce 延迟 3s）。安装后核对日志应有 `discovery started` 与数秒内的 `discovery peer updated`；异常时重启一次应用再观察。
2. **macOS 合成键盘事件的两种位**：`CGEventFlag*` 是设备无关位；IME 的"按住修饰键"检测（微信输入法/WeType 等）还看 `NX_DEVICE*KEYMASK` 设备相关位（左⌘=0x8、右⌘=0x10、左⌥=0x20、右⌥=0x40、左⌃=0x1、右⌃=0x2000、左⇧=0x2、右⇧=0x4）。注入必须两者都带；`core-graphics` 的 `CGEventFlags::from_bits_truncate` 会丢掉未声明的设备位，必须用 `from_bits_retain`。
3. **微信输入法（WeType）语音 PTT**：配置在 `~/Library/Application Support/WeType/mmkv/wetype.settings`（`voicePTTShortcut_keyCodes`，55=左 Command），长按阈值实测约 1.2s。本机同时装着豆包输入法，两者都会在 HID 层监听 flagsChanged，调试时注意事件竞争/消费。
4. **系统状态类阶段状态不能复用启动快照**：注入/捕获状态里的"当前系统是否有权限/是否开着安全键盘输入/QUIC 是否就绪"会随系统变化，而应用登录自启时会在 `.setup` 里 eager start 输入链路，很容易撞上锁屏/登录窗口的安全输入状态。客户端 inject 状态过去只在 `start_input` 算一次，导致「安全键盘输入已开启」提示在来源关闭后一直显示、且前端每 2s 轮询把用户关掉的弹窗重新打开（见 [2026-10 复盘](Doc/lessons/2026-10-安全键盘输入状态误报复盘.md)）。现在 `runtime_status_for_layout` 在 `started && inputMode=receive` 时每次读取都重新探测，前端 `src/runtimeBlocker.ts` 对同一阻塞原因只弹一次并在解除后自动关闭。新增类似提示时必须照此处理。
5. **仓库约定**：遵循 [CONTRIBUTING.md](CONTRIBUTING.md) 的 Conventional Commits；push 到 `main` 会触发 Release workflow，`fix:`/`feat:` 会发版，纯文档用 `docs:`。

## 改动输入/发现后的验证清单

1. `cargo test --lib` 全绿；`rustfmt` 只检查改动文件（`cargo fmt` 会顺带改动 `input-helper/src/main.rs`，提交前 revert 无关文件）。
2. 安装后日志：无 `input inject/capture unavailable`；有 `discovery started`；数秒内出现 `discovery peer updated`（或抓包能看到本机 announce）。
3. 真机：对端能扫描到本机；按住 Alt（=Command）能触发微信输入法语音；Alt+C/V 复制粘贴正常。
4. macOS 安全键盘输入提示：用 Carbon `EnableSecureEventInput()/DisableSecureEventInput()` 模拟开/关，提示应出现一次、点击关闭后不再重开、解除后自动消失；日志应成对出现 `Secure Keyboard Entry turned on/off`（时间戳为 UTC）。
