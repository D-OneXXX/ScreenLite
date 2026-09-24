//! 帧格式契约。
//!
//! WGC 交付 BGRA8；Media Foundation 的 H.264 编码器不接受 BGRA，
//! 其接受的未压缩输入为 I420/IYUV/YV12/NV12/YUY2。
//! 因此必须在管线里显式转换，禁止指望 MF 自动插入 Video Processor MFT。

use crate::error::{MediaError, MediaResult};

/// 录制区域（以采集画幅的物理像素为坐标，原点在显示器左上角）。
///
/// 区域录制在 V0.3 的第一阶段采用「采集整屏 → CPU 裁剪」：WGC 的 capture item
/// 始终是整块显示器，我们只转换区域内的像素。代价是 GPU→CPU 回读仍是整屏，
/// 但转换开销与区域面积成正比；GPU 端裁剪（Crop/Copy）是后续优化。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// 把区域裁剪到画幅内，并把宽高收敛到偶数（NV12 是 4:2:0 子采样）。
    /// 返回 None 表示裁剪后没有有效区域（完全在画幅外或尺寸为 0）。
    pub fn clamp_to(&self, frame_w: u32, frame_h: u32) -> Option<Region> {
        if frame_w == 0 || frame_h == 0 {
            return None;
        }
        let x = self.x.min(frame_w.saturating_sub(1));
        let y = self.y.min(frame_h.saturating_sub(1));
        let max_w = frame_w - x;
        let max_h = frame_h - y;
        let (w, h) = even_dimensions(self.width.min(max_w), self.height.min(max_h));
        if w == 0 || h == 0 {
            return None;
        }
        Some(Region {
            x,
            y,
            width: w,
            height: h,
        })
    }

    pub fn is_full_frame(&self, frame_w: u32, frame_h: u32) -> bool {
        self.x == 0 && self.y == 0 && self.width == frame_w && self.height == frame_h
    }

    /// 由归一化选区（0..1，相对显示器左上角）换算为物理像素区域。
    ///
    /// 这是框选覆盖层与媒体管线之间的唯一换算点。刻意让前端只传 0..1 的比例：
    /// 前端不必知道 DPI 感知状态与缩放系数，也就不会出现"逻辑像素当物理像素用"
    /// 这类错误。
    ///
    /// 入参 `(nx, ny)` 是左上角、`(nw, nh)` 是右下角（同为 0..1）。
    /// 返回 None 表示选区退化（宽或高不足 2 像素）。
    pub fn from_normalized(
        nx: f64,
        ny: f64,
        nw: f64,
        nh: f64,
        phys_w: u32,
        phys_h: u32,
    ) -> Option<Region> {
        if phys_w == 0 || phys_h == 0 {
            return None;
        }
        let c = |v: f64| v.clamp(0.0, 1.0);
        let left = c(nx).min(c(nw));
        let right = c(nx).max(c(nw));
        let top = c(ny).min(c(nh));
        let bottom = c(ny).max(c(nh));

        let x = (left * phys_w as f64).round() as u32;
        let y = (top * phys_h as f64).round() as u32;
        let w = ((right - left) * phys_w as f64).round() as u32;
        let h = ((bottom - top) * phys_h as f64).round() as u32;

        Region { x, y, width: w, height: h }.clamp_to(phys_w, phys_h)
    }
}

/// BGRA8 只读视图。
#[derive(Debug, Clone, Copy)]
pub struct BgraImageView<'a> {
    pub data: &'a [u8],
    /// 行跨度，必须来自 `D3D11_MAPPED_SUBRESOURCE.RowPitch`，通常是 256 字节对齐。
    /// 用 `width * 4` 代替它会产生斜纹画面。
    pub pitch: usize,
    pub width: u32,
    pub height: u32,
}

/// NV12 图像：Y 平面 + 交错 UV 平面，top-down。
#[derive(Debug, Clone)]
pub struct Nv12Image {
    pub data: Vec<u8>,
    /// 本缓冲的行跨度。写入 MF 2D buffer 时必须改用 MF 报告的 pitch。
    pub stride: usize,
    pub width: u32,
    pub height: u32,
}

/// 把奇数尺寸裁剪到偶数（NV12 是 4:2:0 子采样）。
pub fn even_dimensions(width: u32, height: u32) -> (u32, u32) {
    (width & !1, height & !1)
}

#[inline]
fn rgb_to_y(r: i32, g: i32, b: i32) -> u8 {
    (((47 * r + 157 * g + 16 * b + 128) >> 8) + 16).clamp(16, 235) as u8
}

#[inline]
fn rgb_to_cb(r: i32, g: i32, b: i32) -> u8 {
    (((-26 * r - 86 * g + 112 * b + 128) >> 8) + 128).clamp(16, 240) as u8
}

