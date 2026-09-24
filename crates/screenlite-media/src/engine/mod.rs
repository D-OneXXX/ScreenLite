//! Recorder Engine：状态机 + 30Hz CFR 调度器 + 有界队列 + 停止流程。
//!
//! 线程模型：
//!
//! ```text
//! 调用方线程 ── Recorder::start()（快速同步校验）→ 立即返回
//! │
//! ├─ 媒体线程（专用 std::thread，非 tokio）
//! │ COM/MF 初始化 → D3D11 设备 → device manager → WGC 会话
//! │ → 等待首帧（超时 5s）→ 创建编码器 → 30Hz tick 循环
//! │
//! ├─ WGC 工作线程（CreateFreeThreaded 的内部线程）
//! │ CopyResource → frame.Close() → Map → BGRA→NV12 → 放入「最新帧槽」
//! │
//! └─ 编码线程
//! 消费有界队列 → 提交 MF → finalize
//! ```
//!
//! 停止顺序严格遵循技术基线，且先关采集、后 drain 队列是安全的，
//! 因为 V0.1 的帧是自包含 CPU 缓冲（不引用 D3D11 资源）。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{bounded, Receiver, Sender};

use crate::audio::AudioSource;
use crate::capture::{self, WgcCapture};
use crate::consts::*;
use crate::convert::{even_dimensions, Nv12Image};
use crate::encoder::{
    create_encoder, ConfigureOutcome, EncoderConfig, FinalizeOutcome, HardwarePreference,
    VideoEncoder, VideoFrame, VideoFrameData,
};
use crate::error::{MediaError, MediaResult, StopPhase};
use crate::mf::MfRuntime;
use crate::scheduler::{slot_deadline_ns, slot_duration_mf, slot_time_mf};
use crate::time::{MfTime100ns, Srt100ns, Timeline};

/// 引擎状态机。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum EngineState {
    Idle,
    Preparing,
    Recording,
    Stopping,
    Error,
}

impl EngineState {
    pub fn as_str(&self) -> &'static str {
        match self {
            EngineState::Idle => "Idle",
            EngineState::Preparing => "Preparing",
            EngineState::Recording => "Recording",
            EngineState::Stopping => "Stopping",
            EngineState::Error => "Error",
        }
    }
}

#[derive(Debug, Clone)]
pub struct RecordingConfig {
    pub display_id: String,
    pub fps: u32,
    /// None → 用 `default_bitrate(w, h, fps)`
    pub bitrate: Option<u32>,
    pub output_dir: PathBuf,
    pub hardware: HardwarePreference,
    /// 录制时长上限（秒）。默认 [`MAX_RECORDING_SECS`]（2 小时），且不允许超过该硬上限。
    pub max_duration_secs: u64,
    /// 区域录制：以采集画幅的物理像素为坐标的裁剪矩形。None = 整屏。
    ///
    /// 引擎会在拿到真实采集尺寸后把它裁剪到画幅内并收敛为偶数尺寸；
    /// 裁剪后无效（完全在画幅外）则回退为整屏录制。
    pub region: Option<crate::convert::Region>,
    /// 是否采集音频。默认开。
    ///
    /// 设备缺失/格式异常时降级为纯视频（WARN），绝不失败录制（契约见 `audio::source`）。
    /// 旧注释里那句"静音时不添加音频轨（懒创建）"已经不成立： 定了 AddStream 必须在
    /// BeginWriting 之前，所以音频轨在开始录制时就已创建；桌面静音由静音补位维持。
    pub audio: bool,
    /// 音频来源列表：每路 = 种类 + 每源增益。至少一路（空 = 调用方拒绝启动，
    /// 不做"静音录制"这种模糊态）。某路设备打不开时该路退出混音（WARN），绝不失败录制。
    pub audio_sources: Vec<crate::audio::AudioSourceSpec>,
}

impl RecordingConfig {
    pub fn new(display_id: impl Into<String>, output_dir: impl Into<PathBuf>) -> Self {
        Self {
            display_id: display_id.into(),
            fps: DEFAULT_FPS,
            bitrate: None,
            output_dir: output_dir.into(),
            hardware: HardwarePreference::PreferHardware,
            max_duration_secs: MAX_RECORDING_SECS,
            region: None,
            audio: true,
            audio_sources: vec![crate::audio::AudioSourceSpec {
                kind: crate::audio::AudioSourceKind::default(),
                gain: 1.0,
            }],
        }
    }
}

/// 把每源指标发布进共享 Metrics：每源快照 + 聚合值（各路之和/最大值）。
/// 音频线程每次轮询后调用——50 Hz 的 Mutex 写，开销可忽略。
fn publish_audio_metrics(mixer: &crate::audio::AudioMixer, metrics: &Metrics) {
    let live: Vec<AudioSourceLive> = mixer
        .source_metrics()
        .into_iter()
        .map(|(kind, endpoint, m)| AudioSourceLive {
            kind: kind.as_str().to_string(),
            endpoint,
            metrics: m,
        })
        .collect();
    let mut chunks = 0u64;
    let mut filler = 0u64;
    let mut peak = 0u64;
    let mut peak1 = 0u64;
    for s in &live {
        chunks += s.metrics.chunks;
        filler += s.metrics.filler_blocks;
        peak = peak.max(s.metrics.peak);
        peak1 = peak1.max(s.metrics.peak_last_sec);
    }
    *metrics.audio_sources.lock().unwrap() = live;
    metrics.audio_chunks.store(chunks, Ordering::Relaxed);
    metrics.audio_filler_blocks.store(filler, Ordering::Relaxed);
    metrics.audio_peak.store(peak, Ordering::Relaxed);
    metrics.audio_peak_1s.store(peak1, Ordering::Relaxed);
}

/// 交给写入线程的一块音频。
struct AudioItem {
    chunk: crate::audio::AudioChunk,
    format: crate::audio::AudioFormat,
    pts: MfTime100ns,
    duration: MfTime100ns,
}

/// 本次录制是为什么结束的。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum StopReason {
    /// 用户（或调用方）请求停止
    UserRequested,
    /// 到达配置的时长上限，自动停止（文件正常 finalize）
    DurationLimit,
    /// 磁盘可用空间跌破硬地板，提前优雅停止（文件正常 finalize，避免写到满盘导致文件损坏）
    DiskSpaceLow,
}

impl StopReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            StopReason::UserRequested => "UserRequested",
            StopReason::DurationLimit => "DurationLimit",
            StopReason::DiskSpaceLow => "DiskSpaceLow",
        }
    }
}

/// 每源音频指标的实时快照：进 RecordingStatus/RecorderOutcome 与每源日志行。
/// `metrics.real_blocks = chunks − filler_blocks` 才是"这一路真实设备数据"的直接依据。
#[derive(Clone, Debug, Default)]
pub struct AudioSourceLive {
    pub kind: String,
    pub endpoint: String,
    pub metrics: crate::audio::SourceMetrics,
}

