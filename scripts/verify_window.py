# ScreenLite 主窗口运行时状态只读回读（Phase 1 证据链最后一环）。
# 纯只读：EnumWindows + GetWindowLongW + GetWindowRect + DwmGetWindowAttribute。
# 不碰输入设备、不改任何窗口状态。
import ctypes
from ctypes import wintypes

user32 = ctypes.windll.user32
dwmapi = ctypes.windll.dwmapi

GWL_STYLE = -16
GWL_EXSTYLE = -20
WS_CAPTION = 0x00C00000
WS_THICKFRAME = 0x00040000
WS_POPUP = 0x80000000
WS_EX_TOPMOST = 0x00000008
DWMWA_NCRENDERING_POLICY = 1
DWMWA_WINDOW_CORNER_PREFERENCE = 33
DWMWA_BORDER_COLOR = 34


class RECT(ctypes.Structure):
    _fields_ = [("left", wintypes.LONG), ("top", wintypes.LONG),
                ("right", wintypes.LONG), ("bottom", wintypes.LONG)]


def find_screenlite_windows():
    results = []
    proc_id = wintypes.DWORD()

    @ctypes.WINFUNCTYPE(wintypes.BOOL, wintypes.HWND, wintypes.LPARAM)
    def cb(hwnd, _lparam):
        user32.GetWindowThreadProcessId(hwnd, ctypes.byref(proc_id))
        # 找标题为 ScreenLite 的可见顶层窗口（主窗；覆盖层标题是"选择录制区域"）
        if user32.IsWindowVisible(hwnd):
            length = user32.GetWindowTextLengthW(hwnd)
            if length > 0:
                buf = ctypes.create_unicode_buffer(length + 1)
                user32.GetWindowTextW(hwnd, buf, length + 1)
                if buf.value == "ScreenLite":
                    results.append((hwnd, proc_id.value))
        return True

    user32.EnumWindows(cb, 0)
    return results


def main():
    wins = find_screenlite_windows()
    if not wins:
        print("未找到 ScreenLite 窗口（应用未运行？）")
        return
    for hwnd, pid in wins:
        style = user32.GetWindowLongW(hwnd, GWL_STYLE) & 0xFFFFFFFF
        exstyle = user32.GetWindowLongW(hwnd, GWL_EXSTYLE) & 0xFFFFFFFF
        rect = RECT()
        user32.GetWindowRect(hwnd, ctypes.byref(rect))
        w, h = rect.right - rect.left, rect.bottom - rect.top

        def dwm(attr):
            val = wintypes.UINT(0)
            hr = dwmapi.DwmGetWindowAttribute(
                wintypes.HWND(hwnd), wintypes.DWORD(attr),
                ctypes.byref(val), ctypes.sizeof(val))
            return val.value if hr == 0 else None

        ncr = dwm(DWMWA_NCRENDERING_POLICY)
        corner = dwm(DWMWA_WINDOW_CORNER_PREFERENCE)
        border = dwm(DWMWA_BORDER_COLOR)
        print(f"HWND={hwnd} PID={pid}")
        print(f"  STYLE   = 0x{style:08X}  WS_CAPTION={'在位!!' if style & WS_CAPTION else '已清 ✓'}"
              f"  WS_THICKFRAME={'在位!!' if style & WS_THICKFRAME else '已清 ✓'}"
              f"  WS_POPUP={'在位 ✓' if style & WS_POPUP else '缺席!!'}")
        print(f"  EXSTYLE = 0x{exstyle:08X}  WS_EX_TOPMOST={'在位 ✓' if exstyle & WS_EX_TOPMOST else '缺席!!'}")
        print(f"  RECT    = {w}x{h} @ ({rect.left},{rect.top})")
        print(f"  DWM: NCRENDERING_POLICY={ncr} (期望 2=DISABLED)  "
              f"CORNER_PREFERENCE={corner} (期望 1=DONOTROUND)  "
              f"BORDER_COLOR={'0x%08X' % border if border is not None else None} (期望 0xFFFFFFFE=NONE)")


if __name__ == "__main__":
    main()
