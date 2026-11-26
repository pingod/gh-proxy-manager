//! 命令行前端。所有子命令共享同一套核心逻辑，行为与 GUI 完全一致。

use std::io::{IsTerminal, Write};
use std::path::Path;

use clap::{Parser, Subcommand, ValueEnum};

use crate::accelerator::{self, NodeCache};
use crate::apply::{self, Settings};
use crate::error::{Error, Result};
use crate::exec::{Runner, SystemRunner};
use crate::gitcfg;
use crate::logging::{self, Logger};
use crate::mirrors::{self, MirrorList, ResultStore};
use crate::paths::{DEFAULT_PREFIX, Paths};
use crate::prefs;
use crate::probe;
use crate::shell;
use crate::sources;
use crate::state::{State, on_off};

/// 顶层命令行参数。
#[derive(Debug, Parser)]
#[command(
    name = "gh-proxy-manager",
    version,
    about = "管理 GitHub 下载加速（yay/makepkg、git clone、curl）",
    long_about = None,
    disable_help_subcommand = true
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Cmd>,
}

#[derive(Debug, Subcommand)]
pub enum Cmd {
    /// 打开图形界面
    Gui,
    /// 开启 yay/makepkg 下载加速
    On,
    /// 关闭 yay/makepkg 下载加速
    Off,
    /// 开启 git clone 加速
    GitOn,
    /// 关闭 git clone 加速
    GitOff,
    /// 开启/关闭 raw.githubusercontent.com 的 jsDelivr CDN 加速
    Jsdelivr {
        #[arg(value_enum)]
        state: Toggle,
    },
    /// 开启/关闭失败或龟速时自动换站
    Failover {
        #[arg(value_enum)]
        state: Toggle,
    },
    /// 查看或设置龟速换站阈值（KB/s，允许 5–500）
    Slow {
        /// 低于该速度持续 30 秒即换站；省略则只显示当前值
        kbps: Option<u32>,
    },
    /// 设置生效的加速前缀（如 https://ghfast.top/）
    #[command(alias = "use")]
    Url { prefix: String },
    /// 添加备选加速地址
    Add { url: String },
    /// 删除备选加速地址
    #[command(alias = "remove")]
    Del { url: String },
    /// 列出备选加速地址
    Proxies,
    /// 显示当前状态
    Status,
    /// 测速（下载真实 release 文件）
    Test {
        /// 测试全部备选地址
        #[arg(long)]
        all: bool,
    },
    /// 拉取云端节点列表，测速并做 SHA-256 完整性校验
    Nodes {
        /// 忽略 2 小时缓存，强制重新测速
        #[arg(long)]
        refresh: bool,
        /// 输出 JSON
        #[arg(long)]
        json: bool,
        /// 测速并发数
        #[arg(long, default_value_t = accelerator::DEFAULT_CONCURRENCY)]
        concurrency: usize,
    },
    /// 测速并自动选择最快的节点作为生效地址
    Auto {
        /// 只显示结果，不写入系统
        #[arg(long)]
        dry_run: bool,
    },
    /// 显示一个 GitHub 链接会被怎样改写（不下载）
    Resolve { url: String },
    /// 检测当前出口地区（中国大陆/境外）
    #[command(alias = "where")]
    Region,
    /// 按当前加速配置下载 URL
    Get {
        url: String,
        #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
        curl_args: Vec<String>,
    },
    /// 查看调试日志
    Log {
        /// 只显示最后 N 行
        #[arg(short = 'n', long)]
        lines: Option<usize>,
        /// 只显示不低于该级别的记录
        #[arg(long, value_enum, default_value_t = LevelArg::Info)]
        level: LevelArg,
    },
    /// 删除所有运行时文件（卸载后清理用）
    Reset {
        /// 不询问确认
        #[arg(short, long)]
        yes: bool,
        /// 连同 ~/.config/gh-proxy-manager 一起删除
        #[arg(long)]
        purge: bool,
    },
}

