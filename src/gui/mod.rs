//! GTK4 + libadwaita 图形界面。
//!
//! 界面只是 [`crate::apply`] 的薄壳：所有写入、校验、测速逻辑与 CLI 共用，
//! 这里只负责收集用户意图、显示真实状态、在需要时弹密码框。
//!
//! 窗口用 `AdwViewStack` 分成「设置」和「调试日志」两页，日志不再另开窗口。

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use adw::prelude::*;
use gtk::{gio, glib};

use crate::accelerator;
use crate::apply::{self, Settings};
use crate::error::{Error, Result};
use crate::exec::{self, SystemRunner};
use crate::logging::{self, Level, Logger};
use crate::mirrors::{self, MirrorList, ResultStore};
use crate::paths::Paths;
use crate::probe;
use crate::sources::{self, SourceList};
use crate::state::State;

const APP_ID: &str = "io.github.duter2016.GhProxyManager";
/// 日志页面最多显示的最后行数（文件本身不截断）。
const LOG_TAIL_LINES: usize = 2000;

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
    check: Option<gtk::CheckButton>,
}

struct Ui {
    window: adw::ApplicationWindow,
    toast: adw::ToastOverlay,
    paths: Arc<Paths>,
    logger: Arc<Logger>,
    stack: adw::ViewStack,
    sw_makepkg: adw::SwitchRow,
    sw_git: adw::SwitchRow,
    sw_jsd: adw::SwitchRow,
    sw_failover: adw::SwitchRow,
    prefix_row: adw::EntryRow,
    prefix_test_btn: gtk::Button,
    mirrors_group: adw::PreferencesGroup,
    filter_entry: gtk::SearchEntry,
    test_scope: gtk::DropDown,
    batch_test_btn: gtk::Button,
    add_row: adw::EntryRow,
    nodes_row: adw::ActionRow,
    nodes_btn: gtk::Button,
    sources_group: adw::PreferencesGroup,
    source_rows: RefCell<Vec<adw::ActionRow>>,
    add_source_row: adw::EntryRow,
    blacklist_row: adw::ActionRow,
    apply_btn: gtk::Button,
    rows: RefCell<Vec<MirrorRow>>,
    log_buffer: gtk::TextBuffer,
    log_monitor: RefCell<Option<gio::FileMonitor>>,
    log_filter: gtk::DropDown,
    log_scrolled: gtk::ScrolledWindow,
    log_status: gtk::Label,
}

fn open_window(app: &adw::Application, paths: Arc<Paths>, logger: Arc<Logger>) {
    if let Some(existing) = app.active_window() {
        existing.present();
        return;
    }
    let ui = build_ui(app, paths, logger);
    refresh_mirrors(&ui);
    refresh_sources(&ui);
    refresh_blacklist_row(&ui);
    refresh_log(&ui);
    ui.window.present();
}

