//! BitGalaxy.GifRecorder —— 轻量级 GIF 录屏工具。
//! 控制窗口 + 全局热键（F9 开始/停止，F10 取消）+ 拖拽选区 + 保存对话框。

mod capture;
mod gif;
mod recorder;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, HINSTANCE, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DrawTextW, EndPaint, FillRect, GetDC, GetDeviceCaps, GetStockObject,
    InvalidateRect, LOGPIXELSY, SetBkMode, SetTextColor, UpdateWindow, BLACK_BRUSH,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, DT_CALCRECT, DT_CENTER, DT_SINGLELINE,
    FF_DONTCARE, FW_NORMAL, HBRUSH, HFONT, HDC, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    WHITE_BRUSH,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_HIDEREADONLY, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, RegisterHotKey, ReleaseCapture, SetCapture, MOD_NOREPEAT, VK_ESCAPE, VK_F10,
    VK_F9,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, BN_CLICKED, CB_ADDSTRING, CB_GETCURSEL, CB_SETCURSEL, CBS_DROPDOWNLIST,
    CreateWindowExW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT, DefWindowProcW,
    DestroyWindow, DispatchMessageW, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, GWLP_USERDATA, HMENU, IDC_ARROW, IDC_CROSS, LoadCursorW, MessageBoxW,
    MB_ICONERROR, MB_OK, PostMessageW, PostQuitMessage, RegisterClassW, SetForegroundWindow,
    SetLayeredWindowAttributes, SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow,
    TranslateMessage, LWA_ALPHA, MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN, SW_HIDE, SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE,
    WM_COMMAND, WM_CREATE, WM_DESTROY, WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MOUSEMOVE, WM_PAINT, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD,
    WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_MINIMIZEBOX, WS_OVERLAPPED, WS_POPUP,
    WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
};

use recorder::{Config, Outcome, Recorder};

const CLASS_MAIN: &str = "BGR_MAIN";
const CLASS_SEL: &str = "BGR_SEL";

const IDC_BTN_SELECT: i32 = 1001;
const IDC_BTN_START: i32 = 1002;
const IDC_BTN_CANCEL: i32 = 1003;

const WM_REGION_PICKED: u32 = WM_APP + 0x11;

const FPS_OPTIONS: [u32; 7] = [8, 10, 12, 15, 20, 25, 30];
const SCALE_OPTIONS: [u32; 3] = [100, 75, 50];

/// 选区窗口 → 主窗口 的区域回传
static PICKED: Mutex<Option<(i32, i32, i32, i32)>> = Mutex::new(None);

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

struct App {
    hinst: HINSTANCE,
    hwnd: HWND,
    dpi: u32,
    font: HFONT,
    rec: Option<Recorder>,
    rec_started: Option<Instant>,
    region: Option<(i32, i32, i32, i32)>,
    // 控件
    cb_fps: HWND,
    cb_scale: HWND,
    btn_select: HWND,
    lbl_region: HWND,
    btn_start: HWND,
    btn_cancel: HWND,
    lbl_status: HWND,
}

impl App {
    fn new(hinst: HINSTANCE, dpi: u32) -> Self {
        Self {
            hinst,
            hwnd: HWND::default(),
            dpi,
            font: HFONT::default(),
            rec: None,
            rec_started: None,
            region: None,
            cb_fps: HWND::default(),
            cb_scale: HWND::default(),
            btn_select: HWND::default(),
            lbl_region: HWND::default(),
            btn_start: HWND::default(),
            btn_cancel: HWND::default(),
            lbl_status: HWND::default(),
        }
    }

    /// 逻辑坐标（96dpi 基准）→ 物理像素
    fn s(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }

    unsafe fn create_font(&mut self) {
        unsafe {
            let mut face = [0u16; 32];
            for (i, c) in "Microsoft YaHei UI".encode_utf16().take(31).enumerate() {
                face[i] = c;
            }
            self.font = CreateFontW(
                -self.s(12),
                0,
                0,
                0,
                FW_NORMAL.0 as i32,
                0,
                0,
                0,
                windows::Win32::Graphics::Gdi::DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                PCWSTR::from_raw(face.as_ptr()),
            );
        }
    }

