//! GTK4 + libadwaita 图形界面。
//!
//! 界面只是 [`crate::apply`] 的薄壳：所有写入、校验、测速逻辑与 CLI 共用，
//! 这里只负责收集用户意图、显示真实状态、在需要时弹密码框。

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;
use std::sync::Arc;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::accelerator::{self, CachedNode, NodeCache};
use crate::apply::{self, Settings};
use crate::error::{Error, Result};
use crate::exec::{self, SystemRunner};
use crate::logging::Logger;
use crate::mirrors::{self, MirrorList, ResultStore};
use crate::paths::Paths;
use crate::probe;
use crate::state::State;

const APP_ID: &str = "io.github.duter2016.GhProxyManager";

/// 启动图形界面（阻塞到窗口关闭）。
pub fn run() -> Result<i32> {
    let paths = Arc::new(Paths::system()?);
    let logger = Arc::new(Logger::new(paths.log_file.clone()));

    let app = adw::Application::builder().application_id(APP_ID).build();

    app.connect_activate(move |app| {
        open_window(app, paths.clone(), logger.clone());
    });

    // 不要用 app.run()：它会把 "gui" 子命令当成待打开的文件，
    // GtkApplication 找不到对应 handler 就立刻退出。只传程序名即可。
    let _ = app.run_with_args::<&str>(&["gh-proxy-manager"]);
    Ok(0)
}

struct MirrorRow {
    url: String,
    row: adw::ActionRow,
}

struct Ui {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    paths: Arc<Paths>,
    logger: Arc<Logger>,
    sw_makepkg: adw::SwitchRow,
    sw_git: adw::SwitchRow,
    sw_jsd: adw::SwitchRow,
    sw_failover: adw::SwitchRow,
    prefix_row: adw::EntryRow,
    prefix_test_btn: gtk::Button,
    mirrors_group: adw::PreferencesGroup,
    add_row: adw::EntryRow,
    test_all_btn: gtk::Button,
    nodes_btn: gtk::Button,
    apply_btn: gtk::Button,
    rows: RefCell<Vec<MirrorRow>>,
}

fn open_window(app: &adw::Application, paths: Arc<Paths>, logger: Arc<Logger>) {
    if let Some(existing) = app.active_window() {
        existing.present();
        return;
    }
    let ui = build_ui(app, paths, logger);
    refresh_mirrors(&ui);
    ui.window.present();
}

