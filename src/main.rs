#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::cell::RefCell;
use std::ffi::c_void;
use std::fs::{self, File};
use std::io::BufWriter;
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Ole::CF_DIB;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::SystemServices::MK_LBUTTON;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const APP_NAME: &str = "Copyshot";
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_CAPTURE_REGION: u32 = WM_APP + 2;
const WM_CAPTURE_FULLSCREEN: u32 = WM_APP + 3;
const TRAY_UID: u32 = 1;
const MENU_REGION: usize = 1001;
const MENU_FULLSCREEN: usize = 1002;
const MENU_OPEN_FOLDER: usize = 1003;
const MENU_STARTUP: usize = 1004;
const MENU_EXIT: usize = 1005;
const STARTUP_VALUE: &str = "Copyshot";
const OVERLAY_FADE_TIMER: usize = 1;
const OVERLAY_FADE_INTERVAL_MS: u32 = 12;
const OVERLAY_DIM_ALPHA: u8 = 132;
const OVERLAY_FADE_STEP: u8 = 18;

thread_local! {
    static APP: RefCell<Option<AppState>> = const { RefCell::new(None) };
    static OVERLAY: RefCell<Option<OverlayState>> = const { RefCell::new(None) };
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: i32,
    y: i32,
    width: i32,
    height: i32,
}

impl Rect {
    fn from_points(a: POINT, b: POINT) -> Option<Self> {
        let x1 = a.x.min(b.x);
        let y1 = a.y.min(b.y);
        let x2 = a.x.max(b.x);
        let y2 = a.y.max(b.y);
        let width = x2 - x1;
        let height = y2 - y1;
        if width < 2 || height < 2 {
            None
        } else {
            Some(Self {
                x: x1,
                y: y1,
                width,
                height,
            })
        }
    }
}

struct AppState {
    hwnd: HWND,
    screenshot_dir: PathBuf,
}

struct OverlayState {
    hwnd: HWND,
    frozen: Capture,
    base: Vec<u8>,
    frame: Vec<u8>,
    dim_alpha: u8,
    fade: FadeDirection,
    start: Option<POINT>,
    current: POINT,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum FadeDirection {
    In,
    Out,
    Done,
}

struct Capture {
    width: u32,
    height: u32,
    bgra: Vec<u8>,
}

fn main() {
    if let Err(err) = run() {
        unsafe {
            MessageBoxW(
                null_mut(),
                wide(&err).as_ptr(),
                wide(APP_NAME).as_ptr(),
                MB_ICONERROR | MB_OK,
            );
        }
    }
}

fn run() -> Result<(), String> {
    unsafe {
        let hinstance = GetModuleHandleW(null());
        if hinstance.is_null() {
            return Err("Could not get module handle.".to_string());
        }

        register_window_classes(hinstance)?;

        let hwnd = CreateWindowExW(
            0,
            wide("CopyshotMainWindow").as_ptr(),
            wide(APP_NAME).as_ptr(),
            0,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinstance,
            null_mut(),
        );
        if hwnd.is_null() {
            return Err("Could not create message window.".to_string());
        }

        let screenshot_dir = screenshot_dir();
        fs::create_dir_all(&screenshot_dir)
            .map_err(|err| format!("Could not create screenshot folder: {err}"))?;

        APP.with(|app| {
            *app.borrow_mut() = Some(AppState {
                hwnd,
                screenshot_dir,
            });
        });

        add_tray_icon(hwnd);
        let keyboard_hook =
            SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_hook_proc), hinstance, 0);
        if keyboard_hook.is_null() {
            notify(
                "Keyboard hook failed",
                "Print Screen could not be captured. Use the tray menu or restart Copyshot.",
            );
        }

        ensure_startup_enabled();

        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }

        remove_tray_icon(hwnd);
        if !keyboard_hook.is_null() {
            UnhookWindowsHookEx(keyboard_hook);
        }
    }
    Ok(())
}

