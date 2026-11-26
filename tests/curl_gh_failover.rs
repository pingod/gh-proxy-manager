//! 验证安装出来的 `curl-gh` 包装脚本真的会换站。
//!
//! 起一个本地 HTTP 服务：`/bad/` 永远 503，`/good/` 返回内容。
//! 生效站指向 /bad/，备选列表里第二个是 /good/，脚本应当在第一次失败后
//! 自动切换到第二个并成功写入文件。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

/// 本地测试服务器，记录收到的请求路径。
fn spawn_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_thread = hits.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            hits_for_thread.fetch_add(1, Ordering::SeqCst);

            let mut buf = [0u8; 2048];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();

            let response: Vec<u8> = if path.starts_with("/good/") {
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello\n".to_vec()
            } else {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                    .to_vec()
            };
            let _ = stream.write_all(&response);
            let _ = stream.flush();
        }
    });

    (port, hits)
}

/// 本地服务器：`/slow/` 每秒约 400 KB 地吐 1.2 MB 数据（30 秒才发完的“龟速”），
/// `/good/` 立即返回。用于验证龟速换站阈值。
fn spawn_trickle_server() -> (u16, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let hits = Arc::new(AtomicUsize::new(0));
    let hits_for_thread = hits.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            hits_for_thread.fetch_add(1, Ordering::SeqCst);
            std::thread::spawn(move || {
                let mut buf = [0u8; 2048];
                let n = stream.read(&mut buf).unwrap_or(0);
                let request = String::from_utf8_lossy(&buf[..n]);
                let path = request
                    .lines()
                    .next()
                    .and_then(|line| line.split_whitespace().nth(1))
                    .unwrap_or("/")
                    .to_string();

                if path.starts_with("/good/") {
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello\n",
                    );
                    return;
                }

                let total = 1200 * 1024usize;
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {total}\r\nConnection: close\r\n\r\n"
                );
                if stream.write_all(header.as_bytes()).is_err() {
                    return;
                }
                let chunk = vec![b'x'; 4096];
                for _ in 0..total / chunk.len() {
                    if stream.write_all(&chunk).is_err() {
                        return;
                    }
                    std::thread::sleep(Duration::from_millis(10));
                }
            });
        }
    });

    (port, hits)
}

fn install_script() -> (tempfile::TempDir, std::path::PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let script = dir.path().join("curl-gh");
    std::fs::write(&script, include_str!("../assets/curl-gh.sh")).unwrap();
    (dir, script)
}

fn run_script(
    dir: &std::path::Path,
    script: &std::path::Path,
    conf_dir: &std::path::Path,
    url: &str,
    extra: &[&str],
) -> std::process::Output {
    run_script_with_env(dir, script, conf_dir, url, extra, &[])
}

fn run_script_with_env(
    dir: &std::path::Path,
    script: &std::path::Path,
    conf_dir: &std::path::Path,
    url: &str,
    extra: &[&str],
    env: &[(&str, &str)],
) -> std::process::Output {
    let mut command = Command::new("bash");
    command
        .arg(script)
        .arg(url)
        .args(extra)
        .env("GH_PROXY_CONF_DIR", conf_dir)
        .env("XDG_CONFIG_HOME", dir.join("config"))
        .env("HOME", dir.join("home"));
    for (key, value) in env {
        command.env(key, value);
    }
    command.output().unwrap()
}

fn setup(dir: &std::path::Path, port: u16, failover: bool) -> std::path::PathBuf {
    write_config(dir, port, "/bad/", &["/bad/", "/good/"], failover)
}

/// 写入生效站与备选列表。`active` / `list` 是相对本地服务器的路径前缀。
fn write_config(
    dir: &std::path::Path,
    port: u16,
    active: &str,
    list: &[&str],
    failover: bool,
) -> std::path::PathBuf {
    let conf = dir.join("etc/gh-proxy");
    let list_dir = dir.join("config/gh-proxy-manager");
    std::fs::create_dir_all(&conf).unwrap();
    std::fs::create_dir_all(&list_dir).unwrap();

    std::fs::write(
        conf.join("prefix"),
        format!("http://127.0.0.1:{port}{active}\n"),
    )
    .unwrap();
    if !failover {
        std::fs::write(conf.join("nofailover"), "").unwrap();
    }
    let list: String = list
        .iter()
        .map(|path| format!("http://127.0.0.1:{port}{path}\n"))
        .collect();
    std::fs::write(list_dir.join("proxies"), list).unwrap();
    conf
}

