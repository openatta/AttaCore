//! Sed command validation — parse and validate sed expressions before execution.
//!
//! Performs structural checks on sed commands:
//! - Detects unbalanced delimiters in `s///` expressions
//! - Detects `sed -i` without backup extension
//! - Detects `sed ... > file` overwriting source (file truncation)
//! - Validates that target file exists before `sed -i`
//!
//! All checks are warning-only — they never block execution. Warnings are
//! surfaced via the progress sender as system reminders so the model can
//! self-correct if needed.

/// Result of sed validation.
#[derive(Debug, Default)]
pub struct SedValidation {
    /// Warning messages collected during analysis.
    pub warnings: Vec<String>,
}

impl SedValidation {
    /// True if no warnings were raised.
    pub fn is_clean(&self) -> bool {
        self.warnings.is_empty()
    }
}

/// Validate a sed command, returning any warnings about potential issues.
///
/// Only processes commands whose first token is `sed` (optionally with a path
/// prefix like `/usr/bin/sed`). Non-sed commands return clean immediately.
pub fn validate_sed_command(cmd: &str) -> SedValidation {
    let mut result = SedValidation::default();

    // Only validate sed commands
    let trimmed = cmd.trim();
    if !trimmed.starts_with("sed") {
        return result;
    }

    // Verify it's really a sed command (not something like "sedate")
    let first_token = trimmed.split_whitespace().next().unwrap_or("");
    let base_name = std::path::Path::new(first_token)
        .file_name()
        .map(|f| f.to_str().unwrap_or(""))
        .unwrap_or(first_token);
    if base_name != "sed" {
        return result;
    }

    let parts: Vec<&str> = trimmed.split_whitespace().collect();
    if parts.len() < 2 {
        return result;
    }

    // Locate the sed expression — look for -e/-E/-r flags or a direct s/// argument
    let expr_index = parts.iter().position(|p| {
        p.starts_with("s/")
            || *p == "-e"
            || *p == "-E"
            || *p == "-r"
            || p.starts_with("'s/")
            || p.starts_with("\"s/")
    });

    let expr_str = match expr_index {
        Some(i) if parts[i] == "-e" || parts[i] == "-E" || parts[i] == "-r" => {
            parts.get(i + 1).copied().unwrap_or("")
        }
        Some(i) => parts[i],
        None => return result,
    };

    // Strip surrounding quotes
    let expr = expr_str
        .trim_start_matches('\'')
        .trim_end_matches('\'')
        .trim_start_matches('"')
        .trim_end_matches('"');

    // Only validate substitution commands (s///)
    if !expr.starts_with('s') {
        return result;
    }

    // Find the delimiter (character after 's')
    let delimiter = match expr.chars().nth(1) {
        Some(d) if d.is_alphanumeric() => return result, // not a sed substitution
        Some(d) => d,
        None => return result,
    };

    // Count delimiter occurrences in the expression (excluding the 's' prefix)
    let delim_count = expr.chars().skip(1).filter(|&c| c == delimiter).count();

    // Minimum: 3 delimiters for s/pat/repl/, 4 for s/pat/repl/flags
    if delim_count < 3 {
        result.warnings.push(format!(
            "Sed expression '{expr}' appears to have unbalanced delimiters ('{delimiter}'): expected at least 3 delimiters for s///, found {delim_count}"
        ));
    }

    // Check for -i without backup extension
    let has_i = parts.contains(&"-i");
    let has_backup = parts.iter().any(|&p| p.starts_with("-i") && p.len() > 2);

    if has_i && !has_backup {
        // Find the file argument — last non-flag, non-expression token
        let file_arg = parts
            .iter()
            .rev()
            .find(|p| {
                !p.starts_with('-')
                    && !p.starts_with('\'')
                    && !p.starts_with('"')
                    && !p.starts_with("s/")
                    && !p.starts_with("'s/")
                    && !p.starts_with("\"s/")
                    && **p != "-e"
                    && **p != "-E"
                    && **p != "-r"
            })
            .copied();

        if let Some(file) = file_arg {
            if !file.is_empty() {
                result.warnings.push(
                    "sed -i without backup extension: in-place edit with no backup. Consider 'sed -i.bak' to create a backup.".to_string(),
                );

                // Check if the target file exists
                if !std::path::Path::new(file).exists() {
                    result.warnings.push(format!(
                        "Target file '{file}' does not exist for sed -i. The command will create it empty."
                    ));
                }
            }
        }
    }

    // Check for sed ... > file overwriting source
    if let Some(redirect_pos) = cmd.find("> ") {
        let after_arrow = cmd[redirect_pos + 2..].trim();
        let redirect_file = after_arrow.split_whitespace().next().unwrap_or("");

        if !redirect_file.is_empty() {
            let sed_file = parts
                .iter()
                .rev()
                .find(|p| {
                    !p.starts_with('-')
                        && !p.starts_with('\'')
                        && !p.starts_with('"')
                        && !p.starts_with("s/")
                        && !p.starts_with("'s/")
                        && !p.starts_with("\"s/")
                })
                .copied()
                .unwrap_or("");

            if !sed_file.is_empty() && redirect_file == sed_file {
                result.warnings.push(format!(
                    "sed output redirect to the same file '{redirect_file}': this will truncate the file before sed reads it, resulting in an empty output. Use 'sed -i' for in-place editing instead."
                ));
            }
        }
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_sed_no_warnings() {
        let r = validate_sed_command("sed 's/foo/bar/' file.txt");
        assert!(r.is_clean(), "expected no warnings, got: {:?}", r.warnings);
    }

    #[test]
    fn non_sed_command_returns_no_warnings() {
        let r = validate_sed_command("grep foo bar.txt");
        assert!(r.is_clean());
    }

    #[test]
    fn sed_with_path_prefix() {
        let r = validate_sed_command("/usr/bin/sed -i 's/foo/bar/g' config.txt");
        assert!(!r.is_clean());
        assert!(r.warnings.iter().any(|w| w.contains("without backup")));
    }

    #[test]
    fn balanced_delimiter_passes() {
        let r = validate_sed_command("sed 's|foo|bar|' file.txt");
        assert!(r.is_clean(), "expected no warnings, got: {:?}", r.warnings);
    }

    #[test]
    fn sed_i_without_backup() {
        let r = validate_sed_command("sed -i 's/foo/bar/g' config.txt");
        assert!(!r.is_clean(), "expected warnings for sed -i without backup");
        assert!(r.warnings.iter().any(|w| w.contains("without backup")));
    }

    #[test]
    fn sed_i_with_backup_silent() {
        let r = validate_sed_command("sed -i.bak 's/foo/bar/g' config.txt");
        assert!(
            r.is_clean(),
            "expected no warnings with -i.bak, got: {:?}",
            r.warnings
        );
    }

    #[test]
    fn sed_redirect_same_file_warning() {
        let r = validate_sed_command("sed 's/foo/bar/' file.txt > file.txt");
        assert!(!r.is_clean(), "expected warning for redirect to same file");
        assert!(r.warnings.iter().any(|w| w.contains("redirect")));
    }

    #[test]
    fn sed_redirect_different_file_ok() {
        let r = validate_sed_command("sed 's/foo/bar/' input.txt > output.txt");
        assert!(r.is_clean());
    }

    #[test]
    fn unbalanced_delimiter_detected() {
        let r = validate_sed_command("sed 's/foo/bar' file.txt");
        assert!(
            !r.is_clean(),
            "expected warning for unbalanced delimiter, got: {:?}",
            r.warnings
        );
        assert!(r.warnings.iter().any(|w| w.contains("unbalanced")));
    }

    #[test]
    fn complex_sed_with_flags() {
        let r = validate_sed_command("sed -E -i '' 's/[[:space:]]+$//' src/main.rs");
        assert!(!r.is_clean(), "expected warning for sed -i without backup");
    }

    #[test]
    fn empty_command_no_warnings() {
        let r = validate_sed_command("");
        assert!(r.is_clean());
    }
}
