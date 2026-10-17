//! 系统托盘：运行时绘制图标（免 .ico 资源）、气泡通知、右键菜单。
//! 参考 ShareX / ScreenToGif 的托盘交互：
//! - 左键单击 = 切换主窗口；右键 = 菜单；录制中左键也弹菜单（避免误显示窗口入镜）
//! - 录制时图标切换为红点，tooltip 实时显示时长
//! - 结束（保存/失败/保留临时文件）用气泡 + 系统提示音反馈

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, LPARAM, POINT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateDIBSection, DeleteObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB,
    DIB_RGB_COLORS, HBITMAP, HGDIOBJ,
};
use windows::Win32::UI::Shell::{
    Shell_NotifyIconW, NOTIFYICONDATAW, NIF_ICON, NIF_INFO, NIF_MESSAGE, NIF_TIP, NIIF_INFO,
    NIIF_WARNING, NIM_ADD, NIM_DELETE, NIM_MODIFY,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreateIconIndirect, CreatePopupMenu, DestroyIcon, DestroyMenu, GetCursorPos,
    PostMessageW, SetForegroundWindow, TrackPopupMenu, HICON, ICONINFO, MF_GRAYED, MF_SEPARATOR,
    MF_STRING, TPM_BOTTOMALIGN, TPM_RETURNCMD, TPM_RIGHTBUTTON, WM_APP, WM_COMMAND, WM_NULL,
};

use crate::to_wide;

/// 托盘回调消息（lParam = 鼠标消息）
pub const WM_TRAYICON: u32 = WM_APP + 0x30;

// 托盘菜单命令（经 WM_COMMAND 送回主窗口，与按钮共用处理逻辑）
pub const IDM_SHOW: i32 = 2101;
pub const IDM_TOGGLE_REC: i32 = 2102;
pub const IDM_CANCEL_REC: i32 = 2103;
pub const IDM_EXIT: i32 = 2104;

const TIP_DEFAULT: &str = "BitGalaxy GIF 录屏 (F9)";

pub struct Tray {
    nid: NOTIFYICONDATAW,
    icon_idle: HICON,
    icon_rec: HICON,
}

fn fill_str(buf: &mut [u16], s: &str) {
    for (i, c) in s.encode_utf16().take(buf.len() - 1).enumerate() {
        buf[i] = c;
    }
}

impl Tray {
    pub unsafe fn new(hwnd: HWND) -> Tray {
        unsafe {
            let icon_idle = make_icon(false);
            let icon_rec = make_icon(true);
            let mut nid = NOTIFYICONDATAW::default();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = 1;
            nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
            nid.uCallbackMessage = WM_TRAYICON;
            nid.hIcon = icon_idle;
            fill_str(&mut nid.szTip, TIP_DEFAULT);
            Tray { nid, icon_idle, icon_rec }
        }
    }

    pub unsafe fn add(&mut self) -> bool {
        unsafe { Shell_NotifyIconW(NIM_ADD, &self.nid).as_bool() }
    }

    pub unsafe fn remove(&mut self) {
        unsafe {
            let _ = Shell_NotifyIconW(NIM_DELETE, &self.nid);
        }
    }

    pub unsafe fn destroy(self) {
        unsafe {
            let _ = DestroyIcon(self.icon_idle);
            let _ = DestroyIcon(self.icon_rec);
        }
    }

    /// 切换录制/空闲图标
    pub unsafe fn set_recording(&mut self, on: bool) {
        unsafe {
            self.nid.hIcon = if on { self.icon_rec } else { self.icon_idle };
            self.nid.uFlags = NIF_ICON;
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid);
        }
    }

    pub unsafe fn set_tip(&mut self, tip: &str) {
        unsafe {
            self.nid.uFlags = NIF_TIP;
            fill_str(&mut self.nid.szTip, tip);
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid);
        }
    }

    pub unsafe fn balloon(&mut self, title: &str, text: &str, warning: bool) {
        unsafe {
            self.nid.uFlags = NIF_INFO;
            self.nid.dwInfoFlags = if warning { NIIF_WARNING } else { NIIF_INFO };
            fill_str(&mut self.nid.szInfoTitle, title);
            fill_str(&mut self.nid.szInfo, text);
            self.nid.Anonymous.uTimeout = 10;
            let _ = Shell_NotifyIconW(NIM_MODIFY, &self.nid);
        }
    }
}

