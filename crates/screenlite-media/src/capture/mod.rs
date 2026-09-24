//! 采集层：Windows Graphics Capture + Direct3D 11。

mod display;
mod top_windows;
mod wgc;

pub use display::{list_displays, resolve_display, DisplayInfo, GdiDeviceName, ResolvedDisplay};
pub use top_windows::{list_top_level_windows, TopLevelWindow};
pub use wgc::{WgcCapture, WgcFrame};

use crate::error::{MediaError, MediaResult};

/// 采集会话是否受支持。必须在开始录制前调用。
pub fn is_supported() -> bool {
    use windows::Graphics::Capture::GraphicsCaptureSession;
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// 采集层可用的显示器枚举结果。空列表说明系统没有任何可用显示器。
pub fn enumerate_displays() -> MediaResult<Vec<DisplayInfo>> {
    list_displays()
}

/// 抓取单帧（自检 / 预览用）。会创建独立的短生命周期设备与采集会话。
pub fn capture_single_frame(
    display_id: &str,
    timeout: std::time::Duration,
) -> MediaResult<crate::convert::Nv12Image> {
    use std::sync::{Arc, Mutex};

    let _rt = crate::mf::MfRuntime::start()?;
    let resolved = resolve_display(display_id)?;
    if resolved.info.hdr {
        return Err(MediaError::HdrDisplayNotSupported);
    }
    let (device, context) = create_d3d11_device()?;
    let slot: Arc<Mutex<Option<crate::convert::Nv12Image>>> = Arc::new(Mutex::new(None));
    let sink_slot = slot.clone();
    let capture = WgcCapture::start(resolved.hmonitor, &device, &context, None, move |f| {
        let mut guard = sink_slot.lock().unwrap();
        if guard.is_none() {
            *guard = Some(f.nv12);
        }
    })?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        if let Some(img) = slot.lock().unwrap().take() {
            // stop() 内部会等待在途回调退出，因此这里不需要额外的 sleep
            capture.stop();
            return Ok(img);
        }
        if std::time::Instant::now() > deadline {
            capture.stop();
            return Err(MediaError::CaptureNoFrames(timeout.as_secs()));
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
}

/// 读取显示器的权威采集尺寸（物理像素），不启动采集会话。
///
/// 用途：框选覆盖层需要把用户拖出的选区（归一化坐标）换算成物理像素，
/// 而物理尺寸只有 `GraphicsCaptureItem.Size()` 是权威的（GDI 给的是逻辑尺寸）。
pub fn capture_size_of(display_id: &str) -> MediaResult<(u32, u32)> {
    let _rt = crate::mf::MfRuntime::start()?;
    let resolved = resolve_display(display_id)?;
    let (_item, size) = WgcCapture::create_item(resolved.hmonitor)?;
    Ok(size)
}

/// 主显示器的稳定 ID（首次启动的默认选择）。
pub fn primary_display_id() -> Option<String> {
    display::primary_display_id()
}

/// 创建采集/编码共用的 D3D11 设备。
///
/// 三个标志都是必需的：
/// - `BGRA_SUPPORT`：WGC 要求（BGRA 表面）
/// - `VIDEO_SUPPORT`：视频处理与硬件 MFT 路径需要
/// - 多线程保护在 [`enable_multithread_protection`] 里开启
pub fn create_d3d11_device(
) -> MediaResult<(windows::Win32::Graphics::Direct3D11::ID3D11Device, windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext)>
{
    use windows::Win32::Graphics::Direct3D::{
        D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_10_1, D3D_FEATURE_LEVEL_11_0,
        D3D_FEATURE_LEVEL_11_1,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CREATE_DEVICE_VIDEO_SUPPORT,
        D3D11_SDK_VERSION, ID3D11Device, ID3D11DeviceContext,
    };
    use windows::Win32::Graphics::Dxgi::IDXGIAdapter;

    unsafe {
        let flags = D3D11_CREATE_DEVICE_BGRA_SUPPORT | D3D11_CREATE_DEVICE_VIDEO_SUPPORT;
        let mut device: Option<ID3D11Device> = None;
        let mut context: Option<ID3D11DeviceContext> = None;

        let try_create = |levels: &[windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL],
                          device: &mut Option<ID3D11Device>,
                          context: &mut Option<ID3D11DeviceContext>|
         -> windows::core::Result<()> {
            D3D11CreateDevice(
                None::<&IDXGIAdapter>,
                D3D_DRIVER_TYPE_HARDWARE,
                Default::default(),
                flags,
                Some(levels),
                D3D11_SDK_VERSION,
                Some(device),
                None,
                Some(context),
            )
        };

        // 请求 11_1；个别驱动不支持时整体失败，因此回退到 11_0
        if try_create(
            &[
                D3D_FEATURE_LEVEL_11_1,
                D3D_FEATURE_LEVEL_11_0,
                D3D_FEATURE_LEVEL_10_1,
            ],
            &mut device,
            &mut context,
        )
        .is_err()
        {
            device = None;
            context = None;
            try_create(
                &[D3D_FEATURE_LEVEL_11_0, D3D_FEATURE_LEVEL_10_1],
                &mut device,
                &mut context,
            )
            .map_err(|e| MediaError::win32("D3D11CreateDevice", e))?;
        }

        match (device, context) {
            (Some(d), Some(c)) => Ok((d, c)),
            _ => Err(MediaError::Internal("D3D11CreateDevice 未返回设备".into())),
        }
    }
}

/// 开启 D3D11 多线程保护。MF 与 WGC 工作线程会共用同一个设备，这是既有要求。
pub fn enable_multithread_protection(
    context: &windows::Win32::Graphics::Direct3D11::ID3D11DeviceContext,
) -> MediaResult<()> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D11::ID3D11Multithread;
    let mt: ID3D11Multithread = context
        .cast()
        .map_err(|e| MediaError::win32("QueryInterface(ID3D11Multithread)", e))?;
    unsafe {
        let _ = mt.SetMultithreadProtected(true);
    }
    Ok(())
}

/// 创建交给 Sink Writer 的 DXGI device manager，让硬件 MFT 在我们的设备上工作。
pub fn create_device_manager(
    device: &windows::Win32::Graphics::Direct3D11::ID3D11Device,
) -> MediaResult<windows::Win32::Media::MediaFoundation::IMFDXGIDeviceManager> {
    use windows::Win32::Media::MediaFoundation::{MFCreateDXGIDeviceManager, IMFDXGIDeviceManager};
    unsafe {
        let mut token = 0u32;
        let mut manager: Option<IMFDXGIDeviceManager> = None;
        MFCreateDXGIDeviceManager(&mut token, &mut manager)
            .map_err(|e| MediaError::win32("MFCreateDXGIDeviceManager", e))?;
        let manager = manager
            .ok_or_else(|| MediaError::Internal("MFCreateDXGIDeviceManager 未返回对象".into()))?;
        manager
            .ResetDevice(device, token)
            .map_err(|e| MediaError::win32("IMFDXGIDeviceManager::ResetDevice", e))?;
        Ok(manager)
    }
}


