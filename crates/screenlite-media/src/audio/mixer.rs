//! 音频混音点：`Vec<GridAdapter>` → 一条流。
//!
//! ```text
//! N 路 AudioSource → 各自 GridAdapter（对齐 20ms 网格 + 每源增益 + 每源指标）
//! → 混音器（等长求和 + 限幅）→ 单路 PCM → AAC → mux
//! ```
//!
//! ## 每源网格适配器
//!
//! 每路源各自用自己的 QPC 位置对齐到共享的 20ms 网格：设备包大小不一（10ms/20ms…），
//! 适配器把它们拼装成等长网格块，缺口用静音补位（每源独立计数）。
//! 块的 srt = 该格首样本的设备时间（随样本数推进）——时间戳不被伪造，
//! 音视频同步就锚在这条设备时钟上。
//!
//! ## 多源 = 等长求和 + 限幅
//!
//! 顺序：每源乘增益 → 求和 → 硬限幅到 i16。不做压缩器/AGC/降噪：录屏的首要属性是
//! "文件与真实一致"。

use crate::audio::pcm_peak;
use crate::consts::SILENCE_GUARD_MS;
use crate::error::MediaResult;
use crate::time::Srt100ns;

use super::source::{AudioSource, AudioSourceKind};
use super::AudioChunk;
use super::AudioFormat;

/// 每源增益上限：线性增益 `gain ∈ [0.0, 2.0]`，默认 1.0。
pub const GAIN_MAX: f32 = 2.0;

pub fn clamp_gain(gain: f32) -> f32 {
    if gain.is_nan() {
        return 1.0;
    }
    gain.clamp(0.0, GAIN_MAX)
}

/// 每源指标：保住 直接依据——
/// `真实设备块 = chunks − filler_blocks`。**只留聚合值的话，混合场景下
/// "一路真实 + 一路全补位"会被聚合值掩盖**，所以这是验收要求，不是锦上添花。
#[derive(Debug, Default, Clone)]
pub struct SourceMetrics {
    pub chunks: u64,
    pub filler_blocks: u64,
    pub dropped_backwards: u64,
    pub peak: u64,
    pub peak_last_sec: u64,
}

impl SourceMetrics {
    pub fn real_blocks(&self) -> u64 {
        self.chunks.saturating_sub(self.filler_blocks)
    }
}

/// 网格槽的三种产出：真实数据 / 静音补位 / 本次还没凑满（下次再问）。
enum Slot {
    Real(AudioChunk),
    Silent(AudioChunk),
    Pending,
}

/// 每源网格适配器：持有一路源，把它的输出拼装成等长网格块。
pub struct GridAdapter {
    pub kind: AudioSourceKind,
    /// 端点友好名（`opened()`），用于每源日志行与"选错设备"诊断
    pub endpoint: String,
    /// 每源线性增益，已夹到 `[0, 2]`
    pub gain: f32,
    source: Box<dyn AudioSource>,
    format: AudioFormat,
    /// 网格槽的帧数（20ms @ 采样率；测试里可以给小值）
    slot_frames: usize,
    /// 已收到但还没凑满一格的样本（交错排列，与 format.channels 一致）
    pending: Vec<i16>,
    /// 下一格的起点 srt（100ns）。首个真实块到达时用它的 srt 初始化——
    /// 之后按消费掉的样本数推进（设备自己的时钟，漂移被如实保留）
    cursor_srt: Option<i64>,
    pub metrics: SourceMetrics,
}

impl GridAdapter {
    pub fn new(
        kind: AudioSourceKind,
        source: Box<dyn AudioSource>,
        gain: f32,
        slot_frames: usize,
    ) -> Self {
        let format = source.format();
        Self {
            kind,
            endpoint: source.opened(),
            gain: clamp_gain(gain),
            source,
            slot_frames: slot_frames.max(1),
            format,
            pending: Vec::new(),
            cursor_srt: None,
            metrics: SourceMetrics::default(),
        }
    }

