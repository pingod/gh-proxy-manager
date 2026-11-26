//! 加速前缀（镜像站）的规范化、判定与备选列表存储。
//!
//! 这里的所有逻辑都是纯函数或“只碰自己文件”的小结构体，方便单元测试；
//! 提权写入 `/etc` 的部分在 [`crate::apply`]。

use std::collections::HashMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::paths::DEFAULT_PREFIX;

/// 从用户粘贴的任意文本里提取并规范化一个加速前缀。
///
/// 规则：
/// - 取文本中**最后一个** `http(s)://` 开头的片段（兼容从网页/聊天软件复制的脏文本）
/// - 自动补末尾 `/`
/// - 没有协议时把整段文本当成主机名，补 `https://`
/// - 拒绝带查询串/锚点的地址（前缀拼接语义下没有意义），也拒绝非法字符
pub fn normalize(input: &str) -> Option<String> {
    let text = input.trim();
    if text.is_empty() {
        return None;
    }

    let raw = match last_scheme_start(text) {
        Some(pos) => {
            // URL 到空白为止；再剥掉中文标点/引号等复制时带上的尾巴。
            let tail = &text[pos..];
            let end = tail.find(char::is_whitespace).unwrap_or(tail.len());
            tail[..end].trim_end_matches(|c: char| "\"'`<>，。；、）)】》」".contains(c))
        }
        None => {
            // 没写协议：把整段当成 host[:port][/path]，仅在像地址时才补 https://
            let token = text.trim_matches(|c: char| c.is_whitespace() || "\"'`<>".contains(c));
            if token.is_empty() || token.contains(char::is_whitespace) || token.contains("://") {
                return None;
            }
            if !(token.contains('.') || token.starts_with("localhost")) {
                return None;
            }
            return canonicalize(&format!("https://{token}"));
        }
    };

    canonicalize(raw)
}

fn last_scheme_start(text: &str) -> Option<usize> {
    let https = text.rfind("https://");
    let http = text.rfind("http://");
    match (https, http) {
        (Some(a), Some(b)) => Some(a.max(b)),
        (a, b) => a.or(b),
    }
}

fn canonicalize(raw: &str) -> Option<String> {
    let (scheme, rest) = raw.split_once("://")?;
    let scheme = scheme.to_ascii_lowercase();
    if scheme != "http" && scheme != "https" {
        return None;
    }
    if rest.contains(['?', '#']) {
        // 前缀是拿来和原始 URL 直接拼接的，查询串会把结果拼坏。
        return None;
    }
    if rest.is_empty() || rest.contains(char::is_whitespace) {
        return None;
    }

    let auth_end = rest.find('/').unwrap_or(rest.len());
    let auth = &rest[..auth_end];
    if auth.is_empty() {
        return None;
    }
    let host_port = auth.rsplit('@').next().unwrap_or(auth);
    let host = match host_port.rsplit_once(':') {
        Some((h, port)) if !port.is_empty() => h,
        _ => host_port,
    };
    if host.is_empty() {
        return None;
    }
    if !host
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '[' | ']' | ':' | '_'))
    {
        return None;
    }

    let mut out = format!("{scheme}://{rest}");
    if !out.ends_with('/') {
        out.push('/');
    }
    Some(out)
}

/// 链接是否已经带了加速前缀（`https://proxy/https://github.com/...`）。
pub fn is_proxied(url: &str) -> bool {
    let Some(rest) = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
    else {
        return false;
    };
    match rest.find('/') {
        Some(i) => {
            let after = &rest[i + 1..];
            after.starts_with("https://") || after.starts_with("http://")
        }
        None => false,
    }
}

/// `raw.githubusercontent.com/user/repo/branch/path` →
/// `https://cdn.jsdelivr.net/gh/user/repo@branch/path`。
///
/// 旧版把分支段直接丢掉（走默认分支），对非默认分支的 raw 链接是错的；
/// 这里用 jsDelivr 的 `@branch` 语法保留分支。
pub fn raw_to_jsdelivr(url: &str) -> Option<String> {
    let rest = url
        .strip_prefix("https://raw.githubusercontent.com/")
        .or_else(|| url.strip_prefix("http://raw.githubusercontent.com/"))?;
    let rest = rest.split(['?', '#']).next()?;
    let mut parts = rest.splitn(4, '/');
    let user = parts.next()?;
    let repo = parts.next()?;
    let branch = parts.next()?;
    let path = parts.next()?;
    if user.is_empty() || repo.is_empty() || branch.is_empty() || path.is_empty() {
        return None;
    }
    Some(format!(
        "https://cdn.jsdelivr.net/gh/{user}/{repo}@{branch}/{path}"
    ))
}

