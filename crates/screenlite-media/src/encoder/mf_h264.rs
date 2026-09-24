//! Media Foundation H.264 编码后端。
//!
//! 硬件编码的准确语义：
//!
//! ```text
//! Sink Writer / Source Reader 默认不使用硬件 MFT。
//! 必须显式设置 MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS = TRUE，
//! 并提供 MF_SINK_WRITER_D3D_MANAGER 让硬件 MFT 在我们的 D3D11 设备上工作。
//! ```
//!
//! 诚实性要求：Sink Writer 不公开它最终使用了哪个 MFT，
//! 因此本模块只报告「已请求硬件 / 硬件 MFT 枚举结果 / 初始化结果」，
//! 不声称「正在使用硬件编码」。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::{Interface, HSTRING};
use windows::Win32::System::Com::{STGM_CREATE, STGM_READWRITE, STGM_SHARE_DENY_WRITE};
use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;
use windows::Win32::Media::MediaFoundation::{
    IMFSinkWriter, IMFAttributes, IMFByteStream, IMFDXGIDeviceManager, IMFMediaBuffer,
    IMFMediaType, IMFSample, IMF2DBuffer, MFCreate2DMediaBuffer, MFCreateAttributes,
    MFCreateMediaType, MFCreateMFByteStreamOnStream, MFCreateSample, MFCreateSinkWriterFromURL,
    MFMediaType_Video, MFNominalRange_16_235, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFCreateMemoryBuffer,
    MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT709, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE,
    MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE, MF_MT_MPEG2_PROFILE,
    MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
    MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, MF_SINK_WRITER_D3D_MANAGER,
    CODECAPI_AVEncCommonMaxBitRate, CODECAPI_AVEncCommonMeanBitRate,
    CODECAPI_AVEncCommonRateControlMode, CODECAPI_AVEncMPVGOPSize,
    eAVEncCommonRateControlMode_PeakConstrainedVBR, eAVEncH264VProfile_High,
    eAVEncH264VProfile_Main,
};
use crate::convert::Nv12Image;
use crate::error::{MediaError, MediaResult};
use crate::time::MfTime100ns;

use super::capabilities::probe_h264;
use super::{
    ConfigureOutcome, EncoderCapabilities, EncoderConfig, FinalizeOutcome, HardwarePreference,
    PixelFormat, VideoEncoder, VideoFrame,
};

/// MAKEFOURCC('N','V','1','2')，用于 MFCreate2DMediaBuffer。
const NV12_FOURCC: u32 = 0x3231_564E;

/// 已创建的样本计数（用于统计与自检）。
static SAMPLES_CREATED: AtomicU64 = AtomicU64::new(0);

pub fn samples_created() -> u64 {
    SAMPLES_CREATED.load(Ordering::Relaxed)
}

/// 已添加的音频流（懒创建：只有真的收到音频块才 AddStream）。
///
/// 为什么懒创建：WASAPI loopback 在没有音频播放时根本不产生数据包，
/// 若一开始就加音频流，纯静音的录制会得到一个 0 样本的音频轨，
/// 可能让 finalize 失败或产出空轨。懒创建让"无声音"的录制与现在完全一致。
struct AudioStream {
    index: u32,
}

pub struct MfSinkWriterEncoder {
    writer: IMFSinkWriter,
    stream_index: u32,
    width: u32,
    height: u32,
    fps: u32,
    samples: u64,
    /// 音频流（懒创建）
    audio: Option<AudioStream>,
    caps: EncoderCapabilities,
    outcome: ConfigureOutcome,
    partial_path: PathBuf,
    final_path: PathBuf,
    finalized: bool,
}

