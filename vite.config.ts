import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";

// Tauri 会把前端构建产物嵌进二进制，因此这里固定输出目录与端口。
export default defineConfig({
  plugins: [react()],
  clearScreen: false,
  server: {
    // 必须钉死 127.0.0.1：不设置时 vite 跟随系统 localhost 解析，
    // 本机 localhost 优先 IPv6 ⇒ vite 只绑 [::1]:1420，而 WebView2 走 127.0.0.1
    // ⇒ 页面永远加载不到（现象：看门狗报"启动 10 秒仍未收到前端就绪"）。
    host: "127.0.0.1",
    port: 1420,
    strictPort: true,
    watch: {
      // 必须忽略 Rust 构建产物目录。
      // `tauri dev` 会在 Vite 运行期间用 cargo 写 target/，
      // 若把它纳入监听，watcher 会撞上 EBUSY("resource busy or locked, watch ...target\debug\deps\screenlite.exe")
      // 并直接崩溃，表现为 "beforeDevCommand terminated with a non-zero status code"。
      ignored: ["**/target/**"],
    },
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    target: "chrome110",
  },
});
