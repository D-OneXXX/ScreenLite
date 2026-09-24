//! 平台行为验证：`EnumWindows`的枚举顺序是不是 z 序（从上到下）。
//!
//! 运行：`cargo test --release --test window_zorder -- --nocapture`
//!
//! ## 为什么需要它
//!
//! 框选覆盖层的"点一下选中整个窗口"依赖一条前提：**窗口列表里第一个包含光标的窗口，
//! 就是视觉上最靠上的那一个**。前端原先用"面积最小的包含窗口"近似这条前提，
//! 结果同位置有浮层时会选中外层大窗口（实测报过：浮层选错、范围明显不对）。
//!
//! 改成"取第一个命中"的前提是 `EnumWindows` 按 z 序遍历——而 MSDN 只承诺
//! "枚举所有顶层窗口"，z 序是业界普遍依赖的实现行为，文档没有白纸黑字承诺。
//! 所以本测试用 `WindowFromPoint` 做地面真值交叉验证：
//! 对每个候选窗口的中心点，比较"我们列表里第一个包含该点的窗口"与
//! "`WindowFromPoint` 真正命中的顶层窗口"是否一致。
//!
//! ## 为什么产品代码反而不能用 `WindowFromPoint`
//!
//! 覆盖层自己是全屏置顶窗口，`WindowFromPoint` 永远返回覆盖层自己，拿不到它下面的
//! 窗口。要拿到就得临时加/去 `WS_EX_TRANSPARENT` 做穿透（会连带丢掉拖拽事件，
//! 与现有交互冲突），或自己按 z 序枚举跳过本进程窗口——那就绕回本方案了。
//! 再加上 30Hz 限流、IPC 往返、COM 调用，全是给这个陷阱付的税。
//! 而测试进程没有自己的覆盖层，此处 `WindowFromPoint` 是可信的。
//!
//! 环境前提：需要有可见的、带标题的顶层窗口；无桌面会话时自行跳过
//! （与 `content_truth` / `functional` 的环境前提检查做法一致）。

use windows::core::BOOL;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, RECT};
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, GA_ROOT, GetAncestor, GetForegroundWindow, GetWindowRect, GetWindowTextLengthW,
    GetWindowTextW, GetWindowThreadProcessId, IsWindowVisible, WindowFromPoint,
};

/// 候选窗口：过滤条件与 `capture::top_windows::enum_proc` 完全一致，
/// 否则交叉验证就不是在验证那条真正生效的顺序了。
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
        return true.into(); // 无标题的（桌面、各类工具窗）不参与吸附
    }
    let mut pid = 0u32;
    GetWindowThreadProcessId(hwnd, Some(&mut pid));
    if pid == GetCurrentProcessId() {
        return true.into(); // 跳过本进程窗口（与产品代码一致）
    }
    let mut rect = RECT::default();
    if GetWindowRect(hwnd, &mut rect).is_err() {
        return true.into();
    }
    if rect.right - rect.left < 80 || rect.bottom - rect.top < 60 {
        return true.into(); // 太小（浮动工具条等）不值得吸附
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let copied = GetWindowTextW(hwnd, &mut buf);
    let title = String::from_utf16_lossy(&buf[..copied as usize]);
    if title.trim().is_empty() {
        return true.into();
    }

    list.push(Win {
        hwnd,
        pid,
        title,
        rect,
    });
    true.into()
}

fn title_of(hwnd: HWND) -> String {
    let len = unsafe { GetWindowTextLengthW(hwnd) };
    if len <= 0 {
        return String::new();
    }
    let mut buf = vec![0u16; (len + 1) as usize];
    let copied = unsafe { GetWindowTextW(hwnd, &mut buf) };
    String::from_utf16_lossy(&buf[..copied as usize])
}

fn contains(r: &RECT, p: POINT) -> bool {
    p.x >= r.left && p.x < r.right && p.y >= r.top && p.y < r.bottom
}

