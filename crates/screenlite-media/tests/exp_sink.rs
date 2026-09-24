//! 对照实验 2：2D buffer vs 1D memory buffer，定位样本未被处理的原因。
//! 运行：cargo test --test exp_sink -- --nocapture

use screenlite_media::mf::MfRuntime;

use windows::core::Interface;
use windows::Win32::Media::MediaFoundation::{
    IMFSinkWriter, IMFAttributes, IMFMediaType, IMF2DBuffer, MFCreate2DMediaBuffer,
    MFCreateAttributes, MFCreateMediaType, MFCreateMemoryBuffer, MFCreateSample,
    MFCreateSinkWriterFromURL, MFGetStrideForBitmapInfoHeader, MFMediaType_Video,
    MFNominalRange_16_235, MFVideoFormat_H264, MFVideoFormat_NV12,
    MFVideoInterlace_Progressive, MFVideoTransferMatrix_BT709, MF_MT_AVG_BITRATE,
    MF_MT_FRAME_RATE, MF_MT_FRAME_SIZE, MF_MT_INTERLACE_MODE, MF_MT_MAJOR_TYPE,
    MF_MT_MPEG2_PROFILE, MF_MT_PIXEL_ASPECT_RATIO, MF_MT_SUBTYPE, MF_MT_VIDEO_NOMINAL_RANGE,
    MF_MT_YUV_MATRIX, eAVEncH264VProfile_High,
};

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 30;
const NV12_FOURCC: u32 = 0x3231_564E;

unsafe fn make_output_type() -> IMFMediaType {
    let t = MFCreateMediaType().unwrap();
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
    t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_H264).unwrap();
    t.SetUINT32(&MF_MT_AVG_BITRATE, 4_000_000).unwrap();
    t.SetUINT64(&MF_MT_FRAME_SIZE, ((W as u64) << 32) | H as u64).unwrap();
    t.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1).unwrap();
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).unwrap();
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).unwrap();
    t.SetUINT32(&MF_MT_MPEG2_PROFILE, eAVEncH264VProfile_High.0 as u32).unwrap();
    t
}

unsafe fn make_input_type() -> IMFMediaType {
    let t = MFCreateMediaType().unwrap();
    t.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
    t.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
    t.SetUINT64(&MF_MT_FRAME_SIZE, ((W as u64) << 32) | H as u64).unwrap();
    t.SetUINT64(&MF_MT_FRAME_RATE, ((FPS as u64) << 32) | 1).unwrap();
    t.SetUINT64(&MF_MT_PIXEL_ASPECT_RATIO, (1u64 << 32) | 1).unwrap();
    t.SetUINT32(&MF_MT_INTERLACE_MODE, MFVideoInterlace_Progressive.0 as u32).unwrap();
    t.SetUINT32(&MF_MT_YUV_MATRIX, MFVideoTransferMatrix_BT709.0 as u32).unwrap();
    t.SetUINT32(&MF_MT_VIDEO_NOMINAL_RANGE, MFNominalRange_16_235.0 as u32).unwrap();
    t
}

/// 用 2D buffer（MFCreate2DMediaBuffer + Lock2D）
unsafe fn write_2d(writer: &IMFSinkWriter, idx: u32, n: u64) -> Result<(), String> {
    let buffer = MFCreate2DMediaBuffer(W, H, NV12_FOURCC, false)
        .map_err(|e| format!("MFCreate2DMediaBuffer {:#x}", e.code().0))?;
    let b2d: IMF2DBuffer = buffer.cast().unwrap();
    let mut scan0: *mut u8 = std::ptr::null_mut();
    let mut pitch: i32 = 0;
    b2d.Lock2D(&mut scan0, &mut pitch).unwrap();
    for y in 0..H as usize {
        std::ptr::write_bytes(scan0.add(y * pitch as usize), 63, W as usize);
    }
    let uv = scan0.add(pitch as usize * H as usize);
    for y in 0..H as usize / 2 {
        for x in 0..W as usize / 2 {
            *uv.add(y * pitch as usize + x * 2) = 102;
            *uv.add(y * pitch as usize + x * 2 + 1) = 240;
        }
    }
    b2d.Unlock2D().unwrap();
    let contiguous = b2d.GetContiguousLength().unwrap();
    buffer.SetCurrentLength(contiguous).unwrap();

    let sample = MFCreateSample().unwrap();
    sample.AddBuffer(&buffer).unwrap();
    sample.SetSampleTime(n as i64 * 333_333).unwrap();
    sample.SetSampleDuration(333_333).unwrap();
    writer
        .WriteSample(idx, &sample)
        .map_err(|e| format!("WriteSample(2D) {:#x}", e.code().0))
}