fn build_ui(app: &adw::Application, paths: Arc<Paths>, logger: Arc<Logger>) -> Rc<Ui> {
    let state = State::detect(&paths);

    let window = adw::ApplicationWindow::builder()
        .application(app)
        .title("GitHub 下载加速")
        .default_width(720)
        .default_height(820)
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
    menu.append(Some("调试日志"), Some("win.show-log"));
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
    mirrors_group.set_description(Some(
        "列表按实测速度从快到慢排列 · 勾选可批量删除 · 「测速」下载真实文件 · 「使用」填入生效地址（再点应用）",
    ));

    let bulk_row = adw::ActionRow::builder()
        .title("批量删除")
        .subtitle("直接在下方列表勾选，删除后自动加入黑名单")
        .build();
    let select_all_btn = gtk::Button::with_label("全选");
    select_all_btn.add_css_class("flat");
    select_all_btn.set_valign(gtk::Align::Center);
    bulk_row.add_suffix(&select_all_btn);
    let delete_selected_btn = gtk::Button::with_label("删除选中");
    delete_selected_btn.add_css_class("flat");
    delete_selected_btn.set_valign(gtk::Align::Center);
    bulk_row.add_suffix(&delete_selected_btn);
    let delete_failed_btn = gtk::Button::with_label("删除不可用");
    delete_failed_btn.add_css_class("flat");
    delete_failed_btn.set_valign(gtk::Align::Center);
    delete_failed_btn.set_tooltip_text(Some("删除所有最近一次测速失败的地址（加入黑名单）"));
    bulk_row.add_suffix(&delete_failed_btn);
    let delete_slow_btn = gtk::Button::with_label("删除慢速");
    delete_slow_btn.add_css_class("flat");
    delete_slow_btn.set_valign(gtk::Align::Center);
    delete_slow_btn.set_tooltip_text(Some("删除实测速度低于 200 KB/s 的地址（加入黑名单）"));
    bulk_row.add_suffix(&delete_slow_btn);
    mirrors_group.add(&bulk_row);

    let test_scope = gtk::DropDown::from_strings(&["全部地址", "未测速", "已测速", "不可用"]);
    test_scope.set_valign(gtk::Align::Center);
    test_scope.set_selected(0);
    let batch_test_row = adw::ActionRow::builder()
        .title("批量测速")
        .subtitle("按所选范围并发测速（8 路），结果写回列表")
        .build();
    batch_test_row.add_suffix(&test_scope);
    let batch_test_btn = gtk::Button::with_label("开始");
    batch_test_btn.add_css_class("flat");
    batch_test_btn.set_valign(gtk::Align::Center);
    batch_test_row.add_suffix(&batch_test_btn);
    mirrors_group.add(&batch_test_row);

    let blacklist_row = adw::ActionRow::builder()
        .title("黑名单")
        .subtitle("拉取节点清单时自动跳过")
        .build();
    let blacklist_btn = gtk::Button::with_label("管理");
    blacklist_btn.add_css_class("flat");
    blacklist_btn.set_valign(gtk::Align::Center);
    blacklist_row.add_suffix(&blacklist_btn);
    mirrors_group.add(&blacklist_row);

    let filter_entry = gtk::SearchEntry::builder()
        .placeholder_text("筛选：地址 / 速度 / 可用 / 未测试")
        .width_request(300)
        .build();
    let filter_row = adw::ActionRow::builder().title("筛选").build();
    filter_row.add_suffix(&filter_entry);
    mirrors_group.add(&filter_row);

    let add_row = adw::EntryRow::builder().title("添加备选地址").build();
    let add_btn = gtk::Button::with_label("添加");
    add_btn.add_css_class("flat");
    add_btn.set_valign(gtk::Align::Center);
    add_row.add_suffix(&add_btn);
    mirrors_group.add(&add_row);

    // ---- 节点接口 ----
    let sources_group = adw::PreferencesGroup::new();
    sources_group.set_title("节点接口");
    sources_group.set_description(Some("当前接口："));

    let nodes_row = adw::ActionRow::builder()
        .title("拉取云端节点清单")
        .subtitle("全部并入备选地址（跳过黑名单）")
        .build();
    let nodes_btn = gtk::Button::with_label("拉取");
    nodes_btn.add_css_class("flat");
    nodes_btn.set_valign(gtk::Align::Center);
    nodes_row.add_suffix(&nodes_btn);
    sources_group.add(&nodes_row);

    let add_source_row = adw::EntryRow::builder().title("添加接口地址").build();
    let add_source_btn = gtk::Button::with_label("添加");
    add_source_btn.add_css_class("flat");
    add_source_btn.set_valign(gtk::Align::Center);
    add_source_row.add_suffix(&add_source_btn);
    sources_group.add(&add_source_row);

    let page = adw::PreferencesPage::new();
    page.add(&channel_group);
    page.add(&prefix_group);
    page.add(&mirrors_group);
    page.add(&sources_group);

    let settings_scrolled = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vexpand(true)
        .child(&page)
        .build();

    // ---- 调试日志页 ----
    let log_buffer = gtk::TextBuffer::new(None);
    let log_view = gtk::TextView::builder()
        .buffer(&log_buffer)
        .editable(false)
        .cursor_visible(false)
        .monospace(true)
        .left_margin(12)
        .right_margin(12)
        .top_margin(8)
        .bottom_margin(8)
        .wrap_mode(gtk::WrapMode::WordChar)
        .build();

    for (name, color) in [
        ("log-debug", "#9a9996"),
        ("log-warn", "#e5a50a"),
        ("log-error", "#e01b24"),
    ] {
        let tag = gtk::TextTag::builder().name(name).foreground(color).build();
        log_buffer.tag_table().add(&tag);
    }

    let log_scrolled = gtk::ScrolledWindow::builder()
        .child(&log_view)
        .vexpand(true)
        .hexpand(true)
        .build();

    let log_copy_btn = gtk::Button::with_label("复制");
    log_copy_btn.add_css_class("flat");
    log_copy_btn.set_tooltip_text(Some("复制当前显示的日志到剪贴板"));
    let log_clear_btn = gtk::Button::with_label("清空");
    log_clear_btn.add_css_class("flat");
    log_clear_btn.set_tooltip_text(Some("清空 debug.log"));

    let log_filter = gtk::DropDown::from_strings(&["全部", "信息及以上", "警告及以上", "仅错误"]);
    log_filter.set_tooltip_text(Some("按级别过滤：调试 → 信息 → 警告 → 错误"));
    log_filter.set_selected(0);

    let log_bar = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    log_bar.set_margin_start(12);
    log_bar.set_margin_end(12);
    log_bar.set_margin_top(6);
    log_bar.set_margin_bottom(6);
    let log_title = gtk::Label::builder().label("调试日志").xalign(0.0).build();
    log_title.add_css_class("heading");
    log_bar.append(&log_title);
    let log_spacer = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    log_spacer.set_hexpand(true);
    log_bar.append(&log_spacer);
    log_bar.append(&log_filter);
    log_bar.append(&log_copy_btn);
    log_bar.append(&log_clear_btn);

    let log_status = gtk::Label::builder()
        .xalign(0.0)
        .margin_start(12)
        .margin_end(12)
        .margin_bottom(8)
        .ellipsize(gtk::pango::EllipsizeMode::Middle)
        .build();
    log_status.add_css_class("dim-label");
    log_status.add_css_class("caption");

    let log_page = gtk::Box::new(gtk::Orientation::Vertical, 0);
    log_page.append(&log_bar);
    log_page.append(&log_scrolled);
    log_page.append(&log_status);

    // ---- 页面切换 ----
    let stack = adw::ViewStack::new();
    stack.add_titled(&settings_scrolled, Some("settings"), "设置");
    stack.add_titled(&log_page, Some("log"), "调试日志");

    let switcher = adw::InlineViewSwitcher::new();
    switcher.set_stack(Some(&stack));
    header.set_title_widget(Some(&switcher));

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&stack));

    let toast = adw::ToastOverlay::new();
    toast.set_child(Some(&toolbar));
    window.set_content(Some(&toast));

    let ui = Rc::new(Ui {
        window: window.clone(),
        toast,
        paths,
        logger,
        stack,
        sw_makepkg,
        sw_git,
        sw_jsd,
        sw_failover,
        prefix_row,
        prefix_test_btn: prefix_test_btn.clone(),
        mirrors_group,
        filter_entry: filter_entry.clone(),
        test_scope: test_scope.clone(),
        batch_test_btn: batch_test_btn.clone(),
        add_row: add_row.clone(),
        nodes_row,
        nodes_btn: nodes_btn.clone(),
        sources_group,
        source_rows: RefCell::new(Vec::new()),
        add_source_row: add_source_row.clone(),
        blacklist_row,
        apply_btn: apply_btn.clone(),
        rows: RefCell::new(Vec::new()),
        log_buffer,
        log_monitor: RefCell::new(None),
        log_filter: log_filter.clone(),
        log_scrolled,
        log_status,
    });

    // ---- 信号 ----
    {
        let ui = ui.clone();
        apply_btn.connect_clicked(move |_| on_apply(ui.clone()));
    }
    {
        let ui = ui.clone();
        let button = ui.prefix_test_btn.clone();
        button.connect_clicked(move |_| test_current(&ui));
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
        let button = ui.nodes_btn.clone();
        button.connect_clicked(move |_| start_node_pull(&ui));
    }
    {
        let ui = ui.clone();
        select_all_btn.connect_clicked(move |_| toggle_select_all(&ui));
    }
    {
        let ui = ui.clone();
        delete_selected_btn.connect_clicked(move |_| delete_selected(&ui));
    }
    {
        let ui = ui.clone();
        delete_failed_btn.connect_clicked(move |_| delete_unavailable(&ui));
    }
    {
        let ui = ui.clone();
        delete_slow_btn.connect_clicked(move |_| delete_slow(&ui));
    }
    {
        let ui = ui.clone();
        batch_test_btn.connect_clicked(move |_| start_batch_test(&ui));
    }
    {
        let ui = ui.clone();
        let source = ui.clone();
        filter_entry.connect_search_changed(move |_| refresh_mirrors(&source));
    }
    {
        let ui = ui.clone();
        blacklist_btn.connect_clicked(move |_| show_blacklist(&ui));
    }
    {
        let ui = ui.clone();
        add_source_btn.connect_clicked(move |_| add_source(&ui));
    }
    {
        let ui = ui.clone();
        add_source_row.connect_apply(move |_| add_source(&ui));
    }
    {
        let ui = ui.clone();
        log_copy_btn.connect_clicked(move |_| copy_log(&ui));
    }
    {
        let ui = ui.clone();
        log_clear_btn.connect_clicked(move |_| confirm_clear_log(&ui));
    }
    {
        let ui = ui.clone();
        log_filter.connect_selected_notify(move |_| refresh_log(&ui));
    }

    // ---- 窗口动作 ----
    let log_action = gio::SimpleAction::new("show-log", None);
    {
        let ui = ui.clone();
        log_action.connect_activate(move |_, _| {
            ui.stack.set_visible_child_name("log");
            refresh_log(&ui);
        });
    }
    window.add_action(&log_action);

    let about_action = gio::SimpleAction::new("about", None);
    {
        let window = window.clone();
        about_action.connect_activate(move |_, _| show_about(&window));
    }
    window.add_action(&about_action);

    ui.logger.log(format!(
        "[启动] 界面已启动 · 版本 {} · 配置目录 {} · {}",
        env!("CARGO_PKG_VERSION"),
        ui.paths.user_dir.display(),
        if ui.paths.sandboxed() {
            "沙箱模式"
        } else {
            "系统模式"
        }
    ));

    {
        let state = State::detect(&ui.paths);
        let mirrors = MirrorList::new(&ui.paths.mirrors_file)
            .load()
            .map(|list| list.len())
            .unwrap_or(0);
        let blacklist = MirrorList::new(&ui.paths.blacklist_file)
            .load()
            .map(|list| list.len())
            .unwrap_or(0);
        let sources = SourceList::new(&ui.paths.sources_file)
            .load()
            .map(|list| list.len())
            .unwrap_or(0);
        ui.logger.debug(format!(
            "[启动] 路径：系统根 {} · 用户配置 {} · 测速目标 {}",
            ui.paths.sysroot.display(),
            ui.paths.user_dir.display(),
            ui.paths.test_url
        ));
        ui.logger.debug(format!(
            "[启动] 当前状态：makepkg 加速={} git 加速={} jsDelivr={} 自动换站={} 生效地址={}",
            crate::state::on_off(state.makepkg),
            crate::state::on_off(state.git),
            crate::state::on_off(state.jsdelivr),
            crate::state::on_off(state.failover),
            state.prefix.as_deref().unwrap_or("（未设置）")
        ));
        ui.logger.debug(format!(
            "[启动] 本地数据：备选 {mirrors} 个 · 黑名单 {blacklist} 个 · 接口 {sources} 个"
        ));
    }

    // 日志实时流：监听 debug.log 的写入事件（100 ms 合并突发写入），不再轮询。
    {
        let file = gio::File::for_path(&ui.paths.log_file);
        match file.monitor_file(gio::FileMonitorFlags::NONE, gio::Cancellable::NONE) {
            Ok(monitor) => {
                let weak = Rc::downgrade(&ui);
                let pending = Rc::new(std::cell::Cell::new(false));
                monitor.connect_changed(move |_, _, _, _| {
                    if pending.get() {
                        return;
                    }
                    pending.set(true);
                    let weak = weak.clone();
                    let pending = pending.clone();
                    glib::timeout_add_local_once(
                        std::time::Duration::from_millis(100),
                        move || {
                            pending.set(false);
                            if let Some(ui) = weak.upgrade() {
                                if ui.stack.visible_child_name().as_deref() == Some("log") {
                                    refresh_log(&ui);
                                }
                            }
                        },
                    );
                });
                ui.log_monitor.replace(Some(monitor));
            }
            Err(err) => ui.logger.warn(format!(
                "[日志] 无法监听日志文件：{err}（仅在切换页面时刷新）"
            )),
        }
    }

    // 切换到日志页时立刻显示现有内容。
    {
        let weak = Rc::downgrade(&ui);
        ui.stack.connect_visible_child_name_notify(move |stack| {
            if stack.visible_child_name().as_deref() == Some("log") {
                if let Some(ui) = weak.upgrade() {
                    refresh_log(&ui);
                }
            }
        });
    }

    ui
}