impl MfSinkWriterEncoder {
    pub fn create(
        cfg: &EncoderConfig,
        device_manager: Option<&IMFDXGIDeviceManager>,
    ) -> MediaResult<Self> {
        let mut caps = probe_h264(cfg.hardware);
        let hardware_requested = cfg.hardware == HardwarePreference::PreferHardware;
        let hardware_mft_found = caps.hardware == super::HardwareSupport::Available;

        let mut notes = Vec::new();
        let mut encoder_params_applied = false;

        let writer = unsafe {
            // 1. Sink Writer 属性
            let mut attrs_slot: Option<IMFAttributes> = None;
            MFCreateAttributes(&mut attrs_slot, 4)
                .map_err(|e| MediaError::win32("MFCreateAttributes", e))?;
            let attrs = attrs_slot
                .ok_or_else(|| MediaError::Internal("MFCreateAttributes 未返回对象".into()))?;
            let hw_enabled = match cfg.hardware {
                HardwarePreference::PreferHardware => true,
                HardwarePreference::PreferSoftware => false,
            };
            attrs
                .SetUINT32(&MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS, hw_enabled as u32)
                .map_err(|e| MediaError::win32("SetUINT32(hardware transforms)", e))?;
            if let Some(mgr) = device_manager {
                attrs
                    .SetUnknown(&MF_SINK_WRITER_D3D_MANAGER, mgr)
                    .map_err(|e| MediaError::win32("SetUnknown(D3D manager)", e))?;
            } else {
                notes.push("未提供 DXGI device manager：硬件 MFT 可能需要隐式 CPU 往返".into());
            }

            // 2. 输出类型（必须显式设置，不能只填 major/subtype）
            // profile 回退是真实逻辑：High 被拒时改用 Main 重新协商
            let mut profile = "High";
            let out_type = create_h264_output_type(cfg, eAVEncH264VProfile_High.0 as u32)?;

            // 3. 字节流 + Sink Writer
            //
            // 这里有两个实测出来的坑（对照实验见 tests/exp_sink.rs）：
            //
            // a) MFCreateSinkWriterFromMediaSink（自己建 MPEG4 sink）不会插入编码器：
            // WriteSample 会成功，但 Finalize 返回 0xC00D4A44「接收器未处理任何示例」。
            // 因此必须用 MFCreateSinkWriterFromURL，让 Sink Writer 自己搭编码拓扑。
            // b) MFCreateFile 收的是 file URL 而不是磁盘路径（传路径 → 0x80070002 / 0x8007007B）。
            //
            // 组合方案：URL 只用于推断容器类型（取 .mp4 扩展名），
            // 真正的写入目标是我们自己打开的 `.partial` 字节流 ——
            // 于是既保住了「finalize 成功后才改名为 .mp4」，又让编码器正常插进拓扑。
            let partial_w = HSTRING::from(cfg.partial_path.to_string_lossy().as_ref());
            let stream = SHCreateStreamOnFileEx(
                &partial_w,
                STGM_CREATE.0 | STGM_READWRITE.0 | STGM_SHARE_DENY_WRITE.0,
                0,
                true,
                None::<&windows::Win32::System::Com::IStream>,
            )
            .map_err(|e| MediaError::win32("SHCreateStreamOnFileEx", e))?;
            let byte_stream: IMFByteStream = MFCreateMFByteStreamOnStream(&stream)
                .map_err(|e| MediaError::win32("MFCreateMFByteStreamOnStream", e))?;

            let container_hint = HSTRING::from(cfg.final_path.to_string_lossy().as_ref());
            let writer: IMFSinkWriter =
                MFCreateSinkWriterFromURL(&container_hint, &byte_stream, &attrs)
                    .map_err(MediaError::MuxerInitFailed)?;

            let stream_index = match writer.AddStream(&out_type) {
                Ok(idx) => idx,
                Err(_) => {
                    let fallback =
                        create_h264_output_type(cfg, eAVEncH264VProfile_Main.0 as u32)?;
                    let idx = writer
                        .AddStream(&fallback)
                        .map_err(MediaError::MuxerInitFailed)?;
                    notes.push("High profile 被拒，已回退 Main profile".into());
                    profile = "Main";
                    idx
                }
            };

            // 4. 音频流：AddStream 必须在任何 SetInputMediaType 之前。
            // 实测：先 SetInputMediaType 再 AddStream 会返回 0xC00D36B2「当前状态的请求无效」，
            // 同样 BeginWriting 之后也不能再 AddStream。所以顺序固定为
            // 全部 AddStream → 全部 SetInputMediaType → BeginWriting
            let audio_index_format = match cfg.audio {
                Some(fmt) => {
                    let aac = create_aac_output_type(&fmt)?;
                    let idx = writer
                        .AddStream(&aac)
                        .map_err(MediaError::MuxerInitFailed)?;
                    tracing::info!(
                        index = idx,
                        sample_rate = fmt.sample_rate,
                        channels = fmt.channels,
                        "音频流已添加（AAC 128kbps）"
                    );
                    Some((idx, fmt))
                }
                None => None,
            };

            // 5. 输入类型：NV12 + 显式色彩信息
            let in_type = create_nv12_input_type(cfg)?;
            let params = create_encoder_params(cfg);
            match &params {
                Some(p) => match writer.SetInputMediaType(stream_index, &in_type, p) {
                    Ok(()) => encoder_params_applied = true,
                    Err(e) => {
                        notes.push(format!(
                            "编码参数被拒绝（{:#x}），回退编码器默认值",
                            e.code().0
                        ));
                        writer
                            .SetInputMediaType(stream_index, &in_type, None::<&IMFAttributes>)
                            .map_err(|e| MediaError::EncoderInitFailed {
                                stage: "SetInputMediaType(NV12, fallback)",
                                source: e,
                            })?;
                    }
                },
                None => {
                    writer
                        .SetInputMediaType(stream_index, &in_type, None::<&IMFAttributes>)
                        .map_err(|e| MediaError::EncoderInitFailed {
                            stage: "SetInputMediaType(NV12)",
                            source: e,
                        })?;
                }
            }

            // 音频输入类型（AddStream 已在视频输入类型之前完成，顺序不能颠倒）
            if let Some((idx, fmt)) = &audio_index_format {
                let pcm = create_pcm_input_type(fmt);
                writer
                    .SetInputMediaType(*idx, &pcm, None::<&IMFAttributes>)
                    .map_err(|e| MediaError::EncoderInitFailed {
                        stage: "SetInputMediaType(PCM)",
                        source: e,
                    })?;
            }

            writer.BeginWriting().map_err(|e| MediaError::EncoderInitFailed {
                stage: "BeginWriting",
                source: e,
            })?;

            let audio_stream = audio_index_format.map(|(index, _)| AudioStream { index });

            notes.extend(caps.notes.drain(..));
            (writer, stream_index, profile, audio_stream)
        };

        let (writer, stream_index, profile, audio_stream) = writer;

        let outcome = ConfigureOutcome {
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            bitrate: cfg.bitrate,
            profile: profile.to_string(),
            hardware_requested,
            hardware_mft_found,
            encoder_params_applied,
            notes,
        };

        caps.input_formats = vec![PixelFormat::Nv12];

        Ok(Self {
            writer,
            stream_index,
            width: cfg.width,
            height: cfg.height,
            fps: cfg.fps,
            samples: 0,
            audio: audio_stream,
            caps,
            outcome,
            partial_path: cfg.partial_path.clone(),
            final_path: cfg.final_path.clone(),
            finalized: false,
        })
    }