    unsafe fn child(
        &self,
        class: &str,
        text: &str,
        extra_style: WINDOW_STYLE,
        x: i32,
        y: i32,
        w: i32,
        h: i32,
        id: i32,
    ) -> HWND {
        unsafe {
            let cls = to_wide(class);
            let txt = to_wide(text);
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(cls.as_ptr()),
                PCWSTR(txt.as_ptr()),
                WS_CHILD | WS_VISIBLE | extra_style,
                self.s(x),
                self.s(y),
                self.s(w),
                self.s(h),
                Some(self.hwnd),
                Some(HMENU(id as isize as *mut c_void)),
                Some(self.hinst),
                None,
            )
            .expect("create child control");
            if !self.font.is_invalid() {
                let _ = send_msg(hwnd, WM_SETFONT, self.font.0 as usize, 1);
            }
            hwnd
        }
    }

    unsafe fn create_controls(&mut self) {
        unsafe {
            self.cb_fps = self.child(
                "COMBOBOX",
                "",
                WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_TABSTOP,
                68, 10, 70, 200, 0,
            );
            for f in FPS_OPTIONS {
                let t = to_wide(&f.to_string());
                let _ = send_msg(self.cb_fps, CB_ADDSTRING, 0, t.as_ptr() as isize);
            }
            let _ = send_msg(self.cb_fps, CB_SETCURSEL, 3, 0); // 默认 15fps

            self.cb_scale = self.child(
                "COMBOBOX",
                "",
                WINDOW_STYLE(CBS_DROPDOWNLIST as u32) | WS_TABSTOP,
                208, 10, 76, 200, 0,
            );
            for f in SCALE_OPTIONS {
                let t = to_wide(&format!("{}%", f));
                let _ = send_msg(self.cb_scale, CB_ADDSTRING, 0, t.as_ptr() as isize);
            }
            let _ = send_msg(self.cb_scale, CB_SETCURSEL, 0, 0); // 默认 100%

            self.child("STATIC", "帧率:", WINDOW_STYLE(0), 16, 14, 48, 20, 0);
            self.child("STATIC", "缩放:", WINDOW_STYLE(0), 156, 14, 48, 20, 0);

            self.btn_select = self.child(
                "BUTTON",
                "选择区域...",
                WINDOW_STYLE(0),
                300, 10, 104, 30, IDC_BTN_SELECT,
            );
            self.lbl_region =
                self.child("STATIC", "区域: 全部屏幕", WINDOW_STYLE(0), 16, 50, 388, 20, 0);
            self.btn_start = self.child(
                "BUTTON",
                "● 开始录制 (F9)",
                WINDOW_STYLE(0),
                16, 82, 388, 46, IDC_BTN_START,
            );
            self.btn_cancel = self.child(
                "BUTTON",
                "取消录制 (F10)",
                WINDOW_STYLE(0),
                16, 140, 388, 32, IDC_BTN_CANCEL,
            );
            let _ = EnableWindow(self.btn_cancel, false);
            self.lbl_status = self.child(
                "STATIC",
                "就绪。F9 开始/停止录制，F10 取消。",
                WINDOW_STYLE(0),
                16, 190, 388, 40, 0,
            );
        }
    }

    unsafe fn set_status(&self, text: &str) {
        unsafe {
            let t = to_wide(text);
            let _ = SetWindowTextW(self.lbl_status, PCWSTR(t.as_ptr()));
        }
    }

    unsafe fn update_region_label(&self) {
        unsafe {
            let t = match self.region {
                Some((x, y, w, h)) => format!("区域: {w}×{h} @({x},{y})"),
                None => "区域: 全部屏幕".to_string(),
            };
            let t = to_wide(&t);
            let _ = SetWindowTextW(self.lbl_region, PCWSTR(t.as_ptr()));
        }
    }
}

