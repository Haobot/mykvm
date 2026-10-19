# 2026-10 配对故障排查与修复复盘

> 范围：Windows 端与 macOS 端"设备列表可见、但布局离线、重新配对报 no pairing challenge"故障的定位、修复、验证与部署。
> 涉及提交：`56779ae`（构建修复）、`4c8a491`（配对修复）。
> 结论先行：代码修复本身不大（3 处健壮性缺口），主要成本在**构建环境**与**系统信任模型（TCC/签名）**。

## 1. 现象

- Win 端回连 Mac 端异常：设备列表能看到对端名称和 IP，但显示器布局里该设备显示离线。
- 设备设置点"重新配对"无效，提示：
  `No pairing challenge received from 192.168.3.44; Make sure the client is running and reachable`
- 该 IP 是历史地址：排查时 Mac 实机为 `192.168.3.45`、Windows 为 `192.168.3.172`，`.44` 已无主机响应。

## 2. 排查时间线（2026-10-19）

| 时间（本地） | 事件 |
| --- | --- |
| 11:42 | 收到故障报告，开始阅读代码/本机配置/日志 |
| 11:43–12:00 | 读取 `~/Library/Application Support/com.xzhpl.mykvm/layout.json`、`~/Library/Logs/com.xzhpl.mykvm/mykvm.log`；对本机与局域网做 UDP 探测；确认 Mac 当前 IP 与 Windows 主机（PROART16 192.168.3.172）在线情况 |
| 12:00–12:20 | 搭 Rust/Node 构建环境（本机原本没有 Rust；官方源卡死，换清华镜像；crates 并发预取） |
| 12:20–13:00 | 定位 3 个配对健壮性缺口 + 发现 main 在 macOS/Linux 编译失败；实现修复与单测 |
| 13:00–13:15 | 前端修复（另一 agent 完成）、Rust 修复、全量验证、提交并推送 `56779ae`/`4c8a491` |
| 13:18–13:21 | 按要求构建 macOS 端并安装到 `/Applications/mykvm.app`（版本 0.1.0） |
| 13:22–13:24 | 出现"已授权仍提示缺少辅助功能/输入监控"；定位为本地重签名导致 TCC 条目失配，`tccutil reset` + 重新授权后恢复（日志确认无权限告警） |

## 3. 根因

### 3.1 重新配对只使用持久化旧地址（主因）

- 前端"重新配对"取 `device.host`（布局里保存的地址）直接请求；对端 DHCP 换址/证书轮换后该设备在布局中一直离线，`host` 不会被 discovery 刷新，而它的**实时地址其实就在 `runtime.discovery.peers`（设备列表）里**。
- 请求打到旧 IP → UDP 无人应答 → 1.8s 超时 → 报 `No pairing challenge received from <旧IP>`。
- 附带：成功配对后前端按 `id`/`transportPublicKey` 匹配旧设备；换 IP/证书后两者都不相等，会**新增一台重复设备**，旧的离线设备永远留在布局里。

### 3.2 客户端对 `pair-request` 完全静默

`begin_pairing_challenge` 在以下情况返回 false 且**不回任何数据包**：

- 对端（本机）不是 `client` 角色；
- 已与其它服务端配对，且请求方不像已知控制端；
- 正在处理另一台设备的配对（challenge 被占用）。

服务端只能看到超时，错误信息因此具有误导性（"客户端没运行/不可达"，实际客户端在线且已收到请求）。协议里早有 `pairing_error` 字段，但从未使用。

### 3.3 本机 host/IP 标签只在启动时生成一次

`layout.devices[local].host`（形如 `"MacBook-Pro.local / 192.168.3.172, 192.168.3.44"`）来自启动时的探测，DHCP 换址后不再刷新——用户会照着 UI 里的**过期 IP** 手动配对/排障。

### 3.4 附带发现：main 在非 Windows 平台编译失败

Windows 构建不受影响，但 macOS/Linux 自 10-13 起无法编译（两天内无人发现）：

- `ca0eef6` 清理 clippy 时误删 `activate_existing_instance()` macOS 分支的 `return`/分号处理 → 分支返回 `()` 而签名是 `-> bool`；
- `232989a` 把 Windows 锁屏服务机械搬移到 `windows_service.rs` 时，文件顶部 `std/crate` import 未按平台门控（`wide_null` 等仅在 `cfg(windows)` 下存在）。