fn build_ui(app: &adw::Application, paths: Arc<Paths>, logger: Arc<Logger>) -> Rc<Ui> {
    let state = State::detect(&paths);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("GitHub 下载加速")
        .default_width(680)
        .default_height(780)
        .build();

    // ---- 顶栏 ----
    let header = adw::HeaderBar::new();
    let apply_btn = gtk::Button::with_label("应用");
    apply_btn.add_css_class("suggested-action");
    apply_btn.set_tooltip_text(Some("写入系统配置（需要管理员权限）"));
    header.pack_end(&apply_btn);

    let menu_btn = gtk::MenuButton::new();
    menu_btn.set_icon_name("open-menu-symbolic");
    menu_btn.set_tooltip_text(Some("菜单"));
    let menu = gio::Menu::new();
    menu.append(Some("查看调试日志"), Some("win.show-log"));
    menu.append(Some("关于 gh-proxy-manager"), Some("win.about"));
    menu_btn.set_menu_model(Some(&menu));
    header.pack_start(&menu_btn);

    // ---- 加速通道 ----
    let channel_group = adw::PreferencesGroup::new();
    channel_group.set_title("加速通道");
    channel_group.set_description(Some("改动后点右上角「应用」才会写进系统"));

    let sw_makepkg = adw::SwitchRow::builder()
        .title("yay / makepkg 下载加速")
        .subtitle("通过 /etc/makepkg.conf.d/99-gh-proxy.conf 接管 https 下载")
        .active(state.makepkg)
        .build();
    let sw_git = adw::SwitchRow::builder()
        .title("git clone 加速")
        .subtitle("全局 insteadOf 规则，可随时关闭")
        .active(state.git)
        .build();
    let sw_jsd = adw::SwitchRow::builder()
        .title("raw.githubusercontent.com 走 jsDelivr")
        .subtitle("CDN 加速；失败时自动回退到加速站")
        .active(state.jsdelivr)
        .build();
    let sw_failover = adw::SwitchRow::builder()
        .title("失败或龟速时自动换站")
        .subtitle("按备选列表断点续传，低于 20KB/s 持续 30 秒即换站")
        .active(state.failover)
        .build();
    channel_group.add(&sw_makepkg);
    channel_group.add(&sw_git);
    channel_group.add(&sw_jsd);
    channel_group.add(&sw_failover);

    // ---- 生效地址 ----
    let prefix_group = adw::PreferencesGroup::new();
    prefix_group.set_title("生效地址");
    let prefix_row = adw::EntryRow::builder()
        .title("加速前缀（自动补 /）")
        .text(state.effective_prefix())
        .build();
    let prefix_test_btn = gtk::Button::with_label("测试");
    prefix_test_btn.add_css_class("flat");
    prefix_test_btn.set_valign(gtk::Align::Center);
    prefix_test_btn.set_tooltip_text(Some("下载真实 release 文件测速"));
    prefix_row.add_suffix(&prefix_test_btn);
    prefix_group.add(&prefix_row);

    // ---- 备选地址 ----
    let mirrors_group = adw::PreferencesGroup::new();
    mirrors_group.set_title("备选地址");
    mirrors_group.set_description(Some("✓ 填入地址（再点「应用」生效）· ▶ 测速 · 🗑 删除"));

    // ---- 管理 ----
    let manage_group = adw::PreferencesGroup::new();
    manage_group.set_title("管理");

    let add_row = adw::EntryRow::builder().title("添加备选地址").build();
    let add_btn = gtk::Button::with_label("添加");
    add_btn.add_css_class("flat");
    add_btn.set_valign(gtk::Align::Center);
    add_row.add_suffix(&add_btn);
    manage_group.add(&add_row);

    let test_all_row = adw::ActionRow::builder()
        .title("测速全部备选地址")
        .subtitle("逐个下载真实 release 文件，结果写入列表")
        .build();
    let test_all_btn = gtk::Button::with_label("开始");
    test_all_btn.add_css_class("flat");
    test_all_btn.set_valign(gtk::Align::Center);
    test_all_row.add_suffix(&test_all_btn);
    manage_group.add(&test_all_row);

    let nodes_row = adw::ActionRow::builder()
        .title("云端节点测速")
        .subtitle("拉取 hubp 节点列表，SHA-256 校验后按延迟选出最快节点")
        .build();
    let nodes_btn = gtk::Button::with_label("测速");
    nodes_btn.add_css_class("flat");
    nodes_btn.set_valign(gtk::Align::Center);
    nodes_row.add_suffix(&nodes_btn);
    manage_group.add(&nodes_row);

    let page = adw::PreferencesPage::new();
    page.add(&channel_group);
    page.add(&prefix_group);
    page.add(&mirrors_group);
    page.add(&manage_group);

    let scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&page)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&scrolled));

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&toolbar));
    window.set_content(Some(&toast));

    let ui = Rc::new(Ui {
        window: window.clone(),
        toast,
        paths,
        logger,
        sw_makepkg,
        sw_git,
        sw_jsd,
        sw_failover,
        prefix_row,
        prefix_test_btn: prefix_test_btn.clone(),
        mirrors_group,
        add_row: add_row.clone(),
        test_all_btn: test_all_btn.clone(),
        nodes_btn: nodes_btn.clone(),
        apply_btn: apply_btn.clone(),
        rows: RefCell::new(Vec::new()),
    });

    // ---- 信号 ----
    {
        let ui = ui.clone();
        apply_btn.connect_clicked(move |_| on_apply(ui.clone()));
    }
    {
        let ui = ui.clone();
        prefix_test_btn.connect_clicked(move |_| test_current(&ui));
    }
    {
        let ui = ui.clone();
        add_btn.connect_clicked(move |_| add_mirror(&ui));
    }
    {
        let ui = ui.clone();
        add_row.connect_apply(move |_| add_mirror(&ui));
    }
    {
        let ui = ui.clone();
        test_all_btn.connect_clicked(move |_| start_test_all(&ui));
    }
    {
        let ui = ui.clone();
        nodes_btn.connect_clicked(move |_| start_node_scan(&ui));
    }

    // ---- 窗口动作 ----
    let log_action = gio::SimpleAction::new("show-log", None);
    {
        let window = window.clone();
        let path = ui.paths.log_file.clone();
        log_action.connect_activate(move |_, _| show_log(&window, &path));
    }
    window.add_action(&log_action);

    let about_action = gio::SimpleAction::new("about", None);
    {
        let window = window.clone();
        about_action.connect_activate(move |_, _| show_about(&window));
    }
    window.add_action(&about_action);

    ui
}

