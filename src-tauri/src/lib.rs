//! ScreenLite Tauri 外壳。
//!
//! 分层：本层只做 IPC 与窗口，媒体逻辑全在 `screenlite-media`（可独立测试）。
//!
//! [`configure`] 把「状态 + 插件 + 命令注册」集中到一处，生产入口与集成测试
//! 都走它，避免测试与生产环境的命令集合漂移。

mod commands;
mod settings;

use std::time::Duration;

use tauri::Manager;

use screenlite_media::consts::STOP_TOTAL_TIMEOUT_MS;

pub use commands::{AppState, QuitGate, UiCommand};
// 无边框子类与"剥样式"：生产入口在 `run()` 里按 装子类 → 剥出生样式 的顺序调用，
// 集成测试要复现同一条链路（见 tests/frameless_subclass.rs），所以必须能从 crate 外拿到。
#[cfg(windows)]
pub use commands::{enforce_frameless_style, install_frameless_subclass};

/// 给任意运行时装配应用（Wry 用于生产，MockRuntime 用于集成测试）。
pub fn configure<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    builder
        .manage(AppState::default())
        // 原生文件夹选择器：前端用 @tauri-apps/plugin-dialog 的 open({directory:true})。
        // 走官方插件的 JS API 而不是自己写命令——插件自己处理线程与消息循环，
        // 不会在命令线程上阻塞。
        .invoke_handler(tauri::generate_handler![
            commands::list_displays,
            commands::start_recording,
            commands::stop_recording,
            commands::get_status,
            commands::open_output_directory,
            commands::open_region_selector,
            commands::confirm_region,
            commands::cancel_region,
            commands::list_windows,
            // 命令队列：托盘/热键 → 后端入队 → 前端拉取
            commands::ui_ready,
            commands::take_ui_command,
            commands::ui_probe,
            commands::quit_done,
            // 设置：音频源/增益 + 全局快捷键（可配置、可停用）
            commands::get_settings,
            commands::set_settings,
        ])
}

