//! BitGalaxy.GifRecorder —— 轻量级 GIF 录屏工具。
//! 控制窗口（暗色现代 UI）+ 系统托盘 + 全局热键（F9 开始/停止，F10 取消）+ 拖拽选区 + 保存对话框。
//! 录制期间：区域红色脉冲边框 + 悬浮控制条（画在区域外侧，不入镜）+ 托盘图标/时长反馈。
//! 录制产物：无损帧缓存 → 编辑器（预览/剪切/裁切）→ GIF 导出。
//! 主窗口「MCP · AI 集成」面板：一键把本程序注册进 Trae / Claude Desktop / Cursor 等
//! AI 客户端的 MCP 配置（安装状态实时显示，可一键卸载）。
//!
//! 控制台子系统：CLI/MCP 调用可被 shell 正常等待并取回退出码；
//! GUI 模式（无参数）以 DETACHED_PROCESS 分离重启自身，避免常驻控制台窗口。

/// GUI 分离重启的内部标记参数
const ARG_GUI: &str = "__bgr_gui__";

mod automation;
mod cache;
mod capture;
mod cli;
mod editor;
mod export;
mod gif;
mod json;
mod mcp;
mod mcp_install;
mod overlay;
mod recorder;
mod tray;

use std::ffi::c_void;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering as AtomicOrdering};
use std::sync::Mutex;
use std::time::Instant;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HWND, HINSTANCE, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Dwm::{
    DwmSetWindowAttribute, DWMWA_USE_IMMERSIVE_DARK_MODE, DWMWA_WINDOW_CORNER_PREFERENCE,
    DWM_WINDOW_CORNER_PREFERENCE,
};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CreateFontW, CreatePen, CreateSolidBrush, DeleteObject, DrawTextW, EndPaint,
    FillRect, GetDC, GetDeviceCaps, GetStockObject, InvalidateRect, LOGPIXELSY, SelectObject,
    SetBkColor, SetBkMode, SetTextColor, UpdateWindow, BLACK_BRUSH, CLEARTYPE_QUALITY,
    CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, DRAW_TEXT_FORMAT, DT_CALCRECT, DT_CENTER, DT_LEFT,
    DT_RIGHT, DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD, FW_NORMAL, HBRUSH,
    HDC, HFONT, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, TRANSPARENT, WHITE_BRUSH,
};
use windows::Win32::System::Diagnostics::Debug::MessageBeep;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::Dialogs::{
    GetSaveFileNameW, OFN_HIDEREADONLY, OFN_OVERWRITEPROMPT, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows::Win32::UI::Controls::{
    SetWindowTheme, BST_CHECKED, DRAWITEMSTRUCT, MEASUREITEMSTRUCT, ODS_DISABLED, ODS_SELECTED,
    ODT_BUTTON, ODT_COMBOBOX,
};
use windows::Win32::UI::Input::KeyboardAndMouse::{
    EnableWindow, IsWindowEnabled, RegisterHotKey, ReleaseCapture, SetCapture, UnregisterHotKey,
    MOD_NOREPEAT, VK_ESCAPE, VK_F10, VK_F9,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRect, BM_GETCHECK, BN_CLICKED, BS_AUTOCHECKBOX, BS_OWNERDRAW, CB_ADDSTRING,
    CB_GETCURSEL, CB_GETLBTEXT, CB_GETLBTEXTLEN, CB_SETCURSEL, CBS_DROPDOWNLIST,
    CBS_OWNERDRAWFIXED, CreateWindowExW, CREATESTRUCTW, CS_HREDRAW, CS_VREDRAW, CW_USEDEFAULT,
    DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, GetSystemMetrics,
    GetWindowLongPtrW, GetWindowTextLengthW, GetWindowTextW, GWLP_USERDATA, HICON, HMENU,
    IDC_ARROW, IDC_CROSS, IsWindowVisible, KillTimer, LoadCursorW, LoadIconW, MB_ICONASTERISK,
    MB_ICONERROR, MB_ICONWARNING, MB_OK, MB_OKCANCEL, MessageBoxW, PostMessageW, PostQuitMessage,
    RegisterClassW, RegisterWindowMessageW, SetForegroundWindow, SetLayeredWindowAttributes,
    SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow, TranslateMessage, IDOK, LWA_ALPHA,
    MSG, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SW_HIDE,
    SW_SHOW, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY,
    WM_DRAWITEM, WM_ERASEBKGND, WM_HOTKEY, WM_KEYDOWN, WM_LBUTTONDBLCLK, WM_LBUTTONDOWN,
    WM_LBUTTONUP, WM_MEASUREITEM, WM_MOUSEMOVE, WM_PAINT, WM_RBUTTONUP, WM_SETFONT, WM_TIMER,
    WNDCLASSW, WS_CAPTION, WS_CHILD, WS_EX_LAYERED, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_MINIMIZEBOX, WS_OVERLAPPED, WS_POPUP, WS_SYSMENU, WS_TABSTOP, WS_VISIBLE,
    WM_CTLCOLORBTN, WM_CTLCOLORLISTBOX, WM_CTLCOLORSTATIC,
};

use recorder::{Config, Outcome, Recorder};

const CLASS_MAIN: &str = "BGR_MAIN";
const CLASS_SEL: &str = "BGR_SEL";

const IDC_BTN_SELECT: i32 = 1001;
const IDC_BTN_START: i32 = 1002;
const IDC_BTN_CANCEL: i32 = 1003;
const IDC_BTN_MCP_INSTALL: i32 = 1004;
const IDC_BTN_MCP_UNINSTALL: i32 = 1005;
const IDC_CB_FPS: i32 = 1011;
const IDC_CB_SCALE: i32 = 1012;

const WM_REGION_PICKED: u32 = WM_APP + 0x11;

const FPS_OPTIONS: [u32; 7] = [8, 10, 12, 15, 20, 25, 30];
const SCALE_OPTIONS: [u32; 3] = [100, 75, 50];

// ============================================================
// 暗色主题（自绘主窗口 + 自绘按钮/下拉框）
// ============================================================

/// 0x00BBGGRR
const fn rgb(r: u32, g: u32, b: u32) -> COLORREF {
    COLORREF(((b & 0xFF) << 16) | ((g & 0xFF) << 8) | (r & 0xFF))
}

const CLR_BG: COLORREF = rgb(30, 31, 36); // 窗口底
const CLR_CARD: COLORREF = rgb(38, 40, 47); // 卡片底
const CLR_CARD_BORDER: COLORREF = rgb(53, 56, 66); // 卡片描边
const CLR_FIELD: COLORREF = rgb(45, 48, 56); // 下拉框底
const CLR_FIELD_BORDER: COLORREF = rgb(61, 65, 76);
const CLR_TEXT: COLORREF = rgb(232, 234, 237);
const CLR_TEXT_SUB: COLORREF = rgb(154, 160, 170);
const CLR_TEXT_DIM: COLORREF = rgb(106, 111, 123); // 禁用态文字
const CLR_SEL_HIGHLIGHT: COLORREF = rgb(57, 64, 77); // 下拉列表悬停/选中

// 按钮（按下 = 加深）
const BTN_PRIMARY: COLORREF = rgb(196, 43, 44); // 录制红
const BTN_PRIMARY_DOWN: COLORREF = rgb(158, 34, 35);
const BTN_ACCENT: COLORREF = rgb(47, 111, 222); // MCP 安装蓝
const BTN_ACCENT_DOWN: COLORREF = rgb(37, 88, 176);
const BTN_SEC_BG: COLORREF = rgb(46, 49, 56);
const BTN_SEC_BG_DOWN: COLORREF = rgb(38, 41, 49);
const BTN_SEC_BORDER: COLORREF = rgb(63, 67, 78);
const CLR_WHITE: COLORREF = rgb(255, 255, 255);
const CLR_BTN_SEC_TEXT: COLORREF = rgb(216, 219, 226);

/// 主窗口逻辑布局（96dpi 基准，实际随 DPI 缩放）
mod layout {
    pub const W: i32 = 420;
    pub const H: i32 = 476;
    /// 卡片 1（录制设置）
    pub const CARD1: (i32, i32, i32, i32) = (12, 64, 396, 240);
    /// 卡片 2（MCP · AI 集成）
    pub const CARD2: (i32, i32, i32, i32) = (12, 316, 396, 140);
}

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
    font_title: HFONT,
    font_sec: HFONT,
    font_btn: HFONT,
    font_small: HFONT,
    brush_card: HBRUSH,
    brush_field: HBRUSH,
    brush_bg: HBRUSH,
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
    // MCP 面板
    lbl_mcp_status: HWND,
    btn_mcp_install: HWND,
    btn_mcp_uninstall: HWND,
}

