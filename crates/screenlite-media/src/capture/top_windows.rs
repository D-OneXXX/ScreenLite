//! 顶层窗口枚举（供"点一下选中整个窗口"的吸附功能使用）。
//!
//! 放在媒体层而不是 Tauri 壳层：按项目要求，所有 Windows API 代码集中在 windows-specific 模块，
//! 壳层只负责 IPC 与窗口。这里也只做"枚举 + 归一化"，不做任何 UI 决策。

use windows::core::BOOL; use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    IsWindowVisible,
};

/// 一个可吸附的顶层窗口。坐标已归一化到 0..1（相对显示器物理画幅），
/// 调用方（前端）因此完全不需要知道 DPI 与缩放。
#[derive(Debug, Clone)]
pub struct TopLevelWindow {
    pub title: String,
    pub nx: f64,
    pub ny: f64,
    pub nw: f64,
    pub nh: f64,
}

struct RawWindow {
    title: String,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<RawWindow>);

    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return true.into(); // 无标题的（桌面、各类工具窗）不参与吸附
    }
    // 跳过本进程自己的窗口（主窗口、框选覆盖层）
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return true.into();
    }

    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return true.into();
    }
    let (w, h) = (rect.right - rect.left, rect.bottom - rect.top);
    if w < 80 || h < 60 {
        return true.into(); // 太小（浮动工具条等）不值得吸附
    }

    let mut buf = vec![0u16; (len + 1) as usize];
    let copied = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..copied as usize]);
    if title.trim().is_empty() {
        return true.into();
    }

    list.push(RawWindow {
        title,
        x: rect.left,
        y: rect.top,
        w,
        h,
    });
    true.into()
}

/// 枚举可吸附的顶层窗口，并归一化到 `phys_w × phys_h` 画幅。
pub fn list_top_level_windows(phys_w: u32, phys_h: u32) -> Vec<TopLevelWindow> {
    if phys_w == 0 || phys_h == 0 {
        return Vec::new();
    }
    let (pw, ph) = (phys_w as f64, phys_h as f64);
    let mut raw: Vec<RawWindow> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut raw as *mut _ as isize));
    }

    let clamp01 = |v: f64| v.clamp(0.0, 1.0);
    raw.into_iter()
        .map(|r| {
            let nx = clamp01(r.x as f64 / pw);
            let ny = clamp01(r.y as f64 / ph);
            TopLevelWindow {
                title: r.title,
                nx,
                ny,
                nw: clamp01((r.x + r.w) as f64 / pw) - nx,
                nh: clamp01((r.y + r.h) as f64 / ph) - ny,
            }
        })
        .filter(|w| w.nw > 0.0 && w.nh > 0.0)
        .collect()
}

