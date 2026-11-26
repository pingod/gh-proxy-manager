//! 把一组设置应用到系统：生成 root 脚本 → 提权执行 → 同步 git → 校验。
//!
//! 核心原则（沿用旧版但更严格）：
//! - 只写我们自己的文件，从不改 `/etc/makepkg.conf` 主配置
//! - 一次提权完成全部 root 操作
//! - 写完必须回读校验，杜绝“以为成功了其实没写进去”

use std::fmt::Write as _;
use std::path::Path;

use crate::error::{Error, Result};
use crate::exec::Runner;
use crate::gitcfg;
use crate::logging::Logger;
use crate::mirrors;
use crate::paths::Paths;
use crate::shell;
use crate::state::State;

/// 随二进制一起安装的下载器包装脚本。
const CURL_GH_SCRIPT: &str = include_str!("../assets/curl-gh.sh");
/// makepkg 的 DLAGENT 覆盖配置。
const DLAGENT_CONF: &str = include_str!("../assets/99-gh-proxy.conf");

/// 一次“应用”要落地的完整设置。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub makepkg: bool,
    pub git: bool,
    pub jsdelivr: bool,
    pub failover: bool,
    pub prefix: String,
}

impl Settings {
    /// 以当前系统状态为基准。
    pub fn from_state(state: &State) -> Self {
        Self {
            makepkg: state.makepkg,
            git: state.git,
            jsdelivr: state.jsdelivr,
            failover: state.failover,
            prefix: state.effective_prefix(),
        }
    }

    /// 规范化前缀；非法时返回错误。
    pub fn normalized(mut self) -> Result<Self> {
        self.prefix = mirrors::normalize(&self.prefix)
            .ok_or_else(|| Error::InvalidPrefix(self.prefix.trim().to_string()))?;
        Ok(self)
    }
}

/// 生成要交给 root 执行的脚本（纯函数，便于审查和测试）。
pub fn root_script(paths: &Paths, settings: &Settings) -> String {
    let qp = |p: &Path| shell::quote(&p.display().to_string());

    let mut s = String::new();
    let _ = writeln!(s, "#!/usr/bin/env bash");
    let _ = writeln!(
        s,
        "# 由 gh-proxy-manager {} 生成，请勿手改（下次应用会覆盖）",
        env!("CARGO_PKG_VERSION")
    );
    let _ = writeln!(s, "set -euo pipefail");
    let _ = writeln!(s, "umask 022");
    let _ = writeln!(
        s,
        "mkdir -p {} {} {}",
        qp(&paths.etc_dir),
        qp(&paths.makepkg_conf_dir),
        qp(&paths.bin_dir)
    );

    // ---- 生效前缀（原子替换）----
    let template = format!("{}/.prefix.XXXXXX", paths.etc_dir.display());
    let _ = writeln!(s, "tmp=\"$(mktemp {})\"", shell::quote(&template));
    let _ = writeln!(
        s,
        "printf '%s\\n' {} > \"$tmp\"",
        shell::quote(&settings.prefix)
    );
    let _ = writeln!(s, "chmod 644 \"$tmp\"");
    let _ = writeln!(s, "mv -f \"$tmp\" {}", qp(&paths.prefix_file));

    // ---- 标记文件 ----
    write_marker(&mut s, &paths.jsdelivr_marker, settings.jsdelivr);
    write_marker(&mut s, &paths.nofailover_marker, !settings.failover);

    // ---- makepkg/yay 通道 ----
    if settings.makepkg {
        let _ = writeln!(s, "cat > {} <<'GHPM_CURL_GH_EOF'", qp(&paths.curl_gh));
        s.push_str(CURL_GH_SCRIPT);
        if !CURL_GH_SCRIPT.ends_with('\n') {
            s.push('\n');
        }
        let _ = writeln!(s, "GHPM_CURL_GH_EOF");
        let _ = writeln!(s, "chmod 755 {}", qp(&paths.curl_gh));

        let _ = writeln!(s, "cat > {} <<'GHPM_DLAGENT_EOF'", qp(&paths.dlagent_conf));
        s.push_str(DLAGENT_CONF);
        if !DLAGENT_CONF.ends_with('\n') {
            s.push('\n');
        }
        let _ = writeln!(s, "GHPM_DLAGENT_EOF");
        let _ = writeln!(s, "chmod 644 {}", qp(&paths.dlagent_conf));
    } else {
        let _ = writeln!(
            s,
            "rm -f {} {}",
            qp(&paths.curl_gh),
            qp(&paths.dlagent_conf)
        );
    }

    s
}

fn write_marker(script: &mut String, path: &Path, present: bool) {
    let q = shell::quote(&path.display().to_string());
    if present {
        let _ = writeln!(script, ": > {q}");
        let _ = writeln!(script, "chmod 644 {q}");
    } else {
        let _ = writeln!(script, "rm -f {q}");
    }
}