/// 用经典 1D memory buffer（stride = width）
unsafe fn write_1d(writer: &IMFSinkWriter, idx: u32, n: u64) -> Result<(), String> {
    let len = W * H * 3 / 2;
    let buffer = MFCreateMemoryBuffer(len)
        .map_err(|e| format!("MFCreateMemoryBuffer {:#x}", e.code().0))?;
    let mut ptr: *mut u8 = std::ptr::null_mut();
    buffer.Lock(&mut ptr, None, None)
        .map_err(|e| format!("Lock {:#x}", e.code().0))?;
    for y in 0..H as usize {
        std::ptr::write_bytes(ptr.add(y * W as usize), 63, W as usize);
    }
    let uv = ptr.add((W * H) as usize);
    for y in 0..H as usize / 2 {
        for x in 0..W as usize / 2 {
            *uv.add(y * W as usize + x * 2) = 102;
            *uv.add(y * W as usize + x * 2 + 1) = 240;
        }
    }
    buffer.Unlock().unwrap();
    buffer.SetCurrentLength(len).unwrap();

    let sample = MFCreateSample().unwrap();
    sample.AddBuffer(&buffer).unwrap();
    sample.SetSampleTime(n as i64 * 333_333).unwrap();
    sample.SetSampleDuration(333_333).unwrap();
    writer
        .WriteSample(idx, &sample)
        .map_err(|e| format!("WriteSample(1D) {:#x}", e.code().0))
}

fn case(name: &str, path: &std::path::Path, use_2d: bool, hw: bool) {
    unsafe {
        println!("--- {} ---", name);
        let mut attrs_slot: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_slot, 2).unwrap();
        let attrs = attrs_slot.unwrap();
        attrs
            .SetUINT32(
                &windows::Win32::Media::MediaFoundation::MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                hw as u32,
            )
            .unwrap();

        let url = windows::core::HSTRING::from(path.to_string_lossy().as_ref());
        let writer = match MFCreateSinkWriterFromURL(
            &url,
            None::<&windows::Win32::Media::MediaFoundation::IMFByteStream>,
            &attrs,
        ) {
            Ok(w) => w,
            Err(e) => {
                println!("  创建 writer 失败：{:#x}", e.code().0);
                return;
            }
        };
        let idx = writer.AddStream(&make_output_type()).unwrap();
        if let Err(e) = writer.SetInputMediaType(idx, &make_input_type(), None::<&IMFAttributes>) {
            println!("  SetInputMediaType 失败：{:#x} {}", e.code().0, e.message());
            return;
        }
        writer.BeginWriting().unwrap();

        let mut first_err = None;
        for n in 0..30u64 {
            let r = if use_2d {
                write_2d(&writer, idx, n)
            } else {
                write_1d(&writer, idx, n)
            };
            if let Err(msg) = r {
                if first_err.is_none() {
                    first_err = Some(msg);
                }
            }
        }
        if let Some(m) = &first_err {
            println!("  首次写入错误：{}", m);
        }
        match writer.Finalize() {
            Ok(()) => println!("  Finalize OK"),
            Err(e) => println!("  Finalize 失败：{:#x} {}", e.code().0, e.message()),
        }
        println!(
            "  文件大小：{} 字节",
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        );
    }
}

