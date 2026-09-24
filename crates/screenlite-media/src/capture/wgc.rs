//! Windows Graphics Capture 采集。
//!
//! 关键约束（每一条都对应一个真实事故）：
//!
//! 1. 使用 `CreateFreeThreaded`，`FrameArrived` 在 WGC 内部工作线程触发，不 marshal 到 UI 线程。
//! 2. 回调内顺序固定为 `CopyResource → frame.Close() → Map → 转换 → Unmap`，
//! 每帧必须 Close；忘记 Close 会让帧池缓冲区耗尽并静默停止回调（无任何报错）。
//! 3. 回调内不做编码、文件 IO、UI 交互与日志刷盘。

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use windows::core::{factory, Interface};
use windows::Foundation::TypedEventHandler;
use windows::Graphics::Capture::{
    Direct3D11CaptureFramePool, GraphicsCaptureItem, GraphicsCaptureSession,
};
use windows::Graphics::DirectX::DirectXPixelFormat;
use windows::Win32::Graphics::Gdi::HMONITOR;
use windows::Win32::Graphics::Direct3D11::{
    ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
};
use windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_B8G8R8A8_UNORM;
use windows::Win32::Graphics::Dxgi::IDXGIDevice;
use windows::Win32::System::WinRT::Direct3D11::{
    CreateDirect3D11DeviceFromDXGIDevice, IDirect3DDxgiInterfaceAccess,
};
use windows::Win32::System::WinRT::Graphics::Capture::IGraphicsCaptureItemInterop;

use crate::adapter::FrameAdapter;
use crate::consts::FRAME_POOL_BUFFERS;
use crate::convert::Nv12Image;
use crate::error::{MediaError, MediaResult};
use crate::time::Srt100ns;

/// 一帧采集结果：自包含 CPU NV12 缓冲 + 原始 SRT 时间戳。
///
/// 自包含是刻意的：帧不引用任何 D3D11 资源，
/// 因此停止时可以「先关采集、后 drain 队列」而不悬挂引用。
#[derive(Debug)]
pub struct WgcFrame {
    pub nv12: Nv12Image,
    pub srt: Srt100ns,
    pub sequence: u64,
}

type FrameSink = Box<dyn Fn(WgcFrame) + Send + Sync>;

pub struct WgcCapture {
    session: GraphicsCaptureSession,
    pool: Direct3D11CaptureFramePool,
    handler_token: i64,
    size: (u32, u32),
    shared: Arc<Shared>,
}

struct Shared {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    adapter: Mutex<Option<FrameAdapter>>,
    /// 区域录制的裁剪矩形（None = 整屏）
    region: Option<crate::convert::Region>,
    sink: FrameSink,
    sequence: AtomicU64,
    frames: AtomicU64,
    /// 采集起点，用于 stall 检测（不是视频时间基准）。
    started_at: Instant,
    /// 最后一次回调时间（相对 started_at 的毫秒数）。
    last_callback_ms: AtomicU64,
    last_srt: AtomicU64,
    fatal: Mutex<Option<MediaError>>,
    border_required: AtomicU64,
    /// 正在执行中的回调数量。
    ///
    /// 用途：`Close()` 返回不代表回调已经退出。若此时调用方拆掉 COM/MF 运行时
    /// （`CoUninitialize` / `MFShutdown`），而 WGC 工作线程仍在回调里，就会访问违例。
    /// [`WgcCapture::stop`] 必须等它归零。
    in_flight: AtomicU64,
}

/// 回调进入/退出的计数守卫（保证任何 return 路径都会减一）。
struct InFlightGuard<'a>(&'a AtomicU64);

impl Drop for InFlightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::AcqRel);
    }
}

