//! 音频采集（WASAPI）：系统声音（渲染端点 + loopback）与麦克风（采集端点）。
//!
//! 只做采集 + 时间戳：把端点上的 PCM 抓成块，每块带一个由 WASAPI 的 QPC 位置换算出的
//! 系统相对时间（SRT）。契约见 [`source`]（trait 上写着两条：时间戳域、块网格）。
//!
//! ```text
//! WASAPI 给的是 QPC 位置（pu64QPCPosition）
//! ↓ srt_from_qpc(qpc, QueryPerformanceFrequency()) ← 唯一的显式换算
//! Srt100ns（与视频帧同一个时间域）
//! ↓ Timeline::to_mf（同一个 t0）
//! MfTime100ns → 交给封装器
//! ```
//!
//! 绝不直接拿 QPC 计数值当时间用——两个来源的 tick 必须经过频率换算才能比较。
//!
//! 多路音频的汇合点在 [`mixer`]（`Vec<Box<dyn AudioSource>> → 一条流`）。

pub mod mixer;
pub mod source;

pub use mixer::{
    clamp_gain, sum_blocks, sum_blocks_with_gain, AudioMixer, GridAdapter, SourceMetrics,
};
pub use source::{AudioDirection, AudioEndpoint, AudioSource, AudioSourceKind, EndpointSpec};

use std::ptr;

use windows::Win32::Devices::FunctionDiscovery::PKEY_Device_FriendlyName;
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, DEVICE_STATE_ACTIVE, IAudioCaptureClient, IAudioClient,
    IMMDevice, IMMDeviceEnumerator, AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_LOOPBACK, MMDeviceEnumerator, WAVEFORMATEX,
};
use windows::Win32::System::Com::StructuredStorage::PropVariantToStringAlloc;
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::UI::Shell::PropertiesSystem::IPropertyStore;

use crate::error::{MediaError, MediaResult};
use crate::time::{srt_from_qpc, QpcTicks, Srt100ns};

/// 设备混音格式的采样类型（我们只区分这两种：32 位浮点与 16 位整数）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    Pcm16,
    Float32,
}

#[derive(Debug, Clone, Copy)]
pub struct AudioFormat {
    pub sample_rate: u32,
    pub channels: u16,
    pub format: SampleFormat,
}

impl AudioFormat {
    /// 每帧（所有声道）的字节数。
    pub fn frame_bytes(&self) -> usize {
        self.channels as usize * 2
    }
}

/// 一块已转换为交错 16 位 PCM 的音频。
#[derive(Debug, Clone)]
pub struct AudioChunk {
    pub pcm: Vec<i16>,
    pub frames: u32,
    /// 该块首帧对应的系统相对时间（100ns），与视频帧同一时间域
    pub srt: Srt100ns,
}

impl AudioChunk {
    /// 合成一块静音（采样格式与设备混音格式一致：交错 16 位）。
    ///
    /// 为什么需要它：WASAPI loopback 在桌面没有声音播放时一个数据包都不给
    /// （`GetNextPacketSize` 恒为 0，见本模块测试的说明）。但音频轨一旦
    /// `AddStream`，就必须持续前进——否则 Media Foundation 的 Sink Writer 会
    /// 一直等音频轨交错，把视频的 `WriteSample` 阻塞住。
    ///
    /// 实测（2026-09-22）：桌面静音 + 音频开，30fps 区域录制 15 秒丢帧 191、
    /// 实到仅 5.47fps；同一构建音频关掉即恢复正常（丢帧 3）。
    pub fn silence(format: &AudioFormat, frames: u32, srt: Srt100ns) -> Self {
        Self {
            pcm: vec![0i16; frames as usize * format.channels as usize],
            frames,
            srt,
        }
    }
}

/// PCM 的峰值（`|sample|` 最大值）。
///
/// 为什么需要它："录了但没声音"是最典型的投诉，而它有两个完全不同的根因：
/// ① 我们没接上（链路问题）；② 系统里那个设备本来就是静音（系统音量 0 / 麦克风静音 / 选错设备）。
/// 一行观测就能分清：
/// - 全程 peak == 0 → 进设备的信号本身就是数字静音 → 让用户去查系统音量/静音开关
/// - peak > 0 但没声音 → 问题在播放端或文件端
///
/// 对 loopback 同样有用（用户以为在放声音，其实系统静音）。
pub fn pcm_peak(pcm: &[i16]) -> u64 {
    pcm.iter().map(|s| s.unsigned_abs() as u64).max().unwrap_or(0)
}