#[test]
fn fails_over_to_next_mirror() {
    let (port, hits) = spawn_server();
    let (dir, script) = install_script();
    let conf = setup(dir.path(), port, true);
    let out_file = dir.path().join("download.bin");

    let output = run_script(
        dir.path(),
        &script,
        &conf,
        "https://github.com/example/releases/download/v1/file.bin",
        &["-f", "-sS", "-o", out_file.to_str().unwrap()],
    );

    assert!(
        output.status.success(),
        "curl-gh 应该换站成功: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "hello\n");
    assert!(
        hits.load(Ordering::SeqCst) >= 2,
        "至少应该请求过两个加速站，实际 {}",
        hits.load(Ordering::SeqCst)
    );
}

#[test]
fn nofailover_uses_only_active_mirror() {
    let (port, _hits) = spawn_server();
    let (dir, script) = install_script();
    let conf = setup(dir.path(), port, false);
    let out_file = dir.path().join("download.bin");

    let output = run_script(
        dir.path(),
        &script,
        &conf,
        "https://github.com/example/releases/download/v1/file.bin",
        &["-f", "-sS", "-o", out_file.to_str().unwrap()],
    );

    assert!(!output.status.success(), "关闭换站后不应回退到其它站");
    assert!(!out_file.exists() || std::fs::read_to_string(&out_file).unwrap().is_empty());
}

#[test]
fn canonicalizes_codeload_and_blob_before_prefixing() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen = Arc::new(Mutex::new(Vec::new()));
    let seen_for_thread = seen.clone();

    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]);
            let path = request
                .lines()
                .next()
                .and_then(|line| line.split_whitespace().nth(1))
                .unwrap_or("/")
                .to_string();
            seen_for_thread.lock().unwrap().push(path.clone());

            let response: &[u8] = if path.starts_with("/good/") {
                b"HTTP/1.1 200 OK\r\nContent-Length: 6\r\nConnection: close\r\n\r\nhello\n"
            } else {
                b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
            };
            let _ = stream.write_all(response);
        }
    });

    let (dir, script) = install_script();
    let conf = setup(dir.path(), port, true);

    let cases = [
        (
            "https://codeload.github.com/o/r/tar.gz/refs/tags/v1",
            "https://github.com/o/r/archive/refs/tags/v1.tar.gz",
        ),
        (
            "https://github.com/o/r/blob/main/dir/f.sh",
            "https://github.com/o/r/raw/main/dir/f.sh",
        ),
    ];

    for (index, (url, canonical)) in cases.iter().enumerate() {
        let out_file = dir.path().join(format!("canonical-{index}.bin"));
        let output = run_script(
            dir.path(),
            &script,
            &conf,
            url,
            &["-f", "-sS", "-o", out_file.to_str().unwrap()],
        );
        assert!(
            output.status.success(),
            "curl-gh 应能下载 {url}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "hello\n");

        let wanted = format!("/good/{canonical}");
        let paths = seen.lock().unwrap();
        assert!(
            paths.iter().any(|path| path == &wanted),
            "加速站应收到规范化后的 {wanted}，实际: {paths:?}"
        );
    }
}

#[test]
fn non_github_urls_bypass_proxies() {
    let (port, hits) = spawn_server();
    let (dir, script) = install_script();
    let conf = setup(dir.path(), port, true);
    let out_file = dir.path().join("download.bin");

    // 普通 http 地址：脚本必须原样交给 curl（这里 /good/x 能通）
    let output = run_script(
        dir.path(),
        &script,
        &conf,
        &format!("http://127.0.0.1:{port}/good/x"),
        &["-f", "-sS", "-o", out_file.to_str().unwrap()],
    );

    assert!(output.status.success());
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "hello\n");
    assert_eq!(hits.load(Ordering::SeqCst), 1);
}

#[test]
fn configured_slow_threshold_switches_to_next_mirror() {
    let (port, hits) = spawn_trickle_server();
    let (dir, script) = install_script();
    let conf = write_config(dir.path(), port, "/slow/", &["/slow/", "/good/"], true);
    // 阈值设到上限 500 KB/s，约 400 KB/s 的首个站会被判定为龟速
    std::fs::write(
        dir.path().join("config/gh-proxy-manager/slow-speed"),
        "500\n",
    )
    .unwrap();
    let out_file = dir.path().join("download.bin");

    let output = run_script_with_env(
        dir.path(),
        &script,
        &conf,
        "https://github.com/example/releases/download/v1/file.bin",
        &["-f", "-sS", "-o", out_file.to_str().unwrap()],
        &[("GH_PROXY_SPEED_TIME", "1")],
    );

    assert!(
        output.status.success(),
        "龟速后应换站成功: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::read_to_string(&out_file).unwrap(), "hello\n");
    assert!(
        hits.load(Ordering::SeqCst) >= 2,
        "至少应尝试两个站，实际 {}",
        hits.load(Ordering::SeqCst)
    );
}

#[test]
fn default_threshold_tolerates_moderately_slow_mirror() {
    let (port, hits) = spawn_trickle_server();
    let (dir, script) = install_script();
    let conf = write_config(dir.path(), port, "/slow/", &["/slow/", "/good/"], true);
    let out_file = dir.path().join("download.bin");

    // 不写 slow-speed：默认 20 KB/s，约 400 KB/s 的站应正常下载完
    let output = run_script_with_env(
        dir.path(),
        &script,
        &conf,
        "https://github.com/example/releases/download/v1/file.bin",
        &["-f", "-sS", "-o", out_file.to_str().unwrap()],
        &[("GH_PROXY_SPEED_TIME", "1")],
    );

    assert!(
        output.status.success(),
        "默认阈值下不应误判龟速: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(std::fs::metadata(&out_file).unwrap().len(), 1200 * 1024);
    assert_eq!(hits.load(Ordering::SeqCst), 1, "默认 20 KB/s 不应换站");
}