    fn submit_nv12(
        &mut self,
        nv12: &Nv12Image,
        pts: MfTime100ns,
        duration: MfTime100ns,
    ) -> MediaResult<()> {
        if nv12.width != self.width || nv12.height != self.height {
            return Err(MediaError::Internal(format!(
                "帧尺寸 {}x{} 与编码器配置 {}x{} 不一致（分辨率中途变化必须停止录制）",
                nv12.width, nv12.height, self.width, self.height
            )));
        }

        unsafe {
            // 每帧新建 buffer：已提交给 Sink Writer 的 buffer 不得复用或改写
            let buffer: IMFMediaBuffer =
                MFCreate2DMediaBuffer(self.width, self.height, NV12_FOURCC, false)
                    .map_err(|e| MediaError::win32("MFCreate2DMediaBuffer", e))?;
            let b2d: IMF2DBuffer = buffer
                .cast()
                .map_err(|e| MediaError::win32("IMF2DBuffer", e))?;

            let mut scan0: *mut u8 = std::ptr::null_mut();
            let mut pitch: i32 = 0;
            b2d.Lock2D(&mut scan0, &mut pitch)
                .map_err(|e| MediaError::win32("Lock2D", e))?;

            let w = self.width as usize;
            let h = self.height as usize;
            let dst_pitch = pitch as usize;
            // 必须用 MF 报告的 pitch，而不是我们自己的 stride，否则画面斜纹/绿边
            for y in 0..h {
                std::ptr::copy_nonoverlapping(
                    nv12.data.as_ptr().add(y * nv12.stride),
                    scan0.add(y * dst_pitch),
                    w,
                );
            }
            let uv_src = nv12.data.as_ptr().add(w * h);
            let uv_dst = scan0.add(dst_pitch * h);
            for y in 0..h / 2 {
                std::ptr::copy_nonoverlapping(
                    uv_src.add(y * nv12.stride),
                    uv_dst.add(y * dst_pitch),
                    w,
                );
            }

            b2d.Unlock2D().map_err(|e| MediaError::win32("Unlock2D", e))?;
            let contiguous = b2d
                .GetContiguousLength()
                .map_err(|e| MediaError::win32("GetContiguousLength", e))?;
            buffer
                .SetCurrentLength(contiguous)
                .map_err(|e| MediaError::win32("SetCurrentLength", e))?;

            let sample: IMFSample =
                MFCreateSample().map_err(|e| MediaError::win32("MFCreateSample", e))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| MediaError::win32("AddBuffer", e))?;
            sample
                .SetSampleTime(pts.0)
                .map_err(|e| MediaError::win32("SetSampleTime", e))?;
            sample
                .SetSampleDuration(duration.0)
                .map_err(|e| MediaError::win32("SetSampleDuration", e))?;

            self.writer
                .WriteSample(self.stream_index, &sample)
                .map_err(|e| MediaError::EncoderInitFailed {
                    stage: "WriteSample",
                    source: e,
                })?;

            SAMPLES_CREATED.fetch_add(1, Ordering::Relaxed);
            self.samples += 1;
        }
        Ok(())
    }
}