// ---------------------------------------------------------------------------
// 备选地址列表
// ---------------------------------------------------------------------------

fn refresh_mirrors(ui: &Rc<Ui>) {
    {
        let rows = ui.rows.borrow();
        for item in rows.iter() {
            item.row.unparent();
        }
    }
    ui.rows.borrow_mut().clear();

    let list = MirrorList::new(&ui.paths.mirrors_file)
        .load_or_init()
        .unwrap_or_default();
    let state = State::detect(&ui.paths);
    let results = ResultStore::new(&ui.paths.results_file).load();

    if list.is_empty() {
        let row = adw::ActionRow::builder()
            .title("暂无备选地址")
            .subtitle("在下方「添加备选地址」里填一个加速站")
            .build();
        ui.mirrors_group.add(&row);
        ui.rows.borrow_mut().push(MirrorRow {
            url: String::new(),
            row,
        });
        return;
    }

    for url in list {
        let is_current = state.prefix.as_deref() == Some(url.as_str());
        let cached = results.get(&url).cloned();
        let row = build_mirror_row(ui, &url, is_current, cached);
        ui.mirrors_group.add(&row);
        ui.rows.borrow_mut().push(MirrorRow { url, row });
    }
}

fn build_mirror_row(
    ui: &Rc<Ui>,
    url: &str,
    current: bool,
    cached: Option<String>,
) -> adw::ActionRow {
    let title = if current {
        format!("★ {}", esc(url))
    } else {
        esc(url)
    };
    let subtitle = match (current, cached) {
        (true, Some(verdict)) => format!("当前生效 · {}", esc(&verdict)),
        (true, None) => "当前生效".to_string(),
        (false, Some(verdict)) => esc(&verdict),
        (false, None) => "未测试".to_string(),
    };

    let row = adw::ActionRow::builder()
        .title(&title)
        .subtitle(&subtitle)
        .build();

    let test = icon_button("media-playback-start-symbolic", "测速");
    {
        let ui = ui.clone();
        let url = url.to_string();
        test.connect_clicked(move |_| test_mirror(&ui, &url));
    }

    let select = icon_button("object-select-symbolic", "填入为生效地址");
    {
        let entry = ui.prefix_row.clone();
        let toast = ui.toast.clone();
        let url = url.to_string();
        select.connect_clicked(move |_| {
            entry.set_text(&url);
            toast.add_toast(adw::Toast::new("已填入地址，点「应用」生效"));
        });
    }

    let delete = icon_button("user-trash-symbolic", "从列表删除");
    {
        let ui = ui.clone();
        let url = url.to_string();
        delete.connect_clicked(move |_| confirm_delete(&ui, &url));
    }

    row.add_suffix(&test);
    row.add_suffix(&select);
    row.add_suffix(&delete);
    row
}

fn icon_button(icon: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::from_icon_name(icon);
    button.add_css_class("flat");
    button.set_tooltip_text(Some(tooltip));
    button.set_valign(gtk::Align::Center);
    button
}

fn find_row(ui: &Rc<Ui>, url: &str) -> Option<adw::ActionRow> {
    ui.rows
        .borrow()
        .iter()
        .find(|item| item.url == url)
        .map(|item| item.row.clone())
}

fn add_mirror(ui: &Rc<Ui>) {
    let text = ui.add_row.text().to_string();
    if text.trim().is_empty() {
        toast(ui, "请输入加速地址");
        return;
    }
    let list = MirrorList::new(&ui.paths.mirrors_file);
    let _ = list.load_or_init();
    match list.add(&text) {
        Ok(url) => {
            ui.add_row.set_text("");
            refresh_mirrors(ui);
            toast(ui, &format!("已添加 {url}"));
        }
        Err(err) => show_error(&ui.window, "添加失败", &err.to_string()),
    }
}

