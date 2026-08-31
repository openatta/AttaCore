//! `FileSystem` — reading and writing wherever the tools' files live.

use std::path::{Path, PathBuf};

use super::ExecError;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Metadata {
    pub is_dir: bool,
    pub is_file: bool,
    pub is_symlink: bool,
    pub len: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub path: PathBuf,
    pub is_dir: bool,
}

/// Where a tool's files are.
///
/// Whole-value rather than streaming, which is the opposite of [`Process`] and
/// deliberately so: every call site in the engine reads or writes a whole
/// file, and a tool result is capped long before a file could be large enough
/// to need chunking. Designing a streaming interface with no consumer would
/// oblige every implementation to provide one.
///
/// **Path safety is not here.** Whether a path may be written is a policy —
/// it answers the same question regardless of which machine the file is on,
/// and a provider that decided its own out-of-bounds rules could cancel them.
/// It stays in `permissions::path_safety`, above this contract.
///
/// One thing does cross: [`canonicalize`](Self::canonicalize). A remote's
/// symlink graph is the remote's, so resolving one here would answer about the
/// wrong filesystem. The order is: canonicalize through the provider, check
/// the resolved path against the policy, then write through the provider.
///
/// [`Process`]: super::Process
#[async_trait::async_trait]
pub trait FileSystem: Send + Sync {
    async fn read(&self, path: &Path) -> Result<Vec<u8>, ExecError>;
    async fn write(&self, path: &Path, bytes: &[u8]) -> Result<(), ExecError>;
    async fn create_dir_all(&self, path: &Path) -> Result<(), ExecError>;
    async fn read_dir(&self, path: &Path) -> Result<Vec<DirEntry>, ExecError>;
    async fn remove_dir_all(&self, path: &Path) -> Result<(), ExecError>;
    async fn metadata(&self, path: &Path) -> Result<Metadata, ExecError>;
    async fn canonicalize(&self, path: &Path) -> Result<PathBuf, ExecError>;

    /// What most callers actually want. Provided, so an implementor writes
    /// `read` and gets this.
    async fn read_to_string(&self, path: &Path) -> Result<String, ExecError> {
        let bytes = self.read(path).await?;
        String::from_utf8(bytes)
            .map_err(|e| ExecError::failed(format!("{} is not valid UTF-8: {e}", path.display())))
    }

    /// Convenience with the same relationship to `write`.
    async fn write_str(&self, path: &Path, text: &str) -> Result<(), ExecError> {
        self.write(path, text.as_bytes()).await
    }

    /// Metadata of what `path` points at rather than of `path` itself.
    ///
    /// [`metadata`](Self::metadata) reports on the link — that is what its
    /// `is_symlink` field is for — so a caller asking "how big is this, is it
    /// a directory" has to resolve first or a symlink to a directory answers
    /// as a small ordinary file.
    async fn metadata_following_symlinks(&self, path: &Path) -> Result<Metadata, ExecError> {
        let resolved = self.canonicalize(path).await?;
        self.metadata(&resolved).await
    }

    /// The resolved form of a path that may not exist yet.
    ///
    /// A write policy has to be checked against a path that has already had
    /// its symlinks resolved, but the file being created has no symlinks to
    /// resolve. Falling back to the resolved parent plus the final component
    /// answers the question the policy actually asks — "where would this land"
    /// — for both cases.
    async fn canonicalize_best_effort(&self, path: &Path) -> PathBuf {
        if let Ok(resolved) = self.canonicalize(path).await {
            return resolved;
        }
        if let Some(parent) = path.parent() {
            if let Ok(resolved_parent) = self.canonicalize(parent).await {
                return resolved_parent.join(path.file_name().unwrap_or_default());
            }
        }
        path.to_path_buf()
    }
}
