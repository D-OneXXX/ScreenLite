/**
 * Island 3 —— 遥测岛（TelemetryIsland）+ 遥测面板（HudPanel）。
 *
 * 结构：
 * ┌────────────────────────────────────┐
 * │ 🎬 打开监测面板 ?│
 * └────────────────────────────────────┘
 *
 * 岛身带轻微洋红边缘光（很轻——"这个按钮比较特别"）。
 * 点击展开 HudPanel：全部读数来自真实 telemetry（recording-progress 4Hz 事件
 * + get_status 轮询的 actual 编码参数）——无假数据、无占位指标。
 */
import { STOP_REASON_LABEL, type FinishedPayload, type ProgressPayload, type RecordingStatus } from "../types";
import { formatDuration } from "../utils";

export interface TelemetryIslandProps {
  hudOpen: boolean;
  onToggle: () => void;
}

export default function TelemetryIsland({ hudOpen, onToggle }: TelemetryIslandProps) {
  return (
    <section className="island island-glow" data-tauri-drag-region>
      <button className="action-row" onClick={onToggle}>
        <span className="action-icon">🎬</span>
        <span className="action-label">{hudOpen ? "收起监测面板" : "打开监测面板"}</span>
        <span className="help-dot" title="录制状态 / 丢帧 / 每源电平等真实指标">?</span>
      </button>
    </section>
  );
}

/* ============================== 遥测面板（监测模式）============================== */

/**
 * 监测面板（监控模式 / 录制中都可展开）。
 *
 * 它始终是流内岛（不是 absolute 覆盖层）：排在最后一个岛下方，
 * 由 App 的 mode 驱动尺寸联动让主窗口向下生长，从而得到"向下展开"的观感。
 * 不要再引入 absolute 整窗覆盖层——那正是"点一下整块跳出来"的根源。
 * 也不要因为它去 `setSize`（那是 tao 重写 GWL_STYLE 的老触发器；
 * 无边框由 `install_frameless_subclass` 保证，与 resize 解耦）。
 */
export function HudPanel({
  progress,
  finished,
  actual,
}: {
  progress: ProgressPayload | null;
  finished: FinishedPayload | null;
  actual: RecordingStatus["actual"];
}) {
  return (
    <section className="island hud" data-tauri-drag-region>
      <div className="hud-timer">
        <span className="hud-caption">录制时长</span>
        <span className="hud-time">{formatDuration(progress?.elapsed_ms ?? 0)}</span>
      </div>
      <div className="hud-grid">
        <HudMetric label="采集" value={progress?.frames_captured} />
        <HudMetric label="已编码" value={progress?.frames_encoded} />
        <HudMetric label="重复帧" value={progress?.frames_duplicated} />
        <HudMetric
          label="丢帧"
          value={progress?.frames_dropped}
          warn={(progress?.frames_dropped ?? 0) > 0}
        />
      </div>
      {progress?.audio_sources && progress.audio_sources.length > 0 && (
        <div className="hud-grid hud-audio">
          {progress.audio_sources.map((s) => (
            <HudMetric
              key={s.kind}
              label={`${s.kind === "microphone" ? "麦克风" : "系统声音"} 峰值/秒`}
              value={s.peak_last_sec}
              warn={s.real_blocks === 0}
            />
          ))}
        </div>
      )}
      {actual && (
        <p className="hud-line muted">
          实际参数：{actual.width}×{actual.height} @{actual.fps}fps · {(actual.bitrate / 1_000_000).toFixed(1)} Mbps ·{" "}
          {actual.profile} profile · 硬件={String(actual.hardware_mft_found)}
        </p>
      )}
      {!progress && finished && (
        <p className="hud-line muted">
          上次录制：{STOP_REASON_LABEL[finished.stop_reason] ?? finished.stop_reason} ·{" "}
          {formatDuration(finished.duration_ms)} · 编码 {finished.frames_encoded} 帧 · 丢帧 {finished.frames_dropped}
        </p>
      )}
    </section>
  );
}

function HudMetric({ label, value, warn }: { label: string; value?: number; warn?: boolean }) {
  return (
    <div className={`hud-metric${warn ? " hud-warn" : ""}`}>
      <span className="hud-metric-label">{label}</span>
      <span className="hud-metric-value">{value ?? "—"}</span>
    </div>
  );
}