/// 运行时指标。全部用原子量，UI 读取不阻塞媒体线程。
#[derive(Debug, Default)]
pub struct Metrics {
    pub frames_captured: AtomicU64,
    /// WGC 回调里被更新版本覆盖的帧（正常降采样，不是异常丢帧）
    pub frames_overwritten: AtomicU64,
    pub frames_scheduled: AtomicU64,
    pub frames_duplicated: AtomicU64,
    pub frames_dropped_backpressure: AtomicU64,
    /// 其中发生在启动瞬态窗口（[`STARTUP_WINDOW_SECS`]）内的部分。
    ///
    /// 分开放是为了让看板上的"丢帧"只反映稳态：实测丢帧全部集中在前 ~1.5 秒
    /// （MFT 首次协商 + 首个关键帧），稳态为 0，混在一起会被误读成持续跟不上。
    /// 总丢帧数 = frames_dropped_backpressure + frames_dropped_startup。
    pub frames_dropped_startup: AtomicU64,
    pub frames_encoded: AtomicU64,
    /// 其中属于「重复上一帧」的样本数（CFR 静态画面会产生大量重复，属正常）
    pub encoded_duplicates: AtomicU64,
    /// 已写入的音频块数
    pub audio_chunks: AtomicU64,
    /// 因音频通道满而丢弃的块数（音频让位给视频，绝不会反过来）
    pub frames_dropped_audio: AtomicU64,
    /// 因时间戳倒退被写入侧拒绝的音频块数。
    ///
    /// MF 的 sink writer 不接受同一轨时间戳倒退。静音补位之后，真实数据块
    /// "回追"是可能的（设备卡顿 → 静音已补到 now−100ms → 该块带着更早的
    /// 时间戳到达）。期望长期为 0；一旦增长，说明音频设备真的卡过。
    pub audio_dropped_backwards: AtomicU64,
    /// 采集"无帧"的观测：每 1 秒检查一次，
    /// `idle ≥ CAPTURE_STALL_WARN_SECS` 就 +1 —— 值大说明画面长时间没变（静止桌面属正常）。
    pub capture_idle_checks: AtomicU64,
    /// 观测到的最长"无帧"时长（ms）。只用于诊断，不参与任何判定。
    pub capture_idle_ms_max: AtomicU64,
    /// 音频峰值（全程 `|sample|` 最大值，滑动最大）。
    ///
    /// 用来给"录了但没声音"定性：全程 0 说明进设备的信号本身就是数字静音
    /// （系统音量 0 / 麦克风静音 / 选错设备）——不是我们没接上。
    /// loopback 同样适用（用户以为在放声音，其实系统静音）。
    pub audio_peak: AtomicU64,
    /// 当前这一秒的峰值（在已有的 1 秒 tick 里复位）——将来的实时电平条直接用它。
    ///
    /// 为什么不能只有历史最大：历史最大一旦有过声音就永远 > 0，只能回答"整段有没有信号"，
    /// 回答不了"此刻有没有信号"——而用户投诉的恰恰是后者。
    pub audio_peak_1s: AtomicU64,
    /// 刚过去那一秒的峰值（摘要与判读用）：
    /// `历史 > 0 且 最近1秒 == 0` → 中途断了/设备被占用/被静音；两者都 > 0 → 信号正常。
    pub audio_peak_last_sec: AtomicU64,
    /// 每源音频指标的实时快照。由音频线程发布；聚合值 = 各路之和/最大值。
    pub audio_sources: std::sync::Mutex<Vec<AudioSourceLive>>,
    /// 我们合成的静音块数（补位路径 + 收尾补位），与 `audio_chunks` 在同一处计数。
    ///
    /// 用途：把"真实音频在流"的检查点从代理升级为直接。
    /// `peak > 0` 只是代理——一个正在播放但内容恰好是数字静音的流，会给真实设备包而 `peak == 0`；
    /// 反过来，一个"有声音但很小"的流也满足代理。直接依据是
    /// `audio_chunks - audio_filler_blocks > 0`（有真实块）且补位块不构成主体：
    /// 走补位路径的长跑测到的是我们自己的时钟（按构造不漂），
    /// 对"要不要重采样"零信息量。
    pub audio_filler_blocks: AtomicU64,
    /// 已写入音频轨的时间上界（100ns，含静音补位）。长时间停在很小值说明
    /// 音频轨没有前进——此时 MF 混流器会等音频轨交错，把视频写入阻塞住
    /// （见 2026-09-22 的丢帧事故：桌面静音时每帧被阻塞约 1s）。
    pub audio_last_pts_100ns: AtomicI64,
    /// 最近一次提交的视频帧时间（100ns）。与 `audio_last_pts_100ns` 对照，
    /// 才能把"音频轨停了"与"编码器真的慢"区分开：
    /// 两者同步增长 = 编码/磁盘瓶颈；视频猛涨而音频不动 = 混流器等音频轨。
    pub video_last_pts_100ns: AtomicI64,
    pub timestamp_anomalies: AtomicU64,
    /// 因「晚于 slot + 2 个帧间隔」被拒绝的帧数。
    /// 期望长期为 0；持续增长说明 tick 时钟与捕获时间轴锚定不一致。
    pub frames_rejected_future: AtomicU64,
    /// tick 落后于 deadline 超过 1 个帧间隔的次数
    pub slots_late: AtomicU64,
    /// 已流逝的 100ns（供 UI 显示；权威时长以最后一帧的 SRT 为准）
    pub elapsed_100ns: AtomicU64,
    /// 单帧提交耗时（毫秒 × 100，累计和 / 计数）
    pub submit_ms_x100_sum: AtomicU64,
    pub submit_count: AtomicU64,
}

/// 每源音频指标的验收/展示形状。
#[derive(Debug, Clone, serde::Serialize)]
pub struct AudioSourceStatus {
    pub kind: String,
    pub endpoint: String,
    pub chunks: u64,
    pub filler_blocks: u64,
    /// `chunks − filler_blocks`：这一路的真实设备块
    pub real_blocks: u64,
    pub peak: u64,
    pub peak_last_sec: u64,
    pub dropped_backwards: u64,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecordingStatus {
    pub state: EngineState,
    pub elapsed_ms: i64,
    pub frames_captured: u64,
    pub frames_overwritten: u64,
    pub frames_scheduled: u64,
    pub frames_duplicated: u64,
    pub frames_dropped_backpressure: u64,
    /// 其中属于启动瞬态窗口的部分（见 [`STARTUP_WINDOW_SECS`]）。
    /// 总丢帧 = 两者之和；`frames_dropped_backpressure` 只反映稳态，
    /// 就是为了让看板上的"丢帧"不被启动瞬态污染。
    pub frames_dropped_startup: u64,
    /// 因音频时间戳倒退被写入侧拒绝的块数。期望长期为 0：
    /// 一旦增长就说明静音补位的安全边距不够（或音频设备真的卡过）。
    pub audio_dropped_backwards: u64,
    /// 采集"无帧"的观测：`idle ≥ CAPTURE_STALL_WARN_SECS`的秒数
    pub capture_idle_checks: u64,
    /// 观测到的最长"无帧"时长（ms）——判读"画面是不是长时间静止/显示器是不是被关了"
    pub capture_idle_ms_max: u64,
    /// 音频峰值（全程 `|sample|` 最大值）。0 = 进设备的信号本身就是数字静音：
    /// 让用户去查系统音量 / 麦克风静音开关 / 是不是选错了设备。
    pub audio_peak: u64,
    /// 当前这一秒的峰值（实时电平条的现成数据源）
    pub audio_peak_1s: u64,
    /// 刚过去那一秒的峰值：`历史 > 0 且 它 == 0` → 中途断了/设备被占用/被静音
    pub audio_peak_last_sec: u64,
    /// 每源音频指标。真实设备块 = chunks − filler_blocks（每源独立，直接依据）。
    pub audio_sources: Vec<AudioSourceStatus>,
    /// 已写入的音频块总数（真实 + 补位）
    pub audio_chunks: u64,
    /// 我们合成的静音块数（补位 + 收尾补位）。与 `audio_chunks` 相减 = 真实设备块数：
    /// 这是"真实音频在流"的直接检查点（`peak > 0` 只是代理，见 `Metrics::audio_filler_blocks`）。
    pub audio_filler_blocks: u64,
    pub frames_encoded: u64,
    pub timestamp_anomalies: u64,
    pub slots_late: u64,
    pub error_code: Option<&'static str>,
    pub error_message: Option<String>,
    pub output_path: Option<PathBuf>,
    pub actual: Option<ConfigureOutcome>,
    /// 10 秒窗口内丢帧率 > 5%
    pub degraded: bool,
}

#[derive(Debug)]
pub struct RecorderOutcome {
    pub finalize: FinalizeOutcome,
    pub elapsed_ms: i64,
    pub frames_scheduled: u64,
    pub frames_duplicated: u64,
    pub frames_dropped_backpressure: u64,
    /// 启动瞬态窗口内的丢帧（见 [`STARTUP_WINDOW_SECS`]），与上者分开统计
    pub frames_dropped_startup: u64,
    /// 因音频时间戳倒退被写入侧拒绝的块数
    pub audio_dropped_backwards: u64,
    /// 采集"无帧"的观测：`idle ≥ CAPTURE_STALL_WARN_SECS`的秒数
    pub capture_idle_checks: u64,
    /// 观测到的最长"无帧"时长（ms）
    pub capture_idle_ms_max: u64,
    /// 音频峰值（全程 `|sample|` 最大值）；0 = 进设备的信号就是数字静音
    pub audio_peak: u64,
    /// 当前这一秒的峰值（实时电平条用）
    pub audio_peak_1s: u64,
    /// 刚过去那一秒的峰值（判读"中途是否断了"）
    pub audio_peak_last_sec: u64,
    /// 每源音频指标：kind/endpoint/块/补位/真实设备块/峰值/倒退
    pub audio_sources: Vec<AudioSourceStatus>,
    /// 合成的静音块数（`audio_chunks - 它` = 真实设备块数，准入条件的直接依据）
    pub audio_filler_blocks: u64,
    /// 已写入的音频块总数（真实 + 补位）
    pub audio_chunks: u64,
    pub frames_captured: u64,
    pub timestamp_anomalies: u64,
    pub configured: ConfigureOutcome,
    /// 结束原因（用户请求 / 时长上限）
    pub stop_reason: StopReason,
    /// 最后一帧视频的时间轴位置（100ns）。与 `audio_last_pts_100ns` 的差 = A/V 差。
    ///
    /// 为什么带出来：长跑验收要报"A/V 差与 5 秒预检的对比"。
    /// 这两个数在 `Metrics` 里本来就有，但没进 `RecorderOutcome`，
    /// 于是调用方只能去逐帧解码探测产物（`probe_mp4` 对 1.6 GB 要跑几分钟）。
    pub video_last_pts_100ns: i64,
    pub audio_last_pts_100ns: i64,
}

#[derive(Debug)]
struct Shared {
    state: Mutex<EngineState>,
    error: Mutex<Option<MediaError>>,
    metrics: Arc<Metrics>,
    output_path: Mutex<Option<PathBuf>>,
    actual: Mutex<Option<ConfigureOutcome>>,
}

enum Command {
    Stop,
}

/// 交给编码线程的一帧。
struct TickItem {
    nv12: Arc<Nv12Image>,
    pts: MfTime100ns,
    duration: MfTime100ns,
    sequence: u64,
    duplicate: bool,
    discontinuity: bool,
}

pub struct Recorder {
    shared: Arc<Shared>,
    cmd_tx: Sender<Command>,
    media_handle: Option<JoinHandle<MediaResult<RecorderOutcome>>>,
}

impl Recorder {
    /// 启动录制。只做快速同步校验，随后立即返回；
    /// 后续失败通过 [`Recorder::status`] 的 Error 状态暴露。
    pub fn start(cfg: RecordingConfig) -> MediaResult<Self> {
        // 同步校验：显示器存在、帧率在白名单内、输出目录可写
        if !is_allowed_fps(cfg.fps) {
            return Err(MediaError::UnsupportedFrameRate {
                fps: cfg.fps,
                allowed: &ALLOWED_FPS,
            });
        }
        if cfg.max_duration_secs == 0 || cfg.max_duration_secs > MAX_RECORDING_SECS {
            return Err(MediaError::Internal(format!(
                "非法时长上限：{} 秒（允许范围 1–{} 秒）",
                cfg.max_duration_secs, MAX_RECORDING_SECS
            )));
        }
        let resolved = capture::resolve_display(&cfg.display_id)?;
        if resolved.info.hdr {
            return Err(MediaError::HdrDisplayNotSupported);
        }
        ensure_output_dir(&cfg.output_dir)?;

        let shared = Arc::new(Shared {
            state: Mutex::new(EngineState::Preparing),
            error: Mutex::new(None),
            metrics: Arc::new(Metrics::default()),
            output_path: Mutex::new(None),
            actual: Mutex::new(None),
        });
        let (cmd_tx, cmd_rx) = bounded::<Command>(4);
        let thread_shared = shared.clone();

        let media_handle = std::thread::Builder::new()
            .name("screenlite-media".into())
            .spawn(move || run_media_thread(cfg, thread_shared, cmd_rx))
            .map_err(|e| MediaError::Internal(format!("创建媒体线程失败：{}", e)))?;

        Ok(Self {
            shared,
            cmd_tx,
            media_handle: Some(media_handle),
        })
    }

