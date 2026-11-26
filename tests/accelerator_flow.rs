//! 端到端验证从用户脚本迁移过来的加速能力：
//! 云端节点列表 → 并发测速 → SHA-256 完整性校验 → 选最快节点 → 写入备选列表。
//!
//! 全部网络请求都指向本机临时 HTTP 服务，不访问真实节点。

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::PathBuf;
use std::process::{Command, Output};

use gh_proxy_manager::paths::DEFAULT_PREFIX;

const ICON: &[u8] = b"\x89PNG\r\n\x1a\nfake-icon-for-tests";

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

    fn run(&self, args: &[&str], envs: &[(&str, String)]) -> Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_gh-proxy-manager"));
        command
            .args(args)
            .env("GHPM_SYSROOT", self.dir.path().join("root"))
            .env("GHPM_GIT_CONFIG", self.dir.path().join("gitconfig"))
            .env("XDG_CONFIG_HOME", self.dir.path().join("config"))
            .env("HOME", self.dir.path().join("home"))
            .env("GHPM_TEST_URL", "https://example.invalid/test.tar.gz");
        for (key, value) in envs {
            command.env(key, value);
        }
        command.output().expect("二进制可以执行")
    }

    fn nodes_file(&self) -> PathBuf {
        self.dir.path().join("config/gh-proxy-manager/nodes.json")
    }

    fn proxies_file(&self) -> PathBuf {
        self.dir.path().join("config/gh-proxy-manager/proxies")
    }
}

/// 启动一个“节点”：所有路径都返回 `body`。
fn spawn_node(body: Vec<u8>, nodes_json: Option<String>) -> u16 {
    spawn_node_on("127.0.0.1:0", body, nodes_json)
}

/// 在指定地址上启动节点（用于同一台机器上模拟多个不同主机的节点）。
fn spawn_node_on(addr: &str, body: Vec<u8>, nodes_json: Option<String>) -> u16 {
    let listener = TcpListener::bind(addr).unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut buf = [0u8; 8192];
            let _ = stream.read(&mut buf);
            let payload = match &nodes_json {
                Some(nodes) if request_path(&buf).starts_with("/nodes.json") => nodes.as_bytes(),
                _ => &body,
            };
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                payload.len()
            );
            let _ = stream.write_all(header.as_bytes());
            let _ = stream.write_all(payload);
        }
    });
    port
}

fn request_path(buf: &[u8]) -> String {
    String::from_utf8_lossy(buf)
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("/")
        .to_string()
}

