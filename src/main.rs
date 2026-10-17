//! BitGalaxy.GifRecorder —— 轻量级 GIF 录屏工具。
//! 控制窗口 + 系统托盘 + 全局热键（F9 开始/停止，F10 取消）+ 拖拽选区 + 保存对话框。
//! 录制期间：区域红色脉冲边框 + 悬浮控制条（画在区域外侧，不入镜）+ 托盘图标/时长反馈。
//! 录制产物：无损帧缓存 → 编辑器（预览/剪切/裁切）→ GIF 导出。

// release 用 windows 子系统：双击运行不弹黑色控制台；debug 保留控制台便于调试
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod cache;
mod capture;
mod editor;
mod export;
mod gif;
mod overlay;
mod recorder;
mod tray;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Mutex;
use std::time::Instant;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, HINSTANCE, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, DrawTextW, EndPaint, FillRect, GetDC, GetDeviceCaps, GetStockObject,
    InvalidateRect, LOGPIXELSY, SetBkMode, SetTextColor, UpdateWindow, BLACK_BRUSH,
    CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, DT_CALCRECT, DT_CENTER, DT_SINGLELINE,
    FF_DONTCARE, FW_NORMAL, HBRUSH, HFONT, HDC, OUT_DEFAULT_PRECIS, PAINTSTRUCT, TRANSPARENT,
    WHITE_BRUSH,
};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_HIDEREADONLY, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Controls::BST_CHECKED;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey, MOD_NOREPEAT,
    VK_ESCAPE, VK_F10, VK_F9,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, BM_GETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, CB_ADDSTRING,
    CB_GETCURSEL, CB_SETCURSEL, CBS_DROPDOWNLIST, CreateWindowExW, CREATESTRUCTW, CS_HREDRAW,
    CS_VREDRAW, CW_USEDEFAULT, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    GetSystemMetrics, GetWindowLongPtrW, GWLP_USERDATA, HICON, HMENU, IDC_ARROW, IDC_CROSS,
    IsWindowVisible, KillTimer, LoadCursorW, LoadIconW, MB_ICONASTERISK, MB_ICONERROR, MB_ICONWARNING,
    MB_OK, MB_OKCANCEL, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetLayeredWindowAttributes,
    SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, IDOK, LWA_ALPHA,
    MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE,
    SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN, WM_LBUTTONUP,
    WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WM_SETFONT, WM_TIMER, WNDCLASSW, WS_CAPTION, WS_CHILD,
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

/// "TaskbarCreated" 广播消息号（explorer 重启后据此重新注册托盘图标）
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

pub(crate) fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 加载 exe 内嵌应用图标（IDI_APP=101，多尺寸）。
/// 标题栏/任务栏/Alt-Tab 取自窗口类图标；资源缺失时回退运行时手绘。
pub(crate) unsafe fn load_app_icon(hinst: HINSTANCE) -> HICON {
    unsafe {
        LoadIconW(Some(hinst), PCWSTR(101usize as *const u16))
            .unwrap_or_else(|_| tray::make_icon(false))
    }
}

