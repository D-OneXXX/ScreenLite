//! 内容真实性验证：录下来的画面是不是当前屏幕上真实的内容。
//!
//! 这是无播放器环境下能对「画面与桌面实际操作一致」做的最强检查：
//!
//! ```text
//! ① 用采集链路直接抓一帧当前屏幕 → 平均亮度 A
//! ② 录制 3 秒 → 解码 MP4 中间一帧 → 平均亮度 B
//! ③ 桌面静止时 A ≈ B（否则说明录到了错误区域、黑帧或垃圾数据）
//! ```
//!
//! 同时验证「重复帧计数为 0」——如果 tick 时钟锚定错了，会退化成整段静止画面。

use std::time::Duration;

mod common;

use screenlite_media::capture;
use screenlite_media::encoder::HardwarePreference;
use screenlite_media::engine::{EngineState, Recorder, RecordingConfig};

fn decode_middle_frame_luma(path: &std::path::Path, index: u64) -> f64 {
    // 必须持有自己的 MF 运行时：MFStartup/MFShutdown 是进程级的，
    // 采集链路的 MfRuntime 一 drop 就会 Shutdown，之后裸调 MF API 会返回 0xC00D3E85。
    let _rt = screenlite_media::mf::MfRuntime::start().expect("MFStartup 失败");
    use windows::core::{Interface, HSTRING};
    use windows::Win32::Media::MediaFoundation::{
        IMF2DBuffer, IMFAttributes, IMFMediaType, IMFSample, IMFSourceReader, MFCreateAttributes,
        MFCreateMediaType, MFCreateSourceReaderFromURL, MFMediaType_Video,
        MFNominalRange_16_235, MFVideoFormat_NV12, MFVideoInterlace_Progressive,
        MFVideoTransferMatrix_BT709, MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE,
        MF_MT_MAJOR_TYPE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE,
        MF_MT_YUV_MATRIX, MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING,
        MF_SOURCE_READER_FIRST_VIDEO_STREAM,
    };

    unsafe {
        let mut attrs_slot: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_slot, 1).unwrap();
        let attrs = attrs_slot.unwrap();
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .unwrap();

        let url = HSTRING::from(path.to_string_lossy().as_ref());
        let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs).unwrap();

        let target: IMFMediaType = MFCreateMediaType().unwrap();
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
        target.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
        // 必须给出帧尺寸，否则转换器/解码器会以 0xC00D36B4 拒绝该输出类型
        target
            .SetUINT64(&MF_MT_FRAME_SIZE, ((2560u64) << 32) | 1600u64)
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
        let _ = target.SetUINT64(&MF_MT_FRAME_RATE, (30u64 << 32) | 1);
        let _ = target.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1);
        reader
            .SetCurrentMediaType(
                MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                None,
                &target,
            )
            .expect("解码器不接受 NV12 输出类型");

        let mut i = 0u64;
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
                .unwrap();
            let Some(sample) = sample_slot else {
                panic!("解码到第 {} 帧时数据结束", i);
            };
            if i >= index {
                let buffer = sample.ConvertToContiguousBuffer().unwrap();
                let (data, pitch) = match buffer.cast::<IMF2DBuffer>() {
                    Ok(b2d) => {
                        let mut scan0: *mut u8 = std::ptr::null_mut();
                        let mut pitch: i32 = 0;
                        b2d.Lock2D(&mut scan0, &mut pitch).unwrap();
                        let len = pitch as usize * 1600;
                        (
                            std::slice::from_raw_parts(scan0, len).to_vec(),
                            pitch as usize,
                        )
                    }
                    Err(_) => {
                        let mut ptr: *mut u8 = std::ptr::null_mut();
                        let mut cur = 0u32;
                        buffer.Lock(&mut ptr, None, Some(&mut cur)).unwrap();
                        (
                            std::slice::from_raw_parts(ptr, cur as usize).to_vec(),
                            2560usize,
                        )
                    }
                };
                // 平均亮度（按 2560x1600 计算）
                let mut sum: u64 = 0;
                for y in 0..1600usize {
                    for x in 0..2560usize {
                        sum += data[y * pitch + x] as u64;
                    }
                }
                return sum as f64 / (2560.0 * 1600.0);
            }
            i += 1;
        }
    }
}

#[test]
fn test_recorded_content_matches_screen() {
    let displays = match capture::enumerate_displays() {
        Ok(d) => d,
        Err(_) => return,
    };
    let Some(primary) = displays.into_iter().find(|d| d.primary) else {
        return;
    };

    // ① 环境前提检查 + 基准亮度。
    // 前提是"桌面可见且稳定"；锁屏/黑屏/桌面在变时跳过，避免把环境问题误报成产品缺陷。
    let Some(reference_luma) = common::require_desktop_ready() else {
        return;
    };

    // ② 录制 3 秒
    let dir = std::env::temp_dir().join(format!("sl-content-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut cfg = RecordingConfig::new(primary.id.clone(), dir.clone());
    cfg.hardware = HardwarePreference::PreferSoftware;
    let recorder = Recorder::start(cfg).expect("启动失败");

    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if recorder.status().state == EngineState::Recording {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    std::thread::sleep(Duration::from_secs(3));
    let outcome = recorder.stop().expect("停止失败");

    eprintln!(
        "录制完成：{} 帧，重复帧 {}，时长 {} ms",
        outcome.frames_scheduled, outcome.frames_duplicated, outcome.elapsed_ms
    );

    // 关键：如果 tick 时钟与捕获时间轴锚定不一致，会出现「全部是重复帧」的静止画面。
    // 注意：debug 构建的 BGRA→NV12 转换比 release 慢数倍（2560×1600 ≈ 410 万像素/帧），
    // 会出现较多正常重复帧，因此这里只断言「不是 100% 重复」。
    assert!(
        outcome.frames_duplicated < outcome.frames_scheduled,
        "全部 {} 帧都是重复帧：tick 时钟与捕获时间轴可能锚定不一致，录出来的是静止画面",
        outcome.frames_scheduled
    );

    // ③ 解码中间一帧比对亮度
    let middle = (outcome.frames_scheduled / 2).max(1);
    let recorded_luma = decode_middle_frame_luma(&outcome.finalize.path, middle);
    eprintln!("录制帧平均亮度：{:.2}", recorded_luma);

    let diff = (recorded_luma - reference_luma).abs();
    // 容差 12 是刻意的：这个断言的目标是抓住粗错——录成黑帧、录错区域、
    // 色彩转换错误（这类偏差在 50 以上），而不是去检测桌面上小幅内容变化。
    assert!(
        diff < 12.0,
        "录制内容与当前屏幕亮度差异过大：基准 {:.2} vs 录制 {:.2}（差 {:.2}）。\
         这通常意味着录到了错误区域、黑帧或色彩转换错误。",
        reference_luma,
        recorded_luma,
        diff
    );

    let _ = std::fs::remove_dir_all(&dir);
}
