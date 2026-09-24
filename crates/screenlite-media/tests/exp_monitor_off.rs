//! 手动实验：关掉显示器会不会毁掉录制（默认跳过，必须显式开启）。
//!
//! 运行：
//! ```text
//! SL_EXP_MONITOR_OFF=1 cargo test --release -p screenlite-media --test exp_monitor_off -- --nocapture
//! ```
//!
//! ## 为什么要做这个实验
//!
//! 把「长时间无 `FrameArrived`」从致命错误降级成了观测：桌面静止时 WGC 本就不给帧，
//! 而 CFR 输出自己补重复帧（tick 循环不依赖输入帧）。但这条降级路径无法注入验证——
//! 除非把"帧源"抽成 trait。
//!
//! 显示器休眠是这条路径的真实场景：用户开了长录后走开，显示器自动关闭。
//!
//! ## 结果判读（三选一，结论完全不同）
//!
//! | 结果 | 含义 | 结论 |
//! |---|---|---|
//! | A 录完不中断、文件有效、时长 ≈ 录制时长 | 帧停了但 tick 继续补重复帧、正常收尾 | 降级生效 |
//! | B 中断且报采集会话错误（`take_fatal` 那条路径） | 关屏让会话真的失效了，不是"不给帧" | 也算正确行为，但对降级路径没有信息量——别当成验证通过 |
//! | C 中断却报错含糊 / 没报错但文件损坏 | — | 真问题 |
//!
//! 这是手动实验，不进"已验证"清单：真实场景值得跑一次拿信息，但它不可重复、结论有歧义。
//! 无歧义的验证要等"帧源 trait + 注入测试"。
//!
//! ## 安全性
//!
//! 会把显示器关掉约 15 秒（`WM_SYSCOMMAND`/`SC_MONITORPOWER`），随后自动开回来
//! （开屏由 RAII 保护负责，测试失败也不会把你的屏幕留在关闭状态）。
//! 默认不运行：必须显式设置 `SL_EXP_MONITOR_OFF=1`。

use std::time::Duration;

use screenlite_media::encoder::HardwarePreference;
use screenlite_media::engine::{EngineState, Recorder, RecordingConfig};
use windows::Win32::Foundation::{LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    HWND_BROADCAST, SC_MONITORPOWER, SMTO_ABORTIFHUNG, SendMessageTimeoutW, WM_SYSCOMMAND,
};

mod common;

/// 显示器电源：2 = 关闭，-1 = 打开。
const MONITOR_OFF: isize = 2;
const MONITOR_ON: isize = -1;

fn set_monitor(on: bool) {
    let lp = if on { MONITOR_ON } else { MONITOR_OFF };
    let mut result: usize = 0;
    unsafe {
        let _ = SendMessageTimeoutW(
            HWND_BROADCAST,
            WM_SYSCOMMAND,
            WPARAM(SC_MONITORPOWER as usize),
            LPARAM(lp),
            SMTO_ABORTIFHUNG,
            2_000,
            Some(&mut result),
        );
    }
}

/// 开屏 RAII：无论测试怎么退出（含 panic），显示器都会被打开。
struct MonitorGuard;
impl Drop for MonitorGuard {
    fn drop(&mut self) {
        set_monitor(true);
    }
}

