//! Tauri 命令与事件。
//!
//! 两条硬性要求：
//!
//! 1. `stop_recording` 绝不在主线程同步等待。 `Recorder::stop()` 会阻塞到
//! finalize 完成（正常 29–37ms，但磁盘慢或长录制可能到秒级）。这里把它放到
//! 后台线程，命令立即返回，完成后发 `recording-finished`。
//! 2. UI 线程不碰媒体：所有命令只做参数校验与状态读取，真正的采集/编码
//! 在 `screenlite-media` 自己的线程里。

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};

use screenlite_media::capture;
use screenlite_media::encoder::HardwarePreference;
use screenlite_media::engine::{EngineState, Recorder, RecordingConfig};

/// 后端 → 前端的命令。
///
/// 它是"状态"不是"事件"：先入队，再由前端拉取。不能靠 `emit` 直接投递——
/// `emit` 走 `ICoreWebView2::ExecuteScript`，页面就绪前会以 0x8007139F 失败，
/// 命令永久丢失。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum UiCommand {
    Toggle,
    OpenDir,
    Quit,
}

/// 托盘"退出"的一次性交会点。
///
/// `Recorder::stop()` 的最坏预算是 `STOP_TOTAL_TIMEOUT_MS = 20s`
/// （`crates/screenlite-media/src/consts.rs`）。半路杀进程会让正在写的 mp4
/// 缺 `moov` → 文件不可播。所以托盘不能"睡一会儿就 `exit`"，
/// 必须等前端把录制停完并最终化之后回声。
#[derive(Default)]
pub struct QuitGate {
    done: Mutex<bool>,
    cv: Condvar,
    /// 是否已经有人发起过退出。两个入口（托盘菜单 / 窗口关闭按钮）都可能被连点，
    /// 只有第一次需要 spawn 等待线程，否则会出现两个线程同时 `exit`。
    requested: AtomicBool,
}

impl QuitGate {
    /// 前端回声。可重复调用（幂等）：兜底与正常路径可能都会走一次。
    pub fn signal(&self) {
        let mut done = self.done.lock().unwrap();
        *done = true;
        self.cv.notify_all();
    }

    /// 第一个发起退出的人返回 true（只有它该去 spawn 等待线程）。
    pub fn mark_requested(&self) -> bool {
        !self.requested.swap(true, Ordering::SeqCst)
    }

    /// 返回 true = 前端在超时前回声了。
    pub fn wait(&self, timeout: Duration) -> bool {
        match self.cv.wait_timeout_while(self.done.lock().unwrap(), timeout, |d| !*d) {
            Ok((guard, _)) => *guard,
            Err(_) => false, // 锁中毒：当作没等到，走兜底超时
        }
    }
}

/// 应用级共享状态。
pub struct AppState {
    /// 当前活跃的 Recorder。`stop_recording` 会把它取出（置 None），
    /// 因此进度轮询与停止流程不会互相卡住。
    pub recorder: Arc<Mutex<Option<Recorder>>>,
    /// 框选覆盖层的上下文（目标显示器 + 权威物理尺寸），仅在覆盖层打开期间有值。
    pub selector: Arc<Mutex<Option<SelectorContext>>>,
    /// 待投递的命令队列。热键与托盘只入队，前端来取。
    pub ui_pending: Arc<Mutex<VecDeque<UiCommand>>>,
    /// 前端是否已注册完监听器（`ui_ready` 置位）。
    /// 未就绪时不发唤醒提示：那些 emit 注定以 0x8007139F 失败，
    /// 发了只是往日志里刷 ERROR。
    pub ui_ready: Arc<AtomicBool>,
    /// 退出交会点。
    pub quit_gate: Arc<QuitGate>,
}

/// 框选覆盖层打开期间记住的换算基准。
#[derive(Debug, Clone)]
pub struct SelectorContext {
    pub display_id: String,
    /// 权威采集尺寸（物理像素）
    pub phys_w: u32,
    pub phys_h: u32,
    /// 框选窗口的逻辑尺寸（CSS 像素），用于把归一化坐标还原
    pub logical_w: f64,
    pub logical_h: f64,
}

impl Default for AppState {
    fn default() -> Self {
        Self {
            recorder: Arc::new(Mutex::new(None)),
            selector: Arc::new(Mutex::new(None)),
            ui_pending: Arc::new(Mutex::new(VecDeque::new())),
            ui_ready: Arc::new(AtomicBool::new(false)),
            quit_gate: Arc::new(QuitGate::default()),
        }
    }
}

impl AppState {
    /// 命令入队。去重规则：只在"待入队命令与队尾同类"时丢弃并返回 false。
    ///
    /// 为什么必须去重：页面未就绪期间用户会连按热键（"没反应就再按几下"）。
    /// 原样重放会变成"开始 → 停止 → 开始"；更糟的是 `toggle` 在前端读的是
    /// 执行那一刻的状态，所以第二条会把刚启动的录制立刻停掉。
    /// 用户的本意是"我要它开始录"，不是"录一下再停"。
    ///
    /// 只比队尾（不比全队列）：`toggle, open-dir, toggle` 三条都要留——
    /// 那是三个不同的意图，不能折叠。
    pub fn push_ui_command(&self, cmd: UiCommand) -> bool {
        let mut q = self.ui_pending.lock().unwrap();
        if q.back() == Some(&cmd) {
            return false;
        }
        q.push_back(cmd);
        true
    }

    /// 原子取走：一次加锁内 pop 并清除。
    ///
    /// 绝不能做成"读状态 + 前端回 ack"两步：这条链上必然有多个读者
    /// （唤醒提示 + 常驻轮询），两步之间重入会让同一条 `toggle` 被执行两次
    /// ——第一次 start、第二次读到 `Preparing` 于是立刻 stop。
    pub fn take_ui_command(&self) -> Option<UiCommand> {
        self.ui_pending.lock().unwrap().pop_front()
    }

    /// 看一眼队首但不消费。只给诊断/测试用。
    ///
    /// 为什么 `ui_ready` 必须用 peek 而不是 take：前端的命令分发只有一条路径
    /// （`takeCommand` → `take_ui_command`），它忽略 `ui_ready` 的返回值。如果握手时
    /// 顺手把命令 pop 出来，那条命令就被"读走又没人执行"地弄丢了——
    /// 与 那个 bug 同样的后果，而且更难查。
    pub fn peek_ui_command(&self) -> Option<UiCommand> {
        self.ui_pending.lock().unwrap().front().copied()
    }

    /// 标记前端已就绪，返回是否是首次。可重复调用（前端重挂监听器时）。
    pub fn mark_ui_ready(&self) -> bool {
        !self.ui_ready.swap(true, Ordering::SeqCst)
    }

    pub fn ui_ready(&self) -> bool {
        self.ui_ready.load(Ordering::SeqCst)
    }

    /// 是否有活跃录制。`stop_recording` 会把 Recorder 取出置 None，
    /// 所以 `is_some()` 就是"还没停完"——比读 `EngineState` 少一次加锁。
    pub fn is_recording(&self) -> bool {
        self.recorder.lock().unwrap().is_some()
    }
}

#[derive(Debug, Deserialize)]
pub struct StartRequest {
    pub display_id: String,
    pub fps: u32,
    pub output_dir: String,
    /// 区域录制：以采集画幅的物理像素为坐标的裁剪矩形。省略或等于整幅 = 全屏录制。
    #[serde(default)]
    pub region: Option<screenlite_media::convert::Region>,
    /// 可选：诊断用软件编码路径
    #[serde(default)]
    pub prefer_software: bool,
    /// 音频来源：`[{kind:"system"|"microphone", gain:1.0}]`。
    /// 空数组 = 拒绝启动（不做"静音录制"模糊态）；缺席 = 用旧字段 `audio_kind`（兼容别名）。
    #[serde(default)]
    pub audio_sources: Option<Vec<AudioSourceInput>>,
    /// 兼容别名：`audio_sources` 缺席时生效，等价于"单路 + gain 1.0"。
    #[serde(default)]
    pub audio_kind: Option<String>,
}

