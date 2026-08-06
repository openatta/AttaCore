//! 平台沙盒包装。把 `bash -c <cmd>` 之外裹一层平台沙盒，限制写权限到 cwd /
//! additional 子树以内。
//!
//! - **macOS**：`sandbox-exec -p <profile> bash -c <cmd>`，profile 用 TinyScheme
//!   拒写非允许路径。exec 不限制。网络默认放行（见 [`NetworkMode`]），可选
//!   `DenyAll`/`Allowlist`——两者都用 `(deny network*)`，Allowlist 再逐域名
//!   `(allow network-outbound (remote tcp "domain:*"))` 放行；DNS 解析走
//!   `mDNSResponder` IPC，不受 `network*` 过滤影响，两种受限模式下都还能用。
//! - **Linux**：`bwrap --ro-bind / / --bind <writable> <writable> ... bash -c <cmd>`。
//!   bwrap 不在 PATH → 降级（无沙盒）+ warn。网络：`DenyAll` 用 `--unshare-net`
//!   （新网络命名空间，只剩 loopback）；bwrap 没有域名/IP 级过滤能力，
//!   `Allowlist` 在这个平台上退化成同样的 `--unshare-net`（+ warn 日志）而不是
//!   静默放开成 Unrestricted——失败要往"更安全"的方向失败。
//! - **Windows**：不做；`dangerously_disable_sandbox` 自动开（warn 一次）。
//!
//! 受 `EngineConfig::dangerously_disable_sandbox` 控制；为 true 直跑无沙盒。
//!
//! `SandboxPolicy`（含 `network_mode`/`allowed_domains`）目前只从
//! `bash.rs::to_sandbox_policy()` 转换 `ToolContext.sandbox` 得到——而
//! `ToolContext.sandbox` 本身在所有构造点都还是 `Default::default()`（没有
//! settings.json → EngineConfig → ToolContext 这一段的接线），所以在真实
//! settings.json 里配置 `sandbox.network_mode` 目前还不会生效。这个模块内的
//! 逻辑（profile 生成）是完整、经测试的；缺的是更上游的配置读取，不在本次
//! 改动范围内。
//!
//! # 已知沙盒逃逸风险
//!
//! 本沙盒是**轻量写限制**，不是安全隔离。已知逃逸路径：
//!
//! 1. **`/dev` 写权限**：macOS profile 放行了 `file-write*` 到 `/dev`，进程可
//!    `mknod` 创建 raw 磁盘设备节点绕过文件 ACL。
//! 2. **环境变量继承**：`LD_PRELOAD`/`DYLD_INSERT_LIBRARIES` 未清除，可注入动态库。
//! 3. **`/proc` 泄露**：bwrap 挂载了宿主的 `/proc`，可通过 `/proc/self/{fd,root}`
//!    访问宿主文件系统 / fd。
//! 4. **Open fd 泄露**：未设 `CLOEXEC`，子进程可读取继承的 fd（git 仓库、socket 等）。
//!
//! 强化方向见 `HARDENING.md`。

use std::path::{Path, PathBuf};

/// The closed set of scenes this engine registers, mirrored here so the
/// sandbox can protect every scene's user-level `settings.json` without
/// needing to know which one the current session actually uses (`tools`
/// doesn't depend on the `scene` crate). Keep in sync with
/// `daemon::main::resolve_scene` and `crates/scene/src/scene/*.rs`.
const KNOWN_SCENES: &[&str] = &["coding", "chat", "demo", "research"];

/// 沙盒包装结果：直接喂给 `tokio::process::Command::new(prog).args(args)`。
#[derive(Debug, Clone)]
pub struct SandboxedCommand {
    pub program: String,
    pub args: Vec<String>,
    /// 选定的沙盒模式（仅给 logging / 测试用）
    pub mode: SandboxMode,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SandboxMode {
    /// 用户显式禁用 / 配置关闭
    Disabled,
    /// 平台不支持（Windows / 缺 bwrap）
    Unavailable,
    /// macOS sandbox-exec
    MacOSSandboxExec,
    /// Linux bubblewrap
    LinuxBwrap,
}

#[derive(Debug, Clone)]
pub struct SandboxOptions<'a> {
    pub command: &'a str,
    pub cwd: &'a Path,
    pub additional_writable: &'a [PathBuf],
    pub disable: bool,
    /// **Hardening **: extended policy (deny-read, network mode, etc).
    /// Falls back to safe defaults via `SandboxPolicy::default()`.
    pub policy: SandboxPolicy,
}

