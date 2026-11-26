//! 追加式调试日志：每条记录带本地时间，写入 `debug.log`。
//!
//! 多线程同时写也没问题——每次都是带 O_APPEND 的短行追加。

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;

/// 首行标记：用于区分带级别的 3.x 日志与更早的旧格式记录。
const FORMAT_MARKER: &str =
    "===== gh-proxy-manager 3.x 日志（含级别；旧版记录见 debug.log.legacy）=====";

/// 判断首行是不是当前格式的标记。
const MARKER_TAG: &str = "gh-proxy-manager 3.x 日志（含级别";

/// 日志级别，由轻到重。界面过滤时按“不低于所选级别”显示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Level {
    Debug,
    Info,
    Warn,
    Error,
}

impl Level {
    /// 写进日志行的英文标签。
    pub fn label(self) -> &'static str {
        match self {
            Level::Debug => "DEBUG",
            Level::Info => "INFO",
            Level::Warn => "WARN",
            Level::Error => "ERROR",
        }
    }

    /// 界面里显示的中文名。
    pub fn name_zh(self) -> &'static str {
        match self {
            Level::Debug => "调试",
            Level::Info => "信息",
            Level::Warn => "警告",
            Level::Error => "错误",
        }
    }

    pub fn from_label(text: &str) -> Option<Self> {
        match text.trim().to_ascii_uppercase().as_str() {
            "DEBUG" => Some(Level::Debug),
            "INFO" => Some(Level::Info),
            "WARN" => Some(Level::Warn),
            "ERROR" => Some(Level::Error),
            _ => None,
        }
    }
}

/// 解析一行日志的级别；格式不符（如格式标记行）按 INFO 处理。
pub fn line_level(line: &str) -> Level {
    line.split_whitespace()
        .nth(1)
        .and_then(Level::from_label)
        .unwrap_or(Level::Info)
}

