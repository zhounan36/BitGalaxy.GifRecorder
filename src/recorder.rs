//! 录制线程：定时抓帧 → Floyd–Steinberg 抖动量化 → 帧差分 → 流式写 GIF89a。
//! 边录边写临时文件，内存只需两帧索引缓冲；按实际时间戳计算每帧 delay，
//! 编码慢于目标帧率时自动降帧但不缩短 GIF 时长。

use std::fs::{self, File};
use std::io::BufWriter;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::capture::ScreenGrabber;
use crate::gif::{build_palette, crop_diff, diff_bbox, ColorMapper, GifWriter};

/// 录制线程完成时向 UI 线程投递的消息
pub const WM_REC_DONE: u32 = WM_APP + 0x10;

/// HWND 跨线程传递的 Send 安全载体
#[derive(Clone, Copy)]
struct HwndSend(usize);

impl HwndSend {
    fn get(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }
}

/// 单次录制安全上限（10 分钟）
const MAX_DURATION: Duration = Duration::from_secs(600);

#[derive(Clone, Copy)]
pub struct Config {
    /// 虚拟屏绝对坐标 (x, y, w, h)，None = 全部屏幕
    pub region: Option<(i32, i32, i32, i32)>,
    pub fps: u32,
    pub scale_percent: u32,
}

pub enum Cmd {
    Stop,
    Cancel,
}

pub enum Outcome {
    /// 完成，临时文件已含合法 GIF（trailer 已写）
    Saved { tmp: PathBuf, frames: u64, ms: u64 },
    /// 用户取消，临时文件已删除
    Cancelled,
    Failed(String),
}

pub struct Recorder {
    pub tmp: PathBuf,
    pub frames: Arc<AtomicU64>,
    cmd: Option<Sender<Cmd>>,
    join: JoinHandle<Outcome>,
}

impl Recorder {
    pub fn start(cfg: Config, notify: HWND) -> Self {
        let (tx, rx) = channel::<Cmd>();
        let frames = Arc::new(AtomicU64::new(0));
        let ts = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let tmp = std::env::temp_dir().join(format!("bgr_rec_{ts}.gif.tmp"));
        let f = frames.clone();
        let tmp_in = tmp.clone();
        let notify_in = HwndSend(notify.0 as usize);
        let join = std::thread::Builder::new()
            .name("gif-recorder".into())
            .spawn(move || run(cfg, rx, f, notify_in, tmp_in))
            .expect("spawn recorder thread");
        Self {
            tmp,
            frames,
            cmd: Some(tx),
            join,
        }
    }

    pub fn stop(&mut self) {
        if let Some(tx) = self.cmd.take() {
            let _ = tx.send(Cmd::Stop);
        }
    }

    pub fn cancel(&mut self) {
        if let Some(tx) = self.cmd.take() {
            let _ = tx.send(Cmd::Cancel);
        }
    }

    pub fn finish(self) -> Outcome {
        self.join.join().unwrap_or(Outcome::Failed("编码线程异常退出".into()))
    }
}

fn run(
    cfg: Config,
    rx: Receiver<Cmd>,
    frames: Arc<AtomicU64>,
    notify: HwndSend,
    tmp: PathBuf,
) -> Outcome {
    let t0 = Instant::now();
    let result = record(cfg, &rx, &frames, tmp);
    let out = match result {
        Ok(Some(tmp)) => Outcome::Saved {
            tmp,
            frames: frames.load(Ordering::Relaxed),
            ms: t0.elapsed().as_millis() as u64,
        },
        Ok(None) => Outcome::Cancelled,
        Err(e) => Outcome::Failed(e),
    };
    unsafe {
        let _ = PostMessageW(
            Some(notify.get()),
            WM_REC_DONE,
            Default::default(),
            Default::default(),
        );
    }
    out
}

