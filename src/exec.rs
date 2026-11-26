//! 提权执行：把一次性脚本交给 root（sudo / pkexec）执行。
//!
//! 设计要点：
//! - 脚本先落到 0600 的临时文件，密码只走 stdin，绝不出现在命令行里
//! - 沙箱模式（`GHPM_SYSROOT`）直接 `bash`，不需要提权，方便端到端测试
//! - 交互模式（CLI 终端）让 sudo 自己在终端里要密码；图形界面则用密码框取到的密码

use std::io::{IsTerminal, Write};
use std::process::{Command, Output, Stdio};

use crate::error::{Error, Result};
use crate::shell;

/// 执行 root 脚本的抽象，便于测试替换。
pub trait Runner {
    fn run_root(&self, script: &str) -> Result<()>;
}

/// 真实系统上的执行器。
pub struct SystemRunner {
    sandboxed: bool,
    interactive: bool,
    password: Option<String>,
}

impl SystemRunner {
    /// `interactive = true` 时，sudo 会在当前终端里自行提示密码。
    pub fn new(sandboxed: bool, interactive: bool) -> Self {
        Self {
            sandboxed,
            interactive,
            password: None,
        }
    }

    /// 使用图形界面取到的密码。
    pub fn with_password(mut self, password: impl Into<String>) -> Self {
        self.password = Some(password.into());
        self
    }

    fn write_script(script: &str) -> Result<tempfile::NamedTempFile> {
        let mut file = tempfile::Builder::new()
            .prefix("ghpm-")
            .suffix(".sh")
            .tempfile()
            .map_err(|e| Error::io("/tmp", e))?;
        file.write_all(script.as_bytes())
            .map_err(|e| Error::io(file.path(), e))?;
        file.flush().map_err(|e| Error::io(file.path(), e))?;
        Ok(file)
    }
}

impl Runner for SystemRunner {
    fn run_root(&self, script: &str) -> Result<()> {
        let file = Self::write_script(script)?;
        let path = file.path();

        if self.sandboxed || shell::is_root() {
            let out = Command::new("bash")
                .arg(path)
                .output()
                .map_err(|e| Error::io("bash", e))?;
            return check(out, "bash");
        }

        if let Some(password) = self.password.as_deref() {
            return run_sudo_with_password(path, password);
        }

        if shell::have("sudo") {
            if self.interactive && std::io::stdin().is_terminal() {
                let status = Command::new("sudo")
                    .arg("--")
                    .arg("bash")
                    .arg(path)
                    .status()
                    .map_err(|e| Error::io("sudo", e))?;
                if status.success() {
                    return Ok(());
                }
                return Err(Error::Privilege(format!(
                    "sudo 返回 {}",
                    status.code().unwrap_or(-1)
                )));
            }

            // 非交互：只在 sudo 凭据仍有效时静默执行，否则交给上层去要密码。
            let out = Command::new("sudo")
                .args(["-n", "--", "bash"])
                .arg(path)
                .output()
                .map_err(|e| Error::io("sudo", e))?;
            if out.status.success() {
                return Ok(());
            }
            return Err(Error::NeedPrivilege);
        }

        if shell::have("pkexec") {
            let out = Command::new("pkexec")
                .arg("bash")
                .arg(path)
                .output()
                .map_err(|e| Error::io("pkexec", e))?;
            return check(out, "pkexec");
        }

        Err(Error::NeedPrivilege)
    }
}

fn run_sudo_with_password(path: &std::path::Path, password: &str) -> Result<()> {
    let mut child = Command::new("sudo")
        .args(["-S", "-p", "", "--", "bash"])
        .arg(path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| Error::io("sudo", e))?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin
            .write_all(format!("{password}\n").as_bytes())
            .map_err(|e| Error::io("sudo stdin", e))?;
        // 关闭 stdin，让 sudo 正常结束读取
    }

    let out = child.wait_with_output().map_err(|e| Error::io("sudo", e))?;
    if out.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if stderr.is_empty() {
        Err(Error::Privilege(format!(
            "sudo 返回 {}",
            out.status.code().unwrap_or(-1)
        )))
    } else {
        Err(Error::Privilege(stderr))
    }
}

fn check(out: Output, program: &str) -> Result<()> {
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Command {
        program: program.to_string(),
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// `sudo` 凭据当前是否仍然有效（无需密码）。
pub fn sudo_cached() -> bool {
    Command::new("sudo")
        .args(["-n", "true"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// 当前是否完全不需要提权就能写系统目录。
pub fn privilege_ready(sandboxed: bool) -> bool {
    sandboxed || shell::is_root() || sudo_cached()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sandbox_runs_script_without_root() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("marker");
        let script = format!(
            "#!/bin/bash\nset -e\ntouch {}\n",
            shell::quote(&marker.display().to_string())
        );
        let runner = SystemRunner::new(true, false);
        runner.run_root(&script).unwrap();
        assert!(marker.is_file());
    }

    #[test]
    fn sandbox_reports_script_failure() {
        let runner = SystemRunner::new(true, false);
        let err = runner.run_root("#!/bin/bash\nexit 7\n").unwrap_err();
        assert!(err.to_string().contains("bash"));
    }
}