fn send_msg(hwnd: HWND, msg: u32, wp: usize, lp: isize) -> LRESULT {
    unsafe {
        windows::Win32::UI::WindowsAndMessaging::SendMessageW(
            hwnd,
            msg,
            Some(WPARAM(wp)),
            Some(LPARAM(lp)),
        )
    }
}

unsafe fn app_of(hwnd: HWND) -> Option<&'static mut App> {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if p == 0 {
            None
        } else {
            Some(&mut *(p as *mut App))
        }
    }
}

// ============================================================
// 录制控制
// ============================================================

unsafe fn start_recording(app: &mut App) {
    unsafe {
        let fps = FPS_OPTIONS
            .get(send_msg(app.cb_fps, CB_GETCURSEL, 0, 0).0.max(0) as usize)
            .copied()
            .unwrap_or(15);
        let scale = SCALE_OPTIONS
            .get(send_msg(app.cb_scale, CB_GETCURSEL, 0, 0).0.max(0) as usize)
            .copied()
            .unwrap_or(100);
        let cfg = Config { region: app.region, fps, scale_percent: scale };

        app.rec = Some(Recorder::start(cfg, app.hwnd));
        app.rec_started = Some(Instant::now());

        let t = to_wide("■ 停止并保存 (F9)");
        let _ = SetWindowTextW(app.btn_start, PCWSTR(t.as_ptr()));
        let _ = EnableWindow(app.btn_cancel, true);
        let _ = EnableWindow(app.btn_select, false);
        // 隐藏控制窗口，避免入镜
        let _ = ShowWindow(app.hwnd, SW_HIDE);
    }
}

unsafe fn stop_recording(app: &mut App) {
    if let Some(rec) = app.rec.as_mut() {
        rec.stop();
    }
}

unsafe fn cancel_recording(app: &mut App) {
    if let Some(rec) = app.rec.as_mut() {
        rec.cancel();
    }
}

unsafe fn on_rec_done(app: &mut App) {
    unsafe {
        let Some(rec) = app.rec.take() else { return };
        let outcome = rec.finish();
        app.rec_started = None;

        let _ = ShowWindow(app.hwnd, SW_SHOW);
        let _ = SetForegroundWindow(app.hwnd);
        let t = to_wide("● 开始录制 (F9)");
        let _ = SetWindowTextW(app.btn_start, PCWSTR(t.as_ptr()));
        let _ = EnableWindow(app.btn_cancel, false);
        let _ = EnableWindow(app.btn_select, true);

        match outcome {
            Outcome::Saved { tmp, frames, ms } => {
                let secs = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0);
                let suggest = format!("gifrec_{secs}.gif");
                match save_dialog(app.hwnd, &suggest) {
                    Some(dst) => match move_file(&tmp, &dst) {
                        Ok(()) => {
                            let kb = std::fs::metadata(&dst).map(|m| m.len()).unwrap_or(0) / 1024;
                            let s = ms / 1000;
                            app.set_status(&format!(
                                "已保存 {}（{} 帧，{}分{:02}秒，{} KB）",
                                dst.display(),
                                frames,
                                s / 60,
                                s % 60,
                                kb
                            ));
                        }
                        Err(_) => {
                            app.set_status(&format!("移动文件失败，已保留: {}", tmp.display()));
                        }
                    },
                    None => {
                        app.set_status(&format!("已保留临时文件: {}", tmp.display()));
                    }
                }
            }
            Outcome::Cancelled => app.set_status("已取消录制。"),
            Outcome::Failed(e) => {
                let t = to_wide(&e);
                let cap = to_wide("录制失败");
                let _ = MessageBoxW(
                    Some(app.hwnd),
                    PCWSTR(t.as_ptr()),
                    PCWSTR(cap.as_ptr()),
                    MB_OK | MB_ICONERROR,
                );
                app.set_status("录制失败。");
            }
        }
    }
}