/// 线程安全性说明：
///
/// `IMFSinkWriter` / 编码器 MFT 不是 Rust 意义上的 `Send`（windows-rs 的 COM 指针未实现 Send）。
/// 但本对象的生命周期满足：创建后移交给编码线程，此后只被那一个线程使用，
/// 且创建线程与使用线程都在同一个 MTA（两者都调用 `CoInitializeEx(COINIT_MULTITHREADED)`），
/// 不存在并发使用与跨 apartment marshalling。因此该断言是受控且可检查的。
unsafe impl Send for MfSinkWriterEncoder {}

impl VideoEncoder for MfSinkWriterEncoder {    fn capabilities(&self) -> &EncoderCapabilities {
        &self.caps
    }

    fn configure_outcome(&self) -> &ConfigureOutcome {
        &self.outcome
    }

    fn submit(&mut self, frame: &VideoFrame) -> MediaResult<()> {
        self.submit_nv12(frame.nv12(), frame.pts, frame.duration)
    }

    fn submit_audio(
        &mut self,
        chunk: &crate::audio::AudioChunk,
        _format: crate::audio::AudioFormat,
        pts: MfTime100ns,
        duration: MfTime100ns,
    ) -> MediaResult<()> {
        if chunk.pcm.is_empty() {
            return Ok(());
        }
        // 未配置音频轨（cfg.audio = None，或采集启动失败）→ 静默忽略
        let Some(audio) = &self.audio else {
            return Ok(());
        };
        let index = audio.index;

        unsafe {
            let bytes = chunk.pcm.len() * 2; // i16
            let buffer: IMFMediaBuffer = MFCreateMemoryBuffer(bytes as u32)
                .map_err(|e| MediaError::win32("MFCreateMemoryBuffer(audio)", e))?;
            let mut ptr: *mut u8 = std::ptr::null_mut();
            buffer
                .Lock(&mut ptr, None, None)
                .map_err(|e| MediaError::win32("Lock(audio)", e))?;
            std::ptr::copy_nonoverlapping(
                chunk.pcm.as_ptr() as *const u8,
                ptr,
                bytes,
            );
            buffer.Unlock().map_err(|e| MediaError::win32("Unlock(audio)", e))?;
            buffer
                .SetCurrentLength(bytes as u32)
                .map_err(|e| MediaError::win32("SetCurrentLength(audio)", e))?;

            let sample: IMFSample =
                MFCreateSample().map_err(|e| MediaError::win32("MFCreateSample(audio)", e))?;
            sample
                .AddBuffer(&buffer)
                .map_err(|e| MediaError::win32("AddBuffer(audio)", e))?;
            sample
                .SetSampleTime(pts.0)
                .map_err(|e| MediaError::win32("SetSampleTime(audio)", e))?;
            sample
                .SetSampleDuration(duration.0)
                .map_err(|e| MediaError::win32("SetSampleDuration(audio)", e))?;

            self.writer
                .WriteSample(index, &sample)
                .map_err(|e| MediaError::EncoderInitFailed {
                    stage: "WriteSample(audio)",
                    source: e,
                })?;
        }
        Ok(())
    }