// ---------------------------------------------------------------------------
// 备选地址列表
// ---------------------------------------------------------------------------

/// 把控件移到分组末尾（用于让「添加…」输入框始终位于列表下方）。
fn move_row_to_end(group: &adw::PreferencesGroup, row: &impl IsA<gtk::Widget>) {
    group.remove(row);
    group.add(row);
}

fn refresh_mirrors(ui: &Rc<Ui>) {
    {
        let mut rows = ui.rows.borrow_mut();
        for item in rows.iter() {
            ui.mirrors_group.remove(&item.row);
        }
        rows.clear();
    }

    let list = MirrorList::new(&ui.paths.mirrors_file)
        .load_or_init()
        .unwrap_or_default();
    let state = State::detect(&ui.paths);
    let results = ResultStore::new(&ui.paths.results_file).load();
    let needle = ui.filter_entry.text().to_string().to_lowercase();
    ui.logger.debug(format!(
        "[备选] 读取 {} 个地址，{} 条测速缓存{}",
        list.len(),
        results.len(),
        if needle.is_empty() {
            String::new()
        } else {
            format!("，筛选「{needle}」")
        }
    ));

    if list.is_empty() {
        let row = adw::ActionRow::builder()
            .title("暂无备选地址")
            .subtitle("在下方「添加备选地址」里填一个加速站")
            .build();
        ui.mirrors_group.add(&row);
        ui.rows.borrow_mut().push(MirrorRow {
            url: String::new(),
            row,
            check: None,
        });
        move_row_to_end(&ui.mirrors_group, &ui.add_row);
        return;
    }

    // 测速过的按速度从快到慢排在前面；测速失败和未测速的保持原顺序排在后面。
    let mut items: Vec<(String, Option<String>)> = list
        .into_iter()
        .map(|url| {
            let cached = results.get(&url).cloned();
            (url, cached)
        })
        .collect();
    items.sort_by(|a, b| {
        let speed_a = a.1.as_deref().and_then(mirrors::verdict_speed);
        let speed_b = b.1.as_deref().and_then(mirrors::verdict_speed);
        match (speed_a, speed_b) {
            (Some(x), Some(y)) => y.cmp(&x),
            (Some(_), None) => std::cmp::Ordering::Less,
            (None, Some(_)) => std::cmp::Ordering::Greater,
            (None, None) => std::cmp::Ordering::Equal,
        }
    });
    let ranked = items
        .iter()
        .filter(|(_, verdict)| {
            verdict
                .as_deref()
                .and_then(mirrors::verdict_speed)
                .is_some()
        })
        .count();
    ui.logger.debug(format!(
        "[备选] 排序完成：{ranked} 个有速度结果，{} 个未测速或不可用",
        items.len().saturating_sub(ranked)
    ));

    let mut shown = 0usize;
    for (url, cached) in items {
        let is_current = state.prefix.as_deref() == Some(url.as_str());
        if !needle.is_empty() && !matches_filter(&url, cached.as_deref(), is_current, &needle) {
            continue;
        }
        shown += 1;
        let (row, check) = build_mirror_row(ui, &url, is_current, cached);
        ui.mirrors_group.add(&row);
        ui.rows.borrow_mut().push(MirrorRow {
            url,
            row,
            check: Some(check),
        });
    }

    if shown == 0 {
        let row = adw::ActionRow::builder()
            .title("没有匹配的地址")
            .subtitle(format!("筛选：{}", esc(&needle)))
            .build();
        ui.mirrors_group.add(&row);
        ui.rows.borrow_mut().push(MirrorRow {
            url: String::new(),
            row,
            check: None,
        });
    }
    move_row_to_end(&ui.mirrors_group, &ui.add_row);
}

