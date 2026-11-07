//! Agent 通道 L0 只读文件访问（RFC：Doc/Rfc/跨机Agent互联与受控协作通道.md
//! §5.3 / §6.2，P1b）。
//!
//! 这是「对端可读本机文件」暴露给网络的边界：
//! - 路径安全层 [`resolve_agent_readable_path`]：请求必须是绝对路径；规范化（解析
//!   `..`、symlink/junction、Windows `\\?\` 前缀与大小写）之后，结果必须**逐组件**
//!   落在某个根目录内才放行。防线判的是「规范化结果」而不是「原始字符串」——根内
//!   链接指向根外、`..` 穿越都会在规范化之后现形。
//!
//! 根目录与日志目录一律以参数注入（不在本模块读取真实用户目录），测试注入临时目录。

use std::{
    ffi::OsString,
    path::{Component, Path, PathBuf, Prefix},
};

use crate::agent_channel::AgentChannelConfig;

/// file.stat/file.read 的可读根：layout 配置了 file_roots（sanitized 后非空）时
/// 用配置，否则用注入的默认根（生产 = [本机日志目录, 文件接收根]）。
pub(crate) fn agent_file_roots(
    config: &AgentChannelConfig,
    default_roots: &[PathBuf],
) -> Vec<PathBuf> {
    let configured = config.sanitized().file_roots;
    if configured.is_empty() {
        return default_roots.to_vec();
    }
    configured.into_iter().map(PathBuf::from).collect()
}

/// 默认可读根的组成：[本机日志目录, 文件接收根]（缺失的项跳过）。
pub(crate) fn default_agent_file_roots(
    log_dir: Option<&Path>,
    receive_root: Option<&Path>,
) -> Vec<PathBuf> {
    [log_dir, receive_root]
        .into_iter()
        .flatten()
        .map(Path::to_path_buf)
        .collect()
}

/// 把对端请求的路径解析为本机可读的规范化路径；不在任何根内一律拒绝。
///
/// 判定顺序（前三步是纯词法、不触盘）：
/// 1. 空路径、相对路径拒绝——相对路径的含义取决于本机进程当前目录。
/// 2. 设备命名空间（`\\.\pipe\...` 等）拒绝；UNC 只在存在网络根时才继续——否则
///    对端可借一次 stat 让本机向任意 SMB 主机发起连接（Windows 会自动携带 NTLM
///    凭据），解析不可达主机还会阻塞数秒。
/// 3. 相对根忽略（fail closed）：macOS GUI 应用的当前目录是 `/`，相对根 "." 等于
///    整个文件系统。
/// 4. 请求与各根都规范化（不存在的部分见 [`canonicalize_allowing_missing_tail`]），
///    规范化结果按组件前缀（`Path::starts_with`，非字符串前缀）落在某个根内才放行。
///    两侧都取盘上真实大小写与 verbatim 形式，Windows 大小写/`\\?\` 差异不影响判定。
///
/// 返回值是规范化后的路径，调用方后续一律操作它而不是原始请求。
pub(crate) fn resolve_agent_readable_path(
    roots: &[PathBuf],
    requested: &str,
) -> Result<PathBuf, String> {
    if requested.trim().is_empty() {
        return Err("路径为空".into());
    }
    let requested_path = Path::new(requested);
    if !requested_path.is_absolute() {
        return Err("必须是绝对路径（Windows 需带盘符，如 C:\\...）".into());
    }
    let usable_roots: Vec<&PathBuf> = roots.iter().filter(|root| root.is_absolute()).collect();
    match path_volume(requested_path) {
        PathVolume::Device => return Err("不支持设备命名空间路径".into()),
        PathVolume::Network
            if !usable_roots
                .iter()
                .any(|root| path_volume(root) == PathVolume::Network) =>
        {
            return Err("路径不在允许的根目录内（未配置网络共享根）".into());
        }
        _ => {}
    }

    let resolved = canonicalize_allowing_missing_tail(requested_path)?;
    let inside_some_root = usable_roots.iter().any(|root| {
        canonicalize_allowing_missing_tail(root)
            .is_ok_and(|canonical_root| resolved.starts_with(&canonical_root))
    });
    if inside_some_root {
        Ok(resolved)
    } else {
        Err("路径解析后不在允许的根目录内".into())
    }
}

