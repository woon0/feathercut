//! A non-interactive compositor host below Feathercut, never over its rendered pixels.
use std::ffi::c_void;
use windows::{
    Win32::Foundation::HWND,
    core::{Error, Result},
};

#[repr(C)]
#[derive(Default)]
struct Rect {
    left: i32,
    top: i32,
    right: i32,
    bottom: i32,
}
#[repr(C)]
#[derive(Default)]
struct Point {
    x: i32,
    y: i32,
}
#[link(name = "user32")]
unsafe extern "system" {
    fn CreateWindowExW(
        ex: u32,
        class: *const u16,
        title: *const u16,
        style: u32,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        parent: *mut c_void,
        menu: *mut c_void,
        instance: *mut c_void,
        param: *mut c_void,
    ) -> *mut c_void;
    fn DestroyWindow(hwnd: *mut c_void) -> i32;
    fn GetClientRect(hwnd: *mut c_void, rect: *mut Rect) -> i32;
    fn ClientToScreen(hwnd: *mut c_void, point: *mut Point) -> i32;
    fn IsIconic(hwnd: *mut c_void) -> i32;
    fn IsWindowVisible(hwnd: *mut c_void) -> i32;
    fn SetWindowPos(
        hwnd: *mut c_void,
        after: *mut c_void,
        x: i32,
        y: i32,
        width: i32,
        height: i32,
        flags: u32,
    ) -> i32;
    fn ShowWindow(hwnd: *mut c_void, command: i32) -> i32;
}
#[link(name = "dwmapi")]
unsafe extern "system" {
    fn DwmSetWindowAttribute(hwnd: *mut c_void, attribute: u32, data: *const i32, size: u32)
    -> i32;
}
pub struct Underlay {
    pub hwnd: HWND,
    main: HWND,
    enabled: bool,
}
impl Underlay {
    pub fn new(main: HWND) -> Result<Self> {
        // STATIC is a system-registered class. This window is not owned by Feathercut:
        // owned popups would be forced ABOVE their owner, hiding the editor again.
        let class: Vec<u16> = "STATIC\0".encode_utf16().collect();
        // No activation, no taskbar/Alt-Tab entry, no redirection bitmap, no input.
        // WS_EX_NOACTIVATE | WS_EX_NOREDIRECTIONBITMAP | WS_EX_TOOLWINDOW |
        // WS_EX_TRANSPARENT, WS_POPUP | WS_DISABLED.
        let hwnd = unsafe {
            CreateWindowExW(
                0x082000a0,
                class.as_ptr(),
                class.as_ptr(),
                0x88000000,
                0,
                0,
                1,
                1,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if hwnd.is_null() {
            return Err(Error::from_win32());
        }
        let underlay = Self {
            hwnd: HWND(hwnd),
            main,
            enabled: false,
        };
        unsafe {
            // Do not enable the pre-blurred host material. The raw backdrop brush
            // samples through this window's WS_EX_NOREDIRECTIONBITMAP surface.
            DwmSetWindowAttribute(hwnd, 17, &0, 4);
            DwmSetWindowAttribute(hwnd, 34, &-2, 4); // DWMWA_COLOR_NONE
            DwmSetWindowAttribute(hwnd, 33, &2, 4);
        }
        Ok(underlay)
    }
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
        self.sync();
    }
    pub fn sync(&self) {
        // Never activate either window or lift Feathercut above the user's other apps.
        unsafe {
            if !self.enabled || IsIconic(self.main.0) != 0 || IsWindowVisible(self.main.0) == 0 {
                ShowWindow(self.hwnd.0, 0);
                return;
            }
            let mut rect = Rect::default();
            let mut origin = Point::default();
            if GetClientRect(self.main.0, &mut rect) == 0
                || ClientToScreen(self.main.0, &mut origin) == 0
            {
                ShowWindow(self.hwnd.0, 0);
                return;
            }
            // Insert directly below the main HWND, not HWND_TOP/TOPMOST.
            SetWindowPos(
                self.hwnd.0,
                self.main.0,
                origin.x,
                origin.y,
                rect.right - rect.left,
                rect.bottom - rect.top,
                0x0050,
            ); // NOACTIVATE | SHOWWINDOW
        }
    }
}
impl Drop for Underlay {
    fn drop(&mut self) {
        unsafe {
            DestroyWindow(self.hwnd.0);
        }
    }
}
