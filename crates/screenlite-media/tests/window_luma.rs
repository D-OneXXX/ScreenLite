//! 诊断测试：ScreenLite 窗口的内容区是不是黑的。
//!
//! 运行（需要 App 正在运行）：
//! ```text
//! cargo test --release -p screenlite-media --test window_luma -- --nocapture
//! ```
//!
//! 为什么要它：用户报"整个窗口是黑屏的"，但"黑屏"有三种完全不同的成因，肉眼截图分不清：
//! ① 窗口内容区真的是黑的（webview 没渲染）
//! ② 截图工具抓不到 webview 的合成表面（DirectComposition）——实际屏幕是好的
//! ③ 抓帧工具（WGC）自己拿不到该窗口
//! 所以这里用同一帧捕获同时量三块区域的亮度做对照：
//! - 标题栏（原生绘制，应当亮 ≈ 240）→ 证明"这一帧确实包含了该窗口"
//! - 内容区（webview 绘制，黑屏时 ≈ 0）
//! - 另一个大窗口的内容区（对照，证明测量本身能看见非黑内容）
//!
//! 环境前提：需要可见桌面 + App 正在运行；不满足时跳过（与 functional 的做法一致）。

use std::time::Duration;

use screenlite_media::capture;
use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, RECT};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GetWindowRect, GetWindowTextLengthW, GetWindowTextW, GetWindowThreadProcessId,
    HWND_TOP, IsIconic, IsWindowVisible, SW_RESTORE, SWP_NOMOVE, SWP_NOSIZE, SWP_SHOWWINDOW,
    SetForegroundWindow, SetWindowPos, ShowWindow,
};

mod common;

#[derive(Clone)]
struct Win {
    hwnd: HWND,
    pid: u32,
    title: String,
    rect: RECT,
}

