//! 录后编辑器：帧缓存 → 预览播放 + 时间轴剪切 + 画面裁切 → 导出 GIF。
//! 布局参考 ScreenToGif 编辑器：上预览（拖拽画裁切框）、中传输控制、
//! 下时间轴（缩略图 + 双把手选择范围），底部动作按钮。

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, HINSTANCE, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, InvalidateRect, SelectObject, SetBkMode, SetTextColor, StretchDIBits, BITMAPINFO,
    BITMAPINFOHEADER, BI_RGB, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, DT_CENTER,
    DT_RIGHT, DT_SINGLELINE, FF_DONTCARE, FW_NORMAL, HBRUSH, HFONT, HGDIOBJ, OUT_DEFAULT_PRECIS,
    PAINTSTRUCT, PS_DASH, DIB_RGB_COLORS, SRCCOPY, TRANSPARENT,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, ReleaseCapture, SetCapture, VK_END, VK_ESCAPE, VK_HOME, VK_LEFT, VK_RIGHT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, BN_CLICKED, CreateWindowExW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW,
    DefWindowProcW, DestroyWindow, EN_CHANGE, ES_AUTOHSCROLL, ES_RIGHT, GetWindowLongPtrW,
    GetWindowTextW, GWLP_USERDATA, HMENU, IDC_ARROW, KillTimer, LoadCursorW, MB_ICONWARNING,
    MB_OK, MB_OKCANCEL, MessageBoxW, PostMessageW, RegisterClassW, SendMessageW,
    SetForegroundWindow, SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow, SW_SHOW, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_KEYDOWN, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_BORDER, WS_CAPTION, WS_CHILD,
    WS_OVERLAPPED, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE, WINDOW_EX_STYLE, WINDOW_STYLE,
};

use crate::cache::FrameCache;
use crate::to_wide;

pub const CLASS_EDITOR: &str = "BGR_EDITOR";

/// 编辑器结束 → 通知主窗口（wp: 1=已导出 0=已丢弃；EXPORT_MSG 带详情）
pub const WM_EDITOR_DONE: u32 = WM_APP + 0x50;
const WM_EXPORT_PROG: u32 = WM_APP + 0x41;
const WM_EXPORT_DONE: u32 = WM_APP + 0x42;

const TID_PLAY: usize = 1;

// 控件 ID
const IDB_FIRST: i32 = 3001;
const IDB_PREV: i32 = 3002;
const IDB_PLAY: i32 = 3003;
const IDB_NEXT: i32 = 3004;
const IDB_LAST: i32 = 3005;
const IDB_CROP_RESET: i32 = 3006;
const IDB_DISCARD: i32 = 3007;
const IDB_EXPORT: i32 = 3008;
const IDE_TARGET: i32 = 3009; // 目标时长（秒）输入框

// 布局（逻辑像素，96dpi 基准）
const CX: i32 = 720;
const CY: i32 = 548;
const PREVIEW: (i32, i32, i32, i32) = (12, 12, 696, 372);
const TL: (i32, i32, i32, i32) = (12, 432, 696, 56);

const CLR_RED: COLORREF = COLORREF(0x0023_11E8);
const CLR_DARKBG: COLORREF = COLORREF(0x0022_1814);
const CLR_DIM: COLORREF = COLORREF(0x0078_6E_46); // 未选中时间轴遮罩
const THUMB_H: i32 = 36;
const THUMB_MAX_W: i32 = 96;
const N_THUMBS: usize = 48;
const CROP_MIN: i32 = 16;

pub static EXPORT_MSG: Mutex<String> = Mutex::new(String::new());

#[derive(Clone, Copy, PartialEq)]
enum TlDrag {
    None,
    Start,
    End,
    Seek,
}

struct Ed {
    hwnd: HWND,
    owner: HWND,
    dpi: u32,
    cache: FrameCache,
    cur: usize,
    sel: (usize, usize), // [start, end] 含端点
    crop: Option<(u32, u32, u32, u32)>,
    playing: bool,
    frame_buf: Vec<u8>, // 当前帧 RGB
    dib: Vec<u8>,       // 当前帧 BGRA（显示）
    thumbs: Vec<Vec<u8>>, // BGRA 缩略图
    thumb_w: i32,
    thumb_h: i32,
    crop_drag: Option<(i32, i32, i32, i32)>, // 客户区坐标
    tl_drag: TlDrag,
    exporting: bool,
    font: HFONT,
    truncated: bool,
    // 控件
    btns: Vec<HWND>,
    btn_play: HWND,
    lbl_info: HWND,
    lbl_crop: HWND,
    ed_target: HWND, // 目标时长（秒）；空 = 原速
}

/// HWND 跨线程载体
#[derive(Clone, Copy)]
struct HwndSend(usize);

