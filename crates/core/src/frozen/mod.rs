//! Session-start frozen environment snapshot — the data sources for system prompt
//! segments [4] and [5].
//!
//! See docs/SYSTEM_PROMPT.md SS3.4 / SS3.5. All git subcommands have a 1.5 s timeout;
//! failures silently skip the related field. **Not refreshed during a session** —
//! mid-session git commits are not updated.

pub mod frontmatter;
pub mod git;
pub mod memory;
pub mod regex;
pub mod skill;
pub mod utils;

pub use self::frontmatter::split_frontmatter;
pub use self::memory::{find_relevant_memories, maybe_migrate_claude_to_atta, MemoryFileEntry};
pub use self::skill::{
    activate_conditional_skills, expand_skill_vars, load_session_skills,
    load_session_skills_with_bundled, load_skill_from_path, SkillEntry, SkillSource,
    try_expand_skill_command,
};

use self::memory::{collect_memory, collect_memory_files_with, load_all_memory_files};
use self::utils::truncate_chars;
use std::path::{Path, PathBuf};
use time::format_description::well_known::Iso8601;
use time::OffsetDateTime;

/// Git status output character limit.
const MAX_GIT_STATUS_CHARS: usize = 2000;

/// 会话级冻结的环境信息。一次会话只算一次；turn 之间不变。
#[derive(Debug, Clone, Default)]
pub struct FrozenContext {
    pub cwd: PathBuf,
    pub is_git: bool,
    pub is_worktree: bool,
    pub git_branch: Option<String>,
    pub git_main_branch: Option<String>,
    pub git_user_name: Option<String>,
    pub git_status: Option<String>,
    pub git_log: Option<String>,
    pub platform: String,
    pub shell: Option<String>,
    pub today: String,
    pub memory_blocks: Vec<MemoryFileEntry>,
    pub user_email: Option<String>,
    /// `~/.atta/code/skills/<name>/SKILL.md` + `<cwd>/.atta/code/skills/<name>/SKILL.md`
    /// 的 metadata 索引。只入 frontmatter（name/description/when_to_use）
    /// + 不嵌入 body / 不做参数代换 / 不做 slash 调用。模型能"知道"skills 存在 +
    ///   在合适时主动引用。
    pub skills: Vec<SkillEntry>,
    /// 跨会话 memory 目录（每个 cwd 一个）。位于
    /// `~/.atta/code/memory/<sha256(canonical_cwd)[..16]>/`。
    pub memory_dir: PathBuf,
    /// 上面目录里 MEMORY.md 的内容（仅当存在时加载）。注入 system prompt。
    pub memory_index: Option<String>,
    /// Topic memory files selected for the current user prompt. MEMORY.md is
    /// only an index; these files provide the matched details.
    pub relevant_memories: Vec<MemoryFileEntry>,
    /// Memory file paths already surfaced in prior turns. Used to deduplicate
    /// and avoid re-injecting the same memories every turn.
    pub already_surfaced: std::collections::HashSet<String>,
    /// **A-5 **: output style 已加载的内容（`name` 来自 EngineConfig，
    /// 由 `collect_output_style` 从 user/project 级别 `output-styles/<name>.md`
    /// 读取）。None = 没指定或文件不存在。
    pub output_style: Option<OutputStyle>,
}

/// **A-5 **: 一个 output-style 文件的加载结果。注入 system prompt 时用。
#[derive(Debug, Clone)]
pub struct OutputStyle {
    pub name: String,
    pub source: OutputStyleSource,
    pub content: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputStyleSource {
    /// `~/.atta/code/output-styles/<name>.md`
    User,
    /// `<cwd>/.atta/code/output-styles/<name>.md`
    Project,
}

/// FrozenContext 收集选项。
#[derive(Debug, Clone, Default)]
pub struct CollectOptions {
    /// 是否在 cwd 之上向上爬找 ATTA.md（含 git root 与
    /// `~/.atta/ATTA.md`）。默认 true。
    /// monorepo 子目录不想吃父级 monorepo 上下文时设为 false。
    pub walk_up_claude_md: bool,

