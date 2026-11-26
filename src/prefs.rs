//! 用户级可调参数。
//!
//! 与系统开关不同，这些值只影响 `curl-gh` 的运行时行为，不需要提权，
//! 改完立即生效。`curl-gh` 每次启动都会重新读取，因此不存在“应用”一步。

use std::path::Path;

use crate::error::{Error, Result};

/// 龟速换站的默认阈值（KB/s）。
pub const DEFAULT_SLOW_KBPS: u32 = 20;
/// 可设置的最小阈值（KB/s）。
pub const MIN_SLOW_KBPS: u32 = 5;
/// 可设置的最大阈值（KB/s）。
pub const MAX_SLOW_KBPS: u32 = 500;
/// 低于阈值持续多少秒才换站。
pub const SLOW_TIME_SECS: u32 = 30;

/// 把任意输入夹到允许区间。
pub fn clamp_kbps(kbps: u32) -> u32 {
    kbps.clamp(MIN_SLOW_KBPS, MAX_SLOW_KBPS)
}

/// 解析文件内容；空文件、非数字、0 都视为无效。
pub fn parse(text: &str) -> Option<u32> {
    text.trim().parse::<u32>().ok().filter(|v| *v > 0)
}

/// 读取用户设置；文件缺失或内容非法时回落到默认值。
pub fn load_slow_kbps(path: &Path) -> u32 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| parse(&text))
        .map(clamp_kbps)
        .unwrap_or(DEFAULT_SLOW_KBPS)
}

/// 保存用户设置（自动夹到允许区间）。
pub fn save_slow_kbps(path: &Path, kbps: u32) -> Result<u32> {
    let kbps = clamp_kbps(kbps);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    std::fs::write(path, format!("{kbps}\n")).map_err(|e| Error::io(path, e))?;
    Ok(kbps)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_when_missing() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            load_slow_kbps(&dir.path().join("slow-speed")),
            DEFAULT_SLOW_KBPS
        );
    }

    #[test]
    fn parses_and_clamps() {
        assert_eq!(parse(" 150 \n"), Some(150));
        assert_eq!(parse("0"), None);
        assert_eq!(parse("abc"), None);
        assert_eq!(clamp_kbps(0), MIN_SLOW_KBPS);
        assert_eq!(clamp_kbps(10_000), MAX_SLOW_KBPS);
        assert_eq!(clamp_kbps(64), 64);
    }

    #[test]
    fn save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested/slow-speed");
        assert_eq!(save_slow_kbps(&path, 80).unwrap(), 80);
        assert_eq!(load_slow_kbps(&path), 80);
        assert_eq!(save_slow_kbps(&path, 1).unwrap(), MIN_SLOW_KBPS);
        assert_eq!(load_slow_kbps(&path), MIN_SLOW_KBPS);
    }

    #[test]
    fn invalid_file_falls_back() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("slow-speed");
        std::fs::write(&path, "nonsense\n").unwrap();
        assert_eq!(load_slow_kbps(&path), DEFAULT_SLOW_KBPS);
    }
}