/// Ok(Some(tmp)) = 完成；Ok(None) = 已取消；Err = 失败。
fn record(
    cfg: Config,
    rx: &Receiver<Cmd>,
    frames: &AtomicU64,
    tmp: PathBuf,
) -> Result<Option<PathBuf>, String> {
    let mut grabber =
        ScreenGrabber::new(cfg.region, cfg.scale_percent).map_err(|e| format!("初始化屏幕捕获失败: {e}"))?;
    let (w, h) = (grabber.width, grabber.height);

    let file = File::create(&tmp).map_err(|e| format!("创建临时文件失败: {e}"))?;

    // 首帧：定调色板（255 内容色 + 1 透明槽 = 256 色，8-bit）
    let rgb = grabber.grab().map_err(|e| format!("屏幕抓取失败: {e}"))?.to_vec();
    let mut pal256 = build_palette(&rgb, 255);
    pal256.resize(256, [0, 0, 0]);
    let mut writer = GifWriter::new(BufWriter::new(file), w as u16, h as u16, &pal256, 8)
        .map_err(|e| format!("写 GIF 头失败: {e}"))?;
    let mut mapper = ColorMapper::new(pal256);
    let mut prev = dither_map(&rgb, w, h, &mut mapper);

    let first_delay = (100 / cfg.fps.clamp(1, 50)).max(2) as u16;
    writer
        .write_frame(&prev, 0, 0, w as u16, h as u16, first_delay, false)
        .map_err(|e| format!("写首帧失败: {e}"))?;
    frames.store(1, Ordering::Relaxed);

    // 主循环：固定节奏抓帧，实际间隔累计为 delay（1/100s）
    let interval = Duration::from_millis(1000 / cfg.fps.clamp(1, 50) as u64);
    let mut next_shot = Instant::now() + interval;
    let mut t_prev = Instant::now();
    let mut pending_cs: u32 = 0;
    let t0 = Instant::now();

    loop {
        if let Ok(cmd) = rx.try_recv() {
            return match cmd {
                Cmd::Stop => match writer.finish() {
                    Ok(()) => Ok(Some(tmp)),
                    Err(e) => {
                        let _ = fs::remove_file(&tmp);
                        Err(format!("收尾写入失败: {e}"))
                    }
                },
                Cmd::Cancel => {
                    drop(writer); // 先关闭文件再删除
                    let _ = fs::remove_file(&tmp);
                    Ok(None)
                }
            };
        }

        let now = Instant::now();
        if now < next_shot {
            std::thread::sleep(Duration::from_millis(2));
            continue;
        }
        next_shot += interval;
        if next_shot < now {
            next_shot = now + interval; // 严重欠载时重新对齐，避免连环快进
        }

        let rgb = grabber.grab().map_err(|e| format!("屏幕抓取失败: {e}"))?;
        let cur = dither_map(rgb, w, h, &mut mapper);

        let dt_cs = ((now - t_prev).as_millis() / 10).max(1) as u32;
        t_prev = now;
        pending_cs = (pending_cs + dt_cs).min(600);

        if let Some((bx, by, bw, bh)) = diff_bbox(&cur, &prev, w, h) {
            let cropped = crop_diff(&cur, &prev, w, (bx, by, bw, bh), 255);
            writer
                .write_frame(&cropped, bx as u16, by as u16, bw as u16, bh as u16, pending_cs as u16, true)
                .map_err(|e| format!("写帧失败: {e}"))?;
            pending_cs = 0;
            prev.copy_from_slice(&cur);
            frames.fetch_add(1, Ordering::Relaxed);
        }
        // 无变化帧：不写，delay 累计到下一变化帧

        if t0.elapsed() >= MAX_DURATION {
            return match writer.finish() {
                Ok(()) => Ok(Some(tmp)),
                Err(e) => {
                    let _ = fs::remove_file(&tmp);
                    Err(format!("收尾写入失败: {e}"))
                }
            };
        }
    }
}