#[test]
fn node_scan_verifies_and_selects_fastest() {
    let good_port = spawn_node(ICON.to_vec(), None);
    // 用 127.0.0.2 模拟另一个主机：节点列表按主机名去重，同 IP 不同端口会被合并。
    let bad_port = spawn_node_on("127.0.0.2:0", b"tampered".to_vec(), None);
    let nodes_json = format!(
        r#"{{"data":["http://127.0.0.1:{good_port}/node","http://127.0.0.2:{bad_port}/node"]}}"#
    );
    // 节点列表与期望哈希都由“好节点”提供；坏节点返回的内容哈希不匹配。
    let list_port = spawn_node(ICON.to_vec(), Some(nodes_json));

    let sb = Sandbox::new();
    let envs = [
        (
            "GHPM_NODES_URL",
            format!("http://127.0.0.1:{list_port}/nodes.json"),
        ),
        (
            "GHPM_ICON_HASH_URL",
            format!("http://127.0.0.1:{list_port}/icon"),
        ),
        (
            "GHPM_NODE_TEST_URL",
            "https://raw.githubusercontent.com/hubporg/ghproxy-extension/main/icon.png".to_string(),
        ),
    ];

    let out = sb.run(&["nodes", "--refresh", "--json"], &envs);
    assert!(
        out.status.success(),
        "nodes 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("nodes --json 应输出合法 JSON");
    let nodes = value["nodes"].as_array().unwrap();
    assert_eq!(nodes.len(), 1, "哈希不匹配的节点必须被淘汰: {value}");
    assert_eq!(
        value["best"].as_str().unwrap(),
        format!("http://127.0.0.1:{good_port}/node")
    );
    assert_eq!(value["cached"], false);
    assert_eq!(
        value["added_to_mirrors"], 2,
        "清单里的地址应全部并入备选: {value}"
    );
    assert_eq!(value["tests"].as_array().unwrap().len(), 2);
    assert!(sb.nodes_file().exists());

    // 清单里的两个地址都进了备选列表（校验失败的也保留，只是不参与自动选择）。
    let proxies = std::fs::read_to_string(sb.proxies_file()).unwrap();
    assert!(proxies.contains(&format!("http://127.0.0.1:{good_port}/node/")));
    assert!(proxies.contains(&format!("http://127.0.0.2:{bad_port}/node/")));

    // 第二次不带 --refresh：应直接读缓存。
    let cached = sb.run(&["nodes", "--json"], &envs);
    assert!(cached.status.success());
    let value: serde_json::Value = serde_json::from_slice(&cached.stdout).unwrap();
    assert_eq!(value["cached"], true);
    assert_eq!(
        value["best"].as_str().unwrap(),
        format!("http://127.0.0.1:{good_port}/node")
    );

    let dry = sb.run(&["auto", "--dry-run"], &envs);
    assert!(dry.status.success());
    let stdout = String::from_utf8_lossy(&dry.stdout);
    assert!(stdout.contains(&format!("http://127.0.0.1:{good_port}/node/")));
    assert!(
        !stdout.contains(&format!("http://127.0.0.2:{bad_port}/node")),
        "坏节点不能被选中: {stdout}"
    );

    // 真正执行 auto 会把校验通过的节点设为生效地址（沙箱内无需提权）。
    let applied = sb.run(&["auto"], &envs);
    assert!(
        applied.status.success(),
        "auto 失败: {}",
        String::from_utf8_lossy(&applied.stderr)
    );
    let proxies = std::fs::read_to_string(sb.proxies_file()).unwrap();
    assert!(proxies.contains(&format!("http://127.0.0.1:{good_port}/node/")));
    // 清单里未通过校验的地址仍然保留在备选列表里，只是不会被 auto 选中。
    assert!(proxies.contains(&format!("http://127.0.0.2:{bad_port}/node/")));
    let prefix = std::fs::read_to_string(sb.dir.path().join("root/etc/gh-proxy/prefix")).unwrap();
    assert_eq!(prefix.trim(), format!("http://127.0.0.1:{good_port}/node/"));
}

#[test]
fn resolve_rewrites_codeload_and_blob() {
    let sb = Sandbox::new();

    let out = sb.run(
        &[
            "resolve",
            "https://codeload.github.com/o/r/tar.gz/refs/tags/v1",
        ],
        &[],
    );
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("https://github.com/o/r/archive/refs/tags/v1.tar.gz"),
        "{stdout}"
    );
    assert!(
        stdout.contains(&format!(
            "{DEFAULT_PREFIX}https://github.com/o/r/archive/refs/tags/v1.tar.gz"
        )),
        "{stdout}"
    );

    let out = sb.run(&["resolve", "https://github.com/o/r/blob/main/f.sh"], &[]);
    assert!(out.status.success());
    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(
        stdout.contains("https://github.com/o/r/raw/main/f.sh"),
        "{stdout}"
    );

    let out = sb.run(&["resolve", "https://example.com/file.zip"], &[]);
    assert_eq!(out.status.code(), Some(2));
}