/// 一路音频来源的入参
#[derive(Debug, serde::Deserialize)]
pub struct AudioSourceInput {
    /// `"system"`（系统声音/扬声器回环）或 `"microphone"`（默认输入设备）
    pub kind: String,
    /// 每源线性增益 [0, 2]，默认 1.0
    #[serde(default = "default_gain")]
    pub gain: f32,
}

fn default_gain() -> f32 {
    1.0
}

#[derive(Debug, Serialize)]
pub struct StartResult {
    pub accepted: bool,
}

/// 事件负载：进度（4Hz 节流）
#[derive(Debug, Clone, Serialize)]
pub struct ProgressPayload {
    pub elapsed_ms: i64,
    pub frames_captured: u64,
    pub frames_scheduled: u64,
    pub frames_duplicated: u64,
    pub frames_dropped: u64,
    pub frames_encoded: u64,
    pub queue_degraded: bool,
    /// 每源音频指标：UI的每源峰值/补位读数
    pub audio_sources: Vec<screenlite_media::engine::AudioSourceStatus>,
}

/// 事件负载：状态跳变
#[derive(Debug, Clone, Serialize)]
pub struct StatePayload {
    pub state: EngineState,
    pub error_code: Option<String>,
    pub error_message: Option<String>,
}

/// 事件负载：结束
#[derive(Debug, Clone, Serialize)]
pub struct FinishedPayload {
    pub output_path: String,
    pub duration_ms: i64,
    pub frames_encoded: u64,
    pub frames_dropped: u64,
    pub stop_reason: String,
}

#[tauri::command]
pub fn list_displays() -> Result<Vec<capture::DisplayInfo>, String> {
    capture::enumerate_displays().map_err(|e| format!("[{}] {}", e.code(), e))
}

/// 启动录制。
///
/// 只做同步校验（帧率白名单、显示器存在、输出目录可写、磁盘空间预检在
/// 媒体线程内完成），随后立即返回。失败通过 `recording-error` 事件上报，
/// 不让 UI 卡在命令调用上。
///
/// 泛型 `R`：生产用 `Wry`，集成测试用 `MockRuntime`，两边共用同一份实现。
#[tauri::command]
pub fn start_recording<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    request: StartRequest,
) -> Result<StartResult, String> {
    {
        let guard = state.recorder.lock().unwrap();
        if guard.is_some() {
            return Err("已有录制在进行中".into());
        }
    }

    let mut cfg = RecordingConfig::new(request.display_id, if request.output_dir.trim().is_empty() {
        // 空目录 → 用系统解析出的默认目录（%USERPROFILE%\Videos\ScreenLite）。
        // 不能把空串直接传下去，否则目录校验必然失败。
        screenlite_media::disk::default_output_dir()
            .map_err(|e| format!("解析默认输出目录失败：[{}] {}", e.code(), e))?
            .to_string_lossy()
            .to_string()
    } else {
        request.output_dir
    });
    cfg.fps = request.fps;
    cfg.hardware = if request.prefer_software {
        HardwarePreference::PreferSoftware
    } else {
        HardwarePreference::PreferHardware
    };
    cfg.region = request.region;
    // ---- 音频来源：新形状 audio_sources 优先；旧字段 audio_kind 为兼容别名 ----
    match &request.audio_sources {
        Some(list) => {
            if list.is_empty() {
                return Err("至少选择一路音频来源（系统声音 / 麦克风）——不做\"静音录制\"模糊态".into());
            }
            let mut sources = Vec::with_capacity(list.len());
            for s in list {
                let kind = match s.kind.as_str() {
                    "microphone" => screenlite_media::audio::AudioSourceKind::Microphone,
                    // 未知取值 → 系统声音（向后兼容）
                    _ => screenlite_media::audio::AudioSourceKind::SystemLoopback,
                };
                sources.push(screenlite_media::audio::AudioSourceSpec {
                    kind,
                    gain: screenlite_media::audio::clamp_gain(s.gain),
                });
            }
            cfg.audio_sources = sources;
        }
        None => {
            // 兼容别名：单路 + gain 1.0（现有 IPC 测试/旧调用方都走这条）
            let kind = match request.audio_kind.as_deref() {
                Some("microphone") => screenlite_media::audio::AudioSourceKind::Microphone,
                _ => screenlite_media::audio::AudioSourceKind::SystemLoopback,
            };
            cfg.audio_sources = vec![screenlite_media::audio::AudioSourceSpec { kind, gain: 1.0 }];
        }
    }
    tracing::info!(
        sources = ?cfg
            .audio_sources
            .iter()
            .map(|s| (s.kind.as_str(), s.gain))
            .collect::<Vec<_>>(),
        "本次录制的音频来源（实际端点名与格式见下面「音频端点已打开」那行）"
    );

    // 同步校验（快速失败，错误直接返回给调用方）
    let recorder = match Recorder::start(cfg) {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("[{}] {}", e.code(), e);
            let _ = app.emit(
                "recording-error",
                serde_json::json!({ "code": e.code(), "message": e.to_string() }),
            );
            return Err(msg);
        }
    };

    *state.recorder.lock().unwrap() = Some(recorder);

    // 录制开始前必须关掉框选覆盖层（这是个看不见的坑）：
    // 那个窗口是 `transparent + always_on_top + skip_taskbar + 全屏`（见 open_region_selector）——
    // 只要它还活着，就会看不见地吞掉整块屏幕的鼠标输入：用户看着正常桌面，
    // 但点击落在覆盖层上（会拉起一次框选）、滚轮没有反应 ⇒ 症状就是
    // "鼠标左右键和滚轮都不正常、点不到我想要的目标"。
    // 触发路径很自然：打开框选 → 没确认也没取消 → 直接按热键/托盘开始录制。
    if let Some(overlay) = app.get_webview_window("region-overlay") {
        tracing::info!("开始录制：关闭仍打开的框选覆盖层（否则它会吞掉全屏鼠标输入）");
        let _ = overlay.close();
    }

    // ADR-009：录制开始后最小化主窗口，避免控制界面被录进画面
    // （V0.1 没有托盘/快捷键，用户从任务栏恢复窗口来停止）
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.minimize();
    }

    Ok(StartResult { accepted: true })
}

/// 停止录制。
///
/// 立即返回；真正的 finalize 在后台线程完成，结果通过 `recording-finished` 上报。
#[tauri::command]
pub fn stop_recording<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
) -> Result<(), String> {
    let recorder = state
        .recorder
        .lock()
        .unwrap()
        .take()
        .ok_or_else(|| "当前没有录制在进行".to_string())?;

    // 恢复窗口，让看到最终状态
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.unminimize();
        let _ = window.set_focus();
    }

    std::thread::Builder::new()
        .name("screenlite-finalize".into())
        .spawn(move || match recorder.stop() {
            Ok(outcome) => {
                // 停止汇总：验收要看的数一次写全。
                //
                // 为什么必须落日志：这些值过去只通过 `recording-finished` 发给前端，
                // 于是"报数"只能靠人读界面——长跑验收就无法脚本化判读了。
                // A/V 差直接由两个 PTS 相减得到，不做逐帧探测（`probe_mp4` 对
                // 1.6 GB 产物要跑几分钟，不能放进停止路径）。
                let av_diff_ms =
                    (outcome.video_last_pts_100ns - outcome.audio_last_pts_100ns) / 10_000;
                tracing::info!(
                    path = %outcome.finalize.path.display(),
                    停止原因 = outcome.stop_reason.as_str(),
                    时长_ms = outcome.elapsed_ms,
                    已编码帧 = outcome.finalize.samples,
                    计划帧 = outcome.frames_scheduled,
                    重复帧 = outcome.frames_duplicated,
                    丢帧_稳态 = outcome.frames_dropped_backpressure,
                    丢帧_启动瞬态 = outcome.frames_dropped_startup,
                    音频块 = outcome.audio_chunks,
                    音频补位块 = outcome.audio_filler_blocks,
                    音频真实设备块 = outcome.audio_chunks.saturating_sub(outcome.audio_filler_blocks),
                    音频峰值 = outcome.audio_peak,
                    音频倒退丢弃 = outcome.audio_dropped_backwards,
                    时间戳异常 = outcome.timestamp_anomalies,
                    采集最长无帧_ms = outcome.capture_idle_ms_max,
                    av差_ms = av_diff_ms,
                    "停止汇总"
                );
                // 每源明细：
                // 只看聚合值抓不住"一路真实 + 一路全是补位"——那正是"某一路没进来"最常见的样子，
                // 而聚合行看起来完全正常。长跑验收脚本按这几行判每一路是否真的在流。
                for s in &outcome.audio_sources {
                    tracing::info!(
                        kind = %s.kind,
                        endpoint = ?s.endpoint,
                        块 = s.chunks,
                        补位 = s.filler_blocks,
                        真实设备块 = s.real_blocks,
                        峰值 = s.peak,
                        最近1秒 = s.peak_last_sec,
                        倒退丢弃 = s.dropped_backwards,
                        "音频源明细"
                    );
                }
                let _ = app.emit(
                    "recording-finished",
                    FinishedPayload {
                        output_path: outcome.finalize.path.to_string_lossy().to_string(),
                        duration_ms: outcome.elapsed_ms,
                        frames_encoded: outcome.finalize.samples,
                        frames_dropped: outcome.frames_dropped_backpressure,
                        stop_reason: outcome.stop_reason.as_str().to_string(),
                    },
                );
            }
            Err(e) => {
                let _ = app.emit(
                    "recording-error",
                    serde_json::json!({ "code": e.code(), "message": e.to_string() }),
                );
            }
        })
        .map_err(|e| format!("创建 finalize 线程失败：{}", e))?;

    Ok(())
}

