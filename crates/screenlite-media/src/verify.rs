//! 产物校验：把录出来的 MP4 解码回读，确认它是完整可播放的文件。
//!
//! 为什么需要：`Finalize` 失败或文件被截断时，文件大小仍然非 0，
//! 只看大小会把损坏文件判为成功。这里逐帧解码并统计，能真正暴露：
//! 0 样本、尾部截断、moov 缺失、时间轴断层。

use std::path::Path;

use windows::core::HSTRING;
use windows::Win32::Media::MediaFoundation::{
    IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MFCreateAttributes,
    MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video, MFVideoFormat_NV12,
    MF_MT_MAJOR_TYPE, MF_MT_SUBTYPE, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
};

use crate::error::{MediaError, MediaResult};
use crate::mf::MfRuntime;

#[derive(Debug, Clone)]
pub struct Mp4Probe {
    pub frame_count: u64,
    /// 文件真实分辨率（读自原生压缩媒体类型）
    pub native_width: u32,
    pub native_height: u32,
    /// 解码器协商出的 NV12 输出尺寸（可能因 16 对齐而比原生尺寸大，仅用于读取缓冲）
    pub width: u32,
    pub height: u32,
    /// 最后一个样本的时间戳 + 时长（100ns），即文件时间轴总长度。
    pub duration_100ns: i64,
    /// 时间轴上的断层次数（相邻样本时间戳间隔 > 2 个帧间隔）
    pub gaps: u64,
}

impl Mp4Probe {
    pub fn duration_ms(&self) -> i64 {
        self.duration_100ns / 10_000
    }
}

/// 逐帧解码校验。`expected_fps` 用于判断时间轴断层。
pub fn probe_mp4(path: &Path, expected_fps: u32) -> MediaResult<Mp4Probe> {
    let _rt = MfRuntime::start()?;
    if !path.exists() {
        return Err(MediaError::Internal(format!(
            "产物不存在：{}",
            path.display()
        )));
    }

    unsafe {
        let mut attrs_slot: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_slot, 1)
            .map_err(|e| MediaError::win32("MFCreateAttributes", e))?;
        let attrs = attrs_slot
            .ok_or_else(|| MediaError::Internal("MFCreateAttributes 未返回对象".into()))?;

        let url = HSTRING::from(path.to_string_lossy().as_ref());
        let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs)
            .map_err(|e| MediaError::win32("MFCreateSourceReaderFromURL", e))?;

        // 先读原生（已压缩）媒体类型的尺寸 —— 这才是文件里记录的真实分辨率。
        // 之后设置的 NV12 输出类型是给解码器协商用的，其尺寸可能因 16 字节对齐而偏大，
        // 直接拿来当"分辨率"会误判。
        let (native_width, native_height) = match reader
            .GetNativeMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, 0)
        {
            Ok(native) => {
                let packed = native
                    .GetUINT64(&windows::Win32::Media::MediaFoundation::MF_MT_FRAME_SIZE)
                    .unwrap_or(0);
                ((packed >> 32) as u32, (packed & 0xFFFF_FFFF) as u32)
            }
            Err(_) => (0, 0),
        };

        // 让 Source Reader 自己协商尺寸，只指定 NV12
        let target: IMFMediaType =
            MFCreateMediaType().map_err(|e| MediaError::win32("MFCreateMediaType", e))?;
        target
            .SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
            .map_err(|e| MediaError::win32("MF_MT_MAJOR_TYPE", e))?;
        target
            .SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
            .map_err(|e| MediaError::win32("MF_MT_SUBTYPE(NV12)", e))?;
        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &target)
            .map_err(|e| MediaError::win32("SetCurrentMediaType(NV12)", e))?;

        let mut frame_count = 0u64;
        let mut width = 0u32;
        let mut height = 0u32;
        let mut last_end = 0i64;
        let mut prev_end = 0i64;
        let mut gaps = 0u64;
        let frame_tick = 10_000_000i64 / expected_fps.max(1) as i64;

        loop {
            let mut actual = 0u32;
            let mut flags = 0u32;
            let mut ts = 0i64;
            let mut sample_slot: Option<IMFSample> = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut actual),
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample_slot),
                )
                .map_err(|e| MediaError::win32("ReadSample", e))?;

            let Some(sample) = sample_slot else {
                break; // 正常结束
            };

            if frame_count == 0 {
                if let Ok(media_type) = reader.GetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32) {
                    let packed = media_type.GetUINT64(&windows::Win32::Media::MediaFoundation::MF_MT_FRAME_SIZE).unwrap_or(0);
                    width = (packed >> 32) as u32;
                    height = (packed & 0xFFFF_FFFF) as u32;
                }
            }

            let dur = sample.GetSampleDuration().unwrap_or(frame_tick);
            let end = ts + dur;
            if frame_count > 0 && ts - prev_end > frame_tick * 2 {
                gaps += 1;
            }
            prev_end = end;
            last_end = last_end.max(end);
            frame_count += 1;
        }

        if frame_count == 0 {
            return Err(MediaError::Internal(format!(
                "{} 解码出 0 帧（文件损坏或未正确 finalize）",
                path.display()
            )));
        }

        Ok(Mp4Probe {
            frame_count,
            native_width,
            native_height,
            width,
            height,
            duration_100ns: last_end,
            gaps,
        })
    }
}