unsafe fn register_window_classes(hinstance: HINSTANCE) -> Result<(), String> {
    let main_class_name = wide("CopyshotMainWindow");
    let main_class = WNDCLASSW {
        lpfnWndProc: Some(main_wnd_proc),
        hInstance: hinstance,
        lpszClassName: main_class_name.as_ptr(),
        ..zeroed()
    };
    if RegisterClassW(&main_class) == 0 {
        return Err("Could not register main window class.".to_string());
    }

    let overlay_class_name = wide("CopyshotOverlayWindow");
    let overlay_class = WNDCLASSW {
        lpfnWndProc: Some(overlay_wnd_proc),
        hInstance: hinstance,
        hCursor: LoadCursorW(null_mut(), IDC_CROSS),
        hbrBackground: CreateSolidBrush(rgb(0, 0, 0)),
        lpszClassName: overlay_class_name.as_ptr(),
        ..zeroed()
    };
    if RegisterClassW(&overlay_class) == 0 {
        return Err("Could not register overlay window class.".to_string());
    }
    Ok(())
}

unsafe extern "system" fn main_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_COMMAND => {
            match wparam & 0xffff {
                MENU_REGION => start_region_capture(),
                MENU_FULLSCREEN => capture_fullscreen(),
                MENU_OPEN_FOLDER => open_screenshot_folder(),
                MENU_STARTUP => toggle_startup(),
                MENU_EXIT => {
                    DestroyWindow(hwnd);
                }
                _ => {}
            }
            0
        }
        WM_CAPTURE_REGION => {
            start_region_capture();
            0
        }
        WM_CAPTURE_FULLSCREEN => {
            capture_fullscreen();
            0
        }
        WM_TRAYICON => {
            let event = loword(lparam as usize) as u32;
            if event == WM_RBUTTONUP || event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            } else if event == WM_LBUTTONDBLCLK {
                start_region_capture();
            }
            0
        }
        WM_DESTROY => {
            remove_tray_icon(hwnd);
            PostQuitMessage(0);
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe extern "system" fn keyboard_hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code >= 0 {
        let event = wparam as u32;
        if event == WM_KEYDOWN || event == WM_SYSKEYDOWN {
            let key = &*(lparam as *const KBDLLHOOKSTRUCT);
            if key.vkCode == VK_SNAPSHOT as u32 {
                let message = if shift_pressed() {
                    WM_CAPTURE_FULLSCREEN
                } else {
                    WM_CAPTURE_REGION
                };
                APP.with(|app| {
                    if let Some(app) = app.borrow().as_ref() {
                        PostMessageW(app.hwnd, message, 0, 0);
                    }
                });
                return 1;
            }
        }
    }

    CallNextHookEx(null_mut(), code, wparam, lparam)
}