/// 一个可选中的顶层窗口（坐标已归一化到 0..1，前端不需要做任何 DPI 换算）。
#[derive(Debug, Clone, Serialize)]
pub struct WindowRectDto {
    pub title: String,
    pub nx: f64,
    pub ny: f64,
    pub nw: f64,
    pub nh: f64,
}

/// 列出可吸附的顶层窗口（"点一下选中整个窗口"）。
///
/// 枚举逻辑在媒体层的 `capture::list_top_level_windows`——Win32 代码不放在 Tauri 壳层，
/// 而且那里直接返回归一化坐标，前端不需要做任何 DPI 换算。
#[tauri::command]
pub fn list_windows(state: State<'_, AppState>) -> Result<Vec<WindowRectDto>, String> {
    let ctx = state
        .selector
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "框选会话不存在".to_string())?;

    Ok(
        screenlite_media::capture::list_top_level_windows(ctx.phys_w, ctx.phys_h)
            .into_iter()
            .map(|w| WindowRectDto {
                title: w.title,
                nx: w.nx,
                ny: w.ny,
                nw: w.nw,
                nh: w.nh,
            })
            .collect(),
    )
}

///
/// 窗口定位用逻辑坐标（Tauri 的 position/size 是逻辑单位），
/// 而换算用权威物理尺寸（`GraphicsCaptureItem.Size()`）——
/// 这样不管应用是否 DPI 感知、缩放多少，换算都正确。
#[tauri::command]
pub async fn open_region_selector<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    display_id: String,
) -> Result<(), String> {
    use tauri::{WebviewUrl, WebviewWindowBuilder};

    let resolved = capture::resolve_display(&display_id)
        .map_err(|e| format!("[{}] {}", e.code(), e))?;
    if resolved.info.hdr {
        return Err("目标显示器处于 HDR 模式，V0.1 暂不支持".into());
    }

    // 权威物理尺寸（不启动采集，只建捕获项读尺寸）
    let (phys_w, phys_h) = capture::capture_size_of(&display_id)
        .map_err(|e| format!("[{}] {}", e.code(), e))?;

    let logical_w = resolved.info.width as f64;
    let logical_h = resolved.info.height as f64;

    if let Some(old) = app.get_webview_window("region-overlay") {
        let _ = old.close();
    }

    let overlay = WebviewWindowBuilder::new(
        &app,
        "region-overlay",
        WebviewUrl::App("index.html?overlay=1".into()),
    )
    .title("选择录制区域")
    .position(resolved.info.x as f64, resolved.info.y as f64)
    .inner_size(logical_w, logical_h)
    .decorations(false)
    .transparent(true)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(false)
    .shadow(false)
    .build()
    .map_err(|e| format!("创建框选窗口失败：{}", e))?;

    let _ = overlay.set_focus();

    *state.selector.lock().unwrap() = Some(SelectorContext {
        display_id,
        phys_w,
        phys_h,
        logical_w,
        logical_h,
    });

    tracing::info!(phys_w, phys_h, logical_w, logical_h, "框选覆盖层已打开");
    Ok(())
}

/// 覆盖层确认选区。
///
/// 入参是归一化坐标 0..1（相对显示器），在这里换算为物理像素——
/// 前端不需要知道任何 DPI 细节，这是刻意的设计（避免再次踩逻辑/物理坐标混用的坑）。
#[tauri::command]
pub fn confirm_region<R: tauri::Runtime>(
    app: AppHandle<R>,
    state: State<'_, AppState>,
    nx: f64,
    ny: f64,
    nw: f64,
    nh: f64,
) -> Result<screenlite_media::convert::Region, String> {
    let ctx = state
        .selector
        .lock()
        .unwrap()
        .clone()
        .ok_or_else(|| "框选会话不存在（可能已取消）".to_string())?;

    let clamp01 = |v: f64| v.clamp(0.0, 1.0);
    // 换算逻辑放在媒体层的纯函数里（有单测覆盖），这里只负责取参数与收尾
    let region = screenlite_media::convert::Region::from_normalized(
        clamp01(nx),
        clamp01(ny),
        clamp01(nw),
        clamp01(nh),
        ctx.phys_w,
        ctx.phys_h,
    )
    .ok_or_else(|| "选区过小或完全在画幅外".to_string())?;

    tracing::info!(
        x = region.x,
        y = region.y,
        width = region.width,
        height = region.height,
        "框选完成（已换算为物理像素）"
    );

    if let Some(overlay) = app.get_webview_window("region-overlay") {
        let _ = overlay.close();
    }
    *state.selector.lock().unwrap() = None;

    // 把结果推给主窗口（覆盖层自己不需要持有结果）
    let _ = app.emit("region-selected", &region);
    Ok(region)
}

/// 取消框选：关闭覆盖层。
#[tauri::command]
pub fn cancel_region<R: tauri::Runtime>(app: AppHandle<R>, state: State<'_, AppState>) {
    if let Some(overlay) = app.get_webview_window("region-overlay") {
        let _ = overlay.close();
    }
    *state.selector.lock().unwrap() = None;
}

#[tauri::command]
pub fn get_status(state: State<'_, AppState>) -> Option<screenlite_media::engine::RecordingStatus> {
    state
        .recorder
        .lock()
        .unwrap()
        .as_ref()
        .map(|r| r.status())
}

/// 打开输出目录（V0.1 用资源管理器；原生文件夹选择器是后续小改动）。
#[tauri::command]
pub fn open_output_directory(path: String) -> Result<(), String> {
    std::process::Command::new("explorer")
        .arg(&path)
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("打开目录失败：{}", e))
}

