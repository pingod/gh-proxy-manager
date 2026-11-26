//! 追加式调试日志：每条记录带本地时间，写入 `debug.log`。
//!
//! 多线程同时写也没问题——每次都是带 O_APPEND 的短行追加。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// 日志写入器。
#[derive(Debug, Clone)]
pub struct Logger {
    path: PathBuf,
    echo: bool,
}

impl Logger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            echo: false,
        }
    }

    /// 同时输出到 stderr，CLI 使用。
    pub fn with_echo(mut self, echo: bool) -> Self {
        self.echo = echo;
        self
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn log(&self, message: impl AsRef<str>) {
        let message = message.as_ref();
        let ts = jiff::ZonedDateTime::now().strftime("%H:%M:%S").to_string();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "[{ts}] {message}");
        }
        if self.echo {
            eprintln!("[{ts}] {message}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn appends_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/debug.log");
        let logger = Logger::new(&path);
        logger.log("hello");
        logger.log("world");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("hello"));
        assert!(text.contains("world"));
        assert_eq!(text.lines().count(), 2);
    }
}
