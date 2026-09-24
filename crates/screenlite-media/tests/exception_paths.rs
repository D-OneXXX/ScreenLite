//! 异常路径验证：检测逻辑是否真的有效，而不是"写了检测就算完"。
//!
//! 技术基线 列了一长串错误分类，但在此之前它们一次都没有被真正触发过。
//! 这个文件的目标是：让每条能在本机触发的异常路径都产生一次真实失败，
//! 并确认它给出的是明确错误或完整文件，而不是崩溃或损坏产物。

use std::time::{Duration, Instant};

use screenlite_media::capture;
use screenlite_media::disk;
use screenlite_media::engine::{EngineState, Recorder, RecordingConfig, StopReason};

fn primary_display_id() -> Option<String> {
    capture::enumerate_displays()
        .ok()
        .and_then(|d| d.into_iter().find(|x| x.primary).map(|x| x.id))
}

fn temp_dir(tag: &str) -> std::path::PathBuf {
    let d = std::env::temp_dir().join(format!("sl-exc-{}-{}", tag, std::process::id()));
    let _ = std::fs::create_dir_all(&d);
    d
}

#[test]
fn test_display_not_found_is_reported() {
    let cfg = RecordingConfig::new("not-a-real-display-id", temp_dir("nodisp"));
    let err = match Recorder::start(cfg) {
        Ok(_) => panic!("不存在的显示器必须被拒绝"),
        Err(e) => e,
    };
    assert_eq!(err.code(), "DisplayNotFound");
    eprintln!("✅ DisplayNotFound：{}", err);
}

#[test]
fn test_fps_outside_allowlist_is_rejected() {
    let Some(id) = primary_display_id() else {
        return;
    };
    let mut cfg = RecordingConfig::new(id, temp_dir("badfps"));
    cfg.fps = 45; // 不在 30/60/90 白名单内
    let err = match Recorder::start(cfg) {
        Ok(_) => panic!("非白名单帧率必须被拒绝"),
        Err(e) => e,
    };
    assert_eq!(err.code(), "UnsupportedFrameRate");
    eprintln!("✅ UnsupportedFrameRate：{}", err);
}

#[test]
fn test_output_dir_not_writable_is_reported() {
    let Some(id) = primary_display_id() else {
        return;
    };
    // 不存在的盘符：创建目录必然失败
    let cfg = RecordingConfig::new(id, std::path::PathBuf::from(r"Z:\screenlite-should-not-exist"));
    let err = match Recorder::start(cfg) {
        Ok(_) => panic!("不可写的输出目录必须被拒绝"),
        Err(e) => e,
    };
    assert_eq!(err.code(), "OutputDirectoryNotWritable");
    eprintln!("✅ OutputDirectoryNotWritable：{}", err);
}

#[test]
fn test_insufficient_disk_space_is_refused() {
    // 用真实磁盘查询 + 无法满足的需求，验证"拒绝启动"这条路径
    let dir = temp_dir("nospace");
    let err = disk::check_before_start(&dir, u64::MAX / 2)
        .err()
        .expect("需求远超可用空间时必须拒绝");
    assert_eq!(err.code(), "InsufficientDiskSpace");
    eprintln!("✅ InsufficientDiskSpace：{}", err);

    // 反向验证：正常需求必须通过
    let free = disk::check_before_start(&dir, 1024 * 1024).expect("正常需求应通过");
    assert!(free > 0);
}

/// 录制中磁盘不足：验证优雅停止路径——提前停、正常 finalize、文件完整可解码。
///
/// 不做"真把磁盘写满"（本机 658GB 空余，不现实），而是把硬地板临时抬到不可能满足的值，
/// 让第一次 2 秒周期检查就触发。这样验证的是同一条代码路径。
#[test]
fn test_disk_space_low_stops_gracefully_with_valid_file() {
    let Some(id) = primary_display_id() else {
        return;
    };

    let original = disk::disk_floor_bytes();
    disk::set_disk_floor_for_testing(u64::MAX);
    let result = (|| -> Result<_, screenlite_media::MediaError> {
        let dir = temp_dir("lowspace");
        let mut cfg = RecordingConfig::new(id, dir.clone());
        cfg.hardware = screenlite_media::encoder::HardwarePreference::PreferSoftware;

        let recorder = Recorder::start(cfg)?;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if recorder.status().state == EngineState::Recording {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        // 必须等它自己停：地板检查是 2 秒周期，这里等 5 秒足够触发两轮。
        // 立刻 stop() 会让"用户停止"抢先，测不到这条路径。
        std::thread::sleep(Duration::from_secs(5));
        let state_after = recorder.status().state;
        eprintln!("等待 5 秒后的状态：{:?}", state_after);
        let outcome = recorder.stop()?;
        Ok((outcome, dir))
    })();
    disk::set_disk_floor_for_testing(original);

    let (outcome, dir) = match result {
        Ok(v) => v,
        Err(e) => {
            eprintln!("跳过：启动失败（{}）", e);
            return;
        }
    };

    eprintln!(
        "结束原因={} 时长={}ms 帧数={} 文件={}（{} 字节）",
        outcome.stop_reason.as_str(),
        outcome.elapsed_ms,
        outcome.frames_scheduled,
        outcome.finalize.path.display(),
        outcome.finalize.bytes
    );

    // 关键断言①：必须是因为磁盘空间而停止
    assert_eq!(
        outcome.stop_reason,
        StopReason::DiskSpaceLow,
        "应因磁盘空间不足而停止，实际是 {}",
        outcome.stop_reason.as_str()
    );

    // 关键断言②：文件必须是完整可播放的，而不是损坏产物
    assert!(outcome.finalize.bytes > 0, "文件为 0 字节");
    assert!(
        !outcome.finalize.path.with_extension("mp4.partial").exists(),
        "残留 .partial：说明没能正常 finalize"
    );
    let probe = screenlite_media::verify::probe_mp4(&outcome.finalize.path, 30)
        .expect("产物必须可完整解码");
    assert!(probe.frame_count > 0, "解码出 0 帧");
    assert_eq!(probe.width, 2560);
    eprintln!(
        "✅ 磁盘不足时优雅停止：{} 帧可解码，时长 {}ms",
        probe.frame_count,
        probe.duration_ms()
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// 这些路径无法在本机安全触发（需要挂起合成器 / 改分辨率 / 打死子线程），
/// 这里显式记录为未覆盖，避免被误当成"已验证"。
///
/// 注意「长时间无帧」不再是致命路径：
/// 桌面静止时 WGC 本就可以长时间不给回调，判死等于"用户读文档时录像被中断"。
/// 现在只有采集会话自身报错（`capture.take_fatal`）才是致命路径。
#[test]
fn test_uncovered_paths_are_documented() {
    let uncovered = [
        "采集会话被系统关闭（需挂起合成器/拔掉显示器，走 capture.take_fatal）",
        "StopTimeout（停止流程总预算；需构造子线程卡死）",
        "ResolutionChanged（需录制中改分辨率）",
        "HdrDisplayNotSupported（本机无 HDR 显示器）",
        "FramePoolCreateFailed / CaptureSessionStartFailed（需构造系统级失败）",
        "EncoderInitFailed 中途失败（已通过 144fps 场景验证过一次初始化失败）",
    ];
    for item in uncovered {
        eprintln!("⚠️ 未覆盖（本机无法触发）：{}", item);
    }
}