/// 前端就绪握手：所有监听器挂好之后调用一次。
///
/// 返回的是队首命令的非破坏性快照（peek）：
/// - 前端忽略它——命令只有一条分发路径（`take_ui_command`），
/// 这样不会出现"两份分发逻辑各自执行一次"。
/// 也正因为前端不看返回值，这里绝不能改成 take：那会把命令读走而没人执行（见 `peek_ui_command`）。
/// - 要返回值是为了可测性/诊断：集成测试断言"就绪前入队的命令能被这次握手看到"，
/// 前端自检探针：把 DOM 的实际挂载状态 报进日志。
///
/// 为什么需要：在这台机器上，"窗口里到底显示没显示表单"的像素层证据全部不可靠——
/// PrintWindow 对 WebView2 抓出来是黑的、WGC 会被 topmost 窗口遮挡、亮度读数互相矛盾。
/// 唯一可信的仪器是前端自己把 DOM 状态报上来（走 IPC → 日志）：
/// `root_children == 0` = JS 在跑但 React 没渲染出内容；
/// `>0` 且 body 文本足够长 = 表单真的在 DOM 里。
#[derive(Debug, serde::Deserialize)]
pub struct UiProbe {
    pub root_children: i32,
    pub root_exists: bool,
    pub body_text_len: i32,
    pub ready_state: String,
    pub url: String,
    pub head: String,
    /// 关键控件的屏幕坐标（由前端从 getBoundingClientRect 换算）。
    /// 用途：注入类自动化无法可靠截取 WebView2 内容（PrintWindow 是黑的、WGC 被遮挡），
    /// 于是"按钮在哪"只能由前端自己报——DOM 几何 + 窗口位置换算，像素无关。
    #[serde(default)]
    pub buttons: serde_json::Value,
    /// 布局几何：滑块/行的宽度与是否超出视口、文档是否需要滚动。
    /// 用途：布局类缺陷（撑破容器、被挤成一列字、控件超出窗口被裁）在日志与像素里都"看不出来"，
    /// 只有几何数字能一眼判定（`overflow > 0` 就是被裁掉了）。
    #[serde(default)]
    pub layout: serde_json::Value,
}

#[tauri::command]
pub fn ui_probe(_state: State<'_, AppState>, probe: UiProbe) -> Result<(), String> {
    tracing::info!(
        root_children = probe.root_children,
        root_exists = probe.root_exists,
        body_text_len = probe.body_text_len,
        ready_state = %probe.ready_state,
        url = %probe.url,
        head = %probe.head,
        buttons = ?probe.buttons,
        layout = ?probe.layout,
        "UI 自检探针（DOM 实际状态）"
    );
    Ok(())
}

/// 日志里也能直接读出"前端就绪时手里还有没有活"。
#[tauri::command]
pub fn ui_ready(state: State<'_, AppState>) -> Option<UiCommand> {
    let first = state.mark_ui_ready();
    let pending = state.peek_ui_command();
    tracing::info!(首次 = first, 队列中待执行 = ?pending, "前端已就绪（监听器注册完成）");
    pending
}

/// 原子取走一条待执行命令。没有命令时返回 `null`。
#[tauri::command]
pub fn take_ui_command(state: State<'_, AppState>) -> Option<UiCommand> {
    let cmd = state.take_ui_command();
    if let Some(c) = cmd {
        tracing::info!(命令 = ?c, "前端取走命令");
    }
    cmd
}

/// 前端确认"已停完并最终化"。托盘据此才退出进程。
#[tauri::command]
pub fn quit_done(state: State<'_, AppState>) {
    tracing::info!("前端已确认停止完成，通知退出流程");
    state.quit_gate.signal();
}

// ---------- 设置 ----------

/// `set_settings` 的结果：保存与"快捷键是否生效"分开报告。
///
/// 为什么分开：快捷键可能注册失败（格式错 / 被别的程序占用），
/// 但音频设置该照样保存 —— 否则用户改音频会跟着一起丢。
#[derive(Debug, Serialize)]
pub struct SettingsApplied {
    pub saved: bool,
    /// 非空表示快捷键没生效（保存仍然成功）
    pub hotkey_error: Option<String>,
}

#[tauri::command]
pub fn get_settings() -> crate::settings::AppSettings {
    crate::settings::load()
}

/// 保存设置，并立即应用全局快捷键（注册/改键/停靠都由这里收口）。
#[tauri::command]
pub fn set_settings<R: tauri::Runtime>(
    app: AppHandle<R>,
    settings: crate::settings::AppSettings,
) -> Result<SettingsApplied, String> {
    let s = settings.normalized();
    crate::settings::save(&s)?;
    let hotkey_error = apply_hotkey(&app, s.hotkey.as_deref()).err();
    // 捕获可见性随设置立即生效（用户打开"允许截屏"后不需要重启）
    apply_capture_affinity(&app, s.capture_visible);
    tracing::info!(
        音频源数 = s.audio_sources.len(),
        快捷键 = s.hotkey.as_deref().unwrap_or("(停用)"),
        hotkey_error = hotkey_error.as_deref().unwrap_or("无"),
        "设置已保存"
    );
    Ok(SettingsApplied {
        saved: true,
        hotkey_error,
    })
}

/// 应用全局快捷键：先全部注销，再按需注册（改键与停用都走这一句）。
///
/// 启动时与 `set_settings` 都调它，保证"配置是唯一真相"。
pub fn apply_hotkey<R: tauri::Runtime>(
    app: &AppHandle<R>,
    hotkey: Option<&str>,
) -> Result<(), String> {
    use tauri_plugin_global_shortcut::GlobalShortcutExt;

    // 测试环境（MockRuntime + 未装插件）没有这个 state —— 直接跳过，别 panic
    if app
        .try_state::<tauri_plugin_global_shortcut::GlobalShortcut<R>>()
        .is_none()
    {
        tracing::debug!("未安装全局快捷键插件（测试环境），跳过应用快捷键");
        return Ok(());
    }

    let gs = app.global_shortcut();
    gs.unregister_all()
        .map_err(|e| format!("注销旧快捷键失败：{}", e))?;

    // Dock 显隐热键：独立于录制热键，恒定 Alt+Shift+S（将来可进设置）。
    // 放在 unregister_all 之后注册——每次应用设置都会先全清再重建，所以它必须在这里被重建。
    if let Err(e) = gs.register("alt+shift+s") {
        tracing::warn!(error = %e, "注册 Dock 显隐热键失败（Alt+Shift+S，可能被占用）");
    }

    let Some(h) = hotkey.map(str::trim).filter(|h| !h.is_empty()) else {
        tracing::info!("全局快捷键已停用（用户设置里关掉了）");
        return Ok(());
    };
    gs.register(h).map_err(|e| {
        tracing::warn!(hotkey = h, error = %e, "注册全局快捷键失败（可能已被其它程序占用）");
        format!("「{}」注册失败：{}（可能已被其它程序占用）", h, e)
    })?;
    tracing::info!(hotkey = h, "全局快捷键已注册");
    Ok(())
}

/// 应用"控制窗口是否出现在截图/录制里"。
///
/// 默认排除（`WDA_EXCLUDEFROMCAPTURE`）：窗口对用户照常可见，但对捕获 API
/// （含 WGC、PrintWindow、系统截图）不存在 —— 否则每一段录像里都有控制界面。
///
/// 但排除会让用户自己截图时窗口"消失" （实测反馈），所以留了设置开关
/// `capture_visible`：需要截图/演示时打开，恢复成普通窗口。
///
/// 副作用（现成的检查点）：排除生效时，用 PrintWindow 抓它会得到空白/黑。
pub fn apply_capture_affinity<R: tauri::Runtime>(app: &AppHandle<R>, visible: bool) {
    #[cfg(windows)]
    {
        use windows::Win32::UI::WindowsAndMessaging::{
            SetWindowDisplayAffinity, WDA_EXCLUDEFROMCAPTURE, WDA_NONE,
        };
        let Some(w) = app.get_webview_window("main") else {
            return;
        };
        let Ok(hwnd) = w.hwnd() else {
            tracing::warn!("取窗口句柄失败，无法设置捕获可见性（录像里会出现控制界面）");
            return;
        };
        let affinity = if visible {
            WDA_NONE
        } else {
            WDA_EXCLUDEFROMCAPTURE
        };
        match unsafe { SetWindowDisplayAffinity(hwnd, affinity) } {
            Ok(()) => tracing::info!(
                capture_visible = visible,
                "控制窗口捕获可见性已应用（false = 录像/截图里不会出现它；需要截图请到设置里打开）"
            ),
            Err(e) => tracing::warn!(
                error = %e, capture_visible = visible,
                "设置捕获可见性失败（需要 Win10 2004+）"
            ),
        }
    }
    #[cfg(not(windows))]
    {
        let _ = (app, visible);
    }
}