/// Floyd–Steinberg 抖动 + 调色板映射。
/// 两行滚动误差缓冲，x 侧加 1 偏移免边界判断。
fn dither_map(rgb: &[u8], w: usize, h: usize, m: &mut ColorMapper) -> Vec<u8> {
    let mut out = vec![0u8; w * h];
    let mut cur = vec![[0i32; 3]; w + 2];
    let mut nxt = vec![[0i32; 3]; w + 2];
    for y in 0..h {
        let row = y * w;
        for x in 0..w {
            let p = (row + x) * 3;
            let i = x + 1;
            let old_r = rgb[p] as i32 + cur[i][0];
            let old_g = rgb[p + 1] as i32 + cur[i][1];
            let old_b = rgb[p + 2] as i32 + cur[i][2];
            let idx = m.map(old_r, old_g, old_b);
            out[row + x] = idx;
            let pc = m.color(idx);
            // 误差以裁剪后的原值为基准
            let er = old_r.clamp(0, 255) - pc[0] as i32;
            let eg = old_g.clamp(0, 255) - pc[1] as i32;
            let eb = old_b.clamp(0, 255) - pc[2] as i32;
            // 7/16 → 右邻；3/16 → 左下；5/16 → 正下；1/16 → 右下
            cur[i + 1][0] += (er * 7) >> 4;
            cur[i + 1][1] += (eg * 7) >> 4;
            cur[i + 1][2] += (eb * 7) >> 4;
            nxt[i - 1][0] += (er * 3) >> 4;
            nxt[i - 1][1] += (eg * 3) >> 4;
            nxt[i - 1][2] += (eb * 3) >> 4;
            nxt[i][0] += (er * 5) >> 4;
            nxt[i][1] += (eg * 5) >> 4;
            nxt[i][2] += (eb * 5) >> 4;
            nxt[i + 1][0] += er >> 4;
            nxt[i + 1][1] += eg >> 4;
            nxt[i + 1][2] += eb >> 4;
        }
        std::mem::swap(&mut cur, &mut nxt);
        nxt.iter_mut().for_each(|v| *v = [0; 3]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dither_map_output_range() {
        // 简单调色板：黑/白/红
        let mut pal = vec![[0u8, 0, 0], [255, 255, 255], [255, 0, 0]];
        pal.resize(256, [0, 0, 0]);
        let mut m = ColorMapper::new(pal);
        let (w, h) = (16, 16);
        let rgb: Vec<u8> = (0..w * h * 3)
            .map(|i| ((i * 31 + i / 7) % 256) as u8)
            .collect();
        let out = dither_map(&rgb, w, h, &mut m);
        assert_eq!(out.len(), w * h);
        assert!(out.iter().all(|&v| v < 3));
    }

    /// 端到端冒烟：真实屏幕抓帧 → 完整编码管线落盘 → 校验 GIF 结构。
    /// 需要交互桌面会话（开发机本地运行）。
    #[test]
    fn test_capture_pipeline_smoke() {
        let mut grabber = match crate::capture::ScreenGrabber::new(Some((0, 0, 320, 240)), 100) {
            Ok(g) => g,
            Err(e) => panic!("ScreenGrabber 初始化失败: {e}"),
        };
        assert_eq!((grabber.width, grabber.height), (320, 240));
        let rgb = grabber.grab().expect("抓帧失败");
        assert_eq!(rgb.len(), 320 * 240 * 3);

        let mut pal256 = build_palette(rgb, 255);
        pal256.resize(256, [0, 0, 0]);
        let mut mapper = ColorMapper::new(pal256.clone());
        let idx = dither_map(rgb, 320, 240, &mut mapper);
        assert_eq!(idx.len(), 320 * 240);
        assert!(idx.iter().all(|&v| v <= 255));

        let path = std::env::temp_dir().join("bgr_smoke_test.gif");
        {
            let f = std::fs::File::create(&path).unwrap();
            let mut w = crate::gif::GifWriter::new(f, 320, 240, &pal256, 8).unwrap();
            w.write_frame(&idx, 0, 0, 320, 240, 4, false).unwrap();
            w.finish().unwrap();
        }
        let bytes = std::fs::read(&path).unwrap();
        assert!(bytes.starts_with(b"GIF89a"), "GIF 头缺失");
        assert_eq!(*bytes.last().unwrap(), 0x3B, "trailer 缺失");
        assert!(bytes.len() > 1000, "帧数据异常小");
        let _ = std::fs::remove_file(&path);
    }
}