/// 某一方向的系统默认端点（用的数据流端点）。
fn dataflow_of(d: AudioDirection) -> windows::Win32::Media::Audio::EDataFlow {
    match d {
        AudioDirection::Render => eRender,
        AudioDirection::Capture => eCapture,
    }
}

/// 端点友好名（拿不到就退化为 id）。
///
/// 为什么要它：日志里出现真名（"麦克风阵列 (Realtek Audio)"）才能一眼分清
/// "麦克风没声音"是选错了设备还是设备对了但没数据。
unsafe fn endpoint_friendly_name(device: &IMMDevice, fallback_id: &str) -> String {
    // STGM(0) = STGM_READ
    let store: IPropertyStore = match device.OpenPropertyStore(STGM(0)) {
        Ok(s) => s,
        Err(_) => return fallback_id.to_string(),
    };
    let pv = match store.GetValue(&PKEY_Device_FriendlyName) {
        Ok(v) => v,
        Err(_) => return fallback_id.to_string(),
    };
    let name = match PropVariantToStringAlloc(&pv) {
        Ok(pw) => {
            let s = String::from_utf16_lossy(pw.as_wide());
            CoTaskMemFree(Some(pw.0 as *const core::ffi::c_void));
            s
        }
        Err(_) => String::new(),
    };
    if name.trim().is_empty() {
        fallback_id.to_string()
    } else {
        name
    }
}

/// 取端点 id（`IMMDevice::GetId` 返回的内存必须自己释放）。
unsafe fn endpoint_id(device: &IMMDevice) -> String {
    match device.GetId() {
        Ok(pw) => {
            let s = String::from_utf16_lossy(pw.as_wide());
            CoTaskMemFree(Some(pw.0 as *const core::ffi::c_void));
            s
        }
        Err(_) => String::new(),
    }
}

/// 枚举某一方向的活动端点。
///
/// V0.2 只用默认端点，但列表能力现在就有：① 每个端点打一行 INFO（诊断"选错设备"）；
/// ② 将来加设备下拉时不需要改 trait（只是把 `Some(id)` 传进 [`EndpointSpec`]）。
pub fn enumerate_endpoints(direction: AudioDirection) -> MediaResult<Vec<AudioEndpoint>> {
    let mut out = Vec::new();
    unsafe {
        let enumerator: IMMDeviceEnumerator =
            CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                .map_err(|e| MediaError::win32("CoCreateInstance(MMDeviceEnumerator)", e))?;
        let default_id = enumerator
            .GetDefaultAudioEndpoint(dataflow_of(direction), eConsole)
            .map(|d| endpoint_id(&d))
            .unwrap_or_default();
        let collection = enumerator
            .EnumAudioEndpoints(dataflow_of(direction), DEVICE_STATE_ACTIVE)
            .map_err(|e| MediaError::win32("EnumAudioEndpoints", e))?;
        let count = collection.GetCount().unwrap_or(0);
        for i in 0..count {
            let Ok(device) = collection.Item(i) else {
                continue;
            };
            let id = endpoint_id(&device);
            if id.is_empty() {
                continue;
            }
            let name = endpoint_friendly_name(&device, &id);
            out.push(AudioEndpoint {
                is_default: id == default_id,
                id,
                name,
                direction,
            });
        }
    }
    Ok(out)
}

/// WASAPI 端点采集：系统声音（渲染端点 + loopback）与麦克风（采集端点）共用这一个实现。
///
/// 两者的差别只有两处：枚举设备时的数据流方向，以及 `Initialize` 是否带 `LOOPBACK` 标志。
/// 所以按"端点 + 方向"参数化，而不是写两个几乎相同的结构。
pub struct WasapiEndpoint {
    client: IAudioClient,
    capture: IAudioCaptureClient,
    format: AudioFormat,
    qpc_frequency: i64,
    direction: AudioDirection,
    /// 端点友好名（拿不到时退化为 id），用于日志与"选错设备"的诊断
    name: String,
    /// GetMixFormat 返回的内存，需用 CoTaskMemFree 释放
    mix_format: *mut WAVEFORMATEX,
}

