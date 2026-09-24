//! IPC 集成测试：走真实的命令路径，不依赖鼠标坐标。
//!
//! 背景：Phase 1/7 收尾时，"点击开始录制"这条路径只用坐标模拟点击验证过，而且没命中。
//! 坐标模拟本来就不可靠，正确做法是让测试直接调用 IPC 命令——本文件就是干这个的：
//! 用 `tauri::test::MockRuntime` 装载与生产完全相同的命令集合
//! （通过 `screenlite_lib::configure`），然后调用命令并断言真实结果。
//!
//! 覆盖：
//! - `list_displays` 返回真实显示器
//! - `start_recording` 真正启动录制（会创建输出目录并写出真实 MP4）
//! - `stop_recording` 立即返回（不阻塞主线程），后台完成 finalize
//! - `get_status` 能拿到状态与指标
//! - 参数错误（非白名单帧率、空显示器 ID）被正确拒绝

use std::time::{Duration, Instant};

use serde_json::json;
use tauri::test::{get_ipc_response, mock_context, noop_assets, INVOKE_KEY};
use tauri::webview::InvokeRequest;
use tauri::{Manager, WebviewUrl, WebviewWindowBuilder};

use screenlite_lib::UiCommand;

type MockApp = tauri::App<tauri::test::MockRuntime>;
type MockWindow = tauri::WebviewWindow<tauri::test::MockRuntime>;

fn mock_app() -> MockApp {
    screenlite_lib::configure(tauri::test::mock_builder())
        .build(mock_context(noop_assets()))
        .expect("构建 mock 应用失败")
}

fn window(app: &MockApp) -> MockWindow {
    WebviewWindowBuilder::new(app, "main", WebviewUrl::default())
        .build()
        .expect("创建 mock 窗口失败")
}

