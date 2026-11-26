//! 端到端测试：把系统根目录、用户配置、git 配置全部指向临时目录，
//! 用编译出的真实二进制跑完整流程，绝不触碰本机 /etc 或 ~/.gitconfig。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

struct Sandbox {
    dir: tempfile::TempDir,
}

impl Sandbox {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["root", "config", "home"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        Self { dir }
    }

    fn root(&self) -> PathBuf {
        self.dir.path().join("root")
    }

    fn gitconfig(&self) -> PathBuf {
        self.dir.path().join("gitconfig")
    }

    fn proxies_file(&self) -> PathBuf {
        self.dir.path().join("config/gh-proxy-manager/proxies")
    }

    fn run(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_gh-proxy-manager"))
            .args(args)
            .env("GHPM_SYSROOT", self.root())
            .env("GHPM_GIT_CONFIG", self.gitconfig())
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("HOME", self.dir.path().join("home"))
            .env("GHPM_TEST_URL", "https://example.invalid/test.tar.gz")
            .output()
            .expect("二进制可以执行")
    }

    fn ok(&self, args: &[&str]) -> String {
        let out = self.run(args);
        assert!(
            out.status.success(),
            "命令 {args:?} 失败: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).to_string()
    }

    fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.root().join(relative)).unwrap_or_default()
    }

    fn exists(&self, relative: &str) -> bool {
        self.root().join(relative).exists()
    }
}

#[test]
fn makepkg_channel_roundtrip() {
    let sb = Sandbox::new();

    sb.ok(&["on"]);
    assert!(sb.exists("etc/makepkg.conf.d/99-gh-proxy.conf"));
    assert!(sb.exists("etc/gh-proxy/prefix"));
    assert!(sb.exists("usr/local/bin/curl-gh"));
    assert!(sb.read("etc/gh-proxy/prefix").contains("https://"));
    assert!(
        sb.read("etc/makepkg.conf.d/99-gh-proxy.conf")
            .contains("/usr/local/bin/curl-gh")
    );

    // 安装出来的包装脚本必须是合法 bash
    let script = sb.root().join("usr/local/bin/curl-gh");
    let syntax = Command::new("bash")
        .arg("-n")
        .arg(&script)
        .output()
        .unwrap();
    assert!(syntax.status.success(), "curl-gh 语法错误");

    sb.ok(&["off"]);
    assert!(!sb.exists("etc/makepkg.conf.d/99-gh-proxy.conf"));
    assert!(!sb.exists("usr/local/bin/curl-gh"));
    // 前缀文件保留，方便下次开启直接用
    assert!(sb.exists("etc/gh-proxy/prefix"));
}

#[test]
fn git_rule_roundtrip_and_prefix_switch() {
    let sb = Sandbox::new();

    sb.ok(&["git-on"]);
    let config = std::fs::read_to_string(sb.gitconfig()).unwrap();
    assert!(config.contains("insteadOf = https://github.com/"));
    assert!(config.contains("url \"https://"));
    let first_section = config
        .lines()
        .find(|l| l.trim_start().starts_with("[url"))
        .unwrap()
        .to_string();

    sb.ok(&["use", "https://ghfast.top/"]);
    let config = std::fs::read_to_string(sb.gitconfig()).unwrap();
    assert!(config.contains("https://ghfast.top/https://github.com/"));
    assert!(
        !config.contains(
            first_section
                .trim_start_matches("[url \"")
                .trim_end_matches("\"]")
        ),
        "旧前缀规则应被删除"
    );

    sb.ok(&["git-off"]);
    let config = std::fs::read_to_string(sb.gitconfig()).unwrap_or_default();
    assert!(!config.contains("insteadOf"));
}

#[test]
fn only_github_rules_are_touched() {
    let sb = Sandbox::new();
    std::fs::write(
        sb.gitconfig(),
        "[url \"https://mirror.example/gitlab/\"]\n\tinsteadOf = https://gitlab.com/\n",
    )
    .unwrap();

    sb.ok(&["git-on"]);
    sb.ok(&["git-off"]);

    let config = std::fs::read_to_string(sb.gitconfig()).unwrap();
    assert!(
        config.contains("https://gitlab.com/"),
        "用户自己的 insteadOf 规则不能被改动"
    );
    assert!(!config.contains("github.com/"));
}

