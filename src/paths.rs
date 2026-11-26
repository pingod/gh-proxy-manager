//! 所有运行时路径集中在这里。
//!
//! 与旧版脚本保持完全相同的目录布局，因此重写版可以直接接管旧安装：
//!
//! ```text
//! /etc/gh-proxy/prefix               当前生效的加速前缀
//! /etc/gh-proxy/jsdelivr             存在 = raw.githubusercontent.com 走 jsDelivr
//! /etc/gh-proxy/nofailover           存在 = 关闭自动换站
//! /etc/makepkg.conf.d/99-gh-proxy.conf
//! /usr/local/bin/curl-gh             下载器包装脚本（自动换站）
//! ~/.config/gh-proxy-manager/proxies 备选地址列表
//! ~/.config/gh-proxy-manager/blacklist 不再自动加入的地址
//! ~/.config/gh-proxy-manager/sources 节点清单接口列表
//! ~/.config/gh-proxy-manager/source 当前使用的接口
//! ~/.config/gh-proxy-manager/results 测速结果缓存
//! ~/.config/gh-proxy-manager/nodes.json 云端节点测速缓存
//! ~/.config/gh-proxy-manager/debug.log
//! ```
//!
//! `GHPM_SYSROOT` 可以把整个 `/etc`、`/usr/local` 前缀搬到别的目录，
//! 用于不触碰真实系统的端到端测试（见 `tests/apply_flow.rs`）。

use std::path::{Path, PathBuf};

use crate::error::{Error, Result};

/// 内置的默认加速站。
pub const DEFAULT_PREFIX: &str = "https://ghfile.geekertao.top/";

/// 测速用的真实 release 文件（hyperfine，~700 KB）。
/// 只测 raw README 会漏掉“raw 可用但 release 404/限速”的坏站。
pub const DEFAULT_TEST_URL: &str = "https://github.com/sharkdp/hyperfine/releases/download/v1.18.0/hyperfine-v1.18.0-x86_64-unknown-linux-musl.tar.gz";

/// 加速规则只覆盖 GitHub 的这几个域名。
pub const GIT_HOST_PREFIX: &str = "https://github.com/";

/// 磁盘上的全部路径。
#[derive(Debug, Clone)]
pub struct Paths {
    /// 正常为 `/`，测试时可指向临时目录。
    pub sysroot: PathBuf,
    pub home: PathBuf,
    /// `/etc/gh-proxy`
    pub etc_dir: PathBuf,
    pub prefix_file: PathBuf,
    pub jsdelivr_marker: PathBuf,
    pub nofailover_marker: PathBuf,
    /// `/etc/makepkg.conf.d`
    pub makepkg_conf_dir: PathBuf,
    pub dlagent_conf: PathBuf,
    /// `/usr/local/bin`
    pub bin_dir: PathBuf,
    pub curl_gh: PathBuf,
    /// `~/.config/gh-proxy-manager`
    pub user_dir: PathBuf,
    pub mirrors_file: PathBuf,
    /// 删除过的地址：拉取远端清单时自动跳过。
    pub blacklist_file: PathBuf,
    /// 可选的节点清单接口列表。
    pub sources_file: PathBuf,
    /// 当前选中的节点清单接口。
    pub source_file: PathBuf,
    pub results_file: PathBuf,
    pub nodes_file: PathBuf,
    pub log_file: PathBuf,
    /// 测速目标 URL。
    pub test_url: String,
}

impl Paths {
    /// 读取真实系统路径。环境变量只提供测试/便携用途。
    pub fn system() -> Result<Self> {
        let sysroot = std::env::var_os("GHPM_SYSROOT")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from("/"));
        let home = std::env::var_os("HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| Error::msg("HOME 未设置，无法定位用户配置目录"))?;
        let config_home = std::env::var_os("XDG_CONFIG_HOME")
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        let test_url = std::env::var("GHPM_TEST_URL")
            .ok()
            .filter(|v| !v.trim().is_empty())
            .unwrap_or_else(|| DEFAULT_TEST_URL.to_string());
        Ok(Self::new(sysroot, home, config_home, test_url))
    }

    /// 显式构造路径集合，便于测试。
    pub fn new(
        sysroot: impl Into<PathBuf>,
        home: impl Into<PathBuf>,
        config_home: impl Into<PathBuf>,
        test_url: impl Into<String>,
    ) -> Self {
        let sysroot = sysroot.into();
        let home = home.into();
        let config_home = config_home.into();
        let etc_dir = sysroot.join("etc/gh-proxy");
        let makepkg_conf_dir = sysroot.join("etc/makepkg.conf.d");
        let bin_dir = sysroot.join("usr/local/bin");
        let user_dir = config_home.join("gh-proxy-manager");
        Self {
            prefix_file: etc_dir.join("prefix"),
            jsdelivr_marker: etc_dir.join("jsdelivr"),
            nofailover_marker: etc_dir.join("nofailover"),
            dlagent_conf: makepkg_conf_dir.join("99-gh-proxy.conf"),
            curl_gh: bin_dir.join("curl-gh"),
            mirrors_file: user_dir.join("proxies"),
            blacklist_file: user_dir.join("blacklist"),
            sources_file: user_dir.join("sources"),
            source_file: user_dir.join("source"),
            results_file: user_dir.join("results"),
            nodes_file: user_dir.join("nodes.json"),
            log_file: user_dir.join("debug.log"),
            etc_dir,
            makepkg_conf_dir,
            bin_dir,
            user_dir,
            sysroot,
            home,
            test_url: test_url.into(),
        }
    }

    /// 是否运行在 `GHPM_SYSROOT` 沙箱里（此时不需要提权）。
    pub fn sandboxed(&self) -> bool {
        self.sysroot != Path::new("/")
    }

    /// 当前生效前缀（去掉首尾空白；文件不存在返回 None）。
    pub fn read_prefix(&self) -> Option<String> {
        std::fs::read_to_string(&self.prefix_file)
            .ok()
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_is_backwards_compatible() {
        let p = Paths::new("/", "/home/u", "/home/u/.config", DEFAULT_TEST_URL);
        assert_eq!(p.prefix_file, PathBuf::from("/etc/gh-proxy/prefix"));
        assert_eq!(
            p.dlagent_conf,
            PathBuf::from("/etc/makepkg.conf.d/99-gh-proxy.conf")
        );
        assert_eq!(p.curl_gh, PathBuf::from("/usr/local/bin/curl-gh"));
        assert_eq!(
            p.mirrors_file,
            PathBuf::from("/home/u/.config/gh-proxy-manager/proxies")
        );
    }

    #[test]
    fn sandbox_detection() {
        let p = Paths::new("/tmp/root", "/home/u", "/home/u/.config", "https://t/");
        assert!(p.sandboxed());
        let p = Paths::new("/", "/home/u", "/home/u/.config", "https://t/");
        assert!(!p.sandboxed());
    }
}