impl App {
    fn new(hinst: HINSTANCE, dpi: u32) -> Self {
        Self {
            hinst,
            hwnd: HWND::default(),
            dpi,
            font: HFONT::default(),
            font_title: HFONT::default(),
            font_sec: HFONT::default(),
            font_btn: HFONT::default(),
            font_small: HFONT::default(),
            brush_card: HBRUSH(std::ptr::null_mut()),
            brush_field: HBRUSH(std::ptr::null_mut()),
            brush_bg: HBRUSH(std::ptr::null_mut()),
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
            lbl_mcp_status: HWND::default(),
            btn_mcp_install: HWND::default(),
            btn_mcp_uninstall: HWND::default(),
        }
    }

    /// 逻辑坐标（96dpi 基准）→ 物理像素
    fn s(&self, v: i32) -> i32 {
        v * self.dpi as i32 / 96
    }

    unsafe fn make_font(&mut self, height: i32, weight: i32) -> HFONT {
        unsafe {
            let mut face = [0u16; 32];
            for (i, c) in "Microsoft YaHei UI".encode_utf16().take(31).enumerate() {
                face[i] = c;
            }
            CreateFontW(
                -self.s(height),
                0,
                0,
                0,
                weight,
                0,
                0,
                0,
                windows::Win32::Graphics::Gdi::DEFAULT_CHARSET,
                OUT_DEFAULT_PRECIS,
                CLIP_DEFAULT_PRECIS,
                CLEARTYPE_QUALITY,
                (DEFAULT_PITCH.0 | FF_DONTCARE.0) as u32,
                PCWSTR::from_raw(face.as_ptr()),
            )
        }
    }