### 3.5 部署阶段：本地重签名导致 TCC 授权失效

用本机自签名证书重新构建安装后：

- 新旧 app 的 designated requirement 不同：旧 leaf `H"e5f86bf3…"`，新 leaf `H"4b15c980…"`；
- TCC（辅助功能/输入监控）授权按**代码要求（证书 leaf）**匹配，系统日志给出明确证据：
  `tccd: Failed to match existing code requirement for subject com.xzhpl.mykvm and service kTCCServiceAccessibility`；
- 用户在设置里勾选的是旧条目，对新构建无效 → "已授权 + 重启"仍然提示缺少权限。
- 修复：`tccutil reset Accessibility com.xzhpl.mykvm` + `tccutil reset ListenEvent com.xzhpl.mykvm`，重新触发系统授权并完全退出重开；之后启动日志不再出现 `input capture/inject unavailable`。

## 4. 修复内容

### 4.1 `56779ae` fix(构建)

- `activate_existing_instance()` macOS 分支改为块尾表达式（去掉分号），返回值语义不变；
- `windows_service.rs` 平台相关 import 全部加 `#[cfg(target_os = "windows")]`；类型与 `current_input_service_status` 桩函数仍全平台可用。
- 零行为变更，Windows 构建语义不变。

### 4.2 `4c8a491` fix(配对)

后端（`src-tauri/src/lib.rs`、`headless_client.rs`）：

- `request_lan_pairing`/`confirm_lan_pairing` 增加可选 `device_id`；按 **实时 peer IP/host → 设备保存地址（支持 `"name / ip, ip"` 拆分）→ 调用方 host** 逐候选请求，返回首个 `pair-challenge`；错误信息列出实际尝试过的地址；
- 候选被无关设备回绝时继续尝试后续候选；`confirm` 后的校验改打 challenge 来源的 `(ip, discovery port)`；
- 客户端无法配对时回 `pair-reject` + `pairing_error` 具体原因（角色不是客户端/已与其他服务端配对/正在配对/仅 headless 服务在线）；`headless_client` 也回明确原因；旧版对端会忽略未知 kind，协议向后兼容；
- 发现线程每 10s 刷新本机 host 标签（`local_host_label`），不再显示失效 IP；
- 新增日志：pairing request accepted/rejected 及原因（服务端与客户端日志都能看到）。

前端（`src/pairingRepair.ts`、`App.tsx`、`desktopApi.ts`）：

- 纯函数模块：实时 peer 分层匹配（传输公钥 > 设备 id > 同 cluster + host/ip > 名称兜底）、候选地址计算、"同机离线设备"识别、按 `preferredDeviceId` **原位 upsert**（保留 id/颜色/指针与滚轮速度/屏幕摆放/用户可见名称）；
- 重新配对、Devices 页"重新配对"、确认配对后的设备更新均接入；配对 API 增加可选 `deviceId`；
- #26 的"手动设备保留手动地址"语义在非 repair 路径不变。

## 5. 验证证据

| 检查 | 命令 | 结果 |
| --- | --- | --- |
| Rust 全量测试 | `cargo test --workspace --locked` | lib **216 passed**，input-helper **2 passed**，0 failed |
| Rust 全目标编译 | `cargo check --workspace --all-targets --locked` | exit 0 |
| 前端测试 | `npm test` | **21 passed**（新增 13 个 pairingRepair 用例） |
| 前端 lint/build | `npm run lint` / `npm run build` | exit 0 |
| 测试灵敏度 | 变异测试 | Rust 3/3 命中（候选顺序反转、删除拒绝原因、回绝后不再尝试）；前端 4 个初始缺口补测后全部可被 mutation 抓住 |
| 格式 | `cargo fmt --all -- --check` | 仅 `input-helper/src/main.rs` 既有 7 处 drift（工具链从旧 rustfmt → 1.10 的漂移，HEAD 既有，不计入本轮） |
| clippy | 本机 clippy 1.99 | 59 条新 lint 全部落在未改动行（`manual_c_str_literals` 等工具链漂移），本轮新增行 0 告警 |
| Windows target 交叉编译 | `cargo check --target x86_64-pc-windows-msvc` | **环境限制失败**：`ring` build script 需要 Windows MSVC/mingw C 工具链（缺 `assert.h`）；已用只读审阅 + Rust 私有字段跨子模块可见性实验佐证，**真机编译仍需 Windows 机器/CI** |