fn confirm_delete(ui: &Rc<Ui>, url: &str) {
    let state = State::detect(&ui.paths);
    if state.prefix.as_deref() == Some(url) {
        show_error(
            &ui.window,
            "不能删除",
            "该地址当前生效，请先把另一个地址设为生效。",
        );
        return;
    }

    let dialog = adw::AlertDialog::new(Some("删除备选地址"), Some(url));
    dialog.add_response("cancel", "取消");
    dialog.add_response("delete", "删除");
    dialog.set_response_appearance("delete", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("cancel");
    dialog.set_default_response(Some("cancel"));

    let window = ui.window.clone();
    let ui = ui.clone();
    let url = url.to_string();
    dialog.connect_response(None, move |_, response| {
        if response == "delete" {
            match MirrorList::new(&ui.paths.mirrors_file).remove(&url) {
                Ok(_) => refresh_mirrors(&ui),
                Err(err) => show_error(&ui.window, "删除失败", &err.to_string()),
            }
        }
    });
    dialog.present(Some(&window));
}

// ---------------------------------------------------------------------------
// 测速
// ---------------------------------------------------------------------------

fn test_current(ui: &Rc<Ui>) {
    let Some(prefix) = mirrors::normalize(&ui.prefix_row.text()) else {
        show_error(
            &ui.window,
            "地址无效",
            "请填写有效的加速前缀，例如 https://ghfast.top/",
        );
        return;
    };

    ui.prefix_test_btn.set_sensitive(false);
    toast(ui, "正在测试当前地址…");

    let paths = ui.paths.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = probe::probe_prefix(&prefix, &paths.test_url);
        let _ = tx.send_blocking(result);
    });

    glib::spawn_future_local(async move {
        let verdict = match rx.recv().await {
            Ok(result) => result.verdict(),
            Err(_) => "测试任务异常结束".to_string(),
        };
        ui2.prefix_test_btn.set_sensitive(true);
        toast(&ui2, &verdict);
    });
}

fn test_mirror(ui: &Rc<Ui>, url: &str) {
    if let Some(row) = find_row(ui, url) {
        row.set_subtitle("测试中…");
    }

    let url = url.to_string();
    let paths = ui.paths.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = probe::probe_prefix(&url, &paths.test_url);
        let verdict = result.verdict();
        let _ = ResultStore::new(&paths.results_file).merge(&[(url.clone(), verdict.clone())]);
        let _ = tx.send_blocking((url, verdict));
    });

    glib::spawn_future_local(async move {
        if let Ok((url, verdict)) = rx.recv().await {
            if let Some(row) = find_row(&ui2, &url) {
                row.set_subtitle(&esc(&verdict));
            }
            toast(&ui2, &verdict);
        }
    });
}

fn start_test_all(ui: &Rc<Ui>) {
    let list = MirrorList::new(&ui.paths.mirrors_file)
        .load()
        .unwrap_or_default();
    if list.is_empty() {
        toast(ui, "没有备选地址");
        return;
    }
    ui.test_all_btn.set_sensitive(false);
    test_queue(ui.clone(), list);
}

fn test_queue(ui: Rc<Ui>, mut queue: Vec<String>) {
    if queue.is_empty() {
        ui.test_all_btn.set_sensitive(true);
        toast(&ui, "全部测速完成");
        return;
    }

    let url = queue.remove(0);
    if let Some(row) = find_row(&ui, &url) {
        row.set_subtitle("测试中…");
    }

    let paths = ui.paths.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = probe::probe_prefix(&url, &paths.test_url);
        let verdict = result.verdict();
        let _ = ResultStore::new(&paths.results_file).merge(&[(url.clone(), verdict.clone())]);
        let _ = tx.send_blocking((url, verdict));
    });

    glib::spawn_future_local(async move {
        if let Ok((url, verdict)) = rx.recv().await {
            if let Some(row) = find_row(&ui2, &url) {
                row.set_subtitle(&esc(&verdict));
            }
        }
        test_queue(ui2, queue);
    });
}

// ---------------------------------------------------------------------------
// 云端节点（链接识别与完整性校验测速）
// ---------------------------------------------------------------------------