/// 黑名单里的地址不会被重新写入备选列表。
#[test]
fn blacklist_filters_remote_nodes() {
    let good_port = spawn_node(ICON.to_vec(), None);
    let nodes_json = format!(r#"{{"data":["http://127.0.0.1:{good_port}/node"]}}"#);
    let list_port = spawn_node(ICON.to_vec(), Some(nodes_json));

    let sb = Sandbox::new();
    std::fs::create_dir_all(sb.dir.path().join("config/gh-proxy-manager")).unwrap();
    std::fs::write(
        sb.dir.path().join("config/gh-proxy-manager/blacklist"),
        format!("http://127.0.0.1:{good_port}/node\n"),
    )
    .unwrap();

    let envs = [
        (
            "GHPM_NODES_URL",
            format!("http://127.0.0.1:{list_port}/nodes.json"),
        ),
        (
            "GHPM_ICON_HASH_URL",
            format!("http://127.0.0.1:{list_port}/icon"),
        ),
        (
            "GHPM_NODE_TEST_URL",
            "https://example.invalid/icon.png".to_string(),
        ),
    ];
    let out = sb.run(&["nodes", "--refresh", "--json"], &envs);
    assert!(
        out.status.success(),
        "nodes 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["added_to_mirrors"], 0, "{value}");
    let proxies = std::fs::read_to_string(sb.proxies_file()).unwrap();
    assert!(
        !proxies.contains(&format!("http://127.0.0.1:{good_port}/node")),
        "黑名单地址不应被重新加入: {proxies}"
    );
}

/// `sources` / `source` 文件里的当前接口优先于内置接口。
#[test]
fn nodes_use_selected_source() {
    let node_a = spawn_node(ICON.to_vec(), None);
    let node_b = spawn_node_on("127.0.0.2:0", ICON.to_vec(), None);
    let json_a = format!(r#"{{"data":["http://127.0.0.1:{node_a}/node"]}}"#);
    let json_b = format!(r#"{{"data":["http://127.0.0.2:{node_b}/node"]}}"#);
    let list_a = spawn_node(ICON.to_vec(), Some(json_a));
    let list_b = spawn_node_on("127.0.0.2:0", ICON.to_vec(), Some(json_b));

    let sb = Sandbox::new();
    let config = sb.dir.path().join("config/gh-proxy-manager");
    std::fs::create_dir_all(&config).unwrap();
    let source_a = format!("http://127.0.0.1:{list_a}/nodes.json");
    let source_b = format!("http://127.0.0.2:{list_b}/nodes.json");
    std::fs::write(config.join("sources"), format!("{source_a}\n{source_b}\n")).unwrap();
    std::fs::write(config.join("source"), format!("{source_b}\n")).unwrap();

    // 不设置 GHPM_NODES_URL，让程序自己从配置里选接口。
    let envs = [
        (
            "GHPM_ICON_HASH_URL",
            format!("http://127.0.0.1:{list_a}/icon"),
        ),
        ("GHPM_NODE_TEST_URL", "icon.png".to_string()),
    ];
    let out = sb.run(&["nodes", "--refresh", "--json"], &envs);
    assert!(
        out.status.success(),
        "nodes 失败: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let value: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(value["source"], source_b);
    let proxies = std::fs::read_to_string(sb.proxies_file()).unwrap();
    assert!(proxies.contains(&format!("http://127.0.0.2:{node_b}/node/")));
    assert!(!proxies.contains(&format!("http://127.0.0.1:{node_a}/node")));
}

#[test]
fn node_cache_is_valid_json() {
    let port = spawn_node(ICON.to_vec(), None);
    let nodes_json = format!(r#"{{"data":["http://127.0.0.1:{port}/node"]}}"#);
    let list_port = spawn_node(ICON.to_vec(), Some(nodes_json));

    let sb = Sandbox::new();
    let envs = [
        (
            "GHPM_NODES_URL",
            format!("http://127.0.0.1:{list_port}/nodes.json"),
        ),
        (
            "GHPM_ICON_HASH_URL",
            format!("http://127.0.0.1:{list_port}/icon"),
        ),
        (
            "GHPM_NODE_TEST_URL",
            "https://example.invalid/icon.png".to_string(),
        ),
    ];
    let out = sb.run(&["nodes", "--refresh"], &envs);
    assert!(out.status.success());

    let cache: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(sb.nodes_file()).unwrap()).unwrap();
    assert_eq!(
        cache["nodes"][0]["url"],
        format!("http://127.0.0.1:{port}/node")
    );
    assert!(cache["nodes"][0]["latency_ms"].as_u64().unwrap() >= 1);
}