unsafe fn save_dialog(owner: HWND, suggest: &str) -> Option<PathBuf> {
    unsafe {
        let mut name = [0u16; 1024];
        for (i, c) in suggest.encode_utf16().take(name.len() - 1).enumerate() {
            name[i] = c;
        }
        let filter = to_wide("GIF 动画\0*.gif\0所有文件\0*.*\0");
        let defext = to_wide("gif");
        let mut ofn = OPENFILENAMEW {
            lStructSize: std::mem::size_of::<OPENFILENAMEW>() as u32,
            hwndOwner: owner,
            lpstrFilter: PCWSTR(filter.as_ptr()),
            lpstrFile: windows::core::PWSTR(name.as_mut_ptr()),
            nMaxFile: name.len() as u32,
            lpstrDefExt: PCWSTR(defext.as_ptr()),
            Flags: OFN_OVERWRITEPROMPT | OFN_PATHMUSTEXIST | OFN_HIDEREADONLY,
            ..Default::default()
        };
        if GetSaveFileNameW(&mut ofn).as_bool() {
            let end = name.iter().position(|&c| c == 0).unwrap_or(name.len());
            Some(PathBuf::from(String::from_utf16_lossy(&name[..end])))
        } else {
            None
        }
    }
}

fn move_file(src: &std::path::Path, dst: &std::path::Path) -> std::io::Result<()> {
    match std::fs::rename(src, dst) {
        Ok(()) => Ok(()),
        // 跨卷 rename 失败 → 复制后删除
        Err(_) => {
            std::fs::copy(src, dst)?;
            std::fs::remove_file(src)
        }
    }
}

unsafe fn update_rec_status(app: &App) {
    unsafe {
        let Some(rec) = app.rec.as_ref() else { return };
        let Some(t0) = app.rec_started else { return };
        let el = t0.elapsed().as_secs();
        let frames = rec.frames.load(std::sync::atomic::Ordering::Relaxed);
        let kb = std::fs::metadata(&rec.tmp).map(|m| m.len()).unwrap_or(0) / 1024;
        app.set_status(&format!(
            "录制中 {:02}:{:02} · {} 帧 · {} KB",
            el / 60,
            el % 60,
            frames,
            kb
        ));
    }
}

// ============================================================
// 区域选择窗口
// ============================================================

struct SelState {
    owner: HWND,
    vx: i32,
    vy: i32,
    anchor: Option<(i32, i32)>,
    cur: RECT,
}

unsafe fn open_region_selector(app: &App) {
    unsafe {
        let (vx, vy, vw, vh) = (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        );
        let state = Box::new(SelState {
            owner: app.hwnd,
            vx,
            vy,
            anchor: None,
            cur: RECT::default(),
        });
        let cls = to_wide(CLASS_SEL);
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            vx,
            vy,
            vw,
            vh,
            Some(app.hwnd),
            None,
            Some(app.hinst),
            Some(Box::into_raw(state) as *const c_void),
        )
        .expect("create selector window");
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 110, LWA_ALPHA);
        let _ = SetForegroundWindow(hwnd);
    }
}

