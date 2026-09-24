//! ScreenLite V0.1 媒体管线
//!
//! 分层：
//!
//! ```text
//! capture WGC/D3D11 → 帧（BGRA8 表面 + SRT 时间戳）
//! adapter BGRA8 → NV12（显式转换）→ 自包含 CPU 帧
//! encoder → Media Foundation H.264 → MP4
//! engine 状态机 / 30Hz CFR 调度器 / 有界队列 / 停止流程
//! ```
//!
//! 本 crate 不依赖 Tauri，可独立编译与测试。

pub mod adapter;
pub mod audio;
pub mod capture;
pub mod consts;
pub mod convert;
pub mod disk;
pub mod encoder;
pub mod engine;
pub mod error;
pub mod mf;
pub mod scheduler;
pub mod time;
pub mod verify;

pub use error::{MediaError, MediaResult};
