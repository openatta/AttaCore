//! 权限闸 / 规则解析错误。

#[derive(thiserror::Error, Debug)]
pub enum GateError {
    #[error("invalid input: {message} (code {code})")]
    InvalidInput { message: String, code: i32 },
}

#[derive(thiserror::Error, Debug, PartialEq, Eq)]
pub enum ParseRuleError {
    #[error("rule string is empty")]
    Empty,
    #[error("rule has unbalanced parens: {0}")]
    Unbalanced(String),
    #[error("malformed rule: {0}")]
    Malformed(String),
}
