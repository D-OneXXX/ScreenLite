//! 用户设置：**音频来源与增益**、**全局快捷键**。
//!
//! 为什么要有这个文件：
//! 1. 音频设置不持久化 ⇒ 每次启动都要重新勾麦克风（实测抱怨）
//! 2. `Ctrl+Alt+R` 是产品唯一的全局快捷键，而全局快捷键会把它从别的程序手里
//! "scoop"走 —— 如果用户自己的工具也用同一个组合，装了本产品之后那个功能就失效。
//! 这是全局热键的固有代价，必须有出口：可配置 + 可停用。
//!
//! 设计原则：读不到/读坏了就退回默认值（默认值 = 当前行为：系统声音 + Ctrl+Alt+R），
//! 绝不让"配置文件坏了"变成"App 起不来"。

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

/// 单个音频源的偏好。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AudioSourcePref {
    /// `"system"` 或 `"microphone"`（与 UI 的取值一致）
    pub kind: String,
    /// 线性增益 0.0–2.0（越界会被 `clamp_gain` 收敛）
    #[serde(default = "one")]
    pub gain: f32,
}

fn one() -> f32 {
    1.0
}

/// 用户设置。字段全部带默认值：缺字段/多字段都不会导致解析失败。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AppSettings {
    /// 勾选了哪些音频源（空 = 用默认：系统声音）
    #[serde(default)]
    pub audio_sources: Vec<AudioSourcePref>,
    /// 全局快捷键；`None` = 停用（显式关掉）
    #[serde(default = "default_hotkey")]
    pub hotkey: Option<String>,
    /// 允许控制窗口出现在截图/录制里（默认 `false` = 排除）。
    ///
    /// 默认排除是产品该有的行为。
    /// 但排除会让用户自己截图时窗口"消失" （实测反馈）——
    /// 所以留这个开关：需要截图/演示时打开，平时关着。
    #[serde(default)]
    pub capture_visible: bool,
}

fn default_hotkey() -> Option<String> {
    Some("Ctrl+Alt+R".to_string())
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            audio_sources: vec![AudioSourcePref {
                kind: "system".into(),
                gain: 1.0,
            }],
            hotkey: default_hotkey(),
            capture_visible: false,
        }
    }
}

impl AppSettings {
    /// 归一化：把不可能的值收敛成可用的值（坏配置不该变成坏行为）。
    pub fn normalized(mut self) -> Self {
        // 音频源：去掉未知 kind、去重、增益收敛；空则回到默认单路系统声音
        let mut seen: Vec<String> = Vec::new();
        let mut clean: Vec<AudioSourcePref> = Vec::new();
        for s in self.audio_sources.drain(..) {
            let kind = s.kind.trim().to_ascii_lowercase();
            if kind != "system" && kind != "microphone" {
                continue;
            }
            if seen.contains(&kind) {
                continue;
            }
            seen.push(kind.clone());
            clean.push(AudioSourcePref {
                kind,
                gain: screenlite_media::audio::clamp_gain(s.gain),
            });
        }
        if clean.is_empty() {
            clean.push(AudioSourcePref {
                kind: "system".into(),
                gain: 1.0,
            });
        }
        self.audio_sources = clean;

        // 快捷键：空串/纯空白 → 视为停用；其余保留（能否解析由应用层验证）
        self.hotkey = match self.hotkey {
            Some(h) if h.trim().is_empty() => None,
            Some(h) => Some(h.trim().to_string()),
            None => None,
        };
        self
    }
}

/// 设置文件路径：`%LOCALAPPDATA%\ScreenLite\settings.json`。
///
/// 可用环境变量 `SCREENLITE_SETTINGS` 覆盖（集成测试用；也方便手工排查）。
pub fn settings_path() -> PathBuf {
    if let Some(p) = std::env::var_os("SCREENLITE_SETTINGS") {
        return PathBuf::from(p);
    }
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("ScreenLite")
        .join("settings.json")
}

/// 读取设置。任何失败都返回默认值（含"文件不存在"——首次运行就是这样）。
pub fn load() -> AppSettings {
    load_from(&settings_path())
}

/// 写入设置（原子性够用：小文件、单进程写）。
pub fn save(s: &AppSettings) -> Result<(), String> {
    save_to(&settings_path(), s)
}

pub fn load_from(path: &Path) -> AppSettings {
    match std::fs::read_to_string(path) {
        Ok(text) => {
            // 跳过 UTF-8 BOM：记事本/PowerShell 的 `Set-Content -Encoding UTF8`
            // 都会写 BOM，而 serde_json 遇到 BOM 直接报 `expected value at line 1 column 1`。
            // 实测：用户（或脚本）手写的配置会因此被当成坏文件。
            let text = text.trim_start_matches('\u{feff}');
            match serde_json::from_str::<AppSettings>(text) {
                Ok(s) => {
                    let n = s.clone().normalized();
                    if n != s {
                        tracing::warn!(path = %path.display(), "设置里有无效值，已归一化");
                    }
                    n
                }
                Err(e) => {
                    // 先备份再降级：不然前端拿到默认值后会把它回写，用户的原文件就没了
                    // （实测：一个 BOM 就能让用户手写的配置被默认值覆盖）
                    let backup = path.with_extension(format!(
                        "bad-{}",
                        std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|d| d.as_secs())
                            .unwrap_or(0)
                    ));
                    let backed = std::fs::rename(path, &backup).is_ok();
                    tracing::warn!(
                        error = %e,
                        path = %path.display(),
                        备份 = if backed { backup.display().to_string() } else { "(备份失败)".into() },
                        "设置文件解析失败：已备份并改用默认值"
                    );
                    AppSettings::default()
                }
            }
        }
        Err(_) => AppSettings::default(),
    }
}

