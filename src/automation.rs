//! AI/自动化无 GUI 核心：录屏直出 GIF、单帧截图、显示器枚举、开始/停止会话。
//! 供 CLI（cli.rs）与 MCP 服务器（mcp.rs）共用，完全复用 GUI 的录制/导出管线。

use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use windows::core::BOOL;
use windows::Win32::Foundation::{LPARAM, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITORINFO, MONITORINFOEXW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, MONITORINFOF_PRIMARY, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
};

use crate::cache::{FrameCache, FrameCacheWriter};
use crate::capture::ScreenGrabber;
use crate::export::export_gif;
use crate::json::Json;
use crate::recorder::{Config, Outcome, Recorder};

// ============================================================
// 显示器枚举（region 坐标系参考）
// ============================================================

pub struct Monitor {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
    pub primary: bool,
    pub name: String,
}

/// 虚拟屏包围盒 (x, y, w, h)，物理像素（进程为 PerMonitorV2 DPI 感知）
pub fn virtual_screen() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

pub fn monitors() -> Vec<Monitor> {
    unsafe {
        let mut list: Vec<Monitor> = Vec::new();
        let p = &mut list as *mut Vec<Monitor>;
        let ok = EnumDisplayMonitors(None, None, Some(enum_cb), LPARAM(p as isize));
        if ok.as_bool() {
            list
        } else {
            Vec::new()
        }
    }
}

unsafe extern "system" fn enum_cb(hmon: HMONITOR, _dc: HDC, rc: *mut RECT, lp: LPARAM) -> BOOL {
    unsafe {
        let r = &*rc;
        let mut info = MONITORINFOEXW::default();
        info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
        let (primary, name) = if GetMonitorInfoW(
            hmon,
            &mut info as *mut MONITORINFOEXW as *mut MONITORINFO,
        )
        .as_bool()
        {
            let n = info
                .szDevice
                .iter()
                .position(|&c| c == 0)
                .unwrap_or(info.szDevice.len());
            (
                info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
                String::from_utf16_lossy(&info.szDevice[..n]),
            )
        } else {
            (false, String::new())
        };
        let v = &mut *(lp.0 as *mut Vec<Monitor>);
        v.push(Monitor {
            x: r.left,
            y: r.top,
            w: r.right - r.left,
            h: r.bottom - r.top,
            primary,
            name,
        });
        BOOL(1)
    }
}

pub fn screens_json() -> Json {
    let (vx, vy, vw, vh) = virtual_screen();
    let mons: Vec<Json> = monitors()
        .into_iter()
        .map(|m| {
            Json::obj(vec![
                ("x", Json::i(m.x as i64)),
                ("y", Json::i(m.y as i64)),
                ("w", Json::i(m.w as i64)),
                ("h", Json::i(m.h as i64)),
                ("primary", Json::Bool(m.primary)),
                ("name", Json::s(m.name)),
            ])
        })
        .collect();
    Json::obj(vec![
        ("ok", Json::Bool(true)),
        (
            "virtual_screen",
            Json::arr(vec![
                Json::i(vx as i64),
                Json::i(vy as i64),
                Json::i(vw as i64),
                Json::i(vh as i64),
            ]),
        ),
        ("monitors", Json::arr(mons)),
    ])
}

// ============================================================
// 配置组装
// ============================================================

/// 校验并组装录制配置（None 用默认值：fps 15 / scale 100 / 全虚拟屏）
pub fn build_config(
    region: Option<(i32, i32, i32, i32)>,
    fps: Option<u32>,
    scale_percent: Option<u32>,
) -> Result<Config, String> {
    if let Some((_x, _y, w, h)) = region {
        if w < 16 || h < 16 {
            return Err(format!("区域太小（{w}×{h}）：宽高至少 16 像素"));
        }
    }
    Ok(Config {
        region,
        fps: fps.unwrap_or(15).clamp(1, 50),
        scale_percent: scale_percent.unwrap_or(100).clamp(10, 100),
    })
}

// ============================================================
// 录制 → 导出
// ============================================================

pub struct RecordStats {
    pub output: PathBuf,
    pub frames: u64,
    pub duration_ms: u64,
    pub size_bytes: u64,
    pub truncated: bool,
}

impl RecordStats {
    pub fn to_json(&self) -> Json {
        Json::obj(vec![
            ("ok", Json::Bool(true)),
            ("output", Json::s(self.output.to_string_lossy())),
            ("frames", Json::u(self.frames)),
            ("duration_ms", Json::u(self.duration_ms)),
            ("size_bytes", Json::u(self.size_bytes)),
            ("truncated", Json::Bool(self.truncated)),
        ])
    }
}

/// 录制固定时长并直接导出 GIF（无 GUI、无编辑器）。
/// `prog(elapsed_ms, total_ms)` 周期回调用于进度展示。
pub fn record_to_gif<F: Fn(u64, u64)>(
    cfg: Config,
    duration: Duration,
    out: &Path,
    speed: Option<f64>,
    prog: F,
) -> Result<RecordStats, String> {
    let mut rec = Recorder::start_headless(cfg);
    let total_ms = duration.as_millis() as u64;
    let t0 = Instant::now();
    while t0.elapsed() < duration {
        let rem = duration - t0.elapsed();
        thread::sleep(rem.min(Duration::from_millis(100)));
        prog(t0.elapsed().as_millis() as u64, total_ms);
    }
    rec.stop();
    finish_and_export(rec.finish(), out, speed)
}