/// `log --level` 的取值。
#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum LevelArg {
    Debug,
    Info,
    Warn,
    Error,
}

impl From<LevelArg> for logging::Level {
    fn from(value: LevelArg) -> Self {
        match value {
            LevelArg::Debug => logging::Level::Debug,
            LevelArg::Info => logging::Level::Info,
            LevelArg::Warn => logging::Level::Warn,
            LevelArg::Error => logging::Level::Error,
        }
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
pub enum Toggle {
    On,
    Off,
}

impl Toggle {
    fn enabled(self) -> bool {
        matches!(self, Self::On)
    }
}

struct Ctx {
    paths: Paths,
    logger: Logger,
}

/// 执行命令，返回进程退出码。
pub fn run(cli: Cli) -> Result<i32> {
    let paths = Paths::system()?;
    let logger = Logger::new(paths.log_file.clone());
    let ctx = Ctx { paths, logger };
    let command = cli.command.unwrap_or_else(default_command);

    match command {
        Cmd::Gui => launch_gui(),
        Cmd::On => adjust(&ctx, |s| s.makepkg = true),
        Cmd::Off => adjust(&ctx, |s| s.makepkg = false),
        Cmd::GitOn => adjust(&ctx, |s| s.git = true),
        Cmd::GitOff => adjust(&ctx, |s| s.git = false),
        Cmd::Jsdelivr { state } => adjust(&ctx, move |s| s.jsdelivr = state.enabled()),
        Cmd::Failover { state } => adjust(&ctx, move |s| s.failover = state.enabled()),
        Cmd::Slow { kbps } => run_slow(&ctx, kbps),
        Cmd::Url { prefix } => adjust(&ctx, move |s| s.prefix = prefix),
        Cmd::Add { url } => {
            let list = MirrorList::new(&ctx.paths.mirrors_file);
            list.load_or_init()?; // 首次使用也保留内置默认站作为兜底
            let url = list.add(&url)?;
            println!("已添加: {url}");
            Ok(0)
        }
        Cmd::Del { url } => {
            let list = MirrorList::new(&ctx.paths.mirrors_file);
            if list.remove(&url)? {
                println!("已删除: {url}");
                Ok(0)
            } else {
                println!("列表中不存在: {url}");
                Ok(1)
            }
        }
        Cmd::Proxies => {
            print_proxies(&ctx)?;
            Ok(0)
        }
        Cmd::Status => {
            print_status(&ctx)?;
            Ok(0)
        }
        Cmd::Test { all } => run_tests(&ctx, all),
        Cmd::Nodes {
            refresh,
            json,
            concurrency,
        } => run_nodes(&ctx, refresh, json, concurrency),
        Cmd::Auto { dry_run } => run_auto(&ctx, dry_run),
        Cmd::Resolve { url } => run_resolve(&ctx, &url),
        Cmd::Region => run_region(),
        Cmd::Get { url, curl_args } => run_get(&ctx, &url, &curl_args),
        Cmd::Log { lines, level } => run_log(&ctx, lines, level.into()),
        Cmd::Reset { yes, purge } => run_reset(&ctx, yes, purge),
    }
}

fn default_command() -> Cmd {
    #[cfg(feature = "gui")]
    {
        if std::env::var_os("DISPLAY").is_some() || std::env::var_os("WAYLAND_DISPLAY").is_some() {
            return Cmd::Gui;
        }
    }
    Cmd::Status
}

fn launch_gui() -> Result<i32> {
    #[cfg(feature = "gui")]
    {
        crate::gui::run()
    }
    #[cfg(not(feature = "gui"))]
    {
        Err(Error::msg(
            "本次构建未包含图形界面（编译时使用了 --no-default-features）",
        ))
    }
}

/// 以当前状态为基准修改若干项并应用。
fn adjust(ctx: &Ctx, change: impl FnOnce(&mut Settings)) -> Result<i32> {
    let state = State::detect(&ctx.paths);
    let mut settings = Settings::from_state(&state);
    change(&mut settings);
    apply_settings(ctx, settings)
}

fn apply_settings(ctx: &Ctx, settings: Settings) -> Result<i32> {
    let runner = SystemRunner::new(ctx.paths.sandboxed(), std::io::stdin().is_terminal());
    apply::apply(&ctx.paths, &settings, &runner, &ctx.logger)?;
    let state = State::detect(&ctx.paths);
    println!("已应用 ✓\n{}", state.summary());
    Ok(0)
}

fn run_slow(ctx: &Ctx, kbps: Option<u32>) -> Result<i32> {
    match kbps {
        None => println!(
            "龟速换站阈值: {} KB/s（低于该值持续 {} 秒即换站）",
            prefs::load_slow_kbps(&ctx.paths.slow_file),
            prefs::SLOW_TIME_SECS
        ),
        Some(wanted) => {
            let saved = prefs::save_slow_kbps(&ctx.paths.slow_file, wanted)?;
            if saved != wanted {
                println!(
                    "允许范围是 {}–{} KB/s，已按 {} KB/s 保存",
                    prefs::MIN_SLOW_KBPS,
                    prefs::MAX_SLOW_KBPS,
                    saved
                );
            }
            ctx.logger.debug(format!(
                "[设置] 龟速换站阈值已改为 {saved} KB/s（命令行，新下载立即生效）"
            ));
            println!("龟速换站阈值已设为 {saved} KB/s（新下载立即生效）");
        }
    }
    Ok(0)
}

fn print_status(ctx: &Ctx) -> Result<()> {
    let state = State::detect(&ctx.paths);
    println!(
        "makepkg/yay 加速: {}    生效地址: {}",
        on_off(state.makepkg),
        state.prefix.as_deref().unwrap_or("—")
    );
    println!(
        "git clone 加速:   {}    jsDelivr: {}    自动换站: {}",
        on_off(state.git),
        on_off(state.jsdelivr),
        on_off(state.failover)
    );
    println!(
        "龟速换站阈值:     {} KB/s（持续 {} 秒）",
        prefs::load_slow_kbps(&ctx.paths.slow_file),
        prefs::SLOW_TIME_SECS
    );
    if let Some(cache) = NodeCache::load(&ctx.paths.nodes_file) {
        if let Some(best) = cache.best() {
            println!(
                "节点缓存: {} 个 · 最快 {}（{} ms）· {} 分钟前",
                cache.nodes.len(),
                best.url,
                best.latency_ms,
                cache.age_secs() / 60
            );
        }
    }
    println!();
    print_proxies(ctx)?;
    Ok(())
}

fn print_proxies(ctx: &Ctx) -> Result<()> {
    let state = State::detect(&ctx.paths);
    let list = MirrorList::new(&ctx.paths.mirrors_file).load_or_init()?;
    let results = ResultStore::new(&ctx.paths.results_file).load();

    println!("备选地址:");
    for (index, url) in list.iter().enumerate() {
        let active = if state.prefix.as_deref() == Some(url.as_str()) {
            " ★ 当前生效"
        } else {
            ""
        };
        let verdict = results
            .get(url)
            .map(|v| format!(" · {v}"))
            .unwrap_or_default();
        println!("  {}. {url}{active}{verdict}", index + 1);
    }
    if state.prefix.is_none() {
        println!("（尚未写入生效地址；默认将使用 {DEFAULT_PREFIX}）");
    }
    Ok(())
}

fn run_tests(ctx: &Ctx, all: bool) -> Result<i32> {
    let state = State::detect(&ctx.paths);
    let store = ResultStore::new(&ctx.paths.results_file);

    if !all {
        let prefix = state.effective_prefix();
        println!("测试 {prefix} …（下载真实 release 文件）");
        let result = probe::probe_prefix(&prefix, &ctx.paths.test_url);
        let verdict = result.verdict();
        store.merge(&[(prefix.clone(), verdict.clone())])?;
        if verdict.starts_with('✓') {
            ctx.logger
                .log(format!("[测速] 当前生效地址 {prefix} → {verdict}"));
        } else {
            ctx.logger
                .warn(format!("[测速] 当前生效地址 {prefix} → {verdict}"));
        }
        println!("{verdict}");
        return Ok(if result.ok() { 0 } else { 1 });
    }

    let list = MirrorList::new(&ctx.paths.mirrors_file).load_or_init()?;
    let mut updates = Vec::new();
    for url in list {
        eprintln!("测试中: {url}");
        let result = probe::probe_prefix(&url, &ctx.paths.test_url);
        let verdict = result.verdict();
        if verdict.starts_with('✓') {
            ctx.logger.log(format!("[测速] {url} → {verdict}"));
        } else {
            ctx.logger.warn(format!("[测速] {url} → {verdict}"));
        }
        println!("{url}\t{verdict}");
        updates.push((url, verdict));
    }
    store.merge(&updates)?;
    Ok(0)
}

/// 一次节点扫描的结果。
struct NodeScan {
    cache: NodeCache,
    cached: bool,
    fallback: bool,
    /// 本次使用的节点清单接口。
    source: String,
    /// 缓存命中时为空；新扫描时包含每个节点的完整测速结论。
    tests: Vec<accelerator::NodeTest>,
}

/// 返回一份可用的节点缓存：优先读缓存，否则拉取、测速、校验并落盘。
fn node_scan(ctx: &Ctx, refresh: bool, concurrency: usize) -> Result<NodeScan> {
    let source = sources::active(&ctx.paths);
    if !refresh {
        if let Some(cache) = NodeCache::load(&ctx.paths.nodes_file) {
            return Ok(NodeScan {
                cache,
                cached: true,
                fallback: false,
                source,
                tests: Vec::new(),
            });
        }
    }

    let (nodes, fallback) = match accelerator::fetch_nodes_from(&source) {
        Ok(nodes) => (nodes, false),
        Err(err) => {
            eprintln!("从 {source} 拉取节点列表失败（{err}），改用内置兜底节点");
            (accelerator::fallback_nodes(), true)
        }
    };
    ctx.logger.log(format!(
        "[节点] 使用接口 {source}，拉取到 {} 个节点",
        nodes.len()
    ));
    if fallback {
        ctx.logger.warn("[节点] 接口不可用，本次使用内置兜底节点");
    }

    let tests = match accelerator::fetch_expected_hash() {
        Ok(expected) => {
            eprintln!("正在测速 {} 个节点（并发 {}）…", nodes.len(), concurrency);
            accelerator::test_nodes(&nodes, &expected, concurrency)
        }
        Err(err) => {
            // 校验值拿不到时仍然把地址全部收进备选列表，只是标记为未校验。
            eprintln!("无法获取节点完整性校验值（{err}），本次只收集地址、不做校验");
            ctx.logger.warn(format!(
                "[节点] 无法获取完整性校验值：{err}；本次只收集地址，不做校验"
            ));
            nodes
                .iter()
                .map(|node| accelerator::NodeTest::failed(node, "未校验（无法获取完整性校验值）"))
                .collect()
        }
    };

    let cache = NodeCache::from_tests(&tests, accelerator::now_secs());
    let verified = tests.iter().filter(|test| test.verified).count();
    cache.save(&ctx.paths.nodes_file)?;
    ctx.logger.log(format!(
        "[节点] 测速完成：共 {} 个节点，{} 个通过完整性校验",
        tests.len(),
        verified
    ));
    for test in tests.iter().filter(|test| test.verified) {
        ctx.logger.debug(format!(
            "[节点] 通过完整性校验：{}（{} ms）",
            test.url,
            test.latency_ms.unwrap_or(0)
        ));
    }
    for test in tests.iter().filter(|test| !test.verified) {
        ctx.logger.warn(format!(
            "[节点] 未通过完整性校验：{}（{}）",
            test.url, test.detail
        ));
    }
    Ok(NodeScan {
        cache,
        cached: false,
        fallback,
        source,
        tests,
    })
}

/// 把一次扫描里的所有地址并入备选列表：不覆盖、去重、跳过黑名单，返回新增数量。
fn merge_scan_into_mirrors(ctx: &Ctx, scan: &NodeScan) -> Result<usize> {
    let blacklist = MirrorList::new(&ctx.paths.blacklist_file)
        .load()
        .unwrap_or_default();
    let pairs: Vec<(String, String)> = if scan.cached {
        scan.cache
            .nodes
            .iter()
            .map(|node| {
                let url = mirrors::normalize(&node.url).unwrap_or_else(|| node.url.clone());
                (url, node.verdict())
            })
            .collect()
    } else {
        scan.tests
            .iter()
            .map(|test| {
                let url = mirrors::normalize(&test.url).unwrap_or_else(|| test.url.clone());
                (url, test.verdict())
            })
            .collect()
    };
    let pairs: Vec<(String, String)> = pairs
        .into_iter()
        .filter(|(url, _)| !blacklist.contains(url))
        .collect();

    let candidates: Vec<String> = pairs.iter().map(|(url, _)| url.clone()).collect();
    let list = MirrorList::new(&ctx.paths.mirrors_file);
    list.load_or_init()?;
    let added = list.merge(&candidates)?;
    ResultStore::new(&ctx.paths.results_file).merge(&pairs)?;
    ctx.logger.log(format!(
        "[节点] 已并入备选地址：新增 {} 个（跳过黑名单）",
        added.len()
    ));
    Ok(added.len())
}

fn run_nodes(ctx: &Ctx, refresh: bool, json: bool, concurrency: usize) -> Result<i32> {
    let scan = node_scan(ctx, refresh, concurrency)?;
    let added = merge_scan_into_mirrors(ctx, &scan)?;
    let cache = &scan.cache;
    let best = cache.best();

    if json {
        let tests: Vec<serde_json::Value> = if scan.cached {
            cache
                .nodes
                .iter()
                .map(|node| {
                    serde_json::json!({
                        "url": node.url,
                        "latency_ms": node.latency_ms,
                        "verified": true,
                        "verdict": node.verdict(),
                    })
                })
                .collect()
        } else {
            scan.tests
                .iter()
                .map(|test| {
                    serde_json::json!({
                        "url": test.url,
                        "latency_ms": test.latency_ms,
                        "verified": test.verified,
                        "detail": test.detail,
                        "verdict": test.verdict(),
                    })
                })
                .collect()
        };
        let value = serde_json::json!({
            "source": scan.source,
            "cached": scan.cached,
            "fallback": scan.fallback,
            "tested_at": cache.tested_at,
            "age_secs": cache.age_secs(),
            "nodes": cache.nodes.iter().map(|node| serde_json::json!({
                "url": node.url,
                "latency_ms": node.latency_ms,
                "verified": true,
            })).collect::<Vec<_>>(),
            "tests": tests,
            "added_to_mirrors": added,
            "best": best.map(|node| node.url.clone()),
        });
        let text = serde_json::to_string_pretty(&value)
            .map_err(|err| Error::msg(format!("生成 JSON 失败: {err}")))?;
        println!("{text}");
    } else {
        println!("接口: {}", scan.source);
        if scan.cached {
            println!("云端节点（缓存 · {} 分钟前）:", cache.age_secs() / 60);
            for (index, node) in cache.nodes.iter().enumerate() {
                println!("  {}. {}  {} ms", index + 1, node.url, node.latency_ms);
            }
        } else {
            println!("云端节点测速结果:");
            for (index, test) in scan.tests.iter().enumerate() {
                println!("  {}. {}  {}", index + 1, test.url, test.verdict());
            }
        }
        if let Some(best) = best {
            println!("\n最快节点: {}（{} ms）", best.url, best.latency_ms);
        }
        if scan.fallback {
            println!("（节点列表源不可达，本次使用内置兜底节点）");
        }
        println!("已并入备选地址: 新增 {added} 个（已有地址不覆盖、不重复）");
    }
    Ok(0)
}

fn run_auto(ctx: &Ctx, dry_run: bool) -> Result<i32> {
    let scan = node_scan(ctx, false, accelerator::DEFAULT_CONCURRENCY)?;
    let cache = &scan.cache;
    let best = cache
        .best()
        .ok_or_else(|| Error::msg("没有通过完整性校验的节点；先运行 nodes --refresh 查看原因"))?;
    let best_prefix =
        mirrors::normalize(&best.url).ok_or_else(|| Error::InvalidPrefix(best.url.clone()))?;

    if dry_run {
        println!(
            "最快节点: {best_prefix}（{} ms{}）",
            best.latency_ms,
            if scan.cached { " · 缓存" } else { "" }
        );
        return Ok(0);
    }

    let list = MirrorList::new(&ctx.paths.mirrors_file);
    list.load_or_init()?;
    let verified: Vec<String> = cache.nodes.iter().map(|node| node.url.clone()).collect();
    list.merge(&verified)?;

    let state = State::detect(&ctx.paths);
    let mut settings = Settings::from_state(&state);
    settings.prefix = best_prefix.clone();
    apply_settings(ctx, settings)?;
    println!("已选择最快节点: {best_prefix}（{} ms）", best.latency_ms);
    Ok(0)
}

fn run_resolve(ctx: &Ctx, url: &str) -> Result<i32> {
    let state = State::detect(&ctx.paths);
    let canonical = accelerator::canonicalize(url);

    println!("原始链接: {url}");
    if let Some(target) = canonical.as_deref() {
        if target != url {
            println!("规范化:   {target}");
        }
    }

    if mirrors::is_proxied(url) {
        println!("已加速:   {url}");
        return Ok(0);
    }

    let Some(target) = canonical.as_deref() else {
        println!(
            "不是可识别的 GitHub 下载链接（支持 release / archive / raw / codeload / gist raw）"
        );
        return Ok(2);
    };

    let prefix = state.effective_prefix();
    println!("当前前缀: {prefix}");
    println!("加速链接: {prefix}{target}");

    if let Some(cache) = NodeCache::load(&ctx.paths.nodes_file) {
        if let Some(best) = cache.best() {
            let node = mirrors::normalize(&best.url).unwrap_or_else(|| best.url.clone());
            if node != prefix {
                println!("最快节点: {node}（{} ms）", best.latency_ms);
                println!("节点链接: {node}{target}");
            }
        }
    }
    Ok(0)
}

fn run_region() -> Result<i32> {
    match accelerator::detect_location() {
        Some(location) => {
            println!("出口 IP: {}", location.ip);
            println!(
                "地区:    {} ({})",
                location.country_name(),
                location.country
            );
            println!("建议:    {}", location.advice());
            Ok(0)
        }
        None => {
            eprintln!("无法检测出口地区（网络不可达）");
            Ok(1)
        }
    }
}

fn run_get(ctx: &Ctx, url: &str, curl_args: &[String]) -> Result<i32> {
    let state = State::detect(&ctx.paths);
    // codeload / blob 先还原成官方形态，再按前缀改写。
    let canonical = accelerator::canonicalize(url).unwrap_or_else(|| url.to_string());
    let target = mirrors::rewrite_for_get(&canonical, state.prefix.as_deref(), state.jsdelivr);
    ctx.logger.log(format!("[下载] {url} → {target}"));

    let status = std::process::Command::new("curl")
        .args(["-L", "--fail", "--retry", "3", "-O"])
        .args(curl_args)
        .arg(&target)
        .status()
        .map_err(|e| Error::io("curl", e))?;
    Ok(status.code().unwrap_or(1))
}

fn run_log(ctx: &Ctx, lines: Option<usize>, level: logging::Level) -> Result<i32> {
    let text = match std::fs::read_to_string(&ctx.paths.log_file) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            println!("暂无日志: {}", ctx.paths.log_file.display());
            return Ok(0);
        }
        Err(err) => return Err(Error::io(&ctx.paths.log_file, err)),
    };
    let text = logging::filter_at_least(&text, level);
    let all: Vec<&str> = text.lines().collect();
    let start = lines.map(|n| all.len().saturating_sub(n)).unwrap_or(0);
    for line in &all[start..] {
        println!("{line}");
    }
    Ok(0)
}

