//! 集中常量。技术基线 要求：禁止把阈值散落在代码里。

/// Media Foundation 时间单位：100 纳秒。
pub const MF_TICKS_PER_SEC: i64 = 10_000_000;

/// 默认输出帧率。定义的是输出时间轴（CFR），不是采集触发频率。
pub const DEFAULT_FPS: u32 = 30;

/// 调度器 → 编码器 的有界队列容量（≈267ms @30fps）。
/// 这是 30fps 的基准值；实际容量必须按帧率换算，见 [`queue_capacity_for`]。
pub const QUEUE_CAPACITY: usize = 8;

/// 队列要缓冲的时间（秒）。按时间而不是帧数定容量，
/// 否则 60fps 下同样的 8 帧只覆盖 133ms，编码器一抖动就丢帧（实测 60fps 丢 3 帧、时长差 50ms）。
pub const QUEUE_CUSHION_SECS: f64 = 0.267;

/// 队列容量上限，防止极端帧率下占用过多内存（每帧 NV12 ≈ 6MB @2560x1600）。
pub const QUEUE_CAPACITY_MAX: usize = 40;

/// 按帧率换算队列容量，保持恒定的时间缓冲（最少 8 帧）。
pub fn queue_capacity_for(fps: u32) -> usize {
    let n = (fps as f64 * QUEUE_CUSHION_SECS).round() as usize;
    n.clamp(QUEUE_CAPACITY, QUEUE_CAPACITY_MAX)
}

/// WGC 帧池缓冲区数量。
pub const FRAME_POOL_BUFFERS: i32 = 2;

/// 开始采集后多久没有任何帧 → CaptureNoFrames。
pub const FIRST_FRAME_TIMEOUT_SECS: u64 = 5;

/// 连续多久没有 `FrameArrived` 回调 → 记 WARN（只告警，不算致命错误）。
///
/// 桌面完全静止时 WGC 会给不出回调（实测可从 ~48/s 掉到 0/s），而 CFR 输出不依赖输入帧：
/// tick 循环自己补重复帧、时间轴照常推进。所以"无帧"不等于采集会话死了——后者由
/// `capture.take_fatal()` 单独负责，拿无帧来代理只会白白丢掉整段录像。
pub const CAPTURE_STALL_WARN_SECS: u64 = 3;

/// 上面那条 WARN 的限流间隔（秒）：桌面静止十分钟不该刷几分钟的日志。
pub const CAPTURE_STALL_WARN_THROTTLE_SECS: u64 = 5;

/// 静音补位块的长度（毫秒）。
///
/// 桌面没有声音播放时 WASAPI loopback 不出数据包，但音频轨必须持续前进
/// （否则 MF 混流器会等音频轨交错、把视频写入阻塞住）。20ms 是个折中：
/// 每块 960 帧 @48k，约 50 块/秒，既不碎（提交次数可控）也不过粗。
pub const SILENCE_CHUNK_MS: i64 = 20;

/// 静音补位的安全边距（毫秒）：只补到「现在 − 这个值」。
///
/// 真实数据包的起始时间总比"现在"早一点，留出这个边距可以保证它永远排在
/// 已写入的静音之后，不会出现音频时间戳倒退。代价是音频轨最多落后视频轨
/// 100ms（远小于混流器的缓冲容忍度，也远小于一帧的观感阈值）。
pub const SILENCE_GUARD_MS: i64 = 100;

/// 启动瞬态窗口：开始录制后的这段时间内发生的丢帧单独计数。
///
/// 实测：丢帧全部集中在开始后的前 ~1.5 秒——编码器 MFT 首次协商、
/// 首个关键帧、队列被瞬间填满都在这段时间里。把这段的丢帧混进
/// `frames_dropped_backpressure`，会让"录 15 秒丢 6 帧"被误读成"持续跟不上"，
/// 而稳态其实是 0。看到的数字必须只反映稳态。
///
/// 用时间而不是 slot 数来定义窗口：否则 90fps 下同样 2 秒会变成 180 个 slot，
/// 检查点会随帧率漂移。
pub const STARTUP_WINDOW_SECS: u64 = 2;

/// 收尾静音补位的上限（毫秒）。
///
/// 视频异常早停、而音频轨已经写得很长时，无上限补位会一次性灌入大量静音
/// （把文件尾部撑成一段假静音）。1 秒足够覆盖正常的安全边距（100ms）与调度抖动；
/// 异常情况下宁可让音频轨短一点。
pub const AUDIO_TAIL_PAD_MAX_MS: i64 = 1000;

/// 停止流程的总兜底超时（毫秒）。
///
/// 分段超时曾经形同虚设：停止流程用的是 `JoinHandle::join()`，而 join 是无界的——
/// 一次采集会话卡住后，"停止"过了约 4.5 分钟才返回。
/// 现在改成总预算：子步骤超时就 detach 线程（进程仍能退出），报 `StopTimeout`，
/// 并说明文件可能不完整。
pub const STOP_TOTAL_TIMEOUT_MS: u64 = 20_000;

/// 输入信号"极弱"的判定阈值（`|sample|` 峰值）。
///
/// 100 / 32768 ≈ 0.3% ≈ -50 dBFS。安静房间的麦克风底噪、或系统音量被压得很低，
/// 都会落在这条线以下——此时"录了但没声音"的责任在设备侧，不在录制链路，
/// 所以提示的措辞必须指向系统音量/麦克风增益，而不是"我们的链路有问题"。
pub const AUDIO_WEAK_PEAK: u64 = 100;