pub fn save_to(path: &Path, s: &AppSettings) -> Result<(), String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建设置目录失败：{}", e))?;
    }
    let text = serde_json::to_string_pretty(&s.clone().normalized())
        .map_err(|e| format!("序列化设置失败：{}", e))?;
    std::fs::write(path, text).map_err(|e| format!("写入设置失败：{}", e))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("sl-settings-{}-{}.json", name, std::process::id()))
    }

    #[test]
    fn defaults_match_current_behavior() {
        let d = AppSettings::default();
        assert_eq!(d.audio_sources.len(), 1);
        assert_eq!(d.audio_sources[0].kind, "system");
        assert_eq!(d.audio_sources[0].gain, 1.0);
        assert_eq!(d.hotkey.as_deref(), Some("Ctrl+Alt+R"));
    }

    #[test]
    fn missing_file_yields_defaults_not_error() {
        let p = tmp("missing");
        let _ = std::fs::remove_file(&p);
        assert_eq!(load_from(&p), AppSettings::default());
    }

    #[test]
    fn broken_file_yields_defaults_not_error() {
        let p = tmp("broken");
        std::fs::write(&p, "{ this is not json").unwrap();
        assert_eq!(load_from(&p), AppSettings::default());
        // 坏文件必须被备份走，否则前端会把默认值回写、用户的原文件就没了
        assert!(!p.exists(), "解析失败的文件应被改名备份，而不是原样留着");
        let _ = std::fs::remove_file(&p);
        // 清理备份
        if let Some(dir) = p.parent() {
            let stem = p.file_stem().unwrap().to_string_lossy().to_string();
            if let Ok(rd) = std::fs::read_dir(dir) {
                for e in rd.flatten() {
                    let n = e.file_name().to_string_lossy().to_string();
                    if n.starts_with(&stem) && n.contains("bad-") {
                        let _ = std::fs::remove_file(e.path());
                    }
                }
            }
        }
    }

    /// 记事本 / PowerShell 的 `Set-Content -Encoding UTF8` 会写 BOM —— 必须能正常读
    #[test]
    fn utf8_bom_is_tolerated() {
        let p = tmp("bom");
        let json = "{\"audio_sources\":[{\"kind\":\"microphone\",\"gain\":1.5}],\"hotkey\":null}";
        std::fs::write(&p, format!("\u{feff}{}", json)).unwrap();
        let s = load_from(&p);
        assert_eq!(s.audio_sources.len(), 1, "带 BOM 的文件必须能读（否则用户手写的配置会被当坏文件）");
        assert_eq!(s.audio_sources[0].kind, "microphone");
        assert_eq!(s.hotkey, None);
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn roundtrip_keeps_two_sources_and_disabled_hotkey() {
        let p = tmp("roundtrip");
        let s = AppSettings {
            audio_sources: vec![
                AudioSourcePref { kind: "system".into(), gain: 0.5 },
                AudioSourcePref { kind: "microphone".into(), gain: 1.7 },
            ],
            hotkey: None, // 停用
            capture_visible: false,
        };
        save_to(&p, &s).expect("保存应成功");
        let back = load_from(&p);
        assert_eq!(back.audio_sources.len(), 2, "两路必须原样读回");
        assert_eq!(back.audio_sources[1].kind, "microphone");
        assert!((back.audio_sources[1].gain - 1.7).abs() < 1e-6);
        assert_eq!(back.hotkey, None, "停用状态必须保持（不能被默认值顶回 Ctrl+Alt+R）");
        let _ = std::fs::remove_file(&p);
    }

    #[test]
    fn normalization_cleans_impossible_values() {
        let s = AppSettings {
            audio_sources: vec![
                AudioSourcePref { kind: "SPEAKER".into(), gain: 1.0 },   // 未知 → 丢弃
                AudioSourcePref { kind: " microphone ".into(), gain: 99.0 }, // 去空格 + 增益收敛
                AudioSourcePref { kind: "microphone".into(), gain: 1.0 },   // 重复 → 丢弃
            ],
            hotkey: Some("   ".into()), // 空白 → 停用
            capture_visible: false,
        }
        .normalized();
        assert_eq!(s.audio_sources.len(), 1, "未知与重复都要被清掉");
        assert_eq!(s.audio_sources[0].kind, "microphone");
        assert!(s.audio_sources[0].gain <= 2.0, "增益必须被收敛");
        assert_eq!(s.hotkey, None, "空白快捷键等于停用");
    }

    #[test]
    fn empty_audio_list_falls_back_to_system() {
        let s = AppSettings { audio_sources: vec![], hotkey: None, capture_visible: false }.normalized();
        assert_eq!(s.audio_sources.len(), 1);
        assert_eq!(s.audio_sources[0].kind, "system", "空列表必须回到默认单路，否则等于静音录制");
    }
}