    pub fn status(&self) -> RecordingStatus {
        let metrics = self.shared.metrics.as_ref();
        let scheduled = metrics.frames_scheduled.load(Ordering::Relaxed);
        let dropped = metrics.frames_dropped_backpressure.load(Ordering::Relaxed);

        // 注意：每个 Mutex 只锁一次，且先取出值再释放。
        // std::sync::Mutex 不可重入；如果在同一个结构体字面量里对同一把锁取两次，
        // 第一个 guard 会活到语句结束，造成自我死锁（这个 bug 被冒烟测试抓到过一次）。
        let state = *self.shared.state.lock().unwrap();
        let (error_code, error_message) = {
            let guard = self.shared.error.lock().unwrap();
            (
                guard.as_ref().map(|e| e.code()),
                guard.as_ref().map(|e| e.to_string()),
            )
        };
        let output_path = self.shared.output_path.lock().unwrap().clone();
        let actual = self.shared.actual.lock().unwrap().clone();

        RecordingStatus {
            state,
            elapsed_ms: metrics.elapsed_100ns.load(Ordering::Relaxed) as i64 / 10_000,
            frames_captured: metrics.frames_captured.load(Ordering::Relaxed),
            frames_overwritten: metrics.frames_overwritten.load(Ordering::Relaxed),
            frames_scheduled: scheduled,
            frames_duplicated: metrics.frames_duplicated.load(Ordering::Relaxed),
            frames_dropped_backpressure: dropped,
            frames_dropped_startup: metrics.frames_dropped_startup.load(Ordering::Relaxed),
            audio_dropped_backwards: metrics.audio_dropped_backwards.load(Ordering::Relaxed),
            capture_idle_checks: metrics.capture_idle_checks.load(Ordering::Relaxed),
            capture_idle_ms_max: metrics.capture_idle_ms_max.load(Ordering::Relaxed),
            audio_peak: metrics.audio_peak.load(Ordering::Relaxed),
            audio_sources: metrics
                .audio_sources
                .lock()
                .unwrap()
                .iter()
                .map(|s| AudioSourceStatus {
                    kind: s.kind.clone(),
                    endpoint: s.endpoint.clone(),
                    chunks: s.metrics.chunks,
                    filler_blocks: s.metrics.filler_blocks,
                    real_blocks: s.metrics.real_blocks(),
                    peak: s.metrics.peak,
                    peak_last_sec: s.metrics.peak_last_sec,
                    dropped_backwards: s.metrics.dropped_backwards,
                })
                .collect(),
            audio_peak_1s: metrics.audio_peak_1s.load(Ordering::Relaxed),
            audio_peak_last_sec: metrics.audio_peak_last_sec.load(Ordering::Relaxed),
            audio_chunks: metrics.audio_chunks.load(Ordering::Relaxed),
            audio_filler_blocks: metrics.audio_filler_blocks.load(Ordering::Relaxed),
            frames_encoded: metrics.frames_encoded.load(Ordering::Relaxed),
            timestamp_anomalies: metrics.timestamp_anomalies.load(Ordering::Relaxed),
            slots_late: metrics.slots_late.load(Ordering::Relaxed),
            error_code,
            error_message,
            output_path,
            actual,
            degraded: scheduled > 0 && (dropped as f64 / scheduled as f64) > DEGRADED_DROP_RATIO,
        }
    }