    /// **A-5 **: 启动时按名称加载 output style（先项目级再用户级）。
    /// None = 不加载。
    pub output_style: Option<String>,
}

impl CollectOptions {
    pub fn defaults() -> Self {
        Self {
            walk_up_claude_md: true,
            output_style: None,
        }
    }
}

impl FrozenContext {
    /// 收集环境快照。所有 IO 错误都吞掉转 None / 默认值；这个函数不该 panic、
    /// 不该 fail -- 让上层 build_system_prompt 总能拿到一份合理的快照。
    pub async fn collect(cwd: PathBuf) -> Self {
        Self::collect_with_options(cwd, CollectOptions::defaults()).await
    }

    /// 带 options 的收集 -- `` 加 `walk_up_claude_md` 控制 monorepo 父级
    /// 上下文是否进 system prompt。
    pub async fn collect_with_options(cwd: PathBuf, opts: CollectOptions) -> Self {
        let cwd_clone = cwd.clone();

        // git 相关命令并发跑，每条独立超时
        let is_git = git::run_git_check(&cwd_clone).await;
        let is_worktree = if is_git { git::is_worktree(&cwd_clone).await } else { false };

        // 平台信息走 std + 环境变量，不阻塞
        let platform = std::env::consts::OS.to_string();
        let shell = std::env::var("SHELL").ok().map(|p| {
            Path::new(&p)
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or(p)
        });

        let today = OffsetDateTime::now_utc()
            .format(&Iso8601::DATE)
            .unwrap_or_else(|_| "unknown".to_string());

        // user email 仅在已是 git repo 时尝试 git config
        let user_email = if is_git {
            git::run_git_text(&cwd_clone, &["config", "user.email"]).await
        } else {
            None
        };

        let (git_branch, git_main_branch, git_user_name, git_status, git_log) = if is_git {
            let branch =
                git::run_git_text(&cwd_clone, &["symbolic-ref", "--short", "HEAD"]).await;
            let main = git::detect_main_branch(&cwd_clone).await;
            let user = git::run_git_text(&cwd_clone, &["config", "user.name"]).await;
            let status = git::run_git_text(
                &cwd_clone,
                &["--no-optional-locks", "status", "--short"],
            )
            .await
            .map(|s| truncate_chars(&s, MAX_GIT_STATUS_CHARS, "\n... (truncated)"));
            let log = git::run_git_text(
                &cwd_clone,
                &["--no-optional-locks", "log", "--oneline", "-n", "5"],
            )
            .await;
            (branch, main, user, status, log)
        } else {
            (None, None, None, None, None)
        };

        let memory_blocks = collect_memory_files_with(&cwd, opts.walk_up_claude_md).await;
        // P3c : 用 load_session_skills 而非 collect_skills -- 前者把 bundled
        // skills (simplify/verify/debug/batch/stuck) 也并入。否则
        // disk 没装 SKILL.md 时 system prompt 不暴露 bundled，模型不知道有这些
        // skill，/stuck /simplify 等 case 失败。
        let skills = load_session_skills(&cwd).await;

        // : memory_dir = ~/.atta/code/memory/<sha256(canonical_cwd)[..16]>/
        let (memory_dir, memory_index) = collect_memory(&cwd).await;

        // P1.5: Pre-load all memory files from the project memory dir so they
        // are available for injection into the system prompt. Files are capped
        // at 8KB each, up to 5 files (TS parity: max 5 files per turn).
        let relevant_memories = if memory_dir.exists() {
            load_all_memory_files(&memory_dir, 5).await
        } else {
            Vec::new()
        };

        // A-5 : output style -- load by name from project then user dir.
        let output_style = match opts.output_style.as_deref() {
            Some(name) if !name.trim().is_empty() => collect_output_style(&cwd, name).await,
            _ => None,
        };

        Self {
            cwd,
            is_git,
            is_worktree,
            git_branch,
            git_main_branch,
            git_user_name,
            git_status,
            git_log,
            platform,
            shell,
            today,
            memory_blocks,
            user_email,
            skills,
            memory_dir,
            memory_index,
            relevant_memories,
            already_surfaced: std::collections::HashSet::new(),
            output_style,
        }
    }
}

impl FrozenContext {
    pub fn with_relevant_memories(mut self, memories: Vec<MemoryFileEntry>) -> Self {
        self.relevant_memories = memories;
        self
    }
}

/// **A-5 **: load `<cwd>/.atta/code/output-styles/<name>.md` if present,
/// else `~/.atta/code/output-styles/<name>.md`. Trims whitespace and caps body
/// at 8KB. Returns None if neither file is present or readable.
async fn collect_output_style(cwd: &Path, name: &str) -> Option<OutputStyle> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    collect_output_style_with_home(cwd, name, home.as_deref()).await
}

