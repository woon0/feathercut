//! Keep native non-client painting out of the custom titlebar, including activation.
use std::ffi::c_void;

const SUBCLASS_ID: usize = 0x46454154;
const WM_NCACTIVATE: u32 = 0x0086;
const WM_NCPAINT: u32 = 0x0085;
const WM_NCCALCSIZE: u32 = 0x0083;
const WM_STYLECHANGING: u32 = 0x007c;
const WM_NCDESTROY: u32 = 0x0082;
const WS_CAPTION: u32 = 0x00c00000;
type Subclass = unsafe extern "system" fn(*mut c_void, u32, usize, isize, usize, usize) -> isize;
#[link(name = "comctl32")]
unsafe extern "system" {
    fn SetWindowSubclass(hwnd: *mut c_void, callback: Subclass, id: usize, data: usize) -> i32;
    fn RemoveWindowSubclass(hwnd: *mut c_void, callback: Subclass, id: usize) -> i32;
    fn DefSubclassProc(hwnd: *mut c_void, message: u32, wp: usize, lp: isize) -> isize;
}
#[repr(C)]
struct StyleChange {
    old: u32,
    new: u32,
}

pub fn install(hwnd: *mut c_void) {
    // The same callback/id pair updates an existing subclass rather than stacking it.
    // SAFETY: installation and all callbacks run on the owning UI thread.
    unsafe {
        SetWindowSubclass(hwnd, procedure, SUBCLASS_ID, 0);
    }
}
unsafe extern "system" fn procedure(
    hwnd: *mut c_void,
    message: u32,
    wp: usize,
    lp: isize,
    _: usize,
    _: usize,
) -> isize {
    // SAFETY: parameters originate from Windows on the owning UI thread.
    unsafe {
        match message {
            WM_STYLECHANGING if wp as isize == -16 && lp != 0 => {
                // Winit can restore its original WS_CAPTION when changing window state.
                let style = &mut *(lp as *mut StyleChange);
                style.new &= !WS_CAPTION;
            }
            WM_NCPAINT => return 0,
            // Winit forwards the RECT-only variant to DefWindowProc, which would
            // reserve a resize frame. Leave maximized NCCALCSIZE_PARAMS to winit.
            WM_NCCALCSIZE if wp == 0 => return 0,
            WM_NCACTIVATE => {
                // -1 forwards activation to winit but tells DefWindowProc not to paint.
                let result = DefSubclassProc(hwnd, message, wp, -1);
                super::composition::sync();
                return result;
            }
            WM_NCDESTROY => {
                super::composition::shutdown();
                RemoveWindowSubclass(hwnd, procedure, SUBCLASS_ID);
            }
            _ => {}
        }
        let result = DefSubclassProc(hwnd, message, wp, lp);
        // Position changes also cover interactive moves, minimization and Z-order.
        if message == 0x0047 || message == 0x0018 {
            super::composition::sync();
        }
        result
    }
}
