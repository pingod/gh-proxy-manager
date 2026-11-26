//! Shell 相关的小工具：安全的引号转义、外部命令执行、root 判断。

use std::ffi::OsStr;
use std::process::{Command, Output};

/// 把任意字符串转成 POSIX shell 里安全的单引号字面量。
///
/// 单引号内唯一不能出现的字符是单引号本身，用 `'\''` 拆开拼接。
pub fn quote(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('\'');
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

/// 执行一条命令并收集输出。
pub fn run(program: impl AsRef<OsStr>, args: &[impl AsRef<OsStr>]) -> std::io::Result<Output> {
    Command::new(program).args(args).output()
}

/// 命令是否存在于 PATH 中。
pub fn have(program: &str) -> bool {
    which(program).is_some()
}

/// 当前进程是否以 root 运行。
pub fn is_root() -> bool {
    // SAFETY: geteuid 无参数、无副作用，永远成功。
    unsafe { libc::geteuid() == 0 }
}

/// 在 PATH 里查到一个可执行文件的绝对路径（用于替换 PATH 被 sudo 重置的场景）。
pub fn which(program: &str) -> Option<std::path::PathBuf> {
    use std::os::unix::fs::PermissionsExt;

    std::env::var_os("PATH").and_then(|paths| {
        std::env::split_paths(&paths)
            .map(|dir| dir.join(program))
            .find(|p| {
                p.metadata()
                    .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                    .unwrap_or(false)
            })
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn quote_plain() {
        assert_eq!(quote("https://a.example/"), "'https://a.example/'");
    }

    #[test]
    fn quote_with_single_quote() {
        assert_eq!(quote("it's"), "'it'\\''s'");
    }

    #[test]
    fn quote_rejects_shell_metacharacters() {
        let q = quote("$(rm -rf /) `id`");
        assert_eq!(q, "'$(rm -rf /) `id`'");
    }

    #[test]
    fn root_check_does_not_panic() {
        let _ = is_root();
    }
}