/// 线程安全性说明（受控断言，不是随手加的 unsafe）：
///
/// - 本结构在媒体线程创建，并被 `FrameArrived` 回调在 WGC 内部工作线程上使用；
/// - D3D11 device/context 已开启多线程保护（`ID3D11Multithread::SetMultithreadProtected`），
/// 这正是 MSDN 对「多线程共享同一 D3D11 设备」的要求；
/// - 线程内不并发访问：staging 与转换由 `Mutex` 串行化；
/// - 两个线程都在同一 MTA（`CoInitializeEx(COINIT_MULTITHREADED)`），不存在跨 apartment 调用；
/// - windows-rs 的 COM 指针不实现 `Send`，因此这里必须显式断言。
unsafe impl Send for Shared {}
unsafe impl Sync for Shared {}

impl WgcCapture {
    /// 第一步：创建捕获项并读出权威采集尺寸（物理像素）。
    ///
    /// 之所以拆成两步：区域录制需要先用真实采集尺寸把区域裁剪到画幅内，
    /// 而这个尺寸只有在捕获项创建后才能拿到（GDI 给的是逻辑尺寸，不能用）。
    pub fn create_item(hmonitor: HMONITOR) -> MediaResult<(GraphicsCaptureItem, (u32, u32))> {
        if !super::is_supported() {
            return Err(MediaError::ScreenCaptureUnsupported);
        }
        unsafe {
            let item = create_item_for_monitor(hmonitor)?;
            let size = item.Size().map_err(|e| MediaError::win32("item.Size", e))?;
            let size = (size.Width as u32, size.Height as u32);
            if size.0 == 0 || size.1 == 0 {
                return Err(MediaError::Internal(format!(
                    "捕获项尺寸非法：{}x{}",
                    size.0, size.1
                )));
            }
            Ok((item, size))
        }
    }

    /// 整屏录制入口（区域为 None 的便捷路径，也供单帧抓取使用）。
    pub fn start(
        hmonitor: HMONITOR,
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        region: Option<crate::convert::Region>,
        sink: impl Fn(WgcFrame) + Send + Sync + 'static,
    ) -> MediaResult<Self> {
        let (item, size) = Self::create_item(hmonitor)?;
        Self::start_with_item(item, size, device, context, region, sink)
    }