unsafe extern "system" fn overlay_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_LBUTTONDOWN => {
            OVERLAY.with(|overlay_cell| {
                let mut overlay_ref = overlay_cell.borrow_mut();
                if let Some(overlay) = overlay_ref.as_mut() {
                    if overlay.fade == FadeDirection::Out {
                        return;
                    }
                    let point = lparam_point(lparam);
                    overlay.start = Some(point);
                    overlay.current = point;
                    SetCapture(hwnd);
                    update_overlay_selection(overlay, None, None);
                    InvalidateRect(hwnd, null(), 0);
                }
            });
            0
        }
        WM_MOUSEMOVE => {
            if wparam & MK_LBUTTON as usize != 0 {
                OVERLAY.with(|overlay_cell| {
                    let mut overlay_ref = overlay_cell.borrow_mut();
                    if let Some(overlay) = overlay_ref.as_mut() {
                        if overlay.fade == FadeDirection::Out {
                            return;
                        }
                        let old = current_selection(overlay);
                        overlay.current = lparam_point(lparam);
                        let new = current_selection(overlay);
                        let dirty = update_overlay_selection(overlay, old, new);
                        invalidate_rect(hwnd, dirty);
                    }
                });
            }
            0
        }
        WM_LBUTTONUP => {
            ReleaseCapture();
            let capture = OVERLAY.with(|overlay_cell| {
                overlay_cell.borrow().as_ref().and_then(|overlay| {
                    overlay.start.and_then(|start| {
                        Rect::from_points(start, lparam_point(lparam))
                            .and_then(|rect| crop_capture(&overlay.frozen, rect))
                    })
                })
            });
            if let Some(capture) = capture {
                if let Err(err) = finish_capture(capture) {
                    notify("Screenshot failed", &err);
                }
            }
            start_overlay_fade_out(hwnd);
            0
        }
        WM_KEYDOWN => {
            if wparam as u16 == VK_ESCAPE {
                start_overlay_fade_out(hwnd);
            }
            0
        }
        WM_PAINT => {
            paint_overlay(hwnd);
            0
        }
        WM_TIMER => {
            if wparam == OVERLAY_FADE_TIMER {
                let should_destroy = OVERLAY.with(|overlay_cell| {
                    let mut overlay_ref = overlay_cell.borrow_mut();
                    let Some(overlay) = overlay_ref.as_mut() else {
                        return true;
                    };
                    match overlay.fade {
                        FadeDirection::In => {
                            overlay.dim_alpha = overlay
                                .dim_alpha
                                .saturating_add(OVERLAY_FADE_STEP)
                                .min(OVERLAY_DIM_ALPHA);
                            if overlay.dim_alpha == OVERLAY_DIM_ALPHA {
                                overlay.fade = FadeDirection::Done;
                                KillTimer(hwnd, OVERLAY_FADE_TIMER);
                            }
                            compose_full_overlay_frame(overlay);
                            InvalidateRect(hwnd, null(), 0);
                            false
                        }
                        FadeDirection::Out => {
                            overlay.dim_alpha = overlay.dim_alpha.saturating_sub(OVERLAY_FADE_STEP);
                            compose_full_overlay_frame(overlay);
                            InvalidateRect(hwnd, null(), 0);
                            overlay.dim_alpha == 0
                        }
                        FadeDirection::Done => false,
                    }
                });
                if should_destroy {
                    KillTimer(hwnd, OVERLAY_FADE_TIMER);
                    DestroyWindow(hwnd);
                    OVERLAY.with(|overlay| *overlay.borrow_mut() = None);
                }
            }
            0
        }
        WM_ERASEBKGND => 1,
        WM_DESTROY => {
            KillTimer(hwnd, OVERLAY_FADE_TIMER);
            let should_clear = OVERLAY.with(|overlay| {
                overlay.borrow().as_ref().map(|overlay| overlay.hwnd) == Some(hwnd)
            });
            if should_clear {
                OVERLAY.with(|overlay| *overlay.borrow_mut() = None);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

unsafe fn start_region_capture() {
    if OVERLAY.with(|overlay| overlay.borrow().is_some()) {
        return;
    }

    let hinstance = GetModuleHandleW(null());
    let x = GetSystemMetrics(SM_XVIRTUALSCREEN);
    let y = GetSystemMetrics(SM_YVIRTUALSCREEN);
    let width = GetSystemMetrics(SM_CXVIRTUALSCREEN);
    let height = GetSystemMetrics(SM_CYVIRTUALSCREEN);
    let frozen = match capture_screen(Rect {
        x,
        y,
        width,
        height,
    }) {
        Ok(capture) => capture,
        Err(err) => {
            notify("Region capture failed", &err);
            return;
        }
    };
    let base = frozen.bgra.clone();
    let frame = frozen.bgra.clone();

    let hwnd = CreateWindowExW(
        WS_EX_TOPMOST | WS_EX_TOOLWINDOW,
        wide("CopyshotOverlayWindow").as_ptr(),
        wide("Copyshot region selection").as_ptr(),
        WS_POPUP | WS_VISIBLE,
        x,
        y,
        width,
        height,
        null_mut(),
        null_mut(),
        hinstance,
        null_mut(),
    );
    if hwnd.is_null() {
        notify(
            "Region capture failed",
            "Could not create the selection overlay.",
        );
        return;
    }

    OVERLAY.with(|overlay| {
        *overlay.borrow_mut() = Some(OverlayState {
            hwnd,
            frozen,
            base,
            frame,
            dim_alpha: 0,
            fade: FadeDirection::In,
            start: None,
            current: POINT { x: 0, y: 0 },
        });
    });
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
    SetFocus(hwnd);
    SetTimer(hwnd, OVERLAY_FADE_TIMER, OVERLAY_FADE_INTERVAL_MS, None);
}

unsafe fn paint_overlay(hwnd: HWND) {
    let mut ps: PAINTSTRUCT = zeroed();
    let hdc = BeginPaint(hwnd, &mut ps);
    if hdc.is_null() {
        return;
    }

    OVERLAY.with(|overlay_cell| {
        let overlay_ref = overlay_cell.borrow();
        if let Some(overlay) = overlay_ref.as_ref() {
            paint_bgra(
                hdc,
                overlay.frozen.width,
                overlay.frozen.height,
                &overlay.frame,
            );
        }
    });

    EndPaint(hwnd, &ps);
}

unsafe fn capture_fullscreen() {
    let rect = Rect {
        x: GetSystemMetrics(SM_XVIRTUALSCREEN),
        y: GetSystemMetrics(SM_YVIRTUALSCREEN),
        width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
        height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
    };
    capture_rect(rect);
}

unsafe fn capture_rect(rect: Rect) {
    match capture_screen(rect).and_then(finish_capture) {
        Ok(_) => {}
        Err(err) => notify("Screenshot failed", &err),
    }
}

fn finish_capture(capture: Capture) -> Result<PathBuf, String> {
    let path = save_capture(&capture)?;
    unsafe {
        copy_capture_to_clipboard(&capture)?;
    }
    Ok(path)
}

unsafe fn start_overlay_fade_out(hwnd: HWND) {
    OVERLAY.with(|overlay_cell| {
        let mut overlay_ref = overlay_cell.borrow_mut();
        if let Some(overlay) = overlay_ref.as_mut() {
            overlay.fade = FadeDirection::Out;
            overlay.start = None;
            compose_full_overlay_frame(overlay);
        }
    });
    InvalidateRect(hwnd, null(), 0);
    SetTimer(hwnd, OVERLAY_FADE_TIMER, OVERLAY_FADE_INTERVAL_MS, None);
}

fn compose_full_overlay_frame(overlay: &mut OverlayState) {
    let alpha = overlay.dim_alpha;
    overlay.base.copy_from_slice(&overlay.frozen.bgra);
    dim_bgra_in_place(&mut overlay.base, alpha);
    overlay.frame.copy_from_slice(&overlay.base);

    if let Some(rect) = overlay
        .start
        .and_then(|start| Rect::from_points(start, overlay.current))
    {
        draw_selection_into_frame(overlay, rect);
    }
}

fn update_overlay_selection(
    overlay: &mut OverlayState,
    old: Option<Rect>,
    new: Option<Rect>,
) -> Option<Rect> {
    let dirty = old
        .zip(new)
        .map(|(old, new)| union_rect(expand_rect(old, 3), expand_rect(new, 3)));

    if let Some(rect) = dirty {
        restore_frame_region(overlay, rect);
    } else {
        overlay.frame.copy_from_slice(&overlay.base);
    }

    if let Some(rect) = new {
        draw_selection_into_frame(overlay, rect);
    }

    dirty
}

fn current_selection(overlay: &OverlayState) -> Option<Rect> {
    overlay
        .start
        .and_then(|start| Rect::from_points(start, overlay.current))
}

fn draw_selection_into_frame(overlay: &mut OverlayState, rect: Rect) {
    reveal_capture_region(&overlay.frozen, &mut overlay.frame, rect);
    draw_selection_outline_bgra(
        &mut overlay.frame,
        overlay.frozen.width,
        overlay.frozen.height,
        rect,
    );
}

fn restore_frame_region(overlay: &mut OverlayState, rect: Rect) {
    let Some(rect) = clamp_rect_to_capture(&overlay.frozen, rect) else {
        return;
    };
    let stride = overlay.frozen.width as usize * 4;
    let x = rect.x as usize * 4;
    let len = rect.width as usize * 4;

    for row in 0..rect.height as usize {
        let offset = (rect.y as usize + row) * stride + x;
        overlay.frame[offset..offset + len].copy_from_slice(&overlay.base[offset..offset + len]);
    }
}

unsafe fn invalidate_rect(hwnd: HWND, rect: Option<Rect>) {
    if let Some(rect) = rect {
        let invalid = RECT {
            left: rect.x,
            top: rect.y,
            right: rect.x + rect.width,
            bottom: rect.y + rect.height,
        };
        InvalidateRect(hwnd, &invalid, 0);
    } else {
        InvalidateRect(hwnd, null(), 0);
    }
}

fn expand_rect(rect: Rect, amount: i32) -> Rect {
    Rect {
        x: rect.x - amount,
        y: rect.y - amount,
        width: rect.width + amount * 2,
        height: rect.height + amount * 2,
    }
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let left = a.x.min(b.x);
    let top = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    }
}

fn dim_bgra_in_place(bgra: &mut [u8], alpha: u8) {
    if alpha == 0 {
        return;
    }
    let keep = 255u16 - alpha as u16;
    for pixel in bgra.chunks_exact_mut(4) {
        pixel[0] = ((pixel[0] as u16 * keep) / 255) as u8;
        pixel[1] = ((pixel[1] as u16 * keep) / 255) as u8;
        pixel[2] = ((pixel[2] as u16 * keep) / 255) as u8;
    }
}

fn reveal_capture_region(capture: &Capture, frame: &mut [u8], rect: Rect) {
    let Some(rect) = clamp_rect_to_capture(capture, rect) else {
        return;
    };
    let stride = capture.width as usize * 4;
    let x = rect.x as usize * 4;
    let len = rect.width as usize * 4;

    for row in 0..rect.height as usize {
        let offset = (rect.y as usize + row) * stride + x;
        frame[offset..offset + len].copy_from_slice(&capture.bgra[offset..offset + len]);
    }
}

fn draw_selection_outline_bgra(frame: &mut [u8], width: u32, height: u32, rect: Rect) {
    let Some(rect) = clamp_rect_to_size(width, height, rect) else {
        return;
    };
    for line in 0..2 {
        draw_horizontal_bgra(frame, width, height, rect.x, rect.y + line, rect.width);
        draw_horizontal_bgra(
            frame,
            width,
            height,
            rect.x,
            rect.y + rect.height - 1 - line,
            rect.width,
        );
        draw_vertical_bgra(frame, width, height, rect.x + line, rect.y, rect.height);
        draw_vertical_bgra(
            frame,
            width,
            height,
            rect.x + rect.width - 1 - line,
            rect.y,
            rect.height,
        );
    }
}

fn draw_horizontal_bgra(frame: &mut [u8], width: u32, height: u32, x: i32, y: i32, len: i32) {
    if y < 0 || y >= height as i32 {
        return;
    }
    let left = x.clamp(0, width as i32);
    let right = (x + len).clamp(0, width as i32);
    for px in left..right {
        put_white_pixel(frame, width, px, y);
    }
}

fn draw_vertical_bgra(frame: &mut [u8], width: u32, height: u32, x: i32, y: i32, len: i32) {
    if x < 0 || x >= width as i32 {
        return;
    }
    let top = y.clamp(0, height as i32);
    let bottom = (y + len).clamp(0, height as i32);
    for py in top..bottom {
        put_white_pixel(frame, width, x, py);
    }
}

fn put_white_pixel(frame: &mut [u8], width: u32, x: i32, y: i32) {
    let offset = ((y as u32 * width + x as u32) as usize) * 4;
    frame[offset] = 255;
    frame[offset + 1] = 255;
    frame[offset + 2] = 255;
    frame[offset + 3] = 255;
}

unsafe fn paint_bgra(hdc: HDC, width: u32, height: u32, bgra: &[u8]) {
    paint_bgra_at(hdc, 0, 0, width, height, bgra);
}

unsafe fn paint_bgra_at(hdc: HDC, x: i32, y: i32, width: u32, height: u32, bgra: &[u8]) {
    let bmi = bitmap_info(width as i32, -(height as i32));
    StretchDIBits(
        hdc,
        x,
        y,
        width as i32,
        height as i32,
        0,
        0,
        width as i32,
        height as i32,
        bgra.as_ptr() as *const c_void,
        &bmi,
        DIB_RGB_COLORS,
        SRCCOPY,
    );
}

fn crop_capture(capture: &Capture, rect: Rect) -> Option<Capture> {
    let rect = clamp_rect_to_capture(capture, rect)?;
    let mut bgra = vec![0; rect.width as usize * rect.height as usize * 4];
    let source_stride = capture.width as usize * 4;
    let target_stride = rect.width as usize * 4;
    let start_x = rect.x as usize * 4;

    for row in 0..rect.height as usize {
        let source_start = (rect.y as usize + row) * source_stride + start_x;
        let target_start = row * target_stride;
        bgra[target_start..target_start + target_stride]
            .copy_from_slice(&capture.bgra[source_start..source_start + target_stride]);
    }

    Some(Capture {
        width: rect.width as u32,
        height: rect.height as u32,
        bgra,
    })
}

fn clamp_rect_to_capture(capture: &Capture, rect: Rect) -> Option<Rect> {
    clamp_rect_to_size(capture.width, capture.height, rect)
}

fn clamp_rect_to_size(width: u32, height: u32, rect: Rect) -> Option<Rect> {
    let width = width as i32;
    let height = height as i32;
    let left = rect.x.clamp(0, width);
    let top = rect.y.clamp(0, height);
    let right = (rect.x + rect.width).clamp(0, width);
    let bottom = (rect.y + rect.height).clamp(0, height);
    if right <= left || bottom <= top {
        None
    } else {
        Some(Rect {
            x: left,
            y: top,
            width: right - left,
            height: bottom - top,
        })
    }
}

unsafe fn capture_screen(rect: Rect) -> Result<Capture, String> {
    if rect.width <= 0 || rect.height <= 0 {
        return Err("Capture area is empty.".to_string());
    }

    let screen_dc = GetDC(null_mut());
    if screen_dc.is_null() {
        return Err("Could not access the screen device context.".to_string());
    }
    let mem_dc = CreateCompatibleDC(screen_dc);
    if mem_dc.is_null() {
        ReleaseDC(null_mut(), screen_dc);
        return Err("Could not create capture device context.".to_string());
    }

    let bmi = bitmap_info(rect.width, -rect.height);
    let mut bits: *mut c_void = null_mut();
    let bitmap = CreateDIBSection(mem_dc, &bmi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    if bitmap.is_null() || bits.is_null() {
        DeleteDC(mem_dc);
        ReleaseDC(null_mut(), screen_dc);
        return Err("Could not allocate screenshot bitmap.".to_string());
    }

    let old = SelectObject(mem_dc, bitmap as _);
    let copied = BitBlt(
        mem_dc,
        0,
        0,
        rect.width,
        rect.height,
        screen_dc,
        rect.x,
        rect.y,
        SRCCOPY | CAPTUREBLT,
    ) != 0;
    SelectObject(mem_dc, old);

    let byte_len = rect.width as usize * rect.height as usize * 4;
    let bgra = std::slice::from_raw_parts(bits as *const u8, byte_len).to_vec();

    DeleteObject(bitmap as _);
    DeleteDC(mem_dc);
    ReleaseDC(null_mut(), screen_dc);

    if !copied {
        return Err("Could not copy pixels from the screen.".to_string());
    }

    Ok(Capture {
        width: rect.width as u32,
        height: rect.height as u32,
        bgra,
    })
}

fn save_capture(capture: &Capture) -> Result<PathBuf, String> {
    let dir = APP.with(|app| {
        app.borrow()
            .as_ref()
            .map(|app| app.screenshot_dir.clone())
            .unwrap_or_else(screenshot_dir)
    });
    fs::create_dir_all(&dir).map_err(|err| format!("Could not create screenshot folder: {err}"))?;
    let path = dir.join(format!("Screenshot {}.png", timestamp()));
    let file = File::create(&path).map_err(|err| format!("Could not create PNG file: {err}"))?;
    let writer = BufWriter::new(file);
    let mut encoder = png::Encoder::new(writer, capture.width, capture.height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut png_writer = encoder
        .write_header()
        .map_err(|err| format!("Could not start PNG writer: {err}"))?;
    png_writer
        .write_image_data(&bgra_to_rgba(&capture.bgra))
        .map_err(|err| format!("Could not write PNG file: {err}"))?;
    Ok(path)
}

unsafe fn copy_capture_to_clipboard(capture: &Capture) -> Result<(), String> {
    let hwnd = APP.with(|app| {
        app.borrow()
            .as_ref()
            .map(|app| app.hwnd)
            .unwrap_or(null_mut())
    });
    if OpenClipboard(hwnd) == 0 {
        return Err("Could not open the clipboard.".to_string());
    }

    EmptyClipboard();
    let header_size = size_of::<BITMAPINFOHEADER>();
    let pixel_size = capture.bgra.len();
    let total_size = header_size + pixel_size;
    let handle = GlobalAlloc(GMEM_MOVEABLE, total_size);
    if handle.is_null() {
        CloseClipboard();
        return Err("Could not allocate clipboard memory.".to_string());
    }

    let data = GlobalLock(handle) as *mut u8;
    if data.is_null() {
        GlobalFree(handle);
        CloseClipboard();
        return Err("Could not lock clipboard memory.".to_string());
    }

    let header = BITMAPINFOHEADER {
        biSize: header_size as u32,
        biWidth: capture.width as i32,
        biHeight: -(capture.height as i32),
        biPlanes: 1,
        biBitCount: 32,
        biCompression: BI_RGB,
        biSizeImage: pixel_size as u32,
        biXPelsPerMeter: 0,
        biYPelsPerMeter: 0,
        biClrUsed: 0,
        biClrImportant: 0,
    };
    std::ptr::copy_nonoverlapping(
        &header as *const BITMAPINFOHEADER as *const u8,
        data,
        header_size,
    );
    std::ptr::copy_nonoverlapping(capture.bgra.as_ptr(), data.add(header_size), pixel_size);
    GlobalUnlock(handle);

    if SetClipboardData(CF_DIB.into(), handle).is_null() {
        GlobalFree(handle);
        CloseClipboard();
        return Err("Could not write image data to the clipboard.".to_string());
    }

    CloseClipboard();
    Ok(())
}

unsafe fn add_tray_icon(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_MESSAGE | NIF_ICON | NIF_TIP;
    nid.uCallbackMessage = WM_TRAYICON;
    nid.hIcon = load_app_icon();
    copy_wide_to_fixed(&mut nid.szTip, APP_NAME);
    Shell_NotifyIconW(NIM_ADD, &nid);
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn notify(title: &str, message: &str) {
    let hwnd = APP.with(|app| {
        app.borrow()
            .as_ref()
            .map(|app| app.hwnd)
            .unwrap_or(null_mut())
    });
    if hwnd.is_null() {
        return;
    }
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    nid.uFlags = NIF_INFO;
    nid.dwInfoFlags = NIIF_INFO;
    copy_wide_to_fixed(&mut nid.szInfoTitle, title);
    copy_wide_to_fixed(&mut nid.szInfo, message);
    Shell_NotifyIconW(NIM_MODIFY, &nid);
}

unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = CreatePopupMenu();
    if menu.is_null() {
        return;
    }
    AppendMenuW(
        menu,
        MF_STRING,
        MENU_REGION,
        wide("Capture region\tPrtSc").as_ptr(),
    );
    AppendMenuW(
        menu,
        MF_STRING,
        MENU_FULLSCREEN,
        wide("Capture fullscreen\tShift+PrtSc").as_ptr(),
    );
    AppendMenuW(
        menu,
        MF_STRING,
        MENU_OPEN_FOLDER,
        wide("Open screenshots folder").as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    let startup_flags = MF_STRING
        | if startup_enabled() {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
    AppendMenuW(
        menu,
        startup_flags,
        MENU_STARTUP,
        wide("Start with Windows").as_ptr(),
    );
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    AppendMenuW(menu, MF_STRING, MENU_EXIT, wide("Exit").as_ptr());

    let mut point: POINT = zeroed();
    GetCursorPos(&mut point);
    SetForegroundWindow(hwnd);
    TrackPopupMenu(menu, TPM_RIGHTBUTTON, point.x, point.y, 0, hwnd, null());
    DestroyMenu(menu);
}

unsafe fn open_screenshot_folder() {
    let dir = APP.with(|app| {
        app.borrow()
            .as_ref()
            .map(|app| app.screenshot_dir.clone())
            .unwrap_or_else(screenshot_dir)
    });
    let _ = fs::create_dir_all(&dir);
    ShellExecuteW(
        null_mut(),
        wide("open").as_ptr(),
        wide_path(&dir).as_ptr(),
        null(),
        null(),
        SW_SHOWNORMAL,
    );
}

unsafe fn toggle_startup() {
    let result = if startup_enabled() {
        set_startup(false)
    } else {
        set_startup(true)
    };
    if let Err(err) = result {
        notify("Startup setting failed", &err);
    }
}

unsafe fn ensure_startup_enabled() {
    if !startup_enabled() {
        let _ = set_startup(true);
    }
}

unsafe fn startup_enabled() -> bool {
    let mut key: HKEY = null_mut();
    if RegOpenKeyExW(
        HKEY_CURRENT_USER,
        wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run").as_ptr(),
        0,
        KEY_QUERY_VALUE,
        &mut key,
    ) != ERROR_SUCCESS
    {
        return false;
    }

    let mut data_len = 0u32;
    let status = RegQueryValueExW(
        key,
        wide(STARTUP_VALUE).as_ptr(),
        null_mut(),
        null_mut(),
        null_mut(),
        &mut data_len,
    );
    RegCloseKey(key);
    status == ERROR_SUCCESS && data_len > 0
}

unsafe fn set_startup(enabled: bool) -> Result<(), String> {
    let mut key: HKEY = null_mut();
    let status = RegCreateKeyExW(
        HKEY_CURRENT_USER,
        wide("Software\\Microsoft\\Windows\\CurrentVersion\\Run").as_ptr(),
        0,
        null(),
        0,
        KEY_SET_VALUE,
        null(),
        &mut key,
        null_mut(),
    );
    if status != ERROR_SUCCESS {
        return Err("Could not open the Windows startup registry key.".to_string());
    }

    let result = if enabled {
        let exe = std::env::current_exe()
            .map_err(|err| format!("Could not locate Copyshot executable: {err}"))?;
        let command = format!("\"{}\"", exe.display());
        let bytes = wide(&command);
        RegSetValueExW(
            key,
            wide(STARTUP_VALUE).as_ptr(),
            0,
            REG_SZ,
            bytes.as_ptr() as *const u8,
            (bytes.len() * 2) as u32,
        )
    } else {
        RegDeleteValueW(key, wide(STARTUP_VALUE).as_ptr())
    };
    RegCloseKey(key);

    if result == ERROR_SUCCESS || (!enabled && result == ERROR_FILE_NOT_FOUND) {
        Ok(())
    } else {
        Err("Could not update the Windows startup registry value.".to_string())
    }
}

fn screenshot_dir() -> PathBuf {
    std::env::var_os("USERPROFILE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Pictures")
        .join("Screenshots")
}

fn timestamp() -> String {
    unsafe {
        let mut now: SYSTEMTIME = zeroed();
        GetLocalTime(&mut now);
        format!(
            "{:04}-{:02}-{:02} {:02}-{:02}-{:02}",
            now.wYear, now.wMonth, now.wDay, now.wHour, now.wMinute, now.wSecond
        )
    }
}

fn bgra_to_rgba(bgra: &[u8]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(bgra.len());
    for pixel in bgra.chunks_exact(4) {
        rgba.push(pixel[2]);
        rgba.push(pixel[1]);
        rgba.push(pixel[0]);
        rgba.push(255);
    }
    rgba
}

fn bitmap_info(width: i32, height: i32) -> BITMAPINFO {
    BITMAPINFO {
        bmiHeader: BITMAPINFOHEADER {
            biSize: size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB,
            biSizeImage: (width.abs() * height.abs() * 4) as u32,
            biXPelsPerMeter: 0,
            biYPelsPerMeter: 0,
            biClrUsed: 0,
            biClrImportant: 0,
        },
        bmiColors: [RGBQUAD {
            rgbBlue: 0,
            rgbGreen: 0,
            rgbRed: 0,
            rgbReserved: 0,
        }],
    }
}

fn rgb(red: u8, green: u8, blue: u8) -> COLORREF {
    red as u32 | ((green as u32) << 8) | ((blue as u32) << 16)
}

unsafe fn load_app_icon() -> HICON {
    let icon_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("src")
        .join("copyshot.ico");
    let icon = LoadImageW(
        null_mut(),
        wide_path(&icon_path).as_ptr(),
        IMAGE_ICON,
        0,
        0,
        LR_LOADFROMFILE | LR_DEFAULTSIZE,
    ) as HICON;
    if icon.is_null() {
        LoadIconW(null_mut(), IDI_APPLICATION)
    } else {
        icon
    }
}

unsafe fn shift_pressed() -> bool {
    GetAsyncKeyState(VK_SHIFT as i32) < 0
}

fn loword(value: usize) -> u16 {
    (value & 0xffff) as u16
}

fn lparam_point(lparam: LPARAM) -> POINT {
    POINT {
        x: (lparam as i16) as i32,
        y: ((lparam >> 16) as i16) as i32,
    }
}

fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(Some(0)).collect()
}

fn wide_path(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn copy_wide_to_fixed<const N: usize>(target: &mut [u16; N], value: &str) {
    let encoded: Vec<u16> = value.encode_utf16().collect();
    let max = N.saturating_sub(1).min(encoded.len());
    target[..max].copy_from_slice(&encoded[..max]);
    target[max] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_rect_from_points() {
        let rect = Rect::from_points(POINT { x: 20, y: 30 }, POINT { x: 5, y: 10 }).unwrap();
        assert_eq!(
            rect,
            Rect {
                x: 5,
                y: 10,
                width: 15,
                height: 20
            }
        );
    }

    #[test]
    fn rejects_tiny_rects() {
        assert_eq!(
            Rect::from_points(POINT { x: 10, y: 10 }, POINT { x: 11, y: 30 }),
            None
        );
        assert_eq!(
            Rect::from_points(POINT { x: 10, y: 10 }, POINT { x: 30, y: 11 }),
            None
        );
    }

    #[test]
    fn converts_bgra_to_rgba() {
        assert_eq!(bgra_to_rgba(&[1, 2, 3, 4]), vec![3, 2, 1, 255]);
    }
}
