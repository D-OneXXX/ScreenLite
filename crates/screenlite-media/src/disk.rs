//! 磁盘空间预检与录制中监控。
//!
//! 为什么必须有：2 小时 @ 12 Mbps ≈ 10.8 GB。磁盘写满时若不处理，用户会拿到一个
//! 损坏的 MP4（moov 写不进去）——这是最典型的"录了两小时全白录"。
//!
//! 两级策略：
//!
//! ```text
//! 启动前（精确）：按 码率 × 时长 × 1.2 估算所需空间，不足则拒绝启动（InsufficientDiskSpace）
//! 录制中（兜底）：每 2 秒检查一次，低于硬地板则优雅停止并正常 finalize
//! （StopReason::DiskSpaceLow）——宁可提前停并交出完整文件，
//! 也不要写到磁盘满导致文件不可播放
//! ```

use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::core::HSTRING;
use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

use crate::error::{MediaError, MediaResult};

/// 估算系数：实际文件比"码率 × 时长"略大（容器开销、关键帧、索引）。
pub const DISK_ESTIMATE_FACTOR: f64 = 1.2;

/// 估算所需空间的下限（避免短时长/低码率场景下估算过小）。
pub const DISK_MIN_REQUIREMENT: u64 = 1024 * 1024 * 1024; // 1 GB

/// 录制中的硬地板：可用空间低于它就必须停止。
/// 默认 200 MB；可用 [`set_disk_floor_for_testing`] 覆盖以便验证该路径。
static DISK_FLOOR_BYTES: AtomicU64 = AtomicU64::new(200 * 1024 * 1024);

pub fn disk_floor_bytes() -> u64 {
    DISK_FLOOR_BYTES.load(Ordering::Relaxed)
}

/// 仅供测试：覆盖硬地板，用于在不制造"真磁盘满"的情况下验证优雅停止路径。
pub fn set_disk_floor_for_testing(bytes: u64) {
    DISK_FLOOR_BYTES.store(bytes, Ordering::Relaxed);
}

/// 按码率与时长估算所需磁盘空间。
pub fn required_bytes(bitrate_bps: u32, duration_secs: u64) -> u64 {
    let raw = bitrate_bps as f64 / 8.0 * duration_secs as f64 * DISK_ESTIMATE_FACTOR;
    (raw as u64).max(DISK_MIN_REQUIREMENT)
}

/// 查询目录所在卷的可用字节数。
pub fn free_bytes(dir: &Path) -> MediaResult<u64> {
    let mut free: u64 = 0;
    let path = HSTRING::from(dir.to_string_lossy().as_ref());
    unsafe {
        GetDiskFreeSpaceExW(&path, Some(&mut free), None, None)
            .map_err(|e| MediaError::win32("GetDiskFreeSpaceExW", e))?;
    }
    Ok(free)
}

/// 启动前检查：可用空间必须覆盖本次录制的估算需求。
pub fn check_before_start(dir: &Path, required: u64) -> MediaResult<u64> {
    let free = free_bytes(dir)?;
    if free < required {
        return Err(MediaError::InsufficientDiskSpace {
            free,
            required,
            free_human: human(free),
            required_human: human(required),
        });
    }
    Ok(free)
}

/// 录制中检查：可用空间是否已跌破硬地板。
pub fn below_floor(dir: &Path) -> bool {
    match free_bytes(dir) {
        Ok(free) => free < disk_floor_bytes(),
        // 查询失败不当作"磁盘满"（避免误停），只记日志
        Err(e) => {
            tracing::warn!(error = %e, "查询可用磁盘空间失败，跳过本次检查");
            false
        }
    }
}

/// 人类可读的空间描述，用于错误信息与日志。
pub fn human(bytes: u64) -> String {
    const GB: f64 = 1024.0 * 1024.0 * 1024.0;
    const MB: f64 = 1024.0 * 1024.0;
    if bytes as f64 >= GB {
        format!("{:.2} GB", bytes as f64 / GB)
    } else {
        format!("{:.0} MB", bytes as f64 / MB)
    }
}

/// 默认输出目录：`%USERPROFILE%\Videos\ScreenLite`。
///
/// 用 `SHGetKnownFolderPath(FOLDERID_Videos)` 解析，不要用字符串拼 `%USERPROFILE%`——
/// 用户的 Videos 目录可能被重定向到别的盘，拼字符串会写错地方。
pub fn default_output_dir() -> MediaResult<std::path::PathBuf> {
    use windows::Win32::UI::Shell::{FOLDERID_Videos, SHGetKnownFolderPath, KF_FLAG_DEFAULT};
    use windows::Win32::System::Com::CoTaskMemFree;

    unsafe {
        let raw = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None)
            .map_err(|e| MediaError::win32("SHGetKnownFolderPath(Videos)", e))?;
        let path = raw.to_string().map_err(|e| {
            MediaError::Internal(format!("Videos 路径不是有效 UTF-8：{}", e))
        })?;
        // SHGetKnownFolderPath 要求用 CoTaskMemFree 释放
        CoTaskMemFree(Some(raw.as_ptr() as *const core::ffi::c_void));

        let mut dir = std::path::PathBuf::from(path);
        dir.push("ScreenLite");
        Ok(dir)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_bytes_matches_two_hour_estimate() {
        // 12 Mbps ÷ 8 × 7200s × 1.2 = 12,960,000,000 字节 = 12.07 GiB
        assert_eq!(required_bytes(12_000_000, 2 * 60 * 60), 12_960_000_000);
        let gib = 12_960_000_000f64 / (1024.0 * 1024.0 * 1024.0);
        assert!((gib - 12.07).abs() < 0.05, "= {:.2} GiB", gib);
    }

    #[test]
    fn required_bytes_has_a_floor() {
        // 极短/极低码率也要至少 1GB，避免"刚好卡在边界"
        assert_eq!(required_bytes(100_000, 1), DISK_MIN_REQUIREMENT);
    }

    #[test]
    fn required_bytes_is_monotonic_in_duration() {
        let a = required_bytes(12_000_000, 600);
        let b = required_bytes(12_000_000, 1200);
        assert!(b > a);
    }

    #[test]
    fn free_bytes_of_system_drive_is_positive() {
        let dir = std::env::temp_dir();
        let free = free_bytes(&dir).expect("查询可用空间失败");
        assert!(free > 0);
        assert!(!below_floor(&dir), "正常环境不应判定为磁盘不足");
    }
}
