//! Shell-specific safety checks — detect dangerous patterns in shell commands.
//!
//! Patterns detected:
#![allow(dead_code)]
//! - `rm -rf` on system paths (root, /etc, /usr, /home, etc.)
//! - `rm -rf` with wildcards (*) in current directory
//! - `git push --force` (suggests --force-with-lease as safer alternative)
//! - `chmod 777` / `chmod -R 777` (permission escalation warnings)
//!
//! Each check returns a `SafetyWarning` with severity level:
//! - `Info` — informational, no action needed
//! - `Warning` — should prompt in Plan mode, ask in Default mode
//! - `Critical` — requires explicit confirmation regardless of mode

/// Severity level for a safety warning.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Severity {
    /// Informational — just flag it, no action required.
    Info,
    /// Should block in plan mode, ask in default mode.
    Warning,
    /// Requires explicit confirmation regardless of mode.
    Critical,
}

/// A safety warning about a potentially dangerous command pattern.
#[derive(Debug, Clone)]
pub struct SafetyWarning {
    /// Machine-readable pattern identifier.
    pub pattern: &'static str,
    /// Human-readable warning message.
    pub message: String,
    /// Severity of this warning.
    pub severity: Severity,
}

/// Run all safety checks against a command string.
///
/// Returns zero or more warnings. Each warning describes a potential safety
/// concern with the command. The caller decides how to act on them based on
/// the severity and the current permission mode.
pub fn check_safety(cmd: &str) -> Vec<SafetyWarning> {
    let mut warnings = Vec::new();
    check_rm_rf(cmd, &mut warnings);
    check_git_push_force(cmd, &mut warnings);
    check_chmod_777(cmd, &mut warnings);
    warnings
}