/// 窗口比客户区大出多少才算"tao的阴影沟槽回来了"。
///
/// 抽成纯函数是为了让检查点本身可回归（只能靠肉眼看的检查点等于没有检查点）：
/// 返回 `Some((多出宽, 多出高))` = 判定为沟槽，`None` = 正常。
/// 阈值 4px：1~2px 的差可能只是舍入，不该报警。
#[cfg(windows)]
pub(crate) fn shadow_gutter(window: (i32, i32), client: (i32, i32)) -> Option<(i32, i32)> {
    let (dw, dh) = (window.0 - client.0, window.1 - client.1);
    (dw.max(dh) >= 4).then_some((dw, dh))
}

/// 让悬浮岛的窗口不再被系统画外框。
///
/// 实测：卡片外围有一圈浅色细边框 + 大片阴影，看起来像"半透明大玻璃板"。
/// 验证结论（不是猜）：
/// - 不是 CSS 泄漏：`html, body { background: transparent }`、`.dock` 只有布局属性，
/// 外层没有任何 background / border / box-shadow / backdrop-filter。
/// - 是系统非客户区在画：窗口仍带 `WS_CAPTION`，
/// 系统就按浅色主题给它画了一圈边框与阴影 —— 页面是深色、边框却是浅的。
///
/// 补记（"框"的真身，终于钉死）：上面两条只是背景，真正的来源是
/// tao 给"无边框窗口"预留的阴影沟槽 ——
/// tao 0.35.3 `window.rs:1238` 在 `shadow`（默认 true）下把窗口加大
/// `calculate_insets_for_dpi` 的量（实测 L11 T2 R11 B11 @150%），
/// 再于 `:1409` 的 `WM_NCCALCSIZE` 里把客户区按同样的量抠掉，把这一圈留给 DWM 画阴影。
/// DWM 侧被我们 DISABLED 掉之后，这一圈就由传统 GDI 非客户区刷上：
/// 外侧 1px 黑 + 1px 白（经典 3D 抬起边）+ 9px 实心浅蓝 `(186,209,234)` ——
/// 正是的"一圈明显的浅色细边框 + 巨大的半透明矩形外框"。
/// 证据链：窗口矩形 562x553 vs 客户区 540x540（差 22x13）、webview 子窗口正好内缩 (11,2)、
/// `WindowFromPoint` 打在边框上返回父窗口、`PrintWindow` 位图里那圈颜色也在；
/// 而 `SetWindowRgn` 裁到客户区后边框立即消失 （见下方 `spawn_frame_guard` 的兜底）。
///
/// 处置：源头在 `tauri.conf.json` 里 `"shadow": false`（tao 便不再加大窗口、不再抠客户区）；
/// 本函数再用 DWM 精准关掉"非客户区渲染 + 边框 + 系统圆角"作双保险，
/// 绝不改样式表（拿 `!important` 盖整份 CSS 会摧毁刚建立的令牌层）。
#[cfg(windows)]
pub fn make_window_frame_invisible<R: tauri::Runtime>(window: &tauri::WebviewWindow<R>) {
    use windows::Win32::Graphics::Dwm::{
        DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMNCRP_DISABLED, DWMWCP_DONOTROUND,
    };

    let Ok(hwnd) = window.hwnd() else {
        tracing::warn!("取窗口句柄失败，无法关闭系统外框（卡片外会留下一圈浅色边框）");
        return;
    };
    let set = |attr: windows::Win32::Graphics::Dwm::DWMWINDOWATTRIBUTE, val: u32, name: &str| {
        let r = unsafe {
            DwmSetWindowAttribute(
                hwnd,
                attr,
                &val as *const u32 as *const core::ffi::c_void,
                std::mem::size_of::<u32>() as u32,
            )
        };
        match r {
            Ok(()) => tracing::info!(属性 = name, "已关闭系统外框渲染"),
            Err(e) => tracing::warn!(error = %e, 属性 = name, "关闭系统外框失败（需要 Win11）"),
        }
    };
    // ① 停掉整个非客户区渲染（边框与阴影都由此而来）
    set(
        DWMWA_NCRENDERING_POLICY,
        DWMNCRP_DISABLED.0 as u32,
        "NCRENDERING_POLICY=DISABLED",
    );
    // ② 去掉 Win11 的 1px 强调色边框（DWMWA_COLOR_NONE = 0xFFFFFFFE）
    set(DWMWA_BORDER_COLOR, 0xFFFF_FFFE, "BORDER_COLOR=NONE");
    // ③ 不要系统圆角（我们自己有圆角，系统那圈会露出来）
    set(
        DWMWA_WINDOW_CORNER_PREFERENCE,
        DWMWCP_DONOTROUND.0 as u32,
        "CORNER_PREFERENCE=DONOTROUND",
    );
}

/// 无边框样式的清除掩码：
/// 标题栏三件套（CAPTION/THICKFRAME/SYSMENU）+ 两个系统按钮位（MIN/MAXIMIZEBOX）。
/// WS_MIN/MAXIMIZEBOX 在无标题栏时不绘制，但检查截图证明"样式位声明"必须整体干净。
#[cfg(windows)]
const FRAME_CLEAR_MASK: i32 = windows::Win32::UI::WindowsAndMessaging::WS_CAPTION.0 as i32
    | windows::Win32::UI::WindowsAndMessaging::WS_THICKFRAME.0 as i32
    | windows::Win32::UI::WindowsAndMessaging::WS_SYSMENU.0 as i32
    | windows::Win32::UI::WindowsAndMessaging::WS_MINIMIZEBOX.0 as i32
    | windows::Win32::UI::WindowsAndMessaging::WS_MAXIMIZEBOX.0 as i32;

/// 把指定 HWND 校正为无边框样式（Set → Get → Verify；仅在实际发生校正时重画非客户区）。
///
/// FRAMECHANGED的使用要求：
/// - 只在"样式确实发生校正"这一拍调用——它是校正动作的收尾，不是周期手段；
/// - 为什么校正后必须跟它：样式位只是账本，屏幕上已画出的标题栏是画面——
/// 不触发非客户区重建，系统不会把已画出的标题栏擦掉（本次"样式已清、
/// 截图里标题栏还在"的双真相即由此而来）；
/// - 它是我们直接调的 Win32，不触发 tao 的样式重放（tao 只在它自己的
/// setSize/show/hide 路径里重放）。
///
/// 返回 true = 校正后样式干净（Get 复核通过）。
#[cfg(windows)]
pub fn enforce_frameless_style(hwnd: windows::Win32::Foundation::HWND) -> bool {
    use windows::Win32::Foundation::HWND;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongW, SetWindowLongW, SetWindowPos, GWL_STYLE, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WS_POPUP,
    };
    unsafe {
        let cur = GetWindowLongW(hwnd, GWL_STYLE);
        let want = (cur & !FRAME_CLEAR_MASK) | WS_POPUP.0 as i32;
        if want == cur {
            return true; // 已干净：什么都不做（尤其不碰 FRAMECHANGED）
        }
        SetWindowLongW(hwnd, GWL_STYLE, want);
        // 同一拍立刻触发非客户区重建——画面与账本同时收口
        let _ = SetWindowPos(
            hwnd,
            Some(HWND::default()),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
        let back = GetWindowLongW(hwnd, GWL_STYLE);
        (back & FRAME_CLEAR_MASK) == 0
    }
}

/// 无边框子类的原始窗口过程（装子类时保存）。
///
/// 只有一个主窗口，单值足够；子类的地址同时充当"这个 HWND 装过没有"的检查点，
/// 所以 `frameless_wndproc_addr()` 单独暴露出来给护栏的自愈逻辑用。
#[cfg(windows)]
static ORIG_WNDPROC: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);

/// 拦住过多少次含边框位的样式写回（边沿触发打日志，不刷屏）。
///
/// 为什么要有这个计数器：子类装没装上、拦没拦，日志里必须有可观测的证据。
/// 否则"标题栏不再回来"仍然只是一句无法核实的断言（本次被这种不可核实的断言
/// 反复折磨过）。每次 Alt+Shift+S / 托盘显窗都会累加，一眼能看出它在工作。
#[cfg(windows)]
static BLOCKED_STYLE_WRITES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 子类窗口过程的地址（用于判断某个 HWND 上装的是不是我们）。
#[cfg(windows)]
pub(crate) fn frameless_wndproc_addr() -> isize {
    frameless_wndproc as *const () as isize
}