/// 用例 D/E/F：URL 只用来推断容器（.mp4），实际写入自己提供的 .partial 字节流。
/// `use_2d` 与 `set_hw_flag` 用来二分实现与实验之间的差异。
fn case_partial_with_own_stream(dir: &std::path::Path, tag: &str, use_2d: bool, set_hw_flag: bool) {
    use windows::Win32::Media::MediaFoundation::MFCreateMFByteStreamOnStream;
    use windows::Win32::System::Com::{STGM_CREATE, STGM_READWRITE, STGM_SHARE_DENY_WRITE};
    use windows::Win32::UI::Shell::SHCreateStreamOnFileEx;

    let final_path = dir.join(format!("{}_final.mp4", tag));
    let partial_path = dir.join(format!("{}_final.mp4.partial", tag));
    unsafe {
        println!("--- 用例 {}：own stream, 2D={}, hw_flag={} ---", tag, use_2d, set_hw_flag);
        let mut attrs_slot: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut attrs_slot, 2).unwrap();
        let attrs = attrs_slot.unwrap();
        if set_hw_flag {
            attrs
                .SetUINT32(
                    &windows::Win32::Media::MediaFoundation::MF_READWRITE_ENABLE_HARDWARE_TRANSFORMS,
                    0,
                )
                .unwrap();
        }

        let partial_w = windows::core::HSTRING::from(partial_path.to_string_lossy().as_ref());
        let stream = match SHCreateStreamOnFileEx(
            &partial_w,
            STGM_CREATE.0 | STGM_READWRITE.0 | STGM_SHARE_DENY_WRITE.0,
            0,
            true,
            None::<&windows::Win32::System::Com::IStream>,
        ) {
            Ok(s) => s,
            Err(e) => {
                println!("  SHCreateStreamOnFileEx 失败：{:#x}", e.code().0);
                return;
            }
        };
        let byte_stream = MFCreateMFByteStreamOnStream(&stream).unwrap();

        let url = windows::core::HSTRING::from(final_path.to_string_lossy().as_ref());
        let writer = match MFCreateSinkWriterFromURL(&url, &byte_stream, &attrs) {
            Ok(w) => w,
            Err(e) => {
                println!("  创建 writer 失败：{:#x} {}", e.code().0, e.message());
                return;
            }
        };
        let idx = writer.AddStream(&make_output_type()).unwrap();
        if let Err(e) = writer.SetInputMediaType(idx, &make_input_type(), None::<&IMFAttributes>) {
            println!("  SetInputMediaType 失败：{:#x} {}", e.code().0, e.message());
            return;
        }
        writer.BeginWriting().unwrap();
        let mut first_err = None;
        for n in 0..30u64 {
            let r = if use_2d {
                write_2d(&writer, idx, n)
            } else {
                write_1d(&writer, idx, n)
            };
            if let Err(msg) = r {
                if first_err.is_none() {
                    first_err = Some(msg);
                }
            }
        }
        if let Some(m) = &first_err {
            println!("  首次写入错误：{}", m);
        }
        match writer.Finalize() {
            Ok(()) => println!("  Finalize OK"),
            Err(e) => println!("  Finalize 失败：{:#x} {}", e.code().0, e.message()),
        }
        println!(
            "  .partial 大小={}",
            std::fs::metadata(&partial_path).map(|m| m.len()).unwrap_or(0)
        );
    }
}

#[test]
fn exp_buffers() {
    let _rt = MfRuntime::start().unwrap();
    unsafe {
        let stride = MFGetStrideForBitmapInfoHeader(MFVideoFormat_NV12.data1, W).unwrap();
        println!("MFGetStrideForBitmapInfoHeader(NV12, {}) = {}", W, stride);
    }

    let dir = std::env::temp_dir().join(format!("sl-exp2-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();

    case("2D buffer + 软件编码", &dir.join("a_2d_sw.mp4"), true, false);
    case("1D buffer + 软件编码", &dir.join("b_1d_sw.mp4"), false, false);
    case("1D buffer + 硬件编码", &dir.join("c_1d_hw.mp4"), false, true);
    case_partial_with_own_stream(&dir, "d", false, false);
    case_partial_with_own_stream(&dir, "e", true, false);
    case_partial_with_own_stream(&dir, "f", false, true);
}
