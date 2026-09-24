//! Media Foundation / COM 运行时生命周期。
//!
//! 关键决策：按进程生命周期初始化，永不显式拆掉。
//!
//! 顺序要求：先 `CoInitializeEx(MTA)`，再 `MFStartup`。
//!
//! 为什么 `Drop` 里故意不调用 `MFShutdown` / `CoUninitialize`：
//! 这是实测出来的事故，不是偷懒——
//!
//! ```text
//! 现象：一次抓帧后再启动录制，进程 0xc0000005（STATUS_ACCESS_VIOLATION）
//! 根因：抓帧结束时 MfRuntime 析构 → MFShutdown + CoUninitialize 在进程级
//! 拆掉 MF 平台与 COM 公寓；但 WGC 的内部工作线程与已创建的 COM 对象仍然活着，
//! 它们随后访问已拆掉的公寓 → 崩溃。
//! ```
//!
//! 同理，`MFStartup` 只做一次（进程级），避免反复 Startup/Shutdown 造成
//! 编解码器与 MFT 缓存被反复摧毁——这也是「快速反复启停后采集吞吐退化」的可疑来源之一。
//!
//! 参考：Media Foundation 的运行时不需要显式 Shutdown，进程退出会释放；
//! 主流录屏实现同样把 MF 初始化放在进程级。

use std::sync::OnceLock;

use windows::Win32::Foundation::RPC_E_CHANGED_MODE;
use windows::Win32::Media::MediaFoundation::{MFStartup, MFSTARTUP_FULL, MF_VERSION};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_MULTITHREADED};

use crate::error::{MediaError, MediaResult};

/// 进程级 MF 启动结果（只启动一次，永不停止）。
static MF_START: OnceLock<Result<(), String>> = OnceLock::new();

/// 持有 MF 运行时的句柄。
///
/// 语义上等价于「确保 MF 已按进程生命周期启动」，析构不做任何拆卸。
pub struct MfRuntime {
    _priv: (),
}

impl MfRuntime {
    /// 在当前线程初始化 COM（MTA）并确保进程级 MF 已启动。
    ///
    /// 每个需要使用 COM/MF 的线程都必须调用一次。
    pub fn start() -> MediaResult<Self> {
        // COM 是每线程状态：每个调用线程都要自己初始化（重复调用返回 S_FALSE 也正常）
        unsafe {
            let hr = CoInitializeEx(None, COINIT_MULTITHREADED);
            if hr.is_err() && hr != RPC_E_CHANGED_MODE {
                return Err(MediaError::win32(
                    "CoInitializeEx",
                    windows::core::Error::from_hresult(hr),
                ));
            }
        }

        // MF 是进程级资源：只启动一次，且永不 Shutdown
        let init = MF_START.get_or_init(|| unsafe {
            match MFStartup(MF_VERSION, MFSTARTUP_FULL) {
                Ok(()) => Ok(()),
                Err(e) => Err(format!("MFStartup 失败：{e}")),
            }
        });
        match init {
            Ok(()) => Ok(Self { _priv: () }),
            Err(msg) => Err(MediaError::Internal(msg.clone())),
        }
    }
}

impl Drop for MfRuntime {
    fn drop(&mut self) {
        // 故意不做 MFShutdown / CoUninitialize。
        // 进程级拆卸会在 WGC 工作线程或 COM 对象仍存活时导致访问违例（实测 0xc0000005）。
        // 进程退出时由操作系统回收。
    }
}