    /// 停止录制。幂等：重复调用直接返回当前状态。
    ///
    /// 也支持"已经自行停止"的情况（时长上限 / 磁盘不足触发自动停止）——
    /// 此时状态已是 `Idle`，本方法直接 join 并返回那次的结果。
    pub fn stop(mut self) -> MediaResult<RecorderOutcome> {
        {
            let mut state = self.shared.state.lock().unwrap();
            if *state == EngineState::Stopping {
                drop(state);
                return Err(MediaError::Internal("停止已在进行中".into()));
            }
            if *state != EngineState::Idle {
                *state = EngineState::Stopping;
            }
        }
        let _ = self.cmd_tx.send(Command::Stop);

        match self.media_handle.take() {
            Some(handle) => match handle.join() {
                Ok(Ok(outcome)) => Ok(outcome),
                Ok(Err(e)) => Err(e),
                Err(_) => Err(MediaError::Internal("媒体线程 panic".into())),
            },
            None => Err(MediaError::Internal("媒体线程句柄缺失".into())),
        }
    }
}

impl Drop for Recorder {
    fn drop(&mut self) {
        // 兜底：未显式 stop 就析构时，仍然走一遍停止流程，避免产生损坏文件
        if let Some(handle) = self.media_handle.take() {
            // 这条日志很重要：Recorder 被意外 drop 会立即停止录制，
            // 表现是"刚开始录就失败、0 个样本"。出现它说明调用方过早释放了 Recorder。
            tracing::warn!("Recorder 被 drop：将发送停止命令并兜底 finalize（若这不是预期行为，说明调用方过早释放了它）");
            let _ = self.cmd_tx.send(Command::Stop);
            match handle.join() {
                Ok(Ok(outcome)) => {
                    tracing::warn!(
                        path = %outcome.finalize.path.display(),
                        "Recorder 未显式 stop 即被释放，已兜底 finalize"
                    );
                }
                Ok(Err(e)) => tracing::error!(error = %e, "兜底停止失败"),
                Err(_) => tracing::error!("媒体线程 panic（兜底停止）"),
            }
        }
    }
}

/// 记一次丢帧。
///
/// 启动瞬态窗口内（见 [`STARTUP_WINDOW_SECS`]）单独计入 `frames_dropped_startup`：
/// 看板上的"丢帧"必须只反映稳态，否则"录 15 秒丢 6 帧"会被误读成持续跟不上
/// （实测那段丢帧全部发生在前 ~1.5 秒，稳态为 0）。
fn count_drop(metrics: &Metrics, startup: bool) {
    let counter = if startup {
        &metrics.frames_dropped_startup
    } else {
        &metrics.frames_dropped_backpressure
    };
    counter.fetch_add(1, Ordering::Relaxed);
}

/// drain 收尾：用静音块把音频轨补齐到最后一帧视频的结束点。
///
/// 为什么需要：静音补位只补到「现在 − [`SILENCE_GUARD_MS`]」（100ms），所以音频轨尾部
/// 天然比视频轨短一个安全边距；
/// 有声场景因为数据一直流到最后一刻，差只有 -17ms，只有静音场景才明显。
///
/// 三条约束：
/// 1. 补位块必须过单调保护——这里显式复用写入线程的那条规则与上界，
/// 绝不允许把刚建好的写入侧防线自己捅穿；
/// 2. 有上限（[`AUDIO_TAIL_PAD_MAX_MS`]）——视频异常早停而音频轨已经很长时，
/// 无上限补位会一次性灌入大量静音；
/// 3. 只在音频轨确实在被喂养时补（提交失败过就整段放弃）。
///
/// 返回实际补入的时长（100ns）。
fn pad_audio_tail(
    encoder: &mut dyn VideoEncoder,
    fmt: Option<crate::audio::AudioFormat>,
    metrics: &Metrics,
    last_audio_pts: &mut i64,
    audio_written_end: i64,
    video_end: i64,
) -> i64 {
    let Some(fmt) = fmt else { return 0 };
    if video_end <= 0 {
        return 0;
    }
    let sample_rate = fmt.sample_rate.max(1) as i64;
    let frames = ((sample_rate * SILENCE_CHUNK_MS / 1000).max(1)) as u32;
    let duration = MfTime100ns(frames as i64 * 10_000_000 / sample_rate);
    if duration.0 <= 0 {
        return 0;
    }

    // 单调保护的基准：写过音频就是最后一块的起点；一块都没写过（极短录制）视为 0 之前，
    // 更不能拿 i64::MIN 参与运算。
    let mut last_written = if *last_audio_pts == i64::MIN {
        -1
    } else {
        *last_audio_pts
    };
    let mut cursor = audio_written_end.max(0);
    let max_pad = AUDIO_TAIL_PAD_MAX_MS * 10_000;
    let mut padded = 0i64;

    while cursor + duration.0 <= video_end && padded < max_pad {
        // 约束 1：与写入线程同一条单调规则
        if cursor <= last_written {
            metrics
                .audio_dropped_backwards
                .fetch_add(1, Ordering::Relaxed);
            break;
        }
        let chunk = crate::audio::AudioChunk::silence(&fmt, frames, Srt100ns(0));
        // 注：AudioChunk.srt 不参与写入（写入以 pts/duration 为准），此处只是占位。
        match encoder.submit_audio(&chunk, fmt, MfTime100ns(cursor), duration) {
            Ok(()) => {
                last_written = cursor;
                *last_audio_pts = cursor;
                cursor += duration.0;
                padded += duration.0;
                // 收尾补位是合成的静音，但不计入每源 filler（它不属于任何源）；
                // 每源指标见 audio_sources。
                metrics
                    .audio_last_pts_100ns
                    .store(cursor, Ordering::Relaxed);
            }
            Err(e) => {
                // 补位失败不致命：音频轨短一截而已，绝不影响已经录好的视频
                tracing::warn!(error = %e, "收尾静音补位失败，音频轨尾部稍短");
                break;
            }
        }
    }
    padded
}

/// 带截止时间地等一个线程结束（停止流程用）。
///
/// `JoinHandle::join` 是无界的——这正是停止流程曾经最大的隐患： 里那几个分段超时常量
/// 因此形同虚设（实测：一次采集会话卡住后，"停止"花了约 4.5 分钟才返回）。
/// 这里用一个搬运线程 + `recv_timeout` 把它变成有界等待；超时就放弃
/// （搬运线程会一直挂着，但进程可以退出——这正是唯一的目标）。
fn join_with_deadline<T: Send + 'static>(
    handle: JoinHandle<T>,
    deadline: Instant,
    phase: StopPhase,
) -> Result<T, MediaError> {
    let budget = deadline
        .saturating_duration_since(Instant::now())
        .min(Duration::from_millis(STOP_JOIN_TIMEOUT_MS));
    if budget.is_zero() {
        tracing::error!(phase = ?phase, "停止流程总预算已耗尽，不再等待该线程（detach）");
        return Err(MediaError::StopTimeout { phase });
    }
    let (tx, rx) = bounded(1);
    if std::thread::Builder::new()
        .name(format!("join-{phase:?}"))
        .spawn(move || {
            let _ = tx.send(handle.join());
        })
        .is_err()
    {
        tracing::error!(phase = ?phase, "无法创建等待线程");
        return Err(MediaError::StopTimeout { phase });
    }
    match rx.recv_timeout(budget) {
        Ok(Ok(v)) => Ok(v),
        Ok(Err(_)) => Err(MediaError::Internal(format!("{phase:?} 线程 panic"))),
        Err(_) => {
            tracing::error!(
                phase = ?phase,
                waited_ms = budget.as_millis() as u64,
                "停止流程超时：放弃等待该线程（detach），进程可退出；文件可能不完整（保留 .partial）"
            );
            Err(MediaError::StopTimeout { phase })
        }
    }
}

/// 提交一帧视频（含计时与重复帧统计）。
fn submit_video_item(
    encoder: &mut dyn VideoEncoder,
    item: TickItem,
    metrics: &Metrics,
) -> MediaResult<()> {
    // 供丢帧诊断与音频轨时间对照（单调递增，不需要 fetch_max）
    metrics
        .video_last_pts_100ns
        .store(item.pts.0, Ordering::Relaxed);
    if item.duplicate {
        metrics.encoded_duplicates.fetch_add(1, Ordering::Relaxed);
    }
    let frame = VideoFrame {
        data: VideoFrameData::Cpu(item.nv12),
        pts: item.pts,
        duration: item.duration,
        sequence: item.sequence,
        discontinuity: item.discontinuity,
    };
    let t = Instant::now();
    if let Err(e) = encoder.submit(&frame) {
        // 提交失败必须带上帧尺寸与序号，否则无法判断是不是尺寸不匹配
        tracing::error!(
            seq = frame.sequence,
            width = frame.nv12().width,
            height = frame.nv12().height,
            error = %e,
            "编码器提交失败，写入线程退出"
        );
        return Err(e);
    }
    let us = t.elapsed().as_micros() as u64;
    metrics.submit_ms_x100_sum.fetch_add(us / 10, Ordering::Relaxed);
    metrics.submit_count.fetch_add(1, Ordering::Relaxed);
    metrics.frames_encoded.fetch_add(1, Ordering::Relaxed);
    Ok(())
}

/// 媒体线程主体。所有 COM / MF / D3D11 调用都发生在这里。
fn run_media_thread(
    cfg: RecordingConfig,
    shared: Arc<Shared>,
    cmd_rx: Receiver<Command>,
) -> MediaResult<RecorderOutcome> {
    let result = media_pipeline(&cfg, &shared, &cmd_rx);
    match &result {
        Ok(_) => {
            // 管线自行结束（时长上限 / 磁盘不足 / 用户停止）后必须回到 Idle，
            // 否则 UI 会在自动停止后永远显示"录制中"。
            *shared.state.lock().unwrap() = EngineState::Idle;
        }
        Err(e) => {
            tracing::error!(code = e.code(), error = %e, "媒体管线失败");
            *shared.error.lock().unwrap() = Some(recreate_error(e));
            *shared.state.lock().unwrap() = EngineState::Error;
        }
    }
    result
}

fn recreate_error(e: &MediaError) -> MediaError {
    // MediaError 不实现 Clone（内含 windows::core::Error）；这里转为可存储的描述性错误
    MediaError::Internal(format!("[{}] {}", e.code(), e))
}