#[test]
fn exp_display_off_does_not_kill_recording() {
    if std::env::var("SL_EXP_MONITOR_OFF").as_deref() != Ok("1") {
        eprintln!("跳过：这是手动实验（会把显示器关掉 15 秒）。");
        eprintln!("      需要时用 SL_EXP_MONITOR_OFF=1 显式开启。");
        return;
    }

    let Some(display_id) = common::primary_display_id() else {
        eprintln!("跳过：没有可用显示器");
        return;
    };
    if common::require_desktop_ready().is_none() {
        return;
    }

    let dir = std::env::temp_dir().join("sl-exp-monitor-off");
    let _ = std::fs::create_dir_all(&dir);
    let mut cfg = RecordingConfig::new(display_id, dir.clone());
    cfg.hardware = HardwarePreference::PreferHardware;
    cfg.audio = false; // 本实验只看视频链路，音频是另一个变量
    cfg.max_duration_secs = 60;

    let recorder = match Recorder::start(cfg) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("跳过：启动录制失败（{}）", e);
            return;
        }
    };
    let t0 = std::time::Instant::now();
    while t0.elapsed() < Duration::from_secs(10) {
        if recorder.status().state == EngineState::Recording {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    if recorder.status().state != EngineState::Recording {
        eprintln!("跳过：没能进入 Recording 状态");
        return;
    }

    println!("--- 阶段 1：正常录 8 秒 ---");
    std::thread::sleep(Duration::from_secs(8));
    let before = recorder.status();
    println!(
        "    recorded={}ms captured={} idle_max={}ms",
        before.elapsed_ms, before.frames_captured, before.capture_idle_ms_max
    );

    println!("--- 阶段 2：关屏 15 秒（观察采集是否停帧、引擎是否中止）---");
    let _guard = MonitorGuard;
    set_monitor(false);
    let off_at = std::time::Instant::now();
    std::thread::sleep(Duration::from_secs(15));
    let during = recorder.status();
    println!(
        "    关屏后 {:.0}s：state={:?} recorded={}ms captured={} idle_max={}ms idle_checks={} error={:?}",
        off_at.elapsed().as_secs_f64(),
        during.state,
        during.elapsed_ms,
        during.frames_captured,
        during.capture_idle_ms_max,
        during.capture_idle_checks,
        during.error_code
    );

    println!("--- 阶段 3：开屏后再录 8 秒，然后正常停止 ---");
    set_monitor(true);
    std::thread::sleep(Duration::from_secs(8));
    let stop_res = recorder.stop();
    let total = t0.elapsed();
    println!("    总耗时 {:.0}s；stop 结果：{:?}", total.as_secs_f64(), stop_res.as_ref().map(|_| "ok").map_err(|e| e.to_string()));

    // ---- 判读（比"有没有中断"更重要：产物是否有效 + 时长是否≈录制时长）----
    match stop_res {
        Err(e) => {
            eprintln!(
                "❌ 结果 C/B 之间的判定需要看错误码：{}（若是采集会话错误 → 结果 B：本实验对降级路径无信息量）",
                e
            );
            panic!("关屏实验：录制被中止（见上面的错误码）");
        }
        Ok(outcome) => {
            let probe = screenlite_media::verify::probe_mp4(&outcome.finalize.path, 30);
            match probe {
                Ok(p) => {
                    let dur_err = (p.duration_ms() - outcome.elapsed_ms).abs();
                    println!("================ 关屏实验结果 ================");
                    println!("  结果：**A（录完不中断）**");
                    println!("  产物：解码 {} 帧、{}x{}、时长 {}ms", p.frame_count, p.width, p.height, p.duration_ms());
                    println!("  时长差：{}ms（录制 {}ms）", dur_err, outcome.elapsed_ms);
                    println!("  采集最长无帧：{}ms，无帧秒数：{}", outcome.capture_idle_ms_max, outcome.capture_idle_checks);
                    println!("  稳态丢帧：{}，启动瞬态：{}", outcome.frames_dropped_backpressure, outcome.frames_dropped_startup);
                    println!("==============================================");
                    assert!(
                        dur_err < 1_000,
                        "文件时长与录制时长差 {}ms —— 降级的意义是「继续产出有效文件」，时长必须对得上",
                        dur_err
                    );
                    assert!(
                        p.frame_count > 0,
                        "产物 0 帧：降级路径下文件无效"
                    );
                }
                Err(e) => panic!("❌ 结果 C：录制没报错，但产物校验失败：{}", e),
            }
        }
    }
}
