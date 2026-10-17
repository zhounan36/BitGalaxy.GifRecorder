//! 录制可视化覆盖层：区域红色脉冲边框、录制控制条、开始倒计时、选区预览边框。
//! 参考 ScreenToGif 的布局：
//! - 边框画在录制区域**外侧**（窗口挖空中心），永远不入镜；全屏录制时整体出屏、自动不可见
//! - 控制条贴在区域外侧（优先下方，其次上方），放不下（如全屏）则隐藏
//! - 倒计时圆窗悬于区域中心，3-2-1 后投递 WM_COUNTDOWN_DONE 开始录制
//! - 选好区域后、未开始录制时：静态蓝色预览边框持续显示选区（同样画在区域外侧，不入镜）

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{COLORREF, HWND, HINSTANCE, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    BeginPaint, CombineRgn, CreateEllipticRgn, CreateFontW, CreatePen, CreateRectRgn,
    CreateRoundRectRgn, CreateSolidBrush, DeleteObject, DrawTextW, Ellipse, EndPaint, FillRect,
    GetMonitorInfoW, InvalidateRect, LineTo, MonitorFromRect, MoveToEx, SelectObject, SetBkMode,
    SetTextColor, SetWindowRgn, CLEARTYPE_QUALITY, CLIP_DEFAULT_PRECIS, DEFAULT_PITCH, DT_CENTER,
    DT_SINGLELINE, DT_VCENTER, FF_DONTCARE, FW_BOLD, FW_NORMAL, HDC, HFONT, HGDIOBJ, MONITORINFO,
    MONITOR_DEFAULTTONEAREST, OUT_DEFAULT_PRECIS, PAINTSTRUCT, PS_SOLID, RGN_DIFF, TRANSPARENT,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, GetSystemMetrics, GetWindowLongPtrW, GWLP_USERDATA,
    KillTimer, PostMessageW, RegisterClassW, SetLayeredWindowAttributes, SetTimer,
    SetWindowLongPtrW, SetWindowPos, WM_APP, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_ERASEBKGND,
    WM_LBUTTONDOWN, WM_PAINT, WM_TIMER, LWA_ALPHA, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN,
    SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP,
    WS_VISIBLE,
};

use crate::to_wide;

pub const CLASS_BORDER: &str = "BGR_BORDER";
pub const CLASS_PILL: &str = "BGR_PILL";
pub const CLASS_COUNTDOWN: &str = "BGR_COUNTDOWN";
pub const CLASS_SEL_PREVIEW: &str = "BGR_SELPREV";

/// 倒计时结束 → 主窗口开始真正录制
pub const WM_COUNTDOWN_DONE: u32 = WM_APP + 0x12;

const CLR_RED: COLORREF = COLORREF(0x0023_11E8); // RGB(232,17,35)
const CLR_RED_DIM: COLORREF = COLORREF(0x001A_0C96); // 脉冲暗相
const CLR_DARK: COLORREF = COLORREF(0x0022_1814); // 深色底 RGB(20,24,34)
const CLR_GRAY_BTN: COLORREF = COLORREF(0x005A_4C46); // RGB(70,76,90)
const CLR_WHITE: COLORREF = COLORREF(0x00FF_FFFF);
/// 选区预览边框：提示蓝 RGB(0,120,215)，区别于录制中的红色脉冲
const CLR_SEL: COLORREF = COLORREF(0x00D7_7800);

/// 虚拟屏矩形 (x, y, w, h)
fn virt_rect() -> (i32, i32, i32, i32) {
    unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    }
}

