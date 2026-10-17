//! GIF 导出：帧缓存（无损帧流）→ 裁切/裁剪 → 八叉树量化 + Floyd–Steinberg 抖动
//! → 帧差分 → `gif` crate 容器编码。GIF 容器与 LZW 交给成熟的 gif crate。

use std::borrow::Cow;
use std::fs::File;
use std::io::BufWriter;
use std::path::Path;

use crate::cache::FrameCache;
use crate::gif::{build_palette, crop_diff, diff_bbox, dither_map, ColorMapper};

/// 从帧缓存导出 GIF。
/// `sel` 为帧区间 [start, end]（含端点）；`crop` 为帧坐标 (x, y, w, h)。
/// `target_cs` 为目标时长（1/100 秒）：变速导出，选中区间被压缩/拉伸到该时长
/// （加速到单帧低于 GIF 最小帧延时会自动抽帧）；None = 原速导出。
/// `prog(done, total)` 每处理一帧回调一次。
pub fn export_gif<F: FnMut(usize, usize)>(
    cache: &FrameCache,
    out: &Path,
    sel: (usize, usize),
    crop: Option<(u32, u32, u32, u32)>,
    target_cs: Option<u32>,
    mut prog: F,
) -> Result<(), String> {
    let n = cache.len();
    let sel = (sel.0.min(n - 1), sel.1.min(n - 1).max(sel.0));
    let total = sel.1 - sel.0 + 1;
    let (cw, ch) = match crop {
        Some((_, _, w, h)) => (w as usize, h as usize),
        None => (cache.w, cache.h),
    };

    // 变速系数 = 目标时长 / 选中区间实际时长（< 1 加速，> 1 减速）
    let scale = match target_cs {
        Some(t) => {
            let sel_cs = (cache.time_ms_before(sel.1 + 1) - cache.time_ms_before(sel.0)) / 10;
            if sel_cs == 0 || t == 0 { 1.0 } else { t as f64 / sel_cs as f64 }
        }
        None => 1.0,
    };
    // 变速模式下每帧最小输出延时 2cs（GIF 实际下限），不足则抽帧累计；
    // 原速模式保持旧行为：变化帧立即写出。
    let emit_min: f64 = if target_cs.is_some() { 2.0 } else { 0.0 };

    // 调色板：首帧（裁切后）的 255 内容色 + 1 透明槽
    let mut first = Vec::new();
    cache.decode_into(sel.0, &mut first).map_err(|e| format!("解码首帧失败: {e}"))?;
    let first_crop = crop_buf(&first, cache.w, crop);
    let mut pal = build_palette(&first_crop, 255);
    pal.resize(256, [0, 0, 0]);
    let flat: Vec<u8> = pal.iter().flat_map(|c| c.iter().copied()).collect();

    let file = File::create(out).map_err(|e| format!("创建文件失败: {e}"))?;
    let mut enc = gif::Encoder::new(BufWriter::new(file), cw as u16, ch as u16, &flat)
        .map_err(|e| format!("初始化 GIF 编码器失败: {e}"))?;
    enc.set_repeat(gif::Repeat::Infinite).map_err(|e| format!("设置循环失败: {e}"))?;

    let mut mapper = ColorMapper::new(pal);
    let mut prev_idx: Vec<u8> = Vec::new();
    let mut pending_f = 0f64; // 帧间累计延时（1/100 秒，已按变速系数缩放）
    let mut seen = 0usize;
    let mut done = 0usize;
    let mut err: Option<String> = None;

    let res = cache.decode_range(sel.0, sel.1 + 1, |rgb, delay| {
        if err.is_some() {
            return;
        }
        seen += 1;
        let is_last = seen == total;
        pending_f += delay as f64 * scale;

        // 高速缩时：累计延时不足一个最小帧延时（2cs）的帧直接丢弃，
        // 延时滚入后续帧（相当于抽帧/延时摄影）。
        if !prev_idx.is_empty() && !is_last && pending_f < emit_min {
            done += 1;
            prog(done, total);
            return;
        }

        let frame: Cow<[u8]> = match crop {
            Some(c) => Cow::Owned(crop_buf(rgb, cache.w, Some(c))),
            None => Cow::Borrowed(rgb),
        };
        let cur = dither_map(&frame, cw, ch, &mut mapper);

        let changed = prev_idx.is_empty() || diff_bbox(&cur, &prev_idx, cw, ch).is_some();
        let d = (pending_f + 0.5) as u32;
        let write: Result<(), gif::EncodingError> = if prev_idx.is_empty() {
            // 首帧：全量写
            let mut f = gif::Frame::from_indexed_pixels(cw as u16, ch as u16, cur.clone(), None);
            f.delay = d.clamp(2, 600) as u16;
            enc.write_frame(&f)
        } else if let Some((bx, by, bw, bh)) = diff_bbox(&cur, &prev_idx, cw, ch) {
            let patch = crop_diff(&cur, &prev_idx, cw, (bx, by, bw, bh), 255);
            let mut f = gif::Frame::from_indexed_pixels(bw as u16, bh as u16, patch, Some(255));
            f.left = bx as u16;
            f.top = by as u16;
            f.delay = d.clamp(2, 600) as u16;
            f.dispose = gif::DisposalMethod::Keep;
            enc.write_frame(&f)
        } else {
            Ok(()) // 无变化：不写帧，delay 累计到下一变化帧
        };
        match write {
            Ok(()) => {
                // 已写入 → 更新上一帧并清零累计；无变化帧保持累计
                if changed {
                    prev_idx = cur;
                    pending_f = 0.0;
                }
                done += 1;
                prog(done, total);
            }
            Err(e) => err = Some(format!("写 GIF 帧失败: {e}")),
        }
    });
    res.map_err(|e| format!("解码帧缓存失败: {e}"))?;
    if let Some(e) = err {
        let _ = std::fs::remove_file(out);
        return Err(e);
    }
    drop(enc); // Encoder Drop 时写 trailer
    Ok(())
}