/// Test-friendly variant taking the home dir explicitly. The public
/// `collect_output_style` resolves `HOME` from env; this lets tests pass an
/// arbitrary path without racing on the process-global env.
async fn collect_output_style_with_home(
    cwd: &Path,
    name: &str,
    home: Option<&Path>,
) -> Option<OutputStyle> {
    let safe = name.trim();
    if safe.is_empty() || safe.contains('/') || safe.contains('\\') || safe.starts_with('.') {
        return None;
    }
    let project = cwd
        .join(".atta")
        .join("code")
        .join("output-styles")
        .join(format!("{safe}.md"));
    if let Ok(content) = tokio::fs::read_to_string(&project).await {
        if !content.trim().is_empty() {
            return Some(OutputStyle {
                name: safe.to_string(),
                source: OutputStyleSource::Project,
                content: truncate_chars(content.trim(), 8_000, "\n... (output style truncated)"),
            });
        }
    }
    if let Some(home) = home {
        let user = home
            .join(".atta")
            .join("code")
            .join("output-styles")
            .join(format!("{safe}.md"));
        if let Ok(content) = tokio::fs::read_to_string(&user).await {
            if !content.trim().is_empty() {
                return Some(OutputStyle {
                    name: safe.to_string(),
                    source: OutputStyleSource::User,
                    content: truncate_chars(
                        content.trim(),
                        8_000,
                        "\n... (output style truncated)",
                    ),
                });
            }
        }
    }
    None
}

/// **A-5 **: list all output style names from user + project dirs (no
/// content read). Used by `/output-style` slash command.
pub async fn list_output_style_names(cwd: &Path) -> Vec<String> {
    let home = std::env::var_os("HOME").map(PathBuf::from);
    list_output_style_names_with_home(cwd, home.as_deref()).await
}

async fn list_output_style_names_with_home(cwd: &Path, home: Option<&Path>) -> Vec<String> {
    let mut all: Vec<(String, OutputStyleSource)> = Vec::new();
    if let Some(home) = home {
        all.extend(
            scan_output_style_dir(&home.join(".atta").join("code").join("output-styles"))
                .await
                .into_iter()
                .map(|n| (n, OutputStyleSource::User)),
        );
    }
    all.extend(
        scan_output_style_dir(&cwd.join(".atta").join("code").join("output-styles"))
            .await
            .into_iter()
            .map(|n| (n, OutputStyleSource::Project)),
    );
    // Dedup: keep last (project wins over user when names collide).
    let mut seen = std::collections::HashSet::new();
    let mut out: Vec<String> = Vec::new();
    for (name, _) in all.into_iter().rev() {
        if seen.insert(name.clone()) {
            out.push(name);
        }
    }
    out.reverse();
    out
}

