/**
 * 全屏框选覆盖层。
 *
 * 设计要点：只把归一化坐标（0..1）交给后端，物理像素换算全在后端做。
 * 这样前端不需要知道 DPI 感知/缩放系数，也就不可能再踩"逻辑 vs 物理坐标"的坑。
 *
 * 交互：
 * 拖动 → 画选区；松开 → 显示尺寸并等待确认
 * Enter / 双击 → 确认；ESC / 右键 → 取消
 */
import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";

interface Rect {
  x: number;
  y: number;
  w: number;
  h: number;
}

/** 可吸附的顶层窗口（后端已归一化，前端不做任何 DPI 换算）。 */
interface Win {
  title: string;
  nx: number;
  ny: number;
  nw: number;
  nh: number;
}

export default function RegionOverlay() {
  const [rect, setRect] = useState<Rect | null>(null);
  const [dragging, setDragging] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [wins, setWins] = useState<Win[]>([]);
  const [hover, setHover] = useState<Win | null>(null);
  const origin = useRef<{ x: number; y: number } | null>(null);
  const area = useRef<HTMLDivElement | null>(null);

  // 打开时拉一次窗口列表，之后命中测试在本地做（避免每次移动都走 IPC）
  useEffect(() => {
    void invoke<Win[]>("list_windows")
      .then(setWins)
      .catch(() => {
        /* 拿不到就不支持吸附，纯拖拽仍然可用 */
      });
  }, []);

  // 命中测试：取列表里第一个包含光标的窗口。
  //
  // 后端用 EnumWindows 枚举，而 EnumWindows 是按 z 序（从上到下）遍历的——
  // 所以"第一个命中"就是"最靠上的那一个"。这一点 MSDN 没有文字承诺，
  // 已由 `crates/screenlite-media/tests/window_zorder.rs` 用 WindowFromPoint
  // 做地面真值验证验证（8 个采样点全部一致）。
  //
  // 此前按"面积最小的包含窗口"来近似是错的：当最靠上的窗口比它后面的窗口更大时
  // （例如最大化窗口上压着一个小窗口），面积最小法则会选中后面那个小的。
  // 实测报过两条：同位置有浮层时选错、选中范围明显不对。
  const hitTest = useCallback(
    (p: { x: number; y: number }): Win | null => {
      for (const w of wins) {
        if (p.x >= w.nx && p.x <= w.nx + w.nw && p.y >= w.ny && p.y <= w.ny + w.nh) {
          return w;
        }
      }
      return null;
    },
    [wins]
  );

  // 相对覆盖层窗口的归一化坐标（0..1）
  const normalize = useCallback((clientX: number, clientY: number) => {
    const w = window.innerWidth || 1;
    const h = window.innerHeight || 1;
    return {
      x: Math.min(Math.max(clientX / w, 0), 1),
      y: Math.min(Math.max(clientY / h, 0), 1),
    };
  }, []);

  const onMouseDown = (e: React.MouseEvent) => {
    if (e.button !== 0) return;
    const p = normalize(e.clientX, e.clientY);
    origin.current = p;
    setRect({ x: p.x, y: p.y, w: 0, h: 0 });
    setDragging(true);
    setError(null);
  };

  const onMouseMove = (e: React.MouseEvent) => {
    const p = normalize(e.clientX, e.clientY);
    if (!dragging || !origin.current) {
      // 未拖拽时做窗口吸附高亮
      setHover(hitTest(p));
      return;
    }
    setRect({
      x: Math.min(origin.current.x, p.x),
      y: Math.min(origin.current.y, p.y),
      w: Math.abs(p.x - origin.current.x),
      h: Math.abs(p.y - origin.current.y),
    });
  };

  const onMouseUp = (e: React.MouseEvent) => {
    const wasDragging = dragging;
    setDragging(false);
    const p = normalize(e.clientX, e.clientY);

    if (wasDragging && rect && rect.w >= 0.005 && rect.h >= 0.005) {
      return; // 真正的拖拽选择：保留选框，等 Enter 确认
    }

    // 视为单击：若命中了窗口，直接选中该窗口并确认（这就是"点一下选中"）
    const w = hitTest(p);
    if (w) {
      setRect({ x: w.nx, y: w.ny, w: w.nw, h: w.nh });
      void invoke("confirm_region", {
        nx: w.nx,
        ny: w.ny,
        nw: w.nx + w.nw,
        nh: w.ny + w.nh,
      }).catch((err) => setError(String(err)));
    } else {
      setRect(null); // 点在空白处 → 清空
    }
  };

  const confirm = useCallback(async () => {
    if (!rect) return;
    try {
      // 后端换算成物理像素，并把结果以 region-selected 事件推给主窗口
      await invoke("confirm_region", {
        nx: rect.x,
        ny: rect.y,
        nw: rect.x + rect.w,
        nh: rect.y + rect.h,
      });
    } catch (e) {
      setError(String(e));
    }
  }, [rect]);

  const cancel = useCallback(() => {
    void invoke("cancel_region").catch(() => {});
  }, []);

  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      if (e.key === "Escape") cancel();
      if (e.key === "Enter") void confirm();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [cancel, confirm]);

  // 提示：覆盖层是全屏透明的，未选区时只显示一行操作提示
  return (
    <div
      ref={area}
      className="overlay"
      onMouseDown={onMouseDown}
      onMouseMove={onMouseMove}
      onMouseUp={onMouseUp}
      onContextMenu={(e) => {
        e.preventDefault();
        cancel();
      }}
      onDoubleClick={() => void confirm()}
    >
      {rect && rect.w > 0 && (
        <>
          <div className="overlay-dim" style={{ left: 0, top: 0, right: 0, height: `${rect.y * 100}%` }} />
          <div
            className="overlay-dim"
            style={{ left: 0, top: `${(rect.y + rect.h) * 100}%`, right: 0, bottom: 0 }}
          />
          <div
            className="overlay-dim"
            style={{
              left: 0,
              top: `${rect.y * 100}%`,
              width: `${rect.x * 100}%`,
              height: `${rect.h * 100}%`,
            }}
          />
          <div
            className="overlay-dim"
            style={{
              left: `${(rect.x + rect.w) * 100}%`,
              top: `${rect.y * 100}%`,
              right: 0,
              height: `${rect.h * 100}%`,
            }}
          />
          <div
            className="overlay-rect"
            style={{
              left: `${rect.x * 100}%`,
              top: `${rect.y * 100}%`,
              width: `${rect.w * 100}%`,
              height: `${rect.h * 100}%`,
            }}
          >
            <span className="overlay-size">
              {Math.round(rect.w * 100)}% × {Math.round(rect.h * 100)}%
            </span>
          </div>
        </>
      )}

      {hover && !dragging && !rect && (
        <div
          className="overlay-hover"
          style={{
            left: `${hover.nx * 100}%`,
            top: `${hover.ny * 100}%`,
            width: `${hover.nw * 100}%`,
            height: `${hover.nh * 100}%`,
          }}
        >
          <span className="overlay-title">{hover.title}</span>
        </div>
      )}

      <div className="overlay-hint">
        {error ? (
          <span className="overlay-error">⚠️ {error}</span>
        ) : (
          <>
            点一下<b>选中整个窗口</b>，或按住左键拖动自由框选 · <b>Enter</b> 确认 · <b>ESC</b> 取消
          </>
        )}
      </div>
    </div>
  );
}