pub fn register_class(hinst: HINSTANCE) {
    unsafe {
        let mut cls = to_wide(CLASS_EDITOR);
        cls.pop();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(editor_wndproc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
            hIcon: crate::load_app_icon(hinst),
            hbrBackground: HBRUSH(16usize as *mut c_void), // COLOR_BTNFACE + 1
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

/// 打开编辑器（打开缓存 + 生成缩略图 + 建窗口）。失败返回 None。
pub unsafe fn open_editor(
    owner: HWND,
    hinst: HINSTANCE,
    dpi: u32,
    cache_path: PathBuf,
    frames: u64,
    ms: u64,
    truncated: bool,
) -> Option<HWND> {
    unsafe {
        let cache = match FrameCache::open(&cache_path) {
            Ok(c) => c,
            Err(e) => {
                let t = to_wide(&format!("打开帧缓存失败: {e}\n{}", cache_path.display()));
                let cap = to_wide("错误");
                let _ = MessageBoxW(Some(owner), PCWSTR(t.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONWARNING);
                return None;
            }
        };
        let n = cache.len();
        if n == 0 {
            return None;
        }

        // 缩略图（固定高度，按比例宽度）
        let s = |v: i32| v * dpi as i32 / 96;
        let th = s(THUMB_H);
        let tw = ((cache.w as i64 * th as i64 / cache.h as i64) as i32).clamp(8, s(THUMB_MAX_W));
        let mut thumbs = Vec::new();
        let step = ((n + N_THUMBS - 1) / N_THUMBS).max(1);
        let mut idx = 0usize;
        let (cw, chh) = (tw as usize, th as usize);
        let _ = cache.decode_range(0, n, |rgb, _| {
            if idx % step == 0 && thumbs.len() < N_THUMBS {
                let mut t = vec![255u8; cw * chh * 4];
                downscale_bgra(rgb, cache.w, cache.h, &mut t, cw, chh);
                thumbs.push(t);
            }
            idx += 1;
        });

        let mut font_face = [0u16; 32];
        for (i, ch) in "Microsoft YaHei UI".encode_utf16().take(31).enumerate() {
            font_face[i] = ch;
        }
        let font = CreateFontW(
            -s(12),
            0, 0, 0,
            FW_NORMAL.0 as i32,
            0, 0, 0,
            windows::Win32::Graphics::Gdi::DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            CLEARTYPE_QUALITY,
            (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
            PCWSTR::from_raw(font_face.as_ptr()),
        );

        let ed = Box::new(Ed {
            hwnd: HWND::default(),
            owner,
            dpi,
            cache,
            cur: 0,
            sel: (0, n - 1),
            crop: None,
            playing: false,
            frame_buf: Vec::new(),
            dib: Vec::new(),
            thumbs,
            thumb_w: tw,
            thumb_h: th,
            crop_drag: None,
            tl_drag: TlDrag::None,
            exporting: false,
            font,
            truncated,
            btns: Vec::new(),
            btn_play: HWND::default(),
            lbl_info: HWND::default(),
            lbl_crop: HWND::default(),
            ed_target: HWND::default(),
        });
        let _ = (frames, ms); // 统计信息由缓存自身计算

        let mut rect = RECT { left: 0, top: 0, right: CX, bottom: CY };
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU;
        let _ = AdjustWindowRect(&mut rect, style, false);
        let w = (rect.right - rect.left) * dpi as i32 / 96;
        let h = (rect.bottom - rect.top) * dpi as i32 / 96;

        let mut title = to_wide("录制完成 — 预览 · 剪切 · 导出");
        title.pop();
        let mut cls = to_wide(CLASS_EDITOR);
        cls.pop();
        let hwnd = CreateWindowExW(
            windows::Win32::UI::WindowsAndMessaging::WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            windows::Win32::UI::WindowsAndMessaging::CW_USEDEFAULT,
            w,
            h,
            None, // 独立顶层：主窗口隐藏时不连带
            None,
            Some(hinst),
            Some(Box::into_raw(ed) as *const c_void),
        )
        .expect("create editor window");
        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = SetForegroundWindow(hwnd);
        Some(hwnd)
    }
}

fn downscale_bgra(src: &[u8], sw: usize, sh: usize, dst: &mut [u8], dw: usize, dh: usize) {
    for y in 0..dh {
        let sy0 = y * sh / dh;
        let sy1 = ((y + 1) * sh / dh).max(sy0 + 1);
        for x in 0..dw {
            let sx0 = x * sw / dw;
            let sx1 = ((x + 1) * sw / dw).max(sx0 + 1);
            let (mut r, mut g, mut b, mut cnt) = (0u32, 0u32, 0u32, 0u32);
            for yy in sy0..sy1.min(sh) {
                for xx in sx0..sx1.min(sw) {
                    let p = (yy * sw + xx) * 3;
                    r += src[p] as u32;
                    g += src[p + 1] as u32;
                    b += src[p + 2] as u32;
                    cnt += 1;
                }
            }
            let d = (y * dw + x) * 4;
            dst[d] = (b / cnt) as u8;
            dst[d + 1] = (g / cnt) as u8;
            dst[d + 2] = (r / cnt) as u8;
        }
    }
}

impl Ed {
    fn s(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }
}

unsafe fn ed_of(hwnd: HWND) -> Option<&'static mut Ed> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if p == 0 {
            None
        } else {
            Some(&mut *(p as *mut Ed))
        }
    }
}

fn fmt_ms(ms: u64) -> String {
    let t = ms / 100;
    format!("{}:{:02}.{}", t / 600, (t / 10) % 60, t % 10)
}

/// 读取输入框文本
unsafe fn get_edit_text(h: HWND) -> String {
    unsafe {
        let mut buf = [0u16; 32];
        let n = GetWindowTextW(h, &mut buf).max(0) as usize;
        String::from_utf16_lossy(&buf[..n.min(buf.len())])
    }
}

/// 解析目标时长（秒）：空 = 原速 Ok(None)；非法值返回 Err(提示)
fn parse_target(text: &str) -> Result<Option<f64>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    match t.parse::<f64>() {
        Ok(v) if v.is_finite() && (0.1..=600.0).contains(&v) => Ok(Some(v)),
        _ => Err("目标时长无法识别：请输入 0.1 ~ 600 之间的秒数（留空 = 原速）。".to_string()),
    }
}

/// 选中区间时长（毫秒）
fn sel_ms(ed: &Ed) -> u64 {
    ed.cache.time_ms_before(ed.sel.1 + 1) - ed.cache.time_ms_before(ed.sel.0)
}

/// 延时缩放系数 = 目标时长 / 选中时长（< 1 加速，> 1 减速）
fn factor_from(ed: &Ed, secs: Option<f64>) -> Option<f64> {
    let secs = secs?;
    let ms = sel_ms(ed);
    if ms == 0 {
        return None;
    }
    Some(secs * 1000.0 / ms as f64)
}

/// 预览播放第 frame 帧的定时（毫秒），应用目标时长变速
unsafe fn play_timer_ms(ed: &Ed, frame: usize) -> u32 {
    unsafe {
        let secs = parse_target(&get_edit_text(ed.ed_target)).ok().flatten();
        let f = factor_from(ed, secs).unwrap_or(1.0);
        let d = ed.cache.delay_cs(frame).max(2) as f64 * 10.0 * f;
        d.max(15.0) as u32
    }
}

/// 预览画面在客户区的显示矩形（等比缩放居中）
fn preview_map(ed: &Ed) -> (i32, i32, i32, i32) {
    let (px, py, pw, ph) = (ed.s(PREVIEW.0), ed.s(PREVIEW.1), ed.s(PREVIEW.2), ed.s(PREVIEW.3));
    let fw = ed.cache.w as f64;
    let fh = ed.cache.h as f64;
    let scale = (pw as f64 / fw).min(ph as f64 / fh);
    let dw = (fw * scale) as i32;
    let dh = (fh * scale) as i32;
    (px + (pw - dw) / 2, py + (ph - dh) / 2, dw.max(1), dh.max(1))
}

fn tl_rect(ed: &Ed) -> RECT {
    RECT {
        left: ed.s(TL.0),
        top: ed.s(TL.1),
        right: ed.s(TL.0 + TL.2),
        bottom: ed.s(TL.1 + TL.3),
    }
}

fn tl_x_at(ed: &Ed, i: usize) -> i32 {
    let tl = tl_rect(ed);
    let n = ed.cache.len() as f64;
    let ppf = (tl.right - tl.left) as f64 / n;
    (tl.left as f64 + (i as f64 + 0.5) * ppf) as i32
}

fn tl_frame_at(ed: &Ed, x: i32) -> usize {
    let tl = tl_rect(ed);
    let n = ed.cache.len();
    let ppf = (tl.right - tl.left) as f64 / n as f64;
    let i = ((x - tl.left) as f64 / ppf - 0.5).round() as i64;
    i.clamp(0, n as i64 - 1) as usize
}

/// 解码并显示第 i 帧
unsafe fn show_frame(ed: &mut Ed, i: usize) {
    unsafe {
        ed.cur = i.min(ed.cache.len() - 1);
        if ed.cache.decode_into(ed.cur, &mut ed.frame_buf).is_err() {
            stop_play(ed);
        }
        rgb_to_bgra(&ed.frame_buf, &mut ed.dib);
        let rc = RECT {
            left: ed.s(PREVIEW.0),
            top: ed.s(PREVIEW.1),
            right: ed.s(PREVIEW.0 + PREVIEW.2),
            bottom: ed.s(PREVIEW.1 + PREVIEW.3),
        };
        let _ = InvalidateRect(Some(ed.hwnd), Some(&rc), true);
        update_info(ed);
    }
}

fn rgb_to_bgra(rgb: &[u8], dib: &mut Vec<u8>) {
    dib.clear();
    dib.reserve(rgb.len() / 3 * 4);
    for p in rgb.chunks_exact(3) {
        dib.extend_from_slice(&[p[2], p[1], p[0], 255]);
    }
}

unsafe fn stop_play(ed: &mut Ed) {
    unsafe {
        if ed.playing {
            ed.playing = false;
            let _ = KillTimer(Some(ed.hwnd), TID_PLAY);
            let t = to_wide("▶ 播放");
            let _ = SetWindowTextW(ed.btn_play, PCWSTR(t.as_ptr()));
        }
    }
}

unsafe fn start_play(ed: &mut Ed) {
    unsafe {
        if ed.playing || ed.exporting {
            return;
        }
        ed.playing = true;
        let t = to_wide("■ 暂停");
        let _ = SetWindowTextW(ed.btn_play, PCWSTR(t.as_ptr()));
        let _ = SetTimer(Some(ed.hwnd), TID_PLAY, play_timer_ms(ed, ed.cur), None);
    }
}

unsafe fn update_info(ed: &Ed) {
    unsafe {
        let n = ed.cache.len();
        let cur_ms = ed.cache.time_ms_before(ed.cur);
        let total_ms = ed.cache.total_ms();
        let sel_ms = ed.cache.time_ms_before(ed.sel.1 + 1) - ed.cache.time_ms_before(ed.sel.0);
        let crop_txt = match ed.crop {
            Some((_, _, w, h)) => format!("裁切 {w}×{h}"),
            None => "裁切 无".to_string(),
        };
        let speed_txt = {
            let secs = parse_target(&get_edit_text(ed.ed_target)).ok().flatten();
            match secs {
                Some(s) => format!("目标 {s:.1}s ({:.2}×)", sel_ms as f64 / (s * 1000.0)),
                None => "原速".to_string(),
            }
        };
        let txt = format!(
            "帧 {}/{} · {} / {} · 选中 {} 帧 ({}) · {} · {}",
            ed.cur + 1,
            n,
            fmt_ms(cur_ms),
            fmt_ms(total_ms),
            ed.sel.1 - ed.sel.0 + 1,
            fmt_ms(sel_ms),
            crop_txt,
            speed_txt,
        );
        let t = to_wide(&txt);
        let _ = SetWindowTextW(ed.lbl_info, PCWSTR(t.as_ptr()));
        let c = match ed.crop {
            Some((_, _, w, h)) => format!("画面裁切: {w}×{h}"),
            None => "画面裁切: 无（预览区拖拽框选）".to_string(),
        };
        let c = to_wide(&c);
        let _ = SetWindowTextW(ed.lbl_crop, PCWSTR(c.as_ptr()));
    }
}

unsafe fn set_buttons_enabled(ed: &Ed, on: bool) {
    unsafe {
        for &b in &ed.btns {
            let _ = EnableWindow(b, on);
        }
    }
}

// ============================================================
// 导出
// ============================================================

unsafe fn begin_export(ed: &mut Ed) {
    unsafe {
        if ed.exporting {
            return;
        }
        // 先校验目标时长，再弹保存对话框
        let target = match parse_target(&get_edit_text(ed.ed_target)) {
            Ok(t) => t,
            Err(msg) => {
                let t = to_wide(&msg);
                let cap = to_wide("目标时长");
                let _ = MessageBoxW(
                    Some(ed.hwnd),
                    PCWSTR(t.as_ptr()),
                    PCWSTR(cap.as_ptr()),
                    MB_OK | MB_ICONWARNING,
                );
                return;
            }
        };
        let target_cs = target.map(|s| (s * 100.0).round() as u32);
        let secs = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let suggest = format!("gifrec_{secs}.gif");
        let Some(path) = crate::save_dialog(ed.hwnd, &suggest) else {
            return;
        };
        stop_play(ed);
        ed.exporting = true;
        set_buttons_enabled(ed, false);

        let cache_path = ed.cache.path().to_path_buf();
        let sel = ed.sel;
        let crop = ed.crop;
        let notify = HwndSend(ed.hwnd.0 as usize);
        std::thread::Builder::new()
            .name("gif-export".into())
            .spawn(move || {
                let result: Result<(), String> = (|| {
                    let cache = FrameCache::open(&cache_path).map_err(|e| e.to_string())?;
                    crate::export::export_gif(&cache, &path, sel, crop, target_cs, |d, t| {
                        let _ = PostMessageW(
                            Some(HWND(notify.0 as *mut c_void)),
                            WM_EXPORT_PROG,
                            WPARAM(d),
                            LPARAM(t as isize),
                        );
                    })
                })();
                match result {
                    Ok(()) => {
                        *EXPORT_MSG.lock().unwrap() = format!("已导出 {}", path.display());
                        let _ = PostMessageW(
                            Some(HWND(notify.0 as *mut c_void)),
                            WM_EXPORT_DONE,
                            WPARAM(1),
                            LPARAM(0),
                        );
                    }
                    Err(e) => {
                        *EXPORT_MSG.lock().unwrap() = e;
                        let _ = PostMessageW(
                            Some(HWND(notify.0 as *mut c_void)),
                            WM_EXPORT_DONE,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
            })
            .expect("spawn export thread");
    }
}

/// 丢弃/关闭编辑器
unsafe fn close_editor(ed: &mut Ed, hwnd: HWND, confirm: bool) {
    unsafe {
        if ed.exporting {
            return;
        }
        stop_play(ed);
        if confirm {
            let t = to_wide("丢弃这次录制？帧缓存将被删除，且无法恢复。");
            let cap = to_wide("丢弃录制");
            if MessageBoxW(Some(hwnd), PCWSTR(t.as_ptr()), PCWSTR(cap.as_ptr()), MB_OKCANCEL | MB_ICONWARNING) != windows::Win32::UI::WindowsAndMessaging::IDOK
            {
                return;
            }
        }
        *EXPORT_MSG.lock().unwrap() = "已丢弃录制。".to_string();
        let _ = PostMessageW(Some(ed.owner), WM_EDITOR_DONE, WPARAM(0), LPARAM(0));
        let _ = DestroyWindow(hwnd);
    }
}

// ============================================================
// 绘制
// ============================================================

unsafe fn draw_preview(ed: &Ed, dc: windows::Win32::Graphics::Gdi::HDC) {
    unsafe {
        let (dx, dy, dw, dh) = preview_map(ed);
        // 背景框
        let bg = CreateSolidBrush(CLR_DARKBG);
        let prc = RECT {
            left: ed.s(PREVIEW.0),
            top: ed.s(PREVIEW.1),
            right: ed.s(PREVIEW.0 + PREVIEW.2),
            bottom: ed.s(PREVIEW.1 + PREVIEW.3),
        };
        FillRect(dc, &prc, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        // 当前帧
        if !ed.dib.is_empty() {
            let (w, h) = (ed.cache.w, ed.cache.h);
            let bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: w as i32,
                    biHeight: -(h as i32),
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            let _ = StretchDIBits(
                dc,
                dx,
                dy,
                dw,
                dh,
                0,
                0,
                w as i32,
                h as i32,
                Some(ed.dib.as_ptr() as *const c_void),
                &bmi,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }

        let _ = SetBkMode(dc, TRANSPARENT);

        // 裁切框（帧坐标 → 显示坐标）
        let scale = dw as f64 / ed.cache.w as f64;
        if let Some((cx, cy, cw, chh)) = ed.crop {
            let (x, y) = (dx + (cx as f64 * scale) as i32, dy + (cy as f64 * scale) as i32);
            let (w2, h2) = ((cw as f64 * scale) as i32, (chh as f64 * scale) as i32);
            draw_crop_rect(ed, dc, x, y, x + w2, y + h2, cw, chh);
        }
        // 拖拽中的橡皮筋
        if let Some((x0, y0, x1, y1)) = ed.crop_drag {
            let (fw, fh) = (
                ((x1 - x0) as f64 / scale).abs() as u32,
                ((y1 - y0) as f64 / scale).abs() as u32,
            );
            draw_crop_rect(
                ed,
                dc,
                x0.min(x1),
                y0.min(y1),
                x0.max(x1),
                y0.max(y1),
                fw,
                fh,
            );
        }
    }
}

unsafe fn draw_crop_rect(ed: &Ed, dc: windows::Win32::Graphics::Gdi::HDC, x0: i32, y0: i32, x1: i32, y1: i32, fw: u32, fh: u32) {
    unsafe {
        let pen = CreatePen(PS_DASH, 1, CLR_RED);
        let old = SelectObject(dc, HGDIOBJ(pen.0));
        let _ = windows::Win32::Graphics::Gdi::MoveToEx(dc, x0, y0, None);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, x1, y0);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, x1, y1);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, x0, y1);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, x0, y0);
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(pen.0));

        // 尺寸标签
        let label = format!("{fw} × {fh}");
        let mut txt: Vec<u16> = label.encode_utf16().collect();
        let _ = SetTextColor(dc, CLR_RED);
        let mut rc = RECT { left: x0, top: y1 + ed.s(2), right: x1, bottom: y1 + ed.s(20) };
        let _ = DrawTextW(dc, &mut txt, &mut rc, DT_CENTER | DT_SINGLELINE);
    }
}

unsafe fn draw_timeline(ed: &Ed, dc: windows::Win32::Graphics::Gdi::HDC) {
    unsafe {
        let tl = tl_rect(ed);
        let n = ed.cache.len();

        // 底
        let bg = CreateSolidBrush(CLR_DARKBG);
        FillRect(dc, &tl, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        // 缩略图（时间比例均匀分布）
        let t_n = ed.thumbs.len().max(1);
        let bmi_tmpl = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: ed.thumb_w,
                biHeight: -ed.thumb_h,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        for (t, tb) in ed.thumbs.iter().enumerate() {
            let f = (t as f64 * (n as f64 - 1.0) / (t_n as f64 - 1.0)).round() as usize;
            let cx = tl_x_at(ed, f);
            let ty = tl.top + ed.s(8);
            let _ = StretchDIBits(
                dc,
                cx - ed.thumb_w / 2,
                ty,
                ed.thumb_w,
                ed.thumb_h,
                0,
                0,
                ed.thumb_w,
                ed.thumb_h,
                Some(tb.as_ptr() as *const c_void),
                &bmi_tmpl,
                DIB_RGB_COLORS,
                SRCCOPY,
            );
        }

        // 未选中区域遮罩
        let xs = tl_x_at(ed, ed.sel.0);
        let xe = tl_x_at(ed, ed.sel.1);
        let dim = CreateSolidBrush(CLR_DIM);
        if xs > tl.left {
            FillRect(dc, &RECT { left: tl.left, top: tl.top, right: xs, bottom: tl.bottom }, dim);
        }
        if xe < tl.right {
            FillRect(dc, &RECT { left: xe, top: tl.top, right: tl.right, bottom: tl.bottom }, dim);
        }
        let _ = DeleteObject(HGDIOBJ(dim.0));

        // 端把手
        let brand = CreateSolidBrush(CLR_RED);
        for x in [xs, xe] {
            let _ = FillRect(
                dc,
                &RECT {
                    left: x - ed.s(5),
                    top: tl.top + ed.s(2),
                    right: x + ed.s(5),
                    bottom: tl.bottom - ed.s(2),
                },
                brand,
            );
        }
        let _ = DeleteObject(HGDIOBJ(brand.0));

        // 播放头
        let px = tl_x_at(ed, ed.cur);
        let pen = CreatePen(windows::Win32::Graphics::Gdi::PS_SOLID, 2, CLR_RED);
        let old = SelectObject(dc, HGDIOBJ(pen.0));
        let _ = windows::Win32::Graphics::Gdi::MoveToEx(dc, px, tl.top, None);
        let _ = windows::Win32::Graphics::Gdi::LineTo(dc, px, tl.bottom);
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(pen.0));

        // 时间标签
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(0x00E8_E8_E8));
        let mut l: Vec<u16> = "0:00.0".encode_utf16().collect();
        let mut rc = RECT { left: tl.left + ed.s(4), top: tl.bottom - ed.s(16), right: tl.left + ed.s(64), bottom: tl.bottom - ed.s(2) };
        let _ = DrawTextW(dc, &mut l, &mut rc, DT_SINGLELINE);
        let total = fmt_ms(ed.cache.total_ms());
        let mut r: Vec<u16> = total.encode_utf16().collect();
        let mut rc2 = RECT { left: tl.right - ed.s(64), top: tl.bottom - ed.s(16), right: tl.right - ed.s(4), bottom: tl.bottom - ed.s(2) };
        let _ = DrawTextW(dc, &mut r, &mut rc2, DT_SINGLELINE | DT_RIGHT);
    }
}