/// 给 `get` 子命令用的 URL 改写：
/// - 已经带前缀 → 原样
/// - github.com → 前缀 + 原 URL
/// - raw.githubusercontent.com → 开启 jsDelivr 时转 CDN，否则前缀 + 原 URL
/// - gist.githubusercontent.com → 前缀 + 原 URL
/// - 其它地址（含未开启加速）→ 原样
pub fn rewrite_for_get(url: &str, prefix: Option<&str>, jsdelivr: bool) -> String {
    if is_proxied(url) {
        return url.to_string();
    }
    let prefix = prefix.filter(|p| !p.is_empty());
    if is_github_url(url, "github.com") {
        if let Some(prefix) = prefix {
            return format!("{prefix}{url}");
        }
    } else if is_github_url(url, "raw.githubusercontent.com") {
        if jsdelivr {
            if let Some(cdn) = raw_to_jsdelivr(url) {
                return cdn;
            }
        }
        if let Some(prefix) = prefix {
            return format!("{prefix}{url}");
        }
    } else if is_github_url(url, "gist.githubusercontent.com") {
        if let Some(prefix) = prefix {
            return format!("{prefix}{url}");
        }
    }
    url.to_string()
}

fn is_github_url(url: &str, host: &str) -> bool {
    url.strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))
        .map(|rest| rest.starts_with(host) && rest[host.len()..].starts_with('/'))
        .unwrap_or(false)
}

/// 备选地址列表：一行一个 URL。
#[derive(Debug, Clone)]
pub struct MirrorList {
    path: PathBuf,
}

impl MirrorList {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取并清洗列表（去空行、注释、非法项、重复项）。
    pub fn load(&self) -> Result<Vec<String>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::io(&self.path, err)),
        };
        Ok(parse_lines(&text))
    }

    /// 读取；文件为空时写入默认地址并返回。
    pub fn load_or_init(&self) -> Result<Vec<String>> {
        let list = self.load()?;
        if list.is_empty() {
            let list = vec![DEFAULT_PREFIX.to_string()];
            self.save(&list)?;
            return Ok(list);
        }
        Ok(list)
    }

    /// 原子写入整个列表。
    pub fn save(&self, list: &[String]) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ctx(dir)?;
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir).ctx(dir)?;
        for item in list {
            writeln!(tmp, "{item}").ctx(&self.path)?;
        }
        tmp.flush().ctx(&self.path)?;
        tmp.persist(&self.path)
            .map_err(|e| Error::io(&self.path, e.error))?;
        Ok(())
    }

    /// 添加一个地址（自动规范化、去重）。
    pub fn add(&self, input: &str) -> Result<String> {
        let url = normalize(input).ok_or_else(|| Error::InvalidPrefix(input.trim().to_string()))?;
        let mut list = self.load()?;
        if !list.contains(&url) {
            list.push(url.clone());
            self.save(&list)?;
        }
        Ok(url)
    }

    /// 批量并入地址：保留已有条目和顺序，跳过重复项，返回真正新增的地址。
    ///
    /// 用于把云端节点清单写进备选列表：不清空、不覆盖用户已有的地址，
    /// 只追加列表里还没有的。
    pub fn merge(&self, inputs: &[String]) -> Result<Vec<String>> {
        let mut list = self.load()?;
        let mut added = Vec::new();
        for input in inputs {
            let Some(url) = normalize(input) else {
                continue;
            };
            if !list.contains(&url) {
                list.push(url.clone());
                added.push(url);
            }
        }
        if !added.is_empty() {
            self.save(&list)?;
        }
        Ok(added)
    }

    /// 删除一个地址；返回是否真的删掉了。
    pub fn remove(&self, input: &str) -> Result<bool> {
        let target = normalize(input).unwrap_or_else(|| input.trim().to_string());
        let mut list = self.load()?;
        let before = list.len();
        list.retain(|item| {
            item != &target && item.trim_end_matches('/') != target.trim_end_matches('/')
        });
        if list.len() == before {
            return Ok(false);
        }
        self.save(&list)?;
        Ok(true)
    }
}