    /// 懒创建音频流：第一次收到音频块时才 AddStream + 协商媒体类型。
    fn ensure_audio_stream(&mut self, format: crate::audio::AudioFormat) -> MediaResult<u32> {
        if let Some(a) = &self.audio {
            return Ok(a.index);
        }
        unsafe {
            let out_type = create_aac_output_type(&format)?;
            let index = self
                .writer
                .AddStream(&out_type)
                .map_err(MediaError::MuxerInitFailed)?;
            let in_type = create_pcm_input_type(&format);
            self.writer
                .SetInputMediaType(index, &in_type, None::<&IMFAttributes>)
                .map_err(|e| MediaError::EncoderInitFailed {
                    stage: "SetInputMediaType(PCM)",
                    source: e,
                })?;
            tracing::info!(
                index,
                sample_rate = format.sample_rate,
                channels = format.channels,
                "音频流已添加（AAC 128kbps）"
            );
            self.audio = Some(AudioStream { index });
            Ok(index)
        }
    }

    fn flush(&mut self) -> MediaResult<()> {
        // 这里故意不调用 `IMFSinkWriter::Flush()`。
        //
        // 实测（tests/encoder_roundtrip.rs，2026-09-21）：
        // 提交 30 帧 → flush() → Finalize() 返回 0xC00D4A44「接收器未处理任何示例」，
        // 输出退化成约 150 字节、0 样本的 MP4；
        // 去掉 flush() → Finalize 正常，输出 1175 字节且解码颜色校验通过。
        //
        // 结论：`IMFSinkWriter::Flush` 的语义是「丢弃该流上排队的数据」，
        // 而不是「把编码器里的延迟样本推出去」。MF 后端不需要这一步：
        // `Finalize()` 自己会 drain 编码器并把所有样本写进封装。
        //
        // 保留这个 trait 方法是为了后续 D3D12 后端 —— 那里「等待编码器输出全部样本」
        // 是一个真实且不同的操作。
        Ok(())
    }

    fn finish(mut self: Box<Self>) -> MediaResult<FinalizeOutcome> {
        unsafe {
            self.writer
                .Finalize()
                .map_err(MediaError::Mp4FinalizeFailed)?;
        }
        self.finalized = true;

        // finalize 成功后才把 .partial 改名为 .mp4
        std::fs::rename(&self.partial_path, &self.final_path).map_err(|e| {
            MediaError::Internal(format!(
                "重命名 {} -> {} 失败：{}",
                self.partial_path.display(),
                self.final_path.display(),
                e
            ))
        })?;

        let bytes = std::fs::metadata(&self.final_path)
            .map(|m| m.len())
            .unwrap_or(0);

        Ok(FinalizeOutcome {
            path: self.final_path.clone(),
            bytes,
            samples: self.samples,
            duration: MfTime100ns(self.samples as i64 * 10_000_000 / self.fps.max(1) as i64),
        })
    }
}