fn run_reset(ctx: &Ctx, yes: bool, purge: bool) -> Result<i32> {
    if !yes {
        confirm("将删除 /etc/gh-proxy、makepkg 覆盖配置、curl-gh 和 git 加速规则。继续？")?;
    }

    let q = |p: &Path| shell::quote(&p.display().to_string());
    let script = format!(
        "#!/usr/bin/env bash\nset -eu\nrm -f {} {} {} {} {}\nrmdir {} 2>/dev/null || true\n",
        q(&ctx.paths.prefix_file),
        q(&ctx.paths.jsdelivr_marker),
        q(&ctx.paths.nofailover_marker),
        q(&ctx.paths.curl_gh),
        q(&ctx.paths.dlagent_conf),
        q(&ctx.paths.etc_dir),
    );
    let runner = SystemRunner::new(ctx.paths.sandboxed(), std::io::stdin().is_terminal());
    runner.run_root(&script)?;
    gitcfg::sync(None, &ctx.logger)?;

    ctx.logger.log(format!(
        "[清理] 卸载清理完成（{}）",
        if purge {
            "含用户配置"
        } else {
            "保留用户配置"
        }
    ));

    if purge {
        match std::fs::remove_dir_all(&ctx.paths.user_dir) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(Error::io(&ctx.paths.user_dir, err)),
        }
    }
    println!(
        "已清理运行时文件{}",
        if purge { "（含用户配置）" } else { "" }
    );
    Ok(0)
}

