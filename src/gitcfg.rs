//! git `url.<prefix>.insteadOf` 规则的解析与同步。
//!
//! 只处理值恰好是 `https://github.com/` 的规则，绝不碰用户自己的其它
//! `insteadOf` 配置。所有决策都先在纯函数 [`plan`] 里算好，再执行。

use std::process::Command;

use crate::error::{Error, Result};
use crate::logging::Logger;
use crate::paths::GIT_HOST_PREFIX;
use crate::shell;

/// `git config --list` 的一行。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub key: String,
    pub value: String,
}

/// 从 `git config --list` 的输出解析条目。
pub fn parse_list(text: &str) -> Vec<Entry> {
    text.lines()
        .filter_map(|line| {
            let (key, value) = line.split_once('=')?;
            Some(Entry {
                key: key.trim().to_string(),
                value: value.to_string(),
            })
        })
        .collect()
}

fn is_github_rule(entry: &Entry) -> bool {
    let key = entry.key.to_ascii_lowercase();
    key.starts_with("url.") && key.ends_with(".insteadof") && entry.value.trim() == GIT_HOST_PREFIX
}

fn section_of(entry: &Entry) -> Option<&str> {
    let key = entry.key.as_str();
    // "url." + section + ".insteadof"
    if key.len() <= 4 + ".insteadof".len() {
        return None;
    }
    Some(&key[4..key.len() - ".insteadof".len()])
}

/// 当前生效的镜像前缀（来自 github 的 insteadOf 规则）。
pub fn active_prefix(entries: &[Entry]) -> Option<String> {
    entries.iter().find_map(|entry| {
        if !is_github_rule(entry) {
            return None;
        }
        let section = section_of(entry)?;
        section
            .strip_suffix(GIT_HOST_PREFIX)
            .filter(|prefix| !prefix.is_empty())
            .map(|prefix| {
                if prefix.ends_with('/') {
                    prefix.to_string()
                } else {
                    format!("{prefix}/")
                }
            })
    })
}

/// 需要执行的 git 配置变更。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Plan {
    /// 要整段删除的 section 名（不含 `url.` 前缀）。
    pub remove_sections: Vec<String>,
    /// 要写入的 `(key, value)`。
    pub set: Option<(String, String)>,
}

/// 根据现有条目计算变更计划。`prefix = None` 表示关闭加速。
pub fn plan(entries: &[Entry], prefix: Option<&str>) -> Plan {
    let desired_section = prefix.map(|p| format!("{p}{GIT_HOST_PREFIX}"));
    let mut plan = Plan::default();

    for entry in entries {
        if !is_github_rule(entry) {
            continue;
        }
        let Some(section) = section_of(entry) else {
            continue;
        };
        let keep = desired_section.as_deref() == Some(section);
        if !keep && !plan.remove_sections.iter().any(|s| s == section) {
            plan.remove_sections.push(section.to_string());
        }
    }

    if let Some(section) = desired_section {
        plan.set = Some((
            format!("url.{section}.insteadOf"),
            GIT_HOST_PREFIX.to_string(),
        ));
    }
    plan
}

/// 构造 `git config` 命令基座。
///
/// 默认写全局配置（`--global`）；设置 `GHPM_GIT_CONFIG` 时改用 `--file`，
/// 测试可据此指向临时文件，绝不碰用户真实的 `~/.gitconfig`。
fn config_command() -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("config");
    match std::env::var_os("GHPM_GIT_CONFIG") {
        Some(path) if !path.is_empty() => {
            cmd.arg("--file").arg(path);
        }
        _ => {
            cmd.arg("--global");
        }
    }
    cmd
}

/// 读取 git 配置；git 不存在时返回空列表。
pub fn global_entries() -> Vec<Entry> {
    if !shell::have("git") {
        return Vec::new();
    }
    match config_command().arg("--list").output() {
        Ok(out) if out.status.success() => parse_list(&String::from_utf8_lossy(&out.stdout)),
        _ => Vec::new(),
    }
}

fn run_git(args: &[&str]) -> Result<()> {
    let out = config_command()
        .args(args)
        .output()
        .map_err(|e| Error::io("git", e))?;
    if out.status.success() {
        return Ok(());
    }
    Err(Error::Command {
        program: format!("git {}", args.join(" ")),
        code: out.status.code(),
        stderr: String::from_utf8_lossy(&out.stderr).to_string(),
    })
}

/// 把全局配置同步到期望状态。
pub fn sync(prefix: Option<&str>, logger: &Logger) -> Result<()> {
    if !shell::have("git") {
        if prefix.is_some() {
            return Err(Error::msg("未找到 git，无法启用 git clone 加速"));
        }
        return Ok(());
    }

    let entries = global_entries();
    let plan = plan(&entries, prefix);
    logger.log(format!(
        "git 同步: prefix={:?} 删除 sections={:?} 写入={:?}",
        prefix, plan.remove_sections, plan.set
    ));

    for section in &plan.remove_sections {
        let section = format!("url.{section}");
        match run_git(&["--remove-section", &section]) {
            Ok(()) => logger.log(format!("git 已删除 section {section}")),
            Err(err) => logger.log(format!("git 删除 section 失败（忽略）: {err}")),
        }
    }

    if let Some((key, value)) = plan.set {
        run_git(&["--replace-all", &key, &value])?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(section: &str) -> Entry {
        Entry {
            key: format!("url.{section}.insteadof"),
            value: GIT_HOST_PREFIX.to_string(),
        }
    }

    #[test]
    fn parses_list_output() {
        let text = "user.name=u\nurl.https://p.example/https://github.com/.insteadof=https://github.com/\n";
        let entries = parse_list(text);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            active_prefix(&entries).as_deref(),
            Some("https://p.example/")
        );
    }

    #[test]
    fn active_prefix_only_matches_github_rules() {
        let entries = vec![Entry {
            key: "url.https://p.example/https://gitlab.com/.insteadof".into(),
            value: "https://gitlab.com/".into(),
        }];
        assert_eq!(active_prefix(&entries), None);
    }

    #[test]
    fn plan_switches_prefix() {
        let entries = vec![entry("https://old.example/https://github.com/")];
        let plan = plan(&entries, Some("https://new.example/"));
        assert_eq!(
            plan.remove_sections,
            vec!["https://old.example/https://github.com/"]
        );
        assert_eq!(
            plan.set,
            Some((
                "url.https://new.example/https://github.com/.insteadOf".to_string(),
                GIT_HOST_PREFIX.to_string()
            ))
        );
    }

    #[test]
    fn plan_keeps_same_prefix() {
        let entries = vec![entry("https://same.example/https://github.com/")];
        let plan = plan(&entries, Some("https://same.example/"));
        assert!(plan.remove_sections.is_empty());
        assert!(plan.set.is_some());
    }

    #[test]
    fn plan_disables() {
        let entries = vec![
            entry("https://a.example/https://github.com/"),
            entry("https://b.example/https://github.com/"),
        ];
        let plan = plan(&entries, None);
        assert_eq!(plan.remove_sections.len(), 2);
        assert_eq!(plan.set, None);
    }

    #[test]
    fn ignores_foreign_insteadof() {
        let entries = vec![Entry {
            key: "url.https://mirror.example/gitlab/.insteadof".into(),
            value: "https://gitlab.com/".into(),
        }];
        assert_eq!(plan(&entries, None), Plan::default());
    }
}