#[inline]
fn rgb_to_cr(r: i32, g: i32, b: i32) -> u8 {
    (((112 * r - 102 * g - 10 * b + 128) >> 8) + 128).clamp(16, 240) as u8
}

/// BGRA8 → NV12，BT.709 系数 + limited range（16–235 / 16–240）。
///
/// 色度取样：对 2×2 块的 RGB 先平均再算色度（与"先算色度再平均"在舍入上可能差 1，
/// 本实现固定为前者，并有单测钉住）。
pub fn bgra_to_nv12(view: &BgraImageView<'_>) -> MediaResult<Nv12Image> {
    let full = Region {
        x: 0,
        y: 0,
        width: view.width,
        height: view.height,
    };
    bgra_to_nv12_region(view, full)
}

/// 与 [`bgra_to_nv12`] 相同，但只转换 `region` 指定的子矩形（区域录制用）。
pub fn bgra_to_nv12_region(view: &BgraImageView<'_>, region: Region) -> MediaResult<Nv12Image> {
    let w = region.width as usize;
    let h = region.height as usize;
    let x0 = region.x as usize;
    let y0 = region.y as usize;

    if w == 0 || h == 0 {
        return Err(MediaError::FrameConversionFailed {
            reason: format!("尺寸非法：{}x{}", w, h),
        });
    }
    if w % 2 != 0 || h % 2 != 0 {
        return Err(MediaError::FrameConversionFailed {
            reason: format!("NV12 需要偶数宽高，实际 {}x{}（调用方需先裁剪）", w, h),
        });
    }
    if x0 + w > view.width as usize || y0 + h > view.height as usize {
        return Err(MediaError::FrameConversionFailed {
            reason: format!(
                "区域 {}x{}+({},{}) 超出画幅 {}x{}",
                w, h, x0, y0, view.width, view.height
            ),
        });
    }
    let min_pitch = w * 4;
    if view.pitch < view.width as usize * 4 {
        return Err(MediaError::FrameConversionFailed {
            reason: format!(
                "pitch {} 小于整幅宽度所需 {}",
                view.pitch,
                view.width as usize * 4
            ),
        });
    }
    // 区域最右下角所需的字节数
    let needed = (y0 + h - 1) * view.pitch + (x0 + w) * 4;
    if view.data.len() < needed {
        return Err(MediaError::FrameConversionFailed {
            reason: format!("缓冲不足：需要 {} 字节，实际 {}", needed, view.data.len()),
        });
    }
    let _ = min_pitch;

    let mut data = vec![0u8; w * h * 3 / 2];
    let (y_plane, uv_plane) = data.split_at_mut(w * h);

    // Y 平面
    for y in 0..h {
        let row_start = (y0 + y) * view.pitch + x0 * 4;
        let row = &view.data[row_start..row_start + w * 4];
        let dst = &mut y_plane[y * w..(y + 1) * w];
        for (x, d) in dst.iter_mut().enumerate() {
            let px = &row[x * 4..x * 4 + 4];
            // BGRA 内存序：B, G, R, A —— alpha 不参与转换
            *d = rgb_to_y(px[2] as i32, px[1] as i32, px[0] as i32);
        }
    }

    // UV 平面（交错 Cb, Cr）：对区域内的 2×2 块取平均
    for j in 0..h / 2 {
        let dst = &mut uv_plane[j * w..(j + 1) * w];
        for i in 0..w / 2 {
            let (mut r, mut g, mut b) = (0i32, 0i32, 0i32);
            for dy in 0..2 {
                let row_start = (y0 + 2 * j + dy) * view.pitch + x0 * 4;
                let row = &view.data[row_start..];
                for dx in 0..2 {
                    let px = &row[(2 * i + dx) * 4..(2 * i + dx) * 4 + 4];
                    b += px[0] as i32;
                    g += px[1] as i32;
                    r += px[2] as i32;
                }
            }
            let (r, g, b) = (r / 4, g / 4, b / 4);
            dst[i * 2] = rgb_to_cb(r, g, b);
            dst[i * 2 + 1] = rgb_to_cr(r, g, b);
        }
    }

    Ok(Nv12Image {
        data,
        stride: w,
        width: region.width,
        height: region.height,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bgra(pixels: &[(u8, u8, u8)]) -> Vec<u8> {
        let mut v = Vec::with_capacity(pixels.len() * 4);
        for &(r, g, b) in pixels {
            v.extend_from_slice(&[b, g, r, 255]);
        }
        v
    }

    #[test]
    fn test_bgra_to_nv12_2x2_known_values() {
        // 纯红 (255,0,0)：
        // Y = ((47*255 + 128) >> 8) + 16 = (12113 >> 8) + 16 = 47 + 16 = 63
        // Cb = ((-26*255 + 128) >> 8) + 128 = (-6502 >> 8) + 128 = -26 + 128 = 102
        // Cr = ((112*255 + 128) >> 8) + 128 = (28688 >> 8) + 128 = 112 + 128 = 240
        let px = bgra(&[(255, 0, 0); 4]);
        let view = BgraImageView { data: &px, pitch: 8, width: 2, height: 2 };
        let nv12 = bgra_to_nv12(&view).unwrap();
        assert_eq!(nv12.stride, 2);
        assert_eq!(&nv12.data[0..4], &[63, 63, 63, 63]);
        assert_eq!(&nv12.data[4..6], &[102, 240]);
    }

    #[test]
    fn test_bgra_to_nv12_4x4_black_and_white() {
        let black = bgra(&[(0, 0, 0); 16]);
        let v = BgraImageView { data: &black, pitch: 16, width: 4, height: 4 };
        let nv12 = bgra_to_nv12(&v).unwrap();
        assert!(nv12.data[0..16].iter().all(|&y| y == 16));
        assert!(nv12.data[16..24].iter().all(|&c| c == 128));

        // 纯白 → Y = 235（limited range 上限），色度中性
        let white = bgra(&[(255, 255, 255); 16]);
        let v = BgraImageView { data: &white, pitch: 16, width: 4, height: 4 };
        let nv12 = bgra_to_nv12(&v).unwrap();
        assert!(nv12.data[0..16].iter().all(|&y| y == 235));
        assert!(nv12.data[16..24].iter().all(|&c| c == 128));
    }

    #[test]
    fn test_bgra_to_nv12_rowpitch_greater_than_width() {
        // 真实 D3D11 staging 纹理的 RowPitch 几乎总是大于 width*4（256 字节对齐）。
        // 必须用 pitch 跳行，而不是 width*4。
        let w = 4u32;
        let mut padded = Vec::new();
        let row_red = bgra(&[(255, 0, 0); 4]);
        for _ in 0..4 {
            padded.extend_from_slice(&row_red);
            padded.extend_from_slice(&[0xAB; 12]); // padding 垃圾数据，必须被忽略
        }
        let view = BgraImageView { data: &padded, pitch: 28, width: w, height: 4 };
        let nv12 = bgra_to_nv12(&view).unwrap();
        assert!(nv12.data[0..16].iter().all(|&y| y == 63));
        // 色度按 2x2 块为 (102,240) 重复
        assert_eq!(
            &nv12.data[16..24],
            &[102, 240, 102, 240, 102, 240, 102, 240]
        );
    }

    #[test]
    fn test_bgra_to_nv12_odd_size_cropped() {
        assert_eq!(even_dimensions(1921, 1081), (1920, 1080));
        assert_eq!(even_dimensions(1920, 1080), (1920, 1080));

        // 奇数尺寸直接转换必须报错，而不是产出错位图像
        let px = bgra(&[(0, 0, 0); 9]);
        let view = BgraImageView { data: &px, pitch: 12, width: 3, height: 3 };
        let err = bgra_to_nv12(&view).unwrap_err();
        assert_eq!(err.code(), "FrameConversionFailed");
    }

    #[test]
    fn test_pitch_smaller_than_width_is_rejected() {
        let px = bgra(&[(0, 0, 0); 4]);
        let view = BgraImageView { data: &px, pitch: 4, width: 2, height: 2 };
        assert!(bgra_to_nv12(&view).is_err());
    }

    #[test]
    fn test_short_buffer_is_rejected() {
        let px = bgra(&[(0, 0, 0); 4]);
        let view = BgraImageView { data: &px[..8], pitch: 8, width: 2, height: 2 };
        assert!(bgra_to_nv12(&view).is_err());
    }

    /// 区域裁剪必须真正按偏移读取，而不是"总是读左上角"。
    /// 4x4 图：上两行纯红、下两行纯蓝。
    /// 裁剪 (0,0,2,2) 应得红；裁剪 (2,2,2,2) 应得蓝。偏移算错会读到相反的结果。
    #[test]
    fn test_region_crop_reads_correct_offsets() {
        let mut px = Vec::new();
        for _ in 0..2 {
            px.extend_from_slice(&bgra(&[(255, 0, 0); 4])); // 纯红两行
        }
        for _ in 0..2 {
            px.extend_from_slice(&bgra(&[(0, 0, 255); 4])); // 纯蓝两行
        }
        let view = BgraImageView { data: &px, pitch: 16, width: 4, height: 4 };

        let red = bgra_to_nv12_region(&view, Region { x: 0, y: 0, width: 2, height: 2 })
            .expect("裁剪左上角");
        assert_eq!(red.width, 2);
        assert_eq!(red.data[0], 63, "左上角应为红：Y=63");
        assert_eq!(&red.data[4..6], &[102, 240], "左上角应为红：Cb/Cr=102/240");

        let blue = bgra_to_nv12_region(&view, Region { x: 2, y: 2, width: 2, height: 2 })
            .expect("裁剪右下角");
        assert_eq!(blue.data[0], 32, "右下角应为蓝：Y=32");
        assert_eq!(&blue.data[4..6], &[240, 118], "右下角应为蓝：Cb/Cr=240/118");

        // 整幅转换必须与"全画幅区域"一致
        let full = bgra_to_nv12(&view).unwrap();
        let explicit = bgra_to_nv12_region(
            &view,
            Region { x: 0, y: 0, width: 4, height: 4 },
        )
        .unwrap();
        assert_eq!(full.data, explicit.data);
        assert_eq!(full.width, 4);
    }

    /// 区域超出画幅必须报错，而不是读到别的行（那会产生错位画面）
    #[test]
    fn test_region_out_of_bounds_is_rejected() {
        let px = bgra(&[(0, 0, 0); 16]);
        let view = BgraImageView { data: &px, pitch: 16, width: 4, height: 4 };
        let err = bgra_to_nv12_region(&view, Region { x: 2, y: 0, width: 4, height: 2 })
            .unwrap_err();
        assert_eq!(err.code(), "FrameConversionFailed");
        // 奇数宽高也拒绝
        assert!(bgra_to_nv12_region(&view, Region { x: 0, y: 0, width: 3, height: 2 }).is_err());
    }

    #[test]
    fn test_region_clamp_to_frame_and_even() {        // 画幅内：原样返回
        let r = Region { x: 100, y: 100, width: 200, height: 100 };
        assert_eq!(r.clamp_to(2560, 1600), Some(r));

        // 贴边：宽高被裁到画幅内
        let r = Region { x: 2500, y: 1500, width: 200, height: 200 };
        assert_eq!(
            r.clamp_to(2560, 1600),
            Some(Region { x: 2500, y: 1500, width: 60, height: 100 })
        );

        // 奇数尺寸收敛到偶数
        let r = Region { x: 0, y: 0, width: 101, height: 101 };
        assert_eq!(
            r.clamp_to(2560, 1600),
            Some(Region { x: 0, y: 0, width: 100, height: 100 })
        );

        // 完全在画幅外（只剩 1 像素宽，收敛后为 0）→ None
        let r = Region { x: 3000, y: 0, width: 10, height: 10 };
        assert_eq!(r.clamp_to(2560, 1600), None);

        // 整幅判定
        assert!(Region { x: 0, y: 0, width: 2560, height: 1600 }.is_full_frame(2560, 1600));
        assert!(!Region { x: 1, y: 0, width: 2559, height: 1600 }.is_full_frame(2560, 1600));
    }

    /// 框选的归一化坐标 → 物理像素。这是前端与媒体管线之间唯一的换算点，
    /// 必须精确且有边界保护。
    #[test]
    fn test_region_from_normalized() {
        // 正中间的一半：0.25..0.75 → 2560 的 25%~75% = 640..1920，宽 1280
        assert_eq!(
            Region::from_normalized(0.25, 0.25, 0.75, 0.75, 2560, 1600),
            Some(Region { x: 640, y: 400, width: 1280, height: 800 })
        );

        // 反向拖拽（从右下往左上）必须得到同样的结果
        assert_eq!(
            Region::from_normalized(0.75, 0.75, 0.25, 0.25, 2560, 1600),
            Some(Region { x: 640, y: 400, width: 1280, height: 800 })
        );

        // 整屏
        assert_eq!(
            Region::from_normalized(0.0, 0.0, 1.0, 1.0, 2560, 1600),
            Some(Region { x: 0, y: 0, width: 2560, height: 1600 })
        );

        // 超出边界（拖到窗口外）必须被夹住而不是溢出
        assert_eq!(
            Region::from_normalized(-0.5, -0.5, 1.5, 1.5, 2560, 1600),
            Some(Region { x: 0, y: 0, width: 2560, height: 1600 })
        );

        // 贴右下角：900..1000 → 物理 2304..2560（宽 256，even 已满足）
        assert_eq!(
            Region::from_normalized(0.9, 0.9, 1.0, 1.0, 2560, 1600),
            Some(Region { x: 2304, y: 1440, width: 256, height: 160 })
        );

        // 退化选区（不足 2 像素宽高）→ None
        assert_eq!(
            Region::from_normalized(0.5, 0.5, 0.5, 0.5, 2560, 1600),
            None
        );
        // 画幅为 0 → None
        assert_eq!(Region::from_normalized(0.0, 0.0, 1.0, 1.0, 0, 0), None);
    }
}
