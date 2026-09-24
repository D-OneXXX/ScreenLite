import React from "react";
import ReactDOM from "react-dom/client";
import App from "./App";
import RegionOverlay from "./RegionOverlay";
import "./styles.css";

// 同一个前端包承担两个角色：
// 普通窗口 → 录制控制界面
// ?overlay=1 → 全屏框选覆盖层（由 open_region_selector 打开）
const isOverlay = new URLSearchParams(window.location.search).get("overlay") === "1";

if (isOverlay) {
  // 覆盖层必须是真透明的，否则用户看不到桌面内容、无法框选。
  // 光把 Tauri 窗口设成 transparent 不够：页面自身的背景也必须透明，
  // 否则不透明的 body 背景会把整个窗口盖成一片黑（实测就是这个现象）。
  document.documentElement.classList.add("overlay-mode");
}

ReactDOM.createRoot(document.getElementById("root") as HTMLElement).render(
  <React.StrictMode>{isOverlay ? <RegionOverlay /> : <App />}</React.StrictMode>
);