#[test]
fn mirror_list_add_dedupe_and_remove() {
    let sb = Sandbox::new();

    sb.ok(&["add", "https://a.example"]);
    sb.ok(&["add", "镜像：https://a.example/ 备用"]);
    sb.ok(&["add", "b.example/gh/"]);

    let proxies = sb.ok(&["proxies"]);
    assert_eq!(proxies.matches("https://a.example/").count(), 1);
    assert!(proxies.contains("https://b.example/gh/"));

    let file = std::fs::read_to_string(sb.proxies_file()).unwrap();
    assert_eq!(file.lines().count(), 3); // 默认地址 + 两个新增

    sb.ok(&["del", "https://a.example/"]);
    let file = std::fs::read_to_string(sb.proxies_file()).unwrap();
    assert!(!file.contains("https://a.example/"));
}

#[test]
fn invalid_prefix_is_rejected() {
    let sb = Sandbox::new();
    let out = sb.run(&["use", "ftp://example.com/"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("地址"));
    assert!(!sb.exists("etc/gh-proxy/prefix"));
}

#[test]
fn jsdelivr_and_failover_markers() {
    let sb = Sandbox::new();

    sb.ok(&["jsdelivr", "on"]);
    sb.ok(&["failover", "off"]);
    assert!(sb.exists("etc/gh-proxy/jsdelivr"));
    assert!(sb.exists("etc/gh-proxy/nofailover"));

    // 关掉 makepkg 通道不应影响这两个标记
    sb.ok(&["jsdelivr", "off"]);
    sb.ok(&["failover", "on"]);
    assert!(!sb.exists("etc/gh-proxy/jsdelivr"));
    assert!(!sb.exists("etc/gh-proxy/nofailover"));
}

#[test]
fn status_reflects_real_files() {
    let sb = Sandbox::new();
    sb.ok(&["on"]);
    sb.ok(&["git-on"]);

    let status = sb.ok(&["status"]);
    assert!(status.contains("makepkg/yay 加速: 开"));
    assert!(status.contains("git clone 加速:   开"));
    assert!(status.contains("https://"));
}

#[test]
fn reset_cleans_system_files() {
    let sb = Sandbox::new();
    sb.ok(&["on"]);
    sb.ok(&["git-on"]);
    sb.ok(&["add", "https://a.example/"]);

    sb.ok(&["reset", "--yes"]);

    assert!(!sb.exists("etc/gh-proxy/prefix"));
    assert!(!sb.exists("etc/makepkg.conf.d/99-gh-proxy.conf"));
    assert!(!sb.exists("usr/local/bin/curl-gh"));
    let config = std::fs::read_to_string(sb.gitconfig()).unwrap_or_default();
    assert!(!config.contains("github.com"));
    // 不带 --purge 时用户列表保留
    assert!(sb.proxies_file().exists());
}

#[test]
fn legacy_files_are_reused() {
    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.root().join("etc/gh-proxy")).unwrap();
    std::fs::write(
        sb.root().join("etc/gh-proxy/prefix"),
        "https://old.example/\n",
    )
    .unwrap();
    std::fs::create_dir_all(sb.proxies_file().parent().unwrap()).unwrap();
    std::fs::write(
        sb.proxies_file(),
        "https://old.example/\nhttps://new.example\n坏的地址行\n",
    )
    .unwrap();

    let status = sb.ok(&["status"]);
    assert!(status.contains("生效地址: https://old.example/"));
    assert!(status.contains("★ 当前生效"));
    // 旧格式（缺末尾 /）在展示时被规范化
    assert!(status.contains("https://new.example/"));
    assert!(!status.contains("坏的地址行"));
}

#[test]
fn uninstall_path_is_documented_in_help() {
    let sb = Sandbox::new();
    let help = sb.ok(&["--help"]);
    for expected in ["status", "on", "git-on", "failover", "get", "reset"] {
        assert!(help.contains(expected), "帮助里缺少子命令 {expected}");
    }
}

#[test]
fn debug_log_is_written() {
    let sb = Sandbox::new();
    sb.ok(&["on"]);
    let log = sb.ok(&["log"]);
    assert!(log.contains("apply 开始"));
}

#[allow(dead_code)]
fn assert_contains(path: &Path, needle: &str) {
    let text = std::fs::read_to_string(path).unwrap();
    assert!(text.contains(needle));
}