/// **Hardening **: declarative sandbox policy. Source can be settings.json
/// `sandbox.*`. Defaults bake in a sensible deny-read list (~/.ssh, ~/.aws,
/// etc) so naive Bash commands don't dump credential files into the model's
/// transcript by accident.
///
/// Linux note: not all features land cleanly on bwrap — see field docs.
#[derive(Debug, Clone, Default)]
pub struct SandboxPolicy {
    /// Absolute paths the sandbox is allowed to **read**, on top of the
    /// universal default (everything readable). When non-empty, paths in
    /// `deny_read` matching these get re-allowed (most-specific wins).
    pub allow_read: Vec<PathBuf>,
    /// Absolute paths the sandbox is **denied** read access to. Defaults
    /// include common credential stores (see [`default_deny_read`]).
    pub deny_read: Vec<PathBuf>,
    /// Network policy — see [`NetworkMode`].
    pub network_mode: NetworkMode,
    /// Domains allowed to make outbound connections when `network_mode` is
    /// [`NetworkMode::Allowlist`]. Ignored otherwise. Matched literally
    /// against the connection target's hostname (no wildcards) — see
    /// `build_macos_profile`'s network section for the exact rule shape.
    pub allowed_domains: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum NetworkMode {
    /// Unrestricted outbound (default; Bash often needs `curl` / `npm install`).
    #[default]
    Unrestricted,
    /// No outbound network access at all (DNS resolution via the OS
    /// resolver still works on macOS — it's IPC to `mDNSResponder`, not a
    /// raw socket the sandbox profile's `network*` filter covers — but no
    /// TCP/UDP connection actually reaching the network is permitted).
    DenyAll,
    /// Outbound TCP restricted to `SandboxPolicy::allowed_domains`; DNS
    /// resolution still works (same rationale as `DenyAll`). An empty
    /// `allowed_domains` list under this mode is equivalent to `DenyAll`.
    Allowlist,
}

/// Default deny-read paths — credential stores that almost never want to be
/// inside an LLM tool result. User can override via `sandbox.allow_read` in
/// settings.json. Returned absolute (`HOME` resolved).
#[allow(dead_code)]
pub fn default_deny_read() -> Vec<PathBuf> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    let mut out = Vec::new();
    let push = |out: &mut Vec<PathBuf>, p: &str| {
        if let Some(home) = &home {
            out.push(home.join(p));
        }
    };
    push(&mut out, ".ssh");
    push(&mut out, ".aws");
    push(&mut out, ".gnupg");
    push(&mut out, ".docker/config.json");
    push(&mut out, ".kube");
    push(&mut out, ".azure");
    push(&mut out, ".config/gh");
    push(&mut out, ".netrc");
    push(&mut out, ".npmrc");
    push(&mut out, ".pypirc");
    push(&mut out, ".gem/credentials");
    out
}

/// 主入口：选用合适的平台沙盒包装命令。
///
/// 不会 panic、不会失败 —— 沙盒不可用就 fall back 到无沙盒（mode=Unavailable）。
/// 上层 BashTool 拿到 SandboxedCommand 后照常 spawn。
pub fn wrap(opts: SandboxOptions<'_>) -> SandboxedCommand {
    if opts.disable {
        return plain(opts.command, SandboxMode::Disabled);
    }

    #[cfg(target_os = "macos")]
    {
        mac_wrap(opts)
    }

    #[cfg(target_os = "linux")]
    {
        linux_wrap(opts)
    }

    // Windows / other unsupported platforms
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        plain(opts.command, SandboxMode::Unavailable)
    }
}

fn plain(command: &str, mode: SandboxMode) -> SandboxedCommand {
    SandboxedCommand {
        program: "bash".into(),
        args: vec!["-c".into(), command.into()],
        mode,
    }
}