/// AAC 输出类型（128 kbps / LC profile / 已解码的原始 AAC 负载）。
unsafe fn create_aac_output_type(fmt: &crate::audio::AudioFormat) -> MediaResult<IMFMediaType> {
    use windows::Win32::Media::MediaFoundation::{
        MFAudioFormat_AAC, MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, MF_MT_AAC_PAYLOAD_TYPE,
        MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE, MF_MT_AUDIO_NUM_CHANNELS,
        MF_MT_AUDIO_SAMPLES_PER_SECOND, MFMediaType_Audio,
    };
    let t: IMFMediaType =
        MFCreateMediaType().map_err(|e| MediaError::win32("MFCreateMediaType(aac)", e))?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio)
        .map_err(|e| MediaError::win32("aac MAJOR_TYPE", e))?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_AAC)
        .map_err(|e| MediaError::win32("aac SUBTYPE", e))?;
    t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16)
        .map_err(|e| MediaError::win32("aac BITS_PER_SAMPLE", e))?;
    t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, fmt.sample_rate)
        .map_err(|e| MediaError::win32("aac SAMPLES_PER_SECOND", e))?;
    t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, fmt.channels as u32)
        .map_err(|e| MediaError::win32("aac NUM_CHANNELS", e))?;
    // 128 kbps
    t.SetUINT32(&MF_MT_AUDIO_AVG_BYTES_PER_SECOND, 16_000)
        .map_err(|e| MediaError::win32("aac AVG_BYTES_PER_SECOND", e))?;
    // 0 = 原始 AAC（不含 ADTS 头）；0x29 = AAC-LC profile level
    t.SetUINT32(&MF_MT_AAC_PAYLOAD_TYPE, 0)
        .map_err(|e| MediaError::win32("aac PAYLOAD_TYPE", e))?;
    t.SetUINT32(&MF_MT_AAC_AUDIO_PROFILE_LEVEL_INDICATION, 0x29)
        .map_err(|e| MediaError::win32("aac PROFILE_LEVEL", e))?;
    Ok(t)
}

/// 未压缩 PCM 输入类型（16 位整数、与采集格式同采样率/声道数）。
unsafe fn create_pcm_input_type(fmt: &crate::audio::AudioFormat) -> IMFMediaType {
    use windows::Win32::Media::MediaFoundation::{
        MFAudioFormat_PCM, MF_MT_AUDIO_AVG_BYTES_PER_SECOND, MF_MT_AUDIO_BITS_PER_SAMPLE,
        MF_MT_AUDIO_BLOCK_ALIGNMENT, MF_MT_AUDIO_NUM_CHANNELS, MF_MT_AUDIO_SAMPLES_PER_SECOND,
        MFMediaType_Audio,
    };
    let t = MFCreateMediaType().expect("MFCreateMediaType 失败");
    let _ = t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Audio);
    let _ = t.SetGUID(&MF_MT_SUBTYPE, &MFAudioFormat_PCM);
    let _ = t.SetUINT32(&MF_MT_AUDIO_BITS_PER_SAMPLE, 16);
    let _ = t.SetUINT32(&MF_MT_AUDIO_SAMPLES_PER_SECOND, fmt.sample_rate);
    let _ = t.SetUINT32(&MF_MT_AUDIO_NUM_CHANNELS, fmt.channels as u32);
    let block_align = fmt.channels as u32 * 2;
    let _ = t.SetUINT32(&MF_MT_AUDIO_BLOCK_ALIGNMENT, block_align);
    let _ = t.SetUINT32(
        &MF_MT_AUDIO_AVG_BYTES_PER_SECOND,
        fmt.sample_rate * block_align,
    );
    t
}impl Drop for MfSinkWriterEncoder {
    fn drop(&mut self) {
        // 未 finalize 就析构：保留 .partial，绝不假装成功
        if !self.finalized && self.partial_path.exists() {
            tracing::warn!(
                path = %self.partial_path.display(),
                "编码器未 finalize 即被释放，MP4 不完整（保留 .partial）"
            );
        }
    }
}

