//! 编码链路端到端验证（不需要显示器，但需要 Media Foundation）。
//!
//! 这是 V0.1 里唯一能在无播放器环境下验证「颜色是否正确」的手段：
//!
//! ```text
//! 合成 BGRA → BGRA→NV12 转换 → MF H.264 编码 → MP4 → MF 解码回 NV12 → 比对 Y/Cb/Cr
//! ```
//!
//! 它同时验证：色彩系数（BT.709/limited）、NV12 平面布局与 pitch、编码器输入格式、
//! MP4 封装与 finalize、以及「.partial → .mp4」改名。

use std::path::PathBuf;

use screenlite_media::consts::default_bitrate;
use screenlite_media::convert::{bgra_to_nv12, BgraImageView};
use screenlite_media::encoder::{
    create_encoder, EncoderConfig, HardwarePreference, VideoFrame, VideoFrameData,
};
use screenlite_media::mf::MfRuntime;
use screenlite_media::scheduler::slot_time_mf;
use screenlite_media::time::MfTime100ns;

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 30;
const FRAMES: u64 = 30;

fn temp_dir(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!("screenlite-test-{}-{}", tag, std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    dir
}

/// 生成纯色 BGRA 图（pitch 故意大于 width*4，模拟真实 D3D11 staging 布局）。
fn solid_bgra(r: u8, g: u8, b: u8, w: u32, h: u32) -> (Vec<u8>, usize) {
    let pitch = ((w * 4 + 255) / 256) * 256; // 256 字节对齐
    let mut data = vec![0u8; (pitch * h) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let o = y * pitch as usize + x * 4;
            data[o] = b;
            data[o + 1] = g;
            data[o + 2] = r;
            data[o + 3] = 255;
        }
    }
    (data, pitch as usize)
}

struct DecodedFrame {
    y: u8,
    cb: u8,
    cr: u8,
}

/// 用 Media Foundation 解码指定序号附近的帧，返回中心像素的 NV12 值。
fn decode_center_pixel(path: &std::path::Path, frame_index: u64) -> DecodedFrame {
    use windows::core::{Interface, HSTRING};
    use windows::Win32::Media::MediaFoundation::{
        IMF2DBuffer, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader,
        MFCreateAttributes, MFCreateMediaType, MFCreateSourceReaderFromURL,
        MFMediaType_Video, MFNominalRange_16_235, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
        MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
        MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE, MF_MT_YUV_MATRIX,
        MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, MF_SOURCE_READER_FIRST_VIDEO_STREAM,
        MFVideoTransferMatrix_BT709,
    };

    unsafe {
        let mut attrs_slot: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_slot, 1).unwrap();
        let attrs = attrs_slot.unwrap();
        // 允许插入解码器/转换器，确保拿到 NV12
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .unwrap();

        let url = HSTRING::from(path.to_string_lossy().as_ref());
        let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs).unwrap();

        let target: IMFMediaType = MFCreateMediaType().unwrap();
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
        target.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
        target
            .SetUINT64(&MF_MT_FRAME_SIZE, ((W as u64) << 32) | H as u64)
            .unwrap();
        target
            .SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1)
            .unwrap();
        target
            .SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1)
            .unwrap();
        target
            .SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32)
            .unwrap();
        target
            .SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32)
            .unwrap();
        target
            .SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32)
            .unwrap();
        reader
            .SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &target,
            )
            .expect("解码器不接受 NV12 输出类型");

        let mut index = 0u64;
        loop {
            let mut actual_stream = 0u32;
            let mut flags = 0u32;
            let mut timestamp = 0i64;
            let mut sample_slot: Option<IMFSample> = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut actual_stream),
                    Some(&mut flags),
                    Some(&mut timestamp),
                    Some(&mut sample_slot),
                )
                .unwrap();
            let Some(sample) = sample_slot else {
                panic!("解码到第 {} 帧时数据结束", index);
            };
            if index >= frame_index {
                let buffer = sample.ConvertToContiguousBuffer().unwrap();
                // NV12 优先用 IMF2DBuffer 拿真实 pitch
                let (data, pitch) = if let Ok(b2d) = buffer.cast::<IMF2DBuffer>() {
                    let mut scan0: *mut u8 = std::ptr::null_mut();
                    let mut pitch: i32 = 0;
                    b2d.Lock2D(&mut scan0, &mut pitch).unwrap();
                    let slice =
                        std::slice::from_raw_parts(scan0, pitch as usize * (H as usize * 3 / 2));
                    (
                        slice.to_vec(),
                        pitch as usize,
                    )
                } else {
                    let mut ptr: *mut u8 = std::ptr::null_mut();
                    let mut max_len = 0u32;
                    let mut cur_len = 0u32;
                    buffer.Lock(&mut ptr, Some(&mut max_len), Some(&mut cur_len)).unwrap();
                    let slice = std::slice::from_raw_parts(ptr, cur_len as usize);
                    (slice.to_vec(), W as usize)
                };

                let cx = (W / 2) as usize;
                let cy = (H / 2) as usize;
                let y = data[cy * pitch + cx];
                let uv_off = pitch * H as usize;
                let uvi = (cy / 2) * pitch + (cx & !1);
                let cb = data[uv_off + uvi];
                let cr = data[uv_off + uvi + 1];
                return DecodedFrame { y, cb, cr };
            }
            index += 1;
        }
    }
}