/// 路径所在的「卷」类别（纯词法，看 Windows 路径前缀；Unix 路径无前缀恒为 Local）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PathVolume {
    Local,
    Network,
    Device,
}

fn path_volume(path: &Path) -> PathVolume {
    let Some(Component::Prefix(prefix)) = path.components().next() else {
        return PathVolume::Local;
    };
    match prefix.kind() {
        Prefix::Disk(_) | Prefix::VerbatimDisk(_) => PathVolume::Local,
        Prefix::UNC(..) | Prefix::VerbatimUNC(..) => PathVolume::Network,
        // `\\?\GLOBALROOT\...`、`\\?\Volume{GUID}\...` 等非盘符 verbatim 形式一并按
        // 设备处理：根经 canonicalize 后是 `\\?\C:\...` 形式，这类请求本就匹配不上。
        Prefix::DeviceNS(_) | Prefix::Verbatim(_) => PathVolume::Device,
    }
}

/// 规范化一个可能不存在的路径：存在 → `canonicalize()`；不存在 → `canonicalize()`
/// 其最长的存在祖先，再拼回不存在的剩余组件（file.stat 不存在的路径据此得到
/// exists=false 而不是报错）。
///
/// 两条保守规则：剩余部分只允许普通组件——`..` 落在不存在的段里时语义不可判定
/// （Windows 词法折叠、Unix 按真实目录解析），拒绝；不存在却能 lstat 到的段是悬空
/// 链接（或无权访问的条目），其最终指向随时可能被建出来，拒绝。
fn canonicalize_allowing_missing_tail(path: &Path) -> Result<PathBuf, String> {
    let mut ancestor = path.to_path_buf();
    let mut missing_tail: Vec<OsString> = Vec::new();
    loop {
        if ancestor.exists() {
            let mut resolved = ancestor
                .canonicalize()
                .map_err(|error| format!("路径无法规范化: {error}"))?;
            resolved.extend(missing_tail.iter().rev());
            return Ok(resolved);
        }
        if std::fs::symlink_metadata(&ancestor).is_ok() {
            return Err("路径包含无法解析的链接（悬空链接或无权访问）".into());
        }
        match ancestor.components().next_back() {
            Some(Component::Normal(name)) => {
                missing_tail.push(name.to_os_string());
                ancestor.pop();
            }
            _ => return Err("路径不存在且无法解析".into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    /// 测试沙箱（tempfile 自动清理；所有链接目标都在沙箱内，清理不会波及外部）：
    ///
    /// ```text
    /// <base>/root/inner.txt
    /// <base>/root/sub/nested.txt
    /// <base>/root/sub/deeper/
    /// <base>/outside.txt
    /// <base>/outside_dir/secret.txt
    /// <base>/root-evil/secret.txt      与根同名前缀的兄弟目录（字符串前缀判定会误放行）
    /// ```
    struct Sandbox {
        _guard: tempfile::TempDir,
        base: PathBuf,
        root: PathBuf,
    }

    impl Sandbox {
        fn new() -> Self {
            let guard = tempfile::tempdir().expect("创建临时目录");
            let base = guard.path().to_path_buf();
            let root = base.join("root");
            std::fs::create_dir_all(root.join("sub").join("deeper")).expect("建根内目录");
            std::fs::write(root.join("inner.txt"), b"inner").expect("写根内文件");
            std::fs::write(root.join("sub").join("nested.txt"), b"nested").expect("写嵌套文件");
            std::fs::write(base.join("outside.txt"), b"outside").expect("写根外文件");
            std::fs::create_dir_all(base.join("outside_dir")).expect("建根外目录");
            std::fs::write(base.join("outside_dir").join("secret.txt"), b"secret")
                .expect("写根外文件");
            std::fs::create_dir_all(base.join("root-evil")).expect("建同前缀兄弟目录");
            std::fs::write(base.join("root-evil").join("secret.txt"), b"evil")
                .expect("写兄弟目录文件");
            Self {
                _guard: guard,
                base,
                root,
            }
        }

        fn roots(&self) -> Vec<PathBuf> {
            vec![self.root.clone()]
        }
    }

    fn text(path: &Path) -> String {
        path.to_str().expect("测试路径必须是 UTF-8").to_string()
    }

    fn canonical(path: &Path) -> PathBuf {
        path.canonicalize().expect("测试前置：路径必须存在")
    }

    /// 断言请求被拒绝，返回错误信息供进一步断言。
    fn assert_rejected(roots: &[PathBuf], requested: &str) -> String {
        match resolve_agent_readable_path(roots, requested) {
            Ok(resolved) => panic!("请求 {requested:?} 必须被拒绝，实际放行为 {resolved:?}"),
            Err(error) => error,
        }
    }

    /// 在 `link` 处建一个指向 `target` 目录的链接。Unix 用 symlink；Windows 用目录
    /// junction（`mklink /J`）——本机实测 std 的 symlink_file/symlink_dir 在无特权、
    /// 未开开发者模式时报 ERROR_PRIVILEGE_NOT_HELD(1314)，而 junction 普通用户即可
    /// 创建，且同样是会被 canonicalize 解析的重解析点（逃逸防线要防的正是它）。
    fn create_dir_link(target: &Path, link: &Path) {
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, link).expect("创建目录 symlink");
        #[cfg(windows)]
        {
            let output = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(link)
                .arg(target)
                .output()
                .expect("执行 mklink /J");
            assert!(
                output.status.success(),
                "创建 junction 失败: {}",
                String::from_utf8_lossy(&output.stderr)
            );
        }
    }

    // ---- 肯定路径：根内文件 / 根自身 / 嵌套子目录 / 根内不存在的路径 ----

    #[test]
    fn resolves_file_inside_root_to_canonical_path() {
        let sandbox = Sandbox::new();
        let requested = sandbox.root.join("inner.txt");
        assert_eq!(
            resolve_agent_readable_path(&sandbox.roots(), &text(&requested)),
            Ok(canonical(&requested)),
            "根内文件必须放行，并返回规范化路径（后续读取一律用它，而不是原始请求）"
        );
    }

    #[test]
    fn root_itself_is_readable() {
        let sandbox = Sandbox::new();
        assert_eq!(
            resolve_agent_readable_path(&sandbox.roots(), &text(&sandbox.root)),
            Ok(canonical(&sandbox.root)),
            "请求根自身（stat 根目录）必须放行"
        );
    }

    #[test]
    fn nested_paths_inside_root_are_allowed() {
        let sandbox = Sandbox::new();
        for requested in [
            sandbox.root.join("sub").join("deeper"),
            sandbox.root.join("sub").join("nested.txt"),
        ] {
            assert_eq!(
                resolve_agent_readable_path(&sandbox.roots(), &text(&requested)),
                Ok(canonical(&requested)),
                "嵌套路径 {requested:?} 必须放行"
            );
        }
    }

    #[test]
    fn missing_path_inside_root_resolves_against_longest_existing_ancestor() {
        // file.stat 一个不存在的路径要能得到 exists=false 而不是报错：规范化最长的
        // 存在祖先（root/sub），再拼回不存在的剩余部分。
        let sandbox = Sandbox::new();
        let requested = sandbox.root.join("sub").join("missing").join("new.txt");
        assert_eq!(
            resolve_agent_readable_path(&sandbox.roots(), &text(&requested)),
            Ok(canonical(&sandbox.root.join("sub"))
                .join("missing")
                .join("new.txt"))
        );
    }

    #[test]
    fn dot_dot_staying_inside_root_is_resolved_not_rejected() {
        let sandbox = Sandbox::new();
        let requested = sandbox.root.join("sub").join("..").join("inner.txt");
        assert_eq!(
            resolve_agent_readable_path(&sandbox.roots(), &text(&requested)),
            Ok(canonical(&sandbox.root.join("inner.txt"))),
            "`..` 是被解析而不是一律拒绝：解析后仍在根内即放行"
        );
    }

    #[test]
    fn link_inside_root_pointing_inside_root_is_allowed() {
        let sandbox = Sandbox::new();
        let link = sandbox.root.join("inner_link");
        create_dir_link(&sandbox.root.join("sub"), &link);
        assert_eq!(
            resolve_agent_readable_path(&sandbox.roots(), &text(&link.join("nested.txt"))),
            Ok(canonical(&sandbox.root.join("sub").join("nested.txt"))),
            "根内链接指向根内：按解析后的真实路径放行"
        );
    }

    #[test]
    fn any_matching_root_allows_request() {
        let sandbox = Sandbox::new();
        let roots = vec![sandbox.base.join("outside_dir"), sandbox.root.clone()];
        let requested = sandbox.root.join("inner.txt");
        assert_eq!(
            resolve_agent_readable_path(&roots, &text(&requested)),
            Ok(canonical(&requested))
        );
    }

    #[test]
    fn missing_root_still_bounds_requests() {
        // 根尚未创建（如首次接收文件之前的 MyKVM Transfers）：根自身按「最长存在
        // 祖先 + 剩余部分」规范化——stat 它得到 exists=false 而不是「不在根内」；
        // 根外照样拒绝。
        let sandbox = Sandbox::new();
        let pending_root = sandbox.base.join("not-yet-created");
        let roots = vec![pending_root.clone()];
        assert_eq!(
            resolve_agent_readable_path(&roots, &text(&pending_root)),
            Ok(canonical(&sandbox.base).join("not-yet-created"))
        );
        assert_rejected(&roots, &text(&sandbox.base.join("outside.txt")));
    }

    // ---- 否定路径 ----

    #[test]
    fn rejects_absolute_path_outside_roots() {
        let sandbox = Sandbox::new();
        let error = assert_rejected(&sandbox.roots(), &text(&sandbox.base.join("outside.txt")));
        assert!(
            error.contains("不在允许"),
            "错误信息必须说明原因（实际 {error:?}）"
        );
    }

    #[test]
    fn rejects_dot_dot_traversal_out_of_root() {
        let sandbox = Sandbox::new();
        for requested in [
            sandbox.root.join("..").join("outside.txt"),
            sandbox
                .root
                .join("sub")
                .join("..")
                .join("..")
                .join("outside.txt"),
            sandbox
                .root
                .join("sub")
                .join("..")
                .join("..")
                .join("outside_dir")
                .join("secret.txt"),
        ] {
            assert_rejected(&sandbox.roots(), &text(&requested));
        }
    }

    #[test]
    fn rejects_dot_dot_traversal_through_missing_directory() {
        // 穿过不存在的目录再 `..`：Windows 对 `..` 做词法折叠、Unix 按真实目录逐级
        // 解析——两种语义下都不得落到根外（目标存在与否都一样）。
        let sandbox = Sandbox::new();
        for leaf in ["outside.txt", "nope.txt"] {
            let requested = sandbox
                .root
                .join("missing")
                .join("..")
                .join("..")
                .join(leaf);
            assert_rejected(&sandbox.roots(), &text(&requested));
        }
    }

    #[test]
    fn rejects_sibling_sharing_root_name_prefix() {
        // root-evil 与 root 在字符串上前缀相同：必须逐组件比较（字符串前缀会误放行）。
        let sandbox = Sandbox::new();
        assert_rejected(
            &sandbox.roots(),
            &text(&sandbox.base.join("root-evil").join("secret.txt")),
        );
        assert_rejected(&sandbox.roots(), &text(&sandbox.base.join("root-evil")));
    }

    #[test]
    fn rejects_link_inside_root_escaping_to_outside() {
        // 逃逸防线 = 「判规范化结果」本身：链接名在根内（原始字符串看着合法），
        // 解析后落到根外 → 拒绝；经链接拼不存在的尾部同样拒绝。
        let sandbox = Sandbox::new();
        let link = sandbox.root.join("escape");
        create_dir_link(&sandbox.base.join("outside_dir"), &link);
        assert!(
            link.join("secret.txt").exists(),
            "前置：经链接确实能看到根外文件"
        );
        for requested in [
            link.join("secret.txt"),
            link.clone(),
            link.join("missing.txt"),
        ] {
            assert_rejected(&sandbox.roots(), &text(&requested));
        }
    }

    #[test]
    fn rejects_dangling_link_inside_root() {
        // 悬空链接的最终指向不可判定（目标随时可能被建出来）：拒绝，而不是当作
        // 「不存在的普通路径」拼接放行。
        let sandbox = Sandbox::new();
        let target = sandbox.base.join("vanishing_dir");
        std::fs::create_dir_all(&target).expect("建临时目标");
        let link = sandbox.root.join("dangling");
        create_dir_link(&target, &link);
        std::fs::remove_dir(&target).expect("删除目标使链接悬空");
        assert!(
            !link.exists() && std::fs::symlink_metadata(&link).is_ok(),
            "前置：链接已悬空"
        );
        assert_rejected(&sandbox.roots(), &text(&link));
        assert_rejected(&sandbox.roots(), &text(&link.join("new.txt")));
    }

    #[cfg(unix)]
    #[test]
    fn rejects_file_symlink_escaping_root() {
        // Windows 无特权建不了文件 symlink（实测 ERROR_PRIVILEGE_NOT_HELD）：文件级
        // 逃逸在 Windows 由 junction 用例（目录链接 + 其下文件）经同一条防线覆盖。
        let sandbox = Sandbox::new();
        let link = sandbox.root.join("link.txt");
        std::os::unix::fs::symlink(sandbox.base.join("outside.txt"), &link)
            .expect("创建文件 symlink");
        assert_rejected(&sandbox.roots(), &text(&link));
    }

    #[test]
    fn rejects_relative_and_empty_paths() {
        let sandbox = Sandbox::new();
        for requested in ["", "   "] {
            let error = assert_rejected(&sandbox.roots(), requested);
            assert!(
                error.contains("为空"),
                "空路径的错误信息必须说明原因（实际 {error:?}）"
            );
        }
        for requested in [
            "inner.txt",
            "root/inner.txt",
            "./inner.txt",
            "../outside.txt",
            "sub\\nested.txt",
        ] {
            let error = assert_rejected(&sandbox.roots(), requested);
            assert!(
                error.contains("绝对路径"),
                "相对路径 {requested:?} 的错误信息必须说明原因（实际 {error:?}）"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_drive_relative_and_root_relative_windows_paths() {
        // `C:inner.txt`（相对该盘当前目录）、`\inner.txt` 与 `/inner.txt`（相对当前盘
        // 的根）都不是绝对路径——解析结果取决于进程状态，必须拒绝。
        let sandbox = Sandbox::new();
        for requested in ["C:inner.txt", "\\inner.txt", "/inner.txt"] {
            let error = assert_rejected(&sandbox.roots(), requested);
            assert!(error.contains("绝对路径"), "（实际 {error:?}）");
        }
    }

    #[test]
    fn rejects_unc_path_without_touching_network() {
        // 没有任何网络根时，UNC 请求必须在触盘前拒绝：否则对端可借 file.stat 让本机
        // 向任意 SMB 主机发起连接（Windows 会自动携带 NTLM 凭据），且解析不可达主机
        // 会阻塞数秒。Unix 上反斜杠是普通字符，这些串是相对路径，同样拒绝。
        let sandbox = Sandbox::new();
        for requested in [
            r"\\server\share\x.txt",
            r"\\server\share",
            r"\\?\UNC\server\share\x.txt",
        ] {
            let started = Instant::now();
            assert_rejected(&sandbox.roots(), requested);
            assert!(
                started.elapsed() < Duration::from_secs(1),
                "UNC 拒绝必须是纯词法判定、不得等待网络（实际 {:?}）",
                started.elapsed()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn rejects_device_namespace_paths() {
        // 设备命名空间（命名管道、物理磁盘、GLOBALROOT）从来不是文件根：打开命名管道
        // 本身就有副作用（连到本机某个管道服务端），一律在触盘前拒绝。
        let sandbox = Sandbox::new();
        for requested in [
            r"\\.\pipe\mykvm-agent-test",
            r"\\.\PhysicalDrive0",
            r"\\?\GLOBALROOT\Device\Null",
            r"\\.\C:\Windows\win.ini",
        ] {
            assert_rejected(&sandbox.roots(), requested);
        }
    }

    #[cfg(windows)]
    #[test]
    fn case_variants_neither_escape_nor_break_matching_on_windows() {
        // Windows 文件系统大小写不敏感，而 Path::starts_with 按组件做大小写敏感比较：
        // 两侧都 canonicalize 成盘上真实大小写再比——换一种大小写书写的根内请求不得
        // 被误拒，也不得借大小写差异逃出根（如 C:\DEV 与 C:\dev）。
        let sandbox = Sandbox::new();
        let case_root = sandbox.base.join("CaseRoot");
        std::fs::create_dir_all(&case_root).expect("建大小写混合的根");
        std::fs::write(case_root.join("Inner.txt"), b"x").expect("写根内文件");
        std::fs::create_dir_all(sandbox.base.join("CaseRoot-evil")).expect("建兄弟目录");
        std::fs::write(sandbox.base.join("CaseRoot-evil").join("s.txt"), b"s").expect("写兄弟文件");
        let expected = canonical(&case_root.join("Inner.txt"));
        let base = text(&sandbox.base);
        let upper_root = PathBuf::from(format!(r"{base}\CASEROOT"));

        // 请求写成小写/大写变体：放行，且返回盘上真实大小写。
        for requested in [
            format!(r"{base}\caseroot\inner.txt"),
            format!(r"{base}\CASEROOT\INNER.TXT"),
        ] {
            assert_eq!(
                resolve_agent_readable_path(std::slice::from_ref(&case_root), &requested),
                Ok(expected.clone()),
                "请求 {requested}"
            );
        }
        // 根本身配置成另一种大小写：同样匹配。
        assert_eq!(
            resolve_agent_readable_path(
                std::slice::from_ref(&upper_root),
                &text(&case_root.join("Inner.txt"))
            ),
            Ok(expected)
        );
        // 换大小写也逃不出根。
        for requested in [
            format!(r"{base}\caseroot-EVIL\s.txt"),
            format!(r"{base}\CASEROOT\..\caseroot-evil\s.txt"),
        ] {
            assert_rejected(&[case_root.clone(), upper_root.clone()], &requested);
        }
    }

    #[cfg(windows)]
    #[test]
    fn verbatim_and_forward_slash_forms_inside_root_are_allowed() {
        // `\\?\` 前缀差异：canonicalize 统一返回 verbatim 形式，请求写成 verbatim
        // 或正斜杠都应解析到同一路径。
        let sandbox = Sandbox::new();
        let inner = sandbox.root.join("inner.txt");
        for requested in [
            format!(r"\\?\{}", text(&inner)),
            text(&inner).replace('\\', "/"),
        ] {
            assert_eq!(
                resolve_agent_readable_path(&sandbox.roots(), &requested),
                Ok(canonical(&inner)),
                "请求 {requested}"
            );
        }
    }

    #[test]
    fn relative_root_is_ignored() {
        // 根来自本机配置，但相对根会相对进程当前目录解析——macOS GUI 应用的当前
        // 目录是 `/`，相对根 "." 等于把整个文件系统设为可读。一律忽略（fail closed）。
        let cargo_toml = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml");
        assert_rejected(&[PathBuf::from(".")], &text(&cargo_toml));
    }

    #[test]
    fn rejects_everything_when_no_roots() {
        let sandbox = Sandbox::new();
        assert_rejected(&[], &text(&sandbox.root.join("inner.txt")));
    }

    // ---- 根目录来源 ----

    #[test]
    fn file_roots_prefer_sanitized_layout_config() {
        let config = AgentChannelConfig {
            file_roots: vec!["  /cfg/b ".into(), "/cfg/a".into(), "   ".into()],
            ..AgentChannelConfig::default()
        };
        assert_eq!(
            agent_file_roots(&config, &[PathBuf::from("/default")]),
            vec![PathBuf::from("/cfg/a"), PathBuf::from("/cfg/b")],
            "配置非空时用 sanitized 后的配置根，默认根不参与"
        );
    }

    #[test]
    fn file_roots_fall_back_to_injected_defaults_when_config_empty() {
        let defaults = vec![PathBuf::from("/logs"), PathBuf::from("/transfers")];
        assert_eq!(
            agent_file_roots(&AgentChannelConfig::default(), &defaults),
            defaults
        );
        let blank_only = AgentChannelConfig {
            file_roots: vec!["  ".into()],
            ..AgentChannelConfig::default()
        };
        assert_eq!(
            agent_file_roots(&blank_only, &defaults),
            defaults,
            "只有空白项的配置等同未配置"
        );
    }

    #[test]
    fn default_roots_are_log_dir_then_receive_root() {
        let log = PathBuf::from("/logs");
        let receive = PathBuf::from("/downloads/MyKVM Transfers");
        assert_eq!(
            default_agent_file_roots(Some(&log), Some(&receive)),
            vec![log.clone(), receive.clone()]
        );
        assert_eq!(
            default_agent_file_roots(None, Some(&receive)),
            vec![receive]
        );
        assert_eq!(default_agent_file_roots(Some(&log), None), vec![log]);
        assert!(default_agent_file_roots(None, None).is_empty());
    }
}