fn media_pipeline(
    cfg: &RecordingConfig,
    shared: &Arc<Shared>,
    cmd_rx: &Receiver<Command>,
) -> MediaResult<RecorderOutcome> {
    if !capture::is_supported() {
        return Err(MediaError::ScreenCaptureUnsupported);
    }
    let _rt = MfRuntime::start()?;

    let resolved = capture::resolve_display(&cfg.display_id)?;
    if resolved.info.hdr {
        return Err(MediaError::HdrDisplayNotSupported);
    }

    // ---- D3D11 设备 ----
    let (device, context) = crate::capture::create_d3d11_device()?;
    // 共享给 MF 的设备必须开启多线程保护
    crate::capture::enable_multithread_protection(&context)?;
    let device_manager = crate::capture::create_device_manager(&device)?;

    // ---- 采集项与权威尺寸 ----
    // 先建捕获项拿真实物理尺寸，再用它把区域裁剪到画幅内（顺序不能反）。
    let (item, (cw, ch)) = crate::capture::WgcCapture::create_item(resolved.hmonitor)?;

    // 分辨率硬上限：2K 级（长边 ≤ 2560 且高 ≤ 1600）。V0.1 不缩放，超限直接拒绝。
    if cw > MAX_CAPTURE_WIDTH || ch > MAX_CAPTURE_HEIGHT {
        return Err(MediaError::ResolutionExceedsLimit {
            width: cw,
            height: ch,
            max_width: MAX_CAPTURE_WIDTH,
            max_height: MAX_CAPTURE_HEIGHT,
        });
    }

    // 区域录制：采集始终是整屏，这里只把区域裁剪到画幅内并收敛为偶数尺寸。
    let region = match cfg.region {
        Some(req) if !req.is_full_frame(cw, ch) => match req.clamp_to(cw, ch) {
            Some(r) => {
                tracing::info!(
                    x = r.x,
                    y = r.y,
                    width = r.width,
                    height = r.height,
                    frame_w = cw,
                    frame_h = ch,
                    "区域录制：已裁剪到画幅内"
                );
                Some(r)
            }
            None => {
                tracing::warn!(
                    x = req.x,
                    y = req.y,
                    width = req.width,
                    height = req.height,
                    frame_w = cw,
                    frame_h = ch,
                    "请求的区域完全在画幅外，回退为整屏录制"
                );
                None
            }
        },
        _ => None,
    };

    // ---- 采集 ----
    let slot: Arc<Mutex<Option<crate::capture::WgcFrame>>> = Arc::new(Mutex::new(None));
    let sink_slot = slot.clone();
    let sink_metrics = shared.metrics.clone();
    let capture = crate::capture::WgcCapture::start_with_item(
        item,
        (cw, ch),
        &device,
        &context,
        region,
        move |frame| {
        // 容量 1：最新帧胜出，被覆盖的旧帧属正常降采样
        if sink_slot.lock().unwrap().replace(frame).is_some() {
            sink_metrics
                .frames_overwritten
                .fetch_add(1, Ordering::Relaxed);
        }
        sink_metrics.frames_captured.fetch_add(1, Ordering::Relaxed);
    })?;

    let (cw, ch) = capture.size();
    tracing::info!(
        display = %resolved.info.device_name,
        display_id = %resolved.info.id,
        width = cw,
        height = ch,
        border_allowed = capture.border_required(),
        "采集已启动"
    );

    // ---- 等待首帧（t0 取第一帧的 SRT）----
    let deadline = Instant::now() + Duration::from_secs(FIRST_FRAME_TIMEOUT_SECS);
    let first = loop {
        if let Some(fatal) = capture.take_fatal() {
            capture.stop();
            return Err(fatal);
        }
        if let Some(frame) = slot.lock().unwrap().take() {
            break frame;
        }
        if Instant::now() > deadline {
            capture.stop();
            return Err(MediaError::CaptureNoFrames(FIRST_FRAME_TIMEOUT_SECS));
        }
        std::thread::sleep(Duration::from_millis(5));
    };
    let t0 = first.srt;
    // 关键：tick 循环的单调时钟必须与捕获时间轴同一个起点。
    // 若把 start 取在编码器创建之后（晚 50~150ms），帧的 mf 时间会永远比 slot 时间
    // 超前一个固定偏移，导致每个 slot 都被"未来帧守卫"拒绝、整段录制退化成一张静止画面。
    let capture_epoch = Instant::now();
    let (mut w, mut h) = (first.nv12.width, first.nv12.height);
    let (ew, eh) = even_dimensions(w, h);
    if (ew, eh) != (w, h) {
        tracing::warn!(width = w, height = h, "捕获尺寸为奇数，NV12 需要偶数：将裁剪 1 像素");
        w = ew;
        h = eh;
    }

    // ---- 编码器 ----
    let (partial_path, final_path) = unique_output_paths(&cfg.output_dir)?;
    *shared.output_path.lock().unwrap() = Some(final_path.clone());
    // ---- 音频源→ 每源 GridAdapter → 混音点 → 一条流 ----
    // 必须先于编码器启动：音频格式要在建编码器时确定（AddStream 只能在 BeginWriting 之前）。
    let mut adapters: Vec<crate::audio::GridAdapter> = Vec::new();
    if cfg.audio {
        for src in &cfg.audio_sources {
            let kind = src.kind;
            let direction = kind.direction();
            // 枚举结果各打一行 INFO：这样"麦克风没声音"能一眼分清是选错设备还是设备对了但没数据
            match crate::audio::enumerate_endpoints(direction) {
                Ok(list) => {
                    for ep in &list {
                        tracing::info!(
                            id = %ep.id,
                            name = %ep.name,
                            default = ep.is_default,
                            "可用音频端点"
                        );
                    }
                    if list.is_empty() {
                        tracing::warn!(kind = kind.as_str(), "没有可用的音频端点");
                    }
                }
                Err(e) => tracing::warn!(error = %e, "枚举音频端点失败（不影响录制）"),
            }
            let spec = crate::audio::EndpointSpec::default_of(direction);
            match crate::audio::WasapiEndpoint::open_endpoint(&spec) {
                Ok(ep) => {
                    // 网格槽 = 20ms @ 该源自己的采样率
                    let slot_frames =
                        ((ep.format().sample_rate as i64 * SILENCE_CHUNK_MS / 1000).max(1)) as usize;
                    tracing::info!(
                        kind = kind.as_str(),
                        endpoint = %ep.opened(),
                        gain = src.gain,
                        slot_frames,
                        "音频源已就绪"
                    );
                    adapters.push(crate::audio::GridAdapter::new(
                        kind,
                        Box::new(ep),
                        src.gain,
                        slot_frames,
                    ));
                }
                // 设备缺失的降级（契约见 audio/source.rs）：这一路退出混音 + WARN，绝不失败录制
                Err(e) => tracing::warn!(
                    kind = kind.as_str(),
                    error = %e,
                    "音频源打不开 → 这一路不参与混音（全部失败则为纯视频，不失败录制）"
                ),
            }
        }
    }
    let (audio_mixer, described) = match crate::audio::AudioMixer::new(adapters) {
        Ok((mixer, described)) => (mixer, described),
        Err(e) => {
            // 格式不一致就降级成纯视频（重采样不在 V0.2 范围内）
            tracing::warn!(error = %e, "音频源格式不一致 → 本次录制为纯视频");
            (
                crate::audio::AudioMixer::new(Vec::new())
                    .expect("空混音器永远不会失败")
                    .0,
                Vec::new(),
            )
        }
    };
    if !described.is_empty() {
        tracing::info!(
            sources = described.len(),
            detail = %described.join(" + "),
            "混音点已就绪"
        );
    }
    let audio_format = audio_mixer.format();
    if audio_format.is_none() {
        tracing::info!("本次录制无音频轨（纯视频）");
    }

    let encoder_cfg = EncoderConfig {
        width: w,
        height: h,
        fps: cfg.fps,
        bitrate: cfg.bitrate.unwrap_or_else(|| default_bitrate(w, h, cfg.fps)),
        gop_seconds: GOP_SECONDS,
        hardware: cfg.hardware,
        partial_path: partial_path.clone(),
        final_path: final_path.clone(),
        audio: audio_format,
    };
    tracing::info!(
        encoder = "MediaFoundation/H264",
        hardware_requested = cfg.hardware == HardwarePreference::PreferHardware,
        width = w,
        height = h,
        fps = cfg.fps,
        bitrate = encoder_cfg.bitrate,
        gop = cfg.fps * GOP_SECONDS,
        queue_capacity = queue_capacity_for(cfg.fps),
        output = %final_path.display(),
        "启动快照"
    );
    for note in crate::encoder::capabilities::probe_h264(cfg.hardware).notes {
        tracing::info!(note = %note, "编码器能力");
    }

    tracing::info!("正在创建编码器…");
    // 磁盘空间预检：2 小时 @ 12 Mbps ≈ 10.8 GB。不足则拒绝启动，
    // 而不是录到一半把磁盘写满、交出无法播放的文件。
    let disk_required = crate::disk::required_bytes(encoder_cfg.bitrate, cfg.max_duration_secs);
    let disk_free = crate::disk::check_before_start(&cfg.output_dir, disk_required)?;
    tracing::info!(
        free = %crate::disk::human(disk_free),
        required = %crate::disk::human(disk_required),
        "磁盘空间预检通过"
    );
    let encoder = create_encoder(&encoder_cfg, Some(&device_manager))?;
    tracing::info!("编码器已创建");
    *shared.actual.lock().unwrap() = Some(encoder.configure_outcome().clone());

    // ---- 编码线程 ----
    // 音频通道：容量 ≈ 640ms（64 块 × 10ms）。满了丢音频——绝不反过来阻塞视频。
    let (audio_tx, audio_rx) = bounded::<AudioItem>(64);

    // ---- 音频采集线程（独立线程持有 WASAPI，任何音频侧问题都不得影响视频链路）----
    // 线程只跟混音点打交道（今天只有一路源，所以等价于原来的单源路径）。
    let audio_stop = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let audio_handle = if let Some(fmt) = audio_format {
        let stop = audio_stop.clone();
        let metrics = shared.metrics.clone();
        let t0_audio = t0;
        std::thread::Builder::new()
            .name("screenlite-audio".into())
            .spawn(move || {
                let mut mixer = audio_mixer;
                let format = fmt;
                // 与视频共用同一个 t0：这是音视频同步的根
                let mut timeline = Timeline::new(t0_audio);
                let sample_rate = format.sample_rate.max(1) as i64;
                // 网格对齐与静音补位都在每源 GridAdapter 内部；本线程只消费混音后的
                // 网格块，并把每源指标发布进共享 Metrics。
                let mut last_1s = std::time::Instant::now();
                let mut weak_secs = vec![0u64; mixer.source_count()];
                let mut weak_warn = std::time::Instant::now()
                    .checked_sub(Duration::from_secs(AUDIO_WEAK_REWARN_SECS))
                    .unwrap_or_else(std::time::Instant::now);
                // 弱信号阈值：可用环境变量覆盖以验证提示路径本身（与 SL_TEST_STABLE_DELTA 同法）
                let weak_peak: u64 = std::env::var("SL_AUDIO_WEAK_PEAK")
                    .ok()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(AUDIO_WEAK_PEAK);
                while !stop.load(Ordering::Relaxed) {
                    // ---- 每秒一次：每源 1s 峰值归零 + 每源弱信号判别（提示词各指设备侧）----
                    if last_1s.elapsed() >= Duration::from_secs(1) {
                        last_1s = std::time::Instant::now();
                        let peaks = mixer.take_1s_peaks();
                        let live = mixer.source_metrics();
                        for (i, (kind, _ep, m)) in live.iter().enumerate() {
                            let flowing = m.chunks > 0;
                            let p1 = peaks.get(i).copied().unwrap_or(0);
                            if flowing && p1 < weak_peak {
                                weak_secs[i] += 1;
                                let w = weak_secs[i];
                                if w == AUDIO_WEAK_SECS
                                    || (w > AUDIO_WEAK_SECS
                                        && weak_warn.elapsed()
                                            >= Duration::from_secs(AUDIO_WEAK_REWARN_SECS))
                                {
                                    weak_warn = std::time::Instant::now();
                                    let hint = match kind {
                                        crate::audio::AudioSourceKind::SystemLoopback => {
                                            "系统音量 / 静音开关"
                                        }
                                        crate::audio::AudioSourceKind::Microphone => {
                                            "麦克风增益 / 静音开关"
                                        }
                                    };
                                    tracing::warn!(
                                        kind = kind.as_str(),
                                        peak = p1,
                                        threshold = weak_peak,
                                        weak_secs = w,
                                        hint,
                                        "输入信号极弱：设备在工作但电平接近数字静音——请检查{}{}（不是录制链路的问题）",
                                        hint,
                                        "，或确认是不是选错了设备"
                                    );
                                }
                            } else {
                                weak_secs[i] = 0;
                            }
                        }
                    }
                    match mixer.poll() {
                        Ok(Some(chunk)) => {
                            let mf = timeline.to_mf(chunk.srt);
                            if mf.0 <= 0 {
                                continue; // t0 之前的预滚块，丢弃
                            }
                            let duration = MfTime100ns(
                                chunk.frames as i64 * 10_000_000 / sample_rate,
                            );
                            let item = AudioItem { chunk, format, pts: mf, duration };
                            if audio_tx.try_send(item).is_err() {
                                metrics.frames_dropped_audio.fetch_add(1, Ordering::Relaxed);
                            }
                        }
                        Ok(None) => std::thread::sleep(Duration::from_millis(5)),
                        Err(e) => {
                            tracing::error!(error = %e, "音频混音轮询失败，后续为纯视频");
                            break;
                        }
                    }
                    publish_audio_metrics(&mixer, &metrics);
                }
                publish_audio_metrics(&mixer, &metrics); // 把尾部数据带出来
                mixer.stop();
                tracing::info!("音频采集线程退出");
            })
            .ok()
    } else {
        None
    };

    // ---- 写入线程 ----
    let queue_capacity = queue_capacity_for(cfg.fps);
    let (tx, rx) = bounded::<TickItem>(queue_capacity);
    let drop_rx = rx.clone();
    let enc_metrics = shared.metrics.clone();
    // 收尾补位要用音频格式（与建流时用的是同一个值，Copy 一份进线程）
    let tail_fmt = audio_format;
    let encoder_handle = std::thread::Builder::new()
        .name("screenlite-writer".into())
        .spawn(move || -> MediaResult<FinalizeOutcome> {
            let mut encoder = encoder;
            // 音频通道断开后不再参与 select!（否则会变成忙等）
            let mut audio_done = false;
            // 音频提交失败过（与"通道关闭"不同）：失败后不再做收尾补位，
            // 免得对着已经报错的线路灌静音
            let mut audio_failed = false;
            // 写入侧单调保护用的上界（100ns）：音频 pts 必须严格递增
            let mut last_audio_pts = i64::MIN;
            // 音频轨已写入的结束点 / 视频轨已提交的结束点（100ns）——收尾补位用
            let mut audio_written_end = 0i64;
            let mut video_submitted_end = 0i64;
            loop {
                if audio_done {
                    match rx.recv() {
                        Ok(item) => {
                            video_submitted_end = item.pts.0 + item.duration.0;
                            submit_video_item(&mut *encoder, item, &enc_metrics)?;
                        }
                        Err(_) => break,
                    }
                    continue;
                }

                // 单写者：视频与音频都在这一个线程里写入，天然保证交错顺序，
                // 也避免了对 Sink Writer 并发写入的不确定性。
                enum Step {
                    Video(TickItem),
                    Audio(AudioItem),
                    VideoClosed,
                    AudioClosed,
                }
                let step = crossbeam_channel::select! {
                    recv(rx) -> msg => match msg {
                        Ok(item) => Step::Video(item),
                        Err(_) => Step::VideoClosed,
                    },
                    recv(audio_rx) -> msg => match msg {
                        Ok(item) => Step::Audio(item),
                        Err(_) => Step::AudioClosed,
                    },
                };

                match step {
                    Step::Video(item) => {
                        video_submitted_end = item.pts.0 + item.duration.0;
                        submit_video_item(&mut *encoder, item, &enc_metrics)?;
                    }
                    Step::Audio(item) => {
                        // ---- 写入侧单调保护 ----
                        // 音频 pts 必须严格递增：MF 的 sink writer 遇到同轨时间戳
                        // 倒退会报错或产出异常轨。静音补位之后，真实数据"回追"是
                        // 可能的（设备卡顿 500ms → 静音已补到 now−100ms → 这块带着
                        // 500ms 前的时间戳到达）。100ms 边距只是把概率压小，不是保证，
                        // 所以这里必须兜住：最坏情况是丢一小段音频，绝不能是文件异常。
                        if item.pts.0 <= last_audio_pts {
                            enc_metrics
                                .audio_dropped_backwards
                                .fetch_add(1, Ordering::Relaxed);
                            continue; // 丢弃这一块（连同它的时长），不写进 sink
                        }
                        last_audio_pts = item.pts.0;
                        // 先记音频轨推进到哪（静音补位也走这里），再提交：
                        // 提交失败说明这一块没进去，时间戳就不该算数。
                        let audio_end = item.pts.0 + item.duration.0;
                        if let Err(e) = encoder.submit_audio(
                            &item.chunk,
                            item.format,
                            item.pts,
                            item.duration,
                        ) {
                            // 音频失败绝不中断视频：停掉音频来源并记录
                            tracing::error!(
                                error = %e,
                                "音频提交失败，后续音频将被丢弃（视频继续录制）"
                            );
                            audio_done = true;
                            audio_failed = true;
                        } else {
                            // 聚合指标（audio_chunks/audio_filler_blocks）由音频线程按
                            // 每源之和发布——这里不再自增，避免与每源口径打架。
                            enc_metrics
                                .audio_last_pts_100ns
                                .store(audio_end, Ordering::Relaxed);
                            audio_written_end = audio_end;
                        }
                    }
                    Step::VideoClosed => break,
                    Step::AudioClosed => {
                        tracing::info!("音频通道已关闭，写入线程转为纯视频");
                        audio_done = true;
                    }
                }
            }
            // ---- 收尾：把音频轨补齐到最后一帧视频的结束点 ----
            // 静音补位只补到「现在 − SILENCE_GUARD_MS」，音频轨尾部因此天然比视频轨
            // 短一个安全边距。
            // 用静音填上缺口，让"有声"与"静音"两种情况都回到 ~0。
            if !audio_failed {
                let padded = pad_audio_tail(
                    &mut *encoder,
                    tail_fmt,
                    &enc_metrics,
                    &mut last_audio_pts,
                    audio_written_end,
                    video_submitted_end,
                );
                if padded > 0 {
                    tracing::info!(
                        padded_ms = padded / 10_000,
                        gap_ms = (video_submitted_end - audio_written_end) / 10_000,
                        "收尾：用静音把音频轨补齐到视频结束点"
                    );
                }
            }
            encoder.flush()?;
            encoder.finish()
        })
        .map_err(|e| MediaError::Internal(format!("创建编码线程失败：{}", e)))?;

    *shared.state.lock().unwrap() = EngineState::Recording;
    tracing::info!("状态 → Recording，进入 30Hz tick 循环");

    // ---- 30Hz tick 循环 ----
    let tick_result = tick_loop(
        cfg,
        shared,
        cmd_rx,
        &slot,
        &capture,
        &tx,
        &drop_rx,
        t0,
        capture_epoch,
        (w, h),
        first.nv12,
    );
    // ---- 停止流程----
    // 注意：tick 结果先存起来，必须走完关闭采集 → drain → finalize 之后才传播错误，
    // 否则出错时会留下未 finalize 的 .partial 文件。
    //
    // 2026-09-22 补：整段停止流程套一个总预算。此前用的是无界 `join`，
    // 一旦子线程卡住，用户点"停止"后要等几分钟才返回——而那恰好发生在录制已经出错、
    // 用户最需要明确反馈的时刻。现在无论谁卡住，整个停止流程都在预算内返回。
    let stop_deadline = Instant::now() + Duration::from_millis(STOP_TOTAL_TIMEOUT_MS);
    // 1. 停止接收新帧（tick 循环已退出）
    // 2. 关闭采集会话
    capture.stop();
    // 2.5 停掉音频线程（先让音频来源停下，再关写入通道，顺序与视频一致）
    audio_stop.store(true, Ordering::Relaxed);
    if let Some(h) = audio_handle {
        // 音频线程退不出来不该阻止视频收尾：记录后继续走编码线程
        if let Err(e) = join_with_deadline(h, stop_deadline, StopPhase::AudioJoin) {
            tracing::error!(error = %e, "音频线程未在预算内退出，继续收尾视频（不影响视频文件）");
        }
    }
    // 3. 通知编码线程 drain + finalize（关闭通道即让它消费完剩余项后退出）
    drop(tx);
    drop(drop_rx);

    let finalize = match join_with_deadline(encoder_handle, stop_deadline, StopPhase::EncoderJoin) {
        Ok(Ok(outcome)) => outcome,
        Ok(Err(e)) => {
            return match tick_result {
                Ok(_) => Err(e),
                Err(tick_err) => Err(tick_err),
            }
        }
        Err(stop_err) => {
            // 收尾没能在预算内完成：文件可能不完整（.partial 仍在），必须明确报出来。
            // 采集侧的错误更接近根因，优先报它。
            return match tick_result {
                Ok(_) => Err(stop_err),
                Err(tick_err) => Err(tick_err),
            };
        }
    };

    let stop_reason = tick_result?;

    tracing::info!(
        reason = stop_reason.as_str(),
        frames_scheduled = shared.metrics.frames_scheduled.load(Ordering::Relaxed),
        frames_encoded = shared.metrics.frames_encoded.load(Ordering::Relaxed),
        "tick 循环已退出"
    );

    let m = shared.metrics.as_ref();
    let elapsed_100ns = m.elapsed_100ns.load(Ordering::Relaxed) as i64;
    Ok(RecorderOutcome {
        elapsed_ms: elapsed_100ns / 10_000,
        frames_scheduled: m.frames_scheduled.load(Ordering::Relaxed),
        frames_duplicated: m.frames_duplicated.load(Ordering::Relaxed),
        frames_dropped_backpressure: m.frames_dropped_backpressure.load(Ordering::Relaxed),
        frames_dropped_startup: m.frames_dropped_startup.load(Ordering::Relaxed),
        audio_dropped_backwards: m.audio_dropped_backwards.load(Ordering::Relaxed),
        capture_idle_checks: m.capture_idle_checks.load(Ordering::Relaxed),
        capture_idle_ms_max: m.capture_idle_ms_max.load(Ordering::Relaxed),
        audio_peak: m.audio_peak.load(Ordering::Relaxed),
        audio_sources: shared
            .metrics
            .audio_sources
            .lock()
            .unwrap()
            .iter()
            .map(|s| AudioSourceStatus {
                kind: s.kind.clone(),
                endpoint: s.endpoint.clone(),
                chunks: s.metrics.chunks,
                filler_blocks: s.metrics.filler_blocks,
                real_blocks: s.metrics.real_blocks(),
                peak: s.metrics.peak,
                peak_last_sec: s.metrics.peak_last_sec,
                dropped_backwards: s.metrics.dropped_backwards,
            })
            .collect(),
        audio_peak_1s: m.audio_peak_1s.load(Ordering::Relaxed),
        audio_peak_last_sec: m.audio_peak_last_sec.load(Ordering::Relaxed),
        audio_filler_blocks: m.audio_filler_blocks.load(Ordering::Relaxed),
        audio_chunks: m.audio_chunks.load(Ordering::Relaxed),
        frames_captured: m.frames_captured.load(Ordering::Relaxed),
        timestamp_anomalies: m.timestamp_anomalies.load(Ordering::Relaxed),
        video_last_pts_100ns: m.video_last_pts_100ns.load(Ordering::Relaxed),
        audio_last_pts_100ns: m.audio_last_pts_100ns.load(Ordering::Relaxed),
        configured: shared
            .actual
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| ConfigureOutcome {
                width: w,
                height: h,
                fps: cfg.fps,
                bitrate: encoder_cfg.bitrate,
                profile: "?".into(),
                hardware_requested: false,
                hardware_mft_found: false,
                encoder_params_applied: false,
                notes: vec![],
            }),
        finalize,
        stop_reason,
    })
}