// 说明：这些 COM 对象只在创建它们的线程上使用（音频线程），
// 且该线程与其它线程同处 MTA。此处断言 Send 是为了把对象移动到采集线程。
unsafe impl Send for WasapiEndpoint {}

impl WasapiEndpoint {
    /// 打开 [`EndpointSpec`] 指定的端点并开始采样。
    ///
    /// `spec.id = None` → 系统默认端点（V0.2 走这条）；`Some(id)` → 按 id 精确打开
    /// （接口先留好，接上设备选择时不用改 trait 形状）。
    ///
    /// 必须在将使用它的线程上调用（COM 每线程初始化由 `MfRuntime` 负责）。
    pub fn open_endpoint(spec: &EndpointSpec) -> MediaResult<Self> {
        // 保证本线程 COM 已初始化为 MTA（不重复拆，见 mf.rs 的说明）
        let _ = crate::mf::MfRuntime::start()?;

        let qpc_frequency = unsafe {
            let mut f = 0i64;
            QueryPerformanceFrequency(&mut f)
                .map_err(|e| MediaError::win32("QueryPerformanceFrequency", e))?;
            f
        };
        if qpc_frequency <= 0 {
            return Err(MediaError::Internal("QueryPerformanceFrequency 返回非正值".into()));
        }

        unsafe {
            let enumerator: IMMDeviceEnumerator =
                CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
                    .map_err(|e| MediaError::win32("CoCreateInstance(MMDeviceEnumerator)", e))?;
            let dataflow = dataflow_of(spec.direction);
            let device = match &spec.id {
                // 系统默认端点（V0.2 走这条）
                None => enumerator
                    .GetDefaultAudioEndpoint(dataflow, eConsole)
                    .map_err(|e| MediaError::win32("GetDefaultAudioEndpoint", e))?,
                // 按 id 精确打开（V0.2 未接线，接口先留好——将来加设备选择不用改 trait）
                Some(want) => {
                    let collection = enumerator
                        .EnumAudioEndpoints(dataflow, DEVICE_STATE_ACTIVE)
                        .map_err(|e| MediaError::win32("EnumAudioEndpoints", e))?;
                    let count = collection.GetCount().unwrap_or(0);
                    let mut found = None;
                    for i in 0..count {
                        if let Ok(d) = collection.Item(i) {
                            if endpoint_id(&d) == *want {
                                found = Some(d);
                                break;
                            }
                        }
                    }
                    found.ok_or_else(|| {
                        MediaError::Internal(format!("指定的音频端点不存在：{}", want))
                    })?
                }
            };
            let endpoint_id_str = endpoint_id(&device);
            let name = endpoint_friendly_name(&device, &endpoint_id_str);
            let client: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| MediaError::win32("IMMDevice::Activate", e))?;

            // ---- 契约一：优先请求规范格式（48k/2ch/f32）----
            // 带系统转换标志（AUTOCONVERTPCM | SRC_DEFAULT_QUALITY）：重采样由 Windows 音频引擎
            // 完成（与系统混音器同一条路径，不触碰"V0.2 不引入重采样"边界）。
            // 失败 → 回退端点原生混音格式并 WARN（日志记下用的是哪种）。
            let requested = requested_48k_f32();
            let dir_flags = match spec.direction {
                AudioDirection::Render => AUDCLNT_STREAMFLAGS_LOOPBACK,
                // 采集端点（麦克风）不加 loopback 标志（streamflags 参数是裸 u32）
                AudioDirection::Capture => 0,
            };
            let conv = AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY;
            let (mix_format, format) =
                match client.Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    dir_flags | conv,
                    10_000_000,
                    0,
                    &requested,
                    None,
                ) {
                    Ok(()) => {
                        tracing::info!(
                            sample_rate = 48_000,
                            channels = 2,
                            sample_fmt = "f32",
                            "已请求规范音频格式（系统转换开启）"
                        );
                        // 请求路径没有 GetMixFormat 的堆分配需要释放
                        (
                            ptr::null_mut(),
                            AudioFormat {
                                sample_rate: 48_000,
                                channels: 2,
                                format: SampleFormat::Float32,
                            },
                        )
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "规范音频格式请求失败 → 回退端点原生混音格式");
                        let mix_format = client
                            .GetMixFormat()
                            .map_err(|e2| MediaError::win32("GetMixFormat", e2))?;
                        if mix_format.is_null() {
                            return Err(MediaError::Internal("GetMixFormat 返回空指针".into()));
                        }
                        let format = read_format(mix_format)?;
                        (mix_format, format)
                    }
                };

            let capture: IAudioCaptureClient = client
                .GetService()
                .map_err(|e| MediaError::win32("GetService(IAudioCaptureClient)", e))?;

            client
                .Start()
                .map_err(|e| MediaError::win32("IAudioClient::Start", e))?;

            tracing::info!(
                direction = ?spec.direction,
                endpoint = %name,
                endpoint_id = %endpoint_id_str,
                sample_rate = format.sample_rate,
                channels = format.channels,
                format = ?format.format,
                "音频端点已打开"
            );

            Ok(Self {
                client,
                capture,
                format,
                qpc_frequency,
                direction: spec.direction,
                name,
                mix_format,
            })
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    /// 当前时刻的系统相对时间（100ns）。
    ///
    /// 与真实数据走同一条换算路径（QPC → [`srt_from_qpc`]），供
    /// "桌面静音时补静音块"使用。绝不能为静音另起一套时基，否则补出来的
    /// 音频轨会和视频轨错位——同步的根就是"所有时间戳都经过同一个换算点"。
    pub fn srt_now_endpoint(&self) -> MediaResult<Srt100ns> {
        let mut qpc = 0i64;
        unsafe {
            QueryPerformanceCounter(&mut qpc)
                .map_err(|e| MediaError::win32("QueryPerformanceCounter", e))?;
        }
        Ok(srt_from_qpc(QpcTicks(qpc), self.qpc_frequency))
    }

    /// 取一块数据；没有数据时返回 None（非阻塞，调用方自行 sleep）。
    pub fn poll_endpoint(&self) -> MediaResult<Option<AudioChunk>> {
        unsafe {
            let available = self
                .capture
                .GetNextPacketSize()
                .map_err(|e| MediaError::win32("GetNextPacketSize", e))?;
            if available == 0 {
                return Ok(None);
            }

            let mut data: *mut u8 = ptr::null_mut();
            let mut frames: u32 = 0;
            let mut flags: u32 = 0;
            let mut qpc: u64 = 0;
            self.capture
                .GetBuffer(&mut data, &mut frames, &mut flags, None, Some(&mut qpc))
                .map_err(|e| MediaError::win32("IAudioCaptureClient::GetBuffer", e))?;

            let result = if frames == 0 {
                Vec::new()
            } else if flags & AUDCLNT_BUFFERFLAGS_SILENT.0 as u32 != 0 || data.is_null() {
                // 静音块：WASAPI 允许数据指针无效，必须自己补零
                vec![0i16; frames as usize * self.format.channels as usize]
            } else {
                convert_to_pcm16(data, frames as usize, &self.format)
            };

            self.capture
                .ReleaseBuffer(frames)
                .map_err(|e| MediaError::win32("ReleaseBuffer", e))?;

            Ok(Some(AudioChunk {
                pcm: result,
                frames,
                // 唯一的时间戳换算点：QPC → SRT（必须经过频率换算）
                srt: srt_from_qpc(QpcTicks(qpc as i64), self.qpc_frequency),
            }))
        }
    }

    pub fn stop_endpoint(&self) {
        unsafe {
            let _ = self.client.Stop();
        }
    }
}