#[cfg(target_os = "macos")]
fn mac_wrap(opts: SandboxOptions<'_>) -> SandboxedCommand {
    let profile = build_macos_profile(opts.cwd, opts.additional_writable, &opts.policy);
    SandboxedCommand {
        program: "sandbox-exec".into(),
        args: vec![
            "-p".into(),
            profile,
            "bash".into(),
            "-c".into(),
            opts.command.to_string(),
        ],
        mode: SandboxMode::MacOSSandboxExec,
    }
}

#[cfg(target_os = "macos")]
fn build_macos_profile(cwd: &Path, additional: &[PathBuf], policy: &SandboxPolicy) -> String {
    // 默认放行（所有 file-read / process / network / signal / mach 等），
    // 然后单独 deny file-write*，再 allow 我们指定的几个 subpath。
    //
    // /private/tmp 与 /private/var/folders 是 macOS 临时区域；很多工具会写到那里。
    // 不放它们，rm/build/git 之类的会被杀。
    let mut s = String::with_capacity(1024);
    s.push_str("(version 1)\n");
    s.push_str("(allow default)\n");

    // ---- write policy ----
    s.push_str("(deny file-write*)\n");
    s.push_str("(allow file-write*\n");
    s.push_str(&format!(
        "  (subpath \"{}\")\n",
        sandbox_escape(&cwd.display().to_string())
    ));
    s.push_str("  (subpath \"/private/tmp\")\n");
    s.push_str("  (subpath \"/private/var/folders\")\n");
    s.push_str("  (subpath \"/private/var/tmp\")\n");
    s.push_str("  (subpath \"/dev\")\n");
    for p in additional {
        s.push_str(&format!(
            "  (subpath \"{}\")\n",
            sandbox_escape(&p.display().to_string())
        ));
    }
    s.push_str(")\n");
    // **Q4-followup **: re-deny writes to settings.json files even though
    // they sit inside cwd. Stops Bash-driven sandbox escapes via attacode
    // overwriting its own permission rules. Aligns with TS sandbox-adapter.ts.
    // Project-level settings.json is cwd-relative and flat (no scope
    // segment) — see docs/design/2026-08-03-agents-config-migration.md.
    let cwd_str = cwd.display().to_string();
    s.push_str(&format!(
        "(deny file-write* (literal \"{}/.atta/settings.json\"))\n",
        sandbox_escape(&cwd_str)
    ));
    s.push_str(&format!(
        "(deny file-write* (literal \"{}/.atta/settings.local.json\"))\n",
        sandbox_escape(&cwd_str)
    ));
    if let Some(home) = std::env::var_os("HOME") {
        let home_str = std::path::Path::new(&home).display().to_string();
        // Cross-scene global settings.json — flat, single file (2026-08-04
        // personal-`.atta`-flattening round, see
        // `docs/design/2026-08-03-agents-config-migration.md` §10).
        s.push_str(&format!(
            "(deny file-write* (literal \"{}/.atta/settings.json\"))\n",
            sandbox_escape(&home_str)
        ));
        // Scene-specific settings.json lives at
        // `~/.atta/scenes/<scene>/settings.json`, where `<scene>` is one of
        // the small, closed set of scenes this engine registers (see
        // `daemon::resolve_scene` / `KNOWN_SCENES` below) — not an arbitrary
        // string, so we can just enumerate all of them rather than needing
        // to know which one the current session actually uses.
        for scene in KNOWN_SCENES {
            s.push_str(&format!(
                "(deny file-write* (literal \"{}/.atta/scenes/{}/settings.json\"))\n",
                sandbox_escape(&home_str),
                scene
            ));
        }
    }

    // ---- **Hardening **: deny-read for credential paths ----
    if !policy.deny_read.is_empty() {
        for p in &policy.deny_read {
            // Use subpath so /aws/ children are also denied.
            s.push_str(&format!(
                "(deny file-read* (subpath \"{}\"))\n",
                sandbox_escape(&p.display().to_string())
            ));
        }
        // Re-allow specific entries the user explicitly opted back in via
        // sandbox.allow_read. macOS sandbox-exec evaluates rules top-to-bottom
        // so allows AFTER denies win.
        for p in &policy.allow_read {
            s.push_str(&format!(
                "(allow file-read* (subpath \"{}\"))\n",
                sandbox_escape(&p.display().to_string())
            ));
        }
    }

    // ---- **Hardening **: network policy ----
    // `(allow default)` at the top of the profile already covers unrestricted
    // networking — Unrestricted needs no extra rules. DenyAll/Allowlist add a
    // blanket `(deny network*)` (does not affect DNS: that's IPC to
    // mDNSResponder on macOS, not covered by the `network*` filter category),
    // then Allowlist re-allows outbound TCP to each configured domain —
    // sandbox-exec's `remote tcp` filter matches by hostname, not just IP.
    match policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::DenyAll => {
            s.push_str("(deny network*)\n");
        }
        NetworkMode::Allowlist => {
            s.push_str("(deny network*)\n");
            for domain in &policy.allowed_domains {
                s.push_str(&format!(
                    "(allow network-outbound (remote tcp \"{}:*\"))\n",
                    sandbox_escape(domain)
                ));
            }
        }
    }

    s
}

