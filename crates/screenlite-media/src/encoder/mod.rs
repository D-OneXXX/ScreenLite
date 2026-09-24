//! 编码层抽象。
//!
//! `Backend` 与 `CodecId` 是两个正交维度：D3D12 Video Encoding 是独立的驱动级 API，
//! 不是 Media Foundation 的一个 MFT，因此不能写成「MF 下面挂 H264/HEVC/AV1」。

use std::path::PathBuf;

use crate::convert::Nv12Image;
use crate::error::{MediaError, MediaResult};
use crate::time::MfTime100ns;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EncoderBackend {
    MediaFoundation,
    #[allow(dead_code)] // 后续版本
    D3D12Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CodecId {
    H264,
    #[allow(dead_code)]
    HEVC,
    #[allow(dead_code)]
    AV1,
}

impl CodecId {
    pub fn as_str(&self) -> &'static str {
        match self {
            CodecId::H264 => "H264",
            CodecId::HEVC => "HEVC",
            CodecId::AV1 => "AV1",
        }
    }
}

/// 硬件偏好。注意：`PreferHardware` 不等于「必须硬件编码」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwarePreference {
    PreferHardware,
    /// 诊断开关：验证回退路径真实可用，并证明硬件开关不是装饰。
    PreferSoftware,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HardwareSupport {
    Available,
    Unavailable,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeatureSupport {
    Supported,
    Unsupported,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RateControlMode {
    Cbr,
    PeakConstrainedVbr,
    Unknown,
}

/// 能力矩阵：不能简化为 `codec -> bool`。
#[derive(Debug, Clone)]
pub struct EncoderCapabilities {
    pub backend: EncoderBackend,
    pub codec: CodecId,
    pub hardware: HardwareSupport,
    pub input_formats: Vec<PixelFormat>,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: Option<f32>,
    pub features: EncoderFeatures,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct EncoderFeatures {
    pub bframes: FeatureSupport,
    pub rate_control: Vec<RateControlMode>,
    pub low_latency: FeatureSupport,
    pub max_bitrate: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PixelFormat {
    Nv12,
}

#[derive(Debug, Clone)]
pub struct EncoderConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub gop_seconds: u32,
    pub hardware: HardwarePreference,
    /// 先写 `.partial`，finalize 成功后再改名为 [`EncoderConfig::final_path`]。
    pub partial_path: PathBuf,
    pub final_path: PathBuf,
    /// 音频轨格式（None = 纯视频）。
    ///
    /// 必须是 `Option` 而不是"运行时懒创建"：Sink Writer 的 `AddStream`
    /// 只能在 `BeginWriting()` 之前调用，之后调用会返回 `0xC00D36B2`
    /// 「当前状态的请求无效」（实测）。所以格式必须在建编码器时就确定。
    pub audio: Option<crate::audio::AudioFormat>,
}

/// `configure()` 的返回：实际生效的参数（硬件 MFT 可能协商成不同值）。
#[derive(Debug, Clone, serde::Serialize)]
pub struct ConfigureOutcome {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub bitrate: u32,
    pub profile: String,
    pub hardware_requested: bool,
    /// 是否枚举到硬件 H.264 编码器 MFT（不等于「正在使用硬件编码」）。
    pub hardware_mft_found: bool,
    /// 编码参数是否被接受（被拒时回退默认值）。
    pub encoder_params_applied: bool,
    pub notes: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct FinalizeOutcome {
    pub path: PathBuf,
    pub bytes: u64,
    pub samples: u64,
    pub duration: MfTime100ns,
}

/// V0.1 只有 CPU NV12 一种帧数据；后续 GPU 路径会新增 `GpuTexture` 变体。
///
/// 格式由枚举变体承载（而不是另设一个可能与数据不一致的 `format` 字段）。
/// 用 `Arc` 是为了让「重复帧」可以零拷贝地再次提交（CFR 静态画面会产生大量重复帧）。
pub enum VideoFrameData {
    Cpu(std::sync::Arc<Nv12Image>),
}

pub struct VideoFrame {
    pub data: VideoFrameData,
    pub pts: MfTime100ns,
    pub duration: MfTime100ns,
    pub sequence: u64,
    /// 前方发生过丢帧（时间轴上存在间隙）。
    pub discontinuity: bool,
}

impl VideoFrame {
    pub fn nv12(&self) -> &Nv12Image {
        match &self.data {
            VideoFrameData::Cpu(img) => img,
        }
    }
}
/// 已压缩码流样本。
///
/// V0.1 不使用（V0.1 的 encoder 自己拥有文件）；这是给后续 D3D12 后端预留的边界：
/// D3D12 Video Encode 只能产出码流，无法像 Sink Writer 一样自己写文件。
#[allow(dead_code)]
pub struct CompressedSample {
    pub data: Vec<u8>,
    pub pts: MfTime100ns,
    pub duration: MfTime100ns,
    pub keyframe: bool,
}

#[allow(dead_code)]
pub trait Muxer: Send {
    fn write_sample(&mut self, sample: &CompressedSample) -> MediaResult<()>;
    fn finish(self: Box<Self>) -> MediaResult<FinalizeOutcome>;
}

pub trait VideoEncoder: Send {
    fn capabilities(&self) -> &EncoderCapabilities;
    fn configure_outcome(&self) -> &ConfigureOutcome;
    fn submit(&mut self, frame: &VideoFrame) -> MediaResult<()>;
    fn flush(&mut self) -> MediaResult<()>;
    fn finish(self: Box<Self>) -> MediaResult<FinalizeOutcome>;

    /// 确保音频流已就绪，返回该流的索引（后端内部使用；默认表示不支持音频）。
    ///
    /// 单独拆出来是为了支持懒创建：WASAPI loopback 在无声音播放时不产生数据包，
    /// 提前 AddStream 会留下 0 样本的音频轨。见 `submit_audio` 的说明。
    fn ensure_audio_stream(
        &mut self,
        _format: crate::audio::AudioFormat,
    ) -> MediaResult<u32> {
        Err(MediaError::Internal("该编码后端不支持音频".into()))
    }

    /// 音频轨支持（V0.2）。
    ///
    /// 默认实现表示"本后端不处理音频"。注意：音频失败绝不能中断视频录制——
    /// 调用方收到 Err 时应停掉音频来源并记录，而不是让整段录制失败。
    fn submit_audio(
        &mut self,
        _chunk: &crate::audio::AudioChunk,
        _format: crate::audio::AudioFormat,
        _pts: MfTime100ns,
        _duration: MfTime100ns,
    ) -> MediaResult<()> {
        Err(MediaError::Internal("该编码后端不支持音频".into()))
    }
}

/// 启动前的能力探测入口。
pub mod capabilities;
pub mod mf_h264;

pub use capabilities::probe_h264;
pub use mf_h264::MfSinkWriterEncoder;

/// 编码器工厂：V0.1 只有 Media Foundation 后端。
pub fn create_encoder(
    cfg: &EncoderConfig,
    device_manager: Option<&windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager>,
) -> MediaResult<Box<dyn VideoEncoder>> {
    if cfg.width % 2 != 0 || cfg.height % 2 != 0 {
        return Err(MediaError::FrameConversionFailed {
            reason: format!("NV12 编码需要偶数尺寸，实际 {}x{}", cfg.width, cfg.height),
        });
    }
    let enc = MfSinkWriterEncoder::create(cfg, device_manager)?;
    Ok(Box::new(enc))
}