// ============================================================
// 窗口过程
// ============================================================

unsafe extern "system" fn editor_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lp.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                let ed = &mut *(cs.lpCreateParams as *mut Ed);
                ed.hwnd = hwnd;
                let hinst = HINSTANCE(cs.hInstance.0);
                let (dpi_v, font_v) = (ed.dpi, ed.font);
                let create = |class: &str, text: &str, x: i32, y: i32, w: i32, h: i32, id: i32| -> HWND {
                    let cls = to_wide(class);
                    let txt = to_wide(text);
                    let s = |v: i32| v * dpi_v as i32 / 96;
                    let hwnd_c = CreateWindowExW(
                        WINDOW_EX_STYLE(0),
                        PCWSTR(cls.as_ptr()),
                        PCWSTR(txt.as_ptr()),
                        WS_CHILD | WS_VISIBLE,
                        s(x),
                        s(y),
                        s(w),
                        s(h),
                        Some(hwnd),
                        Some(HMENU(id as isize as *mut c_void)),
                        Some(hinst),
                        None,
                    )
                    .expect("create editor control");
                    let _ = SendMessageW(
                        hwnd_c,
                        WM_SETFONT,
                        Some(WPARAM(font_v.0 as usize)),
                        Some(LPARAM(1)),
                    );
                    hwnd_c
                };
                let b1 = create("BUTTON", "|◀", 12, 396, 44, 30, IDB_FIRST);
                let b2 = create("BUTTON", "◀", 60, 396, 44, 30, IDB_PREV);
                let b3 = create("BUTTON", "▶ 播放", 108, 396, 100, 30, IDB_PLAY);
                let b4 = create("BUTTON", "▶", 212, 396, 44, 30, IDB_NEXT);
                let b5 = create("BUTTON", "▶|", 260, 396, 44, 30, IDB_LAST);
                ed.lbl_info = create("STATIC", "", 320, 400, 388, 24, 0);
                let b6 = create("BUTTON", "清除裁切", 216, 504, 100, 30, IDB_CROP_RESET);
                ed.lbl_crop = create("STATIC", "画面裁切: 无", 12, 508, 200, 24, 0);
                let _lbl_target = create("STATIC", "目标(秒)", 318, 508, 54, 24, 0);
                let cls_edit = to_wide("EDIT");
                let txt_edit = to_wide("");
                let sd = |v: i32| v * dpi_v as i32 / 96;
                let ed_target = CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    PCWSTR(cls_edit.as_ptr()),
                    PCWSTR(txt_edit.as_ptr()),
                    WS_CHILD
                        | WS_VISIBLE
                        | WS_BORDER
                        | WS_TABSTOP
                        | WINDOW_STYLE((ES_AUTOHSCROLL | ES_RIGHT) as u32),
                    sd(374),
                    sd(506),
                    sd(56),
                    sd(24),
                    Some(hwnd),
                    Some(HMENU(IDE_TARGET as isize as *mut c_void)),
                    Some(hinst),
                    None,
                )
                .expect("create target edit");
                let _ = SendMessageW(
                    ed_target,
                    WM_SETFONT,
                    Some(WPARAM(font_v.0 as usize)),
                    Some(LPARAM(1)),
                );
                ed.ed_target = ed_target;
                let b7 = create("BUTTON", "丢弃录制", 436, 504, 120, 30, IDB_DISCARD);
                let b8 = create("BUTTON", "导出 GIF...", 560, 504, 148, 30, IDB_EXPORT);
                ed.btn_play = b3;
                ed.btns = vec![b1, b2, b3, b4, b5, b6, b7, b8, ed_target];

                show_frame(ed, 0);
                if ed.truncated {
                    let t = to_wide("已达到录制上限（时长/体积），录制被提前结束。");
                    let _ = SetWindowTextW(ed.lbl_info, PCWSTR(t.as_ptr()));
                }
                LRESULT(0)
            }
            WM_COMMAND => {
                let Some(ed) = ed_of(hwnd) else { return LRESULT(0) };
                let id = (wp.0 & 0xFFFF) as i32;
                // 目标时长输入变化 → 刷新信息栏的速度显示
                if ((wp.0 >> 16) & 0xFFFF) as u32 == EN_CHANGE && id == IDE_TARGET {
                    update_info(ed);
                    return LRESULT(0);
                }
                if ((wp.0 >> 16) & 0xFFFF) as i32 == BN_CLICKED as i32 {
                    match id {
                        IDB_FIRST => {
                            stop_play(ed);
                            show_frame(ed, 0);
                        }
                        IDB_PREV => {
                            stop_play(ed);
                            show_frame(ed, ed.cur.saturating_sub(1));
                        }
                        IDB_PLAY => {
                            if ed.playing {
                                stop_play(ed);
                            } else {
                                if ed.cur >= ed.sel.1 {
                                    show_frame(ed, ed.sel.0);
                                }
                                start_play(ed);
                            }
                        }
                        IDB_NEXT => {
                            stop_play(ed);
                            show_frame(ed, (ed.cur + 1).min(ed.cache.len() - 1));
                        }
                        IDB_LAST => {
                            stop_play(ed);
                            show_frame(ed, ed.cache.len() - 1);
                        }
                        IDB_CROP_RESET => {
                            ed.crop = None;
                            let _ = InvalidateRect(Some(hwnd), None, true);
                            update_info(ed);
                        }
                        IDB_DISCARD => close_editor(ed, hwnd, true),
                        IDB_EXPORT => begin_export(ed),
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_KEYDOWN => {
                let Some(ed) = ed_of(hwnd) else { return LRESULT(0) };
                match wp.0 as u16 {
                    k if k == VK_LEFT.0 => {
                        stop_play(ed);
                        show_frame(ed, ed.cur.saturating_sub(1));
                    }
                    k if k == VK_RIGHT.0 => {
                        stop_play(ed);
                        show_frame(ed, (ed.cur + 1).min(ed.cache.len() - 1));
                    }
                    k if k == VK_HOME.0 => {
                        stop_play(ed);
                        show_frame(ed, 0);
                    }
                    k if k == VK_END.0 => {
                        stop_play(ed);
                        show_frame(ed, ed.cache.len() - 1);
                    }
                    k if k == VK_ESCAPE.0 => stop_play(ed),
                    k if k == ' ' as u16 => {
                        if ed.playing {
                            stop_play(ed);
                        } else {
                            if ed.cur >= ed.sel.1 {
                                show_frame(ed, ed.sel.0);
                            }
                            start_play(ed);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let Some(ed) = ed_of(hwnd) else { return LRESULT(0) };
                let next = if ed.cur >= ed.sel.1 { ed.sel.0 } else { ed.cur + 1 };
                show_frame(ed, next);
                if ed.playing {
                    let _ = SetTimer(Some(hwnd), TID_PLAY, play_timer_ms(ed, ed.cur), None);
                }
                let tl = tl_rect(ed);
                let _ = InvalidateRect(Some(hwnd), Some(&tl), true);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let Some(ed) = ed_of(hwnd) else { return LRESULT(0) };
                if ed.exporting {
                    return LRESULT(0);
                }
                let (mx, my) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
                let tl = tl_rect(ed);
                if mx >= tl.left && mx < tl.right && my >= tl.top && my < tl.bottom {
                    // 时间轴：命中端把手或寻址
                    let xs = tl_x_at(ed, ed.sel.0);
                    let xe = tl_x_at(ed, ed.sel.1);
                    let hw = ed.s(10);
                    ed.tl_drag = if (mx - xs).abs() <= hw {
                        TlDrag::Start
                    } else if (mx - xe).abs() <= hw {
                        TlDrag::End
                    } else {
                        TlDrag::Seek
                    };
                    if ed.tl_drag == TlDrag::Seek {
                        stop_play(ed);
                        show_frame(ed, tl_frame_at(ed, mx));
                    }
                    let _ = SetCapture(hwnd);
                } else {
                    let (dx, dy, dw, dh) = preview_map(ed);
                    if mx >= dx && mx < dx + dw && my >= dy && my < dy + dh {
                        stop_play(ed);
                        ed.crop_drag = Some((mx, my, mx, my));
                        let _ = SetCapture(hwnd);
                    }
                }
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let Some(ed) = ed_of(hwnd) else { return LRESULT(0) };
                let (mx, _) = ((lp.0 & 0xFFFF) as i16 as i32, ((lp.0 >> 16) & 0xFFFF) as i16 as i32);
                match ed.tl_drag {
                    TlDrag::Start => {
                        let f = tl_frame_at(ed, mx).min(ed.sel.1);
                        ed.sel.0 = f;
                        let _ = InvalidateRect(Some(hwnd), Some(&tl_rect(ed)), true);
                    }
                    TlDrag::End => {
                        let f = tl_frame_at(ed, mx).max(ed.sel.0);
                        ed.sel.1 = f;
                        let _ = InvalidateRect(Some(hwnd), Some(&tl_rect(ed)), true);
                    }
                    TlDrag::Seek => {
                        show_frame(ed, tl_frame_at(ed, mx));
                    }
                    TlDrag::None => {
                        if let Some((x0, y0, _, _)) = ed.crop_drag {
                            ed.crop_drag = Some((x0, y0, mx, ((lp.0 >> 16) & 0xFFFF) as i16 as i32));
                            let rc = RECT {
                                left: ed.s(PREVIEW.0),
                                top: ed.s(PREVIEW.1),
                                right: ed.s(PREVIEW.0 + PREVIEW.2),
                                bottom: ed.s(PREVIEW.1 + PREVIEW.3),
                            };
                            let _ = InvalidateRect(Some(hwnd), Some(&rc), true);
                        }
                    }
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let Some(ed) = ed_of(hwnd) else { return LRESULT(0) };
                let _ = ReleaseCapture();
                if ed.tl_drag != TlDrag::None {
                    ed.tl_drag = TlDrag::None;
                    update_info(ed);
                }
                if let Some((x0, y0, x1, y1)) = ed.crop_drag.take() {
                    // 客户区坐标 → 帧坐标
                    let (dx, dy, dw, dh) = preview_map(ed);
                    let scale_x = ed.cache.w as f64 / dw as f64;
                    let scale_y = ed.cache.h as f64 / dh as f64;
                    let fx = |x: i32| ((x - dx) as f64 * scale_x).round() as i32;
                    let fy = |y: i32| ((y - dy) as f64 * scale_y).round() as i32;
                    let x_min = fx(x0.min(x1)).clamp(0, ed.cache.w as i32 - 1);
                    let x_max = fx(x0.max(x1)).clamp(0, ed.cache.w as i32);
                    let y_min = fy(y0.min(y1)).clamp(0, ed.cache.h as i32 - 1);
                    let y_max = fy(y0.max(y1)).clamp(0, ed.cache.h as i32);
                    let (cw, chh) = (x_max - x_min, y_max - y_min);
                    ed.crop = if cw >= CROP_MIN && chh >= CROP_MIN {
                        Some((x_min as u32, y_min as u32, cw as u32, chh as u32))
                    } else {
                        None
                    };
                    let rc = RECT {
                        left: ed.s(PREVIEW.0),
                        top: ed.s(PREVIEW.1),
                        right: ed.s(PREVIEW.0 + PREVIEW.2),
                        bottom: ed.s(PREVIEW.1 + PREVIEW.3),
                    };
                    let _ = InvalidateRect(Some(hwnd), Some(&rc), true);
                    update_info(ed);
                }
                LRESULT(0)
            }
            WM_EXPORT_PROG => {
                if let Some(ed) = ed_of(hwnd) {
                    let t = to_wide(&format!("导出中 {}/{} 帧 ...", wp.0, lp.0));
                    let _ = SetWindowTextW(ed.lbl_info, PCWSTR(t.as_ptr()));
                }
                LRESULT(0)
            }
            WM_EXPORT_DONE => {
                let Some(ed) = ed_of(hwnd) else { return LRESULT(0) };
                ed.exporting = false;
                set_buttons_enabled(ed, true);
                if wp.0 == 1 {
                    // 成功：删缓存，回主窗口
                    let _ = std::fs::remove_file(ed.cache.path());
                    let _ = PostMessageW(Some(ed.owner), WM_EDITOR_DONE, WPARAM(1), LPARAM(0));
                    let _ = DestroyWindow(hwnd);
                } else {
                    let msg = EXPORT_MSG.lock().unwrap().clone();
                    let t = to_wide(&format!("导出失败：{msg}\n帧缓存已保留，可重试。"));
                    let cap = to_wide("导出失败");
                    let _ = MessageBoxW(Some(hwnd), PCWSTR(t.as_ptr()), PCWSTR(cap.as_ptr()), MB_OK | MB_ICONWARNING);
                    update_info(ed);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if let Some(ed) = ed_of(hwnd) {
                    close_editor(ed, hwnd, true);
                }
                LRESULT(0)
            }
            WM_PAINT => {
                let Some(ed) = ed_of(hwnd) else { return DefWindowProcW(hwnd, msg, wp, lp) };
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                draw_preview(ed, dc);
                draw_timeline(ed, dc);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    {
                        let ed = Box::from_raw(p as *mut Ed);
                        let _ = KillTimer(Some(hwnd), TID_PLAY);
                        let _ = DeleteObject(HGDIOBJ(ed.font.0));
                    }
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}