/// 只保留不低于 `min` 级别的行。
pub fn filter_at_least(text: &str, min: Level) -> String {
    let mut out = String::new();
    for line in text.lines() {
        if line_level(line) >= min {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

/// 日志写入器。
#[derive(Debug, Clone)]
pub struct Logger {
    path: PathBuf,
    echo: bool,
}

impl Logger {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        let logger = Self {
            path: path.into(),
            echo: false,
        };
        logger.archive_legacy();
        logger
    }

    /// 首次运行（或日志格式升级）时把旧记录追加到 `debug.log.legacy`，
    /// 保证界面里看到的全是当前格式的可读记录；旧内容不会被删除。
    fn archive_legacy(&self) {
        let text = std::fs::read_to_string(&self.path).unwrap_or_default();
        let has_marker = text
            .lines()
            .next()
            .map(|line| line.contains(MARKER_TAG))
            .unwrap_or(false);
        if has_marker {
            return;
        }
        if !text.trim().is_empty() {
            let archive = self.path.with_file_name("debug.log.legacy");
            if let Ok(mut file) = OpenOptions::new().create(true).append(true).open(&archive) {
                let _ = file.write_all(text.as_bytes());
                if !text.ends_with('\n') {
                    let _ = file.write_all(b"\n");
                }
            }
            let _ = std::fs::remove_file(&self.path);
        }
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let ts = jiff::ZonedDateTime::now().strftime("%H:%M:%S").to_string();
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "[{ts}] {FORMAT_MARKER}");
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

    /// 常规事件（INFO）。
    pub fn log(&self, message: impl AsRef<str>) {
        self.write(Level::Info, message.as_ref());
    }

    /// 排查细节（DEBUG）：默认不刷屏，界面里按级别筛选查看。
    pub fn debug(&self, message: impl AsRef<str>) {
        self.write(Level::Debug, message.as_ref());
    }

    /// 可恢复的问题（WARN）：操作继续，但结果可能不完整。
    pub fn warn(&self, message: impl AsRef<str>) {
        self.write(Level::Warn, message.as_ref());
    }

    /// 操作失败（ERROR）。
    pub fn error(&self, message: impl AsRef<str>) {
        self.write(Level::Error, message.as_ref());
    }

    fn write(&self, level: Level, message: &str) {
        let ts = jiff::ZonedDateTime::now().strftime("%H:%M:%S").to_string();
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        if let Ok(mut file) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            let _ = writeln!(file, "[{ts}] {:<5} {message}", level.label());
        }
        if self.echo {
            eprintln!("[{ts}] {:<5} {message}", level.label());
        }
    }

    /// 读取最后 `max_lines` 行（文件不存在或为空时返回空字符串）。
    ///
    /// 界面里只显示尾部，避免日志变长后每次刷新都卡住。
    pub fn tail(&self, max_lines: usize) -> String {
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return String::new();
        };
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(max_lines);
        let mut out = lines[start..].join("\n");
        if !out.is_empty() {
            out.push('\n');
        }
        out
    }

    /// 清空日志文件（保留文件本身和格式标记）。
    pub fn clear(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let ts = jiff::ZonedDateTime::now().strftime("%H:%M:%S").to_string();
        let mut file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(true)
            .open(&self.path)?;
        writeln!(file, "[{ts}] {FORMAT_MARKER}")?;
        file.sync_all()
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
        assert!(text.contains("3.x 日志"));
        assert_eq!(text.lines().count(), 3);
    }

    #[test]
    fn levels_are_written_and_filtered() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");
        let logger = Logger::new(&path);
        logger.debug("细节");
        logger.log("普通");
        logger.warn("注意");
        logger.error("坏了");

        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("DEBUG 细节"), "{text}");
        assert!(text.contains("INFO  普通"), "{text}");
        assert!(text.contains("WARN  注意"), "{text}");
        assert!(text.contains("ERROR 坏了"), "{text}");

        let warns = filter_at_least(&text, Level::Warn);
        assert!(warns.contains("注意"));
        assert!(warns.contains("坏了"));
        assert!(!warns.contains("普通"));
        assert!(!warns.contains("细节"));
        assert!(!warns.contains("3.x 日志"));

        assert_eq!(line_level("[10:00:00] ERROR 坏了"), Level::Error);
        assert_eq!(line_level("[10:00:00] DEBUG 细节"), Level::Debug);
        assert_eq!(line_level("[10:00:00] ===== 标记 ====="), Level::Info);
    }

    #[test]
    fn tail_keeps_last_lines() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");
        let logger = Logger::new(&path);
        for i in 0..5 {
            logger.log(format!("line {i}"));
        }
        let tail = logger.tail(2);
        assert_eq!(tail.lines().count(), 2);
        assert!(tail.contains("line 3"));
        assert!(tail.contains("line 4"));
        assert!(!tail.contains("line 2"));
        assert_eq!(logger.tail(0), "");
    }

    #[test]
    fn clear_truncates_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");
        let logger = Logger::new(&path);
        logger.log("hello");
        logger.clear().unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("hello"));
        assert!(text.contains("3.x 日志"));
    }

    #[test]
    fn legacy_log_is_archived() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("debug.log");
        std::fs::write(
            &path,
            "[10:12:24] zenity 输出=[关|关|关|关|管理备选地址|]\n",
        )
        .unwrap();

        let logger = Logger::new(&path);
        logger.log("新的记录");

        let archive = dir.path().join("debug.log.legacy");
        assert_eq!(
            std::fs::read_to_string(&archive).unwrap(),
            "[10:12:24] zenity 输出=[关|关|关|关|管理备选地址|]\n"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(!text.contains("zenity"));
        assert!(text.contains("新的记录"));

        // 第二次启动不会再归档。
        Logger::new(&path).log("再一条");
        assert!(dir.path().join("debug.log.legacy").exists());
        assert!(std::fs::read_to_string(&path).unwrap().contains("新的记录"));
    }
}