    fn slot_samples(&self) -> usize {
        self.slot_frames * self.format.channels as usize
    }

    fn slot_duration_100ns(&self) -> i64 {
        self.slot_frames as i64 * 10_000_000 / self.format.sample_rate as i64
    }

    /// 从 `pending` 取出一格（不足补零），乘每源增益，推进游标。
    /// 计数口径：`chunks` = 真实格、`filler_blocks` = 静音格，
    /// `真实设备块 = chunks − filler_blocks`。
    fn emit_slot(&mut self, silent: bool) -> Slot {
        let srt = self.cursor_srt.unwrap_or(0);
        let n = self.slot_samples();
        let mut pcm: Vec<i16> = if silent {
            self.pending.drain(..n.min(self.pending.len())).collect()
        } else {
            self.pending.drain(..n).collect()
        };
        pcm.resize(n, 0);
        if self.gain != 1.0 {
            for v in &mut pcm {
                *v = ((*v as f32) * self.gain).clamp(-32_768.0, 32_767.0) as i16;
            }
        }
        self.cursor_srt = Some(srt + self.slot_duration_100ns());
        // 计数口径：chunks = 全部网格槽（真实 + 补位），filler = 其中静音格，
        // 真实设备块 = chunks − filler_blocks（减法公式才对"断流的那一路"也成立）
        self.metrics.chunks += 1;
        if silent {
            self.metrics.filler_blocks += 1;
        }
        let chunk = AudioChunk {
            pcm,
            frames: self.slot_frames as u32,
            srt: Srt100ns(srt),
        };
        if silent {
            Slot::Silent(chunk)
        } else {
            Slot::Real(chunk)
        }
    }

    /// 拉取这一路在本次的产出：
    /// - 真实块到达 → 攒进 `pending`，凑满一格就产出（乘增益、计数）
    /// - 没有数据且已到「now − 安全边距」→ 产出一格静音（本源自己的补位）
    fn poll_slot(&mut self) -> MediaResult<Slot> {
        if self.pending.len() >= self.slot_samples() {
            return Ok(self.emit_slot(false));
        }
        match self.source.poll()? {
            Some(chunk) => {
                // 本源自己的倒退保护：srt 早于本源游标 → 丢弃并计数（可归因到这一路）
                if let Some(cur) = self.cursor_srt {
                    if chunk.srt.0 < cur {
                        self.metrics.dropped_backwards += 1;
                        return Ok(Slot::Pending);
                    }
                }
                if self.cursor_srt.is_none() {
                    self.cursor_srt = Some(chunk.srt.0);
                }
                let peak = pcm_peak(&chunk.pcm);
                self.metrics.peak = self.metrics.peak.max(peak);
                self.metrics.peak_last_sec = self.metrics.peak_last_sec.max(peak);
                self.pending.extend_from_slice(&chunk.pcm);
                if self.pending.len() >= self.slot_samples() {
                    Ok(self.emit_slot(false))
                } else {
                    Ok(Slot::Pending)
                }
            }
            None => {
                let now = self.source.srt_now()?.0;
                let guard = SILENCE_GUARD_MS as i64 * 10_000;
                if self.cursor_srt.is_none() {
                    // 本源还没有任何真实包（典型：静音的 loopback 一个包都不给）——
                    // 以「now − 安全边距」作为它的时间锚，从这格开始为它补静音并计数。
                    // 这正是"这一路没进来"的证据来源。
                    self.cursor_srt = Some(now - guard);
                }
                if let Some(cur) = self.cursor_srt {
                    if cur + self.slot_duration_100ns() <= now - guard {
                        return Ok(self.emit_slot(true));
                    }
                }
                Ok(Slot::Pending)
            }
        }
    }

    pub fn format(&self) -> AudioFormat {
        self.format
    }

    pub fn stop(&self) {
        self.source.stop();
    }
}

