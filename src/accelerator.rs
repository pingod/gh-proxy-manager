//! GitHub 下载链接识别、规范化与云端节点测速。
//!
//! 这一层迁移自 hubp.org 的 GitHub Accelerator 用户脚本的协议核心：
//!
//! - 识别 7 类下载链接（release / latest / archive / raw / codeload / raw.githubusercontent / gist raw）
//! - 把 `codeload.github.com/o/r/zip/ref` 还原成 `github.com/o/r/archive/ref.zip`
//! - 把 `github.com/o/r/blob/...` 还原成 `/raw/...`
//! - 从 `hubp.tbedu.top/nodes.json` 拉节点列表，32 并发测速
//! - 用固定图标的 SHA-256 做完整性校验，哈希不匹配的节点直接淘汰
//! - 结果缓存 2 小时，按延迟排序，选出最快节点
//!
//! 浏览器里的浮标、倒计时、匿名统计属于扩展形态，不在这里实现。
//! 所有网络访问都通过系统 `curl`，不引入 HTTP 客户端依赖。

use std::collections::HashSet;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::error::{Error, IoContext, Result};

/// 云端节点列表（与用户脚本同源）。
pub const NODES_URL: &str = "https://hubp.tbedu.top/nodes.json";
/// 节点列表不可达时的兜底节点，仍然要过完整性校验才会被采用。
pub const FALLBACK_NODES: [&str; 2] = ["https://gh.dpik.top", "https://github.tbap.top"];
/// 测速用的固定文件：一个真实存在的 raw 图片，节点必须能代理它。
pub const TEST_ICON_URL: &str = "https://raw.githubusercontent.com/hubporg/ghproxy-extension/refs/heads/main/src/icons/icon128.png";
/// 期望哈希的来源：同一张图标的官方地址。
pub const ICON_HASH_URL: &str = "https://hubp.tbedu.top/icons/icon128.png";
/// 测速缓存有效期。
pub const NODE_CACHE_TTL_SECS: u64 = 2 * 60 * 60;
/// 测速并发数。
pub const DEFAULT_CONCURRENCY: usize = 32;

const CF_TRACE_ENDPOINTS: [&str; 2] = [
    "https://www.cloudflare.com/cdn-cgi/trace",
    "https://1.1.1.1/cdn-cgi/trace",
];
const GEO_ENDPOINTS: [&str; 2] = ["https://api.ipapi.is/", "https://api.ip.sb/geoip"];

// ---------------------------------------------------------------------------
// URL 解析与链接识别
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq)]
struct UrlParts<'a> {
    host: &'a str,
    path: &'a str,
    query: Option<&'a str>,
}

/// 极简 URL 拆分：只支持 http(s)，够 GitHub 链接用。
fn parts(url: &str) -> Option<UrlParts<'_>> {
    let (scheme, rest) = url.split_once("://")?;
    if !scheme.eq_ignore_ascii_case("http") && !scheme.eq_ignore_ascii_case("https") {
        return None;
    }
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return None;
    }

    let (authority, remainder) = match rest.find('/') {
        Some(index) => (&rest[..index], &rest[index..]),
        None => (rest, "/"),
    };
    let host_port = authority.rsplit('@').next().unwrap_or(authority);
    let host = match host_port.rsplit_once(':') {
        Some((host, port)) if !port.is_empty() && port.chars().all(|c| c.is_ascii_digit()) => host,
        _ => host_port,
    };
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
    {
        return None;
    }

    let without_fragment = remainder.split('#').next().unwrap_or(remainder);
    let (path, query) = match without_fragment.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (without_fragment, None),
    };
    Some(UrlParts {
        host,
        path,
        query: query.filter(|q| !q.is_empty()),
    })
}

fn host_is(parts: &UrlParts<'_>, host: &str) -> bool {
    parts.host.eq_ignore_ascii_case(host)
}

fn is_github_path(path: &str) -> bool {
    [
        "/releases/download/",
        "/releases/latest/download/",
        "/archive/",
        "/raw/",
    ]
    .iter()
    .any(|segment| path.contains(segment))
}