/// 样式写入门：进这里的一切样式，先剥边框位、强制 `WS_POPUP`，再放行。
///
/// 只做"改 STYLESTRUCT 然后转发"——不吞消息、不抢焦点，语义最小。
#[cfg(windows)]
unsafe extern "system" fn frameless_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    use std::sync::atomic::Ordering;
    use windows::Win32::UI::WindowsAndMessaging::{
        CallWindowProcW, DefWindowProcW, GWL_STYLE, STYLESTRUCT, WM_STYLECHANGING, WS_POPUP,
        WNDPROC,
    };

    if msg == WM_STYLECHANGING && wparam.0 as i32 == GWL_STYLE.0 {
        let p = lparam.0 as *mut STYLESTRUCT;
        if !p.is_null() {
            let incoming: u32 = (*p).styleNew;
            let filtered = (incoming & !(FRAME_CLEAR_MASK as u32)) | WS_POPUP.0;
            if filtered != incoming {
                (*p).styleNew = filtered;
                let n = BLOCKED_STYLE_WRITES.fetch_add(1, Ordering::SeqCst) + 1;
                tracing::warn!(
                    拦截次数 = n,
                    原始样式 = format!("0x{incoming:08X}"),
                    过滤后 = format!("0x{filtered:08X}"),
                    "无边框子类：拦下一次含 WS_CAPTION 的样式写回（tao apply_diff）✓"
                );
            }
        }
    }
    let orig = ORIG_WNDPROC.load(Ordering::SeqCst);
    if orig == 0 {
        DefWindowProcW(hwnd, msg, wparam, lparam)
    } else {
        // WNDPROC 是指针大小的 Option<fn>，与 isize 可以无损互转
        let prev: WNDPROC = std::mem::transmute::<isize, WNDPROC>(orig);
        CallWindowProcW(prev, hwnd, msg, wparam, lparam)
    }
}

/// 永久性无边框：把"标题栏不可能被写回来"焊死在窗口过程里。
///
/// ## 根因（本次从 tao 0.35.3 源码钉死，不再靠猜）
///
/// `WindowFlags::to_window_styles()`（`window_state.rs:244`）无条件
/// `style |= WS_CAPTION | WS_CLIPSIBLINGS | WS_SYSMENU` ——
/// `decorations: false` 只影响 `to_adjusted_window_styles()`，而后者仅供
/// `AdjustWindowRectEx` 计算尺寸用，不参与 `SetWindowLong`。
///
/// 于是 `WindowFlags::apply_diff()`（`window_state.rs:425-461`）在任何一个
/// flag 发生变化时（`VISIBLE` / `MINIMIZED` / `FOCUSABLE` / `ALWAYS_ON_TOP`…）：
///
/// ```text
/// SetWindowLongW(GWL_STYLE, 含 WS_CAPTION 的整份样式)
/// SetWindowPos(..., SWP_FRAMECHANGED) ← 立刻重画非客户区
/// ```
///
/// 在我们的代码里命中这条路径的操作（全部是 Rust 侧，不是前端）：
///
/// | 操作 | 入口 | 后果 |
/// |---|---|---|
/// | `hide()` | Alt+Shift+S 显隐热键 | 隐藏窗口被写入 WS_CAPTION |
/// | `show()` | Alt+Shift+S 显隐热键、托盘"显示主窗口"、托盘左键 | 窗口先变可见再写样式 ⇒ 标题栏真的画出来 |
/// | `unminimize()` | 同上 | 同上 |
///
/// 连创建都逃不掉：`CreateWindowExW`（`window.rs:1251`）直接用
/// `to_window_styles()` 当参数 ⇒ 窗口出生即带 `WS_CAPTION`
/// （实测启动日志 `style_before=0x14CB0000`，正是这份样式的逐位展开）。
/// 这也是"启动瞬间闪一下标题栏"的来源（此前靠首个 Focused 事件擦掉，
/// 约 300ms 的可见期）。
///
/// ## 处置：从"事后擦"改成"事前挡"
///
/// 事后擦（本文件原有的 `enforce_frameless_style` + 750ms 护栏）永远有窗口期：
/// `apply_diff` 是 `Set → SetWindowPos(FRAMECHANGED)` 同一拍完成的，
/// 而窗口此刻已经可见 ⇒ 标题栏会真实出现在画面上。
///
/// 这里换成在 `WM_STYLECHANGING`（样式生效前的最后一刻）把边框位剥掉、
/// 并强制 `WS_POPUP`，再放行走 tao 的原始窗口过程。无论上游怎么改 flag，
/// 标题栏都不可能被写进 `GWL_STYLE`——这才是"NEVER RETURNS"。
///
/// 幂等：已装过就直接返回；窗口句柄被重建后由 `spawn_frame_guard` 发现并重装。
#[cfg(windows)]
pub fn install_frameless_subclass(hwnd: windows::Win32::Foundation::HWND) {
    use std::sync::atomic::Ordering;
    use windows::Win32::UI::WindowsAndMessaging::{
        GetWindowLongPtrW, GWLP_WNDPROC, SetWindowLongPtrW,
    };

    let ours = frameless_wndproc_addr();
    let cur = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
    if cur == 0 {
        tracing::warn!("读主窗口 WNDPROC 失败：无法安装无边框子类（显隐热键后标题栏可能回归）");
        return;
    }
    if cur == ours {
        return; // 幂等：装过了
    }
    let prev = unsafe { SetWindowLongPtrW(hwnd, GWLP_WNDPROC, ours) };
    if prev == 0 {
        tracing::warn!("安装无边框子类失败：标题栏可能在 show/hide/unminimize 后回归");
        return;
    }
    ORIG_WNDPROC.store(prev, Ordering::SeqCst);
    tracing::info!(
        orig_wndproc = format!("0x{:016X}", prev),
        "已安装无边框样式子类（WM_STYLECHANGING 拦截）：此后原生标题栏无法被写回 GWL_STYLE"
    );
}

/// 主窗口"显形"的唯一入口（Alt+Shift+S 显隐热键、托盘"显示主窗口"、
/// 托盘图标左键共用）。
///
/// 这三个入口原先各自 `show + unminimize + set_focus`，而它们是 tao 唯一
/// 会无条件重写 `GWL_STYLE`（含 `WS_CAPTION` + `SWP_FRAMECHANGED`）的路径，
/// 上面这三个正是原先漏掉的地方。收口到一处之后：
/// ① 子类把写回路径挡死；② 这里再补一次即拍核验作双保险（也覆盖子类
/// 因句柄重建而失效的退化场景）。
#[cfg(windows)]
pub fn reveal_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
        // show/unminimize 在 tao 里是 execute_in_thread（同步等事件循环跑完）
        // ⇒ 返回时样式已被 apply_diff 重写过，这里核验正是时机。
        verify_native_frame_after_geometry(app, "show/unminimize/set_focus");
    }
}

/// 主窗口"隐形"的唯一入口（与 [`reveal_main_window`] 对称，便于对照排查）。
#[cfg(windows)]
pub fn conceal_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

#[cfg(not(windows))]
pub fn reveal_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.show();
        let _ = w.unminimize();
        let _ = w.set_focus();
    }
}

#[cfg(not(windows))]
pub fn conceal_main_window<R: tauri::Runtime>(app: &tauri::AppHandle<R>) {
    if let Some(w) = app.get_webview_window("main") {
        let _ = w.hide();
    }
}

/// 焦点生命周期上的样式核验。
///
/// 为什么必须有：实测——启动时无边框，但切走焦点后原生标题栏出现。
/// 说明焦点迁移是样式回归/重画的触发点之一。本函数在每次焦点事件后核验样式：
/// 干净 → DEBUG 记轨迹；回归 → 即拍校正。
/// 绝不调用 set_focus / SetForegroundWindow（不抢前台，输入要求）。
#[cfg(windows)]
pub fn verify_native_frame_on_focus<R: tauri::Runtime>(app: &tauri::AppHandle<R>, focused: bool) {
    verify_main_frame(app, &format!("焦点切换(focused={focused})"), Some(focused));
}