async fn scan_output_style_dir(dir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut entries = match tokio::fs::read_dir(dir).await {
        Ok(e) => e,
        Err(_) => return out,
    };
    while let Ok(Some(e)) = entries.next_entry().await {
        let p = e.path();
        if p.extension().and_then(|s| s.to_str()) == Some("md") {
            if let Some(stem) = p.file_stem().and_then(|s| s.to_str()) {
                out.push(stem.to_string());
            }
        }
    }
    out.sort();
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn collects_basic_fields_for_arbitrary_dir() {
        let dir = TempDir::new().unwrap();
        let ctx = FrozenContext::collect(dir.path().to_path_buf()).await;
        assert!(!ctx.platform.is_empty());
        assert!(ctx.today.len() == 10); // YYYY-MM-DD
                                        // 不在 git 仓库下，is_git=false 且 git_* 字段都 None
        assert!(!ctx.is_git);
        assert!(ctx.git_status.is_none());
    }

    #[tokio::test]
    async fn detects_git_repo_in_a_real_repo() {
        // 当前 cwd 是 attacode 仓库本身，已经初始化了 git
        let pwd = std::env::current_dir().unwrap();
        let ctx = FrozenContext::collect(pwd).await;
        assert!(ctx.is_git, "expected attacode workspace to be inside git");
        assert!(ctx.git_branch.is_some());
    }

    #[tokio::test]
    async fn loads_claude_md_from_cwd() {
        let dir = TempDir::new().unwrap();
        let p = dir.path().join("ATTA.md");
        tokio::fs::write(&p, "# Test instructions\nbe concise.")
            .await
            .unwrap();
        let ctx = FrozenContext::collect(dir.path().to_path_buf()).await;
        assert_eq!(ctx.memory_blocks.len(), 1, "expected one ATTA.md");
        assert!(ctx.memory_blocks[0].content.contains("be concise"));
    }

    #[tokio::test]
    async fn loads_nested_claude_md_in_walk_up_order() {
        let dir = TempDir::new().unwrap();
        let parent = dir.path().join("p");
        let child = parent.join("c");
        tokio::fs::create_dir_all(&child).await.unwrap();
        tokio::fs::write(parent.join("ATTA.md"), "PARENT")
            .await
            .unwrap();
        tokio::fs::write(child.join("ATTA.md"), "CHILD")
            .await
            .unwrap();

        let ctx = FrozenContext::collect(child).await;
        // 应该顺序：parent 在前，child 在后（远到近）
        assert!(ctx.memory_blocks.len() >= 2);
        let parent_idx = ctx
            .memory_blocks
            .iter()
            .position(|e| e.content == "PARENT")
            .unwrap();
        let child_idx = ctx
            .memory_blocks
            .iter()
            .position(|e| e.content == "CHILD")
            .unwrap();
        assert!(parent_idx < child_idx, "parent must come before child");
    }

    // -----------------------------------------------------------------------
    // output_style
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn output_style_project_takes_precedence_over_user() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = home.path().join(".atta/code/output-styles");
        let proj_dir = cwd.path().join(".atta/code/output-styles");
        tokio::fs::create_dir_all(&user_dir).await.unwrap();
        tokio::fs::create_dir_all(&proj_dir).await.unwrap();
        tokio::fs::write(user_dir.join("terse.md"), "USER VERSION")
            .await
            .unwrap();
        tokio::fs::write(proj_dir.join("terse.md"), "PROJECT VERSION")
            .await
            .unwrap();

        let style = collect_output_style_with_home(cwd.path(), "terse", Some(home.path()))
            .await
            .unwrap();
        assert_eq!(style.name, "terse");
        assert_eq!(style.source, OutputStyleSource::Project);
        assert!(style.content.contains("PROJECT VERSION"));
    }

    #[tokio::test]
    async fn output_style_falls_back_to_user_when_no_project() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = home.path().join(".atta/code/output-styles");
        tokio::fs::create_dir_all(&user_dir).await.unwrap();
        tokio::fs::write(user_dir.join("verbose.md"), "explain everything")
            .await
            .unwrap();

        let style = collect_output_style_with_home(cwd.path(), "verbose", Some(home.path()))
            .await
            .unwrap();
        assert_eq!(style.source, OutputStyleSource::User);
        assert!(style.content.contains("explain everything"));
    }

    #[tokio::test]
    async fn output_style_returns_none_for_path_traversal_attempt() {
        let cwd = TempDir::new().unwrap();
        // names with slashes / leading dot must not load anything
        assert!(
            collect_output_style_with_home(cwd.path(), "../etc/passwd", None)
                .await
                .is_none()
        );
        assert!(collect_output_style_with_home(cwd.path(), ".hidden", None)
            .await
            .is_none());
        assert!(collect_output_style_with_home(cwd.path(), "", None)
            .await
            .is_none());
    }

    #[tokio::test]
    async fn list_output_style_names_dedups_user_and_project() {
        let home = TempDir::new().unwrap();
        let cwd = TempDir::new().unwrap();
        let user_dir = home.path().join(".atta/code/output-styles");
        let proj_dir = cwd.path().join(".atta/code/output-styles");
        tokio::fs::create_dir_all(&user_dir).await.unwrap();
        tokio::fs::create_dir_all(&proj_dir).await.unwrap();
        tokio::fs::write(user_dir.join("terse.md"), "x")
            .await
            .unwrap();
        tokio::fs::write(user_dir.join("verbose.md"), "x")
            .await
            .unwrap();
        tokio::fs::write(proj_dir.join("terse.md"), "y")
            .await
            .unwrap();
        tokio::fs::write(proj_dir.join("local.md"), "y")
            .await
            .unwrap();

        let names = list_output_style_names_with_home(cwd.path(), Some(home.path())).await;
        // expected: terse (deduped), verbose (user), local (project)
        let mut sorted = names.clone();
        sorted.sort();
        assert_eq!(sorted, vec!["local", "terse", "verbose"]);
    }
}
