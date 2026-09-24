/**
 * Island 1 —— 尺寸与设备控制岛（ControlIsland）。
 *
 * 结构：
 * ┌────────────────────────────────────┐
 * │ ⚙ 1280 × 720 ↗ ⌖ │ ← TopControls
 * ├────────────────────────────────────┤
 * │ 🎤 🔊 30 FPS 🖱 ⌘│ ← SourceControls
 * └────────────────────────────────────┘
 *
 * 所有读数都来自真实状态（尺寸=region state、FPS=fps state、
 * 音频=AppSettings 同步来的 audioSel/audioGain、快捷键=hotkey 设置）——
 * 本组件只做展示与回调，不持有任何状态。
 */

export interface ControlIslandProps {
  fullScreen: boolean;
  region: { x: number; y: number; width: number; height: number };
  /** 显示器是否已就绪（未就绪时框选不可用） */
  displayReady: boolean;
  audioSel: { system: boolean; microphone: boolean };
  audioGain: { system: number; microphone: number };
  fps: number;
  hotkey: string;
  hotkeyOn: boolean;
  /** 齿轮按钮的 ref */
  gearRef?: React.RefObject<HTMLButtonElement | null>;
  onToggleFullScreen: () => void;
  onOpenSettings: () => void;
  onOpenRegionSelector: () => void;
  onToggleAudio: (kind: "system" | "microphone") => void;
  onCycleFps: () => void;
}

export default function ControlIsland({
  fullScreen,
  region,
  displayReady,
  audioSel,
  audioGain,
  fps,
  hotkey,
  hotkeyOn,
  gearRef,
  onToggleFullScreen,
  onOpenSettings,
  onOpenRegionSelector,
  onToggleAudio,
  onCycleFps,
}: ControlIslandProps) {
  return (
    <section className="island" data-tauri-drag-region>
      {/* ---------- TopControls：设置 / 尺寸胶囊 / 框选 ---------- */}
      <div className="island-row" data-tauri-drag-region>
        <button
          ref={gearRef}
          className="icon-btn"
          title="高级设置（显示器 / 输出目录 / 增益 / 快捷键）"
          onClick={onOpenSettings}
        >
          ⚙
        </button>
        <button
          className="size-pill"
          title={`当前选区：x=${region.x} y=${region.y} w=${region.width} h=${region.height}（点击切换整屏/选区）`}
          onClick={onToggleFullScreen}
        >
          {fullScreen ? "整屏" : `${region.width} × ${region.height}`}
          <span className="size-arrow">↗</span>
        </button>
        <button
          className="icon-btn"
          title="框选区域（唤出全屏覆盖层）"
          disabled={!displayReady}
          onClick={onOpenRegionSelector}
        >
          ⌖
        </button>
      </div>
      <div className="island-divider" />
      {/* ---------- SourceControls：麦克风 / 系统声音 / FPS / 占位 / 快捷键 ---------- */}
      <div className="island-icons">
        <button
          className={`toggle-icon${audioSel.microphone ? " on" : ""}`}
          title={`麦克风（默认输入设备）· 增益 ${audioGain.microphone.toFixed(1)}×`}
          onClick={() => onToggleAudio("microphone")}
        >
          🎤
          {audioSel.microphone && <span className="toggle-check">✓</span>}
        </button>
        <button
          className={`toggle-icon${audioSel.system ? " on" : ""}`}
          title={`系统声音（扬声器回环）· 增益 ${audioGain.system.toFixed(1)}×`}
          onClick={() => onToggleAudio("system")}
        >
          🔊
          {audioSel.system && <span className="toggle-check">✓</span>}
        </button>
        <button
          className="toggle-icon fps-icon"
          title="帧率（30 / 60 / 90 循环切换）"
          onClick={onCycleFps}
        >
          <span className="fps-badge">{fps}</span>
          <span className="fps-unit">fps</span>
        </button>
        {/* 预留位：光标高亮没有真实 backend */}
        <button className="toggle-icon" disabled title="光标高亮（预留：当前版本未启用）">
          🖱
        </button>
        <button
          className={`toggle-icon${hotkeyOn ? " on" : ""}`}
          title={hotkeyOn ? `全局快捷键：${hotkey}` : "全局快捷键已停用"}
          onClick={onOpenSettings}
        >
          ⌘
        </button>
      </div>
    </section>
  );
}