/// 是否是加速器应当接管的下载链接。
///
/// 与用户脚本的 `PATTERNS` 一致；`blob` 页面链接不算下载链接，
/// 但 [`canonicalize`] 仍会把它改写成 raw（显式传入的 `get`/`resolve` 用得到）。
pub fn is_download_url(url: &str) -> bool {
    let Some(parts) = parts(url) else {
        return false;
    };
    if host_is(&parts, "github.com") {
        return is_github_path(parts.path);
    }
    if host_is(&parts, "codeload.github.com") {
        return parts.path.trim_matches('/').split('/').count() >= 4;
    }
    if host_is(&parts, "raw.githubusercontent.com") {
        return true;
    }
    if host_is(&parts, "gist.githubusercontent.com") {
        return parts.path.contains("/raw/");
    }
    false
}

/// 规范化为 GitHub 官方形态：
///
/// - `codeload.github.com/o/r/zip/ref` → `https://github.com/o/r/archive/ref.zip`
/// - `codeload.github.com/o/r/tar.gz/ref` → `.../archive/ref.tar.gz`
/// - `github.com/o/r/blob/...` → `github.com/o/r/raw/...`
/// - 已经是下载链接的 GitHub/raw/gist 地址原样返回
///
/// 非 GitHub 下载链接返回 `None`。
pub fn canonicalize(url: &str) -> Option<String> {
    let parts = parts(url)?;

    if host_is(&parts, "codeload.github.com") {
        let rest = parts.path.trim_start_matches('/');
        let mut segments = rest.splitn(4, '/');
        let user = segments.next().filter(|s| !s.is_empty())?;
        let repo = segments.next().filter(|s| !s.is_empty())?;
        let format = segments.next().filter(|s| !s.is_empty())?;
        let reference = segments.next().filter(|s| !s.is_empty())?;
        let extension = match format {
            "zip" => ".zip",
            "tar.gz" => ".tar.gz",
            _ => return None,
        };
        return Some(format!(
            "https://github.com/{user}/{repo}/archive/{reference}{extension}"
        ));
    }

    if host_is(&parts, "github.com") {
        if let Some(index) = parts.path.find("/blob/") {
            let path = format!("{}/raw/{}", &parts.path[..index], &parts.path[index + 6..]);
            return Some(with_query(format!("https://github.com{path}"), parts.query));
        }
        if is_github_path(parts.path) {
            return Some(url.to_string());
        }
        return None;
    }

    if host_is(&parts, "raw.githubusercontent.com") {
        return Some(url.to_string());
    }
    if host_is(&parts, "gist.githubusercontent.com") && parts.path.contains("/raw/") {
        return Some(url.to_string());
    }

    None
}

fn with_query(mut base: String, query: Option<&str>) -> String {
    if let Some(query) = query {
        base.push('?');
        base.push_str(query);
    }
    base
}

/// 拼接加速链接；非下载链接返回 `None`。
pub fn accelerated(node: &str, original: &str) -> Option<String> {
    let node = node.trim_end_matches('/');
    if node.is_empty() {
        return None;
    }
    let target = canonicalize(original)?;
    Some(format!("{node}/{target}"))
}

// ---------------------------------------------------------------------------
// 节点列表
// ---------------------------------------------------------------------------

/// 节点列表地址（测试可用 `GHPM_NODES_URL` 覆盖）。
pub fn nodes_url() -> String {
    env_url("GHPM_NODES_URL", NODES_URL)
}

/// 测速文件地址（测试可用 `GHPM_NODE_TEST_URL` 覆盖）。
pub fn test_icon_url() -> String {
    env_url("GHPM_NODE_TEST_URL", TEST_ICON_URL)
}

/// 期望哈希来源（测试可用 `GHPM_ICON_HASH_URL` 覆盖）。
pub fn icon_hash_url() -> String {
    env_url("GHPM_ICON_HASH_URL", ICON_HASH_URL)
}

fn env_url(key: &str, default: &str) -> String {
    std::env::var(key)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| default.to_string())
}

