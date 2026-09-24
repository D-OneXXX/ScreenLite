//! 无边框子类的回归测试（Windows only）。
//!
//! ## 为什么必须有真窗口的测试
//!
//! "原生标题栏不再回来"此前只能靠截图 + 轮询 GWL_STYLE 验证，而轮询只能证明
//! "这一秒恰好干净"，证不了"下一次 `apply_diff` 之后也干净"——检查点不可回归
//! 等于没有检查点。
//!
//! 本测试把真凶的动作原样复现：按 tao 0.35.3 `WindowFlags::apply_diff`
//! （`window_state.rs:425-461`）的写法直接
//! ```text
//! SetWindowLongW(GWL_STYLE, to_window_styles() 的产物) ← 含 WS_CAPTION
//! SetWindowPos(..., SWP_FRAMECHANGED)
//! ```
//! 然后断言边框位不可能留在样式里。
//!
//! 子类装在测试线程自建的窗口上，`SetWindowLongW` 会经 `SendMessage`
//! 同步派发 `WM_STYLECHANGING` 到本线程的窗口过程 ⇒ 不需要消息循环，结果确定。
//!
//! ## 覆盖
//! 1. `show()` 的样式写回（≈ `VISIBLE` flag 触发 apply_diff）
//! 2. `hide()` 的样式写回
//! 3. 反复写回（幂等，子类不会被自己冲掉）
//! 4. 重复安装是 no-op
//! 5. 前置条件：不装子类时这个写回真的会写入 `WS_CAPTION`（否则本测试无意义）

#![cfg(windows)]

use screenlite_lib::{enforce_frameless_style, install_frameless_subclass};

use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetClientRect, GetWindowLongW, GetWindowRect,
    RegisterClassW, SendMessageW, SetWindowLongW, SetWindowPos, GWL_STYLE, HMENU, HWND_MESSAGE,
    SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, WNDCLASSW, WS_CAPTION,
    WS_CLIPSIBLINGS, WS_MAXIMIZEBOX, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_POPUP, WS_SYSMENU,
    WS_THICKFRAME, WS_VISIBLE,
};

/// `WindowFlags::to_window_styles()`（tao 0.35.3 `window_state.rs:244`）在
/// "marker decorations=false + minimizable + maximizable + visible" 下的产物。
/// 启动日志里实测到的 `style_before=0x14CB0000` 就是这里的逐位展开。
const TAO_STYLE_UNDECORATED: i32 = WS_CAPTION.0 as i32
    | WS_CLIPSIBLINGS.0 as i32
    | WS_SYSMENU.0 as i32
    | WS_MINIMIZEBOX.0 as i32
    | WS_MAXIMIZEBOX.0 as i32
    | WS_VISIBLE.0 as i32;

const FRAME_BITS: i32 = WS_CAPTION.0 as i32
    | WS_THICKFRAME.0 as i32
    | WS_SYSMENU.0 as i32
    | WS_MINIMIZEBOX.0 as i32
    | WS_MAXIMIZEBOX.0 as i32;

/// `WindowFlags::apply_diff()` 的写回动作：Set → SetWindowPos(FRAMECHANGED)。
/// 这就是"显示主窗口 / 显隐热键"在 tao 里实际发生的两件事。
fn replay_style_like_tao(hwnd: HWND, style: i32) {
    unsafe {
        SetWindowLongW(hwnd, GWL_STYLE, style);
        let _ = SetWindowPos(
            hwnd,
            None,
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
        );
    }
}

fn style_of(hwnd: HWND) -> i32 {
    unsafe { GetWindowLongW(hwnd, GWL_STYLE) }
}

/// 建一个与主窗口同源的顶层窗口（message-only 窗口即可：样式位照样能写能读）。
fn test_hwnd(tag: &str) -> HWND {
    let class_name: Vec<u16> = format!("SLTestFrameless{tag}\0").encode_utf16().collect();
    let wnd = WNDCLASSW {
        lpfnWndProc: Some(test_wndproc),
        lpszClassName: windows::core::PCWSTR(class_name.as_ptr()),
        ..Default::default()
    };
    unsafe {
        let _ = RegisterClassW(&wnd);
        let title: Vec<u16> = format!("sl-test-{tag}\0").encode_utf16().collect();
        CreateWindowExW(
            Default::default(),
            windows::core::PCWSTR(class_name.as_ptr()),
            windows::core::PCWSTR(title.as_ptr()),
            WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU,
            0,
            0,
            40,
            40,
            Some(HWND_MESSAGE),
            Some(HMENU::default()),
            None,
            None,
        )
        .expect("创建测试窗口失败")
    }
}