unsafe fn create_ui_font(h: i32, weight: i32) -> HFONT {
    unsafe {
        let mut face = [0u16; 32];
        for (i, ch) in "Microsoft YaHei UI".encode_utf16().take(31).enumerate() {
            face[i] = ch;
        }
        CreateFontW(
            -h,
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

fn reg_class(hinst: HINSTANCE, name: &str, proc: unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT) {
    unsafe {
        let mut cls = to_wide(name);
        cls.pop();
        let wc = WNDCLASSW {
            lpfnWndProc: Some(proc),
            hInstance: hinst,
            lpszClassName: PCWSTR(cls.as_ptr()),
            ..Default::default()
        };
        RegisterClassW(&wc);
    }
}

pub fn register_classes(hinst: HINSTANCE) {
    reg_class(hinst, CLASS_BORDER, border_wndproc);
    reg_class(hinst, CLASS_PILL, pill_wndproc);
    reg_class(hinst, CLASS_COUNTDOWN, countdown_wndproc);
    reg_class(hinst, CLASS_SEL_PREVIEW, selprev_wndproc);
}

// ============================================================
// 区域脉冲边框
// ============================================================

struct BorderState {
    bright: bool,
}

/// 边框窗口覆盖“区域外扩 3px”的环形区域（SetWindowRgn 挖空中心），
/// 因此绘制内容始终在录制区域之外，不会进入 GIF。
pub unsafe fn create_border(
    owner: HWND,
    hinst: HINSTANCE,
    region: Option<(i32, i32, i32, i32)>,
    dpi: u32,
) -> HWND {
    unsafe {
        let (x, y, w, h) = region.unwrap_or_else(virt_rect);
        let bw = (3 * dpi as i32 / 96).max(2);
        let (wx, wy) = (x - bw, y - bw);
        let (ww, wh) = (w + bw * 2, h + bw * 2);
        let cls = to_wide(CLASS_BORDER);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT, // 点击穿透
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            wx,
            wy,
            ww,
            wh,
            Some(owner),
            None,
            Some(hinst),
            Some(Box::into_raw(Box::new(BorderState { bright: true })) as *const c_void),
        )
        .expect("create border");
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
        let outer = CreateRectRgn(0, 0, ww, wh);
        let inner = CreateRectRgn(bw, bw, ww - bw, wh - bw);
        CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
        let _ = SetWindowRgn(hwnd, Some(outer), true);
        let _ = SetTimer(Some(hwnd), 1, 500, None);
        hwnd
    }
}

unsafe extern "system" fn border_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lp.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                let bright = p != 0 && (*(p as *const BorderState)).bright;
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let brush = CreateSolidBrush(if bright { CLR_RED } else { CLR_RED_DIM });
                FillRect(dc, &ps.rcPaint, brush);
                let _ = DeleteObject(HGDIOBJ(brush.0));
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            WM_TIMER => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let st = &mut *(p as *mut BorderState);
                    st.bright = !st.bright;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    drop(Box::from_raw(p as *mut BorderState));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                let _ = KillTimer(Some(hwnd), 1);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ============================================================
// 选区常驻预览边框（选好区域后、未开始录制时持续显示）
// ============================================================

/// 与录制红色边框同构：环形窗口挖空中心，画在区域外侧（即使随后立刻开录也不入镜）、
/// 点击穿透不挡操作。区别：静态蓝色无脉冲，表示“已选区域，等待录制”。
/// 不设 owner —— 主窗口隐藏（最小化到托盘）时依然保持可见。
pub unsafe fn create_selection_preview(
    hinst: HINSTANCE,
    region: (i32, i32, i32, i32),
    dpi: u32,
) -> HWND {
    unsafe {
        let (x, y, w, h) = region;
        let bw = (3 * dpi as i32 / 96).max(2);
        let (wx, wy) = (x - bw, y - bw);
        let (ww, wh) = (w + bw * 2, h + bw * 2);
        let cls = to_wide(CLASS_SEL_PREVIEW);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_TRANSPARENT | WS_EX_NOACTIVATE,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            wx,
            wy,
            ww,
            wh,
            None,
            None,
            Some(hinst),
            None,
        )
        .expect("create selection preview");
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 235, LWA_ALPHA);
        let outer = CreateRectRgn(0, 0, ww, wh);
        let inner = CreateRectRgn(bw, bw, ww - bw, wh - bw);
        CombineRgn(Some(outer), Some(outer), Some(inner), RGN_DIFF);
        let _ = SetWindowRgn(hwnd, Some(outer), true);
        hwnd
    }
}

unsafe extern "system" fn selprev_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let mut ps = PAINTSTRUCT::default();
                let dc = BeginPaint(hwnd, &mut ps);
                let brush = CreateSolidBrush(CLR_SEL);
                FillRect(dc, &ps.rcPaint, brush);
                let _ = DeleteObject(HGDIOBJ(brush.0));
                let _ = EndPaint(hwnd, &ps);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

// ============================================================
// 录制控制条（状态 + 停止/取消按钮）
// ============================================================

const PILL_W: i32 = 270;
const PILL_H: i32 = 36;
const BTN_STOP_X: i32 = 200;
const BTN_CANCEL_X: i32 = 236;
const BTN_W: i32 = 32;