pub struct AudioMixer {
    adapters: Vec<GridAdapter>,
    format: Option<AudioFormat>,
}

impl AudioMixer {
    /// 组装混音器。各路格式必须一致（正常路径由系统转换保证一致；
    /// 这里的拒绝是防御性断言——连回退都失败才会命中）。
    pub fn new(adapters: Vec<GridAdapter>) -> MediaResult<(Self, Vec<String>)> {
        let format = adapters.first().map(|a| a.format());
        for a in &adapters {
            if let (Some(f), g) = (format, a.format()) {
                if (f.sample_rate, f.channels, f.format) != (g.sample_rate, g.channels, g.format) {
                    return Err(crate::error::MediaError::Internal(format!(
                        "音频源格式不一致：{:?} vs {:?}（正常路径由系统转换保证一致；命中此错误 = 连回退都失败了）",
                        f, g
                    )));
                }
            }
        }
        let described = adapters.iter().map(|a| a.endpoint.clone()).collect();
        Ok((Self { adapters, format }, described))
    }

    pub fn is_empty(&self) -> bool {
        self.adapters.is_empty()
    }

    pub fn format(&self) -> Option<AudioFormat> {
        self.format
    }

    pub fn source_count(&self) -> usize {
        self.adapters.len()
    }

    /// 每源指标快照 `(kind, endpoint, metrics)`——音频线程发布进共享 Metrics 用。
    pub fn source_metrics(&self) -> Vec<(AudioSourceKind, String, SourceMetrics)> {
        self.adapters
            .iter()
            .map(|a| (a.kind, a.endpoint.clone(), a.metrics.clone()))
            .collect()
    }

    /// 取各路刚过去这一秒的峰值并把窗口归零（音频线程每秒调用一次）。
    pub fn take_1s_peaks(&mut self) -> Vec<u64> {
        self.adapters
            .iter_mut()
            .map(|a| std::mem::take(&mut a.metrics.peak_last_sec))
            .collect()
    }

    /// 取一块混音后的音频。所有路都还没凑满网格 → None。
    pub fn poll(&mut self) -> MediaResult<Option<AudioChunk>> {
        if self.adapters.is_empty() {
            return Ok(None);
        }
        let mut parts: Vec<(AudioChunk, bool)> = Vec::with_capacity(self.adapters.len());
        for a in &mut self.adapters {
            match a.poll_slot()? {
                Slot::Real(c) => parts.push((c, false)),
                Slot::Silent(c) => parts.push((c, true)),
                Slot::Pending => {}
            }
        }
        if parts.is_empty() {
            return Ok(None);
        }
        let frames = parts.iter().map(|(c, _)| c.frames).min().unwrap_or(0);
        if frames == 0 {
            return Ok(None);
        }
        let channels = self.format.map(|f| f.channels as usize).unwrap_or(1).max(1);
        let n = frames as usize * channels;
        // srt 取最早的格（各路游标都在共享 QPC 域上，差值 = 设备间抖动/漂移，毫秒级）
        let srt = parts.iter().map(|(c, _)| c.srt.0).min().unwrap_or(0);
        let mut acc = vec![0i64; n];
        for (c, _) in &parts {
            for (i, v) in c.pcm.iter().take(n).enumerate() {
                acc[i] += *v as i64;
            }
        }
        let pcm: Vec<i16> = acc
            .iter()
            .copied()
            .map(|v| v.clamp(i16::MIN as i64, i16::MAX as i64) as i16)
            .collect();
        // 合成标记：所有贡献者都是补位 → 这一块整格都是合成的
        let synthetic = parts.iter().all(|(_, s)| *s);
        let _ = synthetic; // 由调用方（音频线程）按每源指标聚合计数；此处仅保留语义注释
        Ok(Some(AudioChunk {
            pcm,
            frames,
            srt: Srt100ns(srt),
        }))
    }

    pub fn stop(&self) {
        for a in &self.adapters {
            a.stop();
        }
    }
}

