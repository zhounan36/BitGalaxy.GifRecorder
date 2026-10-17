//! 录制线程：定时抓帧 → 帧差分 + RLE 无损压缩 → 流式写帧缓存。
//! 录制结束后由编辑器（editor.rs）预览/剪切/裁切，再经 GIF 编码管线导出。
//! 量化/抖动/差分编码全部移到导出侧，录制路径只做"无损搬运"。

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::Win32::Foundation::HWND;
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_APP};

use crate::cache::FrameCacheWriter;
use crate::capture::ScreenGrabber;

/// 录制线程完成时向 UI 线程投递的消息
pub const WM_REC_DONE: u32 = WM_APP + 0x10;

/// 单次录制安全上限（10 分钟）
const MAX_DURATION: Duration = Duration::from_secs(600);
/// 帧缓存体积上限（超出自动收尾进编辑器，防止磁盘被塞爆）
const CACHE_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// HWND 跨线程传递的 Send 安全载体
#[derive(Clone, Copy)]
struct HwndSend(usize);

impl HwndSend {
    fn get(self) -> HWND {
        HWND(self.0 as *mut core::ffi::c_void)
    }
}

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
    /// 录制完成，帧缓存就绪 → 进入编辑器
    Recorded { cache: PathBuf, frames: u64, ms: u64, truncated: bool },
    /// 用户取消，缓存文件已删除
    Cancelled,
    Failed(String),
}

pub struct Recorder {
    pub cache_path: PathBuf,
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
        let cache = std::env::temp_dir().join(format!("bgr_edit_{ts}.cache"));
        let f = frames.clone();
        let cache_in = cache.clone();
        let notify_in = HwndSend(notify.0 as usize);
        let join = std::thread::Builder::new()
            .name("gif-recorder".into())
            .spawn(move || run(cfg, rx, f, notify_in, cache_in))
            .expect("spawn recorder thread");
        Self { cache_path: cache, frames, cmd: Some(tx), join }
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
        self.join.join().unwrap_or(Outcome::Failed("录制线程异常退出".into()))
    }
}

fn run(
    cfg: Config,
    rx: Receiver<Cmd>,
    frames: Arc<AtomicU64>,
    notify: HwndSend,
    cache: PathBuf,
) -> Outcome {
    let t0 = Instant::now();
    let result = record(cfg, &rx, &frames, cache);
    let out = match result {
        Ok(Some((cache, truncated))) => Outcome::Recorded {
            cache,
            frames: frames.load(Ordering::Relaxed),
            ms: t0.elapsed().as_millis() as u64,
            truncated,
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

/// Ok(Some((cache, 是否因上限截断))) = 完成；Ok(None) = 已取消；Err = 失败。
fn record(
    cfg: Config,
    rx: &Receiver<Cmd>,
    frames: &AtomicU64,
    cache: PathBuf,
) -> Result<Option<(PathBuf, bool)>, String> {
    let mut grabber =
        ScreenGrabber::new(cfg.region, cfg.scale_percent).map_err(|e| format!("初始化屏幕捕获失败: {e}"))?;
    let (w, h) = (grabber.width, grabber.height);

    // 关键帧间隔 ≈ 2 秒（编辑器跳帧解码最多回溯 2 秒）
    let key_interval = (cfg.fps.clamp(1, 50) * 2).clamp(8, 120);
    let mut writer =
        FrameCacheWriter::create(&cache, w, h, key_interval).map_err(|e| format!("创建帧缓存失败: {e}"))?;

    // 首帧（关键帧）
    let rgb = grabber.grab().map_err(|e| format!("屏幕抓取失败: {e}"))?;
    let first_delay = (100 / cfg.fps.clamp(1, 50)).max(2) as u32;
    writer
        .write_frame(&rgb, first_delay)
        .map_err(|e| format!("写帧缓存失败: {e}"))?;
    frames.store(1, Ordering::Relaxed);

    // 主循环：固定节奏抓帧，无变化帧不写、delay 累计到下一变化帧
    let interval = Duration::from_millis(1000 / cfg.fps.clamp(1, 50) as u64);
    let mut next_shot = Instant::now() + interval;
    let mut t_prev = Instant::now();
    let mut pending_cs: u32 = 0;
    let t0 = Instant::now();

    loop {
        if let Ok(cmd) = rx.try_recv() {
            return match cmd {
                Cmd::Stop => match writer.finish() {
                    Ok(()) => Ok(Some((cache, false))),
                    Err(e) => Err(format!("收尾写入失败: {e}")),
                },
                Cmd::Cancel => {
                    drop(writer); // 先关闭文件再删除
                    let _ = std::fs::remove_file(&cache);
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

        let dt_cs = ((now - t_prev).as_millis() / 10).max(1) as u32;
        t_prev = now;
        pending_cs = (pending_cs + dt_cs).min(600);

        if writer
            .write_frame(&rgb, pending_cs)
            .map_err(|e| format!("写帧缓存失败: {e}"))?
        {
            frames.fetch_add(1, Ordering::Relaxed);
            pending_cs = 0;
        }

        // 上限检查：体积或时长，超出即自动收尾进编辑器
        if writer.written_bytes() >= CACHE_MAX_BYTES || t0.elapsed() >= MAX_DURATION {
            return match writer.finish() {
                Ok(()) => Ok(Some((cache, true))),
                Err(e) => Err(format!("收尾写入失败: {e}")),
            };
        }
    }
}
