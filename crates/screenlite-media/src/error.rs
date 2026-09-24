//! 统一错误分类。禁止 silent failure：每个错误都要能给出码 + 描述 + 日志位置。

/// 停止流程的阶段，用于 `StopTimeout`。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopPhase {
    Drain,
    Flush,
    Finalize,
    Release,
    /// 等待音频采集线程退出（`join` 超时）。
    AudioJoin,
    /// 等待编码/写入线程退出（`join` 超时）——它内部串着 drain → flush → finalize。
    EncoderJoin,
}

impl std::fmt::Display for StopPhase {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = match self {
            StopPhase::Drain => "drain",
            StopPhase::Flush => "flush",
            StopPhase::Finalize => "finalize",
            StopPhase::Release => "release",
            StopPhase::AudioJoin => "audio-join",
            StopPhase::EncoderJoin => "encoder-join",
        };
        f.write_str(s)
    }
}

#[derive(Debug, thiserror::Error)]
pub enum MediaError {
    #[error("当前系统不支持屏幕捕获（GraphicsCaptureSession.IsSupported() == false）")]
    ScreenCaptureUnsupported,

    #[error("捕获帧格式不受支持：期望 BGRA8，实际 {actual}")]
    CaptureFormatUnsupported { actual: String },

    #[error("目标显示器处于 HDR / 高级颜色模式，V0.1 暂不支持。请在显示设置中关闭 HDR 后重试")]
    HdrDisplayNotSupported,

    #[error("不支持的帧率：{fps}（仅允许 {allowed:?}）")]
    UnsupportedFrameRate { fps: u32, allowed: &'static [u32] },

    #[error("采集分辨率 {width}x{height} 超过上限 {max_width}x{max_height}")]
    ResolutionExceedsLimit {
        width: u32,
        height: u32,
        max_width: u32,
        max_height: u32,
    },

    #[error("显示器不存在或已断开：{display_id}")]
    DisplayNotFound { display_id: String },

    #[error("创建 WGC 帧池失败：{0}")]
    FramePoolCreateFailed(#[source] windows::core::Error),

    #[error("启动捕获会话失败：{0}")]
    CaptureSessionStartFailed(#[source] windows::core::Error),

    #[error("开始采集后 {0} 秒内没有收到任何帧")]
    CaptureNoFrames(u64),

    #[error("采集中断：{0} 秒没有帧回调")]
    CaptureStalled(u64),

    #[error("帧格式转换失败：{reason}")]
    FrameConversionFailed { reason: String },

    #[error("编码器枚举失败：{0}")]
    EncoderEnumerationFailed(#[source] windows::core::Error),

    #[error("编码器初始化失败（阶段：{stage}）：{source}")]
    EncoderInitFailed {
        stage: &'static str,
        #[source]
        source: windows::core::Error,
    },

    #[error("硬件编码不可用，已回退软件编码：{reason}")]
    EncoderHardwareFallback { reason: String },

    #[error("输出目录不可写：{path}")]
    OutputDirectoryNotWritable { path: String },

    #[error("磁盘可用空间不足：可用 {free_human}，本次录制预计需要 {required_human}")]
    InsufficientDiskSpace {
        free: u64,
        required: u64,
        free_human: String,
        required_human: String,
    },

    #[error("封装初始化失败：{0}")]
    MuxerInitFailed(#[source] windows::core::Error),

    #[error("MP4 finalize 失败：{0}")]
    Mp4FinalizeFailed(#[source] windows::core::Error),

    #[error("停止流程超时（阶段：{phase}）")]
    StopTimeout { phase: StopPhase },

    #[error("媒体线程内部错误：{0}")]
    Internal(String),

    #[error("{context}")]
    Windows {
        context: &'static str,
        #[source]
        source: windows::core::Error,
    },
}

impl MediaError {
    /// 稳定错误码，供 IPC 与日志使用。
    pub fn code(&self) -> &'static str {
        match self {
            MediaError::ScreenCaptureUnsupported => "ScreenCaptureUnsupported",
            MediaError::CaptureFormatUnsupported { .. } => "CaptureFormatUnsupported",
            MediaError::HdrDisplayNotSupported => "HdrDisplayNotSupported",
            MediaError::UnsupportedFrameRate { .. } => "UnsupportedFrameRate",
            MediaError::ResolutionExceedsLimit { .. } => "ResolutionExceedsLimit",
            MediaError::DisplayNotFound { .. } => "DisplayNotFound",
            MediaError::FramePoolCreateFailed(_) => "FramePoolCreateFailed",
            MediaError::CaptureSessionStartFailed(_) => "CaptureSessionStartFailed",
            MediaError::CaptureNoFrames(_) => "CaptureNoFrames",
            MediaError::CaptureStalled(_) => "CaptureStalled",
            MediaError::FrameConversionFailed { .. } => "FrameConversionFailed",
            MediaError::EncoderEnumerationFailed(_) => "EncoderEnumerationFailed",
            MediaError::EncoderInitFailed { .. } => "EncoderInitFailed",
            MediaError::EncoderHardwareFallback { .. } => "EncoderHardwareFallback",
            MediaError::OutputDirectoryNotWritable { .. } => "OutputDirectoryNotWritable",
            MediaError::InsufficientDiskSpace { .. } => "InsufficientDiskSpace",
            MediaError::MuxerInitFailed(_) => "MuxerInitFailed",
            MediaError::Mp4FinalizeFailed(_) => "Mp4FinalizeFailed",
            MediaError::StopTimeout { .. } => "StopTimeout",
            MediaError::Internal(_) => "InternalPanic",
            MediaError::Windows { .. } => "Win32Error",
        }
    }

    pub fn win32(context: &'static str, source: windows::core::Error) -> Self {
        MediaError::Windows { context, source }
    }
}

pub type MediaResult<T> = Result<T, MediaError>;
