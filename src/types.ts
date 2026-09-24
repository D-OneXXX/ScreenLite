/** 与 Rust 侧一一对应的类型（字段名/单位必须一致，避免前后端约定漂移）。 */

export type EngineState = "Idle" | "Preparing" | "Recording" | "Stopping" | "Error";

/**
 * 音频来源（V0.2 二选一，不做混合）。
 *
 * 将来接"两者混合"时：只需在这里加第三个取值 + 后端往混音点里多 push 一个源，
 * UI 与 API 形状都不用改。
 */
export type AudioSourceKind = "system" | "microphone";

export interface DisplayInfo {
  id: string;
  device_name: string;
  friendly_name: string;
  /** UI 展示用尺寸（可能是逻辑像素，缩放环境下与物理分辨率不同） */
  width: number;
  height: number;
  x: number;
  y: number;
  primary: boolean;
  hdr: boolean;
}

export interface ConfigureOutcome {
  width: number;
  height: number;
  fps: number;
  bitrate: number;
  profile: string;
  hardware_requested: boolean;
  hardware_mft_found: boolean;
  encoder_params_applied: boolean;
  notes: string[];
}

export interface RecordingStatus {
  state: EngineState;
  elapsed_ms: number;
  frames_captured: number;
  frames_overwritten: number;
  frames_scheduled: number;
  frames_duplicated: number;
  frames_dropped_backpressure: number;
  frames_encoded: number;
  /** 音频峰值（全程 |sample| 最大值）：0 = 进设备的信号本身就是数字静音 */
  audio_peak: number;
  /** 当前这一秒的峰值：实时电平条的现成数据源 */
  audio_peak_1s: number;
  /** 刚过去那一秒的峰值：历史 > 0 而它为 0 → 中途断了/设备被占用/被静音 */
  audio_peak_last_sec: number;
  /** 每源音频指标：real_blocks = chunks − filler_blocks 即该路真实设备数据 */
  audio_sources?: {
    kind: string;
    endpoint: string;
    chunks: number;
    filler_blocks: number;
    real_blocks: number;
    peak: number;
    peak_last_sec: number;
    dropped_backwards: number;
  }[];
  /**
   * 我们合成的静音块数（补位 + 收尾补位）。
   * `audio_chunks - 它` = 真实设备块数 —— "真实音频在流"的直接检查点
   * （`peak > 0` 只是代理：正在播放但内容恰好是数字静音的流会让代理判否）。
   */
  audio_filler_blocks: number;
  timestamp_anomalies: number;
  slots_late: number;
  error_code: string | null;
  error_message: string | null;
  output_path: string | null;
  actual: ConfigureOutcome | null;
  degraded: boolean;
}

export interface ProgressPayload {
  elapsed_ms: number;
  frames_captured: number;
  frames_scheduled: number;
  frames_duplicated: number;
  frames_dropped: number;
  frames_encoded: number;
  queue_degraded: boolean;
  /** 每源音频指标：real_blocks = chunks − filler_blocks 为该路真实设备数据 */
  audio_sources?: {
    kind: string;
    endpoint: string;
    chunks: number;
    filler_blocks: number;
    real_blocks: number;
    peak: number;
    peak_last_sec: number;
    dropped_backwards: number;
  }[];
}

export interface StatePayload {
  state: EngineState;
  error_code: string | null;
  error_message: string | null;
}

export interface FinishedPayload {
  output_path: string;
  duration_ms: number;
  frames_encoded: number;
  frames_dropped: number;
  stop_reason: string;
}

export interface StartRequest {
  display_id: string;
  fps: number;
  output_dir: string;
  /** 区域录制（物理像素，原点在显示器左上角）；省略 = 全屏 */
  region?: Region;
  prefer_software?: boolean;
}

/**
 * 录制区域。坐标是采集画幅的物理像素。
 * 缩放环境下物理分辨率比桌面显示尺寸大（本机 150%：桌面 1707×1067 ↔ 物理 2560×1600）。
 */
export interface Region {
  x: number;
  y: number;
  width: number;
  height: number;
}

/** 后端白名单：UI 只应提供这三档。 */
export const ALLOWED_FPS = [30, 60, 90] as const;

export const STOP_REASON_LABEL: Record<string, string> = {
  UserRequested: "手动停止",
  DurationLimit: "达到 2 小时上限，已自动停止",
  DiskSpaceLow: "磁盘空间不足，已提前停止（文件已保存）",
};

// ---------- 用户设置 ----------

/** 单个音频源的偏好（对应 Rust 侧 `settings::AudioSourcePref`） */
export interface AudioSourcePref {
  kind: AudioSourceKind;
  /** 线性增益 0.0–2.0 */
  gain: number;
}

/** 用户设置（对应 Rust 侧 `settings::AppSettings`） */
export interface AppSettings {
  audio_sources: AudioSourcePref[];
  /** 全局快捷键；`null` = 停用 */
  hotkey: string | null;
  /**
   * 允许控制窗口出现在截图/录像里（默认 `false` = 排除）。
   * 默认排除是产品该有的行为；打开它才能在截图/录屏里看到自己的界面。
   */
  capture_visible: boolean;
}

/** `set_settings` 的结果：保存与「快捷键是否生效」分开报告 */
export interface SettingsApplied {
  saved: boolean;
  hotkey_error: string | null;
}