/// macOS sandbox-exec 的 TinyScheme 字符串里 `\` 和 `"` 要转义。
fn sandbox_escape(s: &str) -> String {
    s.replace('\\', "\\\\").replace('"', "\\\"")
}

#[cfg(target_os = "linux")]
fn linux_wrap(opts: SandboxOptions<'_>) -> SandboxedCommand {
    if !bwrap_available() {
        return plain(opts.command, SandboxMode::Unavailable);
    }
    let mut args: Vec<String> = vec![
        // 文件系统：root 只读，cwd / tmp 可写
        "--ro-bind".into(),
        "/".into(),
        "/".into(),
        "--bind".into(),
        opts.cwd.display().to_string(),
        opts.cwd.display().to_string(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--unshare-pid".into(),
        "--die-with-parent".into(),
    ];
    for p in opts.additional_writable {
        args.push("--bind".into());
        let s = p.display().to_string();
        args.push(s.clone());
        args.push(s);
    }

    // **Hardening **: re-deny writes to settings.json even though it sits
    // inside `cwd`'s read-write bind — same rationale/paths as
    // `build_macos_profile`'s deny-write rules (stops a sandboxed Bash
    // command from overwriting its own permission rules, or injecting a
    // malicious multi-provider LLM config with an attacker-controlled
    // `base_url`/`api_key`). bwrap applies binds in argument order, so a
    // `--ro-bind-try` here remounts just this one path read-only on top of
    // the read-write `cwd` bind above — `--ro-bind-try` is a no-op (not an
    // error) when the source doesn't exist yet.
    let cwd_str = opts.cwd.display().to_string();
    for name in ["settings.json", "settings.local.json"] {
        let p = format!("{cwd_str}/.atta/{name}");
        args.push("--ro-bind-try".into());
        args.push(p.clone());
        args.push(p);
    }
    // Global/scene settings.json normally already sit outside any writable
    // bind (the read-only `/` bind above covers all of `$HOME` unless `cwd`
    // or an `additional_writable` entry happens to overlap it) — these are
    // defense-in-depth for that overlap case, mirroring the macOS profile's
    // unconditional deny rules for the same paths.
    if let Some(home) = std::env::var_os("HOME") {
        let home_str = std::path::Path::new(&home).display().to_string();
        let global_settings = format!("{home_str}/.atta/settings.json");
        args.push("--ro-bind-try".into());
        args.push(global_settings.clone());
        args.push(global_settings);
        for scene in KNOWN_SCENES {
            let p = format!("{home_str}/.atta/scenes/{scene}/settings.json");
            args.push("--ro-bind-try".into());
            args.push(p.clone());
            args.push(p);
        }
    }

    // **Hardening **: deny-read via tmpfs over the path. Each entry gets
    // mounted as an empty tmpfs so reads return ENOENT-equivalent. Files are
    // overlaid by binding /dev/null. allow_read entries skip overlay.
    let allow_set: std::collections::HashSet<PathBuf> =
        opts.policy.allow_read.iter().cloned().collect();
    for p in &opts.policy.deny_read {
        if allow_set.contains(p) {
            continue;
        }
        let s = p.display().to_string();
        // Best-effort: if the path doesn't exist, skip — bwrap would error.
        if !p.exists() {
            continue;
        }
        if p.is_dir() {
            args.push("--tmpfs".into());
            args.push(s);
        } else {
            args.push("--ro-bind-try".into());
            args.push("/dev/null".into());
            args.push(s);
        }
    }

    // **Hardening **: network policy. bwrap has no domain/IP-level filter —
    // its only lever is `--unshare-net` (new network namespace with nothing
    // but loopback, i.e. total isolation). DenyAll maps onto that directly.
    // Allowlist can't be honored precisely on this platform (no local proxy
    // / iptables setup here), so it degrades to the same full isolation
    // rather than silently falling back to Unrestricted — consistent with
    // this crate's "safe by default, opt into less safety not more"
    // principle (README §Design Principles): failing closed on an
    // unsupported restriction is safer than failing open.
    match opts.policy.network_mode {
        NetworkMode::Unrestricted => {}
        NetworkMode::DenyAll => {
            args.push("--unshare-net".into());
        }
        NetworkMode::Allowlist => {
            tracing::warn!(
                domains = ?opts.policy.allowed_domains,
                "sandbox: NetworkMode::Allowlist has no bwrap equivalent (no domain/IP \
                 filtering primitive) — falling back to full network isolation \
                 (--unshare-net) rather than allowing unrestricted access"
            );
            args.push("--unshare-net".into());
        }
    }

    args.push("--".into());
    args.push("bash".into());
    args.push("-c".into());
    args.push(opts.command.to_string());

    SandboxedCommand {
        program: "bwrap".into(),
        args,
        mode: SandboxMode::LinuxBwrap,
    }
}

/// 一行可读的沙盒可用性描述，给 `/doctor` 用。
/// macOS：恒"available: sandbox-exec"（系统自带）。
/// Linux：bwrap 在 PATH 时 available；否则报 unavailable + 提示装。
/// 其它平台：unavailable + 平台名。
pub fn sandbox_status() -> String {
    #[cfg(target_os = "macos")]
    {
        "available: sandbox-exec".to_string()
    }
    #[cfg(target_os = "linux")]
    {
        if bwrap_available() {
            "available: bwrap".to_string()
        } else {
            "unavailable: bwrap not in PATH (install bubblewrap)".to_string()
        }
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        format!("unavailable on {}", std::env::consts::OS)
    }
}

#[cfg(target_os = "linux")]
fn bwrap_available() -> bool {
    // 一次性探测；用 sync std::process::Command 走 PATH lookup
    static AVAILABLE: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *AVAILABLE.get_or_init(|| {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts<'a>(cmd: &'a str, cwd: &'a Path) -> SandboxOptions<'a> {
        SandboxOptions {
            command: cmd,
            cwd,
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        }
    }

    #[test]
    fn disable_yields_plain_bash() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.disable = true;
        let cmd = wrap(o);
        assert_eq!(cmd.program, "bash");
        assert_eq!(cmd.args[0], "-c");
        assert_eq!(cmd.args[1], "ls");
        assert_eq!(cmd.mode, SandboxMode::Disabled);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_wraps_with_sandbox_exec_and_profile_includes_cwd() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        assert_eq!(cmd.program, "sandbox-exec");
        assert_eq!(cmd.args[0], "-p");
        assert!(cmd.args[1].contains("(deny file-write*)"));
        assert!(cmd.args[1].contains("(subpath \"/tmp/work\")"));
        assert!(cmd.args[1].contains("(subpath \"/private/tmp\")"));
        assert_eq!(cmd.args[2], "bash");
        assert_eq!(cmd.args[3], "-c");
        assert_eq!(cmd.args[4], "ls");
        assert_eq!(cmd.mode, SandboxMode::MacOSSandboxExec);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_additional_writable_dirs_added_to_profile() {
        let extras = vec![
            PathBuf::from("/Users/me/scratch"),
            PathBuf::from("/Users/me/another"),
        ];
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.additional_writable = &extras;
        let cmd = wrap(o);
        assert!(cmd.args[1].contains("(subpath \"/Users/me/scratch\")"));
        assert!(cmd.args[1].contains("(subpath \"/Users/me/another\")"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_path_with_quotes_is_escaped() {
        let cmd = wrap(opts("ls", Path::new("/tmp/with\"quote")));
        // 反斜杠转义后 sandbox-exec 仍然能 parse
        assert!(cmd.args[1].contains("with\\\"quote"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_either_uses_bwrap_or_falls_back() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        match cmd.mode {
            SandboxMode::LinuxBwrap => {
                assert_eq!(cmd.program, "bwrap");
                assert!(cmd.args.iter().any(|a| a == "--ro-bind"));
                assert!(cmd.args.iter().any(|a| a == "/tmp/work"));
                let bash_pos = cmd
                    .args
                    .iter()
                    .position(|a| a == "bash")
                    .expect("bwrap args must end with `-- bash -c <cmd>`");
                assert_eq!(cmd.args[bash_pos + 1], "-c");
                assert_eq!(cmd.args[bash_pos + 2], "ls");
            }
            SandboxMode::Unavailable => {
                // bwrap 没装 —— 平台合理 fallback
                assert_eq!(cmd.program, "bash");
                assert_eq!(cmd.args, vec!["-c", "ls"]);
            }
            other => panic!("unexpected mode on linux: {other:?}"),
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_re_denies_writes_to_project_settings_json() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        if cmd.mode != SandboxMode::LinuxBwrap {
            return; // bwrap not installed in this environment — nothing to assert
        }
        let needle = "/tmp/work/.atta/settings.json".to_string();
        let pos = cmd
            .args
            .iter()
            .position(|a| a == &needle)
            .expect("expected a --ro-bind-try re-mount for the project settings.json");
        assert_eq!(cmd.args[pos - 1], "--ro-bind-try");
        // Source == dest for a self-remount, and settings.local.json gets the same treatment.
        assert_eq!(cmd.args[pos + 1], needle);
        assert!(cmd
            .args
            .iter()
            .any(|a| a == "/tmp/work/.atta/settings.local.json"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_wrap_re_denies_writes_to_global_and_scene_settings_json() {
        let cmd = wrap(opts("ls", Path::new("/tmp/work")));
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home_str = std::path::Path::new(&home).display().to_string();
        assert!(cmd
            .args
            .iter()
            .any(|a| a == &format!("{home_str}/.atta/settings.json")));
        for scene in KNOWN_SCENES {
            let needle = format!("{home_str}/.atta/scenes/{scene}/settings.json");
            assert!(
                cmd.args.iter().any(|a| a == &needle),
                "expected a --ro-bind-try re-mount for scene `{scene}`'s settings.json"
            );
        }
    }

    #[test]
    fn sandbox_escape_handles_quotes_and_backslash() {
        assert_eq!(sandbox_escape("/tmp/normal"), "/tmp/normal");
        assert_eq!(sandbox_escape("/tmp/q\"quote"), "/tmp/q\\\"quote");
        assert_eq!(sandbox_escape("/tmp/back\\slash"), "/tmp/back\\\\slash");
    }

    // ---- **Hardening **: policy tests ----

    #[test]
    fn default_deny_read_includes_credential_paths() {
        // Only meaningful when HOME is set; in CI it usually is. Skip if not.
        let Some(_home) = std::env::var_os("HOME") else {
            return;
        };
        let list = default_deny_read();
        let joined: String = list
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join("|");
        assert!(joined.contains(".ssh"));
        assert!(joined.contains(".aws"));
        assert!(joined.contains(".gnupg"));
        assert!(joined.contains(".kube"));
        assert!(joined.contains("gh"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_emits_deny_read_for_default_policy() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy = SandboxPolicy {
            allow_read: Vec::new(),
            deny_read: default_deny_read(),
            network_mode: NetworkMode::Unrestricted,
            allowed_domains: Vec::new(),
        };
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        // Should at least mention .ssh in the deny-read list
        assert!(profile.contains("(deny file-read*"));
        assert!(profile.contains(".ssh"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allow_read_overrides_deny_read() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy = SandboxPolicy {
            allow_read: vec![PathBuf::from("/tmp/some-secret")],
            deny_read: vec![PathBuf::from("/tmp/some-secret")],
            network_mode: NetworkMode::Unrestricted,
            allowed_domains: vec![],
        };
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        // Both rules emitted; allow comes after deny so it wins per
        // sandbox-exec evaluation order.
        let deny_idx = profile
            .find("(deny file-read* (subpath \"/tmp/some-secret\"))")
            .unwrap();
        let allow_idx = profile
            .find("(allow file-read* (subpath \"/tmp/some-secret\"))")
            .unwrap();
        assert!(allow_idx > deny_idx);
    }

    // ---- network policy ----

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_unrestricted_network_emits_no_network_rules() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Unrestricted;
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert!(
            !profile.contains("network"),
            "Unrestricted must not add any network rule, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_deny_all_network_emits_deny_network_rule() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::DenyAll;
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(
            !profile.contains("network-outbound"),
            "DenyAll must not also emit an allowlist rule, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allowlist_network_denies_then_allows_each_domain() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["api.example.com".into(), "registry.npmjs.org".into()];
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(
            profile.contains("(allow network-outbound (remote tcp \"api.example.com:*\"))"),
            "profile:\n{profile}"
        );
        assert!(
            profile.contains("(allow network-outbound (remote tcp \"registry.npmjs.org:*\"))"),
            "profile:\n{profile}"
        );
        // Both allow rules must come after the deny for sandbox-exec's
        // top-to-bottom evaluation order to actually re-permit them.
        let deny_idx = profile.find("(deny network*)").unwrap();
        let allow_idx = profile.find("(allow network-outbound").unwrap();
        assert!(allow_idx > deny_idx);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_allowlist_with_no_domains_still_denies_network() {
        // Empty allowed_domains under Allowlist == DenyAll in effect (no
        // allow rules to re-permit anything).
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec![];
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert!(profile.contains("(deny network*)"), "profile:\n{profile}");
        assert!(!profile.contains("network-outbound"), "profile:\n{profile}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_network_domain_is_escaped() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["evil\".com".into()];
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert!(profile.contains("evil\\\".com"), "profile:\n{profile}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_network_rules_keep_profile_parens_balanced() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["a.example.com".into(), "b.example.com".into()];
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert_eq!(profile.matches('(').count(), profile.matches(')').count());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_deny_all_network_uses_unshare_net() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::DenyAll;
        let cmd = wrap(o);
        if cmd.mode != SandboxMode::LinuxBwrap {
            return; // bwrap not installed in this environment
        }
        assert!(cmd.args.iter().any(|a| a == "--unshare-net"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_unrestricted_network_omits_unshare_net() {
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Unrestricted;
        let cmd = wrap(o);
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        assert!(!cmd.args.iter().any(|a| a == "--unshare-net"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_allowlist_network_falls_back_to_unshare_net() {
        // bwrap has no domain-level filter — Allowlist must fail closed
        // (full isolation), not silently degrade to Unrestricted.
        let mut o = opts("ls", Path::new("/tmp/work"));
        o.policy.network_mode = NetworkMode::Allowlist;
        o.policy.allowed_domains = vec!["api.example.com".into()];
        let cmd = wrap(o);
        if cmd.mode != SandboxMode::LinuxBwrap {
            return;
        }
        assert!(cmd.args.iter().any(|a| a == "--unshare-net"));
    }

    // ---- Phase 3-3: fault injection tests ----

    #[test]
    fn sandbox_escape_handles_special_whitespace() {
        // sandbox_escape only escapes `\` and `"`. Newlines/tabs pass through
        // unmodified — the test just verifies no panic or corruption.
        let with_nl = sandbox_escape("/path/with\nnewline");
        assert!(with_nl.contains('\n'), "newline passes through unescaped");
        assert!(with_nl.contains("newline"));

        let with_tab = sandbox_escape("/path/with\t tab");
        assert!(with_tab.contains('\t'), "tab passes through unescaped");

        // Double-quote still gets escaped even when surrounded by whitespace
        assert_eq!(sandbox_escape("\" start"), "\\\" start");
    }

    #[test]
    fn sandbox_escape_handles_unicode_and_long_paths() {
        // Unicode and deeply nested paths that could trigger buffer issues.
        let unicode = "/tmp/日本語/パス";
        let escaped = sandbox_escape(unicode);
        assert_eq!(escaped, unicode); // no escaping needed
        let long = "/".repeat(1000) + "a";
        let escaped = sandbox_escape(&long);
        assert!(escaped.len() > 500);
        assert_eq!(escaped, long);
    }

    #[test]
    fn wrap_never_panics_on_edge_case_inputs() {
        // wrap() must always return a valid SandboxedCommand regardless
        // of unusual inputs — the contract is "never fails, never panics".
        let empty_opts = SandboxOptions {
            command: "",
            cwd: Path::new(""),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd = wrap(empty_opts);
        assert!(!cmd.program.is_empty());
        assert!(!cmd.args.is_empty());

        // Unicode command with no cwd
        let unicode_opts = SandboxOptions {
            command: "echo 🦀",
            cwd: Path::new("/"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd2 = wrap(unicode_opts);
        assert!(cmd2.args.iter().any(|a| a.contains("🦀")));

        // Explicit disable must always yield plain bash
        let disabled_opts = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp"),
            additional_writable: &[],
            disable: true,
            policy: SandboxPolicy::default(),
        };
        let cmd3 = wrap(disabled_opts);
        assert_eq!(cmd3.program, "bash");
        assert_eq!(cmd3.mode, SandboxMode::Disabled);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_has_balanced_parentheses() {
        // Structural sanity check: the TinyScheme profile must have matching
        // open/close paren counts so sandbox-exec(1) can parse it.
        let o = SandboxOptions {
            command: "echo hi",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[PathBuf::from("/Users/me/scratch")],
            disable: false,
            policy: SandboxPolicy {
                allow_read: Vec::new(),
                deny_read: default_deny_read(),
                network_mode: NetworkMode::Unrestricted,
                allowed_domains: Vec::new(),
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        let opens = profile.matches('(').count();
        let closes = profile.matches(')').count();
        assert_eq!(
            opens, closes,
            "macOS sandbox profile must have balanced parens"
        );
        // Must start with (version 1) per TinyScheme sandbox-exec convention
        assert!(
            profile.starts_with("(version 1)"),
            "profile must start with (version 1)"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_denies_settings_json_even_without_deny_read() {
        // Settings.json write denial must be emitted unconditionally,
        // regardless of whether credential deny-read paths are configured.
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy {
                allow_read: vec![],
                deny_read: vec![],
                network_mode: NetworkMode::Unrestricted,
                allowed_domains: vec![],
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        // Must still deny writes to .atta/settings.json even when deny_read is empty
        assert!(
            profile.contains("(deny file-write* (literal \"/tmp/work/.atta/settings.json\"))"),
            "settings.json denial must appear unconditionally"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_protects_user_level_settings_json_for_every_known_scene() {
        // The sandbox doesn't know which scene the current session uses (no
        // plumbing for that in `ToolContext`), so it must protect all of
        // them — not just a single hardcoded one.
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert!(
            !KNOWN_SCENES.is_empty(),
            "sanity: scene list must not be empty"
        );
        for scene in KNOWN_SCENES {
            let needle = format!(".atta/scenes/{scene}/settings.json\"))");
            assert!(
                profile.contains(&needle),
                "expected a deny-write rule for scene `{scene}`'s settings.json, profile:\n{profile}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_protects_global_settings_json() {
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy::default(),
        };
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        let Some(home) = std::env::var_os("HOME") else {
            return;
        };
        let home_str = std::path::Path::new(&home).display().to_string();
        let needle = format!("(deny file-write* (literal \"{home_str}/.atta/settings.json\"))");
        assert!(
            profile.contains(&needle),
            "expected a deny-write rule for the cross-scene global settings.json, profile:\n{profile}"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn macos_profile_empty_deny_read_skips_read_rules_entirely() {
        // When deny_read is empty, no file-read* deny or allow rules should
        // appear in the profile (structural optimization).
        let o = SandboxOptions {
            command: "ls",
            cwd: Path::new("/tmp/work"),
            additional_writable: &[],
            disable: false,
            policy: SandboxPolicy {
                allow_read: vec![],
                deny_read: vec![],
                network_mode: NetworkMode::Unrestricted,
                allowed_domains: vec![],
            },
        };
        let cmd = wrap(o);
        let profile = &cmd.args[1];
        assert!(
            !profile.contains("(deny file-read*"),
            "no deny-read rules when deny_read is empty"
        );
    }
}
