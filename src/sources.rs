//! 节点清单接口（nodes.json）的地址管理。
//!
//! 用户可以维护多个接口，并选择其中一个作为“当前接口”；拉取云端节点
//! 清单时只访问它。接口地址原样保存，允许带查询串（`?token=...`）。

use std::io::Write;
use std::path::{Path, PathBuf};

use crate::error::{Error, IoContext, Result};
use crate::paths::Paths;

/// 默认接口：与用户脚本使用的节点源一致。
pub const DEFAULT_SOURCE: &str = "https://hubp.tbedu.top/nodes.json";

/// 校验并清理一个接口地址。
///
/// 与加速前缀不同，接口允许路径和查询串，因此这里只做最小校验。
pub fn normalize(input: &str) -> Option<String> {
    let text = input.trim().trim_matches(|c: char| "\"'`<>".contains(c));
    if text.is_empty() || text.contains(char::is_whitespace) {
        return None;
    }
    let rest = text
        .strip_prefix("https://")
        .or_else(|| text.strip_prefix("http://"))?;
    let host = rest.split(['/', '?', '#']).next()?;
    if host.is_empty()
        || !host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '[' | ']' | ':'))
    {
        return None;
    }
    Some(text.to_string())
}

/// `sources` 文件：一行一个接口地址。
#[derive(Debug, Clone)]
pub struct SourceList {
    path: PathBuf,
}

impl SourceList {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self { path: path.into() }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// 读取接口列表（去空行、注释、非法项与重复项）。
    pub fn load(&self) -> Result<Vec<String>> {
        let text = match std::fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(err) => return Err(Error::io(&self.path, err)),
        };
        let mut out = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            if let Some(url) = normalize(line) {
                if !out.contains(&url) {
                    out.push(url);
                }
            }
        }
        Ok(out)
    }

    /// 原子写入接口列表。
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

    /// 添加一个接口（自动去重）。
    pub fn add(&self, input: &str) -> Result<String> {
        let url = normalize(input)
            .ok_or_else(|| Error::msg(format!("不是有效的接口地址: {}", input.trim())))?;
        let mut list = self.load()?;
        if !list.contains(&url) {
            list.push(url.clone());
            self.save(&list)?;
        }
        Ok(url)
    }

    /// 删除一个接口；返回是否真的删掉了。
    pub fn remove(&self, url: &str) -> Result<bool> {
        let target = normalize(url).unwrap_or_else(|| url.trim().to_string());
        let mut list = self.load()?;
        let before = list.len();
        list.retain(|item| item != &target);
        if list.len() == before {
            return Ok(false);
        }
        self.save(&list)?;
        Ok(true)
    }

    /// 列表为空时写入默认接口，返回当前列表。
    pub fn ensure_default(&self, default: &str) -> Result<Vec<String>> {
        let mut list = self.load()?;
        if list.is_empty() {
            list.push(default.to_string());
            self.save(&list)?;
        }
        Ok(list)
    }
}

/// 选出当前要使用的接口地址。
///
/// 优先级：测试用的 `GHPM_NODES_URL` → `source` 文件里保存且仍在列表中的地址
/// → 列表第一个 → 内置默认接口。
pub fn active(paths: &Paths) -> String {
    if let Ok(value) = std::env::var("GHPM_NODES_URL") {
        let value = value.trim();
        if !value.is_empty() {
            return value.to_string();
        }
    }

    let list = SourceList::new(&paths.sources_file)
        .load()
        .unwrap_or_default();
    let saved = std::fs::read_to_string(&paths.source_file)
        .ok()
        .map(|text| text.trim().to_string())
        .filter(|text| !text.is_empty());

    if let Some(saved) = saved {
        if list.is_empty() || list.contains(&saved) {
            return saved;
        }
    }
    list.first()
        .cloned()
        .unwrap_or_else(|| DEFAULT_SOURCE.to_string())
}

/// 保存当前接口（同时确保它在接口列表里）。
pub fn set_active(paths: &Paths, url: &str) -> Result<String> {
    let url = normalize(url).ok_or_else(|| Error::msg(format!("不是有效的接口地址: {url}")))?;
    SourceList::new(&paths.sources_file).add(&url)?;
    if let Some(dir) = paths.source_file.parent() {
        std::fs::create_dir_all(dir).ctx(dir)?;
    }
    std::fs::write(&paths.source_file, format!("{url}\n")).ctx(&paths.source_file)?;
    Ok(url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_urls() {
        assert_eq!(
            normalize("https://hub.example/nodes.json").as_deref(),
            Some("https://hub.example/nodes.json")
        );
        assert_eq!(
            normalize(" http://127.0.0.1:8080/nodes.json?t=1 ").as_deref(),
            Some("http://127.0.0.1:8080/nodes.json?t=1")
        );
        assert_eq!(normalize("ftp://hub.example/nodes.json"), None);
        assert_eq!(normalize("https://hub.example/nodes list.json"), None);
        assert_eq!(normalize("https://"), None);
    }

    #[test]
    fn add_dedupes_and_removes() {
        let dir = tempfile::tempdir().unwrap();
        let list = SourceList::new(dir.path().join("sources"));
        list.add("https://a.example/nodes.json").unwrap();
        list.add("https://a.example/nodes.json").unwrap();
        list.add("https://b.example/nodes.json").unwrap();
        assert_eq!(
            list.load().unwrap(),
            vec![
                "https://a.example/nodes.json",
                "https://b.example/nodes.json"
            ]
        );
        assert!(list.remove("https://a.example/nodes.json").unwrap());
        assert!(!list.remove("https://a.example/nodes.json").unwrap());
    }

    #[test]
    fn active_prefers_saved_then_first() {
        let dir = tempfile::tempdir().unwrap();
        let paths = Paths::new(
            dir.path().join("root"),
            dir.path().to_path_buf(),
            dir.path().join("config"),
            "https://test.invalid/",
        );
        let list = SourceList::new(&paths.sources_file);
        list.ensure_default(DEFAULT_SOURCE).unwrap();
        assert_eq!(active(&paths), DEFAULT_SOURCE);

        list.add("https://b.example/nodes.json").unwrap();
        set_active(&paths, "https://b.example/nodes.json").unwrap();
        assert_eq!(active(&paths), "https://b.example/nodes.json");
    }
}