fn make_config(dir: &std::path::Path, hw: HardwarePreference) -> EncoderConfig {
    EncoderConfig {
        width: W,
        height: H,
        fps: FPS,
        bitrate: default_bitrate(W, H, FPS).max(4_000_000),
        gop_seconds: 1,
        hardware: hw,
        partial_path: dir.join("out.mp4.partial"),
        final_path: dir.join("out.mp4"),
        audio: None,
    }
}

fn encode_solid(dir: &std::path::Path, hw: HardwarePreference) -> PathBuf {
    let (bgra, pitch) = solid_bgra(255, 0, 0, W, H);
    let view = BgraImageView {
        data: &bgra,
        pitch: pitch as usize,
        width: W,
        height: H,
    };
    let nv12 = bgra_to_nv12(&view).unwrap();
    assert_eq!(nv12.data[0], 63, "转换出的 Y 应为 BT.709 limited 的红色值");
    eprintln!("[stage] 转换完成");

    let cfg = make_config(dir, hw);
    let mut encoder = create_encoder(&cfg, None).expect("创建编码器失败");
    eprintln!("[stage] 编码器已创建");

    for n in 0..FRAMES {
        let pts = slot_time_mf(MfTime100ns(0), n, FPS);
        let duration = MfTime100ns(
            slot_time_mf(MfTime100ns(0), n + 1, FPS).0 - pts.0,
        );
        let frame = VideoFrame {
            data: VideoFrameData::Cpu(std::sync::Arc::new(nv12.clone())),
            pts,
            duration,
            sequence: n,
            discontinuity: false,
        };
        encoder.submit(&frame).unwrap_or_else(|e| panic!("第 {} 帧提交失败：{}", n, e));
        if n == 0 {
            eprintln!("[stage] 第 1 帧已提交");
        }
    }
    eprintln!("[stage] {} 帧全部提交", FRAMES);
    encoder.flush().unwrap();
    eprintln!("[stage] flush 完成（MF 后端为安全空实现）");
    let outcome = encoder.finish().expect("finalize 失败");
    eprintln!("[stage] finalize 完成，{} 字节", outcome.bytes);

    assert_eq!(outcome.path, cfg.final_path);
    assert!(outcome.bytes > 0, "输出文件为 0 字节");
    assert_eq!(outcome.samples, FRAMES);
    cfg.final_path
}

#[test]
fn test_encoder_roundtrip_color_and_file() {
    let _rt = match MfRuntime::start() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("跳过：Media Foundation 不可用（{}）", e);
            return;
        }
    };

    let dir = temp_dir("roundtrip");
    let path = encode_solid(&dir, HardwarePreference::PreferHardware);
    eprintln!("[stage] 文件：{:?}", path);

    // .partial 必须已被改名，且不再存在
    assert!(path.exists(), "最终 MP4 不存在");
    assert!(
        !dir.join("out.mp4.partial").exists(),
        ".partial 未清理"
    );

    let decoded = decode_center_pixel(&path, 5);

    // 期望：Y=63, Cb=102, Cr=240（BT.709 + limited range 的纯红）
    // H.264 是有损的，且色度是 4:2:0，允许小幅偏差
    let dy = (decoded.y as i32 - 63).abs();
    let dcb = (decoded.cb as i32 - 102).abs();
    let dcr = (decoded.cr as i32 - 240).abs();
    assert!(
        dy <= 6 && dcb <= 8 && dcr <= 8,
        "颜色偏差过大：解码得到 Y={} Cb={} Cr={}，期望约 Y=63 Cb=102 Cr=240。\
         这通常意味着色彩系数、NV12 平面布局或 pitch 处理有误。",
        decoded.y,
        decoded.cb,
        decoded.cr
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_software_path_also_works() {
    // PreferSoftware 是诊断开关，必须真实可用（证明硬件开关不是装饰）
    let _rt = match MfRuntime::start() {
        Ok(rt) => rt,
        Err(_) => return,
    };
    let dir = temp_dir("sw");
    let path = encode_solid(&dir, HardwarePreference::PreferSoftware);
    assert!(path.exists());
    let decoded = decode_center_pixel(&path, 5);
    assert!((decoded.y as i32 - 63).abs() <= 6, "软件路径颜色异常");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_odd_dimensions_are_rejected() {
    let _rt = match MfRuntime::start() {
        Ok(rt) => rt,
        Err(_) => return,
    };
    let dir = temp_dir("odd");
    let cfg = EncoderConfig {
        width: 1921,
        height: 1081,
        fps: FPS,
        bitrate: 4_000_000,
        gop_seconds: 2,
        hardware: HardwarePreference::PreferHardware,
        partial_path: dir.join("odd.mp4.partial"),
        final_path: dir.join("odd.mp4"),
        audio: None,
    };
    let err = match create_encoder(&cfg, None) {
        Ok(_) => panic!("奇数尺寸必须被拒绝"),
        Err(e) => e,
    };
    assert_eq!(err.code(), "FrameConversionFailed");
    let _ = std::fs::remove_dir_all(&dir);
}
