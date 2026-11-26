use clap::Parser;

use gh_proxy_manager::cli::{self, Cli};

fn main() -> std::process::ExitCode {
    // 像普通 Unix 命令一样安静地死于 SIGPIPE，而不是在 `| head` 时 panic。
    #[cfg(unix)]
    unsafe {
        libc::signal(libc::SIGPIPE, libc::SIG_DFL);
    }

    let cli = Cli::parse();
    match cli::run(cli) {
        Ok(code) => std::process::ExitCode::from(code.clamp(0, 255) as u8),
        Err(err) => {
            eprintln!("错误: {err}");
            if err.is_need_privilege() {
                eprintln!(
                    "提示: 请在带终端的窗口运行（sudo 会提示密码），或直接 sudo 运行本命令。"
                );
            }
            std::process::ExitCode::FAILURE
        }
    }
}