/// 一条自定义消息，用来证明"子类只改样式、其余照原样转发"。
///
/// 转发链断了的话窗口会整体失灵（不分发 tao 自己的 WM_NCCALCSIZE /
/// WM_NCACTIVATE / 输入消息…），而这类故障肉眼很难看出来的。
const WM_APP_PROBE: u32 = 0x8000 + 0x42;

/// 子类必须原样返回的哨兵值。
const FORWARD_SENTINEL: isize = 0x5CCD_A7ED;

unsafe extern "system" fn test_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == WM_APP_PROBE {
        return LRESULT(FORWARD_SENTINEL);
    }
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

/// 窗口矩形 / 客户区矩形（都取物理像素，直接用屏幕坐标即可）。
fn rects(hwnd: HWND) -> ((i32, i32), (i32, i32)) {
    unsafe {
        let mut w = windows::Win32::Foundation::RECT::default();
        let mut c = windows::Win32::Foundation::RECT::default();
        let _ = GetWindowRect(hwnd, &mut w);
        let _ = GetClientRect(hwnd, &mut c);
        (
            (w.right - w.left, w.bottom - w.top),
            (c.right - c.left, c.bottom - c.top),
        )
    }
}

/// tao 0.35.3 的 `WM_ACTIVATE` 派发入口：失焦 = 1、复焦 = 0（以 is_active 计）。
/// 这里只作为"焦点生命周期"的驱动信号，不作为检查点。
fn drive_focus_change(hwnd: HWND) {
    const WM_ACTIVATE: u32 = 0x0006;
    unsafe {
        SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(0)), Some(LPARAM(0))); // 失焦 (WA_INACTIVE)
        SendMessageW(hwnd, WM_ACTIVATE, Some(WPARAM(1)), Some(LPARAM(0))); // 复焦 (WA_ACTIVE)
    }
}