    unsafe fn create_font(&mut self) {
        unsafe {
            self.font = self.make_font(12, FW_NORMAL.0 as i32);
            self.font_title = self.make_font(17, FW_BOLD.0 as i32);
            self.font_sec = self.make_font(12, FW_BOLD.0 as i32);
            self.font_btn = self.make_font(13, FW_BOLD.0 as i32);
            self.font_small = self.make_font(11, FW_NORMAL.0 as i32);
            self.brush_card = CreateSolidBrush(CLR_CARD);
            self.brush_field = CreateSolidBrush(CLR_FIELD);
            self.brush_bg = CreateSolidBrush(CLR_BG);
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
            // —— 卡片 1：录制设置 ——
            // 自绘下拉框（CBS_OWNERDRAWFIXED：闭合态与列表项都由主窗口绘制）
            let cb_style = WINDOW_STYLE((CBS_DROPDOWNLIST | CBS_OWNERDRAWFIXED) as u32) | WS_TABSTOP;
            self.cb_fps = self.child("COMBOBOX", "", cb_style, 66, 104, 62, 200, IDC_CB_FPS);
            // Win11 22H2+：边框与系统箭头也切暗色（旧系统调用失败自动忽略）
            let _ = SetWindowTheme(self.cb_fps, w!("DarkMode_CFD"), None);
            for f in FPS_OPTIONS {
                let t = to_wide(&f.to_string());
                let _ = send_msg(self.cb_fps, CB_ADDSTRING, 0, t.as_ptr() as isize);
            }
            let _ = send_msg(self.cb_fps, CB_SETCURSEL, 3, 0); // 默认 15fps

            self.cb_scale = self.child("COMBOBOX", "", cb_style, 188, 104, 62, 200, IDC_CB_SCALE);
            let _ = SetWindowTheme(self.cb_scale, w!("DarkMode_CFD"), None);
            for f in SCALE_OPTIONS {
                let t = to_wide(&format!("{}%", f));
                let _ = send_msg(self.cb_scale, CB_ADDSTRING, 0, t.as_ptr() as isize);
            }
            let _ = send_msg(self.cb_scale, CB_SETCURSEL, 0, 0); // 默认 100%

            self.child("STATIC", "帧率", WINDOW_STYLE(0), 28, 109, 34, 16, 0);
            self.child("STATIC", "缩放", WINDOW_STYLE(0), 150, 109, 34, 16, 0);

            self.btn_select = self.child(
                "BUTTON",
                "选择区域...",
                WINDOW_STYLE(BS_OWNERDRAW as u32),
                276, 104, 120, 26, IDC_BTN_SELECT,
            );
            self.lbl_region = self.child(
                "STATIC",
                "区域: 全部屏幕",
                WINDOW_STYLE(0),
                28, 142, 356, 16, 0,
            );
            self.btn_start = self.child(
                "BUTTON",
                "●  开始录制 (F9)",
                WINDOW_STYLE(BS_OWNERDRAW as u32),
                28, 166, 356, 42, IDC_BTN_START,
            );
            self.btn_cancel = self.child(
                "BUTTON",
                "取消录制 (F10)",
                WINDOW_STYLE(BS_OWNERDRAW as u32),
                28, 216, 356, 26, IDC_BTN_CANCEL,
            );
            let _ = EnableWindow(self.btn_cancel, false);
            self.cb_countdown = self.child(
                "BUTTON",
                "开始前倒计时 3 秒",
                WINDOW_STYLE(BS_AUTOCHECKBOX as u32),
                28, 250, 200, 18, 0,
            );
            // 暗色主题复选框（不支持的旧系统自动回退经典样式）
            let _ = SetWindowTheme(self.cb_countdown, w!("DarkMode_Explorer"), None);
            self.lbl_status = self.child(
                "STATIC",
                "就绪。F9 开始/停止录制，F10 取消。",
                // SS_EDITCONTROL：让静态文本按宽度换行
                WINDOW_STYLE(0x2000),
                28, 268, 356, 30, 0,
            );

            // —— 卡片 2：MCP · AI 集成 ——
            self.child(
                "STATIC",
                "内置 MCP 服务器（stdio，7 个工具）：AI 客户端可直接录屏 / 截图并导出 GIF。",
                WINDOW_STYLE(0x2000),
                28, 354, 356, 30, 0,
            );
            self.lbl_mcp_status = self.child(
                "STATIC",
                "",
                WINDOW_STYLE(0x2000),
                28, 390, 356, 30, 0,
            );
            self.btn_mcp_install = self.child(
                "BUTTON",
                "一键安装 MCP",
                WINDOW_STYLE(BS_OWNERDRAW as u32),
                28, 424, 132, 26, IDC_BTN_MCP_INSTALL,
            );
            self.btn_mcp_uninstall = self.child(
                "BUTTON",
                "卸载",
                WINDOW_STYLE(BS_OWNERDRAW as u32),
                168, 424, 96, 26, IDC_BTN_MCP_UNINSTALL,
            );
            self.refresh_mcp_status();
        }
    }

