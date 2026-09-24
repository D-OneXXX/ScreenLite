/**
 * Toast —— 停止录制后的成功产物提示（6 秒自动消失，由 App 的计时器控制）。
 *
 * 关于停止原因：它显示在遥测面板里（`HudPanel` 的"上次录制：… · 时长 · 帧数"），
 * 本组件只给文件名 + 打开目录 —— 与设计契约的胶囊形态一致。
 * 注意：`STOP_REASON_LABEL` 因此不是死代码，`HudPanel` 在用。
 */
import { invoke } from "@tauri-apps/api/core";
import { dirOf } from "../utils";

export interface ToastProps {
  /** 输出文件完整路径 */
  path: string;
  onError: (message: string) => void;
}

export default function Toast({ path, onError }: ToastProps) {
  return (
    <div className="dock-toast">
      <span className="toast-check">✓</span>
      <span className="toast-text" title={path}>
        {path.split(/[\\/]/).pop()}
      </span>
      <button
        className="toast-btn"
        onClick={() => {
          void invoke("open_output_directory", { path: dirOf(path) }).catch((e) =>
            onError(String(e)),
          );
        }}
      >
        打开目录
      </button>
    </div>
  );
}