/// 解析 `{"data":["https://...", ...]}`（也接受裸数组）。
///
/// 规则与用户脚本一致：没有协议时补 `https://`，去掉末尾 `/`，
/// 按主机名去重，丢弃非法项。
pub fn parse_nodes(json: &str) -> Vec<String> {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return Vec::new();
    };
    let list = value
        .get("data")
        .and_then(serde_json::Value::as_array)
        .or_else(|| value.as_array());

    let mut seen = HashSet::new();
    let mut nodes = Vec::new();
    for item in list.into_iter().flatten() {
        let Some(raw) = item.as_str() else {
            continue;
        };
        let Some(node) = normalize_node(raw) else {
            continue;
        };
        let host = parts(&node)
            .map(|p| p.host.to_ascii_lowercase())
            .unwrap_or_default();
        if host.is_empty() || !seen.insert(host) {
            continue;
        }
        nodes.push(node);
    }
    nodes
}

fn normalize_node(raw: &str) -> Option<String> {
    let text = raw.trim().trim_end_matches('/');
    if text.is_empty() {
        return None;
    }
    let node = if text.starts_with("https://") || text.starts_with("http://") {
        text.to_string()
    } else if text.contains("://") {
        return None;
    } else {
        format!("https://{text}")
    };
    let parts = parts(&node)?;
    if parts.query.is_some() {
        return None;
    }
    Some(node)
}

/// 内置兜底节点。
pub fn fallback_nodes() -> Vec<String> {
    FALLBACK_NODES.iter().map(|node| node.to_string()).collect()
}

fn cache_buster(url: &str) -> String {
    let separator = if url.contains('?') { '&' } else { '?' };
    format!("{url}{separator}t={}", now_secs())
}

/// 拉取节点列表。
pub fn fetch_nodes() -> Result<Vec<String>> {
    let bytes = http_get(&cache_buster(&nodes_url()), 8)?;
    let nodes = parse_nodes(&String::from_utf8_lossy(&bytes));
    if nodes.is_empty() {
        return Err(Error::msg("节点列表为空或格式不正确"));
    }
    Ok(nodes)
}

// ---------------------------------------------------------------------------
// 测速与完整性校验
// ---------------------------------------------------------------------------

/// 单个节点的测速结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct NodeTest {
    pub url: String,
    /// 校验通过时的延迟（毫秒）。
    pub latency_ms: Option<u64>,
    pub verified: bool,
    pub detail: String,
}

impl NodeTest {
    pub fn verdict(&self) -> String {
        match (self.verified, self.latency_ms) {
            (true, Some(latency)) => format!("✓ 完整性校验通过 · {latency} ms"),
            _ => format!("✗ {}", self.detail),
        }
    }

    fn failed(node: &str, detail: impl Into<String>) -> Self {
        Self {
            url: node.to_string(),
            latency_ms: None,
            verified: false,
            detail: detail.into(),
        }
    }
}

/// 测速并校验一个节点。
pub fn test_node(node: &str, expected_hash: &str) -> NodeTest {
    let node = node.trim_end_matches('/');
    let target = format!("{node}/{}", test_icon_url());
    let tmp = match tempfile::NamedTempFile::new() {
        Ok(tmp) => tmp,
        Err(err) => return NodeTest::failed(node, format!("无法创建临时文件: {err}")),
    };

    let output = Command::new("curl")
        .args(["-sSL", "--connect-timeout", "5", "--max-time", "8", "-o"])
        .arg(tmp.path())
        .args(["-w", "%{http_code} %{time_total}"])
        .arg(&target)
        .stdin(Stdio::null())
        .output();

    let output = match output {
        Ok(output) => output,
        Err(err) => return NodeTest::failed(node, format!("无法执行 curl: {err}")),
    };
    let stdout = String::from_utf8_lossy(&output.stdout);
    let Some((code, seconds)) = parse_code_time(&stdout) else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return NodeTest::failed(
            node,
            if stderr.is_empty() {
                "连接失败".to_string()
            } else {
                stderr
            },
        );
    };
    if !(200..300).contains(&code) {
        return NodeTest::failed(node, format!("HTTP {code}"));
    }

    let bytes = match std::fs::read(tmp.path()) {
        Ok(bytes) => bytes,
        Err(err) => return NodeTest::failed(node, format!("读取响应失败: {err}")),
    };
    if bytes.is_empty() {
        return NodeTest::failed(node, "响应为空");
    }
    let hash = sha256_hex(&bytes);
    if !hash.eq_ignore_ascii_case(expected_hash) {
        return NodeTest::failed(node, "完整性校验失败（哈希不匹配）");
    }

    let latency_ms = (seconds * 1000.0).round().max(1.0) as u64;
    NodeTest {
        url: node.to_string(),
        latency_ms: Some(latency_ms),
        verified: true,
        detail: "完整性校验通过".to_string(),
    }
}