#[allow(clippy::too_many_arguments)]
fn tick_loop(
    cfg: &RecordingConfig,
    shared: &Arc<Shared>,
    cmd_rx: &Receiver<Command>,
    slot: &Arc<Mutex<Option<crate::capture::WgcFrame>>>,
    capture: &WgcCapture,
    tx: &Sender<TickItem>,
    drop_rx: &Receiver<TickItem>,
    t0: Srt100ns,
    start: Instant,
    (w, h): (u32, u32),
    first: Nv12Image,
) -> MediaResult<StopReason> {
    let metrics = shared.metrics.as_ref();
    let mut timeline = Timeline::new(t0);
    let mut last_frame: Arc<Nv12Image> = Arc::new(first);
    let fps = cfg.fps;
    // 启动瞬态窗口按时间定义（不按 slot 数），否则检查点会随帧率漂移；
    // 窗口内的丢帧单独计入 frames_dropped_startup，让"丢帧"只反映稳态。
    let startup_slots = fps as u64 * STARTUP_WINDOW_SECS;
    let mut n: u64 = 0;
    let mut discontinuity = false;
    let mut last_stall_check = Instant::now();
    let mut last_stall_warn = Instant::now();
    //
    let mut last_disk_check = Instant::now();
    let mut last_drop_log = Instant::now();
    let mut reason = StopReason::UserRequested;

    loop {
        if matches!(cmd_rx.try_recv(), Ok(Command::Stop)) {
            break;
        }
        // 时长硬上限：到点自动停止，走同一套正常 finalize 流程（文件不丢）
        if start.elapsed().as_secs() >= cfg.max_duration_secs {
            tracing::info!(
                limit_secs = cfg.max_duration_secs,
                elapsed_ms = start.elapsed().as_millis() as u64,
                "到达录制时长上限，自动停止并 finalize"
            );
            reason = StopReason::DurationLimit;
            break;
        }
        // 采集回调致命错误
        if let Some(fatal) = capture.take_fatal() {
            record_elapsed(shared, start);
            return Err(recreate_error(&fatal));
        }
        // 采集"无帧"不是致命错误：
        // 桌面完全静止时 WGC 可以长时间不给回调（静止桌面实测 ~10/s，长时间不动可达 0/s），
        // 而 CFR 输出不依赖输入帧——tick 循环自己补重复帧、时间轴照常推进。
        // 把"无帧"判死等于"用户在看文档时录像被中断"：收益≈0，代价是丢掉整段录制。
        // 真正需要保护的是"采集会话真的死了"——那由上面 `capture.take_fatal()`
        // 这个会话自身的信号负责，不再用"无帧"来代理。
        if last_stall_check.elapsed() >= Duration::from_secs(1) {
            last_stall_check = Instant::now();
            record_elapsed(shared, start);
            // 音频 1 秒窗口滚动（聚合值）：把刚过去那一秒的峰值记账，再清零进入下一秒。
            // 每源自己的 1s 窗口与每源弱信号判别在音频线程里做。
            let last_sec = metrics.audio_peak_1s.swap(0, Ordering::Relaxed);
            metrics
                .audio_peak_last_sec
                .store(last_sec, Ordering::Relaxed);
            let idle = capture.ms_since_last_callback();
            metrics.capture_idle_ms_max.fetch_max(idle, Ordering::Relaxed);
            if idle >= CAPTURE_STALL_WARN_SECS * 1000 {
                metrics.capture_idle_checks.fetch_add(1, Ordering::Relaxed);
                // 限流：桌面静止十分钟不该刷几分钟的日志
                if last_stall_warn.elapsed()
                    >= Duration::from_secs(CAPTURE_STALL_WARN_THROTTLE_SECS)
                {
                    last_stall_warn = Instant::now();
                    tracing::warn!(
                        idle_ms = idle,
                        idle_checks = metrics.capture_idle_checks.load(Ordering::Relaxed),
                        "采集回调中断（桌面静止属正常：CFR 会自动补重复帧，录制继续）"
                    );
                }
            }
        }

        // 磁盘空间兜底检查（每 2 秒）：低于硬地板就优雅停止，保住已录内容
        if last_disk_check.elapsed() >= Duration::from_secs(2) {
            last_disk_check = Instant::now();
            if crate::disk::below_floor(&cfg.output_dir) {
                tracing::warn!(
                    floor = %crate::disk::human(crate::disk::disk_floor_bytes()),
                    "磁盘可用空间跌破硬地板，提前停止并 finalize 以保住已录内容"
                );
                reason = StopReason::DiskSpaceLow;
                break;
            }
        }

        let deadline_ns = slot_deadline_ns(n, fps);
        let now_ns = start.elapsed().as_nanos() as u64;
        if now_ns < deadline_ns {
            let sleep_ns = (deadline_ns - now_ns).min(5_000_000); // 最多睡 5ms，保证 Stop 响应
            std::thread::sleep(Duration::from_nanos(sleep_ns));
            continue;
        }
        // tick 落后超过 1 个帧间隔 → 记为 slots_late
        let slot_ns = 1_000_000_000u64 / fps as u64;
        if now_ns > deadline_ns + slot_ns {
            metrics.slots_late.fetch_add(1, Ordering::Relaxed);
        }

        let pts = slot_time_mf(MfTime100ns(0), n, fps);
        let duration = slot_duration_mf(n, fps);

        // 取「捕获时间 ≤ slot 时间」的最新帧；没有则重复上一帧
        let candidate = slot.lock().unwrap().take();
        let (frame, duplicate) = match candidate {
            Some(f) => {
                if f.nv12.width != w || f.nv12.height != h {
                    // 分辨率中途变化：V0.1 不重建流水线
                    return Err(MediaError::Internal(format!(
                        "录制中分辨率从 {}x{} 变为 {}x{}，V0.1 不支持，已停止（文件保持可用）",
                        w, h, f.nv12.width, f.nv12.height
                    )));
                }
                let mf = timeline.to_mf(f.srt);
                // 「不使用未来帧」守卫：SRT 是合成器渲染时刻，投递有抖动，
                // 因此容忍 2 个帧间隔；超出则说明时钟锚定有问题，必须可观测。
                if mf.0 > pts.0 + duration.0 * 2 {
                    metrics.frames_rejected_future.fetch_add(1, Ordering::Relaxed);
                    metrics.frames_duplicated.fetch_add(1, Ordering::Relaxed);
                    (last_frame.clone(), true)
                } else {
                    let arc = Arc::new(f.nv12);
                    last_frame = arc.clone();
                    (arc, false)
                }
            }
            None => {
                metrics.frames_duplicated.fetch_add(1, Ordering::Relaxed);
                (last_frame.clone(), true)
            }
        };

        let item = TickItem {
            nv12: frame,
            pts,
            duration,
            sequence: n,
            duplicate,
            discontinuity,
        };
        // 复位：这个标记只属于"紧跟丢帧之后的那一帧"。永久为真会埋一个将来
        // 会撒谎的标记（一旦用它做"丢帧后强制关键帧"或"记录时间轴断层"就会误导）。
        discontinuity = false;
        match tx.try_send(item) {
            Ok(()) => {}
            // 队列满 → 丢弃最旧的一帧，
            // 再把这帧重发进刚刚腾出的位置：每次只丢 1 帧，且队列里始终是最新的
            // capacity 帧。绝不丢最新——那会让队列持续积压、延迟越滚越大，
            // 是 明确否掉的策略。
            Err(crossbeam_channel::TrySendError::Full(item)) => {
                let startup = n < startup_slots;
                if drop_rx.try_recv().is_ok() {
                    count_drop(&metrics, startup);
                }
                // 单生产者 + 刚腾出一个位置 → 这次发送必然成功；
                // 万一仍失败，把这帧如实计入丢帧，不做静默失败。
                if tx.try_send(item).is_err() {
                    count_drop(&metrics, startup);
                }
                discontinuity = true;

                // 丢帧要可诊断，不能只留一个计数：限流记录（每秒最多一条），
                // 带上队列深度、编码器平均提交耗时、音频轨/视频轨各自的时间位置——
                // 由此可区分「瞬时抖动」「编码/磁盘持续跟不上」与
                // 「混流器在等音频轨」（后者的特征是：视频时间戳猛涨、
                // audio_last_pts_ms 不动或严重落后）。
                if last_drop_log.elapsed() >= Duration::from_secs(1) {
                    last_drop_log = Instant::now();
                    let total = metrics.frames_dropped_backpressure.load(Ordering::Relaxed);
                    let submits = metrics.submit_count.load(Ordering::Relaxed).max(1);
                    let avg_us = metrics.submit_ms_x100_sum.load(Ordering::Relaxed) * 10 / submits;
                    let audio_last = metrics.audio_last_pts_100ns.load(Ordering::Relaxed);
                    let video_last = metrics.video_last_pts_100ns.load(Ordering::Relaxed);
                    tracing::warn!(
                        total_dropped = total,
                        dropped_startup = metrics
                            .frames_dropped_startup
                            .load(Ordering::Relaxed),
                        slot_index = n,
                        fps = fps,
                        queue_capacity = queue_capacity_for(fps),
                        queue_depth = tx.len(),
                        encoder_avg_submit_ms = avg_us as f64 / 1000.0,
                        frame_budget_ms = 1000.0 / fps as f64,
                        audio_chunks = metrics.audio_chunks.load(Ordering::Relaxed),
                        audio_last_pts_ms = audio_last / 10_000,
                        video_last_pts_ms = video_last / 10_000,
                        audio_backwards_dropped = metrics
                            .audio_dropped_backwards
                            .load(Ordering::Relaxed),
                        "队列满：丢弃最旧一帧（真实时间戳保留，时间轴不伪造）"
                    );
                }
            }
            // 编码线程已退出（停止流程中）：后续 tick 会因队列永久满而丢帧，
            // 停止时序由 tick_loop 的既有时序负责，这里不额外处理。
            Err(crossbeam_channel::TrySendError::Disconnected(_)) => {}
        }
        metrics.frames_scheduled.fetch_add(1, Ordering::Relaxed);
        n += 1;

        // 进度埋点：每 150 个 slot（约 5 秒）记录一次，便于定位"看起来卡住"的场景
        if n % 150 == 0 {
            tracing::info!(
                slot = n,
                scheduled = metrics.frames_scheduled.load(Ordering::Relaxed),
                encoded = metrics.frames_encoded.load(Ordering::Relaxed),
                duplicated = metrics.frames_duplicated.load(Ordering::Relaxed),
                dropped = metrics.frames_dropped_backpressure.load(Ordering::Relaxed),
                dropped_startup = metrics.frames_dropped_startup.load(Ordering::Relaxed),
                audio_chunks = metrics.audio_chunks.load(Ordering::Relaxed),
                audio_filler_blocks = metrics.audio_filler_blocks.load(Ordering::Relaxed),
                audio_backwards_dropped = metrics
                    .audio_dropped_backwards
                    .load(Ordering::Relaxed),
                capture_idle_max_ms = metrics.capture_idle_ms_max.load(Ordering::Relaxed),
                late = metrics.slots_late.load(Ordering::Relaxed),
                lag_ms = now_ns.saturating_sub(deadline_ns) / 1_000_000,
                "tick 进度"
            );
        }
    }

    record_elapsed(shared, start);
    Ok(reason)
}