unsafe extern "system" fn enum_proc(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let list = &mut *(lparam.0 as *mut Vec<Win>);
    if !IsWindowVisible(hwnd).as_bool() {
        return true.into();
    }
    let len = GetWindowTextLengthW(hwnd);
    if len <= 0 {
        return true.into();
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return true.into();
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return true.into();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let copied = GetWindowTextW(hwnd, &mut buf);
    list.push(Win {
        title: String::from_utf16_lossy(&buf[..copied as usize]),
        rect,
        hwnd,
        pid,
    });
    true.into()
}

/// 把矩形区域打印成 ASCII 缩略图（按亮度分档），用于"是空白还是真渲染了界面"的判定。
///
/// 深色 UI 与空白深色背景的平均亮度可能一样（本项目的主题 `#14161a` 亮度就是 21.9），
/// 所以平均值不够用——结构才说明问题：有界面就有文字/按钮的亮块。
fn ascii_thumb(img: &screenlite_media::convert::Nv12Image, r: (i32, i32, i32, i32), cols: usize) -> f64 {
    let (x0, y0, x1, y1) = r;
    let x0 = x0.max(0) as usize;
    let y0 = y0.max(0) as usize;
    let x1 = (x1 as usize).min(img.width as usize);
    let y1 = (y1 as usize).min(img.height as usize);
    if x1 <= x0 || y1 <= y0 {
        return f64::NAN;
    }
    let ramp = b" .:-=+*#%@";
    let bw = ((x1 - x0) as f64 / cols as f64).ceil().max(1.0) as usize;
    let rows = ((y1 - y0) as f64 / (bw as f64 * 2.0)).ceil().max(1.0) as usize;
    let bh = ((y1 - y0) as f64 / rows as f64).ceil().max(1.0) as usize;
    println!("  内容区缩略图（宽 {} 格，每格约 {}x{} 像素）:", cols, bw, bh);
    let mut lo = f64::MAX;
    let mut hi = f64::MIN;
    let mut y = y0;
    while y < y1 {
        let mut line = String::from("  |");
        let mut x = x0;
        while x < x1 {
            let mut sum = 0u64;
            let mut n = 0u64;
            for yy in y..(y + bh).min(y1) {
                let row = &img.data[yy * img.stride..yy * img.stride + img.width as usize];
                for &v in &row[x..(x + bw).min(x1)] {
                    sum += v as u64;
                    n += 1;
                }
            }
            let m = if n > 0 { sum as f64 / n as f64 } else { 0.0 };
            lo = lo.min(m);
            hi = hi.max(m);
            let idx = ((m / 255.0 * (ramp.len() - 1) as f64).round() as usize).min(ramp.len() - 1);
            line.push(ramp[idx] as char);
            x += bw;
        }
        line.push('|');
        println!("{}", line);
        y += bh;
    }
    // 块亮度极差：检查点（有界面 ↔ 空白页）。空白页各块几乎相同 → ≈ 0。
    hi - lo
}

/// 把区域导出成 PGM（P5 灰度，无压缩），便于人工看一眼"窗口里到底是什么"。
///
/// 为什么要它：平均亮度和 ASCII 缩略图只能定性（"有结构/没结构"），
/// 但分不清"空白页""错误页""渲染了一半"——直接把像素导出来看最快。
/// 用 PGM 是因为不需要任何图像库；旁边用 Python 转 PNG 即可查看。
fn dump_pgm(img: &screenlite_media::convert::Nv12Image, r: (i32, i32, i32, i32), path: &str) {
    let (x0, y0, x1, y1) = r;
    let x0 = x0.max(0) as usize;
    let y0 = y0.max(0) as usize;
    let x1 = (x1 as usize).min(img.width as usize);
    let y1 = (y1 as usize).min(img.height as usize);
    if x1 <= x0 || y1 <= y0 {
        return;
    }
    let (w, h) = (x1 - x0, y1 - y0);
    let mut out = format!("P5\n{} {}\n255\n", w, h).into_bytes();
    for y in y0..y1 {
        let row = &img.data[y * img.stride..y * img.stride + img.width as usize];
        out.extend_from_slice(&row[x0..x1]);
    }
    match std::fs::write(path, &out) {
        Ok(()) => println!("  已导出窗口内容：{}（{}x{}）", path, w, h),
        Err(e) => println!("  导出失败：{}", e),
    }
}

/// 量物理像素矩形内的 Y 平面平均亮度（矩形按左、上、右、下给出）。
fn mean_luma_rect(img: &screenlite_media::convert::Nv12Image, r: (i32, i32, i32, i32)) -> f64 {
    let (x0, y0, x1, y1) = r;
    let x0 = x0.max(0) as usize;
    let y0 = y0.max(0) as usize;
    let x1 = (x1 as usize).min(img.width as usize);
    let y1 = (y1 as usize).min(img.height as usize);
    if x1 <= x0 || y1 <= y0 {
        return f64::NAN;
    }
    let mut sum = 0u64;
    let mut n = 0u64;
    for y in y0..y1 {
        let row = &img.data[y * img.stride..y * img.stride + img.width as usize];
        for &v in &row[x0..x1] {
            sum += v as u64;
            n += 1;
        }
    }
    sum as f64 / n as f64
}

#[test]
fn screenlite_window_content_is_not_black() {
    let Some(id) = common::primary_display_id() else {
        eprintln!("跳过：没有可用显示器");
        return;
    };
    if common::require_desktop_ready().is_none() {
        return;
    }

    let mut wins: Vec<Win> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut wins as *mut _ as isize));
    }
    // 找窗口不能只看标题：实测 Windows 资源管理器打开本工程目录时，
    // 它的标题就是文件夹名 "ScreenLite"，标题匹配会命中它，量到的是资源管理器。
    // 优先用 SL_APP_PID（由调用方从 tasklist 取），否则回退到"标题匹配中面积最小的那个"
    // （App 窗口 760×560，远小于最大化的资源管理器）。
    let want_pid: Option<u32> = std::env::var("SL_APP_PID")
        .ok()
        .and_then(|v| v.trim().parse().ok());
    let cands: Vec<&Win> = wins
        .iter()
        .filter(|w| w.title.contains("ScreenLite"))
        .filter(|w| want_pid.map_or(true, |p| w.pid == p))
        .collect();
    let Some(app) = cands.iter().min_by_key(|w| {
        (w.rect.right - w.rect.left) as i64 * (w.rect.bottom - w.rect.top) as i64
    }) else {
        eprintln!("跳过：没找到 ScreenLite 窗口（App 没在运行？）");
        return;
    };
    eprintln!(
        "命中窗口：pid={} title={:?}（候选项 {} 个）",
        app.pid,
        app.title,
        cands.len()
    );
    let mut app_rect = app.rect;
    // 最小化窗口的 GetWindowRect 会返回 -32000 之类，直接换算会把整个屏幕框进来（这个坑先记着）
    if app_rect.left <= -10000 || unsafe { IsIconic(app.hwnd).as_bool() } {
        eprintln!("窗口已最小化 → 还原后重测");
        unsafe {
            let _ = ShowWindow(app.hwnd, SW_RESTORE);
            let _ = SetForegroundWindow(app.hwnd);
        }
        std::thread::sleep(Duration::from_millis(800));
        let mut r2 = RECT::default();
        unsafe {
            let _ = GetWindowRect(app.hwnd, &mut r2);
        }
        app_rect = r2;
    }
    if app_rect.left <= -10000 {
        eprintln!("跳过：窗口矩形仍是 {}-{}（无法还原）", app_rect.left, app_rect.top);
        return;
    }
    // 关键：捕获的是屏幕，不是这个窗口的私有缓冲。若有别的窗口压在它上面，
    // 量到的就是那个窗口（实测：量到了终端/编辑器的内容）。
    // 所以测量前必须把目标窗口提到 z 序最前。
    unsafe {
        let _ = ShowWindow(app.hwnd, SW_RESTORE);
        let _ = SetWindowPos(
            app.hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(app.hwnd);
    }
    std::thread::sleep(Duration::from_millis(700));
    let mut r3 = RECT::default();
    unsafe {
        let _ = GetWindowRect(app.hwnd, &mut r3);
    }
    app_rect = r3;
    eprintln!(
        "ScreenLite 窗口（逻辑坐标）：({},{}) {}x{}",
        app_rect.left,
        app_rect.top,
        app_rect.right - app_rect.left,
        app_rect.bottom - app_rect.top
    );

    let img = match capture::capture_single_frame(&id, Duration::from_secs(5)) {
        Ok(i) => i,
        Err(e) => {
            eprintln!("跳过：抓帧失败（{}）", e);
            return;
        }
    };
    let displays = capture::enumerate_displays().unwrap_or_default();
    let Some(primary) = displays.into_iter().find(|d| d.primary) else {
        eprintln!("跳过：拿不到主显示器尺寸");
        return;
    };
    // 本进程 DPI-unaware：GetWindowRect 给逻辑坐标，捕获帧是物理像素 → 必须换算
    let sx = img.width as f64 / primary.width as f64;
    let sy = img.height as f64 / primary.height as f64;
    eprintln!(
        "帧 {}x{}，逻辑桌面 {}x{}，缩放 {:.4}/{:.4}",
        img.width, img.height, primary.width, primary.height, sx, sy
    );

    let map = |x: i32, y: i32| ((x as f64 * sx) as i32, (y as f64 * sy) as i32);
    let (lx0, ly0) = map(app_rect.left, app_rect.top);
    let (lx1, ly1) = map(app_rect.right, app_rect.bottom);
    // 标题栏约占 32 逻辑像素，内容区从它下方开始
    let (_, cy0) = map(0, app_rect.top + 32);

    let title_luma = mean_luma_rect(&img, (lx0, ly0, lx1, cy0));
    let content_luma = mean_luma_rect(&img, (lx0, cy0, lx1, ly1));

    // 对照：任意一个其它的大窗口内容区
    let other = wins
        .iter()
        .filter(|w| !w.title.contains("ScreenLite"))
        .max_by_key(|w| {
            (w.rect.right - w.rect.left) as i64 * (w.rect.bottom - w.rect.top) as i64
        });
    println!("================ 测量结果 ================");
    println!("  ScreenLite 标题栏亮度 : {:.2}", title_luma);
    println!("  ScreenLite 内容区亮度 : {:.2}   ← 黑屏时 ≈ 0", content_luma);
    if let Some(o) = other {
        let (ox0, oy0) = map(o.rect.left, o.rect.top + 32);
        let (ox1, oy1) = map(o.rect.right, o.rect.bottom);
        let l = mean_luma_rect(&img, (ox0, oy0, ox1, oy1));
        println!("  对照窗口「{}」内容区亮度 : {:.2}", o.title, l);
    }
    println!("=========================================");

    if !title_luma.is_finite() || title_luma < 40.0 {
        eprintln!("跳过：这一帧里连标题栏都不亮，说明该窗口没被捕获到（不是产品缺陷）");
        return;
    }
    let spread = ascii_thumb(&img, (lx0, cy0, lx1, ly1), 60);
    println!(
        "  内容区结构强度（块亮度极差）: {:.2}   ← 空白页 ≈ 0，有界面则明显 >40",
        spread
    );
    dump_pgm(
        &img,
        (lx0, cy0, lx1, ly1),
        &std::env::temp_dir()
            .join("sl-window-content.pgm")
            .to_string_lossy(),
    );
    // 整屏也导一份：窗口被别的（可能是 topmost 的）窗口盖住时，只有整屏图才能看出
    // "到底是谁在屏幕上、目标窗口在哪儿" —— 实测踩过这个坑。
    dump_pgm(
        &img,
        (0, 0, img.width as i32, img.height as i32),
        &std::env::temp_dir().join("sl-screen.pgm").to_string_lossy(),
    );
    // 检查点是结构而不是绝对亮度：本项目 UI 是深色主题（#14161a 那一档），
    // 渲染正常的深色界面与"什么都没画的空背景"平均亮度可能一样（21 vs 21.9）。
    assert!(
        spread >= 40.0,
        "ScreenLite 窗口内容区没有结构（块亮度极差 {:.2}，平均亮度 {:.2}，标题栏 {:.2}）\
         ——webview 内容没有渲染出来",
        spread,
        content_luma,
        title_luma
    );
}
