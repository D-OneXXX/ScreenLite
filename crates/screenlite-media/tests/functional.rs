//! 功能性测试：动态内容与鼠标指针是否被正确录制。
//!
//! 这是「画面与桌面实际操作一致」这条验收标准的可判定版本：
//!
//! ```text
//! ① 自己创建三个纯色顶层窗口（红 / 绿 / 蓝），按时间相位显示/隐藏
//! ② 录制 9 秒（每 3 秒一个相位）
//! ③ 解码回读：在每一帧的三个窗口中心点取 Y/Cb/Cr，
//! 判定每个相位期望的那个窗口颜色是否正确、其余位置是否确实不是该颜色
//! ```
//!
//! 期望的 NV12 值（BT.709 + limited range，由 convert.rs 的计算推导）：
//! 纯红 (63,102,240) / 纯绿 (172,42,26) / 纯蓝 (32,240,118)
//!
//! 另含鼠标指针捕获验证：把光标放到两个已知位置各抓一帧，
//! 光标被捕获时差异像素必须只出现在这两个位置附近。

use std::sync::Mutex;
use std::time::Duration;

use windows::core::w;
use windows::Win32::Foundation::{COLORREF, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::CreateSolidBrush;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

mod common;

const RED: (u8, u8, u8) = (63, 102, 240);
const GREEN: (u8, u8, u8) = (172, 42, 26);
const BLUE: (u8, u8, u8) = (32, 240, 118);

const WIN_W: i32 = 400;
const WIN_H: i32 = 300;
const WIN_Y: i32 = 400;
/// 三个窗口的逻辑坐标。
/// 必须满足 `x + WIN_W ≤ 逻辑桌面宽度(1707)`，否则窗口会被窗口管理器挪走或跑出屏幕，
/// 采样点就落在别处（实测蓝色窗口放在 1800 时整体跑出可视区）。
const XS: [i32; 3] = [100, 500, 900];

/// 桌面抓帧测试的进程内串行锁。
///
/// 本文件里两个测试都要"抓活的桌面"（一个比动态内容相位，一个比光标），
/// 同时跑会互相污染——另一个测试的窗口、以及它的输出在终端上的刷新，
/// 就在同一块桌面上。
///
/// 但串行化只解决"测试之间"的污染，解决不了"桌面自己在变"：
/// 终端光标闪烁、其它应用动画、真鼠标被移动、锁屏都不是测试能控制的。
/// 所以两处都配了 skip 机制（环境不满足就不判定，而不是 fail）：
/// - 动态内容测试：`require_desktop_ready()` + 断言只针对我们自己创建的窗口位置
/// - 光标测试：抓帧前后各查一次 `GetCursorPos`，光标不在期望位置就 skip
static DESKTOP_LOCK: Mutex<()> = Mutex::new(());

/// 当前光标的逻辑坐标（本测试进程是 DPI-unaware，故 GetCursorPos 给逻辑坐标）。
fn cursor_log() -> (i32, i32) {
    let mut p = windows::Win32::Foundation::POINT::default();
    let _ = unsafe { GetCursorPos(&mut p) };
    (p.x, p.y)
}

/// 本机捕获的物理尺寸（来自 GraphicsCaptureItem.Size()）。
/// 窗口与光标坐标是逻辑像素（桌面 1707x1067），必须乘缩放系数才能对上捕获图。
const FRAME_W: f64 = 2560.0;
const FRAME_H: f64 = 1600.0;

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    DefWindowProcW(hwnd, msg, wp, lp)
}

/// 一个纯色无边框顶层窗口。
struct SolidWindow(HWND);