impl Drop for WasapiEndpoint {
    fn drop(&mut self) {
        self.stop();
        if !self.mix_format.is_null() {
            unsafe { CoTaskMemFree(Some(self.mix_format as *const core::ffi::c_void)) };
            self.mix_format = ptr::null_mut();
        }
    }
}

/// 规范请求格式 48000 Hz / 2 声道 / 32 位浮点（WAVE_FORMAT_IEEE_FLOAT = 3）
fn requested_48k_f32() -> WAVEFORMATEX {
    let mut w = WAVEFORMATEX::default();
    w.wFormatTag = 3; // WAVE_FORMAT_IEEE_FLOAT
    w.nChannels = 2;
    w.nSamplesPerSec = 48_000;
    w.wBitsPerSample = 32;
    w.nBlockAlign = 8; // 2ch × 32bit ÷ 8
    w.nAvgBytesPerSec = 48_000 * 8;
    w.cbSize = 0;
    w
}

/// WASAPI 系统转换标志：让 Windows 音频引擎完成格式/采样率转换
const AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM: u32 = 0x8000_0000;
const AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY: u32 = 0x0800_0000;

/// 一路音频来源的声明：种类 + 每源线性增益（[0, 2]，默认 1.0）。
#[derive(Clone, Copy, Debug)]
pub struct AudioSourceSpec {
    pub kind: AudioSourceKind,
    pub gain: f32,
}