fn confirm(prompt: &str) -> Result<()> {
    if !std::io::stdin().is_terminal() {
        return Err(Error::msg("非交互环境需要加 --yes 才会执行删除"));
    }
    eprint!("{prompt} [y/N] ");
    std::io::stderr().flush().ok();
    let mut line = String::new();
    std::io::stdin()
        .read_line(&mut line)
        .map_err(|e| Error::io("stdin", e))?;
    if matches!(line.trim(), "y" | "Y" | "yes" | "YES") {
        Ok(())
    } else {
        Err(Error::msg("已取消"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_with_passthrough_args() {
        let cli = Cli::try_parse_from([
            "gh-proxy-manager",
            "get",
            "https://github.com/a/b",
            "-o",
            "file.tar.gz",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Cmd::Get { url, curl_args } => {
                assert_eq!(url, "https://github.com/a/b");
                assert_eq!(curl_args, vec!["-o", "file.tar.gz"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn use_is_an_alias_for_url() {
        let cli = Cli::try_parse_from(["gh-proxy-manager", "use", "https://p.example/"]).unwrap();
        match cli.command.unwrap() {
            Cmd::Url { prefix } => assert_eq!(prefix, "https://p.example/"),
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn toggle_parsing_is_strict() {
        assert!(Cli::try_parse_from(["gh-proxy-manager", "failover", "on"]).is_ok());
        assert!(Cli::try_parse_from(["gh-proxy-manager", "failover", "maybe"]).is_err());
    }
}