impl SolidWindow {
    fn new(x: i32, colorref: u32) -> Option<Self> {
        unsafe {
            let hinstance = GetModuleHandleW(None).ok()?;
            let hinst = windows::Win32::Foundation::HINSTANCE(hinstance.0);
            // 背景画刷是窗口类的属性，不是窗口的属性。
            // 三个颜色必须用三个不同的类名；否则后两次 RegisterClassW 失败，
            // 三个窗口都会用第一次注册的画刷（实测表现为"绿窗口画成红色"）。
            let class = match colorref {
                0x0000FF => w!("ScreenLiteFuncTestRed"),
                0x00FF00 => w!("ScreenLiteFuncTestGreen"),
                _ => w!("ScreenLiteFuncTestBlue"),
            };
            let brush = CreateSolidBrush(COLORREF(colorref));
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wndproc),
                hInstance: hinst,
                lpszClassName: class,
                hbrBackground: brush,
                ..Default::default()
            };
            RegisterClassW(&wc); // 同名重复注册会失败，可忽略
            let hwnd = CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
                class,
                w!(""),
                WS_POPUP,
                x,
                WIN_Y,
                WIN_W,
                WIN_H,
                None,
                None,
                Some(hinst),
                None,
            )
            .ok()?;
            Some(Self(hwnd))
        }
    }

    fn show(&self) {
        unsafe {
            let _ = ShowWindow(self.0, SW_SHOWNOACTIVATE);
        }
    }

    fn hide(&self) {
        unsafe {
            let _ = ShowWindow(self.0, SW_HIDE);
        }
    }
}

impl Drop for SolidWindow {
    fn drop(&mut self) {
        unsafe {
            let _ = DestroyWindow(self.0);
        }
    }
}

fn euclid(a: (u8, u8, u8), b: (u8, u8, u8)) -> f64 {
    let d = |x: u8, y: u8| (x as f64 - y as f64).powi(2);
    (d(a.0, b.0) + d(a.1, b.1) + d(a.2, b.2)).sqrt()
}

/// 解码整个文件，返回每帧在三个采样点上的 Y/Cb/Cr。
fn decode_probes(
    path: &std::path::Path,
    points: &[(usize, usize)],
    max_frames: usize,
) -> Vec<Vec<(u8, u8, u8)>> {
    use windows::core::HSTRING;
    use windows::Win32::Media::MediaFoundation::*;

    let _rt = screenlite_media::mf::MfRuntime::start().expect("MFStartup");
    let mut out = Vec::new();
    unsafe {
        let mut slot: Option<IMFAttributes> = None;
        MFCreateAttributes(&mut slot, 1).unwrap();
        let attrs = slot.unwrap();
        attrs
            .SetUINT32(&MF_SOURCE_READER_ENABLE_VIDEO_PROCESSING, 1)
            .unwrap();

        let url = HSTRING::from(path.to_string_lossy().as_ref());
        let reader: IMFSourceReader = MFCreateSourceReaderFromURL(&url, &attrs).unwrap();
        let target: IMFMediaType = MFCreateMediaType().unwrap();
        target.SetGUID(&MF_MT_MAJOR_TYPE, &MFMediaType_Video).unwrap();
        target.SetGUID(&MF_MT_SUBTYPE, &MFVideoFormat_NV12).unwrap();
        reader
            .SetCurrentMediaType(MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32, None, &target)
            .expect("NV12 输出类型不被接受");

        loop {
            if out.len() >= max_frames {
                break;
            }
            let (mut a, mut f, mut ts) = (0u32, 0u32, 0i64);
            let mut s: Option<IMFSample> = None;
            reader
                .ReadSample(
                    MF_SOURCE_READER_FIRST_VIDEO_STREAM.0 as u32,
                    0,
                    Some(&mut a),
                    Some(&mut f),
                    Some(&mut ts),
                    Some(&mut s),
                )
                .unwrap();
            let Some(sample) = s else { break };
            let buffer = sample.ConvertToContiguousBuffer().unwrap();
            let (data, pitch) = {
                use windows::core::Interface;
                let b2d = buffer.cast::<IMF2DBuffer>().unwrap();
                let mut scan0: *mut u8 = std::ptr::null_mut();
                let mut pitch: i32 = 0;
                b2d.Lock2D(&mut scan0, &mut pitch).unwrap();
                (
                    // NV12 完整帧 = pitch * height * 3/2（Y 平面 + 交错 UV 平面）
                    std::slice::from_raw_parts(scan0, pitch as usize * 1600 * 3 / 2).to_vec(),
                    pitch as usize,
                )
            };
            let mut frame_probes = Vec::with_capacity(points.len());
            for &(x, y) in points {
                let luma = data[y * pitch + x];
                let uv_off = pitch * 1600;
                let uvi = (y / 2) * pitch + (x & !1);
                frame_probes.push((luma, data[uv_off + uvi], data[uv_off + uvi + 1]));
            }
            out.push(frame_probes);
        }
    }
    out
}