/// 等长求和 + 限幅：把若干等长块逐样本相加并夹到 i16 范围。
///
/// 返回写入的样本数（= 最短块的样本数）。抽成自由函数是为了能单独测。
pub fn sum_blocks(blocks: &[&[i16]], out: &mut Vec<i16>) -> usize {
    let n = blocks.iter().map(|b| b.len()).min().unwrap_or(0);
    out.clear();
    out.reserve(n);
    for i in 0..n {
        let mut acc = 0i32;
        for b in blocks {
            acc += b[i] as i32;
        }
        out.push(acc.clamp(i16::MIN as i32, i16::MAX as i32) as i16);
    }
    n
}

/// 带增益的求和：每源先乘自己的线性增益，再逐样本相加并夹到 i16。
///
/// `sources[i] = (块, 增益)`。返回写入的样本数（= 最短块的样本数）。
pub fn sum_blocks_with_gain(sources: &[(&[i16], f32)], out: &mut Vec<i16>) -> usize {
    let n = sources.iter().map(|(b, _)| b.len()).min().unwrap_or(0);
    out.clear();
    out.reserve(n);
    for i in 0..n {
        let mut acc = 0f32;
        for (b, g) in sources {
            acc += b[i] as f32 * g;
        }
        out.push(acc.clamp(i16::MIN as f32, i16::MAX as f32) as i16);
    }
    n
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::source::{AudioDirection, AudioSource, AudioSourceKind, EndpointSpec};
    use crate::audio::SampleFormat;
    use crate::time::Srt100ns;

    /// 假源：按脚本产出块；`infinite` 时最后一块无限重复（模拟"一直在产数据的一路"）。
    /// `now` 独立于数据 srt，可由测试推进——补位检查点用的是它（now − guard）。
    struct FakeSource {
        fmt: AudioFormat,
        script: std::cell::RefCell<Vec<Vec<i16>>>,
        infinite: bool,
        srt: std::cell::Cell<i64>,
        now: std::cell::Cell<i64>,
        opened: String,
    }

    impl FakeSource {
        fn new(script: Vec<Vec<i16>>) -> Self {
            Self::with_options(script, false)
        }
        fn with_options(script: Vec<Vec<i16>>, infinite: bool) -> Self {
            Self {
                fmt: AudioFormat {
                    sample_rate: 48_000,
                    channels: 2,
                    format: SampleFormat::Pcm16,
                },
                script: std::cell::RefCell::new(script),
                infinite,
                srt: std::cell::Cell::new(0),
                now: std::cell::Cell::new(0),
                opened: "假源(48k/2ch)".into(),
            }
        }
    }

    impl AudioSource for FakeSource {
        fn start(_spec: &EndpointSpec) -> MediaResult<Self> {
            unreachable!("假源不经 start 构造")
        }
        fn format(&self) -> AudioFormat {
            self.fmt
        }
        fn poll(&self) -> MediaResult<Option<AudioChunk>> {
            // 每次 poll 推进"现在"20ms：真实系统里 QPC 就是这样随轮询前进的
            self.now.set(self.now.get() + 200_000);
            let mut script = self.script.borrow_mut();
            if script.is_empty() {
                if self.infinite {
                    // 无限重复也要推进 srt——否则会被适配器的倒退保护丢弃（真实设备不会这样）
                    let srt = Srt100ns(self.srt.get());
                    self.srt.set(self.srt.get() + 200_000);
                    return Ok(Some(AudioChunk {
                        pcm: vec![100i16; 8],
                        frames: 4,
                        srt,
                    }));
                }
                return Ok(None);
            }
            let pcm = script.remove(0);
            let frames = (pcm.len() / self.fmt.channels as usize) as u32;
            let srt = Srt100ns(self.srt.get());
            self.srt.set(self.srt.get() + 200_000); // 每块 20ms
            Ok(Some(AudioChunk { pcm, frames, srt }))
        }
        fn srt_now(&self) -> MediaResult<Srt100ns> {
            Ok(Srt100ns(self.now.get()))
        }
        fn stop(&self) {}
        fn opened(&self) -> String {
            self.opened.clone()
        }
    }

    fn fmt() -> AudioFormat {
        AudioFormat {
            sample_rate: 48_000,
            channels: 2,
            format: SampleFormat::Pcm16,
        }
    }

    #[test]
    fn grid_slots_preserve_content_and_device_time() {
        // 网格化只改变分块边界，不改变内容与时间轴：4 帧（8 样本）→ 两格（各 2 帧）
        let s = FakeSource::new(vec![vec![100i16, 200, 300, 400, 500, 600, 700, 800]]);
        let (mut mixer, _) = AudioMixer::new(vec![GridAdapter::new(
            AudioSourceKind::SystemLoopback,
            Box::new(s),
            1.0,
            2, // slot_frames = 2 帧
        )])
        .unwrap();
        let a = mixer.poll().unwrap().unwrap();
        assert_eq!(a.pcm, vec![100, 200, 300, 400]);
        assert_eq!(a.srt.0, 0, "格起点 = 首样本的设备时间");
        let b = mixer.poll().unwrap().unwrap();
        assert_eq!(b.pcm, vec![500, 600, 700, 800]);
        assert_eq!(
            b.srt.0, 416,
            "格起点按消费掉的样本数推进（2 帧 @48k = 416.67 → 取整 416；正式 960 帧的格恰为 200_000 整 ✓）"
        );
    }

    #[test]
    fn per_source_gain_applied_before_sum() {
        // 每源乘增益 → 求和。0.5×A + 2.0×B = 0.5×10000 + 2.0×10000 = 25000
        let a = FakeSource::new(vec![vec![10_000i16; 8]]);
        let b = FakeSource::new(vec![vec![10_000i16; 8]]);
        let (mut mixer, _) = AudioMixer::new(vec![
            GridAdapter::new(AudioSourceKind::SystemLoopback, Box::new(a), 0.5, 2),
            GridAdapter::new(AudioSourceKind::Microphone, Box::new(b), 2.0, 2),
        ])
        .unwrap();
        let out = mixer.poll().unwrap().unwrap();
        assert_eq!(out.pcm, vec![25_000i16; 4]);
    }

    #[test]
    fn gain_zero_mutes_source_and_gain_clamped() {
        assert_eq!(clamp_gain(-1.0), 0.0);
        assert_eq!(clamp_gain(5.0), GAIN_MAX);
        assert_eq!(clamp_gain(f32::NAN), 1.0);
        // gain=0 → 该路被静音
        let a = FakeSource::new(vec![vec![10_000i16; 8]]);
        let (mut mixer, _) = AudioMixer::new(vec![GridAdapter::new(
            AudioSourceKind::SystemLoopback,
            Box::new(a),
            0.0,
            2,
        )])
        .unwrap();
        let out = mixer.poll().unwrap().unwrap();
        assert_eq!(out.pcm, vec![0i16; 4]);
    }

    #[test]
    fn two_full_scale_sources_do_not_overflow_i16() {
        // 两路满幅相加必须被限幅夹住，绝不能溢出
        let a = FakeSource::new(vec![vec![30_000i16; 8]]);
        let b = FakeSource::new(vec![vec![30_000i16; 8]]);
        let (mut mixer, _) = AudioMixer::new(vec![
            GridAdapter::new(AudioSourceKind::SystemLoopback, Box::new(a), 2.0, 2),
            GridAdapter::new(AudioSourceKind::Microphone, Box::new(b), 2.0, 2),
        ])
        .unwrap();
        let out = mixer.poll().unwrap().unwrap();
        assert!(out.pcm.iter().all(|v| *v == i16::MAX));
    }

    #[test]
    fn filler_blocks_are_per_source() {
        // 一路有数据、一路断 → 后者 filler 增长、前者为 0（每源指标独立，直接断言）
        let a = FakeSource::with_options(vec![], true); // 无限产数据 → filler 恒 0
        let b = FakeSource::with_options(vec![vec![500i16; 8]], false); // 给一块就断
        let mut ga = GridAdapter::new(AudioSourceKind::SystemLoopback, Box::new(a), 1.0, 2);
        let mut gb = GridAdapter::new(AudioSourceKind::Microphone, Box::new(b), 1.0, 2);
        // A：一直有数据 → 产出真实格，filler 恒 0
        for _ in 0..3 {
            assert!(matches!(ga.poll_slot().unwrap(), Slot::Real(_)));
        }
        assert_eq!(ga.metrics.chunks, 3);
        assert_eq!(ga.metrics.filler_blocks, 0);
        assert_eq!(ga.metrics.real_blocks(), 3);
        // B：给一块建立游标（srt=0），然后断流；随轮询推进"现在"，越过补位线 → 静音格
        assert!(matches!(gb.poll_slot().unwrap(), Slot::Real(_)));
        let mut silent = 0;
        for _ in 0..15 {
            if matches!(gb.poll_slot().unwrap(), Slot::Silent(_)) {
                silent += 1;
            }
        }
        assert!(silent >= 1, "断流后应产出静音格");
        assert!(gb.metrics.filler_blocks >= 1, "B 的 filler 应随断流增长");
        assert_eq!(ga.metrics.filler_blocks, 0, "A 一直有数据，filler 必须为 0");
    }

    #[test]
    fn mismatched_formats_refuse_to_mix_defensively() {
        // 之后这是防御性断言：正常路径由系统转换保证一致，连回退都失败才会命中
        let s1 = FakeSource::new(vec![vec![0i16; 8]]);
        let mut s2 = FakeSource::new(vec![vec![0i16; 8]]);
        s2.fmt.sample_rate = 44_100;
        let r = AudioMixer::new(vec![
            GridAdapter::new(AudioSourceKind::SystemLoopback, Box::new(s1), 1.0, 2),
            GridAdapter::new(AudioSourceKind::Microphone, Box::new(s2), 1.0, 2),
        ]);
        assert!(r.is_err(), "格式不一致必须拒绝混音（防御性），而不是悄悄混出错音");
    }

    #[test]
    fn empty_mixer_is_silent_not_a_panic() {
        let (mut mixer, _) = AudioMixer::new(Vec::new()).unwrap();
        assert!(mixer.is_empty());
        assert!(mixer.poll().unwrap().is_none());
        assert!(mixer.format().is_none());
    }

    #[test]
    fn sum_blocks_with_gain_matches_contract() {
        let a = [10_000i16, -10_000, 30_000, -30_000];
        let b = [10_000i16, -10_000, 30_000, -30_000];
        let mut out = Vec::new();
        let n = sum_blocks_with_gain(&[(&a, 1.0), (&b, 1.0)], &mut out);
        assert_eq!(n, 4);
        assert_eq!(out, vec![20_000i16, -20_000, i16::MAX, i16::MIN]);
        // 增益 0.5：幅度减半
        let n = sum_blocks_with_gain(&[(&a, 0.5), (&b, 0.0)], &mut out);
        assert_eq!(out[..n], vec![5_000i16, -5_000, 15_000, -15_000][..]);
    }

    #[test]
    fn kind_maps_to_endpoint_direction() {
        assert_eq!(AudioSourceKind::SystemLoopback.direction(), AudioDirection::Render);
        assert_eq!(AudioSourceKind::Microphone.direction(), AudioDirection::Capture);
        assert_eq!(
            EndpointSpec::default_of(AudioDirection::Capture).id,
            None,
            "默认设备 = id 为 None（将来按 id 打开只是把 Some(id) 传进来，trait 形状不变）"
        );
        let _ = fmt();
    }
}