/// 生产装配：基础 + 插件 + 托盘 + 全局快捷键。
///
/// 原生文件夹选择器走官方 dialog 插件的 JS API（插件自己处理线程与消息循环，不会阻塞命令线程）。
///
/// 集成测试不能走这个函数：插件会拖进 `rfd`，它调用 comctl32 v6 才有的
/// `TaskDialogIndirect`。测试二进制没有 `tauri-build` 生成的那份 Windows 清单，
/// 会被绑定到 comctl32 v5，于是在加载阶段就报 `STATUS_ENTRYPOINT_NOT_FOUND`
/// （0xC0000139）——进程根本起不来，表现为"测试毫无输出地失败"。
pub fn configure_app<R: tauri::Runtime>(builder: tauri::Builder<R>) -> tauri::Builder<R> {
    configure(builder)
        .plugin(tauri_plugin_dialog::init())
        .plugin(
            tauri_plugin_global_shortcut::Builder::new()
                .with_handler(|app, shortcut, event| {
                    // 只在按下时触发（避免抬起也触发一次）
                    if event.state() != tauri_plugin_global_shortcut::ShortcutState::Pressed {
                        return;
                    }
                    // Dock 显隐热键
                    let dock_toggle =
                        "alt+shift+s".parse::<tauri_plugin_global_shortcut::Shortcut>();
                    if dock_toggle.as_ref().ok() == Some(shortcut) {
                        tracing::info!("Dock 显隐热键已触发（Alt+Shift+S）");
                        // 必须走 reveal/conceal 收口：`show()`/`unminimize()` 是 tao
                        // 唯一会无条件把 WS_CAPTION 写回 GWL_STYLE 并 FRAMECHANGED 的路径
                        // （标题栏回归的真凶，见 commands::install_frameless_subclass 的根因表）。
                        if let Some(w) = app.get_webview_window("main") {
                            match w.is_visible() {
                                Ok(true) => commands::conceal_main_window(app),
                                _ => commands::reveal_main_window(app),
                            }
                        }
                        return;
                    }
                    // 这行是判别器：热键触发没触发，日志说了算——
                    // 没有这行 → 注入的输入没到 RegisterHotKey（只能人工按）；
                    // 有这行但没开始录制 → 问题在投递链（那是我们的 bug）。
                    tracing::info!("全局快捷键已触发（Ctrl+Alt+R）");
                    push_ui_command(app, UiCommand::Toggle, "全局快捷键");
                })
                .build(),
        )
        .on_window_event(|window, event| {
            // 焦点生命周期样式核验：实测"启动无边框、切走焦点
            // 原生标题栏出现 "——焦点迁移是非客户区重画的触发点。每次焦点事件后核验
            // 主窗口样式：回归即拍校正（函数内部绝不抢焦点）。
            if let tauri::WindowEvent::Focused(f) = event {
                if window.label() == "main" {
                    commands::verify_native_frame_on_focus(window.app_handle(), *f);
                }
            }
            // 几何突变后的即拍核验：setSize/unminimize 等会触发 tao 的
            // 样式重放 ⇒ 标题栏回归。不等护栏的 750ms 拍（那期间框会闪现），
            // Resized 事件一到立刻 enforce。
            if let tauri::WindowEvent::Resized(_) = event {
                if window.label() == "main" {
                    commands::verify_native_frame_after_geometry(window.app_handle(), "Resized");
                }
            }
            // 关掉最后一个窗口 = 进程退出（Tauri 默认；全仓没有别的 on_window_event，
            // 也没有 RunEvent::ExitRequested 兜底）。录制中这样退出会让正在写的
            // mp4 缺 `moov` → 文件报废。
            // 所以录制中拦下来：先停干净并最终化，再走同一条退出收口路径。
            // 空闲时维持原语义——没有任何东西需要最终化。
            if let tauri::WindowEvent::CloseRequested { api, .. } = event {
                // 这个 handler 对每一个窗口都会触发，包括框选覆盖层。
                //
                // 这里出过事故：`confirm_region` 里 `overlay.close()` 触发本 handler，
                // 而"空闲 → 退出"那段没区分窗口，于是框选完成的一瞬间整个 App 被自己退掉了
                // （现象：框选后程序直接退出）。日志形态：
                // 框选完成（已换算为物理像素） x=... y=... width=... height=...
                // 窗口关闭请求：空闲 → 无待最终化内容，退出 ← 相邻 0.1ms
                // 同一个坑还有第二个入口：`open_region_selector` 会先 close 掉旧覆盖层。
                //
                // 所以判断逻辑抽到 `close_action()`（有测试），这里只做分发。
                let recording = window.app_handle().state::<AppState>().is_recording();
                match close_action(window.label(), recording) {
                    CloseAction::PassThrough => {
                        tracing::info!(
                            window = window.label(),
                            "辅助窗口关闭请求：放行（不影响主窗口与录制）"
                        );
                    }
                    CloseAction::StopThenExit => {
                        api.prevent_close();
                        tracing::info!("窗口关闭请求：正在录制 → 先停止并最终化，再退出");
                        begin_quit(window.app_handle(), "窗口关闭按钮");
                    }
                    CloseAction::ExitNow => {
                        // 空闲：没有任何东西需要最终化，但仍然走我们自己的退出路径——
                        // 否则 handler 一返回进程就结束，异步日志缓冲里最后几行
                        // （包括上面这行）会一起丢掉，而那正是退出路径最该留的证据。
                        api.prevent_close();
                        tracing::info!("窗口关闭请求：空闲 → 无待最终化内容，退出");
                        flush_logs();
                        window.app_handle().exit(0);
                    }
                }
            }
        })
}

/// 统一的退出收口：入队 `Quit` → 等前端"停完并最终化"的回声 → 退出进程。
///
/// 托盘菜单与窗口关闭按钮必须走同一条路：任何"直接 `exit`"的路径都会让正在写的
/// mp4 缺 `moov` → 文件报废。这也是为什么不能"睡固定 1.5 秒就杀"（原实现）：
/// `stop()` 的最坏预算是 `STOP_TOTAL_TIMEOUT_MS = 20s`，1.5s 与典型 finalize 耗时
/// 同量级——这不是边界情况，是掷硬币。
fn begin_quit<R: tauri::Runtime>(app: &tauri::AppHandle<R>, source: &str) {
    let gate = app.state::<AppState>().quit_gate.clone();
    if !gate.mark_requested() {
        tracing::info!(来源 = source, "退出流程已在进行中，忽略重复请求");
        return;
    }
    push_ui_command(app, UiCommand::Quit, source);

    let app = app.clone();
    std::thread::spawn(move || {
        let budget = Duration::from_millis(STOP_TOTAL_TIMEOUT_MS + 5_000);
        if gate.wait(budget) {
            tracing::info!("收到前端退出确认，退出进程");
        } else {
            tracing::warn!(
                timeout_ms = budget.as_millis() as u64,
                "等待前端退出确认超时，仍然退出（录制可能未完成最终化）"
            );
        }
        // 必须先刷盘：异步日志的最后几行正是退出路径的诊断依据
        flush_logs();
        app.exit(0);
    });
}