/// 列表筛选：地址、测速结论（可用 / KB\/s / 失败）、当前生效、未测试。
fn matches_filter(url: &str, verdict: Option<&str>, current: bool, needle: &str) -> bool {
    if url.to_lowercase().contains(needle) {
        return true;
    }
    if let Some(verdict) = verdict {
        if verdict.to_lowercase().contains(needle) {
            return true;
        }
    }
    if current && "当前生效".contains(needle) {
        return true;
    }
    if verdict.is_none() && "未测试".contains(needle) {
        return true;
    }
    false
}

fn build_mirror_row(
    ui: &Rc<Ui>,
    url: &str,
    current: bool,
    cached: Option<String>,
) -> (adw::ActionRow, gtk::CheckButton) {
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
        .activatable(false)
        .build();

    let check = gtk::CheckButton::new();
    if current {
        check.set_sensitive(false);
        check.set_tooltip_text(Some("当前生效地址不能批量删除"));
    } else {
        check.set_tooltip_text(Some("勾选后可批量删除"));
    }
    check.set_valign(gtk::Align::Center);
    row.add_prefix(&check);

    let test = text_button("测速", "下载真实 release 文件测速");
    {
        let ui = ui.clone();
        let url = url.to_string();
        test.connect_clicked(move |_| test_mirror(&ui, &url));
    }

    let select = text_button("使用", "填入为生效地址（再点「应用」）");
    {
        let entry = ui.prefix_row.clone();
        let toast = ui.toast.clone();
        let url = url.to_string();
        select.connect_clicked(move |_| {
            entry.set_text(&url);
            toast.add_toast(adw::Toast::new("已填入地址，点「应用」生效"));
        });
    }

    let delete = text_button("删除", "移出列表并加入黑名单");
    {
        let ui = ui.clone();
        let url = url.to_string();
        delete.connect_clicked(move |_| delete_mirror(&ui, &url));
    }

    row.add_suffix(&test);
    row.add_suffix(&select);
    row.add_suffix(&delete);
    (row, check)
}

fn text_button(label: &str, tooltip: &str) -> gtk::Button {
    let button = gtk::Button::with_label(label);
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
            ui.logger.log(format!("[备选] 已添加地址 {url}"));
            refresh_mirrors(ui);
            toast(ui, &format!("已添加 {url}"));
        }
        Err(err) => {
            ui.logger
                .error(format!("[备选] 添加地址失败：{}（{err}）", text.trim()));
            show_error(&ui.window, "添加失败", &err.to_string());
        }
    }
}

/// 立即删除一个备选地址并加入黑名单（不再弹确认框）。
fn delete_mirror(ui: &Rc<Ui>, url: &str) {
    let state = State::detect(&ui.paths);
    if state.prefix.as_deref() == Some(url) {
        ui.logger.warn(format!(
            "[备选] 不能删除当前生效地址 {url}：请先把其它地址设为生效"
        ));
        show_error(
            &ui.window,
            "不能删除",
            "该地址当前生效，请先把另一个地址设为生效。",
        );
        return;
    }

    match MirrorList::new(&ui.paths.mirrors_file).remove(url) {
        Ok(true) => {
            ui.logger.log(format!("[备选] 已删除地址 {url}"));
            let _ = MirrorList::new(&ui.paths.blacklist_file).add(url);
            ui.logger.log(format!(
                "[黑名单] 已加入地址 {url}（之后拉取清单会自动跳过）"
            ));
            refresh_mirrors(ui);
            refresh_blacklist_row(ui);
            toast(ui, "已删除并加入黑名单");
        }
        Ok(false) => toast(ui, "列表里已经没有这个地址"),
        Err(err) => {
            ui.logger
                .error(format!("[备选] 删除地址失败：{url}（{err}）"));
            show_error(&ui.window, "删除失败", &err.to_string());
        }
    }
}

/// 全选 / 取消全选当前可见（已勾选）的地址。
fn toggle_select_all(ui: &Rc<Ui>) {
    let checks: Vec<gtk::CheckButton> = ui
        .rows
        .borrow()
        .iter()
        .filter_map(|item| item.check.clone())
        .filter(|check| check.is_sensitive())
        .collect();
    if checks.is_empty() {
        toast(ui, "没有可勾选的地址");
        return;
    }
    let all_on = checks.iter().all(|check| check.is_active());
    for check in &checks {
        check.set_active(!all_on);
    }
}

/// 删除当前列表里所有勾选的地址（直接执行，全部进黑名单）。
fn delete_selected(ui: &Rc<Ui>) {
    let selected: Vec<String> = ui
        .rows
        .borrow()
        .iter()
        .filter(|item| {
            item.check
                .as_ref()
                .map(|check| check.is_active() && check.is_sensitive())
                .unwrap_or(false)
        })
        .map(|item| item.url.clone())
        .collect();
    if selected.is_empty() {
        toast(ui, "先在列表里勾选要删除的地址");
        return;
    }

    let mut removed = 0usize;
    for url in &selected {
        match MirrorList::new(&ui.paths.mirrors_file).remove(url) {
            Ok(true) => {
                let _ = MirrorList::new(&ui.paths.blacklist_file).add(url);
                ui.logger
                    .log(format!("[备选] 已批量删除地址 {url}，并加入黑名单"));
                removed += 1;
            }
            Ok(false) => {}
            Err(err) => {
                ui.logger
                    .error(format!("[备选] 批量删除失败：{url}（{err}）"));
            }
        }
    }
    refresh_mirrors(ui);
    refresh_blacklist_row(ui);
    toast(ui, &format!("已删除 {removed} 个地址并加入黑名单"));
}

/// 删除所有最近一次测速失败的地址（当前生效地址除外），一并加入黑名单。
fn delete_unavailable(ui: &Rc<Ui>) {
    let results = ResultStore::new(&ui.paths.results_file).load();
    let state = State::detect(&ui.paths);
    let targets: Vec<String> = MirrorList::new(&ui.paths.mirrors_file)
        .load()
        .unwrap_or_default()
        .into_iter()
        .filter(|url| {
            if state.prefix.as_deref() == Some(url.as_str()) {
                return false;
            }
            results
                .get(url)
                .map(|verdict| !verdict.starts_with('✓'))
                .unwrap_or(false)
        })
        .collect();
    if targets.is_empty() {
        toast(ui, "没有测速失败的地址");
        return;
    }

    let mut removed = 0usize;
    for url in &targets {
        match MirrorList::new(&ui.paths.mirrors_file).remove(url) {
            Ok(true) => {
                let _ = MirrorList::new(&ui.paths.blacklist_file).add(url);
                ui.logger
                    .log(format!("[备选] 已删除不可用地址 {url}，并加入黑名单"));
                removed += 1;
            }
            Ok(false) => {}
            Err(err) => ui
                .logger
                .error(format!("[备选] 删除不可用地址失败：{url}（{err}）")),
        }
    }
    refresh_mirrors(ui);
    refresh_blacklist_row(ui);
    toast(ui, &format!("已删除 {removed} 个不可用地址并加入黑名单"));
}