unsafe fn create_h264_output_type(cfg: &EncoderConfig, profile: u32) -> MediaResult<IMFMediaType> {
    let t: IMFMediaType =
        MFCreateMediaType().map_err(|e| MediaError::win32("MFCreateMediaType", e))?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|e| MediaError::win32("MF_MT_MAJOR_TYPE", e))?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264)
        .map_err(|e| MediaError::win32("MF_MT_SUBTYPE", e))?;
    t.SetUINT32(&MF_MT_AVG_BITRATE, cfg.bitrate)
        .map_err(|e| MediaError::win32("MF_MT_AVG_BITRATE", e))?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))
        .map_err(|e| MediaError::win32("MF_MT_FRAME_SIZE", e))?;
    t.SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps, 1))
        .map_err(|e| MediaError::win32("MF_MT_FRAME_RATE", e))?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))
        .map_err(|e| MediaError::win32("MF_MT_PIXEL_ASPECT_RATIO", e))?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        .map_err(|e| MediaError::win32("MF_MT_INTERLACE_MODE", e))?;
    t.SetUINT32(&MF_MT_MPEG2_PROFILE, profile)
        .map_err(|e| MediaError::win32("MF_MT_MPEG2_PROFILE", e))?;
    // 注意：不要在输出类型上设置 MF_MT_MAX_KEYFRAME_SPACING。
    // 实测（见 tests/exp_sink.rs 的对照实验）会让 Sink Writer 完全不处理任何样本
    // （WriteSample 返回成功但 Finalize 报 0xC00D4A44）。
    // 关键帧间隔改用编码参数里的 CODECAPI_AVEncMPVGOPSize 传递。
    Ok(t)
}

unsafe fn create_nv12_input_type(cfg: &EncoderConfig) -> MediaResult<IMFMediaType> {
    let t: IMFMediaType =
        MFCreateMediaType().map_err(|e| MediaError::win32("MFCreateMediaType", e))?;
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video)
        .map_err(|e| MediaError::win32("MF_MT_MAJOR_TYPE", e))?;
    t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12)
        .map_err(|e| MediaError::win32("MF_MT_SUBTYPE(NV12)", e))?;
    t.SetUINT64(&MF_MT_FRAME_SIZE, pack2(cfg.width, cfg.height))
        .map_err(|e| MediaError::win32("MF_MT_FRAME_SIZE", e))?;
    t.SetUINT64(&MF_MT_FRAME_RATE, pack2(cfg.fps, 1))
        .map_err(|e| MediaError::win32("MF_MT_FRAME_RATE", e))?;
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, pack2(1, 1))
        .map_err(|e| MediaError::win32("MF_MT_PIXEL_ASPECT_RATIO", e))?;
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
        .map_err(|e| MediaError::win32("MF_MT_INTERLACE_MODE", e))?;
    // 色彩信息必须显式声明，不允许依赖默认值
    t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
        .map_err(|e| MediaError::win32("MF_MT_YUV_MATRIX", e))?;
    t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
        .map_err(|e| MediaError::win32("MF_MT_VIDEO_NOMINAL_RANGE", e))?;
    Ok(t)
}

unsafe fn create_encoder_params(cfg: &EncoderConfig) -> Option<IMFAttributes> {
    let mut slot: Option<IMFAttributes> = None;
    MFCreateAttributes(&mut slot, 4).ok()?;
    let attrs = slot?;
    attrs
        .SetUINT32(
            &CODECAPI_AVEncCommonRateControlMode,
            eAVEncCommonRateControlMode_PeakConstrainedVBR.0 as u32,
        )
        .ok()?;
    attrs
        .SetUINT32(&CODECAPI_AVEncCommonMeanBitRate, cfg.bitrate)
        .ok()?;
    attrs
        .SetUINT32(&CODECAPI_AVEncCommonMaxBitRate, cfg.bitrate * 3 / 2)
        .ok()?;
    attrs
        .SetUINT32(&CODECAPI_AVEncMPVGOPSize, cfg.fps * cfg.gop_seconds)
        .ok()?;
    Some(attrs)
}

/// 把两个 u32 打包成 Media Foundation 的 u64 布局（高 32 位在前）。
fn pack2(hi: u32, lo: u32) -> u64 {
    ((hi as u64) << 32) | lo as u64
}