    /// 第二步：用已创建的捕获项启动会话。
    ///
    /// `sink` 会在 WGC 的工作线程上被调用；实现必须快速返回（只做入队，不做编码）。
    /// `region` 为 Some 时只转换该子矩形（采集始终是整块显示器，裁剪在转换阶段完成）。
    pub fn start_with_item(
        item: GraphicsCaptureItem,
        size: (u32, u32),
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        region: Option<crate::convert::Region>,
        sink: impl Fn(WgcFrame) + Send + Sync + 'static,
    ) -> MediaResult<Self> {
        unsafe {
            // 3. D3D11 device → WinRT IDirect3DDevice
            let dxgi_device: IDXGIDevice = device
                .cast()
                .map_err(|e| MediaError::win32("ID3D11Device->IDXGIDevice", e))?;
            let inspectable = CreateDirect3D11DeviceFromDXGIDevice(&dxgi_device)
                .map_err(|e| MediaError::win32("CreateDirect3D11DeviceFromDXGIDevice", e))?;
            let rt_device: windows::Graphics::DirectX::Direct3D11::IDirect3DDevice = inspectable
                .cast()
                .map_err(|e| MediaError::win32("IInspectable->IDirect3DDevice", e))?;

            // 4. FreeThreaded 帧池（后台工作线程回调）
            let pool = Direct3D11CaptureFramePool::CreateFreeThreaded(
                &rt_device,
                DirectXPixelFormat::B8G8R8A8UIntNormalized,
                FRAME_POOL_BUFFERS,
                item.Size().map_err(|e| MediaError::win32("item.Size", e))?,
            )
            .map_err(MediaError::FramePoolCreateFailed)?;

            let session = pool
                .CreateCaptureSession(&item)
                .map_err(|e| MediaError::win32("CreateCaptureSession", e))?;

            // 5. 会话参数：必须包含鼠标指针（验收要求画面与操作一致）
            session
                .SetIsCursorCaptureEnabled(true)
                .map_err(|e| MediaError::win32("SetIsCursorCaptureEnabled", e))?;

            // 边框：V0.1 明确「允许系统边框存在」，不请求 Borderless 授权，不尝试绕过。
            let border_required = session.IsBorderRequired().unwrap_or(true);

            let shared = Arc::new(Shared {
                device: device.clone(),
                context: context.clone(),
                adapter: Mutex::new(None),
                region,
                sink: Box::new(sink),
                sequence: AtomicU64::new(0),
                frames: AtomicU64::new(0),
                started_at: Instant::now(),
                last_callback_ms: AtomicU64::new(0),
                last_srt: AtomicU64::new(0),
                fatal: Mutex::new(None),
                border_required: AtomicU64::new(border_required as u64),
                in_flight: AtomicU64::new(0),
            });

            let cb_shared = shared.clone();
            // 事件参数类型未被投影（crate 里是 IInspectable）；帧本身从 pool 取，
            // 因此这里不需要 args。
            let handler = TypedEventHandler::<
                Direct3D11CaptureFramePool,
                windows::core::IInspectable,
            >::new(move |pool, _args| {
                if let Some(pool) = pool.as_ref() {
                    on_frame_arrived(pool, &cb_shared);
                }
                Ok(())
            });

            let handler_token = pool
                .FrameArrived(&handler)
                .map_err(|e| MediaError::win32("FrameArrived", e))?;

            session
                .StartCapture()
                .map_err(MediaError::CaptureSessionStartFailed)?;

            Ok(Self {
                session,
                pool,
                handler_token,
                size,
                shared,
            })
        }
    }

    /// 权威采集尺寸（物理像素）。
    pub fn size(&self) -> (u32, u32) {
        self.size
    }

    pub fn frames_captured(&self) -> u64 {
        self.shared.frames.load(Ordering::Relaxed)
    }

    /// 距最后一次帧回调过去了多少毫秒（用于 stall 检测）。
    pub fn ms_since_last_callback(&self) -> u64 {
        let now = self.shared.started_at.elapsed().as_millis() as u64;
        now.saturating_sub(self.shared.last_callback_ms.load(Ordering::Relaxed))
    }

    pub fn border_required(&self) -> bool {
        self.shared.border_required.load(Ordering::Relaxed) != 0
    }

    /// 回调线程捕获到的第一个致命错误。
    pub fn take_fatal(&self) -> Option<MediaError> {
        self.shared.fatal.lock().unwrap().take()
    }

