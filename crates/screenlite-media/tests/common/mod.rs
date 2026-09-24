//! 测试公共设施：环境前提检查。
//!
//! 本项目的功能性测试依赖一个隐含前提：桌面可见且稳定。
//! 无人值守的会话会自动锁屏 / 显示器休眠（实测同一会话内屏幕平均亮度在
//! 223 ↔ 86 之间波动），此时系统不会把光标画进捕获帧、屏幕内容也在变，
//! 测试会失败——但那是环境问题，不是产品缺陷。
//!
//! 所以这些测试必须先自检环境：前提不成立就跳过，而不是报失败。
//! 否则换一台机器（或同一台机器第二天）跑出来的红绿结果不可信。

use std::time::Duration;

use screenlite_media::capture;
use screenlite_media::convert::Nv12Image;

/// NV12 的 Y 平面平均亮度（0–255）。
pub fn mean_luma(img: &Nv12Image) -> f64 {
    let w = img.width as usize;
    let h = img.height as usize;
    let mut sum: u64 = 0;
    for y in 0..h {
        let row = &img.data[y * img.stride..y * img.stride + w];
        for &v in row {
            sum += v as u64;
        }
    }
    sum as f64 / (w * h) as f64
}

/// 屏幕"黑屏/锁屏"判定阈值。
pub const LOCKED_LUMA: f64 = 40.0;

/// 桌面在两次抓帧之间的允许亮度变化；超过即认为环境不稳定。
pub const STABLE_DELTA: f64 = 2.0;

/// 可用环境变量覆盖稳定性阈值。设为 0 可强制触发跳过路径，
/// 用于验证"环境不满足时会跳过而不是误报失败"这一机制本身。
fn stable_delta() -> f64 {
    std::env::var("SL_TEST_STABLE_DELTA")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(STABLE_DELTA)
}

pub fn primary_display_id() -> Option<String> {
    capture::enumerate_displays()
        .ok()
        .and_then(|d| d.into_iter().find(|x| x.primary).map(|x| x.id))
}

/// 检查桌面是否满足测试前提。
///
/// 通过时返回基准帧的平均亮度；不通过时返回跳过原因（调用方应打印并 return）。
pub fn check_desktop_ready() -> Result<f64, String> {
    let Some(id) = primary_display_id() else {
        return Err("没有可用显示器".into());
    };

    let first = capture::capture_single_frame(&id, Duration::from_secs(5))
        .map_err(|e| format!("抓帧失败：{}", e))?;
    let luma_first = mean_luma(&first);
    if luma_first < LOCKED_LUMA {
        return Err(format!(
            "屏幕平均亮度 {:.2} < {}，疑似锁屏/黑屏",
            luma_first, LOCKED_LUMA
        ));
    }

    // 间隔 400ms 再抓一帧：桌面在变（窗口切换、动画、即将锁屏）时不宜做像素级断言
    std::thread::sleep(Duration::from_millis(400));
    let second = capture::capture_single_frame(&id, Duration::from_secs(5))
        .map_err(|e| format!("抓帧失败：{}", e))?;
    let luma_second = mean_luma(&second);

    let delta = (luma_first - luma_second).abs();
    if delta > stable_delta() {
        return Err(format!(
            "桌面在 400ms 内平均亮度从 {:.2} 变为 {:.2}（差 {:.2} > {}），环境不稳定",
            luma_first,
            luma_second,
            delta,
            stable_delta()
        ));
    }

    Ok(luma_first)
}

/// 便捷宏式函数：前提不满足时打印原因并返回（用于 `#[test]` 提前退出）。
pub fn require_desktop_ready() -> Option<f64> {
    match check_desktop_ready() {
        Ok(luma) => {
            eprintln!("环境前提检查通过：桌面亮度 {:.2}", luma);
            Some(luma)
        }
        Err(why) => {
            eprintln!("⚠️ 跳过：{}（这是环境问题，不是产品缺陷）", why);
            None
        }
    }
}