**未验证**：Windows 端实际构建、安装与双端真机联调（由另一台 Windows 机器执行，结果待回传）。

## 6. 排障手册（可复用）

1. **日志时间戳是 UTC**，文件 mtime 是本地时间；换算后再对时间线：
   `~/Library/Logs/com.xzhpl.mykvm/mykvm.log`（macOS）、`%LOCALAPPDATA%\com.xzhpl.mykvm\logs\mykvm.log`（Windows）
2. **配置文件**：macOS `~/Library/Application Support/com.xzhpl.mykvm/layout.json`；Windows `%APPDATA%\com.xzhpl.mykvm\layout.json`。重点看 `machineRole`、`pairedControllers`、`clusterId`、`devices[].host/transportPublicKey`。
3. **地址到底是谁的**：`arp -a` / `ping` / `ifconfig`；对比 layout 里本机 host 与实时地址（本缺陷就是两者不一致）。
4. **UDP 发现探测**：MyKVM 发现协议为 msgpack over UDP 47833 起 8 个端口 span；可手写最小 msgpack 客户端发送 `probe` 看是否收到 `reply`（见本次排查脚本思路）。
5. **签名身份对比**（macOS）：
   `codesign -dvvv <app>`、`codesign -d -r- <app>` 看 designated requirement 的证书 leaf 是否变化。
6. **TCC 授权诊断/重置**（macOS）：
   `log show --last 10m --predicate 'subsystem == "com.apple.TCC"' | grep -i mykvm`
   `tccutil reset Accessibility <bundle-id>`、`tccutil reset ListenEvent <bundle-id>`，然后重新授权并完全退出重开应用。
7. **端口/进程**：`lsof -nP -iUDP | grep -i mykvm` 确认 47833/47834 绑定；`ps aux | grep mykvm` 确认没有多实例。

## 7. 经验与教训

### 技术

- **"存储值"不是"当前值"**：任何面向设备的动作（配对/文件传输/更新）都应通过实时 discovery peer 解析地址，持久化字段只做兜底。
- **静默失败是调试地狱**：请求-响应协议必须显式回绝并携带原因；协议字段（如 `pairing_error`）设计了就要用。
- **易变信息要有刷新机制**：host/IP、显示列表等不能只在启动时算一次；身份绑定稳定证书，地址只做可变索引。
- **状态模型要显式**：证书轮换后"设备列表有、布局离线"是设计使然，但 UI 应区分 `offline` 与 `needs re-pair`，而不是只显示"离线"。
- **CI/平台门禁必须真实生效**：Windows-only 开发环境下，macOS/Linux 编译错误可以存活数天；至少 `cargo check --all-targets` 要在多平台跑。
- **重签名 = 换身份**：macOS TCC 按代码要求（证书 leaf）授权；替换本地自签名构建必须重置/重新授权，安装脚本应检测 identity 变化并提示。

### 流程

- **先环境与基线，再深挖代码**：本次直到中段才装 Rust，随后发现 main 编译失败，顺序本末倒置。
- **证据优先于猜测**：日志时区、layout.json、网络探测、签名 DR、TCC 日志，每一条都能把范围砍半；不要长时间在"哪边是服务端"上打转。
- **测试绿 ≠ 测试有效**：用变异测试验证测试真的能抓住回归（本次前端 4 个覆盖缺口、Rust 3 个关键点均这样发现）。
- **关键验证不单点依赖 agent**：子 agent 可以并行产出（前端实现、独立验收），但最终结论必须 Lead 自己能复跑命令。
- **提交拆分**：构建修复与功能修复分开 commit，便于回滚与复用；不要把工具链漂移（rustfmt/clippy）顺手混进功能提交。
- **版本策略**：本地测试构建版本号若低于线上版本（如 0.1.0 vs 0.9.13），会触发应用内"有更新"提示，误点会把修复版降级；测试构建应统一用更高的本地版本（如 `0.9.14-local`）。
- **发布分支意识**：main 是 release 触发分支，`feat:`/`fix:` 前缀会驱动自动发版流程；纯文档/调试提交注意前缀（`docs:`/!`chore:`）。