struct PillState {
    owner: HWND,
    cmd_stop: usize,
    cmd_cancel: usize,
    dpi: i32,
    text: Vec<u16>,
    blink: bool,
    font: HFONT,
}

/// 创建控制条；区域外侧无空间（如全屏）则返回 None。
/// cmd_stop/cmd_cancel 为点击按钮后经 WM_COMMAND 发给主窗口的命令 ID。
pub unsafe fn create_pill(
    owner: HWND,
    hinst: HINSTANCE,
    dpi: u32,
    region: Option<(i32, i32, i32, i32)>,
    cmd_stop: usize,
    cmd_cancel: usize,
) -> Option<HWND> {
    unsafe {
        let s = |v: i32| v * dpi as i32 / 96;
        let font = create_ui_font(s(12), FW_NORMAL.0 as i32);
        let state = Box::new(PillState {
            owner,
            cmd_stop,
            cmd_cancel,
            dpi: dpi as i32,
            text: to_wide("REC 00:00"),
            blink: true,
            font,
        });
        let cls = to_wide(CLASS_PILL);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_LAYERED | WS_EX_NOACTIVATE, // 不抢焦点
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP,
            0,
            0,
            s(PILL_W),
            s(PILL_H),
            Some(owner),
            None,
            Some(hinst),
            Some(Box::into_raw(state) as *const c_void),
        )
        .expect("create pill");
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 242, LWA_ALPHA);
        let rgn = CreateRoundRectRgn(0, 0, s(PILL_W) + 1, s(PILL_H) + 1, s(10), s(10));
        let _ = SetWindowRgn(hwnd, Some(rgn), true);
        let (rx, ry, rw, rh) = region.unwrap_or_else(virt_rect);
        if place_pill(hwnd, (rx, ry, rw, rh), s(PILL_W), s(PILL_H)) {
            let _ = SetTimer(Some(hwnd), 2, 500, None);
            Some(hwnd)
        } else {
            let _ = DestroyWindow(hwnd);
            None
        }
    }
}

/// 控制条贴区域外侧：优先区域下方、右对齐；下方放不下则上方；都不行（全屏）返回 false。
unsafe fn place_pill(hwnd: HWND, region: (i32, i32, i32, i32), w: i32, h: i32) -> bool {
    unsafe {
        let rc = RECT {
            left: region.0,
            top: region.1,
            right: region.0 + region.2,
            bottom: region.1 + region.3,
        };
        let mon = MonitorFromRect(&rc, MONITOR_DEFAULTTONEAREST);
        let mut mi = MONITORINFO {
            cbSize: std::mem::size_of::<MONITORINFO>() as u32,
            ..Default::default()
        };
        let _ = GetMonitorInfoW(mon, &mut mi);
        let wa = mi.rcWork;
        let gap = 6;
        let mut y = rc.bottom + gap;
        if y + h > wa.bottom {
            y = rc.top - gap - h;
        }
        if y < wa.top {
            return false;
        }
        let mut x = rc.right - w;
        x = x.clamp(wa.left, wa.right - w);
        let _ = SetWindowPos(hwnd, None, x, y, 0, 0, SWP_NOSIZE | SWP_NOZORDER | SWP_SHOWWINDOW);
        true
    }
}

/// 主窗口每 500ms 刷新控制条文本
pub unsafe fn pill_update(hwnd: HWND, text: &str) {
    unsafe {
        let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
        if p != 0 {
            let st = &mut *(p as *mut PillState);
            st.text = to_wide(text);
            let _ = InvalidateRect(Some(hwnd), None, false);
        }
    }
}