/// 单帧截图 → 单帧 GIF
pub fn snapshot_gif(
    region: Option<(i32, i32, i32, i32)>,
    scale_percent: u32,
    out: &Path,
) -> Result<RecordStats, String> {
    let mut g = ScreenGrabber::new(region, scale_percent)?;
    let (w, h) = (g.width, g.height);
    let rgb = g.grab()?.to_vec();
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let tmp = std::env::temp_dir().join(format!("bgr_snap_{ts}.cache"));
    let r = (|| -> Result<RecordStats, String> {
        let mut wr =
            FrameCacheWriter::create(&tmp, w, h, 30).map_err(|e| format!("创建帧缓存失败: {e}"))?;
        wr.write_frame(&rgb, 10).map_err(|e| format!("写帧缓存失败: {e}"))?;
        wr.finish().map_err(|e| format!("收尾写入失败: {e}"))?;
        export_cache(&tmp, 1, 0, false, out, None)
    })();
    let _ = std::fs::remove_file(&tmp);
    r
}

/// 停止/收尾一条录制并导出；导出后删除中间帧缓存。
fn finish_and_export(outcome: Outcome, out: &Path, speed: Option<f64>) -> Result<RecordStats, String> {
    match outcome {
        Outcome::Cancelled => Err("录制已取消".into()),
        Outcome::Failed(e) => Err(e),
        Outcome::Recorded { cache, frames, ms, truncated } => {
            let stats = export_cache(&cache, frames, ms, truncated, out, speed)?;
            let _ = std::fs::remove_file(&cache);
            Ok(stats)
        }
    }
}

/// 打开帧缓存 → 全区间导出 GIF。speed>1 加速、<1 减速；≈1 按原速。
fn export_cache(
    cache_path: &Path,
    frames: u64,
    ms: u64,
    truncated: bool,
    out: &Path,
    speed: Option<f64>,
) -> Result<RecordStats, String> {
    let cache = FrameCache::open(cache_path).map_err(|e| format!("打开帧缓存失败: {e}"))?;
    let n = cache.len();
    if n == 0 {
        return Err("录制内容为空".into());
    }
    let speed = speed.filter(|s| (s - 1.0).abs() > 1e-9);
    let target_cs = speed.map(|s| {
        let sel_cs = (cache.total_ms() / 10).max(1) as f64;
        (sel_cs / s).round().clamp(2.0, 6_000_000.0) as u32
    });
    export_gif(&cache, out, (0, n - 1), None, target_cs, |_, _| {})?;
    let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
    Ok(RecordStats {
        output: out.to_path_buf(),
        frames,
        duration_ms: ms,
        size_bytes: size,
        truncated,
    })
}

// ============================================================
// 会话式录制（start → [AI 执行其他操作] → stop/cancel）
// ============================================================

struct Active {
    rec: Recorder,
    t0: Instant,
}

/// 同一时刻只允许一条会话录制（多工具调用串行化）
static ACTIVE: Mutex<Option<Active>> = Mutex::new(None);

pub fn start_rec(cfg: Config) -> Result<(), String> {
    let mut g = ACTIVE.lock().unwrap();
    if g.is_some() {
        return Err("已有录制进行中：请先 stop_record（导出）或 cancel_record（丢弃）".into());
    }
    *g = Some(Active { rec: Recorder::start_headless(cfg), t0: Instant::now() });
    Ok(())
}

pub fn stop_rec(out: &Path, speed: Option<f64>) -> Result<RecordStats, String> {
    let mut g = ACTIVE.lock().unwrap();
    let Some(mut a) = g.take() else {
        return Err("当前没有进行中的录制（请先调用 start_record）".into());
    };
    drop(g); // 导出可能耗时，先释放锁
    a.rec.stop();
    finish_and_export(a.rec.finish(), out, speed)
}

pub fn cancel_rec() -> Result<(), String> {
    let mut g = ACTIVE.lock().unwrap();
    let Some(mut a) = g.take() else {
        return Err("当前没有进行中的录制".into());
    };
    a.rec.cancel();
    let _ = a.rec.finish(); // 录制线程自行删除临时文件
    Ok(())
}

pub fn status_json() -> Json {
    let g = ACTIVE.lock().unwrap();
    match g.as_ref() {
        Some(a) => Json::obj(vec![
            ("ok", Json::Bool(true)),
            ("recording", Json::Bool(true)),
            ("elapsed_ms", Json::u(a.t0.elapsed().as_millis() as u64)),
            ("frames", Json::u(a.rec.frames.load(Ordering::Relaxed))),
        ]),
        None => Json::obj(vec![("ok", Json::Bool(true)), ("recording", Json::Bool(false))]),
    }
}