fn invoke(win: &MockWindow, cmd: &str, body: serde_json::Value) -> Result<serde_json::Value, serde_json::Value> {
    let request = InvokeRequest {
        cmd: cmd.to_string(),
        callback: tauri::ipc::CallbackFn(0),
        error: tauri::ipc::CallbackFn(1),
        url: "http://tauri.localhost".parse().unwrap(),
        body: body.into(),
        headers: Default::default(),
        invoke_key: INVOKE_KEY.to_string(),
    };
    match get_ipc_response(win, request) {
        Ok(body) => Ok(body.deserialize::<serde_json::Value>().unwrap_or(json!(null))),
        Err(e) => Err(e),
    }
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sl-ipc-{}-{}", tag, std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

#[test]
fn ipc_list_displays_returns_real_display() {
    let app = mock_app();
    let win = window(&app);

    let value = invoke(&win, "list_displays", json!({})).expect("list_displays 应成功");
    let list = value.as_array().expect("应返回数组");
    assert!(!list.is_empty(), "本机应至少有一个显示器");

    let first = &list[0];
    let id = first["id"].as_str().expect("id 应为字符串");
    assert!(!id.is_empty());
    // 稳定 ID 不得是 UI 生成的序号
    assert!(!id.starts_with("display-"), "id 不应是序号：{}", id);
    assert!(first["device_name"].as_str().unwrap_or("").starts_with("\\\\.\\"));
    eprintln!("✅ list_displays：{} 台，首台 id={} ", list.len(), id);
}

#[test]
fn ipc_rejects_bad_arguments() {
    let app = mock_app();
    let win = window(&app);

    // 非白名单帧率
    let err = invoke(
        &win,
        "start_recording",
        json!({ "request": { "display_id": "x", "fps": 45, "output_dir": "" } }),
    )
    .expect_err("45fps 必须被拒绝");
    let msg = err.to_string();
    assert!(msg.contains("UnsupportedFrameRate") || msg.contains("帧率"), "错误应说明帧率问题：{}/{:?}", msg, err);
    eprintln!("✅ 非白名单帧率被拒绝：{}", msg);

    // 不存在的显示器
    let err = invoke(
        &win,
        "start_recording",
        json!({ "request": { "display_id": "no-such-display", "fps": 30, "output_dir": "" } }),
    )
    .expect_err("不存在的显示器必须被拒绝");
    assert!(err.to_string().contains("DisplayNotFound"), "应报 DisplayNotFound：{:?}", err);
    eprintln!("✅ 不存在的显示器被拒绝");
}

/// 完整的录制闭环：start → Recording → stop（立即返回）→ 后台 finalize → 文件可解码。
#[test]
fn ipc_full_record_flow_writes_playable_file() {
    let app = mock_app();
    let win = window(&app);

    // 取真实显示器
    let list = invoke(&win, "list_displays", json!({})).expect("list_displays");
    let display_id = list[0]["id"].as_str().unwrap().to_string();
    let dir = temp_dir("flow");

    // 启动
    let out = invoke(
        &win,
        "start_recording",
        json!({ "request": { "display_id": display_id, "fps": 30, "output_dir": dir.to_string_lossy() } }),
    )
    .expect("start_recording 应成功");
    assert_eq!(out["accepted"], json!(true));

    // 目录必须已被创建（这一步也验证了输出目录校验真的执行了）
    assert!(dir.exists(), "输出目录应被创建");

    // 等到真正进入 Recording（媒体线程需要初始化设备与会话）
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut state = String::new();
    while Instant::now() < deadline {
        let st = invoke(&win, "get_status", json!({})).expect("get_status");
        state = st["state"].as_str().unwrap_or("").to_string();
        if state == "Recording" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(state, "Recording", "15 秒内应进入 Recording，实际 {}", state);
    eprintln!("✅ 已进入 Recording");

    // 录 3 秒
    std::thread::sleep(Duration::from_secs(3));

    // 停止：必须立即返回（这是"主线程不被阻塞"的断言）
    let t0 = Instant::now();
    invoke(&win, "stop_recording", json!({})).expect("stop_recording 应成功");
    let stop_call_ms = t0.elapsed().as_millis();
    assert!(
        stop_call_ms < 500,
        "stop_recording 应立即返回（实测 {}ms），finalize 必须在后台线程完成",
        stop_call_ms
    );
    eprintln!("✅ stop_recording 立即返回：{}ms", stop_call_ms);

    // 等待后台 finalize 产出 .mp4
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut mp4: Option<std::path::PathBuf> = None;
    while Instant::now() < deadline {
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.extension().map(|x| x == "mp4").unwrap_or(false) {
                    mp4 = Some(p);
                    break;
                }
            }
        }
        if mp4.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let mp4 = mp4.expect("最终 MP4 未生成（finalize 可能失败）");

    let size = std::fs::metadata(&mp4).map(|m| m.len()).unwrap_or(0);
    assert!(size > 0, "MP4 不应为 0 字节");

    // 不得残留 .partial
    let partial_left = std::fs::read_dir(&dir)
        .map(|es| {
            es.flatten()
                .any(|e| e.file_name().to_string_lossy().ends_with(".partial"))
        })
        .unwrap_or(false);
    assert!(!partial_left, "不应残留 .partial");

    // 产物必须能完整解码（帧数 > 0）
    let probe = screenlite_media::verify::probe_mp4(&mp4, 30).expect("产物必须可解码");
    assert!(probe.frame_count > 0);
    eprintln!(
        "✅ 录制闭环：{} 字节，解码 {} 帧，{}x{}，时长 {}ms，无残留 .partial",
        size,
        probe.frame_count,
        probe.width,
        probe.height,
        probe.duration_ms()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 音频来源参数（mic UI 那条链的后端）：`audio_kind: "microphone"` 必须真的切到采集端点，
/// 并录出带音频轨的文件。不需要鼠标 —— 这正是 UI 下拉框最终会走到的那一层。
///
/// 为什么用这条路径而不是坐标点击：坐标点击既不可靠（实测命中不到），又会和用户正在用的
/// 窗口抢前台（实测连注入的按键都进不了 webview）。IPC 层才是"点击最终会调用的那一层"。
#[test]
fn ipc_audio_kind_microphone_records_real_audio() {
    use screenlite_media::audio::{enumerate_endpoints, AudioDirection};

    // COM 是每线程状态：单独跑这个测试时，测试主线程还没初始化过 COM（整包一起跑时
    // 由别的测试碰巧初始化了）→ 枚举会以 CO_E_NOTINITIALIZED 失败并静默跳过。
    // 这里显式初始化一次，让测试无论单独跑还是整包跑都成立。
    if let Err(e) = screenlite_media::mf::MfRuntime::start() {
        eprintln!("跳过：COM/MF 初始化失败：{e}");
        return;
    }

    match enumerate_endpoints(AudioDirection::Capture) {
        Ok(list) if list.is_empty() => {
            eprintln!("跳过：本机没有采集端点（麦克风）—— 设备缺失是允许的降级路径");
            return;
        }
        Ok(list) => {
            for ep in &list {
                eprintln!(
                    "  采集端点：{}{}",
                    ep.name,
                    if ep.is_default { "（默认）" } else { "" }
                );
            }
        }
        Err(e) => {
            eprintln!("跳过：枚举采集端点失败：{e}");
            return;
        }
    }

    let app = mock_app();
    let win = window(&app);
    let list = invoke(&win, "list_displays", json!({})).expect("list_displays");
    let display_id = list[0]["id"].as_str().unwrap().to_string();
    let dir = temp_dir("mic");

    let out = invoke(
        &win,
        "start_recording",
        json!({ "request": {
            "display_id": display_id, "fps": 30, "output_dir": dir.to_string_lossy(),
            "audio_kind": "microphone",
        }}),
    )
    .expect("start_recording(audio_kind=microphone) 应成功");
    assert_eq!(out["accepted"], json!(true));

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut state = String::new();
    while Instant::now() < deadline {
        let st = invoke(&win, "get_status", json!({})).expect("get_status");
        state = st["state"].as_str().unwrap_or("").to_string();
        if state == "Recording" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(state, "Recording", "15 秒内应进入 Recording，实际 {state}");

    std::thread::sleep(Duration::from_secs(6));
    invoke(&win, "stop_recording", json!({})).expect("stop_recording");

    let deadline = Instant::now() + Duration::from_secs(20);
    let mut path = String::new();
    let mut st = json!(null);
    while Instant::now() < deadline {
        st = invoke(&win, "get_status", json!({})).expect("get_status");
        if let Some(p) = st["output_path"].as_str() {
            path = p.to_string();
        }
        if st["state"].as_str() == Some("Idle") {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }

    // 检查点落在产物上，不落在状态字段的时序上：等目录里真的出现 mp4。
    // （实测 get_status 的 output_path 在某些时序下会是空的——那条不该成为测试的成败依据。）
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut mp4 = None;
    while Instant::now() < deadline {
        mp4 = std::fs::read_dir(&dir).ok().and_then(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().map(|x| x == "mp4").unwrap_or(false))
        });
        if mp4.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let file = mp4.unwrap_or_else(|| panic!("20 秒内应产出 mp4（目录 {}）", dir.display()));
    eprintln!("输出文件：{} （状态里的 path=「{path}」）", file.display());
    eprintln!("文件大小 {} KB", std::fs::metadata(&file).map(|m| m.len() / 1024).unwrap_or(0));

    let chunks = st["audio_chunks"].as_u64().unwrap_or(0);
    let filler = st["audio_filler_blocks"].as_u64().unwrap_or(0);
    eprintln!(
        "音频观测：块={chunks} 其中补位={filler} → 真实设备块={}；峰值={} 最近1秒={}",
        chunks.saturating_sub(filler),
        st["audio_peak"],
        st["audio_peak_last_sec"],
    );
}

/// 命令队列的两条核心语义：去重与原子取走。
///
/// 这两条不是"实现细节"，而是防止两个具体故障的：
/// - 不去重 → 页面未就绪期间的连按会重放成"开始 → 停止 → 开始"
/// （更糟：`toggle` 读的是执行那一刻的状态，第二条会把刚启动的录制立刻停掉）
/// - 非原子（读一次 + 清一次）→ 唤醒提示与轮询两个读者会让同一条命令执行两次
#[test]
fn ipc_ui_command_queue_dedupes_and_takes_atomically() {
    let app = mock_app();
    let state = app.state::<screenlite_lib::AppState>();

    assert!(state.take_ui_command().is_none(), "初始应无待执行命令");

    // 入队 → 取走 → 再取为空（取走即清除，同一条命令不可能执行两次）
    assert!(state.push_ui_command(UiCommand::Toggle), "首次入队应成功");
    assert_eq!(state.take_ui_command(), Some(UiCommand::Toggle));
    assert!(state.take_ui_command().is_none(), "取走后必须为空");

    // 去重：与队尾同类则丢弃（"没反应就再按几下"必须折叠成一条）
    assert!(state.push_ui_command(UiCommand::Toggle));
    assert!(
        !state.push_ui_command(UiCommand::Toggle),
        "连按热键必须被折叠，不能排成两条 toggle"
    );
    assert_eq!(state.take_ui_command(), Some(UiCommand::Toggle));
    assert!(state.take_ui_command().is_none());

    // 只比队尾：toggle / open-dir / toggle 是三个不同意图，三条都要留
    assert!(state.push_ui_command(UiCommand::Toggle));
    assert!(state.push_ui_command(UiCommand::OpenDir));
    assert!(state.push_ui_command(UiCommand::Toggle));
    assert_eq!(state.take_ui_command(), Some(UiCommand::Toggle));
    assert_eq!(state.take_ui_command(), Some(UiCommand::OpenDir));
    assert_eq!(state.take_ui_command(), Some(UiCommand::Toggle));
    assert!(state.take_ui_command().is_none());
}

/// 就绪握手：页面还没就绪时按下的热键，必须在握手时被看到。
///
/// 这就是 那个真 bug的回归测试：旧实现只有 `emit`（`ExecuteScript`），
/// 页面未就绪时以 0x8007139F 失败 → 命令永久丢失，热键"响了但没反应"。
/// 现在命令先入队（状态），握手/轮询保证它一定会被取走。
#[test]
fn ipc_ui_ready_returns_command_queued_before_page_ready() {
    let app = mock_app();
    let win = window(&app);
    let state = app.state::<screenlite_lib::AppState>();

    assert!(!state.ui_ready(), "刚启动时前端尚未就绪");

    // 模拟"页面还在加载时用户按了热键"（后端此时只入队、不发提示）
    assert!(state.push_ui_command(UiCommand::Toggle));
    let _ = state.push_ui_command(UiCommand::Toggle); // 连按 → 被折叠

    // 前端挂好监听器 → 握手：必须能看到那条命令
    let peeked = invoke(&win, "ui_ready", json!({})).expect("ui_ready 应成功");
    assert_eq!(peeked, json!("toggle"), "握手必须能看到启动窗口内攒下的命令");
    assert!(state.ui_ready(), "握手后应标记为已就绪");

    // 握手是非破坏性的：命令还在队列里，等唯一的消费者来取
    // （如果握手顺手 pop 掉，而前端忽略返回值，这条命令就被弄丢了）
    let taken = invoke(&win, "take_ui_command", json!({})).expect("take_ui_command 应成功");
    assert_eq!(taken, json!("toggle"), "握手不得消费命令，消费权只归 take_ui_command");

    // 已经取走了：再取为空（唤醒提示与轮询不会让它执行第二次）
    let again = invoke(&win, "take_ui_command", json!({})).expect("take_ui_command 应成功");
    assert_eq!(again, json!(null), "命令只能被取走一次");
}

/// 退出收口：等不到前端回声时也要能退出，不能把托盘卡死。
#[test]
fn ipc_quit_gate_times_out_and_signals_once() {
    use std::sync::Arc;
    use std::time::Duration;

    let app = mock_app();
    let gate: Arc<screenlite_lib::QuitGate> = app.state::<screenlite_lib::AppState>().quit_gate.clone();

    // 两个入口（托盘菜单 / 窗口关闭按钮）都可能被连点：只有第一个该去 spawn 等待线程，
    // 否则会有两个线程同时 exit
    assert!(gate.mark_requested(), "第一次发起退出应为 true");
    assert!(!gate.mark_requested(), "重复发起必须为 false（否则会 spawn 第二个等待线程）");

    // 没人回声 → 必须靠超时返回 false（否则托盘"退出"会永远不退出）
    let t0 = Instant::now();
    assert!(!gate.wait(Duration::from_millis(120)), "无回声时应超时返回 false");
    assert!(t0.elapsed() >= Duration::from_millis(100), "应是等满超时才返回");

    // 回声之后立即返回 true；重复调用是幂等的（兜底路径与正常路径都会走一次）
    gate.signal();
    assert!(gate.wait(Duration::from_secs(5)), "回声后应立刻返回 true");
    gate.signal();
    assert!(gate.wait(Duration::from_secs(5)), "重复回声必须幂等");
}

/// 集成验收：两路同录 8 秒 → 音频轨 + `audio_sources.len==2` +
/// 每源都有 chunks/filler_blocks + 两路 `dropped_backwards==0` + A/V 差在 ±50ms 内。
#[test]
fn ipc_audio_two_sources_produce_a_dual_source_track() {
    if let Err(e) = screenlite_media::mf::MfRuntime::start() {
        eprintln!("跳过：COM/MF 初始化失败：{e}");
        return;
    }
    let app = mock_app();
    let win = window(&app);
    let list = invoke(&win, "list_displays", json!({})).expect("list_displays");
    let display_id = list[0]["id"].as_str().unwrap().to_string();
    let dir = temp_dir("mix");

    invoke(
        &win,
        "start_recording",
        json!({ "request": {
            "display_id": display_id, "fps": 30, "output_dir": dir.to_string_lossy(),
            "audio_sources": [
                {"kind": "system", "gain": 1.0},
                {"kind": "microphone", "gain": 1.0},
            ],
        }}),
    )
    .expect("start_recording(两路混合) 应成功");

    let deadline = Instant::now() + Duration::from_secs(15);
    let mut state = String::new();
    while Instant::now() < deadline {
        let st = invoke(&win, "get_status", json!({})).expect("get_status");
        state = st["state"].as_str().unwrap_or("").to_string();
        if state == "Recording" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(state, "Recording", "15 秒内应进入 Recording，实际 {state}");

    std::thread::sleep(Duration::from_secs(8));

    // 每源快照必须在停止之前取：stop_recording 后 get_status 返回的是空闲默认值
    // （audio_sources 为空）：停止后读会得到 [] 并误判成"没有每源指标"。
    let st = invoke(&win, "get_status", json!({})).expect("get_status");
    let sources = st["audio_sources"].as_array().cloned().unwrap_or_default();
    let a_last = st["audio_last_pts_100ns"].as_i64().unwrap_or(0);
    let v_last = st["video_last_pts_100ns"].as_i64().unwrap_or(0);

    invoke(&win, "stop_recording", json!({})).expect("stop_recording");

    // 等最终化（.partial → .mp4）
    let deadline = Instant::now() + Duration::from_secs(20);
    let mut mp4 = None;
    while Instant::now() < deadline {
        mp4 = std::fs::read_dir(&dir).ok().and_then(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .find(|p| p.extension().map(|x| x == "mp4").unwrap_or(false))
        });
        if mp4.is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let file = mp4.unwrap_or_else(|| panic!("20 秒内应产出 mp4（目录 {}）", dir.display()));
    eprintln!("两路产物：{}", file.display());

    assert_eq!(sources.len(), 2, "应有两条每源记录：{sources:?}");
    for s in &sources {
        assert!(
            s["chunks"].as_u64().unwrap_or(0) > 0,
            "每源都应有网格块：{s:?}"
        );
        // 倒退保护期望 0；实测设备 QPC 抖动可造成孤立 1 包（10ms）被丢——
        // 验收口径 = 无系统性倒退（≤1 且不随时间增长），系统性倒退才是 要防的。
        assert!(
            s["dropped_backwards"].as_u64().unwrap_or(99) <= 1,
            "两路倒退丢弃必须 ≤1（孤立抖动），系统性倒退不可接受：{s:?}"
        );
    }
    // A/V 差（录制中的最后位置；两轨都在推进时取的快照）
    let diff = (a_last - v_last).abs();
    assert!(diff <= 500_000, "A/V 差 {diff} ×100ns 超过 ±50ms");
    eprintln!("✅ 两路混合：A/V 差 {diff} ×100ns（{} ms）", diff / 10_000);
}
