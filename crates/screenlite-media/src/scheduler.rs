//! 帧调度。
//!
//! V0.1 输出 CFR 30fps：由媒体线程 30Hz 时基驱动，不由 FrameArrived 驱动。
//! 槽位时间必须由 `n` 反算，禁止 `next_pts += 333_333`（会漂移且不可逆）。

use crate::consts::MF_TICKS_PER_SEC;
use crate::time::MfTime100ns;

/// `slot(n) = t0 + n * 10_000_000 / fps`，整数除法，无累加误差。
pub fn slot_time_mf(t0: MfTime100ns, n: u64, fps: u32) -> MfTime100ns {
    debug_assert!(fps > 0);
    let offset = (n as i128) * (MF_TICKS_PER_SEC as i128) / (fps as i128);
    MfTime100ns(t0.0 + offset as i64)
}

/// 第 n 个槽位的时长 = `slot(n+1) - slot(n)`，可能是 333333 或 333334，不是常量。
pub fn slot_duration_mf(n: u64, fps: u32) -> MfTime100ns {
    MfTime100ns(slot_time_mf(MfTime100ns(0), n + 1, fps).0 - slot_time_mf(MfTime100ns(0), n, fps).0)
}

/// 第 n 个槽位相对于循环起点的等待时间，用于 deadline 唤醒。
pub fn slot_deadline_ns(n: u64, fps: u32) -> u64 {
    debug_assert!(fps > 0);
    (n as u128 * 1_000_000_000u128 / fps as u128) as u64
}

/// 槽位总数：给定已流逝的 MF 时间，返回应该已经产出的槽位数。
pub fn slots_elapsed(elapsed: MfTime100ns, fps: u32) -> u64 {
    if elapsed.0 <= 0 {
        return 0;
    }
    ((elapsed.0 as i128) * (fps as i128) / (MF_TICKS_PER_SEC as i128)) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn nv12_ticks(n: u64, fps: u32) -> i64 {
        slot_time_mf(MfTime100ns(0), n, fps).0
    }

    #[test]
    fn test_slot_time_no_drift() {
        // 30fps × 9000 槽 = 300 秒，必须精确等于 3_000_000_000 个 100ns
        assert_eq!(nv12_ticks(9000, 30), 3_000_000_000);

        // 如果用累加（每帧固定 333_333），300 秒会漂移 3000 个 tick（0.3ms）
        let accumulated = 9000i64 * 333_333;
        assert_eq!(accumulated, 2_999_997_000);
        assert_eq!(nv12_ticks(9000, 30) - accumulated, 3000);

        // 1 小时：累加式漂移会达到 36_000 tick = 3.6ms
        assert_eq!(nv12_ticks(108_000, 30), 36_000_000_000);
    }

    #[test]
    fn test_slot_time_strictly_increasing() {
        let mut prev = -1i64;
        for n in 0..1000u64 {
            let t = nv12_ticks(n, 30);
            assert!(t > prev, "slot {} 未递增", n);
            prev = t;
        }
    }

    #[test]
    fn test_slot_duration_is_1_or_2_ticks_of_variation() {
        let mut d_min = i64::MAX;
        let mut d_max = i64::MIN;
        for n in 0..1000u64 {
            let d = slot_duration_mf(n, 30).0;
            d_min = d_min.min(d);
            d_max = d_max.max(d);
        }
        assert_eq!(d_min, 333_333);
        assert_eq!(d_max, 333_334);
        assert!(d_max - d_min <= 1);
    }

    #[test]
    fn test_slot_deadline_ns() {
        assert_eq!(slot_deadline_ns(0, 30), 0);
        assert_eq!(slot_deadline_ns(30, 30), 1_000_000_000);
        // 不允许累加误差：9000 槽 = 300 秒
        assert_eq!(slot_deadline_ns(9000, 30), 300_000_000_000);
    }

    #[test]
    fn test_slots_elapsed() {
        assert_eq!(slots_elapsed(MfTime100ns(0), 30), 0);
        assert_eq!(slots_elapsed(MfTime100ns(333_333), 30), 0);
        assert_eq!(slots_elapsed(MfTime100ns(1_000_000), 30), 3);
        assert_eq!(slots_elapsed(MfTime100ns(10_000_000), 30), 30);
    }

    /// 对白名单里的每一档帧率逐个验证时间轴数学：
    /// 这是"录制时长是否准确"的根，必须在纯逻辑层就精确成立。
    #[test]
    fn slot_math_is_exact_for_every_allowed_frame_rate() {
        use crate::consts::{ALLOWED_FPS, MF_TICKS_PER_SEC};
        use crate::scheduler::slot_deadline_ns;

        for fps in ALLOWED_FPS {
            // ① 10 秒的槽位时间必须精确等于 10 秒（有理数反算，无漂移）
            let n10 = (10 * fps) as u64;
            assert_eq!(
                slot_time_mf(MfTime100ns(0), n10, fps).0,
                10 * MF_TICKS_PER_SEC,
                "{}fps：10 秒槽位时间不精确",
                fps
            );

            // ② 严格递增
            let mut prev = -1i64;
            for n in 0..n10.min(2000) {
                let t = slot_time_mf(MfTime100ns(0), n, fps).0;
                assert!(t > prev, "{}fps：槽位 {} 未严格递增", fps, n);
                prev = t;
            }

            // ③ 每帧时长与理论值的偏差 ≤ 1 tick（且只有两种取值）
            let ideal = MF_TICKS_PER_SEC as f64 / fps as f64;
            let mut seen = Vec::new();
            for n in 0..200u64 {
                let d = slot_duration_mf(n, fps).0;
                assert!(
                    (d as f64 - ideal).abs() <= 1.0,
                    "{}fps：第 {} 帧时长 {} 偏离理论值 {:.2}",
                    fps,
                    n,
                    d,
                    ideal
                );
                if !seen.contains(&d) {
                    seen.push(d);
                }
            }
            assert!(
                seen.len() <= 2,
                "{}fps：帧时长出现了 {} 种取值（应只有 2 种）",
                fps,
                seen.len()
            );

            // ④ 调度唤醒（纳秒）与槽位时间（100ns）必须同源：
            // fps 个槽位精确等于 1 秒，且任意 n 的两者换算一致（≤1 tick）
            assert_eq!(
                slot_deadline_ns(fps as u64, fps),
                1_000_000_000,
                "{}fps：{} 个 deadline 不等于 1 秒",
                fps,
                fps
            );            for n in [1u64, 7, 1000] {
                let ns = slot_deadline_ns(n, fps) as i64;
                let ticks = slot_time_mf(MfTime100ns(0), n, fps).0;
                assert!(
                    (ns - ticks * 100).abs() <= 100,
                    "{}fps：第 {} 槽 deadline({}ns) 与槽位时间({}tick) 不一致",
                    fps,
                    n,
                    ns,
                    ticks
                );
            }

            // ⑤ 一小时的槽位数与理论值一致（长录制不丢/不多帧）
            let n1h = slots_elapsed(MfTime100ns(3600 * MF_TICKS_PER_SEC), fps);
            assert_eq!(n1h, 3600 * fps as u64, "{}fps：1 小时槽位数不符", fps);
        }
    }
}