/// 把已流逝时长写入共享状态（供 UI 显示；权威时长以最后一帧 SRT 为准）。
fn record_elapsed(shared: &Arc<Shared>, start: Instant) {
    shared
        .metrics
        .elapsed_100ns
        .store(start.elapsed().as_nanos() as u64 / 100, Ordering::Relaxed);
}

/// 输出目录准备：不存在则创建；无写权限 → `OutputDirectoryNotWritable`
/// （在 Preparing 阶段就报错，不要等到录制中途）。
fn ensure_output_dir(dir: &Path) -> MediaResult<()> {
    std::fs::create_dir_all(dir).map_err(|e| MediaError::OutputDirectoryNotWritable {
        path: format!("{}（{}）", dir.display(), e),
    })?;
    let probe = dir.join(".screenlite-write-test");
    std::fs::write(&probe, b"ok").map_err(|e| MediaError::OutputDirectoryNotWritable {
        path: format!("{}（{}）", dir.display(), e),
    })?;
    let _ = std::fs::remove_file(&probe);
    Ok(())
}

fn unique_output_paths(dir: &Path) -> MediaResult<(PathBuf, PathBuf)> {
    ensure_output_dir(dir)?;
    let stamp = local_timestamp();
    for i in 0..1000 {
        let suffix = if i == 0 {
            String::new()
        } else {
            format!("_{}", i)
        };
        let final_path = dir.join(format!("ScreenLite_{}{}.mp4", stamp, suffix));
        let partial_path = dir.join(format!("ScreenLite_{}{}.mp4.partial", stamp, suffix));
        if !final_path.exists() && !partial_path.exists() {
            return Ok((partial_path, final_path));
        }
    }
    Err(MediaError::Internal("无法生成唯一输出文件名".into()))
}

/// 本地时间戳 `YYYY-MM-DD_HH-MM-SS`（文件名与日志文件名共用）。
pub fn local_timestamp() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    // GetLocalTime 在 crate 里按值返回（不是 out 参数）
    let st = unsafe { GetLocalTime() };
    format!(
        "{:04}-{:02}-{:02}_{:02}-{:02}-{:02}",
        st.wYear, st.wMonth, st.wDay, st.wHour, st.wMinute, st.wSecond
    )
}