fn start_node_scan(ui: &Rc<Ui>) {
    if !ui.nodes_btn.is_sensitive() {
        return;
    }
    ui.nodes_btn.set_sensitive(false);
    toast(ui, "正在测速云端节点…");

    let paths = ui.paths.clone();
    let logger = ui.logger.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = scan_nodes(&paths, &logger);
        let _ = tx.send_blocking(result);
    });

    glib::spawn_future_local(async move {
        ui2.nodes_btn.set_sensitive(true);
        match rx.recv().await {
            Ok(Ok((cache, best))) => {
                refresh_mirrors(&ui2);
                if let Some(node) = best {
                    let prefix = mirrors::normalize(&node.url).unwrap_or_else(|| node.url.clone());
                    ui2.prefix_row.set_text(&prefix);
                    toast(
                        &ui2,
                        &format!(
                            "最快节点 {}（{} ms），已填入，点「应用」生效",
                            node.url, node.latency_ms
                        ),
                    );
                } else {
                    toast(
                        &ui2,
                        &format!("已校验 {} 个节点，暂无可用结果", cache.nodes.len()),
                    );
                }
            }
            Ok(Err(err)) => show_error(&ui2.window, "节点测速失败", &err.to_string()),
            Err(_) => show_error(&ui2.window, "节点测速失败", "后台任务异常结束"),
        }
    });
}

fn scan_nodes(paths: &Paths, logger: &Logger) -> Result<(NodeCache, Option<CachedNode>)> {
    let (nodes, fallback) = match accelerator::fetch_nodes() {
        Ok(nodes) => (nodes, false),
        Err(err) => {
            logger.log(format!("节点列表拉取失败，改用兜底节点: {err}"));
            (accelerator::fallback_nodes(), true)
        }
    };
    if fallback {
        logger.log("nodes: 使用内置兜底节点");
    }

    let expected = accelerator::fetch_expected_hash()
        .map_err(|err| Error::msg(format!("无法获取节点完整性校验值: {err}")))?;
    let tests = accelerator::test_nodes(&nodes, &expected, accelerator::DEFAULT_CONCURRENCY);
    let cache = NodeCache::from_tests(&tests, accelerator::now_secs());
    if cache.nodes.is_empty() {
        return Err(Error::msg("没有通过完整性校验的节点"));
    }
    cache.save(&paths.nodes_file)?;

    // 把校验通过的节点并入备选地址，并记录测速结论。
    let list = MirrorList::new(&paths.mirrors_file);
    list.load_or_init()?;
    let mut updates = Vec::new();
    for node in &cache.nodes {
        if let Ok(url) = list.add(&node.url) {
            updates.push((url, node.verdict()));
        }
    }
    let _ = ResultStore::new(&paths.results_file).merge(&updates);

    logger.log(format!(
        "nodes: tested={} verified={}",
        tests.len(),
        cache.nodes.len()
    ));
    let best = cache.best().cloned();
    Ok((cache, best))
}

// ---------------------------------------------------------------------------
// 应用
// ---------------------------------------------------------------------------

fn on_apply(ui: Rc<Ui>) {
    let settings = Settings {
        makepkg: ui.sw_makepkg.is_active(),
        git: ui.sw_git.is_active(),
        jsdelivr: ui.sw_jsd.is_active(),
        failover: ui.sw_failover.is_active(),
        prefix: ui.prefix_row.text().to_string(),
    };

    let settings = match settings.clone().normalized() {
        Ok(settings) => settings,
        Err(err) => {
            show_error(&ui.window, "地址无效", &err.to_string());
            return;
        }
    };

    if !exec::privilege_ready(ui.paths.sandboxed()) {
        if let Some(password) = ask_password(&ui.window) {
            run_apply(ui.clone(), settings, Some(password));
        }
    } else {
        run_apply(ui.clone(), settings, None);
    }
}

fn run_apply(ui: Rc<Ui>, settings: Settings, password: Option<String>) {
    ui.apply_btn.set_sensitive(false);
    toast(&ui, "正在应用…");

    let paths = ui.paths.clone();
    let logger = ui.logger.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);

    std::thread::spawn(move || {
        let mut runner = SystemRunner::new(paths.sandboxed(), false);
        if let Some(password) = password {
            runner = runner.with_password(password);
        }
        let result = apply::apply(&paths, &settings, &runner, &logger);
        let _ = tx.send_blocking(result);
    });

    glib::spawn_future_local(async move {
        let result = rx
            .recv()
            .await
            .unwrap_or_else(|_| Err(Error::msg("后台任务异常结束")));
        finish_apply(&ui2, result);
    });
}

