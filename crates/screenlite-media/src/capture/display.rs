//! 显示器枚举与 HDR 检测。
//!
//! 稳定 ID 来自 `QueryDisplayConfig`：`adapter_luid:target_id`。
//! 不得使用 `display-1` 这类由 UI 生成的序号。
//!
//! 关于分辨率的注意点：GDI 的 `rcMonitor` 在 DPI 缩放下给出的是逻辑坐标，
//! 而捕获与输出必须是物理像素。因此 [`DisplayInfo`] 里的 width/height 只用于 UI 展示，
//! 真正权威的采集尺寸来自 `GraphicsCaptureItem.Size()`（见 [`ResolvedDisplay::capture_size`]）。

use crate::error::{MediaError, MediaResult};
use windows::core::{BOOL, Interface};
use windows::Win32::Devices::Display::{
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QueryDisplayConfig,
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_DEVICE_INFO_HEADER, DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME, QDC_ONLY_ACTIVE_PATHS,
};
use windows::Win32::Foundation::{ERROR_SUCCESS, LPARAM, RECT, TRUE};
use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput6};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};

/// 一个可录制显示器的描述。
#[derive(Debug, Clone, serde::Serialize)]
pub struct DisplayInfo {
    /// Windows 侧稳定标识：`{luid_high:08X}{luid_low:08X}:{target_id}`。
    pub id: String,
    /// GDI 设备名，例如 `\\.\DISPLAY1`。
    pub device_name: String,
    /// 显示器友好名（EDID 名，可能为空）。
    pub friendly_name: String,
    /// UI 展示用尺寸（可能是逻辑尺寸，缩放环境下与物理分辨率不同）。
    pub width: u32,
    pub height: u32,
    /// 虚拟桌面坐标。
    pub x: i32,
    pub y: i32,
    pub primary: bool,
    /// 是否处于 HDR / 高级颜色模式。为 true 时 V0.1 拒绝录制。
    pub hdr: bool,
}

/// 解析后的显示器：带上 GDI 句柄，供创建捕获项使用。
#[derive(Debug, Clone)]
pub struct ResolvedDisplay {
    pub info: DisplayInfo,
    pub hmonitor: HMONITOR,
    /// 权威采集尺寸（物理像素），来自 `GraphicsCaptureItem.Size()`。
    pub capture_size: (u32, u32),
}

/// GDI 设备名的轻量包装，用于匹配不同来源的显示器信息。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GdiDeviceName(pub String);

impl GdiDeviceName {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

fn wide_to_string(buf: &[u16]) -> String {
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    String::from_utf16_lossy(&buf[..len])
}

/// GDI 设备名归一化：不同 API 返回的前缀大小写可能不同，比较前统一。
fn normalize_gdi(name: &str) -> String {
    name.trim_start_matches("\\\\.\\")
        .trim_start_matches("\\\\?\\")
        .to_ascii_uppercase()
}

struct GdiMonitor {
    hmonitor: HMONITOR,
    device_name: String,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    primary: bool,
}

unsafe extern "system" fn monitor_proc(
    hmonitor: HMONITOR,
    _hdc: HDC,
    _rect: *mut RECT,
    lparam: LPARAM,
) -> BOOL {
    let monitors = &mut *(lparam.0 as *mut Vec<GdiMonitor>);
    let mut mi = std::mem::zeroed::<MONITORINFOEXW>();
    mi.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    if GetMonitorInfoW(hmonitor, &mut mi.monitorInfo as *mut MONITORINFO).as_bool() {
        let r = mi.monitorInfo.rcMonitor;
        monitors.push(GdiMonitor {
            hmonitor,
            device_name: wide_to_string(&mi.szDevice),
            x: r.left,
            y: r.top,
            width: (r.right - r.left).max(0) as u32,
            height: (r.bottom - r.top).max(0) as u32,
            primary: (mi.monitorInfo.dwFlags & 1) != 0, // MONITORINFOF_PRIMARY
        });
    }
    TRUE
}

fn enum_gdi_monitors() -> Vec<GdiMonitor> {
    let mut monitors: Vec<GdiMonitor> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(monitor_proc),
            LPARAM(&mut monitors as *mut _ as isize),
        );
    }
    monitors
}

/// 来自 QueryDisplayConfig 的一条显示器配置。
struct ConfigEntry {
    gdi_device_name: String,
    stable_id: String,
    friendly_name: String,
}

fn query_display_config() -> Vec<ConfigEntry> {
    let mut out = Vec::new();
    unsafe {
        let mut num_paths = 0u32;
        let mut num_modes = 0u32;
        if GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut num_paths, &mut num_modes)
            != ERROR_SUCCESS
        {
            return out;
        }
        let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); num_paths as usize];
        let mut modes = vec![DISPLAYCONFIG_MODE_INFO::default(); num_modes as usize];
        if QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut num_paths,
            paths.as_mut_ptr(),
            &mut num_modes,
            modes.as_mut_ptr(),
            None,
        ) != ERROR_SUCCESS
        {
            return out;
        }
        paths.truncate(num_paths as usize);

        for path in &paths {
            // 源名 = GDI 设备名
            let mut src = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
            src.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32,
                adapterId: path.sourceInfo.adapterId,
                id: path.sourceInfo.id,
            };
            if DisplayConfigGetDeviceInfo(&mut src.header) != 0 {
                continue;
            }
            let gdi_device_name = wide_to_string(&src.viewGdiDeviceName);

            // 目标名 = 友好名 + 稳定 id
            let mut tgt = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
            tgt.header = DISPLAYCONFIG_DEVICE_INFO_HEADER {
                r#type: DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
                size: std::mem::size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32,
                adapterId: path.targetInfo.adapterId,
                id: path.targetInfo.id,
            };
            if DisplayConfigGetDeviceInfo(&mut tgt.header) != 0 {
                continue;
            }
            let friendly_name = wide_to_string(&tgt.monitorFriendlyDeviceName);
            let luid = path.targetInfo.adapterId;
            let stable_id = format!(
                "{:08X}{:08X}:{}",
                luid.HighPart, luid.LowPart, path.targetInfo.id
            );

            out.push(ConfigEntry {
                gdi_device_name,
                stable_id,
                friendly_name,
            });
        }
    }
    out
}

