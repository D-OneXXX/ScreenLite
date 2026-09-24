/**
 * Island 2 —— 核心录制动作岛（RecordingIsland）。
 *
 * 结构：
 * ┌────────────────────────────────────┐
 * │ ▣ 录制选区 ⌥↵ │ ← RegionButton
 * ├────────────────────────────────────┤
 * │ 🎥 录制整屏 ↵ │ ← FullscreenButton
 * └────────────────────────────────────┘
 *
 * 两个动作直接触发现有 start_recording 链路（选区=当前 region，整屏=undefined），
 * 本组件只发回调，不持有状态。
 */

export interface RecordingIslandProps {
  /** busy || 无显示器 || 无音频源 → 两个动作都禁用 */
  disabled: boolean;
  onStartRegion: () => void;
  onStartFullscreen: () => void;
}

export default function RecordingIsland({
  disabled,
  onStartRegion,
  onStartFullscreen,
}: RecordingIslandProps) {
  return (
    <section className="island" data-tauri-drag-region>
      <button
        className="action-row"
        disabled={disabled}
        onClick={onStartRegion}
        title="以当前选区（或默认区域）启动录制"
      >
        <span className="action-icon">▣</span>
        <span className="action-label">录制选区</span>
        <kbd>⌥↵</kbd>
      </button>
      <div className="island-divider" />
      <button
        className="action-row"
        disabled={disabled}
        onClick={onStartFullscreen}
        title="录制当前显示器整屏"
      >
        <span className="action-icon">🎥</span>
        <span className="action-label">录制整屏</span>
        <kbd>↵</kbd>
      </button>
    </section>
  );
}