/// 把命令交给前端。三步，缺一不可：
///
/// 1. 入队——命令是状态，不会丢；
/// 2. 页面已就绪才发"来取一下"的唤醒提示，且提示里不带命令：
/// 这样"提示"与"队列"不会各执行一次（否则同一条 `toggle` 会被执行两次）；
/// 3. 未就绪就只入队：此时 `emit` 注定以 0x8007139F 失败，
/// 发了只是往日志里刷 ERROR；命令本身由 `ui_ready` 或 500ms 轮询取走。
///
/// 托盘与快捷键都只调这一个函数，行为由前端统一决定：录制配置（显示器/帧率/目录）
/// 的唯一真相在前端，后端再造一份状态只会产生两边不一致的问题。
fn push_ui_command<R: tauri::Runtime>(app: &tauri::AppHandle<R>, cmd: UiCommand, source: &str) {
    use tauri::Emitter;

    let state = app.state::<AppState>();
    let queued = state.push_ui_command(cmd);
    tracing::info!(
        命令 = ?cmd,
        来源 = source,
        入队 = queued,
        "命令已入队"
    );

    if !state.ui_ready() {
        tracing::info!(命令 = ?cmd, "页面未就绪，跳过唤醒提示（命令留在队列等前端取走）");
        return;
    }
    match app.emit("ui-command", ()) {
        Ok(()) => tracing::info!(命令 = ?cmd, "唤醒提示已发出"),
        // 提示失败不再是故障：命令在队列里，前端 500ms 内会取走它
        Err(e) => tracing::warn!(error = %e, "唤醒提示发送失败（命令仍在队列，不影响投递）"),
    }
}

/// 创建托盘图标与菜单。
fn setup_tray<R: tauri::Runtime>(app: &tauri::AppHandle<R>) -> tauri::Result<()> {
    use tauri::menu::{Menu, MenuItem, PredefinedMenuItem};
    use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};

    let toggle = MenuItem::with_id(app, "toggle", "开始 / 停止录制", true, None::<&str>)?;
    let show = MenuItem::with_id(app, "show", "显示主窗口", true, None::<&str>)?;
    let open_dir = MenuItem::with_id(app, "open-dir", "打开输出目录", true, None::<&str>)?;
    let sep = PredefinedMenuItem::separator(app)?;
    let quit = MenuItem::with_id(app, "quit", "退出", true, None::<&str>)?;
    let menu = Menu::with_items(app, &[&toggle, &show, &open_dir, &sep, &quit])?;

    let mut builder = TrayIconBuilder::with_id("main-tray")
        .tooltip("ScreenLite")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "toggle" => {
                tracing::info!("托盘菜单已触发：开始 / 停止录制");
                push_ui_command(app, UiCommand::Toggle, "托盘菜单");
            }
            "open-dir" => {
                tracing::info!("托盘菜单已触发：打开输出目录");
                push_ui_command(app, UiCommand::OpenDir, "托盘菜单");
            }
            "show" => {
                tracing::info!("托盘菜单已触发：显示主窗口");
                commands::reveal_main_window(app);
            }
            "quit" => {
                tracing::info!("托盘菜单已触发：退出");
                begin_quit(app, "托盘菜单");
            }
            _ => {}
        })
        .on_tray_icon_event(|tray, event| {
            // 左键单击托盘图标 → 显示主窗口
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                tracing::info!("托盘图标左键：显示主窗口");
                commands::reveal_main_window(tray.app_handle());
                // 这里不再发事件：原来那条 `{"action":"shown"}` 前端根本没处理，
                // 而且它是纯 emit——页面未就绪时会以 0x8007139F 往日志里刷 ERROR。
            }
        });

    if let Some(icon) = app.default_window_icon() {
        builder = builder.icon(icon.clone());
    }
    builder.build(app)?;
    Ok(())
}