/// 几何突变后的样式核验（契约：setSize/setPosition/show/hide/unminimize 之后
/// 立即 enforce，而不是等护栏的下一个 750ms 拍——那 750ms 里框会闪现）。
/// 由 `on_window_event` 的 `Resized` 等事件驱动。
#[cfg(windows)]
pub fn verify_native_frame_after_geometry<R: tauri::Runtime>(app: &tauri::AppHandle<R>, cause: &str) {
    verify_main_frame(app, cause, None);
}

/// 主窗口样式核验的共享核心：读样式 → 脏则即拍校正 → 干净记 DEBUG 轨迹。
#[cfg(windows)]
fn verify_main_frame<R: tauri::Runtime>(app: &tauri::AppHandle<R>, cause: &str, focused: Option<bool>) {
    use windows::Win32::UI::WindowsAndMessaging::{GetWindowLongW, GWL_STYLE};
    let Some(w) = app.get_webview_window("main") else {
        return;
    };
    let Ok(hwnd) = w.hwnd() else {
        return;
    };
    let cur = unsafe { GetWindowLongW(hwnd, GWL_STYLE) };
    if (cur & FRAME_CLEAR_MASK) != 0 {
        let ok = enforce_frameless_style(hwnd);
        tracing::info!(
            触发 = cause,
            focused = focused.unwrap_or_default(),
            style_before = format!("0x{:08X}", cur),
            校正成功 = ok,
            "窗口样式回归：已即拍校正"
        );
    } else {
        tracing::debug!(触发 = cause, style = format!("0x{:08X}", cur), "窗口样式核验干净");
    }
}

#[cfg(not(windows))]
pub fn verify_native_frame_on_focus<R: tauri::Runtime>(_app: &tauri::AppHandle<R>, _focused: bool) {}

#[cfg(not(windows))]
pub fn verify_native_frame_after_geometry<R: tauri::Runtime>(_app: &tauri::AppHandle<R>, _cause: &str) {}

/// 外框护栏：周期性把"不要外框"这件事重新压回去。
///
/// 为什么需要它（这是本次定位到的真机制）：
/// - 前端挂载时会调 `setSize`（尺寸联动），热键显隐会调 `show`/`hide` —— 这些都在我们自己的代码里；
/// - 但 tao 在这些操作里会重新做一次窗口样式应用 ⇒ 我写进去的 DWM 属性被顶掉
/// ⇒ 现象正是"刚启动干净、一两秒后框又出现"；
/// - 而"升级 tao"这条路验证过不存在（`cargo update -p tao` → `Locking 0 packages`）。
///
/// 所以改成驻留护栏：每 750ms 幂等地把三项重压一次。
/// 它不依赖前端代码（前端还会被继续改），也不依赖上游修复；
/// 成本是每秒 3 次 DWM 调用（可忽略），且只在状态发生变化时打日志（不刷屏）。
#[cfg(windows)]
pub fn spawn_frame_guard<R: tauri::Runtime>(app: tauri::AppHandle<R>) {
    std::thread::Builder::new()
        .name("screenlite-frame-guard".into())
        .spawn(move || {
            use windows::Win32::Graphics::Dwm::{
                DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_NCRENDERING_POLICY,
                DWMWA_WINDOW_CORNER_PREFERENCE, DWMNCRP_DISABLED, DWMWCP_DONOTROUND,
            };
            let mut reapplied = 0u64;
            // 阴影沟槽告警：边沿触发（只在状态跳变时打日志，不刷屏）
            let mut gutter_warned = false;
            loop {
                std::thread::sleep(std::time::Duration::from_millis(750));
                let Some(w) = app.get_webview_window("main") else {
                    continue;
                };
                let Ok(hwnd) = w.hwnd() else { continue };
                let put = |attr, val: u32| unsafe {
                    DwmSetWindowAttribute(
                        hwnd,
                        attr,
                        &val as *const u32 as *const core::ffi::c_void,
                        std::mem::size_of::<u32>() as u32,
                    )
                    .is_ok()
                };
                // 幂等重压：被顶掉就修回来，没被顶掉就是无操作
                let ok = put(DWMWA_NCRENDERING_POLICY, DWMNCRP_DISABLED.0 as u32)
                    && put(DWMWA_BORDER_COLOR, 0xFFFF_FFFE)
                    && put(
                        DWMWA_WINDOW_CORNER_PREFERENCE,
                        DWMWCP_DONOTROUND.0 as u32,
                    );
                // 样式位也在这里维持：校正逻辑抽到 `enforce_frameless_style`
                // （Set → Get → Verify；FRAMECHANGED 仅随真实校正触发，见该函数注释）——
                // 护栏线程与焦点事件钩子共用同一份实现，避免两处漂移。
                //
                // 装了子类之后这里本该永远是 no-op；它保留下来是为了"自检"：
                // 万一哪天主窗口句柄被重建（子类只装在那一个 HWND 上），
                // 这里负责发现并把子类重装回去——否则新窗口会重新裸奔。
                let style_ok = enforce_frameless_style(hwnd);
                let _ = style_ok;
                // 子类自愈：只有检出错装的窗口才动。检查点是"当前 WNDPROC 不是我们的"
                // ⇒ 说明 HWND 变了，需要重装（install_frameless_subclass 自身幂等）。
                #[cfg(windows)]
                {
                    use windows::Win32::UI::WindowsAndMessaging::{
                        GetWindowLongPtrW, GWLP_WNDPROC,
                    };
                    let cur_proc = unsafe { GetWindowLongPtrW(hwnd, GWLP_WNDPROC) };
                    if cur_proc != 0 && cur_proc != frameless_wndproc_addr() {
                        tracing::warn!(
                            current_wndproc = format!("0x{:016X}", cur_proc),
                            "外框护栏：主窗口流程不是我们的无边框子类（句柄可能被重建），重装"
                        );
                        install_frameless_subclass(hwnd);
                        let _ = enforce_frameless_style(hwnd);
                    }
                }
                // 类级阴影：`CS_DROPSHADOW` 是窗口类的属性，不是窗口的 ——
                // 它会让系统给无边框窗口画一圈"类级阴影"（改 GWL_STYLE / DWM 都碰不到它）。
                // 这是最后一种还没试过的系统外框来源，一并清掉。
                unsafe {
                    use windows::Win32::UI::WindowsAndMessaging::{
                        GetClassLongPtrW, SetClassLongPtrW, GCL_STYLE,
                    };
                    const CS_DROPSHADOW: isize = 0x0002_0000;
                    let cur_cls = GetClassLongPtrW(hwnd, GCL_STYLE) as isize;
                    if cur_cls & CS_DROPSHADOW != 0 {
                        SetClassLongPtrW(hwnd, GCL_STYLE, (cur_cls & !CS_DROPSHADOW) as _);
                        tracing::info!(
                            before = format!("0x{:08X}", cur_cls),
                            "外框护栏：已清除窗口类样式 CS_DROPSHADOW（系统类级阴影）✓"
                        );
                    }
                }
                // 阴影沟槽护栏：把"框又回来了"变成有名字的告警。
                // 真身见 `make_window_frame_invisible` 的 补记：tao 在 `shadow: true`（默认）时
                // 把窗口加大、并在 `WM_NCCALCSIZE` 里把客户区抠掉一圈留给 DWM 画阴影
                // （实测 @150% 为 L11 T2 R11 B11）；DWM 侧被我们 DISABLED 后，那圈改由传统 GDI
                // 非客户区刷成"1px 黑 + 1px 白 + 9px 实心浅蓝" —— 就是的"外框"。
                // 源头已用 `tauri.conf.json` 的 `"shadow": false` 关掉；
                // 这条只负责发现它无声回归（这个症状反复折磨过多轮，静默复发一次就够受的了）。
                // 检查点：客户区必须等于窗口矩形；不等就是沟槽回来了。
                // 注：只比总量（窗口 − 客户区），所以不需要 ClientToScreen（它在 Gdi 里，
                // 会多开一个 cargo feature）；总量不等就已经足够判定。
                unsafe {
                    use windows::Win32::Foundation::RECT;
                    use windows::Win32::UI::WindowsAndMessaging::{GetClientRect, GetWindowRect};
                    let (mut wr, mut cr) = (RECT::default(), RECT::default());
                    if GetWindowRect(hwnd, &mut wr).is_ok() && GetClientRect(hwnd, &mut cr).is_ok() {
                        let win = (wr.right - wr.left, wr.bottom - wr.top);
                        let cli = (cr.right, cr.bottom);
                        match shadow_gutter(win, cli) {
                            Some((dw, dh)) => {
                                if !gutter_warned {
                                    gutter_warned = true;
                                    tracing::warn!(
                                        多出宽 = dw,
                                        多出高 = dh,
                                        客户区 = format!("{}x{}", cli.0, cli.1),
                                        窗口 = format!("{}x{}", win.0, win.1),
                                        "外框护栏：窗口又比客户区大了 —— tao 的无边框阴影沟槽回来了，\
                                         卡片外会出现一圈浅色边框 ✗；请确认 tauri.conf.json 里仍是 \
                                         \"shadow\": false（兜底裁切：SetWindowRgn 到客户区）"
                                    );
                                }
                            }
                            None => {
                                if gutter_warned {
                                    gutter_warned = false;
                                    tracing::info!("外框护栏：已恢复为「客户区 == 窗口矩形」，无阴影沟槽 ✓");
                                }
                            }
                        }
                    }
                }
                reapplied += 1;
                if reapplied == 1 || reapplied % 400 == 0 {
                    tracing::debug!(次数 = reapplied, 成功 = ok, "外框护栏：周期性重压 DWM 三项");
                }
            }
        })
        .map(|_| ())
        .unwrap_or_else(|e| tracing::warn!(error = %e, "启动外框护栏线程失败"))
}