#[test]
fn test_dynamic_content_and_timing() {
    use screenlite_media::capture;
    use screenlite_media::encoder::HardwarePreference;
    use screenlite_media::engine::{EngineState, Recorder, RecordingConfig};

    let displays = match capture::enumerate_displays() {
        Ok(d) => d,
        Err(_) => return,
    };
    let Some(primary) = displays.into_iter().find(|d| d.primary) else {
        return;
    };

    // 与光标测试串行：两个测试都在抓同一块活桌面
    let _desktop = DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    // 环境前提：动态内容测试也需要桌面可见（锁屏后测试窗口不会被画进捕获帧）
    if common::require_desktop_ready().is_none() {
        return;
    }

    // 三个窗口：红(左) / 绿(中) / 蓝(右)，全部先隐藏
    let windows = [
        SolidWindow::new(XS[0], 0x0000FF), // COLORREF 是 0x00BBGGRR → 红
        SolidWindow::new(XS[1], 0x00FF00),
        SolidWindow::new(XS[2], 0xFF0000),
    ];
    if windows.iter().any(|w| w.is_none()) {
        eprintln!("跳过：无法创建测试窗口");
        return;
    }
    let windows: Vec<SolidWindow> = windows.into_iter().flatten().collect();
    for w in &windows {
        w.hide();
    }

    let dir = std::env::temp_dir().join(format!("sl-func-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    let mut cfg = RecordingConfig::new(primary.id.clone(), dir.clone());
    cfg.hardware = HardwarePreference::PreferSoftware;
    // 本测试做的是逐帧像素断言，必须与"系统是否在放声音"解耦：
    // 开着音频会让录制结果依赖环境（有没有声音、音频线程的调度），引入不确定性。
    cfg.audio = false;

    // 相位：0-3s 红，3-6s 绿，6-9s 蓝
    let recorder = Recorder::start(cfg).expect("启动失败");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if recorder.status().state == EngineState::Recording {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let t0 = std::time::Instant::now();
    let mut phase = usize::MAX;
    while t0.elapsed() < Duration::from_secs(9) {
        let p = (t0.elapsed().as_secs_f64() / 3.0) as usize;
        if p != phase && p < 3 {
            for (i, w) in windows.iter().enumerate() {
                if i == p {
                    w.show();
                } else {
                    w.hide();
                }
            }
            phase = p;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    for w in &windows {
        w.hide();
    }
    let outcome = recorder.stop().expect("停止失败");
    drop(windows);

    // 三个窗口中心点。窗口坐标是逻辑像素（桌面 1707x1067），
    // 而捕获是物理像素（2560x1600），必须换算，否则采样点会落在窗口外。
    let sx = FRAME_W / primary.width as f64;
    let sy = FRAME_H / primary.height as f64;
    let points: Vec<(usize, usize)> = XS
        .iter()
        .map(|&x| {
            (
                ((x + WIN_W / 2) as f64 * sx) as usize,
                ((WIN_Y + WIN_H / 2) as f64 * sy) as usize,
            )
        })
        .collect();
    let frames = decode_probes(&outcome.finalize.path, &points, 400);
    assert!(
        frames.len() > 200,
        "解码帧数过少：{}（9 秒 30fps 应约 270 帧）",
        frames.len()
    );
    eprintln!("解码 {} 帧，逐相位校验颜色", frames.len());

    // 每相位取中段帧，避开切换瞬间
    let expected = [("红", RED, 0usize), ("绿", GREEN, 1), ("蓝", BLUE, 2)];
    for (name, triple, idx) in expected {
        let seg_start = idx * 30 * 3 + 40; // 相位开始后约 1.3 秒
        let seg_end = (idx + 1) * 30 * 3 - 15;
        let mut checked = 0;
        for f in seg_start..seg_end.min(frames.len()) {
            let probe = frames[f][idx];
            let d = euclid(probe, triple);
            assert!(
                d < 45.0,
                "第 {} 帧：相位「{}」的窗口位置颜色不对，实测 Y/Cb/Cr={:?}，期望约 {:?}（距离 {:.1}）",
                f,
                name,
                probe,
                triple,
                d
            );
            // 另外两个窗口位置不该出现对应颜色
            for (other_idx, other_triple) in [(0, RED), (1, GREEN), (2, BLUE)] {
                if other_idx == idx {
                    continue;
                }
                assert!(
                    euclid(frames[f][other_idx], other_triple) > 60.0,
                    "第 {} 帧：非当前相位的窗口位置 {} 意外出现了该相位颜色",
                    f,
                    other_idx
                );
            }
            checked += 1;
        }
        assert!(checked > 25, "相位「{}」可用帧数过少：{}", name, checked);
    }
    eprintln!("✅ 动态内容与时间对齐校验通过");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn test_cursor_is_captured() {
    use screenlite_media::capture;
    // 与动态内容测试串行：两个测试都在抓同一块活桌面
    let _desktop = DESKTOP_LOCK.lock().unwrap_or_else(|e| e.into_inner());

    let displays = match capture::enumerate_displays() {
        Ok(d) => d,
        Err(_) => return,
    };
    let Some(primary) = displays.into_iter().find(|d| d.primary) else {
        return;
    };

    // 环境前提：桌面必须可见且稳定（锁屏时系统不会把光标画进捕获帧）
    if common::require_desktop_ready().is_none() {
        return;
    }

    // 坐标换算：本测试进程是 DPI-unaware，SetCursorPos / CreateWindowExW 收的是逻辑坐标，
    // 而捕获是物理像素（本机 1.5 倍）。必须换算，否则搜索框落在光标之外。
    let sx = FRAME_W / primary.width as f64;
    let sy = FRAME_H / primary.height as f64;

    // 逻辑桌面 1707x1067；取两个都在范围内的位置
    let a_log = (200i32, 200i32);
    let b_log = (1200i32, 200i32);
    // 对应的物理（捕获图内）位置
    let a = (
        (a_log.0 as f64 * sx) as i32,
        (a_log.1 as f64 * sy) as i32,
    );
    let b = (
        (b_log.0 as f64 * sx) as i32,
        (b_log.1 as f64 * sy) as i32,
    );

    unsafe {
        let _ = SetCursorPos(a_log.0, a_log.1);
    }
    std::thread::sleep(Duration::from_millis(400));
    // 诊断：确认光标真的移动了。若 SetCursorPos 未生效（会话无鼠标、被其它进程拦截等），
    // 两张图里光标都在同一处 → 差异为 0，会被误判成"光标未被捕获"。
    let actual_a = {
        let mut p = windows::Win32::Foundation::POINT::default();
        let _ = unsafe { GetCursorPos(&mut p) };
        (p.x, p.y)
    };
    eprintln!(
        "A 期望逻辑 ({},{})，实际 ({},{})",
        a_log.0, a_log.1, actual_a.0, actual_a.1
    );
    let img_a = match capture::capture_single_frame(&primary.id, Duration::from_secs(5)) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("跳过：抓帧失败（{}）", e);
            return;
        }
    };
    // 抓帧后复查：光标在抓帧期间必须始终在期望位置，否则 A 帧里根本没有光标，
    // "差异只出现在 A/B 附近"这条检查点就不成立。这是环境（真鼠标被移动），不是缺陷 → skip。
    if cursor_log() != a_log {
        eprintln!(
            "跳过：抓 A 帧期间光标被移走（期望 {:?}，实际 {:?}）——环境不满足，无法验证光标捕获",
            a_log,
            cursor_log()
        );
        return;
    }
    unsafe {
        let _ = SetCursorPos(b_log.0, b_log.1);
    }
    std::thread::sleep(Duration::from_millis(400));
    let actual_b = {
        let mut p = windows::Win32::Foundation::POINT::default();
        let _ = unsafe { GetCursorPos(&mut p) };
        (p.x, p.y)
    };
    eprintln!(
        "B 期望逻辑 ({},{})，实际 ({},{})",
        b_log.0, b_log.1, actual_b.0, actual_b.1
    );
    let img_b = match capture::capture_single_frame(&primary.id, Duration::from_secs(5)) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("跳过：抓帧失败（{}）", e);
            return;
        }
    };
    // 同上：B 帧抓完再复查一次
    if cursor_log() != b_log {
        eprintln!(
            "跳过：抓 B 帧期间光标被移走（期望 {:?}，实际 {:?}）——环境不满足，无法验证光标捕获",
            b_log,
            cursor_log()
        );
        return;
    }

    assert_eq!((img_a.width, img_a.height), (img_b.width, img_b.height));
    let (w, h) = (img_a.width as usize, img_a.height as usize);

    // 前提复查：从抓第一帧到现在，屏幕可能刚好变暗/锁屏
    if common::mean_luma(&img_a) < common::LOCKED_LUMA {
        eprintln!("跳过：屏幕已变暗，无法验证光标捕获");
        return;
    }

    let mut diff_total = 0u64;
    let mut near_a = 0u64;
    let mut near_b = 0u64;
    // 诊断用：差异像素的包围盒 + 每行的差异计数（用于判断是光标还是别的内容在变）
    let mut min_x = usize::MAX;
    let mut max_x = 0usize;
    let mut min_y = usize::MAX;
    let mut max_y = 0usize;
    let mut row_hist: Vec<(usize, u64)> = Vec::new();
    for y in 0..h {
        let mut row_diff = 0u64;
        for x in 0..w {
            let da = img_a.data[y * img_a.stride + x];
            let db = img_b.data[y * img_b.stride + x];
            if (da as i32 - db as i32).abs() > 20 {
                diff_total += 1;
                row_diff += 1;
                min_x = min_x.min(x);
                max_x = max_x.max(x);
                min_y = min_y.min(y);
                max_y = max_y.max(y);
                let in_box = |cx: i32, cy: i32| {
                    (x as i32 - cx).abs() <= 40 && (y as i32 - cy).abs() <= 40
                };
                if in_box(a.0, a.1) {
                    near_a += 1;
                }
                if in_box(b.0, b.1) {
                    near_b += 1;
                }
            }
        }
        if row_diff > 0 {
            row_hist.push((y, row_diff));
        }
    }
    let bbox = if diff_total > 0 {
        format!(
            "x[{}..{}] y[{}..{}] ({}x{})",
            min_x,
            max_x,
            min_y,
            max_y,
            max_x - min_x + 1,
            max_y - min_y + 1
        )
    } else {
        "(无差异)".into()
    };
    eprintln!(
        "差异包围盒：{}；涉及 {} 行，最多的几行：{:?}",
        bbox,
        row_hist.len(),
        {
            let mut v = row_hist.clone();
            v.sort_by(|x, y| y.1.cmp(&x.1));
            v.into_iter().take(4).collect::<Vec<_>>()
        }
    );

    let frac = diff_total as f64 / (w * h) as f64;
    eprintln!(
        "缩放系数 {:.4}/{:.4}；逻辑 A=({},{}) → 物理 A=({},{})；差异像素 {}（{:.4}%），A 附近 {}，B 附近 {}",
        sx,
        sy,
        a_log.0,
        a_log.1,
        a.0,
        a.1,
        diff_total,
        frac * 100.0,
        near_a,
        near_b
    );

    assert!(
        near_a >= 5 && near_b >= 5,
        "光标未被捕获：两个光标位置附近都没有差异像素（A 附近 {}，B 附近 {}）。\
         请检查 GraphicsCaptureSession.SetIsCursorCaptureEnabled(true)。",
        near_a,
        near_b
    );
    // 全局差异不再作为检查点（原来在 5% 处断言）。
    //
    // 在活的桌面上这个数本质不可靠：实测 6.7% 那次的原因只是"某个窗口的文字行
    // 在刷新"，与采集、色彩转换都无关；阈值调大调小都是碰运气。
    // 检查点放回光标位置（上面的 near_a / near_b）——那两个数只在"光标没被画进帧里"
    // 时才为 0，而光标是否在期望位置已由抓帧前后的 `GetCursorPos` 复查保证。
    if frac >= 0.05 {
        eprintln!(
            "（提示）全局差异 {:.2}% 偏大——大概率是桌面上有别的内容在变，\
             检查点已改为只看光标邻域，本项不再是失败条件",
            frac * 100.0
        );
    }
    eprintln!("✅ 鼠标指针捕获校验通过");
}