/// 运行时绘制 32×32 ARGB 图标：圆角深蓝底 + 中央圆（空闲=白圈，录制=红点）。
/// CreateIconIndirect + 32bpp DIB（alpha 通道）是标准的免资源图标做法。
pub unsafe fn make_icon(recording: bool) -> HICON {
    const S: i32 = 32;
    // 距离场抗锯齿：底=圆角方形 #22314F，标记=圆心图案
    let mut px = vec![0u8; (S * S * 4) as usize];
    let c = (S - 1) as f32 / 2.0;
    let r_corner = 7.5f32;
    let flat = (c - r_corner).max(0.0);
    for y in 0..S {
        for x in 0..S {
            let (fx, fy) = (x as f32, y as f32);
            let dx = ((fx - c).abs() - flat).max(0.0);
            let dy = ((fy - c).abs() - flat).max(0.0);
            let d_rect = (dx * dx + dy * dy).sqrt();
            let a_rect = (r_corner - d_rect + 0.5).clamp(0.0, 1.0);
            let d_c = ((fx - c) * (fx - c) + (fy - c) * (fy - c)).sqrt();
            let (mb, mg, mr, a_mark) = if recording {
                (0x23u8, 0x11u8, 0xE8u8, (9.5 - d_c + 0.5).clamp(0.0, 1.0)) // 红色实心圆
            } else {
                (255u8, 255u8, 255u8, (1.6 - (d_c - 7.6).abs()).clamp(0.0, 1.0)) // 白色圆环
            };
            let a = a_rect.max(a_mark);
            if a > 0.0 {
                let i = (y * S + x) as usize * 4;
                let (b, g, r) = if a_mark > a_rect { (mb, mg, mr) } else { (0x4F, 0x31, 0x22) };
                px[i] = b;
                px[i + 1] = g;
                px[i + 2] = r;
                px[i + 3] = (a * 255.0) as u8;
            }
        }
    }
    unsafe {
        let bmi = BITMAPINFO {
            bmiHeader: BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: S,
                biHeight: -S, // top-down
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut bits: *mut c_void = std::ptr::null_mut();
        let color = CreateDIBSection(None, &bmi, DIB_RGB_COLORS, &mut bits, None, 0)
            .unwrap_or(HBITMAP::default());
        if !color.is_invalid() && !bits.is_null() {
            std::ptr::copy_nonoverlapping(px.as_ptr(), bits as *mut u8, px.len());
        }
        let mask_bits = [0u8; (S * S / 8) as usize]; // alpha 图标的 AND 掩码须全 0
        let mask = CreateBitmap(S, S, 1, 1, Some(mask_bits.as_ptr() as *const c_void));
        let ii = ICONINFO {
            fIcon: windows::core::BOOL(1),
            xHotspot: 0,
            yHotspot: 0,
            hbmMask: mask,
            hbmColor: color,
        };
        let icon = CreateIconIndirect(&ii).unwrap_or_default();
        let _ = DeleteObject(HGDIOBJ(color.0));
        let _ = DeleteObject(HGDIOBJ(mask.0));
        icon
    }
}

/// 弹出托盘上下文菜单（TPM_RETURNCMD 取回所选命令，经 WM_COMMAND 发回主窗口）
pub unsafe fn show_context_menu(hwnd: HWND, recording: bool) {
    unsafe {
        let Ok(menu) = CreatePopupMenu() else { return };
        let show = to_wide("显示主窗口");
        let rec_txt = if recording { "■ 停止并保存 (F9)" } else { "● 开始录制 (F9)" };
        let rec = to_wide(rec_txt);
        let cancel = to_wide("取消录制 (F10)");
        let exit = to_wide("退出");
        let _ = AppendMenuW(menu, MF_STRING, IDM_SHOW as usize, PCWSTR(show.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, IDM_TOGGLE_REC as usize, PCWSTR(rec.as_ptr()));
        let cancel_flags = if recording { MF_STRING } else { MF_STRING | MF_GRAYED };
        let _ = AppendMenuW(menu, cancel_flags, IDM_CANCEL_REC as usize, PCWSTR(cancel.as_ptr()));
        let _ = AppendMenuW(menu, MF_SEPARATOR, 0, PCWSTR::null());
        let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT as usize, PCWSTR(exit.as_ptr()));

        let mut pt = POINT::default();
        let _ = GetCursorPos(&mut pt);
        let _ = SetForegroundWindow(hwnd); // 否则菜单无法失焦关闭
        let cmd = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_RETURNCMD | TPM_BOTTOMALIGN,
            pt.x,
            pt.y,
            None,
            hwnd,
            None,
        );
        let _ = PostMessageW(
            Some(hwnd),
            WM_COMMAND,
            WPARAM(cmd.0 as usize & 0xFFFF),
            LPARAM(0),
        );
        let _ = PostMessageW(Some(hwnd), WM_NULL, WPARAM(0), LPARAM(0)); // 经典托盘菜单补丁
        let _ = DestroyMenu(menu);
    }
}