/// 从测速结论里取出速度（KB/s）。
///
/// 结论形如 `✓ 可用 · 141 KB/s`；失败（`✗ 不可用（HTTP 404）`）或
/// 尚未测过（没有结论）时返回 `None`。
pub fn verdict_speed(verdict: &str) -> Option<u64> {
    let head = verdict.split_once(" KB/s").map(|(head, _)| head)?;
    let digits = head.rsplit(|c: char| !c.is_ascii_digit()).next()?;
    digits.parse().ok()
}

/// 清洗文本形式的列表内容。
pub fn parse_lines(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some(url) = normalize(line) else {
            continue;
        };
        if !out.contains(&url) {
            out.push(url);
        }
    }
    out
}

/// 测速结果缓存（`地址<TAB>结论`）。
#[derive(Debug, Clone)]
pub struct ResultStore {
    path: PathBuf,
}

impl ResultStore {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn load(&self) -> HashMap<String, String> {
        let mut map = HashMap::new();
        let Ok(text) = std::fs::read_to_string(&self.path) else {
            return map;
        };
        for line in text.lines() {
            if let Some((url, verdict)) = line.split_once('\t') {
                map.insert(url.to_string(), verdict.to_string());
            }
        }
        map
    }

    pub fn get(&self, url: &str) -> Option<String> {
        self.load().remove(url)
    }

    /// 覆盖写入多条结果（`None` 表示保留原值）。
    pub fn merge(&self, updates: &[(String, String)]) -> Result<()> {
        let mut map = self.load();
        for (url, verdict) in updates {
            map.insert(url.clone(), verdict.clone());
        }
        self.write_map(&map)
    }