#[test]
fn enum_windows_order_matches_window_from_point() {
    let mut wins: Vec<Win> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(enum_proc), LPARAM(&mut wins as *mut _ as isize));
    }
    if wins.len() < 2 {
        eprintln!(
            "跳过：当前会话可见的带标题顶层窗口不足（{} 个）——本测试需要桌面会话",
            wins.len()
        );
        return;
    }

    eprintln!("EnumWindows 顺序（前 10，越靠前应越靠上）：");
    for (i, w) in wins.iter().take(10).enumerate() {
        eprintln!(
            "  [{}] \"{}\"  rect=({},{},{}x{}) pid={}",
            i,
            w.title,
            w.rect.left,
            w.rect.top,
            w.rect.right - w.rect.left,
            w.rect.bottom - w.rect.top,
            w.pid
        );
    }

    let fg = unsafe { GetForegroundWindow() };
    let fg_title = title_of(fg);
    let fg_idx = wins.iter().position(|w| w.hwnd == fg);
    eprintln!(
        "前台窗口：\"{}\" → 在列表中的位置 {:?}（若为 Some(小数字) 则支持\"z 序从上到下\"）",
        fg_title, fg_idx
    );

    // ---- 地面真值交叉验证 ----
    let mut checked = 0usize;
    let mut skipped = 0usize;
    let mut mismatch: Vec<String> = Vec::new();

    for w in wins.iter().take(8) {
        let p = POINT {
            x: (w.rect.left + w.rect.right) / 2,
            y: (w.rect.top + w.rect.bottom) / 2,
        };
        let hit = unsafe { WindowFromPoint(p) };
        if hit.is_invalid() {
            skipped += 1;
            continue;
        }
        let root = unsafe { GetAncestor(hit, GA_ROOT) };
        if root.is_invalid() {
            skipped += 1;
            continue;
        }
        let mut rpid = 0u32;
        unsafe {
            GetWindowThreadProcessId(root, Some(&mut rpid));
        }
        if rpid == unsafe { GetCurrentProcessId() } {
            skipped += 1;
            continue;
        }
        let rtitle = title_of(root);
        if rtitle.trim().is_empty() {
            eprintln!(
                "  点 ({},{}) 的窗口无标题（产品代码本就会过滤掉）→ 跳过",
                p.x, p.y
            );
            skipped += 1;
            continue;
        }
        // 真值窗口必须在候选列表里；不在说明它被尺寸/标题规则过滤掉了 → 不是本测试的范围
        let Some(truth) = wins.iter().find(|x| x.hwnd == root) else {
            eprintln!(
                "  点 ({},{}) 的真值窗口 \"{}\" 不在候选列表（被尺寸规则过滤）→ 跳过",
                p.x, p.y, rtitle
            );
            skipped += 1;
            continue;
        };

        // 这就是修复后前端的检查点：取列表里第一个包含该点的窗口
        let first = wins.iter().find(|x| contains(&x.rect, p));
        checked += 1;
        match first {
            Some(f) if f.hwnd == truth.hwnd => {}
            Some(f) => mismatch.push(format!(
                "点 ({},{})：第一个命中 = \"{}\"，但 WindowFromPoint = \"{}\"",
                p.x, p.y, f.title, truth.title
            )),
            None => mismatch.push(format!(
                "点 ({},{})：列表里没有命中项，但 WindowFromPoint = \"{}\"",
                p.x, p.y, truth.title
            )),
        }
    }

    eprintln!(
        "交叉验证：检查 {} 个点、跳过 {} 个，不一致 {} 个",
        checked,
        skipped,
        mismatch.len()
    );
    for m in &mismatch {
        eprintln!("  ✗ {}", m);
    }
    if checked == 0 {
        eprintln!("跳过：本次没有可交叉验证的点（候选窗口可能全被遮挡）");
        return;
    }
    assert!(
        mismatch.is_empty(),
        "EnumWindows 顺序与 WindowFromPoint 不一致 → \"取第一个命中\"这条前提不成立，\
         应改用其它命中方案：\n{}",
        mismatch.join("\n")
    );
}
