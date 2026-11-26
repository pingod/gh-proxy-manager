//! 统一的错误类型。
//!
//! 所有面向用户的错误都带上下文（文件路径、命令、返回码），
//! 便于 CLI 直接打印、GUI 直接弹窗、日志直接落盘。

use std::fmt;
use std::path::PathBuf;

/// 本项目的错误类型。
#[derive(Debug)]
pub enum Error {
    /// 文件系统操作失败，带上出问题的路径。
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
    /// 用户输入的加速前缀不合法。
    InvalidPrefix(String),
    /// 需要 root 权限，但当前无法提权（GUI 会据此弹出密码框）。
    NeedPrivilege,
    /// 提权失败（密码错误、polkit 拒绝等）。
    Privilege(String),
    /// 外部命令执行失败。
    Command {
        program: String,
        code: Option<i32>,
        stderr: String,
    },
    /// 其它带说明的错误。
    Message(String),
}

impl Error {
    pub fn io(path: impl Into<PathBuf>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn msg(message: impl Into<String>) -> Self {
        Self::Message(message.into())
    }

    pub fn is_need_privilege(&self) -> bool {
        matches!(self, Self::NeedPrivilege)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io { path, source } => write!(f, "{}: {source}", path.display()),
            Self::InvalidPrefix(input) => write!(f, "不是有效的加速地址: {input}"),
            Self::NeedPrivilege => write!(f, "需要管理员权限"),
            Self::Privilege(detail) => write!(f, "提权失败: {detail}"),
            Self::Command {
                program,
                code,
                stderr,
            } => {
                let code = code
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| "被信号终止".to_string());
                let stderr = stderr.trim();
                if stderr.is_empty() {
                    write!(f, "{program} 执行失败（退出码 {code}）")
                } else {
                    write!(f, "{program} 执行失败（退出码 {code}）: {stderr}")
                }
            }
            Self::Message(message) => f.write_str(message),
        }
    }
}

impl std::error::Error for Error {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Io { source, .. } => Some(source),
            _ => None,
        }
    }
}

/// 给 `io::Result` 补上路径上下文的小工具。
pub trait IoContext<T> {
    fn ctx(self, path: impl Into<PathBuf>) -> Result<T>;
}

impl<T> IoContext<T> for std::io::Result<T> {
    fn ctx(self, path: impl Into<PathBuf>) -> Result<T> {
        self.map_err(|e| Error::io(path, e))
    }
}

pub type Result<T> = std::result::Result<T, Error>;