    /// 停止采集：退订回调 → 关闭会话 → 关闭帧池 → 等待在途回调退出。
    ///
    /// 最后一步是必需的：`Close()` 返回不代表回调已结束。如果调用方紧接着
    /// 释放 COM/MF 运行时（例如 `MfRuntime` 析构触发 `MFShutdown` + `CoUninitialize`），
    /// 而 WGC 工作线程仍在回调中，会产生访问违例（实测 0xc0000005）。
    ///
    /// 必须在 drain 队列之前调用，且不能强杀线程。
    pub fn stop(self) {
        let _ = self.pool.RemoveFrameArrived(self.handler_token);
        let _ = self.session.Close();
        let _ = self.pool.Close();

        let deadline = Instant::now() + Duration::from_millis(2000);
        while self.shared.in_flight.load(Ordering::Acquire) > 0 {
            if Instant::now() > deadline {
                tracing::warn!("等待 WGC 在途回调超时（2s），继续释放");
                break;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }
}

unsafe fn create_item_for_monitor(hmonitor: HMONITOR) -> MediaResult<GraphicsCaptureItem> {
    // 保底路径：官方 interop（所有支持 WGC 的系统都可用）
    let interop: IGraphicsCaptureItemInterop = factory::<GraphicsCaptureItem, IGraphicsCaptureItemInterop>()
        .map_err(|e| MediaError::win32("GraphicsCaptureItem factory", e))?;
    interop
        .CreateForMonitor::<GraphicsCaptureItem>(hmonitor)
        .map_err(|e| MediaError::win32("CreateForMonitor", e))
}

unsafe fn on_frame_arrived(pool: &Direct3D11CaptureFramePool, shared: &Arc<Shared>) {
    // 进入即计数，任何 return 路径都会由守卫归零（stop() 依赖它判断静默）
    shared.in_flight.fetch_add(1, Ordering::AcqRel);
    let _in_flight = InFlightGuard(&shared.in_flight);

    let frame = match pool.TryGetNextFrame() {
        Ok(f) => f,
        // 没有可用帧是正常情况（例如已停止），不是错误
        Err(_) => return,
    };

    let result = (|| -> MediaResult<(Nv12Image, Srt100ns)> {
        let srt = frame
            .SystemRelativeTime()
            .map_err(|e| MediaError::win32("SystemRelativeTime", e))?;
        let content = frame
            .ContentSize()
            .map_err(|e| MediaError::win32("ContentSize", e))?;
        let cw = content.Width as u32;
        let ch = content.Height as u32;

        let surface = frame
            .Surface()
            .map_err(|e| MediaError::win32("frame.Surface", e))?;
        let access: IDirect3DDxgiInterfaceAccess = surface
            .cast()
            .map_err(|e| MediaError::win32("IDirect3DDxgiInterfaceAccess", e))?;
        let texture: ID3D11Texture2D = access
            .GetInterface()
            .map_err(|e| MediaError::win32("GetInterface<ID3D11Texture2D>", e))?;

        // 运行期格式校验：HDR 显示器可能返回 FP16，绝不当 BGRA 硬转
        let mut desc = std::mem::zeroed::<windows::Win32::Graphics::Direct3D11::D3D11_TEXTURE2D_DESC>();
        texture.GetDesc(&mut desc);
        if desc.Format != DXGI_FORMAT_B8G8R8A8_UNORM {
            return Err(MediaError::CaptureFormatUnsupported {
                actual: format!("{:?}", desc.Format),
            });
        }

        // 尺寸变化：帧池与编码器都不支持中途改尺寸。V0.1 不做流水线重建，
        // 由 Engine 检测「帧尺寸 != 配置尺寸」后报错停止，绝不产出损坏文件。
        if cw == 0 || ch == 0 {
            return Err(MediaError::Internal("ContentSize 为 0".into()));
        }

        let mut guard = shared.adapter.lock().unwrap();
        let adapter = guard.get_or_insert_with(|| {
            FrameAdapter::new(shared.context.clone(), shared.region)
        });
        // CopyResource 后立即 Close，把帧持有时间压到最短
        adapter.stage(&shared.device, &texture, (cw, ch))?;
        let _ = frame.Close();
        let nv12 = adapter.readback_and_convert()?;
        Ok((nv12, Srt100ns(srt.Duration)))
    })();

    match result {
        Ok((nv12, srt)) => {
            let seq = shared.sequence.fetch_add(1, Ordering::Relaxed);
            shared.frames.fetch_add(1, Ordering::Relaxed);
            shared
                .last_callback_ms
                .store(shared.started_at.elapsed().as_millis() as u64, Ordering::Relaxed);
            shared.last_srt.store(srt.0 as u64, Ordering::Relaxed);
            (shared.sink)(WgcFrame {
                nv12,
                srt,
                sequence: seq,
            });
        }
        Err(err) => {
            // 回调内不得 panic、不得向外传播错误；只记录第一个致命错误
            let mut slot = shared.fatal.lock().unwrap();
            if slot.is_none() {
                *slot = Some(err);
            }
        }
    }
}
