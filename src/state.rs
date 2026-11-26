//! 从磁盘实际状态推导当前开关（不信任任何缓存文件）。

use crate::paths::{DEFAULT_PREFIX, Paths};

/// 当前系统上的真实状态。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    /// `/etc/makepkg.conf.d/99-gh-proxy.conf` 存在。
    pub makepkg: bool,
    /// 全局 git 有指向 github 的 insteadOf 规则。
    pub git: bool,
    /// `/etc/gh-proxy/jsdelivr` 存在。
    pub jsdelivr: bool,
    /// `/etc/gh-proxy/nofailover` 不存在（默认开启自动换站）。
    pub failover: bool,
    /// `/etc/gh-proxy/prefix` 的内容。
    pub prefix: Option<String>,
}

impl State {
    /// 读取磁盘，得出状态。
    pub fn detect(paths: &Paths) -> Self {
        Self::detect_with(paths, &crate::gitcfg::global_entries())
    }

    /// 用给定的 git 条目推导状态（测试用，避免读取真实的全局配置）。
    pub fn detect_with(paths: &Paths, git_entries: &[crate::gitcfg::Entry]) -> Self {
        let prefix = paths.read_prefix();
        let git = crate::gitcfg::active_prefix(git_entries).is_some();
        Self {
            makepkg: paths.dlagent_conf.is_file(),
            git,
            jsdelivr: paths.jsdelivr_marker.is_file(),
            failover: !paths.nofailover_marker.is_file(),
            prefix,
        }
    }

    /// 生效前缀（未设置时回落到内置默认值）。
    pub fn effective_prefix(&self) -> String {
        self.prefix
            .clone()
            .unwrap_or_else(|| DEFAULT_PREFIX.to_string())
    }

    pub fn summary(&self) -> String {
        format!(
            "makepkg/yay: {}  git clone: {}  jsDelivr: {}  自动换站: {}\n生效地址: {}",
            on_off(self.makepkg),
            on_off(self.git),
            on_off(self.jsdelivr),
            on_off(self.failover),
            self.prefix.as_deref().unwrap_or("—"),
        )
    }
}

pub fn on_off(value: bool) -> &'static str {
    if value { "开" } else { "关" }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_empty_sandbox() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            dir.path(),
            dir.path().join("home"),
            dir.path().join("config"),
            "https://test/",
        );
        std::fs::create_dir_all(&paths.user_dir).unwrap();
        let state = State::detect_with(&paths, &[]);
        assert!(!state.makepkg);
        assert!(!state.jsdelivr);
        assert!(state.failover);
        assert_eq!(state.prefix, None);
        assert_eq!(state.effective_prefix(), DEFAULT_PREFIX);
    }

    #[test]
    fn detect_markers() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            dir.path(),
            dir.path().join("home"),
            dir.path().join("config"),
            "https://test/",
        );
        std::fs::create_dir_all(&paths.etc_dir).unwrap();
        std::fs::create_dir_all(&paths.makepkg_conf_dir).unwrap();
        std::fs::write(&paths.prefix_file, "https://p.example/\n").unwrap();
        std::fs::write(&paths.dlagent_conf, "DLAGENTS=()\n").unwrap();
        std::fs::write(&paths.jsdelivr_marker, "").unwrap();
        std::fs::write(&paths.nofailover_marker, "").unwrap();

        let state = State::detect_with(&paths, &[]);
        assert!(state.makepkg);
        assert!(state.jsdelivr);
        assert!(!state.failover);
        assert_eq!(state.prefix.as_deref(), Some("https://p.example/"));
    }
}