/// 契约实现：见 [`source`]（时间戳域 + 块网格 + 设备缺失降级）。
impl AudioSource for WasapiEndpoint {
    fn start(spec: &EndpointSpec) -> MediaResult<Self> {
        Self::open_endpoint(spec)
    }

    fn format(&self) -> AudioFormat {
        self.format
    }

    fn poll(&self) -> MediaResult<Option<AudioChunk>> {
        self.poll_endpoint()
    }

    fn srt_now(&self) -> MediaResult<Srt100ns> {
        self.srt_now_endpoint()
    }

    fn stop(&self) {
        self.stop_endpoint()
    }

    fn opened(&self) -> String {
        format!("{}（{:?}，{} Hz / {} 声道）", self.name, self.direction, self.format.sample_rate, self.format.channels)
    }
}

///
/// 简化假设（已在本机实测记录）：32 位 → IEEE 浮点；16 位 → PCM 整数。
/// 共享模式下这是 Windows 混音器的常见形态（48kHz / 32bit float / 2ch）。
unsafe fn read_format(mix: *mut WAVEFORMATEX) -> MediaResult<AudioFormat> {
    // WAVEFORMATEX 在这里是 packed(1) 结构：对它的字段取引用是未定义行为
    // （编译器会直接报 E0793）。必须用 read_unaligned 逐个读出。
    let read_u16 = |p: *const u16| std::ptr::read_unaligned(p);
    let read_u32 = |p: *const u32| std::ptr::read_unaligned(p);

    let bits = read_u16(std::ptr::addr_of!((*mix).wBitsPerSample));
    let channels = read_u16(std::ptr::addr_of!((*mix).nChannels));
    let sample_rate = read_u32(std::ptr::addr_of!((*mix).nSamplesPerSec));

    let format = match bits {
        16 => SampleFormat::Pcm16,
        32 => SampleFormat::Float32,
        other => {
            return Err(MediaError::Internal(format!(
                "不支持的混音位深：{} 位（仅支持 16/32）",
                other
            )))
        }
    };
    if channels == 0 || sample_rate == 0 {
        return Err(MediaError::Internal(format!(
            "混音格式非法：{} 声道 @ {} Hz",
            channels, sample_rate
        )));
    }
    Ok(AudioFormat {
        sample_rate,
        channels,
        format,
    })
}