/// 日志落盘：每次运行一个文件，写到 `%LOCALAPPDATA%\ScreenLite\logs\`。
///
/// 为什么必须有：release 版是 `windows_subsystem = "windows"`（没有控制台），
/// 不落盘的话失败时拿不到任何日志——而"保留每次的故障日志"正是排查的前提。
///
/// 返回的 guard 必须在进程存活期间一直持有，否则后台写盘线程会被提前关闭。
fn init_logging() -> Option<tracing_appender::non_blocking::WorkerGuard> {
    use tracing_subscriber::layer::SubscriberExt;
    use tracing_subscriber::util::SubscriberInitExt;

    let dir = std::env::var_os("LOCALAPPDATA")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ScreenLite")
        .join("logs");
    if let Err(e) = std::fs::create_dir_all(&dir) {
        eprintln!("创建日志目录失败：{}", e);
        return None;
    }

    let file_name = format!("screenlite-{}.log", screenlite_media::engine::local_timestamp());
    let appender = tracing_appender::rolling::never(&dir, file_name);
    let (non_blocking, guard) = tracing_appender::non_blocking(appender);
    // 留一份用于退出前刷盘（见 flush_logs）
    let _ = LOG_SINK.set(non_blocking.clone());

    let file_layer = tracing_subscriber::fmt::layer()
        .with_writer(non_blocking)
        .with_ansi(false) // 文件里不要 ANSI 转义
        .with_target(false);

    if cfg!(debug_assertions) {
        // 调试版同时保留控制台输出（tauri dev 的终端里能直接看到）
        let _ = tracing_subscriber::registry()
            .with(file_layer)
            .with(tracing_subscriber::fmt::layer().with_target(false))
            .try_init();
    } else {
        let _ = tracing_subscriber::registry().with(file_layer).try_init();
    }

    tracing::info!(log_dir = %dir.display(), "日志已开始记录");
    Some(guard)
}

/// 全局持有 non-blocking 日志的写入端，用于退出前刷盘。
///
/// 为什么需要：`tracing_appender::non_blocking` 是异步写盘——日志先进通道，
/// 由后台线程落盘。`app.exit(0)` 立刻终止进程，通道里最后几条就此丢失。
/// 本次排查里就遇到过这个问题：`窗口关闭请求` / `收到前端退出确认`（恰恰是最该留的
/// 那两行）没能落盘，导致一度无法判断关窗处理到底执行了没有。
static LOG_SINK: std::sync::OnceLock<tracing_appender::non_blocking::NonBlocking> =
    std::sync::OnceLock::new();

/// 退出前把日志刷出去。必须给后台线程时间：`flush()` 只是往通道里发一条消息，
/// 紧接着 `exit` 会把它一起丢掉。
fn flush_logs() {
    use std::io::Write;
    if let Some(sink) = LOG_SINK.get() {
        let _ = sink.clone().flush();
        std::thread::sleep(Duration::from_millis(150));
    }
}

/// 把 WebView2 的启动条件写进每次运行的日志。
///
/// 为什么必须有这一行：曾经把崩溃归因于 `--disable-gpu`，但那个结论对不上账 ——
/// `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS` 是进程级环境变量，事后在 dumps 和
/// 注册表里都查不到，于是"哪几次运行带了这个开关"永远说不清，实验也就不可证伪。
/// 记下它之后，检查点变成两条硬证据：日志里的开关状态 + Crashpad 的 dump 份数。
///
/// 同时记进程号：Crashpad 的 dump 里带 `pid` 注解，两边一对就知道哪份 dump 是哪次运行。
fn log_webview2_conditions() {
    let args = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();

    // 运行时版本：目录名就是版本号（与 Edge 同 build 时说明用的是同一份 Chromium）
    let mut versions: Vec<String> = Vec::new();
    for root in [
        r"C:\Program Files (x86)\Microsoft\EdgeWebView\Application",
        r"C:\Program Files\Microsoft\EdgeWebView\Application",
    ] {
        if let Ok(entries) = std::fs::read_dir(root) {
            for e in entries.flatten() {
                let name = e.file_name().to_string_lossy().to_string();
                if name.starts_with(|c: char| c.is_ascii_digit()) {
                    versions.push(name);
                }
            }
        }
    }
    versions.sort();

    // 二进制身份：没有它就没有归因能力。
    // 这一整场"黑屏"判断反复更正的共同根因就是：日志不记是哪份 exe 在跑，于是
    // "同一份 exe 一次好、一次白"这句话本身无法核实——`target\release\screenlite.exe`
    // 是同一个路径，每次构建都覆盖它，事后再也分不出哪次跑的是哪份。
    // 记 大小 + 修改时间（epoch 秒）即可唯一标识一次构建。
    let (exe_path, exe_bytes, exe_mtime) = std::env::current_exe()
        .ok()
        .map(|p| {
            let meta = std::fs::metadata(&p).ok();
            let bytes = meta.as_ref().map(|m| m.len()).unwrap_or(0);
            let mtime = meta
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0);
            (p.display().to_string(), bytes, mtime)
        })
        .unwrap_or_else(|| ("(取不到)".to_string(), 0, 0));

    tracing::info!(
        pid = std::process::id(),
        exe = %exe_path,
        exe_bytes = exe_bytes,
        exe_mtime_unix = exe_mtime,
        webview2_args = if args.is_empty() { "(未设置)" } else { args.as_str() },
        webview2_runtime = versions.last().map(|s| s.as_str()).unwrap_or("(未找到)"),
        "启动条件（二进制身份 + WebView2 条件）"
    );
}

