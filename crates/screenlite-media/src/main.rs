//! ScreenLite 端到端冒烟测试（无 UI）：真实录屏 N 秒 → 输出 MP4 + 指标 + 产物校验。
//!
//! 用法：
//! ```text
//! cargo run --release -- [秒数] [--software] [--dir <输出目录>] [--repeat N]
//! ```
//!
//! `--repeat N` 会连续做 N 轮「启动 → 录制 → 停止」，用于稳定性验证。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use screenlite_media::capture;
use screenlite_media::encoder::HardwarePreference;
use screenlite_media::engine::{EngineState, Recorder, RecordingConfig};
use screenlite_media::verify::{probe_mp4, process_memory};

fn main() {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::WARN)
        .with_target(false)
        .init();

    let mut seconds = 5u64;
    let mut software = false;
    let mut dir: Option<PathBuf> = None;
    let mut repeat = 1usize;
    let mut fps: u32 = screenlite_media::consts::DEFAULT_FPS;
    let mut max_secs: Option<u64> = None;
    let mut region: Option<(u32, u32, u32, u32)> = None;
    let mut audio = true;
    // 只列音频端点就退出（诊断用：`--list-audio`）。
    // 与产品代码走同一条枚举路径（`audio::enumerate_endpoints`），所以它列出来的
    // 就是"用户报麦克风不对"时要看的东西。
    let mut list_audio = false;
    // V0.2 只支持"默认端点"；`--mic` 把它切到默认输入设备（设备选择留给后面的 UI）
    let mut audio_kind = screenlite_media::audio::AudioSourceKind::SystemLoopback;

    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--software" => software = true,
            "--no-audio" => audio = false,
            "--mic" => audio_kind = screenlite_media::audio::AudioSourceKind::Microphone,
            "--list-audio" => list_audio = true,
            "--region" => {
                i += 1;
                if let Some(v) = args.get(i) {
                    let parts: Vec<u32> = v
                        .split(',')
                        .filter_map(|s| s.trim().parse::<u32>().ok())
                        .collect();
                    if parts.len() == 4 {
                        region = Some((parts[0], parts[1], parts[2], parts[3]));
                    } else {
                        println!("--region 需要 x,y,w,h 四个数字，收到：{}", v);
                    }
                }
            }
            "--fps" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|s| s.parse::<u32>().ok()) {
                    fps = v;
                }
            }
            "--max-secs" => {
                i += 1;
                if let Some(v) = args.get(i).and_then(|s| s.parse::<u64>().ok()) {
                    max_secs = Some(v);
                }
            }
            "--dir" => {
                i += 1;
                if let Some(d) = args.get(i) {
                    dir = Some(PathBuf::from(d));
                }
            }
            "--repeat" => {
                i += 1;
                if let Some(n) = args.get(i).and_then(|s| s.parse::<usize>().ok()) {
                    repeat = n.max(1);
                }
            }
            other => {
                if let Ok(s) = other.parse::<u64>() {
                    seconds = s;
                }
            }
        }
        i += 1;
    }

    if list_audio {
        // COM 是每线程状态：引擎是在媒体线程里初始化的，CLI 主线程必须自己来一次，
        // 否则 CoCreateInstance(MMDeviceEnumerator) 会以 CO_E_NOTINITIALIZED 失败。
        if let Err(e) = screenlite_media::mf::MfRuntime::start() {
            println!("COM/MF 初始化失败：{}", e);
            std::process::exit(1);
        }
        for dir in [
            screenlite_media::audio::AudioDirection::Render,
            screenlite_media::audio::AudioDirection::Capture,
        ] {
            println!("--- {:?} 端点 ---", dir);
            match screenlite_media::audio::enumerate_endpoints(dir) {
                Ok(list) if list.is_empty() => println!("  （无）"),
                Ok(list) => {
                    for ep in list {
                        println!(
                            "  {}{}",
                            if ep.is_default { "* " } else { "  " },
                            ep.name
                        );
                        println!("      id = {}", ep.id);
                    }
                }
                Err(e) => println!("  枚举失败：{}", e),
            }
        }
        std::process::exit(0);
    }

    println!("=== ScreenLite 稳定性测试 ===");
    println!(
        "轮数={} 每轮={} 秒（共约 {} 秒）帧率={} 编码路径={}",
        repeat,
        seconds,
        repeat as u64 * seconds,
        fps,
        if software { "software" } else { "hardware" }
    );
    println!("capture supported: {}", capture::is_supported());

    let displays = match capture::enumerate_displays() {
        Ok(d) => d,
        Err(e) => {
            println!("枚举显示器失败：[{}] {}", e.code(), e);
            std::process::exit(1);
        }
    };
    let primary = match displays.iter().find(|d| d.primary).or(displays.first()) {
        Some(d) => d.clone(),
        None => {
            println!("没有可用显示器");
            std::process::exit(1);
        }
    };
    let output_dir = dir.unwrap_or_else(|| std::env::temp_dir().join("screenlite-stability"));
    println!(
        "目标显示器：{} (id={})  输出目录：{}",
        primary.device_name,
        primary.id,
        output_dir.display()
    );

    let (ws0, pf0) = process_memory();
    println!(
        "起始内存：工作集 {:.1} MB，提交 {:.1} MB",
        ws0 as f64 / 1048576.0,
        pf0 as f64 / 1048576.0
    );

    let mut failures = 0usize;
    let mut peak_ws = ws0;
    let mut cycle_stats: Vec<(u64, u64, f64, f64)> = Vec::new(); // (scheduled, duplicated, fps, stop_ms)

    for cycle in 1..=repeat {
        let cycle_start = Instant::now();
        println!("\n--- 第 {}/{} 轮 ---", cycle, repeat);

        let mut cfg = RecordingConfig::new(primary.id.clone(), output_dir.clone());
        cfg.hardware = if software {
            HardwarePreference::PreferSoftware
        } else {
            HardwarePreference::PreferHardware
        };
        cfg.fps = fps;
        cfg.audio = audio;
        // CLI 仍是单源口径：--mic → 单路麦克风，默认 → 单路系统声音
        cfg.audio_sources = vec![screenlite_media::audio::AudioSourceSpec {
            kind: audio_kind,
            gain: 1.0,
        }];
        if let Some(m) = max_secs {
            cfg.max_duration_secs = m;
        }
        if let Some((x, y, w, h)) = region {
            cfg.region = Some(screenlite_media::convert::Region {
                x,
                y,
                width: w,
                height: h,
            });
        }

        let recorder = match Recorder::start(cfg) {
            Ok(r) => r,
            Err(e) => {
                println!("❌ 启动失败：[{}] {}", e.code(), e);
                failures += 1;
                continue;
            }
        };

        // 等待进入 Recording
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut entered = false;
        while Instant::now() < deadline {
            let st = recorder.status();
            match st.state {
                EngineState::Recording => {
                    if cycle == 1 {
                        if let Some(a) = &st.actual {
                            println!(
                                "  编码参数：{}x{} @{}fps {} bps profile={} 硬件请求={} 硬件MFT={} 参数生效={}",
                                a.width, a.height, a.fps, a.bitrate, a.profile,
                                a.hardware_requested, a.hardware_mft_found, a.encoder_params_applied
                            );
                        }
                    }
                    entered = true;
                    break;
                }
                EngineState::Error => {
                    println!(
                        "❌ 第 {} 轮出错：[{}] {}",
                        cycle,
                        st.error_code.unwrap_or("?"),
                        st.error_message.unwrap_or_default()
                    );
                    failures += 1;
                    break;
                }
                _ => {}
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        if !entered {
            if failures == 0 {
                println!("❌ 第 {} 轮未能进入 Recording", cycle);
                failures += 1;
            }
            continue;
        }

        // 录制 + 每秒采样内存
        let record_deadline = Instant::now() + Duration::from_secs(seconds);
        let mut samples: Vec<u64> = Vec::new();
        while Instant::now() < record_deadline {
            std::thread::sleep(Duration::from_millis(1000));
            let (ws, _) = process_memory();
            peak_ws = peak_ws.max(ws);
            samples.push(ws);
            let st = recorder.status();
            let elapsed_s = st.elapsed_ms as f64 / 1000.0;
            // 5 分钟时每分钟打印一次，短录制每秒打印
            if seconds <= 15 || (st.elapsed_ms % 60_000) < 1_100 {
                println!(
                    "  [{:>6.1}s] captured={} scheduled={} duplicated={} dropped={} encoded={} 内存={:.1}MB",
                    elapsed_s,
                    st.frames_captured,
                    st.frames_scheduled,
                    st.frames_duplicated,
                    st.frames_dropped_backpressure,
                    st.frames_encoded,
                    ws as f64 / 1048576.0
                );
            }
        }

        let stop_started = Instant::now();
        let outcome = match recorder.stop() {
            Ok(o) => o,
            Err(e) => {
                println!("❌ 第 {} 轮停止失败：[{}] {}", cycle, e.code(), e);
                failures += 1;
                continue;
            }
        };
        let stop_ms = stop_started.elapsed().as_millis() as u64;

        // 产物校验：解码回读
        let probe = match probe_mp4(&outcome.finalize.path, fps) {
            Ok(p) => p,
            Err(e) => {
                println!("❌ 第 {} 轮产物校验失败：{}", cycle, e);
                failures += 1;
                continue;
            }
        };

        // 期望帧数要扣掉启动瞬态丢的那几帧：那几帧确实不在文件里（时间戳保留，
        //，但它不是异常。稳态丢帧仍然照常触发下面的告警。
        let expected = (outcome.elapsed_ms as f64 / 1000.0 * fps as f64).round() as i64
            - outcome.frames_dropped_startup as i64;
        let dur_err = (probe.duration_ms() - outcome.elapsed_ms).abs();
        let dups = outcome.frames_duplicated;
        let dup_pct = if outcome.frames_scheduled > 0 {
            dups as f64 * 100.0 / outcome.frames_scheduled as f64
        } else {
            0.0
        };
        let fps = if outcome.elapsed_ms > 0 {
            probe.frame_count as f64 / (outcome.elapsed_ms as f64 / 1000.0)
        } else {
            0.0
        };
        let partial_left = outcome
            .finalize
            .path
            .with_extension("mp4.partial")
            .exists();

        let mem_delta_mb = (samples.last().copied().unwrap_or(ws0) as f64 - ws0 as f64) / 1048576.0;

        println!(
            "  ✅ 第 {} 轮：{} 解码={}帧 {}x{}（原生 {}x{}）时长={}ms（录制 {}ms，差 {}ms）实到帧率={:.2} \
             重复={:.1}% 丢帧={}（其中启动瞬态 {}） 音频倒退丢弃={} 采集最长无帧={}ms 音频峰值={}（最近1秒 {}） 补位块={} 停止={}ms 内存Δ={:+.1}MB 残留partial={} 结束原因={} 总耗时={}s",
            cycle,
            outcome.finalize.bytes,
            probe.frame_count,
            probe.width,
            probe.height,
            probe.native_width,
            probe.native_height,
            probe.duration_ms(),
            outcome.elapsed_ms,
            dur_err,
            fps,
            dup_pct,
            outcome.frames_dropped_backpressure,
            outcome.frames_dropped_startup,
            outcome.audio_dropped_backwards,
            outcome.capture_idle_ms_max,
            outcome.audio_peak,
            outcome.audio_peak_last_sec,
            outcome.audio_filler_blocks,
            stop_ms,
            mem_delta_mb,
            partial_left,
            outcome.stop_reason.as_str(),
            cycle_start.elapsed().as_secs()
        );

        if probe.gaps > 0 {
            println!("     ⚠️ 时间轴断层 {} 处", probe.gaps);
        }

        // 音频轨与 A/V 同步量
        match screenlite_media::verify::probe_audio(&outcome.finalize.path) {
            Ok(Some(a)) => {
                let av_delta = a.duration_ms() - probe.duration_ms();
                println!(
                    "     🔊 音频轨：{} 块、{} Hz / {} 声道、时长 {}ms；\
                     视频 {}ms → **A/V 差 {}ms**",
                    a.sample_count,
                    a.sample_rate,
                    a.channels,
                    a.duration_ms(),
                    probe.duration_ms(),
                    av_delta
                );
            }
            Ok(None) => println!("     🔇 无音频轨（录制期间系统没有播放声音）"),
            Err(e) => println!("     ⚠️ 音频探测失败：{}", e),
        }
        // 每源明细
        for s in &outcome.audio_sources {
            println!(
                "     音频源明细 kind=\"{}\" endpoint=\"{}\" 块={} 补位={} 真实设备块={} 峰值={} 倒退丢弃={}",
                s.kind,
                s.endpoint,
                s.chunks,
                s.filler_blocks,
                s.real_blocks,
                s.peak,
                s.dropped_backwards
            );
        }
        if (probe.frame_count as i64 - expected).abs() > 3 {
            println!(
                "     ⚠️ 帧数与期望不符：期望约 {}，实际 {}",
                expected, probe.frame_count
            );
        }

        cycle_stats.push((
            outcome.frames_scheduled,
            dups,
            fps,
            stop_ms as f64,
        ));
    }

    let (ws1, pf1) = process_memory();
    println!("\n=== 汇总 ===");
    println!("轮数：{}，失败：{}", repeat, failures);
    for (i, (sched, dup, fps, stop)) in cycle_stats.iter().enumerate() {
        println!(
            "  第 {} 轮：scheduled={} duplicated={} fps={:.2} stop={:.0}ms",
            i + 1,
            sched,
            dup,
            fps,
            stop
        );
    }
    println!(
        "内存：起始工作集 {:.1} MB → 结束 {:.1} MB（峰值 {:.1} MB）；提交 {:.1} → {:.1} MB",
        ws0 as f64 / 1048576.0,
        ws1 as f64 / 1048576.0,
        peak_ws as f64 / 1048576.0,
        pf0 as f64 / 1048576.0,
        pf1 as f64 / 1048576.0
    );

    if failures > 0 {
        println!("❌ 稳定性测试失败（{} 处）", failures);
        std::process::exit(2);
    }
    println!("✅ 稳定性测试完成：{} 轮全部成功，产物均可解码", repeat);
}