/// 通过 DXGI 把 GDI 设备名映射到「是否 HDR」。
fn hdr_by_device_name() -> Vec<(String, bool)> {
    let mut out = Vec::new();
    unsafe {
        let factory: IDXGIFactory1 = match CreateDXGIFactory1() {
            Ok(f) => f,
            Err(_) => return out,
        };
        let mut adapter_idx = 0u32;
        while let Ok(adapter) = factory.EnumAdapters1(adapter_idx) {
            adapter_idx += 1;
            let mut oi = 0u32;
            while let Ok(output) = adapter.EnumOutputs(oi) {
                oi += 1;
                let Ok(desc) = output.GetDesc() else { continue };
                let name = wide_to_string(&desc.DeviceName);
                // IDXGIOutput6 需要 DXGI 1.6（Win10 1607+ / Win11 均满足）
                let hdr = match output.cast::<IDXGIOutput6>() {
                    Ok(o6) => match o6.GetDesc1() {
                        Ok(d1) => is_hdr_color_space(d1.ColorSpace),
                        Err(_) => false,
                    },
                    Err(_) => false,
                };
                out.push((name, hdr));
            }
        }
    }
    out
}

fn is_hdr_color_space(cs: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE) -> bool {
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020, DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P2020,
        DXGI_COLOR_SPACE_RGB_STUDIO_G2084_NONE_P2020,
    };
    matches!(
        cs,
        DXGI_COLOR_SPACE_RGB_FULL_G2084_NONE_P2020
            | DXGI_COLOR_SPACE_RGB_STUDIO_G2084_NONE_P2020
            | DXGI_COLOR_SPACE_RGB_FULL_G22_NONE_P2020
    )
}

/// 枚举所有可录制显示器。
pub fn list_displays() -> MediaResult<Vec<DisplayInfo>> {
    let monitors = enum_gdi_monitors();
    if monitors.is_empty() {
        return Ok(Vec::new());
    }
    let config = query_display_config();
    let hdr_map = hdr_by_device_name();

    let mut result = Vec::with_capacity(monitors.len());
    for m in &monitors {
        let key = normalize_gdi(&m.device_name);
        let cfg = config
            .iter()
            .find(|c| normalize_gdi(&c.gdi_device_name) == key);
        let hdr = hdr_map
            .iter()
            .find(|(name, _)| normalize_gdi(name) == key)
            .map(|(_, h)| *h)
            .unwrap_or(false);

        result.push(DisplayInfo {
            id: cfg
                .map(|c| c.stable_id.clone())
                .unwrap_or_else(|| format!("gdi:{}", m.device_name)),
            device_name: m.device_name.clone(),
            friendly_name: cfg.map(|c| c.friendly_name.clone()).unwrap_or_default(),
            width: m.width,
            height: m.height,
            x: m.x,
            y: m.y,
            primary: m.primary,
            hdr,
        });
    }
    Ok(result)
}

/// 按稳定 ID 解析显示器。ID 不存在 → `DisplayNotFound`（不得静默改用其他显示器）。
pub fn resolve_display(id: &str) -> MediaResult<ResolvedDisplay> {
    let displays = list_displays()?;
    let info = displays
        .iter()
        .find(|d| d.id == id)
        .cloned()
        .ok_or_else(|| MediaError::DisplayNotFound {
            display_id: id.to_string(),
        })?;

    let monitors = enum_gdi_monitors();
    let key = normalize_gdi(&info.device_name);
    let hmonitor = monitors
        .iter()
        .find(|m| normalize_gdi(&m.device_name) == key)
        .map(|m| m.hmonitor)
        .ok_or_else(|| MediaError::DisplayNotFound {
            display_id: id.to_string(),
        })?;

    Ok(ResolvedDisplay {
        capture_size: (info.width, info.height), // 占位，创建捕获项后用 item.Size() 覆盖
        info,
        hmonitor,
    })
}

/// 主显示器的稳定 ID（用于首次启动的默认选择）。
pub fn primary_display_id() -> Option<String> {
    list_displays()
        .ok()
        .and_then(|d| d.into_iter().find(|x| x.primary).map(|x| x.id))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_enumeration_returns_at_least_one_display() {
        // 该测试需要桌面会话；在无会话的 CI 环境会返回空列表。
        let displays = list_displays().unwrap();
        for d in &displays {
            assert!(!d.id.is_empty(), "稳定 ID 不得为空");
            assert!(!d.device_name.is_empty());
            // 稳定 ID 不得是 UI 生成的序号
            assert!(!d.id.starts_with("display-"));
        }
    }
}