## 8. 改进建议

- **P0 代码**：设备状态新增 `needs-re-pair`（证书不匹配时提示"需要重新配对"）；确认两端的 `pair-reject` 已在真机联调覆盖。
- **P0 运维**：`scripts/install-mac-app.sh` 增加"签名身份变化检测 + 权限重置提示"；README 增加"本地自签名构建后的 TCC 授权"章节。
- **P0 流程**：push 前跑与 CI 等价的检查矩阵；修复 rustfmt/clippy 工具链漂移（锁定 toolchain 或随 stable 一次性格式化）。
- **P1 测试**：把"配对候选排序、协议拒绝、状态刷新"三类变异自检固化为模板；为 Windows-only 模块补 `cargo check --target x86_64-pc-windows-msvc` 的 CI job。
- **P1 文档**：本复盘 + Windows 本地构建要点（见附录 B）进入 `Doc/`。

## 9. 遗留风险

- Windows 端构建/安装/联调未在本环境完成（本机交叉编译受 ring C 工具链限制）；
- 双端真机配对闭环待 Windows 新包安装后验证；
- 本机 clippy/fmt 与 HEAD 存在工具链漂移（既有债务），若 CI 使用最新 stable 仍会红；
- 本地 macOS 测试构建版本 0.1.0，需注意更新提示与"不要点更新"。

## 附录 A：本机构建环境要点（macOS）

- 工具链：rustup 官方源在本网络下极慢（~10KB/s），可用镜像：
  `RUSTUP_DIST_SERVER=https://mirrors.tuna.tsinghua.edu.cn/rustup`（实测快）；
- crates 下载：`~/.cargo/config.toml` 配置 sparse 镜像（注意：清华 sparse 索引的 `dl` 仍指向 static.crates.io，速度受限时可用 rsproxy/USTC，或先并发预取 `.crate` 到 `~/.cargo/registry/cache/<registry>/ `）；
- 中断的下载可能留下**截断的 .crate**，表现为 `failed to unpack ... unexpected end of file`——删掉对应文件重新下载即可；
- 构建安装：`npx tauri build --target aarch64-apple-darwin --bundles app --no-sign`，然后 `scripts/install-mac-app.sh <built.app>`（脚本会自签名并重启应用）；
- 签名证书由脚本生成并保存在 `~/Library/Keychains/mykvm-local-signing.keychain-db`，**后续重建复用同一证书，TCC 授权可延续**。

## 附录 B：Windows 本地构建检查清单

1. 依赖：Node ≥22、Rust stable ≥1.89、VS 2022 Build Tools（C++ 工作负载 + Windows SDK）、WebView2；
2. 取源码到 `4c8a491`（或使用同提交源码包），`npm ci`；
3. 基线：`cargo test --manifest-path src-tauri/Cargo.toml --workspace --locked`（0 failed 即可）；
4. `tauri.conf.json` 中 `bundle.createUpdaterArtifacts = true`，本地无更新签名私钥时用配置覆盖：
   `'{"bundle":{"createUpdaterArtifacts":false}}' | Set-Content -Encoding utf8 src-tauri\tauri.local-test.conf.json`
   `npx tauri build --config src-tauri\tauri.local-test.conf.json`；
   - 产物：`src-tauri\target\release\mykvm.exe` 与 `...\bundle\nsis\mykvm_0.1.0_x64-setup.exe`；
5. 验证修复在产物内：读取 exe 字节查找 ASCII `pair-reject` 应为 `FIX_PRESENT`；
6. 本地测试版本若低于线上，忽略"更新可用"提示，避免降级回旧版。

## 附录 C：相关提交

- `56779ae` fix(构建): 修复 macOS/Linux 上 lib 无法编译的两处既有错误
- `4c8a491` fix(配对): 重新配对改用实时发现地址、客户端回绝原因、本机地址刷新