fn finish_apply(ui: &Rc<Ui>, result: Result<()>) {
    ui.apply_btn.set_sensitive(true);
    match result {
        Ok(()) => {
            let state = State::detect(&ui.paths);
            ui.sw_makepkg.set_active(state.makepkg);
            ui.sw_git.set_active(state.git);
            ui.sw_jsd.set_active(state.jsdelivr);
            ui.sw_failover.set_active(state.failover);
            ui.prefix_row.set_text(&state.effective_prefix());
            refresh_mirrors(ui);
            toast(ui, "已应用 ✓");
        }
        Err(err) => show_error(&ui.window, "应用失败", &err.to_string()),
    }
}

fn ask_password(parent: &adw::ApplicationWindow) -> Option<String> {
    let dialog = adw::AlertDialog::new(
        Some("需要管理员权限"),
        Some("写入 /etc 下的加速配置需要 sudo 密码。密码只用于本次提权，不会被保存。"),
    );

    let entry = gtk::PasswordEntry::new();
    entry.set_show_peek_icon(true);
    entry.set_activates_default(true);
    dialog.set_extra_child(Some(&entry));
    dialog.add_response("cancel", "取消");
    dialog.add_response("ok", "确定");
    dialog.set_response_appearance("ok", adw::ResponseAppearance::Suggested);
    dialog.set_default_response(Some("ok"));
    dialog.set_close_response("cancel");

    let answer: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let loop_ = glib::MainLoop::new(None, false);
    {
        let answer = answer.clone();
        let loop_ = loop_.clone();
        let entry = entry.clone();
        dialog.connect_response(None, move |_, response| {
            if response == "ok" {
                let text = entry.text().to_string();
                if !text.is_empty() {
                    *answer.borrow_mut() = Some(text);
                }
            }
            loop_.quit();
        });
    }

    dialog.present(Some(parent));
    loop_.run();
    answer.borrow().clone()
}

// ---------------------------------------------------------------------------
// 提示 / 日志 / 关于
// ---------------------------------------------------------------------------

/// ActionRow 的标题/副标题按 Pango markup 解析，动态内容必须转义。
fn esc(text: &str) -> String {
    glib::markup_escape_text(text).to_string()
}

fn toast(ui: &Rc<Ui>, message: &str) {
    ui.toast.add_toast(adw::Toast::new(message));
}

fn show_error(parent: &adw::ApplicationWindow, heading: &str, body: &str) {
    let dialog = adw::AlertDialog::new(Some(heading), Some(body));
    dialog.add_response("close", "关闭");
    dialog.set_close_response("close");
    dialog.set_default_response(Some("close"));
    dialog.present(Some(parent));
}

fn show_log(parent: &adw::ApplicationWindow, path: &Path) {
    let text =
        std::fs::read_to_string(path).unwrap_or_else(|_| format!("暂无日志：{}", path.display()));

    let buffer = gtk::TextBuffer::new(None);
    buffer.set_text(&text);
    let view = gtk::TextView::builder()
        .buffer(&buffer)
        .editable(false)
        .monospace(true)
        .build();

    let scrolled = gtk::ScrolledWindow::builder()
        .child(&view)
        .vexpand(true)
        .hexpand(true)
        .build();

    let window = adw::Window::builder()
        .title("调试日志")
        .transient_for(parent)
        .default_width(780)
        .default_height(560)
        .build();

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&adw::HeaderBar::new());
    toolbar.set_content(Some(&scrolled));
    window.set_content(Some(&toolbar));
    window.present();
}

fn show_about(parent: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("GitHub 下载加速管理")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("Duter2016")
        .developers(["Duter2016"])
        .website("https://github.com/Duter2016/gh-proxy-manager")
        .issue_url("https://github.com/Duter2016/gh-proxy-manager/issues")
        .license_type(gtk::License::MitX11)
        .comments("管理 yay/makepkg、git clone 与 curl 的 GitHub 下载加速")
        .build();
    about.present(Some(parent));
}