#[cfg(not(windows))]
pub fn spawn_frame_guard<R: tauri::Runtime>(_app: tauri::AppHandle<R>) {}

#[cfg(not(windows))]
pub fn make_window_frame_invisible<R: tauri::Runtime>(_window: &tauri::WebviewWindow<R>) {}

/// 4Hz 进度轮询 + 状态跳变事件。
///
/// 常驻线程：没有活跃录制时什么都不发（轮询成本可忽略）。
/// 节流到 4Hz 是技术基线 要求，避免事件风暴卡住前端。
pub fn spawn_progress_poller(app: AppHandle, state: Arc<Mutex<Option<Recorder>>>) {
    std::thread::Builder::new()
        .name("screenlite-progress".into())
        .spawn(move || {
            let mut last_state: Option<EngineState> = None;
            let mut last_error: Option<Option<String>> = None;
            // 命令积压告警的节流（见循环体开头的说明）
            let mut last_pending_warn: Option<Instant> = None;
            // 启动就绪看门狗（见循环体开头的说明）
            let t_boot = Instant::now();
            let mut not_ready_warned = false;
            loop {
                std::thread::sleep(Duration::from_millis(250));

                // 启动就绪看门狗：启动 10 秒还没收到 `ui_ready`，说明**这一份页面的
                // 加载/执行失败了**（现象就是白屏或黑屏，但窗口在、进程在）。
                //
                // 为什么值得单独一条：这是"白屏/黑屏"唯一不依赖截图的检查点。
                // 这台机器上像素层读数互相矛盾（WGC / PrintWindow 各说各话），
                // 而"前端有没有报到"是前端自己通过 IPC 说的，不受遮挡与合成路径影响。
                // 有了它，用户只要报"我这次是白屏"，日志立刻能对上是哪一类失败。
                let app_state = app.state::<AppState>();
                if app_state.ui_ready() {
                    if not_ready_warned {
                        tracing::info!(
                            delay_ms = t_boot.elapsed().as_millis() as u64,
                            "前端最终就绪了（比 10 秒检查点晚）——说明这次只是**慢**，不是没加载"
                        );
                        not_ready_warned = false;
                    }
                } else if !not_ready_warned && t_boot.elapsed() >= Duration::from_secs(10) {
                    tracing::warn!(
                        "启动 10 秒仍未收到前端就绪——这一份页面很可能没加载成功\
                         （窗口会表现为白屏/黑屏）。处置：先重启 App；\
                         **若反复如此，请确认用的是生产构建**：npm run tauri build\
                         （`cargo build --release` 的产物不带内嵌前端，会去连 localhost:1420，\
                         窗口显示 ERR_CONNECTION_REFUSED —— 这个坑已经已出现三次，）"
                    );
                    not_ready_warned = true;
                }

                // 命令积压告警：命令入队后前端迟迟不取，说明前端已经不再执行了。
                //
                // 最典型的原因：WebView2 的 browser 进程崩溃——窗口变黑、页面死掉，
                // 但我们的进程还活着，于是现象只有"热键响了没反应"。
                // 没有这行，这个故障只能靠翻 Crashpad 的 dump 才查得出来
                //
                //
                // 这段必须在下面那个 `continue` 之前：空闲（没有活跃录制）恰恰是
                // 热键最常被按下的时候，放到 continue 之后等于这个告警永远不会响。
                if let Some(cmd) = app_state.peek_ui_command() {
                    let now = Instant::now();
                    let due = last_pending_warn
                        .map(|t| now.duration_since(t) >= Duration::from_secs(5))
                        .unwrap_or(true);
                    if due {
                        tracing::warn!(
                            命令 = ?cmd,
                            "命令已入队但前端 5 秒没取走——前端可能已停止执行\
                             （先看 Crashpad\\reports 有没有新 dump，再看窗口是否已变黑）"
                        );
                        last_pending_warn = Some(now);
                    }
                } else {
                    last_pending_warn = None;
                }

                let status = {
                    let guard = state.lock().unwrap();
                    guard.as_ref().map(|r| r.status())
                };
                let Some(st) = status else {
                    last_state = None;
                    last_error = None;
                    continue;
                };

                // 状态跳变才发 state-changed
                let err_key = st.error_message.clone();
                if last_state != Some(st.state) || last_error.as_ref() != Some(&err_key) {
                    let _ = app.emit(
                        "recording-state-changed",
                        StatePayload {
                            state: st.state,
                            error_code: st.error_code.map(|s| s.to_string()),
                            error_message: st.error_message.clone(),
                        },
                    );
                    last_state = Some(st.state);
                    last_error = Some(err_key);
                }

                // 进度节流 4Hz
                let _ = app.emit(
                    "recording-progress",
                    ProgressPayload {
                        elapsed_ms: st.elapsed_ms,
                        frames_captured: st.frames_captured,
                        frames_scheduled: st.frames_scheduled,
                        frames_duplicated: st.frames_duplicated,
                        frames_dropped: st.frames_dropped_backpressure,
                        frames_encoded: st.frames_encoded,
                        queue_degraded: st.degraded,
                        audio_sources: st.audio_sources,
                    },
                );
            }
        })
        .ok();
}

#[cfg(test)]
#[cfg(windows)]
mod frame_guard_tests {
    use super::shadow_gutter;

    /// 实测值回归（@150%）：坏窗口 562x553 / 客户区 540x540（多 22x13），
    /// 修好后 540x540 / 540x540。检查点必须把这两种分开，否则要么漏报要么天天误报。
    #[test]
    fn shadow_gutter_separates_real_measurements() {
        // 实测的坏值（tao 阴影沟槽在场，看到的那圈"框"）
        assert_eq!(shadow_gutter((562, 553), (540, 540)), Some((22, 13)));
        // 实测的好值（`"shadow": false` 之后）
        assert_eq!(shadow_gutter((540, 540), (540, 540)), None);
        // 1~2px 舍入噪音：不报（否则护栏会天天误报，等于没有护栏）
        assert_eq!(shadow_gutter((541, 541), (540, 540)), None);
        assert_eq!(shadow_gutter((542, 541), (540, 540)), None);
        // 恰好到阈值：报
        assert_eq!(shadow_gutter((544, 540), (540, 540)), Some((4, 0)));
    }
}