/// 低于该速度（KB/s）的地址视为慢速。
const SLOW_THRESHOLD_KBPS: u64 = 200;

/// 删除所有实测速度低于 [`SLOW_THRESHOLD_KBPS`] 的地址（当前生效地址除外）。
fn delete_slow(ui: &Rc<Ui>) {
    let results = ResultStore::new(&ui.paths.results_file).load();
    let state = State::detect(&ui.paths);
    let targets: Vec<(String, u64)> = MirrorList::new(&ui.paths.mirrors_file)
        .load()
        .unwrap_or_default()
        .into_iter()
        .filter(|url| state.prefix.as_deref() != Some(url.as_str()))
        .filter_map(|url| {
            results
                .get(&url)
                .and_then(|verdict| mirrors::verdict_speed(verdict))
                .filter(|speed| *speed < SLOW_THRESHOLD_KBPS)
                .map(|speed| (url, speed))
        })
        .collect();
    if targets.is_empty() {
        toast(ui, "没有低于 200 KB/s 的地址");
        return;
    }

    let mut removed = 0usize;
    for (url, speed) in &targets {
        match MirrorList::new(&ui.paths.mirrors_file).remove(url) {
            Ok(true) => {
                let _ = MirrorList::new(&ui.paths.blacklist_file).add(url);
                ui.logger.log(format!(
                    "[备选] 已删除慢速地址 {url}（{speed} KB/s），并加入黑名单"
                ));
                removed += 1;
            }
            Ok(false) => {}
            Err(err) => ui
                .logger
                .error(format!("[备选] 删除慢速地址失败：{url}（{err}）")),
        }
    }
    refresh_mirrors(ui);
    refresh_blacklist_row(ui);
    toast(ui, &format!("已删除 {removed} 个慢速地址并加入黑名单"));
}

/// 批量测速的并发数。
const BATCH_TEST_WORKERS: usize = 8;

fn test_scope_name(index: u32) -> &'static str {
    match index {
        1 => "未测速",
        2 => "已测速",
        3 => "不可用",
        _ => "全部地址",
    }
}

/// 按所选范围并发测速：全部 / 未测速 / 已测速 / 不可用。
fn start_batch_test(ui: &Rc<Ui>) {
    if !ui.batch_test_btn.is_sensitive() {
        return;
    }
    let scope = ui.test_scope.selected();
    let results = ResultStore::new(&ui.paths.results_file).load();
    let targets: Vec<String> = MirrorList::new(&ui.paths.mirrors_file)
        .load_or_init()
        .unwrap_or_default()
        .into_iter()
        .filter(|url| {
            let verdict = results.get(url);
            match scope {
                1 => verdict.is_none(),
                2 => verdict.is_some(),
                3 => verdict.map(|v| !v.starts_with('✓')).unwrap_or(false),
                _ => true,
            }
        })
        .collect();
    if targets.is_empty() {
        toast(ui, &format!("「{}」范围内没有地址", test_scope_name(scope)));
        return;
    }

    let total = targets.len();
    ui.batch_test_btn.set_sensitive(false);
    ui.logger.log(format!(
        "[测速] 开始批量测速：范围 {}，共 {total} 个地址（并发 {BATCH_TEST_WORKERS}）",
        test_scope_name(scope)
    ));
    toast(ui, &format!("正在测速 {total} 个地址…"));

    let queue = Arc::new(Mutex::new(VecDeque::from(targets)));
    let paths = ui.paths.clone();
    let logger = ui.logger.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::unbounded();
    let workers = BATCH_TEST_WORKERS.min(total);
    for _ in 0..workers {
        let queue = queue.clone();
        let paths = paths.clone();
        let logger = logger.clone();
        let tx = tx.clone();
        std::thread::spawn(move || {
            loop {
                let next = queue
                    .lock()
                    .map(|mut queue| queue.pop_front())
                    .unwrap_or(None);
                let Some(url) = next else { break };
                let started = Instant::now();
                let result = probe::probe_prefix(&url, &paths.test_url);
                let verdict = result.verdict();
                let line = format!(
                    "[测速] {url} → {verdict}（耗时 {:.1} 秒）",
                    started.elapsed().as_secs_f64()
                );
                if verdict.starts_with('✓') {
                    logger.log(line);
                } else {
                    logger.warn(line);
                }
                if tx.send_blocking((url, verdict)).is_err() {
                    break;
                }
            }
        });
    }
    drop(tx);

    glib::spawn_future_local(async move {
        let mut done = 0usize;
        let mut ok = 0usize;
        let mut updates = Vec::new();
        while let Ok((url, verdict)) = rx.recv().await {
            if let Some(row) = find_row(&ui2, &url) {
                row.set_subtitle(&esc(&verdict));
            }
            if verdict.starts_with('✓') {
                ok += 1;
            }
            updates.push((url, verdict));
            done += 1;
        }
        if !updates.is_empty() {
            let _ = ResultStore::new(&ui2.paths.results_file).merge(&updates);
        }
        ui2.batch_test_btn.set_sensitive(true);
        refresh_mirrors(&ui2);
        ui2.logger
            .log(format!("[测速] 批量测速完成：{done} 个地址，{ok} 个可用"));
        toast(&ui2, &format!("批量测速完成：{done} 个，{ok} 个可用"));
    });
}

// ---------------------------------------------------------------------------
// 黑名单
// ---------------------------------------------------------------------------

fn refresh_blacklist_row(ui: &Rc<Ui>) {
    let count = MirrorList::new(&ui.paths.blacklist_file)
        .load()
        .map(|list| list.len())
        .unwrap_or(0);
    ui.blacklist_row
        .set_subtitle(&format!("{count} 个地址 · 拉取节点清单时自动跳过"));
}