struct App {
    hinst: HINSTANCE,
    hwnd: HWND,
    dpi: u32,
    font: HFONT,
    rec: Option<Recorder>,
    rec_started: Option<Instant>,
    region: Option<(i32, i32, i32, i32)>,
    // 托盘与录制可视化覆盖层
    tray: Option<tray::Tray>,
    border: Option<HWND>,
    pill: Option<HWND>,
    countdown: Option<HWND>,
    /// 选区常驻预览边框（选好后、未录制时显示）
    sel_prev: Option<HWND>,
    /// 录后编辑器窗口
    editor: Option<HWND>,
    /// “已最小化到托盘”提示只弹一次
    hint_shown: bool,
    // 控件
    cb_fps: HWND,
    cb_scale: HWND,
    btn_select: HWND,
    lbl_region: HWND,
    btn_start: HWND,
    btn_cancel: HWND,
    cb_countdown: HWND,
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
            tray: None,
            border: None,
            pill: None,
            countdown: None,
            sel_prev: None,
            editor: None,
            hint_shown: false,
            cb_fps: HWND::default(),
            cb_scale: HWND::default(),
            btn_select: HWND::default(),
            lbl_region: HWND::default(),
            btn_start: HWND::default(),
            btn_cancel: HWND::default(),
            cb_countdown: HWND::default(),
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
            self.cb_countdown = self.child(
                "BUTTON",
                "开始前倒计时 3 秒",
                WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                16, 186, 180, 20, 0,
            );
            self.lbl_status = self.child(
                "STATIC",
                "就绪。F9 开始/停止录制，F10 取消。",
                WINDOW_STYLE(0),
                16, 214, 388, 40, 0,
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
// 录制控制（状态机：空闲 → [倒计时] → 录制中 → 完成）
// ============================================================

/// F9 / 开始按钮 / 托盘菜单的统一入口
unsafe fn toggle_rec(app: &mut App) {
    unsafe {
        if app.countdown.is_some() {
            cancel_countdown(app);
        } else if app.rec.is_some() {
            stop_recording(app);
        } else {
            request_start(app);
        }
    }
}

/// 请求开始：按需先进入 3 秒倒计时（给用户把鼠标移到位的时间）
unsafe fn request_start(app: &mut App) {
    unsafe {
        let use_cd = send_msg(app.cb_countdown, BM_GETCHECK, 0, 0).0 == BST_CHECKED.0 as isize;
        let _ = ShowWindow(app.hwnd, SW_HIDE); // 提前隐藏，避免入镜
        hide_selection_preview(app); // 预览蓝框 → 录制红框
        app.border = Some(overlay::create_border(app.hwnd, app.hinst, app.region, app.dpi));
        if use_cd {
            app.countdown = Some(overlay::create_countdown(
                app.hwnd,
                app.hinst,
                app.region,
                app.dpi,
            ));
            if let Some(t) = app.tray.as_mut() {
                t.set_tip("准备录制…");
            }
        } else {
            begin_recording(app);
        }
    }
}

/// 真正启动录制线程，并点亮全部录制期反馈（边框/控制条/托盘）
unsafe fn begin_recording(app: &mut App) {
    unsafe {
        app.countdown = None;
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

        if app.border.is_none() {
            app.border = Some(overlay::create_border(app.hwnd, app.hinst, app.region, app.dpi));
        }
        app.pill = overlay::create_pill(
            app.hwnd,
            app.hinst,
            app.dpi,
            app.region,
            IDC_BTN_START as usize,
            IDC_BTN_CANCEL as usize,
        );
        if let Some(t) = app.tray.as_mut() {
            t.set_recording(true);
            t.set_tip("录制中 00:00");
        }
        // 全屏/满屏区域：控制条与边框都不可见 → 用气泡告知快捷键
        if app.pill.is_none() {
            if let Some(t) = app.tray.as_mut() {
                t.balloon(
                    "开始录制",
                    "正在录制。按 F9 停止并保存，F10 取消录制。",
                    false,
                );
            }
        }
    }
}

/// 取消倒计时，恢复空闲
unsafe fn cancel_countdown(app: &mut App) {
    unsafe {
        if let Some(h) = app.countdown.take() {
            let _ = DestroyWindow(h);
        }
        if let Some(h) = app.border.take() {
            let _ = DestroyWindow(h);
        }
        let _ = ShowWindow(app.hwnd, SW_SHOW);
        let _ = SetForegroundWindow(app.hwnd);
        app.set_status("已取消倒计时。");
        show_selection_preview(app); // 回到空闲态，恢复选区指示
        if let Some(t) = app.tray.as_mut() {
            t.set_recording(false);
            t.set_tip("BitGalaxy GIF 录屏 (F9)");
        }
    }
}

/// 销毁录制期覆盖层，恢复托盘为空闲态
unsafe fn destroy_overlays(app: &mut App) {
    unsafe {
        if let Some(h) = app.border.take() {
            let _ = DestroyWindow(h);
        }
        if let Some(h) = app.pill.take() {
            let _ = DestroyWindow(h);
        }
        if let Some(t) = app.tray.as_mut() {
            t.set_recording(false);
            t.set_tip("BitGalaxy GIF 录屏 (F9)");
        }
    }
}

/// 显示选区预览边框（已有选区且当前未显示时）
unsafe fn show_selection_preview(app: &mut App) {
    unsafe {
        if app.sel_prev.is_none() && app.region.is_some() {
            let r = app.region.unwrap();
            app.sel_prev = Some(overlay::create_selection_preview(app.hinst, r, app.dpi));
        }
    }
}

/// 销毁选区预览边框（开始录制 / 清除区域 / 退出时）
unsafe fn hide_selection_preview(app: &mut App) {
    unsafe {
        if let Some(h) = app.sel_prev.take() {
            let _ = DestroyWindow(h);
        }
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

/// 托盘“退出”：录制中需确认丢弃；倒计时直接放弃
unsafe fn exit_app(hwnd: HWND) {
    unsafe {
        if let Some(app) = app_of(hwnd) {
            if let Some(h) = app.countdown.take() {
                let _ = DestroyWindow(h);
            }
            if let Some(h) = app.border.take() {
                let _ = DestroyWindow(h);
            }
            if let Some(h) = app.editor.take() {
                let _ = DestroyWindow(h);
            }
            if app.rec.is_some() {
                let txt = to_wide("正在录制，退出将丢弃当前录制。确定要退出吗？");
                let cap = to_wide("退出确认");
                if MessageBoxW(
                    Some(hwnd),
                    PCWSTR(txt.as_ptr()),
                    PCWSTR(cap.as_ptr()),
                    MB_OKCANCEL | MB_ICONWARNING,
                ) != IDOK {
                    return;
                }
                cancel_recording(app); // 线程自行删除临时文件
            }
        }
        let _ = DestroyWindow(hwnd);
    }
}

unsafe fn on_rec_done(app: &mut App) {
    unsafe {
        let Some(rec) = app.rec.take() else { return };
        let outcome = rec.finish();
        app.rec_started = None;

        destroy_overlays(app);
        show_selection_preview(app); // 区域仍保留，恢复选区指示
        let t = to_wide("● 开始录制 (F9)");
        let _ = SetWindowTextW(app.btn_start, PCWSTR(t.as_ptr()));
        let _ = EnableWindow(app.btn_cancel, false);
        let _ = EnableWindow(app.btn_select, true);

        match outcome {
            // 录制完成 → 打开编辑器（预览/剪切/裁切后导出）；主窗口保持隐藏
            Outcome::Recorded { cache, frames, ms, truncated } => {
                app.editor = editor::open_editor(app.hwnd, app.hinst, app.dpi, cache, frames, ms, truncated);
                if app.editor.is_none() {
                    // 编辑器打不开（缓存损坏等）→ 回主窗口
                    let _ = ShowWindow(app.hwnd, SW_SHOW);
                    let _ = SetForegroundWindow(app.hwnd);
                    app.set_status("录制完成，但帧缓存无法打开。");
                }
            }
            Outcome::Cancelled => {
                let _ = ShowWindow(app.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(app.hwnd);
                app.set_status("已取消录制。");
            }
            Outcome::Failed(e) => {
                let _ = ShowWindow(app.hwnd, SW_SHOW);
                let _ = SetForegroundWindow(app.hwnd);
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

pub(crate) unsafe fn save_dialog(owner: HWND, suggest: &str) -> Option<PathBuf> {
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

fn fmt_kb(kb: u64) -> String {
    if kb < 1024 {
        format!("{kb} KB")
    } else {
        format!("{:.1} MB", kb as f64 / 1024.0)
    }
}

struct RecStats {
    mm: u64,
    ss: u64,
    frames: u64,
    kb: u64,
}

fn rec_stats(app: &App) -> Option<RecStats> {
    let rec = app.rec.as_ref()?;
    let t0 = app.rec_started?;
    let el = t0.elapsed().as_secs();
    let frames = rec.frames.load(AtomicOrdering::Relaxed);
    let kb = std::fs::metadata(&rec.cache_path).map(|m| m.len()).unwrap_or(0) / 1024;
    Some(RecStats { mm: el / 60, ss: el % 60, frames, kb })
}

unsafe fn update_rec_status(app: &App) {
    unsafe {
        let Some(st) = rec_stats(app) else { return };
        app.set_status(&format!(
            "录制中 {:02}:{:02} · {} 帧 · {}",
            st.mm,
            st.ss,
            st.frames,
            fmt_kb(st.kb)
        ));
    }
}

/// 覆盖层/托盘文案：(tooltip, 控制条文本)
fn rec_overlay_texts(app: &App) -> Option<(String, String)> {
    let st = rec_stats(app)?;
    let mmss = format!("{:02}:{:02}", st.mm, st.ss);
    Some((
        format!("录制中 {mmss}"),
        format!("REC {mmss} · {} 帧 · {}", st.frames, fmt_kb(st.kb)),
    ))
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
                // 常驻托盘图标
                let mut t = tray::Tray::new(hwnd);
                t.add();
                app.tray = Some(t);
                LRESULT(0)
            }
            WM_COMMAND => {
                let Some(app) = app_of(hwnd) else { return LRESULT(0) };
                let id = (wp.0 & 0xFFFF) as i32;
                let code = ((wp.0 >> 16) & 0xFFFF) as i32;
                if code == BN_CLICKED as i32 {
                    match id {
                        IDC_BTN_SELECT => on_select_clicked(app),
                        IDC_BTN_START => toggle_rec(app),
                        IDC_BTN_CANCEL => cancel_recording(app),
                        tray::IDM_SHOW => {
                            let _ = ShowWindow(hwnd, SW_SHOW);
                            let _ = SetForegroundWindow(hwnd);
                        }
                        tray::IDM_TOGGLE_REC => toggle_rec(app),
                        tray::IDM_CANCEL_REC => cancel_recording(app),
                        tray::IDM_EXIT => exit_app(hwnd),
                        _ => {}
                    }
                }
                LRESULT(0)
            }
            WM_HOTKEY => {
                let Some(app) = app_of(hwnd) else { return LRESULT(0) };
                match wp.0 {
                    1 => toggle_rec(app),
                    2 => {
                        if app.countdown.is_some() {
                            cancel_countdown(app);
                        } else {
                            cancel_recording(app);
                        }
                    }
                    _ => {}
                }
                LRESULT(0)
            }
            WM_TIMER => {
                if let Some(app) = app_of(hwnd) {
                    if app.rec.is_some() {
                        update_rec_status(app);
                        if let Some((tip, pill_text)) = rec_overlay_texts(app) {
                            if let Some(p) = app.pill {
                                overlay::pill_update(p, &pill_text);
                            }
                            if let Some(t) = app.tray.as_mut() {
                                t.set_tip(&tip);
                            }
                        }
                    }
                }
                LRESULT(0)
            }
            recorder::WM_REC_DONE => {
                if let Some(app) = app_of(hwnd) {
                    on_rec_done(app);
                }
                LRESULT(0)
            }
            // 编辑器关闭（wp: 1=已导出 0=已丢弃）
            editor::WM_EDITOR_DONE => {
                if let Some(app) = app_of(hwnd) {
                    app.editor = None;
                    let exported = wp.0 == 1;
                    let msg = editor::EXPORT_MSG.lock().unwrap().clone();
                    let _ = ShowWindow(app.hwnd, SW_SHOW);
                    let _ = SetForegroundWindow(app.hwnd);
                    app.set_status(&msg);
                    if exported {
                        if let Some(t) = app.tray.as_mut() {
                            t.balloon("GIF 已导出", &msg, false);
                        }
                        let _ = MessageBeep(MB_ICONASTERISK);
                    }
                }
                LRESULT(0)
            }
            overlay::WM_COUNTDOWN_DONE => {
                if let Some(app) = app_of(hwnd) {
                    app.countdown = None;
                    begin_recording(app);
                }
                LRESULT(0)
            }
            tray::WM_TRAYICON => {
                let Some(app) = app_of(hwnd) else { return LRESULT(0) };
                let rec = app.rec.is_some() || app.countdown.is_some();
                match lp.0 as u32 {
                    // 空闲时左键 = 切换主窗口；录制中左键 = 菜单（避免误显示窗口入镜）
                    WM_LBUTTONUP if !rec => {
                        if IsWindowVisible(hwnd).as_bool() {
                            let _ = ShowWindow(hwnd, SW_HIDE);
                        } else {
                            let _ = ShowWindow(hwnd, SW_SHOW);
                            let _ = SetForegroundWindow(hwnd);
                        }
                    }
                    WM_LBUTTONUP | WM_RBUTTONUP => tray::show_context_menu(hwnd, app.rec.is_some()),
                    WM_LBUTTONDBLCLK => {
                        let _ = ShowWindow(hwnd, SW_SHOW);
                        let _ = SetForegroundWindow(hwnd);
                    }
                    _ => {}
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
                    show_selection_preview(app); // 持续显示选区，直到开始录制
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                // 关闭 = 最小化到托盘（录制不受影响，托盘/控制条仍可控）
                if let Some(app) = app_of(hwnd) {
                    let _ = ShowWindow(hwnd, SW_HIDE);
                    if !app.hint_shown {
                        app.hint_shown = true;
                        if let Some(t) = app.tray.as_mut() {
                            t.balloon(
                                "已最小化到托盘",
                                "程序仍在后台运行，右键托盘图标可打开菜单或退出。\nF9/F10 快捷键始终可用。",
                                false,
                            );
                        }
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = UnregisterHotKey(Some(hwnd), 1);
                let _ = UnregisterHotKey(Some(hwnd), 2);
                let _ = KillTimer(Some(hwnd), 1);
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    {
                        let app = &mut *(p as *mut App);
                        if let Some(h) = app.sel_prev.take() {
                            let _ = DestroyWindow(h); // 预览边框无 owner，需手动销毁
                        }
                        if let Some(mut t) = app.tray.take() {
                            t.remove();
                            t.destroy();
                        }
                    }
                    drop(Box::from_raw(p as *mut App));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                PostQuitMessage(0);
                LRESULT(0)
            }
            // explorer 重启 → 托盘图标补注册
            _ if msg == TASKBAR_CREATED.load(AtomicOrdering::Relaxed) && msg != 0 => {
                if let Some(app) = app_of(hwnd) {
                    if let Some(t) = app.tray.as_mut() {
                        t.add();
                    }
                }
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
            hide_selection_preview(app);
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
    icon: HICON,
) -> u16 {
    unsafe {
        let mut cls = to_wide(name);
        cls.pop();
        let wc = WNDCLASSW {
            style: CS_HREDRAW | CS_VREDRAW,
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            hCursor: LoadCursorW(None, cursor).unwrap_or_default(),
            hIcon: icon,
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

        // explorer 重启后的托盘补注册广播
        let mut tbc = to_wide("TaskbarCreated");
        tbc.pop();
        TASKBAR_CREATED.store(RegisterWindowMessageW(PCWSTR(tbc.as_ptr())), AtomicOrdering::Relaxed);

        let app_icon = load_app_icon(hinst);
        register_class(hinst, CLASS_MAIN, main_wndproc, IDC_ARROW, app_icon);
        register_class(hinst, CLASS_SEL, sel_wndproc, IDC_CROSS, HICON::default());
        overlay::register_classes(hinst);
        editor::register_class(hinst);

        let app = Box::new(App::new(hinst, dpi));

        // 客户区 420×284（逻辑）→ 外框尺寸
        let mut rect = RECT { left: 0, top: 0, right: 420, bottom: 284 };
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