    /// 刷新 MCP 注册状态行与卸载按钮可用性
    unsafe fn refresh_mcp_status(&mut self) {
        unsafe {
            let t = to_wide(&mcp_install::status_text());
            let _ = SetWindowTextW(self.lbl_mcp_status, PCWSTR(t.as_ptr()));
            let any = mcp_install::any_installed();
            let _ = EnableWindow(self.btn_mcp_uninstall, any);
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

        let t = to_wide("■  停止并保存 (F9)");
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
        let t = to_wide("●  开始录制 (F9)");
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
// MCP 面板：一键安装 / 卸载
// ============================================================

unsafe fn on_mcp_install(app: &mut App) {
    unsafe {
        let r = mcp_install::install();
        app.refresh_mcp_status();
        let msg = r.install_message();
        if r.ok.is_empty() {
            let t = to_wide(&msg);
            let cap = to_wide("MCP 安装失败");
            let _ = MessageBoxW(
                Some(app.hwnd),
                PCWSTR(t.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_OK | MB_ICONERROR,
            );
        } else if let Some(t) = app.tray.as_mut() {
            t.balloon("MCP 已安装", &msg, r.failed.is_empty());
        }
    }
}

unsafe fn on_mcp_uninstall(app: &mut App) {
    unsafe {
        let r = mcp_install::uninstall();
        app.refresh_mcp_status();
        let msg = r.uninstall_message();
        if r.ok.is_empty() {
            let t = to_wide(&msg);
            let cap = to_wide("MCP 卸载");
            let _ = MessageBoxW(
                Some(app.hwnd),
                PCWSTR(t.as_ptr()),
                PCWSTR(cap.as_ptr()),
                MB_OK | MB_ICONWARNING,
            );
        } else if let Some(t) = app.tray.as_mut() {
            t.balloon("MCP 已卸载", &msg, r.failed.is_empty());
        }
    }
}

// ============================================================
// 主窗口暗色自绘（背景 / 标题 / 卡片 / 自绘按钮 / 自绘下拉框）
// ============================================================

/// 主窗口整体绘制：底色 + 页头 + 两张圆角卡片
unsafe fn paint_main(dc: HDC, app: &App) {
    unsafe {
        // 1) 底色
        let mut rc = RECT::default();
        let _ = windows::Win32::UI::WindowsAndMessaging::GetClientRect(app.hwnd, &mut rc);
        FillRect(dc, &rc, app.brush_bg);

        // 2) 页头
        let title = to_wide("BitGalaxy GIF 录屏");
        draw_text_at(
            dc,
            app.font_title,
            CLR_TEXT,
            &title,
            20, 12, 400, 38,
            DT_LEFT | DT_SINGLELINE,
        );
        let sub = to_wide(&format!(
            "轻量级 GIF 录屏 · 纯 Rust 单文件 · v{}",
            env!("CARGO_PKG_VERSION")
        ));
        draw_text_at(
            dc,
            app.font_small,
            CLR_TEXT_SUB,
            &sub,
            21, 40, 400, 20,
            DT_LEFT | DT_SINGLELINE,
        );

        // 3) 卡片
        draw_card(dc, app, layout::CARD1);
        draw_card(dc, app, layout::CARD2);

        // 4) 卡片标题
        let sec1 = to_wide("录制设置");
        draw_text_at(dc, app.font_sec, CLR_TEXT, &sec1, 28, 76, 200, 18, DT_LEFT | DT_SINGLELINE);
        let sec2 = to_wide("MCP · AI 集成");
        draw_text_at(dc, app.font_sec, CLR_TEXT, &sec2, 28, 328, 200, 18, DT_LEFT | DT_SINGLELINE);
        let badge = to_wide("stdio · 7 个工具");
        draw_text_at(
            dc,
            app.font_small,
            CLR_TEXT_SUB,
            &badge,
            260, 331, 134, 16,
            DT_RIGHT | DT_SINGLELINE,
        );
    }
}

unsafe fn draw_text_at(
    dc: HDC,
    font: HFONT,
    color: COLORREF,
    text: &[u16],
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    flags: DRAW_TEXT_FORMAT,
) {
    unsafe {
        let old = SelectObject(dc, font.into());
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, color);
        let mut rc = RECT { left: x, top: y, right: x + w, bottom: y + h };
        let mut buf = text.to_vec();
        let _ = DrawTextW(dc, &mut buf, &mut rc, flags);
        let _ = SelectObject(dc, old);
    }
}

/// 圆角卡片：填充 + 描边
unsafe fn draw_card(dc: HDC, app: &App, (x, y, w, h): (i32, i32, i32, i32)) {
    unsafe {
        let (sx, sy, sw, sh) = (app.s(x), app.s(y), app.s(w), app.s(h));
        let r = app.s(10);
        let brush = CreateSolidBrush(CLR_CARD);
        let pen = CreatePen(PS_SOLID, 1, CLR_CARD_BORDER);
        let old_b = SelectObject(dc, brush.into());
        let old_p = SelectObject(dc, pen.into());
        let _ = windows::Win32::Graphics::Gdi::RoundRect(dc, sx, sy, sx + sw, sy + sh, r * 2, r * 2);
        let _ = SelectObject(dc, old_b);
        let _ = SelectObject(dc, old_p);
        let _ = DeleteObject(pen.into());
        let _ = DeleteObject(brush.into());
    }
}

/// 自绘按钮：主操作（红）/ 强调（蓝）/ 次要（灰）
unsafe fn draw_button(app: &App, dis: &DRAWITEMSTRUCT) {
    unsafe {
        let dc = dis.hDC;
        let rc = dis.rcItem;
        let id = dis.CtlID as i32;
        let down = (dis.itemState.0 & ODS_SELECTED.0) != 0;
        let disabled =
            (dis.itemState.0 & ODS_DISABLED.0) != 0 || !IsWindowEnabled(dis.hwndItem).as_bool();

        let (fill, border, textc, bold, radius) = match id {
            IDC_BTN_START => {
                (if down { BTN_PRIMARY_DOWN } else { BTN_PRIMARY }, BTN_PRIMARY, CLR_WHITE, true, app.s(9))
            }
            IDC_BTN_MCP_INSTALL => {
                (if down { BTN_ACCENT_DOWN } else { BTN_ACCENT }, BTN_ACCENT, CLR_WHITE, false, app.s(7))
            }
            _ => (
                if down { BTN_SEC_BG_DOWN } else { BTN_SEC_BG },
                BTN_SEC_BORDER,
                if disabled { CLR_TEXT_DIM } else { CLR_BTN_SEC_TEXT },
                false,
                app.s(7),
            ),
        };

        let brush = CreateSolidBrush(fill);
        let pen = CreatePen(PS_SOLID, 1, border);
        let old_b = SelectObject(dc, brush.into());
        let old_p = SelectObject(dc, pen.into());
        let _ = windows::Win32::Graphics::Gdi::RoundRect(
            dc, rc.left, rc.top, rc.right, rc.bottom, radius * 2, radius * 2,
        );
        let _ = SelectObject(dc, old_b);
        let _ = SelectObject(dc, old_p);
        let _ = DeleteObject(pen.into());
        let _ = DeleteObject(brush.into());

        // 按钮文字
        let len = GetWindowTextLengthW(dis.hwndItem).max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let n = GetWindowTextW(dis.hwndItem, &mut buf);
        let text = &buf[..n.max(0) as usize];
        let font = if bold { app.font_btn } else { app.font };
        let old_f = SelectObject(dc, font.into());
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, textc);
        let mut trc = RECT {
            left: rc.left + 2,
            top: rc.top + (if down { 1 } else { 0 }),
            right: rc.right - 2,
            bottom: rc.bottom,
        };
        let _ = DrawTextW(dc, &mut text.to_vec(), &mut trc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
        let _ = SelectObject(dc, old_f);
    }
}

/// 自绘下拉框：闭合态（含右侧箭头）与展开列表项
unsafe fn draw_combo(app: &App, dis: &DRAWITEMSTRUCT) {
    unsafe {
        let dc = dis.hDC;
        let rc = dis.rcItem;
        let closed = dis.itemID == 0xFFFF_FFFF;

        // 背板
        let (bg, show_border) = if closed {
            (CLR_FIELD, true)
        } else if (dis.itemState.0 & ODS_SELECTED.0) != 0 {
            (CLR_SEL_HIGHLIGHT, false)
        } else {
            (CLR_CARD, false)
        };
        let brush = CreateSolidBrush(bg);
        let pen = if show_border { Some(CreatePen(PS_SOLID, 1, CLR_FIELD_BORDER)) } else { None };
        let old_p = pen.map(|p| SelectObject(dc, p.into()));
        let old_b = SelectObject(dc, brush.into());
        let radius = app.s(5);
        let _ = windows::Win32::Graphics::Gdi::RoundRect(
            dc, rc.left, rc.top, rc.right, rc.bottom, radius * 2, radius * 2,
        );
        let _ = SelectObject(dc, old_b);
        let _ = DeleteObject(brush.into());
        if let (Some(p), Some(old)) = (pen, old_p) {
            let _ = SelectObject(dc, old);
            let _ = DeleteObject(p.into());
        }

        // 文本
        let text: Vec<u16> = if closed {
            combo_text(dis.hwndItem, None)
        } else {
            combo_text(dis.hwndItem, Some(dis.itemID))
        };
        let old_f = SelectObject(dc, app.font.into());
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, if text.is_empty() { CLR_TEXT_DIM } else { CLR_TEXT });
        let mut trc = RECT {
            left: rc.left + app.s(8),
            top: rc.top,
            right: rc.right - app.s(16),
            bottom: rc.bottom,
        };
        let _ = DrawTextW(dc, &mut text.to_vec(), &mut trc, DT_LEFT | DT_VCENTER | DT_SINGLELINE);
        let _ = SelectObject(dc, old_f);

        // 闭合态右侧下拉箭头
        if closed {
            let mut arrow = to_wide("▾");
            arrow.pop(); // 去掉结尾 NUL
            let old_f2 = SelectObject(dc, app.font_small.into());
            let _ = SetTextColor(dc, CLR_TEXT_SUB);
            let mut arc = RECT {
                left: rc.right - app.s(18),
                top: rc.top,
                right: rc.right - app.s(4),
                bottom: rc.bottom,
            };
            let _ = DrawTextW(dc, &mut arrow, &mut arc, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
            let _ = SelectObject(dc, old_f2);
        }
    }
}

/// 取下拉框文本：None = 当前选中项；Some(i) = 第 i 项
fn combo_text(hwnd: HWND, index: Option<u32>) -> Vec<u16> {
    {
        let idx = match index {
            Some(i) => i as isize,
            None => send_msg(hwnd, CB_GETCURSEL, 0, 0).0,
        };
        if idx < 0 {
            return vec![];
        }
        let len = send_msg(hwnd, CB_GETLBTEXTLEN, idx as usize, 0).0.max(0) as usize;
        let mut buf = vec![0u16; len + 1];
        let _ = send_msg(hwnd, CB_GETLBTEXT, idx as usize, buf.as_mut_ptr() as isize);
        let end = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
        buf[..end].to_vec()
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
                        IDC_BTN_MCP_INSTALL => on_mcp_install(app),
                        IDC_BTN_MCP_UNINSTALL => on_mcp_uninstall(app),
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
            // —— 暗色自绘 ——
            WM_ERASEBKGND => LRESULT(1), // 统一在 WM_PAINT 绘制，避免闪烁
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                if let Some(app) = app_of(hwnd) {
                    paint_main(dc, app);
                }
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_DRAWITEM => {
                let dis = &*(lp.0 as *const DRAWITEMSTRUCT);
                if dis.CtlType == ODT_BUTTON {
                    if let Some(app) = app_of(hwnd) {
                        draw_button(app, dis);
                    }
                    LRESULT(1)
                } else if dis.CtlType == ODT_COMBOBOX {
                    if let Some(app) = app_of(hwnd) {
                        draw_combo(app, dis);
                    }
                    LRESULT(1)
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            WM_MEASUREITEM => {
                // 自绘下拉框行高
                let mi = &mut *(lp.0 as *mut MEASUREITEMSTRUCT);
                if mi.CtlType == ODT_COMBOBOX {
                    if let Some(app) = app_of(hwnd) {
                        mi.itemHeight = app.s(20) as u32;
                    }
                }
                LRESULT(1)
            }
            // 静态文本（全部位于卡片上）：卡片底色 + 浅色文字
            WM_CTLCOLORSTATIC => {
                let dc = HDC(wp.0 as *mut c_void);
                let _ = SetTextColor(dc, CLR_TEXT_SUB);
                let _ = SetBkColor(dc, CLR_CARD);
                if let Some(app) = app_of(hwnd) {
                    LRESULT(app.brush_card.0 as isize)
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            // 复选框文字背景
            WM_CTLCOLORBTN => {
                let dc = HDC(wp.0 as *mut c_void);
                let _ = SetTextColor(dc, CLR_TEXT_SUB);
                let _ = SetBkColor(dc, CLR_CARD);
                if let Some(app) = app_of(hwnd) {
                    LRESULT(app.brush_card.0 as isize)
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
            }
            // 下拉列表背景
            WM_CTLCOLORLISTBOX => {
                let dc = HDC(wp.0 as *mut c_void);
                let _ = SetTextColor(dc, CLR_TEXT);
                let _ = SetBkColor(dc, CLR_CARD);
                if let Some(app) = app_of(hwnd) {
                    LRESULT(app.brush_card.0 as isize)
                } else {
                    DefWindowProcW(hwnd, msg, wp, lp)
                }
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
                        // 释放字体与画刷
                        for f in [app.font, app.font_title, app.font_sec, app.font_btn, app.font_small] {
                            let _ = DeleteObject(f.into());
                        }
                        for b in [app.brush_card, app.brush_field, app.brush_bg] {
                            let _ = DeleteObject(b.into());
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
    brush: HBRUSH,
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
            hbrBackground: brush,
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc)
    }
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == ARG_GUI {
        // 分离重启后的 GUI 子进程（无控制台）
        unsafe { gui_main() };
    } else if args.is_empty() {
        // GUI 模式：分离重启自身后立即退出（失败则原地启动）
        unsafe {
            if !relaunch_gui() {
                gui_main();
            }
        }
    } else {
        // 命令行参数 → CLI / MCP 模式（供 AI 与脚本调用）
        std::process::exit(cli::run(&args));
    }
}

/// 以 DETACHED_PROCESS 重启自身进入 GUI 模式。
/// 双击启动：父进程控制台窗口只闪现一帧即随退出关闭；从 shell 启动：立即归还提示符。
/// 返回 false = 重启失败（调用方应原地运行 GUI 兜底）。
unsafe fn relaunch_gui() -> bool {
    use windows::core::PWSTR;
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::LibraryLoader::GetModuleFileNameW;
    use windows::Win32::System::Threading::{
        CreateProcessW, DETACHED_PROCESS, PROCESS_INFORMATION, STARTUPINFOW,
    };
    unsafe {
        let mut path = [0u16; 1024];
        let n = GetModuleFileNameW(None, &mut path) as usize;
        if n == 0 || n >= path.len() {
            return false;
        }
        let exe = String::from_utf16_lossy(&path[..n]);
        let app = to_wide(&exe);
        let mut cmd = to_wide(&format!("\"{exe}\" {ARG_GUI}"));
        let si = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            ..Default::default()
        };
        let mut pi = PROCESS_INFORMATION::default();
        let ok = CreateProcessW(
            PCWSTR(app.as_ptr()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            DETACHED_PROCESS,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
        .is_ok();
        if ok {
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
        }
        ok
    }
}

unsafe fn gui_main() {
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
        // 主窗口自绘暗色背景（类画刷置空）；选区窗口自带半透明黑底
        register_class(hinst, CLASS_MAIN, main_wndproc, IDC_ARROW, app_icon, HBRUSH(std::ptr::null_mut()));
        register_class(
            hinst,
            CLASS_SEL,
            sel_wndproc,
            IDC_CROSS,
            HICON::default(),
            HBRUSH(16usize as *mut c_void), // COLOR_BTNFACE + 1
        );
        overlay::register_classes(hinst);
        editor::register_class(hinst);

        let app = Box::new(App::new(hinst, dpi));

        // 客户区（逻辑）→ 外框尺寸
        let mut rect = RECT { left: 0, top: 0, right: layout::W, bottom: layout::H };
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

        // Win10 1809+：暗色标题栏；Win11：圆角窗口（旧系统调用失败自动忽略）
        let dark = windows::core::BOOL(1);
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_USE_IMMERSIVE_DARK_MODE,
            &dark as *const _ as *const c_void,
            std::mem::size_of_val(&dark) as u32,
        );
        let round = DWM_WINDOW_CORNER_PREFERENCE(2); // DWMWCP_ROUND
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &round as *const _ as *const c_void,
            std::mem::size_of_val(&round) as u32,
        );

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
