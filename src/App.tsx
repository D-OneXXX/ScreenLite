import { useCallback, useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { open } from "@tauri-apps/plugin-dialog";
import { getCurrentWindow } from "@tauri-apps/api/window";
import { LogicalSize } from "@tauri-apps/api/dpi";
import {
  type AppSettings,
  type AudioSourcePref,
  type DisplayInfo,
  type FinishedPayload,
  type ProgressPayload,
  type RecordingStatus,
  type SettingsApplied,
  type StatePayload,
} from "./types";
import { dirOf } from "./utils";
import ControlIsland from "./components/ControlIsland";
import RecordingIsland from "./components/RecordingIsland";
import TelemetryIsland, { HudPanel } from "./components/TelemetryIsland";
import RecordingMiniBar from "./components/RecordingMiniBar";
import SettingsPanel from "./components/SettingsPanel";
import Toast from "./components/Toast";

type DockPanel = null | "settings";

/**
 * ScreenLiteDock —— 悬浮 Dock 主装配。
 *
 * 职责划分：
 * - 本组件是唯一的状态中枢：显示器/录制状态/设置/命令队列/事件订阅/尺寸联动全在这里；
 * - 三个 Island + MiniBar + SettingsPanel + Toast 都是纯展示组件（props 进、回调出）；
 * - 后端架构零改动：IPC、状态机、RegionOverlay、全局快捷键全部沿用现有实现。
 */
export default function App() {
  const [displays, setDisplays] = useState<DisplayInfo[]>([]);
  const [displayId, setDisplayId] = useState<string>("");
  const [fps, setFps] = useState<number>(30);
  const [outputDir, setOutputDir] = useState<string>("");
  const [state, setState] = useState<string>("Idle");
  const [fullScreen, setFullScreen] = useState(true);
  // 音频来源：勾选 + 每源增益；两路可同时录（等权相加 + 硬限幅）
  const [audioSel, setAudioSel] = useState({ system: true, microphone: false });
  const [audioGain, setAudioGain] = useState({ system: 1.0, microphone: 1.0 });
  // 全局快捷键：可配置、可停用
  const [hotkey, setHotkey] = useState("Ctrl+Alt+R");
  const [hotkeyOn, setHotkeyOn] = useState(true);
  // 允许控制窗口出现在截图/录像里：默认关（录像里不该有控制界面）
  const [captureVisible, setCaptureVisible] = useState(false);
  const [settingsMsg, setSettingsMsg] = useState<string | null>(null);
  const settingsLoaded = useRef(false);
  const settingsTimer = useRef<number | null>(null);
  const [region, setRegion] = useState({ x: 0, y: 0, width: 1280, height: 720 });
  const [progress, setProgress] = useState<ProgressPayload | null>(null);
  const [finished, setFinished] = useState<FinishedPayload | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [actual, setActual] = useState<RecordingStatus["actual"]>(null);
  const [busy, setBusy] = useState(false);
  // 悬浮 Dock的视图状态：Settings/HUD 都是 DOM Popover 层——
  // 三岛恒渲染、主窗口恒尺寸，Popover 以 absolute 盖在其上，绝不触发 setSize。
  const [panel, setPanel] = useState<DockPanel>(null);
  // Settings Popover 的真实锚点
  const gearBtnRef = useRef<HTMLButtonElement | null>(null);
  const [settingsAnchor, setSettingsAnchor] = useState<{ top: number } | null>(null);
  // 遥测面板（监测模式；录制中也可从迷你条展开）
  const [hudOpen, setHudOpen] = useState(false);
  // 停止后的成功产物 Toast
  const [toast, setToast] = useState<string | null>(null);
  const toastTimer = useRef<number | null>(null);
  // 状态轮询兜底：事件节流是 4Hz，但状态跳变也要能及时反映
  const statusTimer = useRef<number | null>(null);
  // 托盘/快捷键的事件回调需要读到最新状态，闭包里的 state 会过期，所以用 ref
  const stateRef = useRef(state);
  stateRef.current = state;
  const dirRef = useRef(outputDir);
  dirRef.current = outputDir;
  const finishedRef = useRef<FinishedPayload | null>(null);
  finishedRef.current = finished;
  // 退出流程：托盘/关窗发起退出后置位；等最终化完成再向后端回声
  const quittingRef = useRef(false);
  // 命令处理函数每帧刷新，通过 ref 交给"只注册一次"的监听器与常驻轮询。
  const takeCommandRef = useRef<() => void>(() => {});

  const refreshDisplays = useCallback(async () => {
    try {
      const list = await invoke<DisplayInfo[]>("list_displays");
      setDisplays(list);
      const primary = list.find((d) => d.primary) ?? list[0];
      if (primary && !displayId) setDisplayId(primary.id);
    } catch (e) {
      setError(String(e));
    }
  }, [displayId]);

  useEffect(() => {
    void refreshDisplays();
  }, [refreshDisplays]);

  // ---------- 用户设置：启动读一次，之后改动防抖回写 ----------
  useEffect(() => {
    (async () => {
      try {
        const s = await invoke<AppSettings>("get_settings");
        const sel = { system: false, microphone: false };
        const gain = { system: 1.0, microphone: 1.0 };
        for (const a of s.audio_sources) {
          if (a.kind === "system" || a.kind === "microphone") {
            sel[a.kind] = true;
            gain[a.kind] = a.gain;
          }
        }
        // 兜底：至少留一路（空配置会被后端拒绝启动，不能把 UI 置于那种状态）
        if (!sel.system && !sel.microphone) sel.system = true;
        setAudioSel(sel);
        setAudioGain(gain);
        setHotkey(s.hotkey ?? "");
        setHotkeyOn(s.hotkey !== null);
        setCaptureVisible(s.capture_visible ?? false);
      } catch (e) {
        setError(String(e));
      } finally {
        settingsLoaded.current = true;
      }
    })();
  }, []);

  useEffect(() => {
    if (!settingsLoaded.current) return;
    if (settingsTimer.current !== null) window.clearTimeout(settingsTimer.current);
    settingsTimer.current = window.setTimeout(async () => {
      const audio_sources: AudioSourcePref[] = (["system", "microphone"] as const)
        .filter((k) => audioSel[k])
        .map((k) => ({ kind: k, gain: audioGain[k] }));
      try {
        const r = await invoke<SettingsApplied>("set_settings", {
          settings: {
            audio_sources,
            hotkey: hotkeyOn ? hotkey.trim() : null,
            capture_visible: captureVisible,
          },
        });
        setSettingsMsg(
          r.hotkey_error ? `已保存；但快捷键没生效：${r.hotkey_error}` : "设置已保存（下次启动生效）",
        );
      } catch (e) {
        setSettingsMsg(String(e));
      }
    }, 600);
    return () => {
      if (settingsTimer.current !== null) window.clearTimeout(settingsTimer.current);
    };
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [audioSel, audioGain, hotkey, hotkeyOn]);

  useEffect(() => {
    const unlisten: Array<() => void> = [];
    const register = async () => {
      unlisten.push(
        await listen<ProgressPayload>("recording-progress", (e) => setProgress(e.payload)),
      );
      unlisten.push(
        await listen<StatePayload>("recording-state-changed", (e) => {
          setState(e.payload.state);
          if (e.payload.error_message) setError(e.payload.error_message);
        }),
      );
      unlisten.push(
        await listen<FinishedPayload>("recording-finished", (e) => {
          setFinished(e.payload);
          setState("Idle");
          setProgress(null);
          // 停止后的成功产物 Toast（6 秒自动消失）；停止原因见遥测面板的"上次录制"行
          setToast(e.payload.output_path);
          if (toastTimer.current !== null) window.clearTimeout(toastTimer.current);
          toastTimer.current = window.setTimeout(() => setToast(null), 6000);
          // 退出流程在等这一刻：文件已经最终化（moov 落盘）才能让后端退出进程。
          if (quittingRef.current) {
            quittingRef.current = false;
            void invoke("quit_done").catch(() => {});
          }
        }),
      );
      unlisten.push(
        await listen<{ x: number; y: number; width: number; height: number }>(
          "region-selected",
          (e) => {
            // 后端已把归一化选区换算成物理像素，这里直接回填
            setRegion(e.payload);
            setFullScreen(false);
          },
        ),
      );
      unlisten.push(
        await listen("ui-command", () => {
          // 这只是"来取一下"的唤醒提示，不带命令：命令永远从队列取。
          void takeCommandRef.current();
        }),
      );
      unlisten.push(
        await listen<{ code: string; message: string }>("recording-error", (e) =>
          setError(`[${e.payload.code}] ${e.payload.message}`),
        ),
      );
    };
    void register().then(() => {
      // 监听器都挂上了 → 告诉后端"我准备好了"，并立刻取走启动窗口内攒下的命令
      void invoke("ui_ready")
        .catch(() => {})
        .then(() => takeCommandRef.current());
      // DOM 自检探针：像素层证据在这台机器上不可靠，让前端把 DOM 实际状态报进日志判读
      let probe: Record<string, unknown>;
      try {
        const root = document.getElementById("root");
        const btnRect = (selector: string) => {
          const el = document.querySelector(selector);
          if (!el) return null;
          const r = el.getBoundingClientRect();
          const sx = window.screenX + (window.outerWidth - window.innerWidth) / 2;
          const sy = window.screenY + (window.outerHeight - window.innerHeight);
          return {
            x: Math.round(sx + r.x + r.width / 2),
            y: Math.round(sy + r.y + r.height / 2),
          };
        };
        probe = {
          root_children: root ? root.childElementCount : -1,
          root_exists: !!root,
          body_text_len: document.body.innerText.length,
          ready_state: document.readyState,
          url: String(location.href).slice(0, 90),
          head: (document.body.innerText || document.body.innerHTML).slice(0, 60),
          buttons: {
            region: btnRect(".island-row .icon-btn:last-child"),
            start: btnRect(".action-row"),
          },
        };
      } catch (e) {
        probe = {
          root_children: -2,
          root_exists: false,
          body_text_len: -1,
          ready_state: String(document.readyState),
          url: String(location.href).slice(0, 90),
          head: `probe-error: ${String(e).slice(0, 40)}`,
        };
      }
      void invoke("ui_probe", { probe }).catch(() => {});
    });
    return () => unlisten.forEach((f) => f());
  }, []);

  // 命令队列的常驻拉取
  useEffect(() => {
    const t = window.setInterval(() => void takeCommandRef.current(), 500);
    return () => window.clearInterval(t);
  }, []);

  // 录制中定期拉一次完整状态，拿到「实际生效的编码参数」与状态兜底
  useEffect(() => {
    if (state !== "Recording") {
      if (statusTimer.current !== null) {
        window.clearInterval(statusTimer.current);
        statusTimer.current = null;
      }
      return;
    }
    statusTimer.current = window.setInterval(async () => {
      try {
        const st = await invoke<RecordingStatus | null>("get_status");
        if (st) {
          setActual(st.actual);
          setState(st.state);
        }
      } catch {
        /* 忽略瞬时错误 */
      }
    }, 1000);
    return () => {
      if (statusTimer.current !== null) window.clearInterval(statusTimer.current);
    };
  }, [state]);

  /**
   * 参数化的启动：regionOverride = undefined → 整屏；否则 = 物理像素选区。
   * （悬浮 Dock 的两个动作键对应两种 region 取值；热键/按钮共用一个 start）
   */
  const startWith = async (regionOverride: typeof region | undefined) => {
    setBusy(true);
    setError(null);
    setFinished(null);
    setToast(null);
    try {
      await invoke("start_recording", {
        request: {
          display_id: displayId,
          fps,
          output_dir: outputDir,
          region: regionOverride,
          audio_sources: (["system", "microphone"] as const)
            .filter((k) => audioSel[k])
            .map((k) => ({ kind: k, gain: audioGain[k] })),
        },
      });
      setState("Preparing");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  const start = () => startWith(fullScreen ? undefined : region);
  const startFullscreen = () => {
    setFullScreen(true);
    void startWith(undefined);
  };
  const startRegion = () => {
    setFullScreen(false);
    void startWith(region);
  };

  const stop = async () => {
    setBusy(true);
    try {
      await invoke("stop_recording");
      setState("Stopping");
    } catch (e) {
      setError(String(e));
    } finally {
      setBusy(false);
    }
  };

  /**
   * 退出流程（托盘"退出" / 录制中点窗口关闭）。先停干净并等最终化，再让后端退出。
   */
  const quitFlow = async () => {
    if (quittingRef.current) return;
    quittingRef.current = true;
    const done = () => {
      quittingRef.current = false;
      void invoke("quit_done").catch(() => {});
    };
    const busyNow =
      stateRef.current === "Recording" ||
      stateRef.current === "Preparing" ||
      stateRef.current === "Stopping";
    if (!busyNow) {
      done();
      return;
    }
    try {
      await stop();
    } finally {
      // 兜底：万一本帧的 `recording-finished` 已经发过，2 秒后也回声（幂等）。
      window.setTimeout(done, 2000);
    }
  };

  /**
   * 取命令并执行——唯一的命令分发点。
   */
  const takeCommand = async () => {
    let cmd: string | null = null;
    try {
      cmd = await invoke<string | null>("take_ui_command");
    } catch {
      return;
    }
    if (!cmd) return;
    if (cmd === "toggle") {
      const busyNow =
        stateRef.current === "Recording" ||
        stateRef.current === "Preparing" ||
        stateRef.current === "Stopping";
      void (busyNow ? stop() : start());
    } else if (cmd === "open-dir") {
      const dir = dirRef.current || dirOf(finishedRef.current?.output_path);
      if (dir) {
        void invoke("open_output_directory", { path: dir }).catch((err) => setError(String(err)));
      }
    } else if (cmd === "quit") {
      void quitFlow();
    }
  };
  takeCommandRef.current = () => void takeCommand();

  // 窗口级快捷键：Enter = 整屏录制、⌥(Alt)+Enter = 选区录制（输入框聚焦时不触发）
  useEffect(() => {
    const onKey = (e: KeyboardEvent) => {
      const tag = (e.target as HTMLElement)?.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
      if (e.key !== "Enter") return;
      const busyNow =
        stateRef.current === "Recording" ||
        stateRef.current === "Preparing" ||
        stateRef.current === "Stopping";
      if (busyNow) return;
      e.preventDefault();
      if (e.altKey) startRegion();
      else startFullscreen();
    };
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [fps, outputDir, displayId, audioSel, audioGain, region]);

  const openRegionSelector = async () => {
    setError(null);
    try {
      await invoke("open_region_selector", { displayId });
    } catch (e) {
      setError(String(e));
    }
  };

  const isRecording = state === "Recording" || state === "Preparing" || state === "Stopping";
  const closeWindow = () => void getCurrentWindow().close();

  // 悬浮 Dock 尺寸联动：
  // mode 驱动——只有"形态跳变"才允许 setSize：
  // 三岛 ─┬─ 录制迷你条 ─┬─ +HUD
  // └─ +HUD
  // 录制时窗口必须收缩（否则透明死区挡住下层点击）；HUD 要从岛3 向下展开，
  // 窗口也得跟着往下长 ⇒ 这两类跳变都必须走 setSize，没有别的办法。
  //
  // HUD 开合会 setSize，而那正是 tao 重写 GWL_STYLE（标题栏回归）的老触发器。
  // 所以这里的无边框保证不靠"不 resize"，而靠 install_frameless_subclass
  // 在 WM_STYLECHANGING 把写回路径挡死（见 commands.rs 的根因表）。
  //
  // Settings 面板 / Toast / Error 仍然是 DOM Overlay（absolute、脱离文档流），
  // 它们的开合不改变 mode、不触发本 effect。
  const dockWin = getCurrentWindow();
  const dockMode = isRecording
    ? hudOpen
      ? "rec-hud"
      : "rec-mini"
    : hudOpen
      ? "dock-hud"
      : "dock";
  useEffect(() => {
    // 等一帧：让本形态的 DOM（岛/迷你条）完成布局再量
    const raf = requestAnimationFrame(() => {
      const dock = document.querySelector<HTMLElement>(".dock");
      if (!dock) return;
      // .dock 有 min-height:100vh（窗口比内容高时它会被撑满）——量内容前临时摘掉，
      // getBoundingClientRect 强制同步重排，不会闪烁（浏览器不在同步 JS 中间绘制）。
      const prevMin = dock.style.minHeight;
      dock.style.minHeight = "0px";
      let h = Math.ceil(dock.getBoundingClientRect().height);
      dock.style.minHeight = prevMin;
      if (h < 80) return; // 内容尚未渲染出的异常值保护（别把窗口缩没）
      h += 10; // 安全区：岛阴影不被窗口硬裁，又不构成巨大透明死区（8~16px）
      // 诊断：setSize 的成败经 ui_probe 落日志（本机的 .catch 吞错误出现过一次）
      // 同时在布局定型后把元素几何一并报上去（`layout`）——横向裁切类问题只能靠它定位
      const layoutSnapshot = () => {
        try {
          const de = document.documentElement;
          const info = (el: Element | null) => {
            if (!el) return null;
            const r = el.getBoundingClientRect();
            return {
              l: Math.round(r.left),
              r: Math.round(r.right),
              w: Math.round(r.width),
              over: Math.round(r.right - de.clientWidth), // >0 = 右端被窗口裁掉
            };
          };
          return {
            viewport: { w: window.innerWidth, h: window.innerHeight },
            doc: { scrollW: de.scrollWidth, clientW: de.clientWidth, scrollH: de.scrollHeight, clientH: de.clientHeight },
            dock: info(document.querySelector(".dock")),
            islands: Array.from(document.querySelectorAll(".island")).map((e) => info(e)),
            rows: Array.from(document.querySelectorAll(".island-row, .island-icons")).map((e) => info(e)),
          };
        } catch (e) {
          return { err: String(e).slice(0, 60) };
        }
      };
      const report = (ok: boolean, note: string) =>
        void invoke("ui_probe", {
          probe: {
            root_children: -9,
            root_exists: ok,
            body_text_len: h,
            ready_state: ok ? "resize-ok" : "resize-fail",
            url: "",
            head: note.slice(0, 70),
            layout: layoutSnapshot(),
          },
        }).catch(() => {});
      dockWin
        .setSize(new LogicalSize(360, h))
        .then(() => report(true, `setSize 360x${h} ok（mode=${dockMode} 量得内容高）`))
        .catch((e) => report(false, `setSize 失败：${String(e)}`));
    });
    return () => cancelAnimationFrame(raf);
    // eslint-disable-next-line react-hooks/exhaustive-deps
  }, [dockMode]);

  /** 打开 Settings Popover：以齿轮按钮的真实几何作锚点 */
  const openSettings = () => {
    const gear = gearBtnRef.current;
    const dock = document.querySelector(".dock");
    if (gear && dock) {
      const g = gear.getBoundingClientRect();
      const d = dock.getBoundingClientRect();
      setSettingsAnchor({ top: Math.round(g.bottom - d.top + 6) });
    } else {
      setSettingsAnchor(null);
    }
    setPanel("settings");
  };

  return (
    <main
      className="dock"
      data-tauri-drag-region
      // 显式拖拽兜底：`data-tauri-drag-region` 由 Tauri 的注入脚本实现，
      // 在无边框+自定义元素场景下不总是可靠。这里在容器自身的空白区
      // （即 `.dock` 的 padding 与岛间缝隙）再挂一次原生拖拽。
      // 必须 `target === currentTarget`：否则点按钮也会变成拖窗口。
      // 依赖权限 `core:window:allow-start-dragging`（capabilities 里必须有
      // 否则和 setSize 一样被静默拒绝）。
      onMouseDown={(e) => {
        if (e.target === e.currentTarget) void getCurrentWindow().startDragging();
      }}
    >
      <button className="dock-close" title="关闭窗口（也可从托盘菜单退出）" onClick={closeWindow}>
        ×
      </button>

      {isRecording ? (
        <>
          {/* ============ 录制态：迷你控制条（点击展开监测面板；HUD 在录制态是流内岛，参与 mode 尺寸）============ */}
          <RecordingMiniBar
            elapsedMs={progress?.elapsed_ms ?? 0}
            framesDropped={progress?.frames_dropped ?? 0}
            hudOpen={hudOpen}
            busy={busy}
            onToggleHud={() => setHudOpen(!hudOpen)}
            onStop={() => void stop()}
          />
          {hudOpen && <HudPanel progress={progress} finished={finished} actual={actual} />}
        </>
      ) : (
        <>
          {/* ============ 三个独立悬浮岛（恒渲染；不要再加第四层总容器背景）============ */}
          <ControlIsland
            fullScreen={fullScreen}
            region={region}
            displayReady={!!displayId}
            audioSel={audioSel}
            audioGain={audioGain}
            fps={fps}
            hotkey={hotkey}
            hotkeyOn={hotkeyOn}
            gearRef={gearBtnRef}
            onToggleFullScreen={() => setFullScreen(!fullScreen)}
            onOpenSettings={openSettings}
            onOpenRegionSelector={() => void openRegionSelector()}
            onToggleAudio={(kind) => setAudioSel({ ...audioSel, [kind]: !audioSel[kind] })}
            onCycleFps={() => setFps(fps === 30 ? 60 : fps === 60 ? 90 : 30)}
          />
          <RecordingIsland
            disabled={busy || !displayId || (!audioSel.system && !audioSel.microphone)}
            onStartRegion={startRegion}
            onStartFullscreen={startFullscreen}
          />
          <TelemetryIsland hudOpen={hudOpen} onToggle={() => setHudOpen(!hudOpen)} />

          {/* ============ Settings：DOM Popover（贴齿轮锚点；不建新窗口、不 resize 主窗口）============ */}
          {panel === "settings" && (
            <>
              <div className="popover-overlay" onClick={() => setPanel(null)} />
              <div
                className="dock-popover"
                style={{
                  top: settingsAnchor?.top ?? 56,
                  left: 12,
                  right: 12,
                  maxHeight: `calc(100vh - ${(settingsAnchor?.top ?? 56) + 10}px)`,
                }}
              >
                <SettingsPanel
                  displays={displays}
                  displayId={displayId}
                  setDisplayId={setDisplayId}
                  outputDir={outputDir}
                  setOutputDir={setOutputDir}
                  audioGain={audioGain}
                  setAudioGain={setAudioGain}
                  hotkey={hotkey}
                  setHotkey={setHotkey}
                  hotkeyOn={hotkeyOn}
                  setHotkeyOn={setHotkeyOn}
                  captureVisible={captureVisible}
                  setCaptureVisible={setCaptureVisible}
                  settingsMsg={settingsMsg}
                  region={region}
                  setRegion={setRegion}
                  fullScreen={fullScreen}
                  setFullScreen={setFullScreen}
                  onPickDir={async () => {
                    try {
                      const picked = await open({
                        directory: true,
                        multiple: false,
                        title: "选择录制输出目录",
                      });
                      if (typeof picked === "string") setOutputDir(picked);
                    } catch (e) {
                      setError(String(e));
                    }
                  }}
                  onBack={() => setPanel(null)}
                />
              </div>
            </>
          )}

          {/* ============ 遥测：岛3 下方的流内岛，窗口随之向下展开============
              点遮罩关闭；面板自身带 data-tauri-drag-region 且 z-index 高于遮罩，
              所以它不会被遮罩吞掉（拖拽、滚动都正常）。 */}
          {hudOpen && (
            <>
              <div className="popover-overlay" onClick={() => setHudOpen(false)} />
              <HudPanel progress={progress} finished={finished} actual={actual} />
            </>
          )}

          {toast && <Toast path={toast} onError={setError} />}
        </>
      )}

      {error && <p className="dock-error">{error}</p>}
    </main>
  );
}
