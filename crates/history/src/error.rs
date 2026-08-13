//! HistoryError 分类。

#[derive(thiserror::Error, Debug)]
pub enum HistoryError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    /// 单行解析错；保留行号便于定位坏文件
    #[error("parse line {line}: {error}")]
    Parse {
        line: usize,
        #[source]
        error: serde_json::Error,
    },

    #[error("schema: {0}")]
    Schema(#[from] serde_json::Error),

    #[error("path: {0}")]
    Path(String),

    #[error("session {0} not found")]
    SessionNotFound(String),

    #[error("HOME not set; set $HOME or pass an explicit projects_root")]
    NoHome,
}
