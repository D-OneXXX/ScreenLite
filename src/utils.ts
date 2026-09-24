/** 共享小工具：时间格式化与路径处理（App 与各组件共用，避免重复定义）。 */

/** mm:ss 或 hh:mm:ss 格式化（始终三段 hh:mm:ss，与录制计时器一致）。 */
export function formatDuration(ms: number): string {
  const total = Math.max(0, Math.floor(ms / 1000));
  const h = Math.floor(total / 3600);
  const m = Math.floor((total % 3600) / 60);
  const s = total % 60;
  return `${h.toString().padStart(2, "0")}:${m.toString().padStart(2, "0")}:${s
    .toString()
    .padStart(2, "0")}`;
}

/** 从文件路径取出所在目录（供「打开输出目录」使用）。 */
export function dirOf(path?: string): string {
  if (!path) return "";
  return path.replace(/[\\/][^\\/]+$/, "");
}
