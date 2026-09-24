//! 音频源契约（trait + 端点描述）。本文件只有契约，不含任何 Win32 代码。
//!
//! ## 设计 framing：按「端点 + 方向」，而不是按「默认设备」
//!
//! loopback 是渲染端点上的采集，麦克风是采集端点上的采集——两者的共性是
//! "打开一个音频端点，产出带时间戳的 PCM"。所以抽象按端点做：`id = None` 的语义是
//! "系统默认"。将来接上设备选择，只是把 `Some(id)` 传进来，trait 形状不变。
//!
//! ## 两条契约（写在 trait 上，review 阶段就能挡住）
//!
//! 1. 时间戳域：`AudioChunk.srt` 必须是 SystemRelativeTime（100ns，由 QPC 经
//! [`crate::time::srt_from_qpc`] 换算），不是"某设备自己的时钟"。loopback 与麦克风都取
//! `IAudioCaptureClient::GetBuffer` 的 QPC 位置，天然同域。
//! 这条契约是用来挡住"自带时钟的虚拟音频设备"的——那类设备一接进来就会破坏 A/V 同步，
//! 必须在 review 阶段发现，而不是等用户报"音画不同步"。
//! 2. 块网格：混音器只做"等长求和 + 限幅"，所以每个源必须按同一网格
//! （[`crate::consts::SILENCE_CHUNK_MS`]）产出——源没有数据时也要产出等长的静音块。
//! 网格与时间轴推进的实现在同一处（今天在引擎的音频线程里；源变多时挪进每个源的
//! GridAdapter），不会因为接了第二个源而重写。
//!
//! ## 设备缺失的降级（现在就定，避免以后变成产品辩论）
//!
//! 打不开端点 → 音频关 + WARN + 标记，绝不失败录制（与"音频失败不中断视频"同一条规则）。
//! 将来接上设备选择后沿用同一策略：用户选的设备不在了 → 同样降级。

use crate::error::MediaResult;
use crate::time::Srt100ns;

use super::{AudioChunk, AudioFormat};

/// 端点的数据流方向。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AudioDirection {
    /// 渲染端点（扬声器 / 耳机）。loopback 从这里"采集正在播放的声音"。
    Render,
    /// 采集端点（麦克风 / 线路输入）。
    Capture,
}

/// 一个可用的音频端点（枚举结果）。
#[derive(Debug, Clone)]
pub struct AudioEndpoint {
    pub id: String,
    pub name: String,
    pub direction: AudioDirection,
    pub is_default: bool,
}

/// 要录制哪一路音频。
///
/// V0.2 只支持一路；`Vec<Box<dyn AudioSource>>` 的混音点已经留好（见 [`super::mixer`]），
/// 加第二路时只是往那个 Vec 里再 push 一个源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioSourceKind {
    /// 系统声音（渲染端点 + loopback）。
    #[default]
    SystemLoopback,
    /// 麦克风（采集端点）。
    Microphone,
}

impl AudioSourceKind {
    pub fn direction(self) -> AudioDirection {
        match self {
            AudioSourceKind::SystemLoopback => AudioDirection::Render,
            AudioSourceKind::Microphone => AudioDirection::Capture,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            AudioSourceKind::SystemLoopback => "system-loopback",
            AudioSourceKind::Microphone => "microphone",
        }
    }
}

#[derive(Debug, Clone)]
pub struct EndpointSpec {
    pub direction: AudioDirection,
    /// `None` = 系统默认端点（V0.2 只用这个）。将来加设备选择时传 `Some(id)`。
    pub id: Option<String>,
}

impl EndpointSpec {
    /// 系统默认端点。
    pub fn default_of(direction: AudioDirection) -> Self {
        Self {
            direction,
            id: None,
        }
    }

    /// 按端点 id 显式打开（V0.2 尚未接线，接口先留好）。
    pub fn by_id(direction: AudioDirection, id: impl Into<String>) -> Self {
        Self {
            direction,
            id: Some(id.into()),
        }
    }
}

/// 一个音频源：打开一个音频端点，产出带 SystemRelativeTime 时间戳的 PCM。
///
/// 契约见本文件头部（时间戳域 + 块网格）。
pub trait AudioSource: Send {
    fn start(spec: &EndpointSpec) -> MediaResult<Self>
    where
        Self: Sized;

    /// 端点的实际混音格式（采样率 / 声道 / 样本类型）。
    fn format(&self) -> AudioFormat;

    /// 取一块数据；没有数据时返回 `None`（非阻塞，调用方自行 sleep）。
    fn poll(&self) -> MediaResult<Option<AudioChunk>>;

    /// 当前时刻的系统相对时间（100ns，QPC 派生）——静音补位与网格推进用。
    fn srt_now(&self) -> MediaResult<Srt100ns>;

    /// 停止采样（不释放；释放交给 Drop）。
    fn stop(&self);

    /// 实际打开的端点描述 + 协商到的格式（供一行 INFO 日志：
    /// "麦克风没声音"时要能一眼分清是选错设备还是设备对了但没数据）。
    fn opened(&self) -> String;
}
