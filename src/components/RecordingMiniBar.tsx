/**
 * RecordingMiniBar —— 录制态迷你条。
 *
 * 结构：
 * ┌────────────────────────────────┐
 * │ 00:01:28 3 ■ │
 * └────────────────────────────────┘
 *
 * 读数全部来自真实 recording-progress 事件（elapsed_ms / frames_dropped）；
 * 停止按钮触发现有 stop_recording 链路。点击主体展开/收起遥测面板。
 */
import { formatDuration } from "../utils";

export interface RecordingMiniBarProps {
  elapsedMs: number;
  framesDropped: number;
  hudOpen: boolean;
  busy: boolean;
  onToggleHud: () => void;
  onStop: () => void;
}

export default function RecordingMiniBar({
  elapsedMs,
  framesDropped,
  hudOpen,
  busy,
  onToggleHud,
  onStop,
}: RecordingMiniBarProps) {
  return (
    <div className="minibar" role="group" aria-label="录制中">
      <button
        className="minibar-main"
        onClick={onToggleHud}
        title="点击展开/收起监测面板"
        aria-expanded={hudOpen}
      >
        <span className="rec-dot" />
        <span className="rec-time">{formatDuration(elapsedMs)}</span>
        {framesDropped > 0 && (
          <span className="rec-warn" title={`已丢 ${framesDropped} 帧（时间轴仍准确）`}>
            ⚠ {framesDropped}
          </span>
        )}
      </button>
      <button className="rec-stop" title="停止录制" onClick={onStop} disabled={busy}>
        ■
      </button>
    </div>
  );
}