/// 连续多少秒"最近 1 秒峰值 < [`AUDIO_WEAK_PEAK`]"才提示。
///
/// 为什么不是立刻提示：录制中途完全可能有正常的安静段（没人说话、音乐间隙），
/// 5 秒的持续性判断能避开它们。
pub const AUDIO_WEAK_SECS: u64 = 5;

/// 弱信号提示的再提示间隔（秒）：信号一直弱时不要刷屏，但也不要在长录里只提醒一次。
pub const AUDIO_WEAK_REWARN_SECS: u64 = 60;

/// 停止流程中单个 join 的预算（音频线程 / 编码线程各一份）。
/// 实际等待取 `min(本值, 总预算剩余)`，所以多段之和不会超过 [`STOP_TOTAL_TIMEOUT_MS`]。
pub const STOP_JOIN_TIMEOUT_MS: u64 = 15_000;

/// 关键帧间隔（秒）。
pub const GOP_SECONDS: u32 = 2;

/// 进度事件节流。
pub const PROGRESS_EVENT_HZ: u32 = 4;

/// 录制时长硬上限：2 小时。 达到上限时自动停止并正常 finalize（不丢文件）。
pub const MAX_RECORDING_SECS: u64 = 2 * 60 * 60;

/// 允许的帧率白名单：30 / 60 / 90。 非白名单值一律拒绝。
///
/// 为什么是白名单而不是"上限 N"：让时间轴、队列容量、丢帧策略都只有有限种确定取值。
/// 为什么最高只到 90：实测 `2560×1600 @144fps`（5.9 亿像素/秒）会被硬件 H.264 编码器
/// 直接拒绝（`0xC00D36B4`），`@120fps`（4.9 亿）可用但已贴近上限。90fps 在 2K 下
/// （3.7 亿像素/秒）留有充分余量，避免"UI 里选得了、点录制才失败"。
pub const ALLOWED_FPS: [u32; 3] = [30, 60, 90];

pub fn is_allowed_fps(fps: u32) -> bool {
    ALLOWED_FPS.contains(&fps)
}

/// 采集分辨率硬上限（2K 级）：长边 ≤ 2560 且高 ≤ 1600。
/// 该上限允许常见的 2560×1600 / 2560×1440 / 1920×1080 原生录制，
/// 超过则拒绝启动（V0.1 不做缩放，避免引入额外的缩放环节与画质问题）。
pub const MAX_CAPTURE_WIDTH: u32 = 2560;
pub const MAX_CAPTURE_HEIGHT: u32 = 1600;

/// 降级告警阈值：10 秒窗口内丢帧率超过该值 → degraded。
pub const DEGRADED_DROP_RATIO: f64 = 0.05;

/// 默认码率：`clamp(w*h*fps*0.10, 4Mbps, 20Mbps)` → 1080p30 约 6.2 Mbps。
pub fn default_bitrate(width: u32, height: u32, fps: u32) -> u32 {
    let raw = (width as u64) * (height as u64) * (fps as u64) / 10;
    raw.clamp(4_000_000, 20_000_000) as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bitrate_1080p30_is_about_6_2_mbps() {
        let b = default_bitrate(1920, 1080, 30);
        assert_eq!(b, 6_220_800);
    }

    #[test]
    fn bitrate_clamps_low_and_high() {
        assert_eq!(default_bitrate(320, 240, 15), 4_000_000);
        assert_eq!(default_bitrate(7680, 4320, 60), 20_000_000);
    }

    #[test]
    fn fps_allowlist_only_accepts_30_60_90() {
        for fps in [30u32, 60, 90] {
            assert!(is_allowed_fps(fps), "{} 应在白名单内", fps);
        }
        // 非白名单一律拒绝（含曾被考虑过的 120/144，以及 123、200 等）
        for fps in [0u32, 15, 24, 25, 29, 50, 120, 123, 144, 165, 200, 240] {
            assert!(!is_allowed_fps(fps), "{} 不应在白名单内", fps);
        }
    }

    #[test]
    fn queue_capacity_keeps_a_constant_time_cushion() {
        // 30fps 保持基准 8 帧（≈267ms）
        assert_eq!(queue_capacity_for(30), 8);
        // 更高帧率必须换来更大的帧数，才能维持同样的时间缓冲
        for fps in [60u32, 90, 120, 144] {
            let cap = queue_capacity_for(fps);
            let cushion_ms = cap as f64 / fps as f64 * 1000.0;
            assert!(
                cushion_ms >= 250.0,
                "{}fps 的队列只缓冲了 {:.0}ms（应 ≥250ms）",
                fps,
                cushion_ms
            );
            assert!(cap <= QUEUE_CAPACITY_MAX);
        }
        // 30fps 与 144fps 的时间缓冲应大致相当（差异 < 30%）
        let c30 = queue_capacity_for(30) as f64 / 30.0;
        let c144 = queue_capacity_for(144) as f64 / 144.0;
        assert!((c30 - c144).abs() / c30 < 0.3);
    }

    #[test]
    fn resolution_cap_allows_native_2560x1600_but_rejects_4k() {        let fits = |w: u32, h: u32| w <= MAX_CAPTURE_WIDTH && h <= MAX_CAPTURE_HEIGHT;
        assert!(fits(2560, 1600), "本机原生 2560x1600 必须允许");
        assert!(fits(2560, 1440));
        assert!(fits(1920, 1080));
        assert!(!fits(3840, 2160), "4K 必须被拒绝");
        assert!(!fits(3440, 1440), "超宽屏超过长边上限，应拒绝");
    }
}