fn show_blacklist(ui: &Rc<Ui>) {
    ui.logger.debug(format!(
        "[黑名单] 打开管理窗口：{} 个地址",
        MirrorList::new(&ui.paths.blacklist_file)
            .load()
            .map(|list| list.len())
            .unwrap_or(0)
    ));
    let window = adw::Window::builder()
        .title("黑名单")
        .transient_for(&ui.window)
        .default_width(580)
        .default_height(480)
        .build();

    let header = adw::HeaderBar::new();
    let wipe_btn = gtk::Button::with_label("全部删除");
    wipe_btn.add_css_class("flat");
    wipe_btn.set_tooltip_text(Some("彻底清空黑名单，不恢复任何地址"));
    header.pack_end(&wipe_btn);
    let restore_all_btn = gtk::Button::with_label("全部恢复");
    restore_all_btn.add_css_class("flat");
    restore_all_btn.set_tooltip_text(Some("把黑名单里的地址全部加回备选地址"));
    header.pack_end(&restore_all_btn);

    let search = gtk::SearchEntry::builder()
        .placeholder_text("筛选黑名单：地址")
        .margin_top(12)
        .margin_start(12)
        .margin_end(12)
        .build();

    let listbox = gtk::ListBox::new();
    listbox.add_css_class("boxed-list");
    listbox.set_margin_top(12);
    listbox.set_margin_bottom(12);
    listbox.set_margin_start(12);
    listbox.set_margin_end(12);
    let scrolled = gtk::ScrolledWindow::builder()
        .vexpand(true)
        .child(&listbox)
        .build();

    let content = gtk::Box::new(gtk::Orientation::Vertical, 0);
    content.append(&search);
    content.append(&scrolled);

    let toolbar = adw::ToolbarView::new();
    toolbar.add_top_bar(&header);
    toolbar.set_content(Some(&content));
    window.set_content(Some(&toolbar));

    fill_blacklist(&listbox, ui, "");

    {
        let ui = ui.clone();
        let listbox = listbox.clone();
        search.connect_search_changed(move |search| {
            fill_blacklist(&listbox, &ui, search.text().as_str());
        });
    }

    {
        let ui = ui.clone();
        let listbox = listbox.clone();
        let search = search.clone();
        restore_all_btn.connect_clicked(move |_| {
            let list = MirrorList::new(&ui.paths.blacklist_file)
                .load()
                .unwrap_or_default();
            let mut restored = 0usize;
            for url in &list {
                if restore_from_blacklist(&ui, url).is_ok() {
                    restored += 1;
                }
            }
            fill_blacklist(&listbox, &ui, search.text().as_str());
            refresh_mirrors(&ui);
            refresh_blacklist_row(&ui);
            toast(&ui, &format!("已恢复 {restored} 个地址"));
        });
    }

    {
        let ui = ui.clone();
        let listbox = listbox.clone();
        let search = search.clone();
        wipe_btn.connect_clicked(move |_| {
            let count = MirrorList::new(&ui.paths.blacklist_file)
                .load()
                .map(|list| list.len())
                .unwrap_or(0);
            match MirrorList::new(&ui.paths.blacklist_file).save(&[]) {
                Ok(()) => {
                    ui.logger.log(format!(
                        "[黑名单] 已全部删除：共 {count} 个地址（不会恢复到备选地址）"
                    ));
                    fill_blacklist(&listbox, &ui, search.text().as_str());
                    refresh_blacklist_row(&ui);
                    toast(&ui, &format!("黑名单已清空（{count} 个）"));
                }
                Err(err) => {
                    ui.logger.error(format!("[黑名单] 清空失败：{err}"));
                    show_error(&ui.window, "清空黑名单失败", &err.to_string());
                }
            }
        });
    }

    window.present();
}

fn fill_blacklist(listbox: &gtk::ListBox, ui: &Rc<Ui>, needle: &str) {
    while let Some(child) = listbox.first_child() {
        listbox.remove(&child);
    }

    let all = MirrorList::new(&ui.paths.blacklist_file)
        .load()
        .unwrap_or_default();
    if all.is_empty() {
        let row = adw::ActionRow::builder()
            .title("黑名单为空")
            .subtitle("删除备选地址时会自动加入这里")
            .build();
        listbox.append(&row);
        return;
    }

    let needle = needle.trim().to_lowercase();
    let list: Vec<String> = all
        .iter()
        .filter(|url| needle.is_empty() || url.to_lowercase().contains(&needle))
        .cloned()
        .collect();
    if list.is_empty() {
        let row = adw::ActionRow::builder()
            .title("没有匹配的地址")
            .subtitle(format!("筛选：{}", esc(&needle)))
            .build();
        listbox.append(&row);
        return;
    }

    for url in list {
        let row = adw::ActionRow::builder().title(esc(&url)).build();
        let restore = text_button("恢复", "移出黑名单并重新加入备选地址");
        let listbox2 = listbox.clone();
        let ui2 = ui.clone();
        let url2 = url.clone();
        let needle2 = needle.clone();
        restore.connect_clicked(move |_| match restore_from_blacklist(&ui2, &url2) {
            Ok(()) => {
                fill_blacklist(&listbox2, &ui2, &needle2);
                refresh_mirrors(&ui2);
                refresh_blacklist_row(&ui2);
                toast(&ui2, "已恢复");
            }
            Err(err) => show_error(&ui2.window, "恢复失败", &err.to_string()),
        });
        row.add_suffix(&restore);
        listbox.append(&row);
    }
}

fn restore_from_blacklist(ui: &Rc<Ui>, url: &str) -> Result<()> {
    MirrorList::new(&ui.paths.blacklist_file).remove(url)?;
    MirrorList::new(&ui.paths.mirrors_file).add(url)?;
    ui.logger
        .log(format!("[黑名单] 已恢复地址 {url}，并放回备选地址"));
    Ok(())
}

// ---------------------------------------------------------------------------
// 节点接口
// ---------------------------------------------------------------------------

fn refresh_sources(ui: &Rc<Ui>) {
    for row in ui.source_rows.borrow_mut().drain(..) {
        ui.sources_group.remove(&row);
    }

    let list = SourceList::new(&ui.paths.sources_file)
        .ensure_default(sources::DEFAULT_SOURCE)
        .unwrap_or_default();
    let active = sources::active(&ui.paths);
    ui.logger.debug(format!(
        "[接口] 共 {} 个接口，当前使用 {active}",
        list.len()
    ));
    ui.sources_group
        .set_description(Some(&format!("当前接口：{}", esc(&active))));
    ui.nodes_row.set_subtitle(&format!(
        "接口：{} · 只拉取，全部并入备选地址（跳过黑名单）",
        esc(&active)
    ));

    for url in list {
        let current = url == active;
        let title = if current {
            format!("★ {}", esc(&url))
        } else {
            esc(&url)
        };
        let row = adw::ActionRow::builder()
            .title(&title)
            .subtitle(if current {
                "当前接口"
            } else {
                "点击「使用」切换"
            })
            .build();

        let use_btn = text_button("使用", "从这个接口拉取节点清单");
        use_btn.set_sensitive(!current);
        {
            let ui = ui.clone();
            let url = url.clone();
            use_btn.connect_clicked(move |_| match sources::set_active(&ui.paths, &url) {
                Ok(url) => {
                    ui.logger.log(format!("[接口] 已切换当前接口为 {url}"));
                    refresh_sources(&ui);
                    toast(&ui, &format!("已切换到 {url}"));
                }
                Err(err) => show_error(&ui.window, "切换接口失败", &err.to_string()),
            });
        }

        let delete = text_button("删除", "从接口列表删除");
        {
            let ui = ui.clone();
            let url = url.clone();
            delete.connect_clicked(move |_| {
                let list = SourceList::new(&ui.paths.sources_file)
                    .load()
                    .unwrap_or_default();
                if list.len() <= 1 {
                    toast(&ui, "至少保留一个接口");
                    return;
                }
                match SourceList::new(&ui.paths.sources_file).remove(&url) {
                    Ok(true) => {
                        ui.logger.log(format!("[接口] 已删除接口 {url}"));
                        refresh_sources(&ui);
                        toast(&ui, "接口已删除");
                    }
                    Ok(false) => toast(&ui, "接口列表里没有这个地址"),
                    Err(err) => show_error(&ui.window, "删除接口失败", &err.to_string()),
                }
            });
        }

        row.add_suffix(&use_btn);
        row.add_suffix(&delete);
        ui.sources_group.add(&row);
        ui.source_rows.borrow_mut().push(row);
    }
    move_row_to_end(&ui.sources_group, &ui.add_source_row);
}