/// 音频轨探测结果。
#[derive(Debug, Clone)]
pub struct AudioProbe {
    /// 音频样本数（AAC 帧数，不是 PCM 帧）
    pub sample_count: u64,
    /// 最后一个样本的时间戳 + 时长（100ns）= 音频轨长度
    pub last_end_100ns: i64,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioProbe {
    pub fn duration_ms(&self) -> i64 {
        self.last_end_100ns / 10_000
    }
}

/// 探测 MP4 里是否存在音频轨。没有音频流时返回 `Ok(None)`（例如静音录制）。
///
/// 用途：验证"系统声音真的写进文件了"以及音视频同步量
/// （把 `last_end_100ns` 与视频时长比较）。
pub fn probe_audio(path: &Path) -> MediaResult<Option<AudioProbe>> {
    use windows::Win32::Media::MediaFoundation::{
        MFMediaType_Audio, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
        MF_MT_MAJOR_TYPE, MF_SOURCE_READER_FIRST_AUDIO_STREAM,
    };
    let _rt = MfRuntime::start()?;

    unsafe {
        let mut attrs_slot: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_slot, 1)
            .map_err(|e| MediaError::win32("MFCreateAttributes(audio)", e))?;
        let attrs = attrs_slot
            .ok_or_else(|| MediaError::Internal("MFCreateAttributes 未返回对象".into()))?;
        let url = HSTRING::from(path.to_string_lossy().as_ref());
        let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs)
            .map_err(|e| MediaError::win32("MFCreateSourceReaderFromURL(audio)", e))?;

        let stream = MF_SOURCE_READER_FIRST_AUDIO_STREAM.0 as u32;
        let native = match reader.GetNativeMediaType(stream, 0) {
            Ok(t) => t,
            Err(_) => return Ok(None), // 没有音频流
        };
        let major = native
            .GetGUID(&MF_MT_MAJOR_TYPE)
            .map_err(|e| MediaError::win32("audio MAJOR_TYPE", e))?;
        if major != MFMediaType_Audio {
            return Ok(None);
        }
        let sample_rate = native.GetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND).unwrap_or(0);
        let channels = native.GetUINT32(&MF_MT_AUDIO_NUM_CHANNELS).unwrap_or(0) as u16;

        let mut sample_count = 0u64;
        let mut last_end = 0i64;
        loop {
            let (mut actual, mut flags, mut ts) = (0u32, 0u32, 0i64);
            let mut sample_slot: Option<IMFSample> = None;
            reader
                .ReadSample(
                    stream,
                    0,
                    Some(&mut actual),
                    Some(&mut flags),
                    Some(&mut ts),
                    Some(&mut sample_slot),
                )
                .map_err(|e| MediaError::win32("ReadSample(audio)", e))?;
            let Some(sample) = sample_slot else { break };
            let dur = sample.GetSampleDuration().unwrap_or(0);
            last_end = last_end.max(ts + dur);
            sample_count += 1;
        }

        Ok(Some(AudioProbe {
            sample_count,
            last_end_100ns: last_end,
            sample_rate,
            channels,
        }))
    }
}
pub fn process_memory() -> (u64, u64) {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut pmc = PROCESS_MEMORY_COUNTERS::default();
    let size = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut pmc, size) }.is_ok();
    if ok {
        (pmc.WorkingSetSize as u64, pmc.PagefileUsage as u64)
    } else {
        (0, 0)
    }
}