unsafe extern "system" fn sel_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lp.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1), // 统一在 WM_PAINT 绘制，避免闪烁
            WM_PAINT => {
                let st = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SelState);
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                draw_selector(dc, &ps.rcPaint, st);
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let st = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SelState);
                let _ = SetCapture(hwnd);
                st.anchor =
                    Some(((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32));
                LRESULT(0)
            }
            WM_MOUSEMOVE => {
                let st = &mut *(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *mut SelState);
                if let Some((ax, ay)) = st.anchor {
                    let (mx, my) = ((lp.0 & 0xFFFF) as i32, ((lp.0 >> 16) & 0xFFFF) as i32);
                    st.cur = RECT {
                        left: ax.min(mx),
                        top: ay.min(my),
                        right: ax.max(mx),
                        bottom: ay.max(my),
                    };
                    let _ = InvalidateRect(Some(hwnd), None, true);
                }
                LRESULT(0)
            }
            WM_LBUTTONUP => {
                let st = &*(GetWindowLongPtrW(hwnd, GWLP_USERDATA) as *const SelState);
                let _ = ReleaseCapture();
                let r = st.cur;
                let (w, h) = (r.right - r.left, r.bottom - r.top);
                if w >= 16 && h >= 16 {
                    *PICKED.lock().unwrap() = Some((st.vx + r.left, st.vy + r.top, w, h));
                    let _ =
                        PostMessageW(Some(st.owner), WM_REGION_PICKED, WPARAM(1), LPARAM(0));
                }
                let _ = DestroyWindow(hwnd); // SelState 在 WM_DESTROY 统一释放
                LRESULT(0)
            }
            WM_KEYDOWN => {
                if wp.0 as u16 == VK_ESCAPE.0 {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    drop(Box::from_raw(p as *mut SelState));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn draw_selector(dc: HDC, rc: &RECT, st: &SelState) {
    unsafe {
        // 半透明黑底（窗口整体 alpha 已设）
        let black = HBRUSH(GetStockObject(BLACK_BRUSH).0);
        FillRect(dc, rc, black);

        // 顶部提示横幅
        let white = HBRUSH(GetStockObject(WHITE_BRUSH).0);
        let banner = RECT {
            left: rc.left,
            top: rc.top,
            right: rc.right,
            bottom: rc.top + 40,
        };
        FillRect(dc, &banner, white);
        let mut txt: Vec<u16> = "拖拽框选录制区域，Esc 取消".encode_utf16().collect();
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, COLORREF(0));
        let mut b = banner;
        let _ = DrawTextW(dc, &mut txt, &mut b, DT_CENTER | DT_SINGLELINE);

        // 选区白框
        let r = st.cur;
        if r.right > r.left && r.bottom > r.top {
            for inset in [0i32, -2] {
                let _ = windows::Win32::Graphics::Gdi::Rectangle(
                    dc,
                    r.left - inset,
                    r.top - inset,
                    r.right + inset + 1,
                    r.bottom + inset + 1,
                );
            }
            // 尺寸标签（选区右下角，放不下则挪到上方）
            let label = format!("{} × {}", r.right - r.left, r.bottom - r.top);
            let mut ltxt: Vec<u16> = label.encode_utf16().collect();
            let mut lrc = RECT::default();
            let _ = DrawTextW(dc, &mut ltxt, &mut lrc, DT_CALCRECT);
            let tw = lrc.right - lrc.left + 12;
            let th = lrc.bottom - lrc.top + 6;
            let mut lbox = RECT {
                left: r.right - tw,
                top: r.bottom + 4,
                right: r.right,
                bottom: r.bottom + 4 + th,
            };
            if lbox.bottom > rc.bottom {
                lbox.top = r.top - 4 - th;
                lbox.bottom = r.top - 4;
            }
            FillRect(dc, &lbox, white);
            let _ = DrawTextW(dc, &mut ltxt, &mut lbox, DT_CENTER | DT_SINGLELINE);
        }
    }
}

// ============================================================
// 主窗口
// ============================================================

unsafe extern "system" fn main_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lp.0 as *const CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                let app = &mut *(cs.lpCreateParams as *mut App);
                app.hwnd = hwnd;
                app.create_font();
                app.create_controls();
                LRESULT(0)
            }
            WM_COMMAND => {
                let Some(app) = app_of(hwnd) else { return LRESULT(0) };
                let id = (wp.0 & 0xFFFF) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as i32;
                if code == BN_CLICKED as i32 {
                    match id {
                        IDC_BTN_SELECT => on_select_clicked(app),
                        IDC_BTN_START => {
                            if app.rec.is_some() {
                                stop_recording(app);
                            } else {
                                start_recording(app);
                            }
                        }
                        IDC_BTN_CANCEL => cancel_recording(app),
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                let Some(app) = app_of(hwnd) else { return LRESULT(0) };
                match wp.0 {
                    1 => {
                        if app.rec.is_some() {
                            stop_recording(app);
                        } else {
                            start_recording(app);
                        }
                    }
                    2 => cancel_recording(app),
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if let Some(app) = app_of(hwnd) {
                    update_rec_status(app);
                }
                LRESULT(0)
            }
            recorder::WM_REC_DONE => {
                if let Some(app) = app_of(hwnd) {
                    on_rec_done(app);
                }
                LRESULT(0)
            }
            WM_REGION_PICKED => {
                let Some(app) = app_of(hwnd) else { return LRESULT(0) };
                if wp.0 == 1 {
                    app.region = PICKED.lock().unwrap().take();
                    let t = to_wide("清除区域（恢复全屏）");
                    let _ = SetWindowTextW(app.btn_select, PCWSTR(t.as_ptr()));
                    app.update_region_label();
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if let Some(app) = app_of(hwnd) {
                    if let Some(rec) = app.rec.as_mut() {
                        rec.cancel(); // 线程自行清理临时文件
                    }
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    drop(Box::from_raw(p as *mut App));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn on_select_clicked(app: &mut App) {
    unsafe {
        if app.region.is_some() {
            // 已有区域 → 点击清除
            app.region = None;
            let t = to_wide("选择区域...");
            let _ = SetWindowTextW(app.btn_select, PCWSTR(t.as_ptr()));
            app.update_region_label();
        } else {
            open_region_selector(app);
        }
    }
}

fn register_class(
    hinst: HINSTANCE,
    name: &str,
    proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT,
    cursor: PCWSTR,
) -> u16 {
    unsafe {
        let mut cls = to_wide(name);
        cls.pop();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, cursor).unwrap_or_default(),
            hbrBackground: HBRUSH(16usize as *mut c_void), // COLOR_BTNFACE + 1
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc)
    }
}

fn main() {
    unsafe {
        let hmodule = GetModuleHandleW(None).expect("GetModuleHandleW");
        let hinst = HINSTANCE(hmodule.0);

        let dpi = {
            let dc = GetDC(None);
            let d = GetDeviceCaps(Some(dc), LOGPIXELSY).max(96) as u32;
            let _ = windows::Win32::Graphics::Gdi::ReleaseDC(None, dc);
            d
        };

        register_class(hinst, CLASS_MAIN, main_wndproc, IDC_ARROW);
        register_class(hinst, CLASS_SEL, sel_wndproc, IDC_CROSS);

        let app = Box::new(App::new(hinst, dpi));

        // 客户区 420×252（逻辑）→ 外框尺寸
        let mut rect = RECT { left: 0, top: 0, right: 420, bottom: 252 };
        let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
        let _ = AdjustWindowRect(&mut rect, style, false);
        let w = (rect.right - rect.left) * dpi as i32 / 96;
        let h = (rect.bottom - rect.top) * dpi as i32 / 96;

        let mut title = to_wide("BitGalaxy GIF 录屏");
        title.pop();
        let mut cls = to_wide(CLASS_MAIN);
        cls.pop();
        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            PCWSTR(cls.as_ptr()),
            PCWSTR(title.as_ptr()),
            style,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            w,
            h,
            None,
            None,
            Some(hinst),
            Some(Box::into_raw(app) as *const c_void),
        )
        .expect("CreateWindowExW");

        let _ = RegisterHotKey(Some(hwnd), 1, MOD_NOREPEAT, VK_F9.0 as u32);
        let _ = RegisterHotKey(Some(hwnd), 2, MOD_NOREPEAT, VK_F10.0 as u32);
        let _ = SetTimer(Some(hwnd), 1, 500, None);

        let _ = ShowWindow(hwnd, SW_SHOW);
        let _ = UpdateWindow(hwnd);

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).0 > 0 {
            let _ = TranslateMessage(&msg);
            let _ = DispatchMessageW(&msg);
        }
    }
}