    pub fn write_map(&self, map: &HashMap<String, String>) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir).ctx(dir)?;
        }
        let dir = self.path.parent().unwrap_or(Path::new("."));
        let mut tmp = tempfile::NamedTempFile::new_in(dir).ctx(dir)?;
        let mut entries: Vec<_> = map.iter().collect();
        entries.sort_by(|a, b| a.0.cmp(b.0));
        for (url, verdict) in entries {
            writeln!(tmp, "{url}\t{verdict}").ctx(&self.path)?;
        }
        tmp.flush().ctx(&self.path)?;
        tmp.persist(&self.path)
            .map_err(|e| Error::io(&self.path, e.error))?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalize_basics() {
        assert_eq!(
            normalize("https://ghproxy.example/").as_deref(),
            Some("https://ghproxy.example/")
        );
        assert_eq!(
            normalize("  https://ghproxy.example  ").as_deref(),
            Some("https://ghproxy.example/")
        );
        assert_eq!(
            normalize("ghproxy.example/gh/").as_deref(),
            Some("https://ghproxy.example/gh/")
        );
    }

    #[test]
    fn normalize_from_dirty_paste() {
        assert_eq!(
            normalize("镜像站：https://a.example/ 备用 https://b.example/gh/ 用这个").as_deref(),
            Some("https://b.example/gh/")
        );
        assert_eq!(
            normalize("\"https://a.example/\"，").as_deref(),
            Some("https://a.example/")
        );
    }

    #[test]
    fn normalize_rejects_garbage() {
        assert_eq!(normalize(""), None);
        assert_eq!(normalize("ftp://a.example/"), None);
        assert_eq!(normalize("https://a.example/?token=1"), None);
        assert_eq!(normalize("not a url"), None);
        assert_eq!(normalize("https://"), None);
    }

    #[test]
    fn proxied_detection() {
        assert!(is_proxied("https://p.example/https://github.com/a/b"));
        assert!(is_proxied("http://p.example/http://github.com/a/b"));
        assert!(!is_proxied("https://github.com/a/b"));
        assert!(!is_proxied("https://p.example/github.com/a/b"));
        assert!(!is_proxied("https://p.example/"));
    }

    #[test]
    fn jsdelivr_conversion_keeps_branch() {
        assert_eq!(
            raw_to_jsdelivr("https://raw.githubusercontent.com/o/r/main/dir/file.sh").as_deref(),
            Some("https://cdn.jsdelivr.net/gh/o/r@main/dir/file.sh")
        );
        assert_eq!(
            raw_to_jsdelivr("https://raw.githubusercontent.com/o/r"),
            None
        );
        assert_eq!(raw_to_jsdelivr("https://example.com/o/r/main/f"), None);
    }

    #[test]
    fn get_rewrites_github_urls() {
        let gh = "https://github.com/o/r/releases/download/v1/f.tar.gz";
        assert_eq!(
            rewrite_for_get(gh, Some("https://p.example/"), false),
            "https://p.example/https://github.com/o/r/releases/download/v1/f.tar.gz"
        );
        assert_eq!(rewrite_for_get(gh, None, false), gh);
        assert_eq!(
            rewrite_for_get(
                "https://p.example/https://github.com/o/r",
                Some("https://q.example/"),
                true
            ),
            "https://p.example/https://github.com/o/r"
        );
        assert_eq!(
            rewrite_for_get("https://example.com/file", Some("https://p.example/"), true),
            "https://example.com/file"
        );
    }

    #[test]
    fn get_prefixes_gist_raw() {
        let gist = "https://gist.githubusercontent.com/u/1/raw/file.sh";
        assert_eq!(
            rewrite_for_get(gist, Some("https://p.example/"), false),
            format!("https://p.example/{gist}")
        );
        assert_eq!(
            rewrite_for_get(gist, Some("https://p.example/"), true),
            format!("https://p.example/{gist}")
        );
    }

    #[test]
    fn get_prefers_jsdelivr_for_raw() {
        let raw = "https://raw.githubusercontent.com/o/r/main/install.sh";
        assert_eq!(
            rewrite_for_get(raw, Some("https://p.example/"), true),
            "https://cdn.jsdelivr.net/gh/o/r@main/install.sh"
        );
        assert_eq!(
            rewrite_for_get(raw, Some("https://p.example/"), false),
            "https://p.example/https://raw.githubusercontent.com/o/r/main/install.sh"
        );
    }

    #[test]
    fn github_url_matching_is_strict() {
        assert!(!is_github_url(
            "https://github.com.evil.example/x",
            "github.com"
        ));
        assert!(!is_github_url("https://notgithub.com/x", "github.com"));
        assert!(is_github_url("http://github.com/x", "github.com"));
    }

    #[test]
    fn mirror_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let list = MirrorList::new(dir.path().join("proxies"));
        assert_eq!(list.load().unwrap(), Vec::<String>::new());
        assert_eq!(list.add("https://b.example").unwrap(), "https://b.example/");
        assert_eq!(
            list.add("https://b.example/").unwrap(),
            "https://b.example/"
        );
        list.add("a.example/gh/").unwrap();
        assert_eq!(
            list.load().unwrap(),
            vec!["https://b.example/", "https://a.example/gh/"]
        );
        assert!(list.remove("https://b.example").unwrap());
        assert!(!list.remove("https://b.example").unwrap());
        assert_eq!(list.load().unwrap(), vec!["https://a.example/gh/"]);
    }

    #[test]
    fn merge_appends_without_overwriting() {
        let dir = tempfile::tempdir().unwrap();
        let list = MirrorList::new(dir.path().join("proxies"));
        list.save(&["https://keep.example/".into()]).unwrap();

        let added = list
            .merge(&[
                "https://keep.example".to_string(),
                "https://new.example".to_string(),
                "https://new.example/".to_string(),
                "not a url".to_string(),
            ])
            .unwrap();

        assert_eq!(added, vec!["https://new.example/"]);
        assert_eq!(
            list.load().unwrap(),
            vec!["https://keep.example/", "https://new.example/"]
        );
        assert!(
            list.merge(&["https://keep.example/".into()])
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn verdict_speed_is_parsed() {
        assert_eq!(verdict_speed("✓ 可用 · 141 KB/s"), Some(141));
        assert_eq!(verdict_speed("✓ 可用 · 1024 KB/s"), Some(1024));
        assert_eq!(verdict_speed("✗ 不可用（HTTP 404）"), None);
        assert_eq!(verdict_speed("未测试"), None);
    }

    #[test]
    fn parse_lines_cleans_dirty_file() {
        let text =
            "\n# comment\nhttps://a.example\nhttps://a.example/\nbad line\nhttps://b.example/gh\n";
        assert_eq!(
            parse_lines(text),
            vec!["https://a.example/", "https://b.example/gh/"]
        );
    }

    #[test]
    fn result_store_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ResultStore::new(dir.path().join("results"));
        store
            .merge(&[("https://a.example/".into(), "✓ 1 KB/s".into())])
            .unwrap();
        assert_eq!(store.get("https://a.example/").as_deref(), Some("✓ 1 KB/s"));
        store
            .merge(&[
                ("https://a.example/".into(), "✓ 2 KB/s".into()),
                ("https://b.example/".into(), "✗ 404".into()),
            ])
            .unwrap();
        let map = store.load();
        assert_eq!(map.len(), 2);
        assert_eq!(map["https://a.example/"], "✓ 2 KB/s");
    }
}