/// 裁出一块 RGB（帧坐标）
fn crop_buf(rgb: &[u8], src_w: usize, crop: Option<(u32, u32, u32, u32)>) -> Vec<u8> {
    let Some((x, y, w, h)) = crop else {
        return rgb.to_vec();
    };
    let (x, y, w, h) = (x as usize, y as usize, w as usize, h as usize);
    let mut out = Vec::with_capacity(w * h * 3);
    for row in 0..h {
        let s = (y + row) * src_w * 3 + x * 3;
        out.extend_from_slice(&rgb[s..s + w * 3]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::FrameCacheWriter;

    fn gradient(w: usize, h: usize, seed: u8) -> Vec<u8> {
        (0..w * h)
            .flat_map(|i| {
                let v = ((i * 7 + seed as usize) % 256) as u8;
                [v, 255 - v, (v / 2) + seed]
            })
            .collect()
    }

    /// 端到端：写缓存 → 导出 → 用 gif::Decoder 解码校验帧数/尺寸/延迟。
    #[test]
    fn test_export_roundtrip() {
        let (w, h) = (48, 32);
        let f0 = gradient(w, h, 0);
        let mut f1 = f0.clone();
        for i in 0..w * h / 4 {
            let p = i * 3;
            f1[p] = 250;
            f1[p + 1] = 20;
            f1[p + 2] = 20;
        }
        let path = std::env::temp_dir().join("bgr_export_test.cache");
        let gif_path = std::env::temp_dir().join("bgr_export_test.gif");

        let mut wr = FrameCacheWriter::create(&path, w, h, 30).unwrap();
        wr.write_frame(&f0, 5).unwrap();
        wr.write_frame(&f1, 5).unwrap();
        wr.write_frame(&f1, 5).unwrap(); // 无变化，不写
        wr.finish().unwrap();

        let cache = FrameCache::open(&path).unwrap();
        assert_eq!(cache.len(), 2);
        let mut prog_calls = 0;
        export_gif(&cache, &gif_path, (0, 1), None, None, |d, t| {
            assert!(d <= t);
            prog_calls += 1;
        })
        .unwrap();
        assert_eq!(prog_calls, 2);

        // 解码校验（gif 0.14：Decoder::new 自带读取头）
        let file = File::open(&gif_path).unwrap();
        let mut dec = gif::Decoder::new(std::io::BufReader::new(file)).unwrap();
        assert_eq!(dec.width(), w as u16);
        assert_eq!(dec.height(), h as u16);
        let mut frames = 0usize;
        let mut delays = Vec::new();
        while let Some(f) = dec.read_next_frame().unwrap() {
            frames += 1;
            delays.push(f.delay);
        }
        // 第 3 帧（与第 2 帧相同）在缓存层就未存储，导出为 2 帧
        assert_eq!(frames, 2, "无变化帧应被合并");
        assert_eq!(delays, vec![5, 5]);

        // 裁切导出
        export_gif(&cache, &gif_path, (0, 1), Some((8, 4, 32, 24)), None, |_, _| {}).unwrap();
        let file = File::open(&gif_path).unwrap();
        let mut dec = gif::Decoder::new(std::io::BufReader::new(file)).unwrap();
        assert_eq!((dec.width(), dec.height()), (32, 24));
        assert!(dec.read_next_frame().unwrap().is_some());

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&gif_path);
    }

    /// 变速导出：目标时长（target_cs）应把选中区间压/拉到目标附近；
    /// 极端加速（单帧 < 2cs）时自动抽帧。
    #[test]
    fn test_export_target_duration() {
        let (w, h) = (32, 24);
        let path = std::env::temp_dir().join("bgr_target_test.cache");
        let gif_path = std::env::temp_dir().join("bgr_target_test.gif");
        // 6 帧各 20cs，共 120cs，每帧内容不同（种子取小值避免 helper 溢出）
        let mut wr = FrameCacheWriter::create(&path, w, h, 30).unwrap();
        for k in 0..6u8 {
            wr.write_frame(&gradient(w, h, k * 17), 20).unwrap();
        }
        wr.finish().unwrap();
        let cache = FrameCache::open(&path).unwrap();

        let read_delays = |p: &std::path::Path| -> Vec<u16> {
            let file = File::open(p).unwrap();
            let mut dec = gif::Decoder::new(std::io::BufReader::new(file)).unwrap();
            let mut v = Vec::new();
            while let Some(f) = dec.read_next_frame().unwrap() {
                v.push(f.delay);
            }
            v
        };

        // 温和加速 120cs → 60cs：单帧缩放后 10cs ≥ 2，全部保留
        export_gif(&cache, &gif_path, (0, 5), None, Some(60), |_, _| {}).unwrap();
        let ds = read_delays(&gif_path);
        assert_eq!(ds.len(), 6);
        let sum: u32 = ds.iter().map(|&d| d as u32).sum();
        assert!((50..=70).contains(&sum), "sum={sum}");

        // 极端加速 120cs → 12cs：单帧缩放后 2cs，全部保留
        export_gif(&cache, &gif_path, (0, 5), None, Some(12), |_, _| {}).unwrap();
        let ds = read_delays(&gif_path);
        assert_eq!(ds.len(), 6);
        let sum: u32 = ds.iter().map(|&d| d as u32).sum();
        assert!((10..=14).contains(&sum), "sum={sum}");

        // 极端加速 120cs → 6cs：单帧缩放后 1cs < 2cs 下限 → 抽帧
        export_gif(&cache, &gif_path, (0, 5), None, Some(6), |_, _| {}).unwrap();
        let ds = read_delays(&gif_path);
        assert!(ds.len() < 6, "应抽帧: {ds:?}");
        let sum: u32 = ds.iter().map(|&d| d as u32).sum();
        assert!((5..=9).contains(&sum), "sum={sum}");

        // 减速 120cs → 240cs
        export_gif(&cache, &gif_path, (0, 5), None, Some(240), |_, _| {}).unwrap();
        let ds = read_delays(&gif_path);
        assert_eq!(ds.len(), 6);
        let sum: u32 = ds.iter().map(|&d| d as u32).sum();
        assert!((220..=260).contains(&sum), "sum={sum}");

        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&gif_path);
    }
}