/// 把设备格式的交错采样转成 16 位整数 PCM。
unsafe fn convert_to_pcm16(data: *const u8, frames: usize, fmt: &AudioFormat) -> Vec<i16> {
    let samples = frames * fmt.channels as usize;
    let mut out = Vec::with_capacity(samples);
    match fmt.format {
        SampleFormat::Pcm16 => {
            let src = std::slice::from_raw_parts(data as *const i16, samples);
            out.extend_from_slice(src);
        }
        SampleFormat::Float32 => {
            let src = std::slice::from_raw_parts(data as *const f32, samples);
            for &v in src {
                // 浮点 [-1,1] → 整数，先夹住再缩放，避免溢出回绕
                let scaled = (v.clamp(-1.0, 1.0) * 32767.0).round();
                out.push(scaled as i16);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 生成一段 440Hz 正弦波 WAV（16 位单声道 44.1kHz）并循环播放。
    ///
    /// 为什么必须真的播声音：WASAPI loopback 在没有音频流播放时不产生任何数据包
    /// （`GetNextPacketSize` 恒为 0）。这是它的固有行为，不是缺陷——
    /// 但也意味着"静音环境下采集不到数据"是正确的，测试必须自己制造声音。
    struct TonePlayer {
        path: std::path::PathBuf,
    }

    impl TonePlayer {
        fn start() -> Option<Self> {
            use windows::core::HSTRING;
            use windows::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_FILENAME, SND_LOOP};

            // 构造 1 秒 440Hz 正弦 WAV
            let rate = 44_100u32;
            let frames = rate as usize;
            let mut data = Vec::with_capacity(44 + frames * 2);
            let byte_rate = rate * 2;
            data.extend_from_slice(b"RIFF");
            data.extend_from_slice(&((36 + frames * 2) as u32).to_le_bytes());
            data.extend_from_slice(b"WAVEfmt ");
            data.extend_from_slice(&16u32.to_le_bytes());
            data.extend_from_slice(&1u16.to_le_bytes()); // PCM
            data.extend_from_slice(&1u16.to_le_bytes()); // 单声道
            data.extend_from_slice(&rate.to_le_bytes());
            data.extend_from_slice(&byte_rate.to_le_bytes());
            data.extend_from_slice(&2u16.to_le_bytes()); // block align
            data.extend_from_slice(&16u16.to_le_bytes()); // bits
            data.extend_from_slice(b"data");
            data.extend_from_slice(&((frames * 2) as u32).to_le_bytes());
            for i in 0..frames {
                let t = i as f32 / rate as f32;
                let v = (t * 440.0 * std::f32::consts::TAU).sin() * 0.35; // 不要太大，避免刺耳
                data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
            }

            let path = std::env::temp_dir().join("screenlite-test-tone.wav");
            std::fs::write(&path, &data).ok()?;

            let wide = HSTRING::from(path.to_string_lossy().as_ref());
            let ok = unsafe {
                PlaySoundW(&wide, None, SND_FILENAME | SND_ASYNC | SND_LOOP).as_bool()
            };
            if !ok {
                eprintln!("跳过：PlaySoundW 播放失败（无音频输出设备？）");
                return None;
            }
            // 让音频引擎真正开始渲染
            std::thread::sleep(std::time::Duration::from_millis(300));
            Some(Self { path })
        }
    }

    impl Drop for TonePlayer {
        fn drop(&mut self) {
            use windows::core::HSTRING;
            use windows::Win32::Media::Audio::PlaySoundW;
            unsafe {
                // null 指针 = 停止播放
                let _ = PlaySoundW(windows::core::PCWSTR::null(), None, Default::default());
            }
            let _ = HSTRING::new(); // 保持 use 一致
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// 麦克风端点（采集方向）：能打开、格式合理、3 秒内的时间戳单调。
    ///
    /// 与 loopback 测试的关键区别：声音由环境提供（我们没法往麦克风里注入声音），
    /// 所以"拿到几个包"只作为观测打印、不作断言——否则在一台麦克风静音/未插好的机器上
    /// 会误报失败。设备缺失（打不开）同样是允许的降级路径，跳过而不是失败。
    #[test]
    fn test_microphone_capture_opens_and_reports_chunks() {
        let cap = match WasapiEndpoint::start(&EndpointSpec::default_of(AudioDirection::Capture)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过：无法打开默认麦克风（{}）——设备缺失是允许的降级路径", e);
                return;
            }
        };
        let fmt = cap.format();
        eprintln!(
            "麦克风打开成功：{}（{} Hz / {} 声道 / {:?}）",
            cap.opened(),
            fmt.sample_rate,
            fmt.channels,
            fmt.format
        );
        assert!(fmt.sample_rate >= 8000, "采样率异常：{}", fmt.sample_rate);
        assert!((1..=8).contains(&fmt.channels), "声道数异常");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let (mut chunks, mut frames) = (0u64, 0u64);
        let mut last_srt: Option<i64> = None;
        while std::time::Instant::now() < deadline {
            match cap.poll_endpoint() {
                Ok(Some(chunk)) => {
                    chunks += 1;
                    frames += chunk.frames as u64;
                    if let Some(prev) = last_srt {
                        assert!(
                            chunk.srt.0 >= prev,
                            "麦克风时间戳倒退：{} → {}（违反 trait 的时间戳域契约）",
                            prev,
                            chunk.srt.0
                        );
                    }
                    last_srt = Some(chunk.srt.0);
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
                Err(e) => panic!("麦克风采集失败：{}", e),
            }
        }
        eprintln!(
            "3 秒内：{} 块、{} 帧（麦克风数据由环境提供，0 块不算失败）",
            chunks, frames
        );
        cap.stop_endpoint();
    }

    #[test]
    fn test_pcm_peak_silence_and_min() {
        assert_eq!(pcm_peak(&[]), 0);
        assert_eq!(pcm_peak(&[0, 0, 0]), 0, "数字静音 → peak 0（用于区分「设备本身静音」）");
        assert_eq!(pcm_peak(&[0, -100, 50]), 100);
        assert_eq!(pcm_peak(&[i16::MIN]), 32768, "i16::MIN 的绝对值不能溢出");
    }

    /// 结构 + 内容验证：能打开 loopback、能拿到块、时间戳单调、
    /// 并且在有声音播放时采到非零样本。
    #[test]
    fn test_loopback_capture_produces_chunks_with_sane_timestamps() {
        let Some(_tone) = TonePlayer::start() else {
            return;
        };

        let cap = match WasapiEndpoint::start(&EndpointSpec::default_of(AudioDirection::Render)) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("跳过：无法打开 loopback 采集（{}）", e);
                return;
            }
        };
        let fmt = cap.format();
        eprintln!(
            "混音格式：{} Hz / {} 声道 / {:?}",
            fmt.sample_rate, fmt.channels, fmt.format
        );
        assert!(fmt.sample_rate >= 8000, "采样率异常：{}", fmt.sample_rate);
        assert!((1..=8).contains(&fmt.channels), "声道数异常");

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
        let mut chunks = 0u64;
        let mut total_frames = 0u64;
        let mut last_srt: Option<i64> = None;
        let mut nonzero_samples = 0u64;

        while std::time::Instant::now() < deadline {
            match cap.poll_endpoint() {
                Ok(Some(chunk)) => {
                    chunks += 1;
                    total_frames += chunk.frames as u64;
                    if let Some(prev) = last_srt {
                        assert!(
                            chunk.srt.0 >= prev,
                            "音频时间戳倒退：{} → {}",
                            prev,
                            chunk.srt.0
                        );
                    }
                    last_srt = Some(chunk.srt.0);
                    nonzero_samples += chunk.pcm.iter().filter(|&&s| s != 0).count() as u64;
                    assert_eq!(
                        chunk.pcm.len(),
                        chunk.frames as usize * fmt.channels as usize,
                        "样本数与帧数×声道数不符"
                    );
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(5)),
                Err(e) => panic!("采集失败：{}", e),
            }
        }

        eprintln!(
            "3 秒内：{} 块、{} 帧、非零样本 {}（末块 SRT={:?}）",
            chunks, total_frames, nonzero_samples, last_srt
        );

        // 正在播放 440Hz 正弦波 → 必须有数据包
        assert!(chunks > 0, "有声音在播放却一块数据都没拿到");
        assert!(total_frames > 0, "拿到 0 帧");
        // 采样率 48kHz、3 秒 → 帧数应在万级别
        assert!(
            total_frames > fmt.sample_rate as u64,
            "3 秒只采到 {} 帧（采样率 {}），明显偏少",
            total_frames,
            fmt.sample_rate
        );
        // 内容验证：正弦波采样必然有大量非零值（静音块会全是 0）
        assert!(
            nonzero_samples > total_frames / 10,
            "采到的几乎全是静音（非零样本 {} / 总样本 {}），内容可能不对",
            nonzero_samples,
            total_frames * fmt.channels as u64
        );
        let first_srt = last_srt.expect("应至少有一个包");
        assert!(first_srt > 0, "SRT 时间戳应大于 0（系统已运行一段时间）");
    }
}