#[test]
fn tao_style_replay_cannot_reintroduce_the_caption() {
    let hwnd = test_hwnd("replay");

    // ── 前置条件：未装子类时，这个写回必须真的能写入 WS_CAPTION ──
    replay_style_like_tao(hwnd, TAO_STYLE_UNDECORATED);
    let unprotected = style_of(hwnd);
    assert_eq!(
        unprotected & WS_CAPTION.0 as i32,
        WS_CAPTION.0 as i32,
        "前置条件不成立：不装子类时 tao 的写回居然写不进 WS_CAPTION，本测试随之失效"
    );

    install_frameless_subclass(hwnd);

    // ── 场景 1：show() —— 窗口先可见再写样式（标题栏真的会画出来那条）──
    replay_style_like_tao(hwnd, TAO_STYLE_UNDECORATED);
    let after = style_of(hwnd);
    assert_eq!(
        after & FRAME_BITS,
        0,
        "show 的样式写回后不允许残留任何边框位"
    );
    assert_ne!(
        after & WS_VISIBLE.0 as i32,
        0,
        "可见性必须保留（子类不能吞掉别的东西）"
    );
    assert_ne!(
        after & WS_CLIPSIBLINGS.0 as i32,
        0,
        "非边框样式（WS_CLIPSIBLINGS）必须原样保留——只剥边框，不碰别的"
    );
    assert_ne!(
        after & WS_POPUP.0 as i32,
        0,
        "强制 WS_POPUP：与 enforce_frameless_style 的口径保持一致"
    );

    // ── 场景 2：hide() —— 隐藏时那份（同样会被写回）──
    replay_style_like_tao(hwnd, TAO_STYLE_UNDECORATED & !WS_VISIBLE.0 as i32);
    let hidden = style_of(hwnd);
    assert_eq!(
        hidden & FRAME_BITS,
        0,
        "hide 的样式写回后不允许残留任何边框位"
    );

    // ── 场景 3：反复写回（幂等；子类不能把自己冲掉）──
    for i in 0..8 {
        replay_style_like_tao(hwnd, TAO_STYLE_UNDECORATED);
        assert_eq!(
            style_of(hwnd) & FRAME_BITS,
            0,
            "第 {i} 次重复写回后仍不允许残留边框位"
        );
    }

    // ── 场景 4：重复安装是 no-op（不会把子类叠成链）──
    install_frameless_subclass(hwnd);
    install_frameless_subclass(hwnd);
    replay_style_like_tao(hwnd, TAO_STYLE_UNDECORATED);
    assert_eq!(style_of(hwnd) & FRAME_BITS, 0, "重复安装后子类必须依然生效");

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// 上游 issue 的原症状：焦点切换之后非客户区 / 边框回来。
///
/// tauri #14764 / #14859、electron #47946 / #51662 描述的都是"启动干净、
/// 切走焦点再回来，标题栏/背景条出现"。本测试把这个生命周期驱动出来，
/// 断言每一轮之后：① 样式无边框；② 客户区仍然占满整个窗口
/// （②才是"标题栏真的没占到地方"的检查点——只有①可能是账本干净而画面还在）。
#[test]
fn focus_change_cannot_reintroduce_the_non_client_area() {
    let hwnd = test_hwnd("focus");

    install_frameless_subclass(hwnd);
    let clean = enforce_frameless_style(hwnd);
    assert!(clean, "装完子类后应能把出生样式校正为无边框");

    // 前置条件（这条同样是检查点）：客户区必须已经等于窗口矩形。
    // 不等 ⇒ tao 的 WM_NCCALCSIZE 或本项目的 enforce 没生效，后面的断言没意义。
    let (win, cli) = rects(hwnd);
    assert_eq!(
        win, cli,
        "前置条件不成立：校正无边框后客户区应等于窗口矩形（说明标题栏没有占到地方）"
    );

    for i in 0..5 {
        drive_focus_change(hwnd);
        replay_style_like_tao(hwnd, TAO_STYLE_UNDECORATED);
        assert_eq!(
            style_of(hwnd) & FRAME_BITS,
            0,
            "第 {i} 轮焦点切换后样式不得残留边框位"
        );
        assert_eq!(
            rects(hwnd),
            (win, win),
            "第 {i} 轮焦点切换后客户区必须仍占满整个窗口（ghost frame 检查点）"
        );
    }

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// 反回归护栏：子类绝不能接管 `WM_NCCALCSIZE`。
///
/// 这条锁的是一个经过源码核对后做出的"不做"决定，防止后人"好心"补上：
///
/// tao 0.35.3 的 `event_loop.rs:2123` 已经正确处理 `WM_NCCALCSIZE`——非装饰
/// 非最大化时返回 `LRESULT(0)`（客户区占满），并且还带两条我们必须保留的分支：
/// - 最大化时把客户区收到 `rcWork`（否则最大化窗口会盖住任务栏）；
/// - `MARKER_UNDECORATED_SHADOW` 时按 DPI inset 收缩客户区。
///
/// 若我们在子类里无条件返回 0：第一条被破坏；而万一哪天 `shadow` 重新打开，
/// 第二条被破坏 ⇒ "卡片外围一圈浅色边框"整个复活（这个 bug 已出现过一次）。
///
/// 验证方式：子类安装前后，同一条自定义消息的返回值必须一致 ⇒ 转发链完好，
/// tao 的 WM_NCCALCSIZE 分支还在按自己的逻辑跑。
#[test]
fn subclass_must_not_intercept_wm_nccalcsize() {
    let hwnd = test_hwnd("forward");
    let style_before = style_of(hwnd);

    // 装之前先量一次基线：这条消息由 test_wndproc 直接返回哨兵值
    let before = unsafe { SendMessageW(hwnd, WM_APP_PROBE, None, None) };
    assert_eq!(
        before.0, FORWARD_SENTINEL,
        "前置条件：测试窗口必须原样返回哨兵值（否则转发测试无意义）"
    );

    install_frameless_subclass(hwnd);

    let after = unsafe { SendMessageW(hwnd, WM_APP_PROBE, None, None) };
    assert_eq!(
        after.0, FORWARD_SENTINEL,
        "子类必须原样转发非 WM_STYLECHANGING 消息 —— \
         否则 tao 的 WM_NCCALCSIZE / WM_NCACTIVATE / 输入分发会被一起吃掉"
    );
    assert_eq!(
        style_of(hwnd),
        style_before,
        "装子类本身不得改动样式账本（那是 enforce_frameless_style 的职责）；\
         谁顺手改了，谁就会掩盖 tao 自己的窗口生命周期"
    );

    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}
