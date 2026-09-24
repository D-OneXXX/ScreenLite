//! 编码器能力探测。
//!
//! 硬性要求：不允许返回假数据。枚举不到就返回 `Unavailable`，
//! 未知就返回 `Unknown`，绝不编造「支持」。

use std::ffi::c_void;

use windows::Win32::Media::MediaFoundation::{
    IMFActivate, MFTEnumEx, MFT_CATEGORY_VIDEO_ENCODER, MFT_ENUM_FLAG, MFT_ENUM_FLAG_ASYNCMFT,
    MFT_ENUM_FLAG_HARDWARE, MFT_ENUM_FLAG_SORTANDFILTER, MFT_ENUM_FLAG_SYNCMFT,
    MFT_FRIENDLY_NAME_Attribute, MFT_REGISTER_TYPE_INFO, MFMediaType_Video, MFVideoFormat_H264,
};
use windows::Win32::System::Com::CoTaskMemFree;

use crate::error::{MediaError, MediaResult};

use super::{
    CodecId, EncoderBackend, EncoderCapabilities, EncoderFeatures, FeatureSupport,
    HardwarePreference, HardwareSupport, PixelFormat, RateControlMode,
};

/// 枚举出的 H.264 编码器 MFT 描述。
#[derive(Debug, Clone)]
pub struct EncoderMftInfo {
    pub friendly_name: String,
    pub hardware: bool,
}

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

fn friendly_name(activate: &IMFActivate) -> String {
    let mut buf = [0u16; 256];
    match unsafe { activate.GetString(&MFT_FRIENDLY_NAME_Attribute, &mut buf, None) } {
        Ok(()) => {
            let s = wide_to_string(&buf);
            if s.is_empty() {
                "(unnamed)".into()
            } else {
                s
            }
        }
        Err(_) => "(unnamed)".into(),
    }
}

/// 枚举 H.264 编码器 MFT（真实枚举，不是猜测）。
pub fn enumerate_h264_encoders(hardware_only: bool) -> MediaResult<Vec<EncoderMftInfo>> {
    let flags: MFT_ENUM_FLAG = if hardware_only {
        MFT_ENUM_FLAG(MFT_ENUM_FLAG_HARDWARE.0 | MFT_ENUM_FLAG_SORTANDFILTER.0)
    } else {
        MFT_ENUM_FLAG(
            MFT_ENUM_FLAG_HARDWARE.0
                | MFT_ENUM_FLAG_SYNCMFT.0
                | MFT_ENUM_FLAG_ASYNCMFT.0
                | MFT_ENUM_FLAG_SORTANDFILTER.0,
        )
    };

    let output_type = MFT_REGISTER_TYPE_INFO {
        guidMajorType: MFMediaType_Video,
        guidSubtype: MFVideoFormat_H264,
    };

    let mut out = Vec::new();
    unsafe {
        let mut activates: *mut Option<IMFActivate> = std::ptr::null_mut();
        let mut count: u32 = 0;
        MFTEnumEx(
            MFT_CATEGORY_VIDEO_ENCODER,
            flags,
            None,
            Some(&output_type),
            &mut activates,
            &mut count,
        )
        .map_err(MediaError::EncoderEnumerationFailed)?;

        if !activates.is_null() {
            for i in 0..count as usize {
                let slot = activates.add(i);
                // 必须 take() 把对象移出数组再让它离开作用域（触发 Release）。
                // 用 as_ref() 只是借用，而 CoTaskMemFree 不会运行析构函数，
                // 那样每个 IMFActivate 的引用计数就永久泄漏了。
                if let Some(activate) = (*slot).take() {
                    out.push(EncoderMftInfo {
                        friendly_name: friendly_name(&activate),
                        hardware: hardware_only,
                    });
                    drop(activate);
                }
            }
            CoTaskMemFree(Some(activates as *const c_void));
        }
    }
    Ok(out)
}

/// 启动前探测 H.264 编码能力。
///
/// 返回的 `hardware` 只反映枚举结果；由于 Sink Writer 不公开它最终选了哪个 MFT，
/// 我们不得据此声称「正在使用硬件编码」。
pub fn probe_h264(pref: HardwarePreference) -> EncoderCapabilities {
    let mut notes = Vec::new();

    let hardware_mfts = match enumerate_h264_encoders(true) {
        Ok(list) => list,
        Err(e) => {
            notes.push(format!("硬件编码器枚举失败：{}", e));
            Vec::new()
        }
    };

    let hardware = match hardware_mfts.len() {
        0 => HardwareSupport::Unavailable,
        _ => HardwareSupport::Available,
    };
    if !hardware_mfts.is_empty() {
        notes.push(format!(
            "枚举到硬件 H.264 编码器 MFT：{}",
            hardware_mfts
                .iter()
                .map(|m| m.friendly_name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ));
    } else {
        notes.push("未枚举到硬件 H.264 编码器 MFT".into());
    }

    match pref {
        HardwarePreference::PreferHardware => {
            notes.push("已请求硬件编码路径（MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS = TRUE）".into())
        }
        HardwarePreference::PreferSoftware => {
            notes.push("诊断模式：已强制软件编码路径（硬件开关 = FALSE）".into())
        }
    }

    // V0.1 只实现这两个 codec 的查询：HEVC/AV1 明确 Unsupported，不谎报
    notes.push("HEVC / AV1 后端在 V0.1 未实现，能力查询返回 Unsupported".into());

    EncoderCapabilities {
        backend: EncoderBackend::MediaFoundation,
        codec: CodecId::H264,
        hardware,
        // V0.1 实现范围：只接受 NV12
        input_formats: vec![PixelFormat::Nv12],
        max_width: 0,
        max_height: 0,
        max_fps: None,
        features: EncoderFeatures {
            bframes: FeatureSupport::Unknown,
            rate_control: vec![RateControlMode::PeakConstrainedVbr],
            low_latency: FeatureSupport::Unknown,
            max_bitrate: None,
        },
        notes,
    }
}