/// 解析 `curl -w '%{http_code} %{time_total}'` 的输出。
pub fn parse_code_time(text: &str) -> Option<(u16, f64)> {
    let last = text.lines().rev().find(|line| !line.trim().is_empty())?;
    let mut fields = last.split_whitespace();
    let code = fields.next()?.parse::<u16>().ok()?;
    let seconds = fields.next()?.parse::<f64>().ok()?;
    if !seconds.is_finite() || seconds < 0.0 {
        return None;
    }
    Some((code, seconds))
}

/// 并发测速；返回按延迟升序排列的结果，未通过的排在最后。
pub fn test_nodes(nodes: &[String], expected_hash: &str, concurrency: usize) -> Vec<NodeTest> {
    if nodes.is_empty() {
        return Vec::new();
    }
    let workers = concurrency.max(1).min(nodes.len());
    let cursor = AtomicUsize::new(0);
    let results = Mutex::new(Vec::with_capacity(nodes.len()));

    std::thread::scope(|scope| {
        for _ in 0..workers {
            scope.spawn(|| {
                loop {
                    let index = cursor.fetch_add(1, Ordering::Relaxed);
                    let Some(node) = nodes.get(index) else {
                        break;
                    };
                    let result = test_node(node, expected_hash);
                    results
                        .lock()
                        .unwrap_or_else(|poisoned| poisoned.into_inner())
                        .push(result);
                }
            });
        }
    });

    let mut results = results
        .into_inner()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    results.sort_by_key(|node| node.latency_ms.unwrap_or(u64::MAX));
    results
}

/// 拉取期望哈希（云端图标本身的 SHA-256）。
pub fn fetch_expected_hash() -> Result<String> {
    let bytes = http_get(&cache_buster(&icon_hash_url()), 8)?;
    if bytes.is_empty() {
        return Err(Error::msg("云端校验图标的响应为空"));
    }
    Ok(sha256_hex(&bytes))
}

// ---------------------------------------------------------------------------
// 缓存
// ---------------------------------------------------------------------------

/// 通过校验的节点及其延迟。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedNode {
    pub url: String,
    pub latency_ms: u64,
}

impl CachedNode {
    pub fn verdict(&self) -> String {
        format!("✓ 完整性校验通过 · {} ms", self.latency_ms)
    }
}

/// 节点测速缓存（`nodes.json`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCache {
    pub tested_at: u64,
    pub nodes: Vec<CachedNode>,
}

impl NodeCache {
    /// 从测速结果构造，只保留通过完整性校验的节点。
    pub fn from_tests(tests: &[NodeTest], tested_at: u64) -> Self {
        let mut nodes: Vec<CachedNode> = tests
            .iter()
            .filter(|test| test.verified)
            .filter_map(|test| {
                test.latency_ms.map(|latency_ms| CachedNode {
                    url: test.url.clone(),
                    latency_ms,
                })
            })
            .collect();
        nodes.sort_by_key(|node| node.latency_ms);
        Self { tested_at, nodes }
    }

    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.tested_at)
    }

    pub fn is_expired(&self) -> bool {
        self.age_secs() > NODE_CACHE_TTL_SECS
    }

    pub fn best(&self) -> Option<&CachedNode> {
        self.nodes
            .iter()
            .filter(|node| node.latency_ms > 0)
            .min_by_key(|node| node.latency_ms)
    }

    /// 读取未过期的缓存；文件不存在、损坏、过期或为空都返回 `None`。
    pub fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let cache: Self = serde_json::from_str(&text).ok()?;
        if cache.nodes.is_empty() || cache.is_expired() {
            return None;
        }
        Some(cache)
    }

    /// 原子写入。
    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).ctx(dir)?;
        }
        let dir = path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir).ctx(dir)?;
        serde_json::to_writer_pretty(&mut tmp, self)
            .map_err(|err| Error::msg(format!("写入节点缓存失败: {err}")))?;
        tmp.write_all(b"\n").ctx(path)?;
        tmp.flush().ctx(path)?;
        tmp.persist(path)
            .map_err(|err| Error::io(path, err.error))?;
        Ok(())
    }
}

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or(Duration::ZERO)
        .as_secs()
}

