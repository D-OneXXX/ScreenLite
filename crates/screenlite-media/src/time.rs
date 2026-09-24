//! 时间戳契约。
//!
//! 事实：`Direct3D11CaptureFrame.SystemRelativeTime` 是合成器渲染该帧时的系统相对时间，
//! 单位 100 纳秒，时基为 QPC 归一化时基。它不是纳秒、不是毫秒、不是绝对时间。
//!
//! 因此本模块用类型承载单位，让"把 100ns 当 ns"这类错误在编译期就不可能发生：
//! 字段名必须带单位后缀，换算只能经过 [`Timeline::to_mf`] 这一个点。

use crate::consts::MF_TICKS_PER_SEC;

/// WGC `SystemRelativeTime` 原始值。单位 100ns，会话内单调递增。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct Srt100ns(pub i64);

/// 交给 Media Foundation 的时间。单位 100ns，相对会话起点（第一个帧）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct MfTime100ns(pub i64);

/// 原始 QPC 计数。仅在需要与音频（WASAPI）对齐时使用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub struct QpcTicks(pub i64);

/// 调度循环的时间基准（仅用于调度与超时，不作为视频时间）。
pub trait BaseInstant: Copy + Send {
    fn elapsed_secs_f64(&self) -> f64;
}

impl BaseInstant for std::time::Instant {
    fn elapsed_secs_f64(&self) -> f64 {
        self.elapsed().as_secs_f64()
    }
}

/// QPC → 系统相对时间。必须经过频率换算，不得直接比较两个不同来源的 tick。
pub fn srt_from_qpc(qpc: QpcTicks, qpc_frequency: i64) -> Srt100ns {
    debug_assert!(qpc_frequency > 0, "QueryPerformanceFrequency 必须为正");
    let ticks = (qpc.0 as i128) * (MF_TICKS_PER_SEC as i128) / (qpc_frequency as i128);
    Srt100ns(ticks as i64)
}

/// 会话时间轴：唯一的 SRT → MF 换算点，并负责强制单调性。
#[derive(Debug, Clone)]
pub struct Timeline {
    t0: Srt100ns,
    last_mf: Option<MfTime100ns>,
    anomalies: u64,
}

impl Timeline {
    /// `t0` 取第一帧到达时的 SRT。
    pub fn new(t0: Srt100ns) -> Self {
        Self {
            t0,
            last_mf: None,
            anomalies: 0,
        }
    }

    pub fn origin(&self) -> Srt100ns {
        self.t0
    }

    /// 唯一的换算点：同为 100ns，是整数减法，不是乘除。
    ///
    /// 单调性强制：若新值不大于上一个值，则 `= last + 1` 并计入 `timestamp_anomalies`。
    pub fn to_mf(&mut self, srt: Srt100ns) -> MfTime100ns {
        let mut mf = MfTime100ns(srt.0 - self.t0.0);
        if let Some(last) = self.last_mf {
            if mf.0 <= last.0 {
                mf = MfTime100ns(last.0 + 1);
                self.anomalies += 1;
            }
        }
        self.last_mf = Some(mf);
        mf
    }

    /// 录制时长 = 最后一帧 SRT − t0。不得用 `Instant` 差值当作视频时长。
    pub fn elapsed(&self, srt: Srt100ns) -> MfTime100ns {
        MfTime100ns(srt.0 - self.t0.0)
    }

    pub fn anomalies(&self) -> u64 {
        self.anomalies
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_conversion_is_a_subtraction_not_a_scale() {
        // 若误把 100ns 当 ns 处理，会得到 10 倍或 1/10 的错值。
        let mut tl = Timeline::new(Srt100ns(1_000_000));
        let a = tl.to_mf(Srt100ns(1_000_000)); // 第一帧 → 0
        assert_eq!(a.0, 0);
        let b = tl.to_mf(Srt100ns(1_333_333)); // 一个 30fps 帧间隔
        assert_eq!(b.0, 333_333);
    }

    #[test]
    fn timestamp_monotonic_clamp() {
        let mut tl = Timeline::new(Srt100ns(0));
        assert_eq!(tl.to_mf(Srt100ns(100)).0, 100);
        // 非单调：必须被修正为 last+1，而不是静默接受
        assert_eq!(tl.to_mf(Srt100ns(100)).0, 101);
        assert_eq!(tl.to_mf(Srt100ns(50)).0, 102);
        assert_eq!(tl.anomalies(), 2);
        assert_eq!(tl.to_mf(Srt100ns(200)).0, 200);
        assert_eq!(tl.anomalies(), 2);
    }

    #[test]
    fn qpc_conversion_uses_frequency() {
        // 10MHz QPC：1 tick == 100ns
        assert_eq!(srt_from_qpc(QpcTicks(12_345), 10_000_000).0, 12_345);
        // 3.579545MHz 常见时基：1 秒 QPC 应换算为 1e7 个 100ns
        let s = srt_from_qpc(QpcTicks(3_579_545), 3_579_545);
        assert_eq!(s.0, 10_000_000);
    }
}
