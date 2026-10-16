//! GDI 屏幕捕获：StretchBlt 抓取虚拟屏区域（支持缩放），GetDIBits 导出 RGB。
//! 长驻 GDI 资源，逐帧复用，避免句柄抖动。

use std::ffi::c_void;

use windows::Win32::Graphics::Gdi::{
    BI_RGB, CAPTUREBLT, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject,
    DIB_RGB_COLORS, GetDC, GetDIBits, HALFTONE, ReleaseDC, SRCCOPY, SelectObject,
    SetStretchBltMode, StretchBlt, BITMAPINFO, BITMAPINFOHEADER, HBITMAP, HDC, HGDIOBJ,
};
use windows::Win32::UI::WindowsAndMessaging::{
    GetSystemMetrics, SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN,
    SM_YVIRTUALSCREEN,
};

pub struct ScreenGrabber {
    hdc_screen: HDC,
    hdc_mem: HDC,
    hbm: HBITMAP,
    old_bm: HGDIOBJ,
    bmi: BITMAPINFO,
    bgra: Vec<u8>,
    pub width: usize,
    pub height: usize,
    src_x: i32,
    src_y: i32,
    src_w: i32,
    src_h: i32,
}

impl ScreenGrabber {
    /// `region` = (x, y, w, h) 虚拟屏绝对坐标；None 录制整个虚拟屏。
    /// `scale_percent` 输出尺寸百分比（10..=100）。
    pub fn new(region: Option<(i32, i32, i32, i32)>, scale_percent: u32) -> Result<Self, String> {
        let (sx, sy, sw, sh) = region.unwrap_or_else(|| unsafe {
            (
                GetSystemMetrics(SM_XVIRTUALSCREEN),
                GetSystemMetrics(SM_YVIRTUALSCREEN),
                GetSystemMetrics(SM_CXVIRTUALSCREEN),
                GetSystemMetrics(SM_CYVIRTUALSCREEN),
            )
        });
        let pct = scale_percent.clamp(10, 100) as i64;
        let tw = (sw as i64 * pct / 100).max(1) as i32;
        let th = (sh as i64 * pct / 100).max(1) as i32;

        unsafe {
            let hdc_screen = GetDC(None);
            if hdc_screen.is_invalid() {
                return Err("GetDC 失败".into());
            }
            let hdc_mem = CreateCompatibleDC(Some(hdc_screen));
            if hdc_mem.is_invalid() {
                ReleaseDC(None, hdc_screen);
                return Err("CreateCompatibleDC 失败".into());
            }
            let hbm = CreateCompatibleBitmap(hdc_screen, tw, th);
            if hbm.is_invalid() {
                let _ = DeleteDC(hdc_mem);
                ReleaseDC(None, hdc_screen);
                return Err("CreateCompatibleBitmap 失败".into());
            }
            let old_bm = SelectObject(hdc_mem, HGDIOBJ(hbm.0));
            // HALFTONE 缩放质量较好；对 100% 缩放无副作用
            let _ = SetStretchBltMode(hdc_mem, HALFTONE);
            Ok(Self {
                hdc_screen,
                hdc_mem,
                hbm,
                old_bm,
                bmi: BITMAPINFO::default(),
                bgra: vec![0u8; (tw as usize) * (th as usize) * 4],
                width: tw as usize,
                height: th as usize,
                src_x: sx,
                src_y: sy,
                src_w: sw,
                src_h: sh,
            })
        }
    }

    /// 抓一帧。返回紧凑 RGB（top-down，已就地由 BGRA 转换）。
    pub fn grab(&mut self) -> Result<&[u8], String> {
        let (tw, th) = (self.width as i32, self.height as i32);
        unsafe {
            let ok = StretchBlt(
                self.hdc_mem,
                0,
                0,
                tw,
                th,
                Some(self.hdc_screen),
                self.src_x,
                self.src_y,
                self.src_w,
                self.src_h,
                SRCCOPY | CAPTUREBLT,
            );
            if !ok.as_bool() {
                return Err("StretchBlt 失败".into());
            }
            // top-down 32bpp DIB（负高度），行序与屏幕一致
            self.bmi.bmiHeader = BITMAPINFOHEADER {
                biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                biWidth: tw,
                biHeight: -th,
                biPlanes: 1,
                biBitCount: 32,
                biCompression: BI_RGB.0,
                ..Default::default()
            };
            let got = GetDIBits(
                self.hdc_mem,
                self.hbm,
                0,
                th as u32,
                Some(self.bgra.as_mut_ptr() as *mut c_void),
                &mut self.bmi,
                DIB_RGB_COLORS,
            );
            if got == 0 {
                return Err("GetDIBits 失败".into());
            }
        }
        // BGRA → RGB 原地压缩
        let px = self.width * self.height;
        let mut s = 0usize;
        let mut d = 0usize;
        let buf = &mut self.bgra;
        for _ in 0..px {
            buf[d] = buf[s + 2]; // R
            buf[d + 1] = buf[s + 1]; // G
            buf[d + 2] = buf[s]; // B
            s += 4;
            d += 3;
        }
        Ok(&buf[..px * 3])
    }
}

impl Drop for ScreenGrabber {
    fn drop(&mut self) {
        unsafe {
            let _ = SelectObject(self.hdc_mem, self.old_bm);
            let _ = DeleteObject(HGDIOBJ(self.hbm.0));
            let _ = DeleteDC(self.hdc_mem);
            ReleaseDC(None, self.hdc_screen);
        }
    }
}
