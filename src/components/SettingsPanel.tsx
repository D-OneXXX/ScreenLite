/**
 * SettingsPanel —— 设置形态。
 *
 * 以「形态切换」替代 Popover：窗口高度随形态联动。
 * 所有值都来自真实 AppSettings（显示器列表 / 输出目录 / 每源增益 / 全局快捷键 /
 * 允许被截屏 / 选区坐标），改动经 App 的防抖回写统一持久化——本组件不另建 state。
 */
import type { CSSProperties } from "react";
import type { DisplayInfo } from "../types";

export interface SettingsPanelProps {
  displays: DisplayInfo[];
  displayId: string;
  setDisplayId: (v: string) => void;
  outputDir: string;
  setOutputDir: (v: string) => void;
  audioGain: { system: number; microphone: number };
  setAudioGain: (v: { system: number; microphone: number }) => void;
  hotkey: string;
  setHotkey: (v: string) => void;
  hotkeyOn: boolean;
  setHotkeyOn: (v: boolean) => void;
  captureVisible: boolean;
  setCaptureVisible: (v: boolean) => void;
  settingsMsg: string | null;
  region: { x: number; y: number; width: number; height: number };
  setRegion: (v: { x: number; y: number; width: number; height: number }) => void;
  fullScreen: boolean;
  setFullScreen: (v: boolean) => void;
  onPickDir: () => void;
  onBack: () => void;
}

export default function SettingsPanel({
  displays,
  displayId,
  setDisplayId,
  outputDir,
  setOutputDir,
  audioGain,
  setAudioGain,
  hotkey,
  setHotkey,
  hotkeyOn,
  setHotkeyOn,
  captureVisible,
  setCaptureVisible,
  settingsMsg,
  region,
  setRegion,
  fullScreen,
  setFullScreen,
  onPickDir,
  onBack,
}: SettingsPanelProps) {
  return (
    <div className="settings-body">
      <div className="settings-head">
        <button className="icon-btn" title="返回" onClick={onBack}>
          ←
        </button>
        <span className="settings-title">设置</span>
      </div>

      <div className="settings-row">
        <span className="settings-label">显示器</span>
        <select value={displayId} onChange={(e) => setDisplayId(e.target.value)}>
          {displays.length === 0 && <option value="">（未检测到显示器）</option>}
          {displays.map((d) => (
            <option key={d.id} value={d.id}>
              {d.device_name}
              {d.friendly_name ? ` · ${d.friendly_name}` : ""}
              {d.primary ? " · 主" : ""}
            </option>
          ))}
        </select>
      </div>

      <div className="settings-row">
        <span className="settings-label">输出目录</span>
        <div className="settings-flex">
          <input
            className="path-input"
            value={outputDir}
            placeholder="留空则使用默认目录"
            onChange={(e) => setOutputDir(e.target.value)}
          />
          <button className="icon-btn" title="选择目录" onClick={onPickDir}>
            …
          </button>
        </div>
      </div>

      <div className="settings-row">
        <span className="settings-label">系统声音增益</span>
        <div className="settings-flex">
          <span className="gain-value">{audioGain.system.toFixed(1)}×</span>
          <input
            type="range"
            min={0}
            max={2}
            step={0.1}
            value={audioGain.system}
            style={{ "--sl-fill": `${(audioGain.system / 2) * 100}%` } as CSSProperties}
            onChange={(e) => setAudioGain({ ...audioGain, system: Number(e.target.value) })}
          />
        </div>
      </div>

      <div className="settings-row">
        <span className="settings-label">麦克风增益</span>
        <div className="settings-flex">
          <span className="gain-value">{audioGain.microphone.toFixed(1)}×</span>
          <input
            type="range"
            min={0}
            max={2}
            step={0.1}
            value={audioGain.microphone}
            style={{ "--sl-fill": `${(audioGain.microphone / 2) * 100}%` } as CSSProperties}
            onChange={(e) => setAudioGain({ ...audioGain, microphone: Number(e.target.value) })}
          />
        </div>
      </div>

      <div className="settings-row">
        <span className="settings-label">全局快捷键</span>
        <div className="settings-flex">
          <input
            className="path-input"
            value={hotkey}
            disabled={!hotkeyOn}
            placeholder="例如 Ctrl+Alt+R"
            onChange={(e) => setHotkey(e.target.value)}
          />
          <button
            className={`toggle-icon small${hotkeyOn ? " on" : ""}`}
            title={hotkeyOn ? "已启用" : "已停用"}
            onClick={() => setHotkeyOn(!hotkeyOn)}
          >
            ⌘
          </button>
        </div>
      </div>

      <div className="settings-row">
        <span className="settings-label">允许被截屏</span>
        <div className="settings-flex">
          <button
            className={`toggle-icon small${captureVisible ? " on" : ""}`}
            title={
              captureVisible
                ? "已允许：窗口会出现在截图与录像里（需要给别人看界面时打开）"
                : "已排除：窗口对截图/录像不可见（默认；想截图自己的界面就打开它）"
            }
            onClick={() => setCaptureVisible(!captureVisible)}
          >
            {captureVisible ? "✓" : "✕"}
          </button>
          <span className="settings-hint">
            {captureVisible ? "会出现在截图/录像里" : "录像里不会出现（默认）"}
          </span>
        </div>
      </div>

      <div className="settings-row">
        <span className="settings-label">选区坐标（物理像素）</span>
        <div className="settings-flex">
          {(["x", "y", "width", "height"] as const).map((k) => (
            <input
              key={k}
              type="number"
              className="coord-input"
              value={region[k]}
              disabled={fullScreen}
              title={k === "x" || k === "y" ? `左上角 ${k.toUpperCase()}` : k === "width" ? "宽" : "高"}
              onChange={(e) => setRegion({ ...region, [k]: Number(e.target.value) })}
            />
          ))}
          <button
            className={`toggle-icon small${fullScreen ? " on" : ""}`}
            title={fullScreen ? "整屏" : "自定义选区"}
            onClick={() => setFullScreen(!fullScreen)}
          >
            ⤢
          </button>
        </div>
      </div>

      {settingsMsg && <p className="settings-msg muted">{settingsMsg}</p>}
    </div>
  );
}
