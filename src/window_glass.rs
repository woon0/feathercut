//! Desktop composition and caption suppression for the frameless glass window.
use slint::winit_030::winit;
#[cfg(windows)]
mod composition;
#[cfg(windows)]
mod frame;
#[cfg(windows)]
mod underlay;

#[cfg(windows)]
fn handle(window: &winit::window::Window) -> Option<*mut std::ffi::c_void> {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    match window.window_handle().ok()?.as_raw() {
        RawWindowHandle::Win32(handle) => Some(handle.hwnd.get() as *mut std::ffi::c_void),
        _ => None,
    }
}
#[cfg(windows)]
pub fn clear_caption(window: &winit::window::Window) {
    use std::ffi::c_void;
    #[link(name = "user32")]
    unsafe extern "system" {
        fn GetWindowLongW(hwnd: *mut c_void, index: i32) -> i32;
        fn SetWindowLongW(hwnd: *mut c_void, index: i32, value: i32) -> i32;
        fn SetWindowPos(
            hwnd: *mut c_void,
            after: *mut c_void,
            x: i32,
            y: i32,
            width: i32,
            height: i32,
            flags: u32,
        ) -> i32;
    }
    let Some(hwnd) = handle(window) else { return };
    frame::install(hwnd);
    // Winit retains WS_CAPTION for snapping even with decorations disabled.
    // Remove its native caption painting while preserving resize/min/max styles.
    // SAFETY: this is the live event-loop-owned HWND. Skip frame updates unless needed.
    unsafe {
        let style = GetWindowLongW(hwnd, -16);
        let without_caption = style & !0x00c00000;
        if style != without_caption {
            SetWindowLongW(hwnd, -16, without_caption);
            SetWindowPos(hwnd, std::ptr::null_mut(), 0, 0, 0, 0, 0x0037);
        }
    }
}
#[cfg(windows)]
pub fn apply(window: &winit::window::Window, transparency: f32, blur: f32) -> bool {
    use std::ffi::c_void;
    #[repr(C)]
    struct Margins {
        left: i32,
        right: i32,
        top: i32,
        bottom: i32,
    }
    #[link(name = "dwmapi")]
    unsafe extern "system" {
        fn DwmSetWindowAttribute(hwnd: *mut c_void, attr: u32, value: *const i32, size: u32)
        -> i32;
        fn DwmExtendFrameIntoClientArea(hwnd: *mut c_void, margins: *const Margins) -> i32;
    }
    let Some(hwnd) = handle(window) else {
        return false;
    };
    clear_caption(window);
    // No opaque system Acrylic and no extended native titlebar. The compositor
    // supplies a separately blended, adjustable desktop blur below Slint's pixels.
    // SAFETY: live HWND, correct attribute scalar sizes and layout.
    unsafe {
        DwmSetWindowAttribute(hwnd, 20, &1, 4);
        DwmSetWindowAttribute(hwnd, 33, &2, 4);
        DwmSetWindowAttribute(hwnd, 2, &2, 4); // DWMNCRP_DISABLED
        DwmSetWindowAttribute(hwnd, 34, &-2, 4); // DWMWA_COLOR_NONE
        DwmSetWindowAttribute(hwnd, 38, &1, 4);
        let margins = Margins {
            left: 0,
            right: 0,
            top: 0,
            bottom: 0,
        };
        DwmExtendFrameIntoClientArea(hwnd, &margins);
        DwmSetWindowAttribute(hwnd, 17, &0, 4);
    }
    let size = window.inner_size();
    composition::apply(
        windows::Win32::Foundation::HWND(hwnd),
        transparency,
        blur,
        size.width,
        size.height,
    )
}
#[cfg(windows)]
pub fn resize(window: &winit::window::Window) {
    let size = window.inner_size();
    composition::resize(size.width, size.height);
}
#[cfg(windows)]
pub fn shutdown() {
    composition::shutdown();
}

#[cfg(not(windows))]
pub fn clear_caption(_: &winit::window::Window) {}
#[cfg(not(windows))]
pub fn resize(_: &winit::window::Window) {}
#[cfg(not(windows))]
pub fn apply(_: &winit::window::Window, _: f32, _: f32) -> bool {
    false
}
#[cfg(not(windows))]
pub fn shutdown() {}