fn add_source(ui: &Rc<Ui>) {
    let text = ui.add_source_row.text().to_string();
    if text.trim().is_empty() {
        toast(ui, "请输入接口地址");
        return;
    }
    match SourceList::new(&ui.paths.sources_file).add(&text) {
        Ok(url) => {
            ui.add_source_row.set_text("");
            ui.logger.log(format!("[接口] 已添加接口 {url}"));
            refresh_sources(ui);
            toast(ui, &format!("已添加接口 {url}"));
        }
        Err(err) => {
            ui.logger.error(format!("[接口] 添加接口失败：{err}"));
            show_error(&ui.window, "添加接口失败", &err.to_string());
        }
    }
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
    ui.logger.debug(format!(
        "[测速] 开始测试当前生效地址 {prefix}（目标 {}，限时 40 秒）",
        ui.paths.test_url
    ));
    toast(ui, "正在测试当前地址…");

    let paths = ui.paths.clone();
    let logger = ui.logger.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let started = Instant::now();
        let result = probe::probe_prefix(&prefix, &paths.test_url);
        let verdict = result.verdict();
        let line = format!(
            "[测速] 当前生效地址 {prefix} → {verdict}（耗时 {:.1} 秒）",
            started.elapsed().as_secs_f64()
        );
        if verdict.starts_with('✓') {
            logger.log(line);
        } else {
            logger.warn(line);
        }
        let _ = tx.send_blocking(verdict);
    });

    glib::spawn_future_local(async move {
        let verdict = match rx.recv().await {
            Ok(verdict) => verdict,
            Err(_) => "测试任务异常结束".to_string(),
        };
        ui2.prefix_test_btn.set_sensitive(true);
        toast(&ui2, &verdict);
    });
}

fn test_mirror(ui: &Rc<Ui>, url: &str) {
    ui.logger.debug(format!(
        "[测速] 开始测试 {url}（目标 {}，限时 40 秒）",
        ui.paths.test_url
    ));
    if let Some(row) = find_row(ui, url) {
        row.set_subtitle("测试中…");
    }

    let url = url.to_string();
    let paths = ui.paths.clone();
    let logger = ui.logger.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let started = Instant::now();
        let result = probe::probe_prefix(&url, &paths.test_url);
        let verdict = result.verdict();
        let line = format!(
            "[测速] {url} → {verdict}（耗时 {:.1} 秒）",
            started.elapsed().as_secs_f64()
        );
        if verdict.starts_with('✓') {
            logger.log(line);
        } else {
            logger.warn(line);
        }
        let _ = ResultStore::new(&paths.results_file).merge(&[(url.clone(), verdict.clone())]);
        let _ = tx.send_blocking((url, verdict));
    });

    glib::spawn_future_local(async move {
        if let Ok((url, verdict)) = rx.recv().await {
            if let Some(row) = find_row(&ui2, &url) {
                row.set_subtitle(&esc(&verdict));
            }
            toast(&ui2, &verdict);
        } else {
            ui2.logger.warn("[测速] 后台任务异常结束");
        }
    });
}

// ---------------------------------------------------------------------------
// 云端节点清单
// ---------------------------------------------------------------------------

struct PullOutcome {
    total: usize,
    added: usize,
    fallback: bool,
}

fn start_node_pull(ui: &Rc<Ui>) {
    if !ui.nodes_btn.is_sensitive() {
        return;
    }
    ui.nodes_btn.set_sensitive(false);
    ui.logger.log("[节点] 开始拉取节点清单");
    toast(ui, "正在拉取节点清单…");

    let paths = ui.paths.clone();
    let logger = ui.logger.clone();
    let ui2 = ui.clone();
    let (tx, rx) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let result = pull_nodes(&paths, &logger);
        let _ = tx.send_blocking(result);
    });

    glib::spawn_future_local(async move {
        ui2.nodes_btn.set_sensitive(true);
        match rx.recv().await {
            Ok(Ok(outcome)) => {
                refresh_mirrors(&ui2);
                toast(
                    &ui2,
                    &format!(
                        "已拉取 {} 个地址，新增 {} 个（已跳过黑名单）",
                        outcome.total, outcome.added
                    ),
                );
                if outcome.fallback {
                    ui2.logger
                        .log("[节点] 本次使用内置兜底节点（未能从接口获取清单）");
                }
            }
            Ok(Err(err)) => {
                ui2.logger.error(format!("[节点] 拉取失败：{err}"));
                show_error(&ui2.window, "拉取节点清单失败", &err.to_string());
            }
            Err(_) => {
                ui2.logger.error("[节点] 后台任务异常结束");
                show_error(&ui2.window, "拉取节点清单失败", "后台任务异常结束");
            }
        }
    });
}

/// 只拉取：从当前接口取节点清单 → 跳过黑名单 → 全部并入备选地址。
///
/// 界面里不再做 SHA-256 测速（那是 CLI `nodes` 的职责）；这里把清单落到
/// 备选列表，用户再按需逐个「测速」。
fn pull_nodes(paths: &Paths, logger: &Logger) -> Result<PullOutcome> {
    let source = sources::active(paths);
    logger.log(format!("[节点] 使用接口 {source}"));
    let (nodes, fallback) = match accelerator::fetch_nodes_from(&source) {
        Ok(nodes) => (nodes, false),
        Err(err) => {
            logger.warn(format!(
                "[节点] 从接口 {source} 拉取失败（{err}），改用内置兜底节点"
            ));
            (accelerator::fallback_nodes(), true)
        }
    };
    logger.debug(format!("[节点] 接口返回 {} 个地址", nodes.len()));

    let blacklist = MirrorList::new(&paths.blacklist_file)
        .load()
        .unwrap_or_default();
    let mut candidates = Vec::new();
    for node in &nodes {
        let Some(url) = mirrors::normalize(node) else {
            continue;
        };
        if blacklist.contains(&url) {
            continue;
        }
        if !candidates.contains(&url) {
            candidates.push(url);
        }
    }
    let skipped = nodes.len().saturating_sub(candidates.len());
    logger.debug(format!(
        "[节点] 规范化后 {} 个候选地址，跳过 {skipped} 个（黑名单或重复）",
        candidates.len()
    ));

    let list = MirrorList::new(&paths.mirrors_file);
    list.load_or_init()?;
    let added_urls = list.merge(&candidates)?;
    let added = added_urls.len();
    if !added_urls.is_empty() {
        let shown: Vec<&str> = added_urls.iter().take(20).map(String::as_str).collect();
        logger.debug(format!(
            "[节点] 新增地址：{}{}",
            shown.join(" "),
            if added_urls.len() > 20 { " …" } else { "" }
        ));
    }
    logger.log(format!(
        "[节点] 已并入备选地址：新增 {added} 个，当前共 {} 个",
        list.load()?.len()
    ));

    Ok(PullOutcome {
        total: nodes.len(),
        added,
        fallback,
    })
}