/// 应用设置：提权写系统文件 → 同步 git → 回读校验。
pub fn apply(
    paths: &Paths,
    settings: &Settings,
    runner: &dyn Runner,
    logger: &Logger,
) -> Result<()> {
    let settings = settings.clone().normalized()?;
    logger.log(format!(
        "apply 开始: makepkg={} git={} jsdelivr={} failover={} prefix={}",
        settings.makepkg, settings.git, settings.jsdelivr, settings.failover, settings.prefix
    ));

    if settings.git && !shell::have("git") {
        return Err(Error::msg("未找到 git，无法启用 git clone 加速"));
    }

    let script = root_script(paths, &settings);
    logger.log(format!("root 脚本 {} 字节", script.len()));
    runner.run_root(&script)?;

    let git_prefix = settings.git.then_some(settings.prefix.as_str());
    gitcfg::sync(git_prefix, logger)?;

    verify(paths, &settings, logger)?;
    logger.log("apply 成功");
    Ok(())
}

/// 回读实际状态并与期望值比较。
pub fn verify(paths: &Paths, settings: &Settings, logger: &Logger) -> Result<()> {
    let state = State::detect(paths);
    let mut problems = Vec::new();

    if state.makepkg != settings.makepkg {
        problems.push(format!(
            "makepkg/yay 通道: 期望 {}，实际 {}",
            crate::state::on_off(settings.makepkg),
            crate::state::on_off(state.makepkg)
        ));
    }
    if state.git != settings.git {
        problems.push(format!(
            "git clone 通道: 期望 {}，实际 {}",
            crate::state::on_off(settings.git),
            crate::state::on_off(state.git)
        ));
    }
    if state.jsdelivr != settings.jsdelivr {
        problems.push(format!(
            "jsDelivr: 期望 {}，实际 {}",
            crate::state::on_off(settings.jsdelivr),
            crate::state::on_off(state.jsdelivr)
        ));
    }
    if state.failover != settings.failover {
        problems.push(format!(
            "自动换站: 期望 {}，实际 {}",
            crate::state::on_off(settings.failover),
            crate::state::on_off(state.failover)
        ));
    }
    if state.prefix.as_deref() != Some(settings.prefix.as_str()) {
        problems.push(format!(
            "生效前缀: 期望 {}，实际 {}",
            settings.prefix,
            state.prefix.as_deref().unwrap_or("（空）")
        ));
    }

    if problems.is_empty() {
        logger.log("校验通过");
        Ok(())
    } else {
        for problem in &problems {
            logger.log(format!("校验失败: {problem}"));
        }
        Err(Error::msg(format!(
            "写入校验失败：\n{}",
            problems.join("\n")
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::paths::DEFAULT_TEST_URL;

    fn paths() -> Paths {
        Paths::new("/", "/home/u", "/home/u/.config", DEFAULT_TEST_URL)
    }

    fn settings() -> Settings {
        Settings {
            makepkg: true,
            git: true,
            jsdelivr: true,
            failover: true,
            prefix: "https://p.example/".into(),
        }
    }

    #[test]
    fn script_enables_everything() {
        let script = root_script(&paths(), &settings());
        assert!(script.starts_with("#!/usr/bin/env bash"));
        assert!(script.contains("set -euo pipefail"));
        assert!(script.contains("/etc/makepkg.conf.d/99-gh-proxy.conf"));
        assert!(script.contains("/usr/local/bin/curl-gh"));
        assert!(script.contains("GHPM_CURL_GH_EOF"));
        assert!(script.contains(": > '/etc/gh-proxy/jsdelivr'"));
        assert!(script.contains("rm -f '/etc/gh-proxy/nofailover'"));
        assert!(script.contains("mv -f \"$tmp\" '/etc/gh-proxy/prefix'"));
    }

    #[test]
    fn script_disables_cleanly() {
        let settings = Settings {
            makepkg: false,
            git: false,
            jsdelivr: false,
            failover: false,
            prefix: "https://p.example/".into(),
        };
        let script = root_script(&paths(), &settings);
        assert!(
            script
                .contains("rm -f '/usr/local/bin/curl-gh' '/etc/makepkg.conf.d/99-gh-proxy.conf'")
        );
        assert!(script.contains("rm -f '/etc/gh-proxy/jsdelivr'"));
        assert!(script.contains(": > '/etc/gh-proxy/nofailover'"));
        assert!(!script.contains("GHPM_CURL_GH_EOF"));
    }

    #[test]
    fn prefix_is_shell_quoted() {
        let mut s = settings();
        s.prefix = "https://p.example/'.x/".into();
        let script = root_script(&paths(), &s);
        assert!(script.contains("'https://p.example/'\\''.x/'"));
    }

    #[test]
    fn normalization_rejects_junk() {
        let mut s = settings();
        s.prefix = "nonsense".into();
        assert!(s.normalized().is_err());
    }
}