unsafe extern "system" fn pill_wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lp.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let mut ps = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut ps);
                    draw_pill(dc, &*(p as *const PillState));
                    let _ = EndPaint(hwnd, &ps);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let st = &mut *(p as *mut PillState);
                    st.blink = !st.blink;
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                LRESULT(0)
            }
            WM_LBUTTONDOWN => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let st = &*(p as *const PillState);
                    let s = |v: i32| v * st.dpi / 96;
                    let x = (lp.0 & 0xFFFF) as i32;
                    if x >= s(BTN_STOP_X) && x < s(BTN_STOP_X + BTN_W) {
                        let _ = PostMessageW(Some(st.owner), WM_COMMAND, WPARAM(st.cmd_stop), LPARAM(0));
                    } else if x >= s(BTN_CANCEL_X) && x < s(BTN_CANCEL_X + BTN_W) {
                        let _ = PostMessageW(Some(st.owner), WM_COMMAND, WPARAM(st.cmd_cancel), LPARAM(0));
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let st = Box::from_raw(p as *mut PillState);
                    let _ = DeleteObject(HGDIOBJ(st.font.0));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                let _ = KillTimer(Some(hwnd), 2);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn draw_pill(dc: HDC, st: &PillState) {
    unsafe {
        let s = |v: i32| v * st.dpi / 96;
        let (w, h) = (s(PILL_W), s(PILL_H));

        // 深色圆角底
        let bg = CreateSolidBrush(CLR_DARK);
        FillRect(dc, &RECT { left: 0, top: 0, right: w + 1, bottom: h + 1 }, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        // REC 闪烁圆点
        if st.blink {
            let dot = CreateSolidBrush(CLR_RED);
            let old = SelectObject(dc, HGDIOBJ(dot.0));
            let (cx, cy, r) = (s(14), h / 2, s(5));
            let _ = Ellipse(dc, cx - r, cy - r, cx + r + 1, cy + r + 1);
            let _ = SelectObject(dc, old);
            let _ = DeleteObject(HGDIOBJ(dot.0));
        }

        // 状态文本
        let old = SelectObject(dc, HGDIOBJ(st.font.0));
        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, CLR_WHITE);
        let mut txt = st.text.clone();
        let mut trc = RECT { left: s(24), top: 0, right: s(BTN_STOP_X - 4), bottom: h };
        let _ = DrawTextW(dc, &mut txt, &mut trc, DT_SINGLELINE | DT_VCENTER);
        let _ = SelectObject(dc, old);

        // 停止按钮：红底白 ■
        let stop = CreateSolidBrush(CLR_RED);
        FillRect(
            dc,
            &RECT {
                left: s(BTN_STOP_X) + 1,
                top: s(2),
                right: s(BTN_STOP_X + BTN_W) - 1,
                bottom: h - s(2),
            },
            stop,
        );
        let sq = CreateSolidBrush(CLR_WHITE);
        let (scx, scy) = (s(BTN_STOP_X + BTN_W / 2), h / 2);
        FillRect(
            dc,
            &RECT { left: scx - s(6), top: scy - s(6), right: scx + s(6), bottom: scy + s(6) },
            sq,
        );
        let _ = DeleteObject(HGDIOBJ(stop.0));
        let _ = DeleteObject(HGDIOBJ(sq.0));

        // 取消按钮：灰底白 ✕
        let can = CreateSolidBrush(CLR_GRAY_BTN);
        FillRect(
            dc,
            &RECT {
                left: s(BTN_CANCEL_X) + 1,
                top: s(2),
                right: s(BTN_CANCEL_X + BTN_W) - 1,
                bottom: h - s(2),
            },
            can,
        );
        let _ = DeleteObject(HGDIOBJ(can.0));
        let pen = CreatePen(PS_SOLID, s(2).max(2), CLR_WHITE);
        let old = SelectObject(dc, HGDIOBJ(pen.0));
        let (ccx, ccy, d) = (s(BTN_CANCEL_X + BTN_W / 2), h / 2, s(6));
        let _ = MoveToEx(dc, ccx - d, ccy - d, None);
        let _ = LineTo(dc, ccx + d, ccy + d);
        let _ = MoveToEx(dc, ccx - d, ccy + d, None);
        let _ = LineTo(dc, ccx + d, ccy - d);
        let _ = SelectObject(dc, old);
        let _ = DeleteObject(HGDIOBJ(pen.0));
    }
}

// ============================================================
// 开始倒计时
// ============================================================

const CD_SECS: i32 = 3;

struct CdState {
    n: i32,
    owner: HWND,
    dpi: i32,
    font_big: HFONT,
    font_small: HFONT,
}

pub unsafe fn create_countdown(
    owner: HWND,
    hinst: HINSTANCE,
    region: Option<(i32, i32, i32, i32)>,
    dpi: u32,
) -> HWND {
    unsafe {
        let s = |v: i32| v * dpi as i32 / 96;
        let (x, y, w, h) = region.unwrap_or_else(virt_rect);
        let size = s(180);
        let (cx, cy) = (x + w / 2 - size / 2, y + h / 2 - size / 2);
        let state = Box::new(CdState {
            n: CD_SECS,
            owner,
            dpi: dpi as i32,
            font_big: create_ui_font(s(84), FW_BOLD.0 as i32),
            font_small: create_ui_font(s(14), FW_NORMAL.0 as i32),
        });
        let cls = to_wide(CLASS_COUNTDOWN);
        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST
                | WS_EX_TOOLWINDOW
                | WS_EX_LAYERED
                | WS_EX_NOACTIVATE
                | WS_EX_TRANSPARENT,
            PCWSTR(cls.as_ptr()),
            PCWSTR::null(),
            WS_POPUP | WS_VISIBLE,
            cx,
            cy,
            size,
            size,
            Some(owner),
            None,
            Some(hinst),
            Some(Box::into_raw(state) as *const c_void),
        )
        .expect("create countdown");
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), 216, LWA_ALPHA);
        let rgn = CreateEllipticRgn(0, 0, size + 1, size + 1);
        let _ = SetWindowRgn(hwnd, Some(rgn), true);
        hwnd
    }
}