/// 关窗请求该怎么处理。
///
/// 抽成纯函数是为了能测：这里曾经出过 bug（辅助窗口被当成主窗口），
/// 而那段逻辑埋在闭包里、测不到。见文件末尾的测试。
#[derive(Debug, PartialEq, Eq)]
enum CloseAction {
    /// 放行：辅助窗口（框选覆盖层等）自己关自己，与主窗口和录制无关。
    PassThrough,
    /// 空闲的主窗口：没有任何东西需要最终化，直接退出。
    ExitNow,
    /// 录制中的主窗口：先让前端停干净并最终化，再退出。
    StopThenExit,
}

/// 主窗口的 label（`commands.rs` 里 `get_webview_window("main")` 用的就是它）。
const MAIN_WINDOW_LABEL: &str = "main";

fn close_action(window_label: &str, is_recording: bool) -> CloseAction {
    if window_label != MAIN_WINDOW_LABEL {
        return CloseAction::PassThrough;
    }
    if is_recording {
        CloseAction::StopThenExit
    } else {
        CloseAction::ExitNow
    }
}

/// 把窗口排除出屏幕捕获。
///
/// 为什么必须有：Dock 是 `always_on_top + 透明` 的常驻控制界面，而屏幕录制会拍到
/// 屏幕上的一切 ⇒ 每一段录像里都会有这个 Dock。旧设计靠"录制时自动最小化主窗口"
/// （ADR-009）来规避，Dock 形态下"最小化"不存在了，所以必须换成系统级机制。
pub fn run() {
    // guard 必须活到进程结束
    let _log_guard = init_logging();
    log_webview2_conditions();

    configure_app(tauri::Builder::default())
        .setup(|app| {
            // 进度轮询线程：4Hz 发事件，供 UI 显示时长与指标
            let state = app.state::<AppState>().recorder.clone();
            commands::spawn_progress_poller(app.handle().clone(), state);

            // 托盘：让"开始/停止/打开目录/退出"不需要回到主窗口
            if let Err(e) = setup_tray(app.handle()) {
                tracing::warn!(error = %e, "创建托盘失败（不影响录制功能）");
            }

            // 构建护栏：`cargo build --release` 产出的 exe 不带内嵌前端资源，
            // 于是它不去加载内嵌页面，而是去连 Vite 开发服务器 `http://localhost:1420`
            // ⇒ 窗口显示 `ERR_CONNECTION_REFUSED`、日志没有 `前端已就绪`，
            // 看起来像"前端加载偶发失败"，其实是构建方式错了。
            // 这个坑已经已出现三次。
            //
            // 这里没有构建护栏了 —— 曾经想加一道"这个 exe 有没有内嵌前端"的运行时检查点，
            // 试了三种都不对（`cfg!(feature="custom-protocol")` 在 Tauri 2 对 `tauri build`
            // 也是 false；`asset_resolver().get("index.html")` 与 `get("/index.html")`
            // 在正确构建上也返回 None），三次都在"应当静默"的场景误报。
            // ⇒ 检查点本身不可靠时不要发布，改为把提示并入已验证过的看门狗
            //

            // 必须尽早做：按设置决定控制窗口是否出现在截图/录像里
            // 默认排除（录像里不会有 Dock）；用户可在设置里打开"允许截屏"。
            let saved = settings::load();
            commands::apply_capture_affinity(app.handle(), saved.capture_visible);

            // 让系统别再给这个窗口画外框
            //
            // 不要再加"剥 WS_CAPTION"那一步：它必须配
            // `SetWindowPos(SWP_FRAMECHANGED)` 才生效，而那一句正是让系统重建非客户区的开关，
            // tao 随后又会按 老样子把 `WS_CAPTION` 加回来 ⇒ 标题栏复活（实测）。
            //
            // ——上面这条"已撤"的结论到此作废：撤的原因（tao 会加回来）本次已被
            // `install_frameless_subclass` 从源头解决。顺序因此变为：
            // ① 先装子类（挡住 WM_STYLECHANGING，tao 从此写不回 WS_CAPTION）
            // ② 再剥掉 CreateWindowExW 出生时那份 WS_CAPTION（子类只挡"写入"，
            // 擦不掉"已经写进去的"——启动那 300ms 的标题栏闪现在就没了）
            // ③ 最后才是 DWM 三项（关非客户区渲染 / 边框色 / 圆角）
            if let Some(w) = app.get_webview_window("main") {
                if let Ok(hwnd) = w.hwnd() {
                    commands::install_frameless_subclass(hwnd);
                    let clean = commands::enforce_frameless_style(hwnd);
                    tracing::info!(style_clean = clean, "启动即校正无边框样式（出生时带的 WS_CAPTION）");
                }
                commands::make_window_frame_invisible(&w);
            }
            // 外框护栏：tao 会在 setSize/show/hide 时把上面的属性顶掉 ⇒ 驻留线程周期重压。
            // 子类装上之后它退化为"自检 + DWM 三项重压"：样式已不可能脏，
            // 句柄若被重建它负责发现并把子类重装回去。
            commands::spawn_frame_guard(app.handle().clone());

            // 全局快捷键：从设置里读。
            //
            // 为什么必须可配置：全局快捷键会把它从别的程序手里"scoop"走——
            // 用户自己的工具（输入法扩展、打字/取词类）若用同一个组合，
            // 装了本产品后那个功能就失效。这是全局热键的固有代价，不是 bug，
            // 但必须有出口，所以默认值仍是 Ctrl+Alt+R，用户可以改或关掉。
            if let Err(e) = commands::apply_hotkey(app.handle(), saved.hotkey.as_deref()) {
                // 不阻塞启动：快捷键失效不影响录制本身
                tracing::warn!(error = %e, "应用全局快捷键失败（不影响录制功能）");
            }

            tracing::info!("ScreenLite 启动");

            // 无边框形态的代码层 set_decorations(false) 已撤：
            // 实测本机 tao 版本对顶层窗口不响应（WS_CAPTION 保留），先记录为"升级 tao 可根治"。
            // 本次交付：拖拽区全覆盖 + 尺寸联动（Alt+Shift+S 显隐热键已注册）。
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("启动 Tauri 应用失败");
}

#[cfg(test)]
mod tests {
    use super::{close_action, CloseAction};

    /// 回归测试：框选覆盖层关闭时绝不能退出 App。
    ///
    /// 这就是用户报的"框选后直接程序闪退"——`confirm_region` 里 `overlay.close()`
    /// 触发了 `on_window_event`，而当时的判断没区分窗口，于是整个 App 被自己退掉。
    /// 实测日志：`框选完成（已换算为物理像素）…` 与 `窗口关闭请求：空闲 → …退出` 相邻 0.1ms。
    ///
    /// 这个测试锁的是判断本身（它是纯函数所以测得到）。至于"Tauri 会不会真为
    /// 覆盖层发 CloseRequested"——用户的日志已经证明了会。
    #[test]
    fn overlay_close_must_not_quit_the_app() {
        // 覆盖层：无论录不录制，一律放行
        assert_eq!(
            close_action("region-overlay", false),
            CloseAction::PassThrough,
            "空闲时关闭覆盖层 → 必须只关覆盖层，不能退出 App（这是实测过的 bug）"
        );
        assert_eq!(
            close_action("region-overlay", true),
            CloseAction::PassThrough,
            "录制中关闭覆盖层 → 同样放行（录制要继续）"
        );
        // 将来新增的辅助窗口默认走同一条放行路径
        assert_eq!(close_action("some-future-window", false), CloseAction::PassThrough);
    }

    #[test]
    fn main_window_close_still_finalizes_before_exit() {
        assert_eq!(close_action("main", true), CloseAction::StopThenExit);
        assert_eq!(close_action("main", false), CloseAction::ExitNow);
    }
}