// ---------------------------------------------------------------------------
// 出口地区检测
// ---------------------------------------------------------------------------

/// 当前出口 IP 与地区。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Location {
    pub ip: String,
    pub country: String,
    pub in_china: bool,
}

impl Location {
    pub fn country_name(&self) -> String {
        country_name(&self.country)
    }

    pub fn advice(&self) -> &'static str {
        if self.in_china {
            "中国大陆 · 建议开启加速"
        } else {
            "境外 · 可直连，也可按需加速"
        }
    }
}

/// 解析 Cloudflare `/cdn-cgi/trace` 的 `key=value` 文本。
pub fn parse_cf_trace(text: &str) -> Option<Location> {
    let mut ip = "unknown".to_string();
    let mut country = None;
    for line in text.lines() {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        match key.trim() {
            "ip" => ip = value.trim().to_string(),
            "loc" => country = Some(value.trim().to_ascii_uppercase()),
            _ => {}
        }
    }
    let country = country?;
    if country.len() != 2 || !country.chars().all(|c| c.is_ascii_alphabetic()) {
        return None;
    }
    Some(Location {
        ip,
        in_china: country == "CN",
        country,
    })
}

/// 解析 ipapi.is / ip.sb 之类的 JSON。
pub fn parse_geo_json(text: &str) -> Option<Location> {
    let value = serde_json::from_str::<serde_json::Value>(text).ok()?;
    let ip = ["ip", "query", "ip_address"]
        .iter()
        .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .unwrap_or("unknown")
        .to_string();

    let mut country = ["country_code", "countryCode"]
        .iter()
        .find_map(|key| value.get(key).and_then(serde_json::Value::as_str))
        .map(str::to_string);

    if country.is_none() {
        country = value
            .get("location")
            .and_then(|location| location.get("country_code"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string);
    }
    if country.is_none() {
        country = match value.get("country") {
            Some(serde_json::Value::String(name)) => Some(name.clone()),
            Some(object @ serde_json::Value::Object(_)) => object
                .get("iso_code")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            _ => None,
        };
    }

    let country = normalize_country(country?.as_str())?;
    Some(Location {
        ip,
        in_china: country == "CN",
        country,
    })
}

fn normalize_country(raw: &str) -> Option<String> {
    let text = raw.trim();
    if matches!(text, "中国" | "中国大陆" | "China") {
        return Some("CN".to_string());
    }
    let code = text.to_ascii_uppercase();
    if code.len() == 2 && code.chars().all(|c| c.is_ascii_alphabetic()) {
        Some(code)
    } else {
        None
    }
}

fn country_name(code: &str) -> String {
    match code {
        "CN" => "中国大陆",
        "HK" => "中国香港",
        "TW" => "中国台湾",
        "US" => "美国",
        "JP" => "日本",
        "KR" => "韩国",
        "SG" => "新加坡",
        "DE" => "德国",
        "GB" => "英国",
        "FR" => "法国",
        _ => code,
    }
    .to_string()
}

/// 检测出口地区；全部接口失败返回 `None`。
pub fn detect_location() -> Option<Location> {
    for endpoint in CF_TRACE_ENDPOINTS {
        if let Ok(bytes) = http_get(endpoint, 4) {
            if let Some(location) = parse_cf_trace(&String::from_utf8_lossy(&bytes)) {
                return Some(location);
            }
        }
    }
    for endpoint in GEO_ENDPOINTS {
        if let Ok(bytes) = http_get(endpoint, 5) {
            if let Some(location) = parse_geo_json(&String::from_utf8_lossy(&bytes)) {
                return Some(location);
            }
        }
    }
    None
}

// ---------------------------------------------------------------------------
// 工具
// ---------------------------------------------------------------------------

fn http_get(url: &str, max_secs: u64) -> Result<Vec<u8>> {
    let output = Command::new("curl")
        .args([
            "-fsSL",
            "--connect-timeout",
            "5",
            "--max-time",
            &max_secs.to_string(),
            url,
        ])
        .stdin(Stdio::null())
        .output()
        .map_err(|err| Error::io("curl", err))?;
    if !output.status.success() {
        return Err(Error::Command {
            program: "curl".to_string(),
            code: output.status.code(),
            stderr: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    Ok(output.stdout)
}

/// 小写十六进制 SHA-256。
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;
    use std::sync::Arc;

    #[test]
    fn recognizes_every_download_pattern() {
        for url in [
            "https://github.com/o/r/releases/download/v1/f",
            "https://github.com/o/r/releases/latest/download/f",
            "https://github.com/o/r/archive/refs/tags/v1.zip",
            "https://github.com/o/r/raw/main/f",
            "https://codeload.github.com/o/r/zip/refs/heads/main",
            "https://raw.githubusercontent.com/o/r/main/f",
            "https://gist.githubusercontent.com/u/1/raw/f",
        ] {
            assert!(is_download_url(url), "should match: {url}");
        }
        assert!(!is_download_url("https://github.com/o/r"));
        assert!(!is_download_url("https://github.com/o/r/blob/main/f"));
        assert!(!is_download_url(
            "https://example.com/o/r/releases/download/v1/f"
        ));
        assert!(!is_download_url(
            "https://github.com.evil.example/o/r/archive/x"
        ));
        assert!(!is_download_url("ftp://github.com/o/r/archive/x"));
        assert!(!is_download_url("not a url"));
    }

    #[test]
    fn canonicalizes_codeload_and_blob() {
        assert_eq!(
            canonicalize("https://codeload.github.com/o/r/zip/refs/heads/main").as_deref(),
            Some("https://github.com/o/r/archive/refs/heads/main.zip")
        );
        assert_eq!(
            canonicalize("https://codeload.github.com/o/r/tar.gz/v1.0.0").as_deref(),
            Some("https://github.com/o/r/archive/v1.0.0.tar.gz")
        );
        assert_eq!(
            canonicalize("https://github.com/o/r/blob/main/dir/f?plain=1").as_deref(),
            Some("https://github.com/o/r/raw/main/dir/f?plain=1")
        );
        assert_eq!(
            canonicalize("https://github.com/o/r/releases/download/v1/f").as_deref(),
            Some("https://github.com/o/r/releases/download/v1/f")
        );
        assert_eq!(
            canonicalize("https://github.com/o/r/archive/main.zip").as_deref(),
            Some("https://github.com/o/r/archive/main.zip")
        );
        assert_eq!(
            canonicalize("https://raw.githubusercontent.com/o/r/main/f").as_deref(),
            Some("https://raw.githubusercontent.com/o/r/main/f")
        );
        assert_eq!(canonicalize("https://github.com/o/r"), None);
        assert_eq!(
            canonicalize("https://codeload.github.com/o/r/rar/main"),
            None
        );
        assert_eq!(
            canonicalize("https://example.com/o/r/archive/main.zip"),
            None
        );
    }

    #[test]
    fn accelerates_after_canonicalizing() {
        assert_eq!(
            accelerated(
                "https://gh.example/",
                "https://codeload.github.com/o/r/tar.gz/refs/tags/v1"
            )
            .as_deref(),
            Some("https://gh.example/https://github.com/o/r/archive/refs/tags/v1.tar.gz")
        );
        assert_eq!(
            accelerated("https://gh.example/", "https://github.com/o/r"),
            None
        );
        assert_eq!(
            accelerated("", "https://github.com/o/r/archive/main.zip"),
            None
        );
    }

    #[test]
    fn parses_and_normalizes_node_lists() {
        let json = r#"{"data":[
            "https://gh.example",
            "https://gh.example/",
            "gh2.example/gh",
            "http://gh3.example:8080/base/",
            "ftp://bad.example",
            "not a url with spaces",
            42,
            {"url":"https://obj.example"}
        ]}"#;
        assert_eq!(
            parse_nodes(json),
            vec![
                "https://gh.example".to_string(),
                "https://gh2.example/gh".to_string(),
                "http://gh3.example:8080/base".to_string()
            ]
        );
        assert_eq!(
            parse_nodes(r#"["https://a.example"]"#),
            vec!["https://a.example".to_string()]
        );
        assert!(parse_nodes("not json").is_empty());
        assert!(parse_nodes(r#"{"data":[]}"#).is_empty());
    }

    #[test]
    fn parses_curl_timing() {
        assert_eq!(parse_code_time("200 0.123456\n"), Some((200, 0.123456)));
        assert_eq!(parse_code_time("\n404 0.010\n"), Some((404, 0.010)));
        assert_eq!(parse_code_time("garbage"), None);
        assert_eq!(parse_code_time("200 -1"), None);
    }

    #[test]
    fn sha256_known_vector() {
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    #[test]
    fn cache_roundtrip_and_expiry() {
        let tests = vec![
            NodeTest {
                url: "https://a.example".into(),
                latency_ms: Some(120),
                verified: true,
                detail: "ok".into(),
            },
            NodeTest {
                url: "https://b.example".into(),
                latency_ms: Some(40),
                verified: true,
                detail: "ok".into(),
            },
            NodeTest {
                url: "https://bad.example".into(),
                latency_ms: None,
                verified: false,
                detail: "hash".into(),
            },
        ];
        let cache = NodeCache::from_tests(&tests, now_secs());
        assert_eq!(cache.nodes.len(), 2);
        assert_eq!(cache.best().unwrap().url, "https://b.example");

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("nodes.json");
        cache.save(&path).unwrap();
        assert_eq!(NodeCache::load(&path), Some(cache.clone()));

        let expired = NodeCache {
            tested_at: now_secs().saturating_sub(NODE_CACHE_TTL_SECS + 1),
            nodes: cache.nodes,
        };
        expired.save(&path).unwrap();
        assert!(NodeCache::load(&path).is_none());
    }

    #[test]
    fn parses_cf_trace() {
        let text = "fl=1\nip=203.0.113.9\nloc=cn\nts=1\n";
        let location = parse_cf_trace(text).unwrap();
        assert_eq!(location.ip, "203.0.113.9");
        assert_eq!(location.country, "CN");
        assert!(location.in_china);
        assert_eq!(location.country_name(), "中国大陆");
        assert!(parse_cf_trace("ip=1.2.3.4").is_none());
        assert!(parse_cf_trace("loc=CHN").is_none());
    }

    #[test]
    fn parses_geo_json() {
        let ipapi = r#"{"ip":"198.51.100.7","location":{"country_code":"JP"}}"#;
        let location = parse_geo_json(ipapi).unwrap();
        assert_eq!(location.country, "JP");
        assert!(!location.in_china);

        let ipsb = r#"{"ip":"198.51.100.8","country":"China","country_code":"CN"}"#;
        assert!(parse_geo_json(ipsb).unwrap().in_china);

        let nested = r#"{"ip":"198.51.100.9","country":{"iso_code":"DE"}}"#;
        assert_eq!(parse_geo_json(nested).unwrap().country, "DE");

        assert!(parse_geo_json("{}").is_none());
        assert!(parse_geo_json("nope").is_none());
    }

    fn icon_server(body: Arc<Vec<u8>>) -> (String, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let handle = std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { continue };
                let mut buf = [0u8; 4096];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: image/png\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
            }
        });
        (format!("http://127.0.0.1:{port}"), handle)
    }

    #[test]
    fn verifies_node_by_hash() {
        let body = Arc::new(b"fake-icon-bytes".to_vec());
        let hash = sha256_hex(&body);
        let (server, handle) = icon_server(body);

        let good = test_node(&server, &hash);
        assert!(good.verified, "{}", good.detail);
        assert!(good.latency_ms.is_some_and(|latency| latency >= 1));
        assert!(good.verdict().contains("完整性校验通过"));

        let bad = test_node(&server, &"0".repeat(64));
        assert!(!bad.verified);
        assert!(bad.detail.contains("哈希"));

        drop(handle);
    }

    #[test]
    fn concurrent_tests_are_sorted() {
        let body = Arc::new(b"another-icon".to_vec());
        let hash = sha256_hex(&body);
        let (server, handle) = icon_server(body);
        let nodes = vec![server.clone(), format!("{server}/a"), format!("{server}/b")];
        let results = test_nodes(&nodes, &hash, 32);
        assert_eq!(results.len(), 3);
        assert!(results.iter().all(|result| result.verified));
        let mut sorted = results.clone();
        sorted.sort_by_key(|node| node.latency_ms);
        assert_eq!(results, sorted);
        drop(handle);
    }
}