unsafe extern "system" fn countdown_wndproc(
    hwnd: HWND,
    msg: u32,
    wp: WPARAM,
    lp: LPARAM,
) -> LRESULT {
    unsafe {
        match msg {
            WM_CREATE => {
                let cs = &*(lp.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
                SetWindowLongPtrW(hwnd, GWLP_USERDATA, cs.lpCreateParams as isize);
                let _ = SetTimer(Some(hwnd), 1, 1000, None);
                LRESULT(0)
            }
            WM_ERASEBKGND => LRESULT(1),
            WM_PAINT => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let mut ps = PAINTSTRUCT::default();
                    let dc = BeginPaint(hwnd, &mut ps);
                    draw_countdown(dc, &*(p as *const CdState));
                    let _ = EndPaint(hwnd, &ps);
                }
                LRESULT(0)
            }
            WM_TIMER => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let st = &mut *(p as *mut CdState);
                    st.n -= 1;
                    if st.n <= 0 {
                        let owner = st.owner;
                        let _ = DestroyWindow(hwnd); // CdState 在 WM_DESTROY 释放
                        let _ = PostMessageW(Some(owner), WM_COUNTDOWN_DONE, WPARAM(0), LPARAM(0));
                    } else {
                        let _ = InvalidateRect(Some(hwnd), None, false);
                    }
                }
                LRESULT(0)
            }
            WM_DESTROY => {
                let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
                if p != 0 {
                    let st = Box::from_raw(p as *mut CdState);
                    let _ = DeleteObject(HGDIOBJ(st.font_big.0));
                    let _ = DeleteObject(HGDIOBJ(st.font_small.0));
                    SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
                }
                let _ = KillTimer(Some(hwnd), 1);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wp, lp),
        }
    }
}

unsafe fn draw_countdown(dc: HDC, st: &CdState) {
    unsafe {
        let s = |v: i32| v * st.dpi / 96;
        let size = s(180);
        let bg = CreateSolidBrush(CLR_DARK);
        FillRect(dc, &RECT { left: 0, top: 0, right: size + 1, bottom: size + 1 }, bg);
        let _ = DeleteObject(HGDIOBJ(bg.0));

        let _ = SetBkMode(dc, TRANSPARENT);
        let _ = SetTextColor(dc, CLR_RED);
        let old = SelectObject(dc, HGDIOBJ(st.font_big.0));
        let mut num = to_wide(&st.n.to_string());
        num.pop();
        let mut nrc = RECT { left: 0, top: s(18), right: size, bottom: s(128) };
        let _ = DrawTextW(dc, &mut num, &mut nrc, DT_SINGLELINE | DT_CENTER);
        let _ = SelectObject(dc, old);

        let _ = SetTextColor(dc, CLR_WHITE);
        let old = SelectObject(dc, HGDIOBJ(st.font_small.0));
        let mut tip: Vec<u16> = "即将开始录制".encode_utf16().collect();
        let mut trc = RECT { left: 0, top: s(124), right: size, bottom: s(156) };
        let _ = DrawTextW(dc, &mut tip, &mut trc, DT_SINGLELINE | DT_CENTER);
        let _ = SelectObject(dc, old);
    }
}