// ---------------------------------------------------------------------------
// 调试日志页
// ---------------------------------------------------------------------------

/// 日志页筛选框当前的范围名称。
fn log_scope_name(ui: &Ui) -> &'static str {
    match ui.log_filter.selected() {
        1 => "信息及以上",
        2 => "警告及以上",
        3 => "仅错误",
        _ => "全部",
    }
}

/// 日志页筛选框对应的最低级别（“全部”= 调试）。
fn log_min_level(ui: &Ui) -> Level {
    match ui.log_filter.selected() {
        1 => Level::Info,
        2 => Level::Warn,
        3 => Level::Error,
        _ => Level::Debug,
    }
}

fn refresh_log(ui: &Rc<Ui>) {
    let min = log_min_level(ui);
    let text = logging::filter_at_least(&ui.logger.tail(LOG_TAIL_LINES), min);
    let current = ui
        .log_buffer
        .text(&ui.log_buffer.start_iter(), &ui.log_buffer.end_iter(), true);
    if current.as_str() == text {
        return;
    }

    // 用户滚上去看历史时不打断；回到底部后恢复跟随。
    let adjustment = ui.log_scrolled.vadjustment();
    let at_end = adjustment.upper() - (adjustment.value() + adjustment.page_size()) < 8.0;
    let first_fill = current.is_empty();
    ui.log_buffer.set_text(&text);
    apply_log_tags(ui);
    ui.log_status.set_label(&format!(
        "{} · {} 行 · 范围：{} · 实时更新（上滚暂停跟随，回到底部恢复）",
        ui.paths.log_file.display(),
        text.lines().count(),
        log_scope_name(ui)
    ));
    if at_end || first_fill {
        scroll_log_to_end(ui);
        // 文本重新排版后再贴底一次，避免长行换行后停在中间。
        let weak = Rc::downgrade(ui);
        glib::timeout_add_local_once(std::time::Duration::from_millis(16), move || {
            if let Some(ui) = weak.upgrade() {
                scroll_log_to_end(&ui);
            }
        });
    }
}

/// 按级别给每行着色：调试灰、警告橙、错误红，信息用默认色。
fn apply_log_tags(ui: &Ui) {
    let buffer = &ui.log_buffer;
    buffer.remove_all_tags(&buffer.start_iter(), &buffer.end_iter());

    let mut iter = buffer.start_iter();
    while !iter.is_end() {
        let mut line_end = iter;
        line_end.forward_to_line_end();
        let line = buffer.text(&iter, &line_end, true);
        let tag = match logging::line_level(line.as_str()) {
            Level::Debug => Some("log-debug"),
            Level::Warn => Some("log-warn"),
            Level::Error => Some("log-error"),
            Level::Info => None,
        };
        if let Some(tag) = tag {
            buffer.apply_tag_by_name(tag, &iter, &line_end);
        }
        if !line_end.forward_line() {
            break;
        }
        iter = line_end;
    }
}

fn scroll_log_to_end(ui: &Ui) {
    let adjustment = ui.log_scrolled.vadjustment();
    adjustment.set_value(adjustment.upper() - adjustment.page_size());
}

fn copy_log(ui: &Rc<Ui>) {
    let text = ui
        .log_buffer
        .text(&ui.log_buffer.start_iter(), &ui.log_buffer.end_iter(), true);
    ui.window.clipboard().set_text(&text);
    toast(ui, "日志已复制到剪贴板");
}

fn confirm_clear_log(ui: &Rc<Ui>) {
    let dialog = adw::AlertDialog::new(
        Some("清空调试日志"),
        Some("删除 debug.log 的全部内容？此操作不可撤销。"),
    );
    dialog.add_response("cancel", "取消");
    dialog.add_response("clear", "清空");
    dialog.set_response_appearance("clear", adw::ResponseAppearance::Destructive);
    dialog.set_close_response("cancel");
    dialog.set_default_response(Some("cancel"));

    let window = ui.window.clone();
    let ui = ui.clone();
    dialog.connect_response(None, move |_, response| {
        if response != "clear" {
            return;
        }
        match ui.logger.clear() {
            Ok(()) => {
                ui.logger.log("[日志] 已清空调试日志");
                refresh_log(&ui);
                toast(&ui, "日志已清空");
            }
            Err(err) => show_error(&ui.window, "清空日志失败", &err.to_string()),
        }
    });
    dialog.present(Some(&window));
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
            ui.logger
                .error(format!("[应用] 未执行：加速地址无效（{err}）"));
            show_error(&ui.window, "地址无效", &err.to_string());
            return;
        }
    };

    ui.logger.log(format!(
        "[应用] 用户请求应用：makepkg 加速={}，git 加速={}，jsDelivr={}，自动换站={}，加速地址={}",
        crate::state::on_off(settings.makepkg),
        crate::state::on_off(settings.git),
        crate::state::on_off(settings.jsdelivr),
        crate::state::on_off(settings.failover),
        settings.prefix
    ));

    if !exec::privilege_ready(ui.paths.sandboxed()) {
        if let Some(password) = ask_password(&ui.window) {
            run_apply(ui.clone(), settings, Some(password));
        } else {
            ui.logger.warn("[应用] 已取消：未输入管理员密码");
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
        Err(err) => {
            ui.logger.error(format!("[应用] 失败：{err}"));
            show_error(&ui.window, "应用失败", &err.to_string());
        }
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

fn show_about(parent: &adw::ApplicationWindow) {
    let about = adw::AboutDialog::builder()
        .application_name("GitHub 下载加速管理")
        .application_icon(APP_ID)
        .version(env!("CARGO_PKG_VERSION"))
        .developer_name("pingod")
        .developers(["pingod", "Duter2016"])
        .website("https://github.com/pingod/gh-proxy-manager")
        .issue_url("https://github.com/pingod/gh-proxy-manager/issues")
        .copyright("原始 Bash 版本 © Donghui (Duter2016)；Rust 重写 © pingod")
        .license_type(gtk::License::MitX11)
        .comments("管理 yay/makepkg、git clone 与 curl 的 GitHub 下载加速")
        .build();
    about.present(Some(parent));
}