/// Detect `rm -rf` targeting system-critical paths or using wildcards.
fn check_rm_rf(cmd: &str, warnings: &mut Vec<SafetyWarning>) {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("rm ") {
        return;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 2 {
        return;
    }

    // Check for recursive (-r) and force (-f) flags in any position
    let has_recursive = tokens.iter().any(|t| {
        *t == "-r" || *t == "-rf" || *t == "-fr" || *t == "--recursive" || t.contains("-rf")
    });
    let has_force = tokens
        .iter()
        .any(|t| *t == "-f" || *t == "-rf" || *t == "-fr" || *t == "--force" || t.contains("-rf"));

    if !has_recursive && !has_force {
        return;
    }

    // Find all non-flag arguments (target paths)
    let targets: Vec<&str> = tokens
        .iter()
        .filter(|t| !t.starts_with('-'))
        .copied()
        .collect();

    for target in targets {
        let target_trimmed = target.trim_end_matches('/');
        // System-critical paths
        let system_paths = [
            "/", "/etc", "/usr", "/var", "/bin", "/sbin", "/lib", "/lib64", "/lib32", "/opt",
            "/boot", "/dev", "/sys", "/proc", "/run", "/root", "/home",
        ];
        if system_paths.contains(&target_trimmed) {
            warnings.push(SafetyWarning {
                pattern: "rm-rf-system",
                message: format!(
                    "rm -rf on '{target}': this will destroy system files. \
                     Double-check the path before proceeding."
                ),
                severity: Severity::Critical,
            });
            continue;
        }

        // Wildcard in current directory
        if target == "*" || target == ".*" || target == "*/*" {
            warnings.push(SafetyWarning {
                pattern: "rm-rf-wildcard",
                message: format!(
                    "rm -rf with wildcard '{target}': this will delete many files in the \
                     current directory. Verify the operation is intentional."
                ),
                severity: Severity::Warning,
            });
        }
    }
}

/// Detect `git push --force` and suggest --force-with-lease as a safer alternative.
fn check_git_push_force(cmd: &str, warnings: &mut Vec<SafetyWarning>) {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("git push") {
        return;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 3 {
        return;
    }

    let has_force = tokens.iter().any(|t| *t == "--force" || *t == "-f");
    let has_force_with_lease = tokens
        .iter()
        .any(|t| *t == "--force-with-lease" || *t == "--force-if-includes");
    let has_upstream = tokens.iter().any(|t| *t == "-u" || *t == "--set-upstream");

    if has_force && !has_force_with_lease {
        let mut msg = String::from(
            "git push --force: this will overwrite the remote branch history. \
             Consider using --force-with-lease instead for a safer force-push that \
             rejects if the remote has new commits.",
        );
        if has_upstream {
            msg.push_str(
                " Combined with --set-upstream, this creates or overwrites a remote \
                 branch — ensure the branch name is correct.",
            );
        }
        warnings.push(SafetyWarning {
            pattern: "git-push-force",
            message: msg,
            severity: Severity::Warning,
        });
    }
}

/// Detect `chmod 777` and suggest more restrictive alternatives.
fn check_chmod_777(cmd: &str, warnings: &mut Vec<SafetyWarning>) {
    let trimmed = cmd.trim();
    if !trimmed.starts_with("chmod") {
        return;
    }

    let tokens: Vec<&str> = trimmed.split_whitespace().collect();
    if tokens.len() < 3 {
        return;
    }

    let has_777 = tokens
        .iter()
        .any(|t| *t == "777" || *t == "0777" || *t == "a+rwx");
    let has_recursive = tokens.iter().any(|t| *t == "-R" || *t == "--recursive");

    if has_777 {
        if has_recursive {
            warnings.push(SafetyWarning {
                pattern: "chmod-777-recursive",
                message: "chmod -R 777: recursively grants full permissions to everyone. \
                     This is a security risk — use targeted permissions like 755 (rwxr-xr-x) \
                     for directories or 644 (rw-r--r--) for files."
                    .into(),
                severity: Severity::Critical,
            });
        } else {
            warnings.push(SafetyWarning {
                pattern: "chmod-777",
                message: "chmod 777: grants read/write/execute to all users. \
                     Consider more restrictive permissions like 755 (rwxr-xr-x) \
                     for directories or 700 (rwx------) for sensitive files."
                    .into(),
                severity: Severity::Warning,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rm_rf_root_detected() {
        let w = check_safety("rm -rf /");
        assert!(w.iter().any(|w| w.pattern == "rm-rf-system"));
    }

    #[test]
    fn rm_rf_etc_detected() {
        let w = check_safety("rm -rf /etc");
        assert!(w.iter().any(|w| w.pattern == "rm-rf-system"));
    }

    #[test]
    fn rm_rf_slash_home_detected() {
        let w = check_safety("rm -rf /home");
        assert!(w.iter().any(|w| w.pattern == "rm-rf-system"));
    }

    #[test]
    fn rm_wildcard_detected() {
        let w = check_safety("rm -rf *");
        assert!(w.iter().any(|w| w.pattern == "rm-rf-wildcard"));
    }

    #[test]
    fn rm_single_file_not_flagged() {
        let w = check_safety("rm file.txt");
        assert!(w.is_empty());
    }

    #[test]
    fn rm_dir_not_flagged() {
        let w = check_safety("rm -r tempdir");
        assert!(w.is_empty());
    }

    #[test]
    fn git_push_force_detected() {
        let w = check_safety("git push --force origin main");
        assert!(w.iter().any(|w| w.pattern == "git-push-force"));
    }

    #[test]
    fn git_push_force_f_short_detected() {
        let w = check_safety("git push -f origin main");
        assert!(w.iter().any(|w| w.pattern == "git-push-force"));
    }

    #[test]
    fn git_push_force_with_lease_not_flagged() {
        let w = check_safety("git push --force-with-lease origin main");
        assert!(!w.iter().any(|w| w.pattern == "git-push-force"));
    }

    #[test]
    fn git_push_normal_not_flagged() {
        let w = check_safety("git push origin main");
        assert!(w.is_empty());
    }

    #[test]
    fn chmod_777_detected() {
        let w = check_safety("chmod 777 script.sh");
        assert!(w.iter().any(|w| w.pattern == "chmod-777"));
    }

    #[test]
    fn chmod_777_recursive_detected() {
        let w = check_safety("chmod -R 777 /some/dir");
        assert!(w.iter().any(|w| w.pattern == "chmod-777-recursive"));
    }

    #[test]
    fn chmod_safe_not_flagged() {
        let w = check_safety("chmod 755 script.sh");
        assert!(w.is_empty());
    }

    #[test]
    fn chmod_700_not_flagged() {
        let w = check_safety("chmod 700 private.key");
        assert!(w.is_empty());
    }

    #[test]
    fn non_matching_command_not_flagged() {
        let w = check_safety("ls -la");
        assert!(w.is_empty());
    }

    #[test]
    fn git_push_force_with_upstream_extra_message() {
        let w = check_safety("git push --force -u origin new-branch");
        assert!(
            w.iter().any(|w| w.message.contains("--set-upstream")),
            "expected combined --force and -u warning"
        );
    }
}
