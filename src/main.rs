#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

#[cfg(not(windows))]
compile_error!("Copyshot only supports Windows.");

mod update;

use std::cell::RefCell;
use std::ffi::c_void;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, ErrorKind};
use std::mem::{size_of, zeroed};
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::ptr::{null, null_mut};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Dwm::{DwmGetWindowAttribute, DWMWA_EXTENDED_FRAME_BOUNDS};
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::Media::Audio::{PlaySoundW, SND_ASYNC, SND_MEMORY, SND_NODEFAULT};
use windows_sys::Win32::System::DataExchange::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Memory::*;
use windows_sys::Win32::System::Ole::CF_DIB;
use windows_sys::Win32::System::Registry::*;
use windows_sys::Win32::System::SystemInformation::GetLocalTime;
use windows_sys::Win32::System::SystemServices::MK_LBUTTON;
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::*;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const APP_NAME: &str = "Copyshot";
const WM_TRAYICON: u32 = WM_APP + 1;
const WM_CAPTURE_REGION: u32 = WM_APP + 2;
const WM_CAPTURE_FULLSCREEN: u32 = WM_APP + 3;
const WM_CAPTURE_WINDOW: u32 = WM_APP + 4;
const WM_UPDATE_FOUND: u32 = WM_APP + 5;
const WM_UPDATE_INSTALL_DONE: u32 = WM_APP + 6;
const WM_ALREADY_RUNNING: u32 = WM_APP + 7;
const TRAY_UID: u32 = 1;
const MENU_REGION: usize = 1001;
const MENU_FULLSCREEN: usize = 1002;
const MENU_OPEN_FOLDER: usize = 1003;
const MENU_STARTUP: usize = 1004;
const MENU_EXIT: usize = 1005;
const MENU_SOUND: usize = 1006;
const MENU_UPDATE: usize = 1007;
const STARTUP_VALUE: &str = "Copyshot";
const MAIN_CLASS: &str = "CopyshotMainWindow";
const OVERLAY_FADE_TIMER: usize = 1;
const OVERLAY_FADE_INTERVAL_MS: u32 = 12;
const OVERLAY_DIM_ALPHA: u8 = 132;
const OVERLAY_FADE_STEP: u8 = 18;
const UPDATE_TIMER: usize = 2;
// Copyshot usually runs for days, so check again twice a day, not only at login.
const UPDATE_INTERVAL_MS: u32 = 12 * 60 * 60 * 1000;
const SOUND_OFF_MARKER: &str = "sound-off";
// An unassigned virtual key; tapping it stops a lone Alt release from opening menu bars.
const VK_MENU_MASK: u16 = 0xE8;

thread_local! {
    static APP: RefCell<Option<AppState>> = const { RefCell::new(None) };
    static OVERLAY: RefCell<Option<OverlayState>> = const { RefCell::new(None) };
}

// Background update threads hand results to the UI thread through these.
static FOUND_UPDATE: Mutex<Option<update::Update>> = Mutex::new(None);
static UPDATE_INSTALL_ERROR: Mutex<Option<String>> = Mutex::new(None);

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
    sound_enabled: bool,
    available_update: Option<update::Update>,
    update_balloon_shown: bool,
    installing_update: bool,
}

struct OverlayState {
    hwnd: HWND,
    font: HFONT,
    scale: f32,
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
        // Without this, Windows reports scaled sizes at non-100% display scaling and captures get cropped.
        if SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) == 0 {
            SetProcessDPIAware();
        }

        // One Copyshot per session; a second launch just tells the first one to say hello.
        // The handle stays open for the life of the process.
        let _instance = CreateMutexW(null(), 0, wide("Local\\Copyshot.SingleInstance").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            let existing = FindWindowW(wide(MAIN_CLASS).as_ptr(), null());
            if !existing.is_null() {
                PostMessageW(existing, WM_ALREADY_RUNNING, 0, 0);
            }
            return Ok(());
        }

        let hinstance = GetModuleHandleW(null());
        if hinstance.is_null() {
            return Err("Could not get module handle.".to_string());
        }

        register_window_classes(hinstance)?;

        let hwnd = CreateWindowExW(
            0,
            wide(MAIN_CLASS).as_ptr(),
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
                sound_enabled: !settings_dir().join(SOUND_OFF_MARKER).exists(),
                available_update: None,
                update_balloon_shown: false,
                installing_update: false,
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

        enable_startup_on_first_run();
        // Point the startup entry at this exe, e.g. after moving from a dev build to the installer.
        if startup_enabled() {
            let _ = set_startup(true);
        }

        start_update_check(hwnd);
        SetTimer(hwnd, UPDATE_TIMER, UPDATE_INTERVAL_MS, None);

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
    let main_class_name = wide(MAIN_CLASS);
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
                MENU_SOUND => toggle_sound(),
                MENU_UPDATE => install_update(),
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
        WM_CAPTURE_WINDOW => {
            capture_active_window();
            0
        }
        WM_ALREADY_RUNNING => {
            notify(
                "Copyshot is already running",
                "It lives in the tray. Press Print Screen to capture.",
            );
            0
        }
        WM_UPDATE_FOUND => {
            on_update_found();
            0
        }
        WM_UPDATE_INSTALL_DONE => {
            if wparam == 1 {
                // The installer is running and will start the new version.
                DestroyWindow(hwnd);
            } else {
                with_app(|app| app.installing_update = false);
                let error = UPDATE_INSTALL_ERROR
                    .lock()
                    .ok()
                    .and_then(|mut error| error.take())
                    .unwrap_or_default();
                notify("Update failed", &error);
            }
            0
        }
        WM_TIMER => {
            if wparam == UPDATE_TIMER {
                start_update_check(hwnd);
            }
            0
        }
        WM_TRAYICON => {
            // NOTIFYICON_VERSION: right-click and Shift+F10 both arrive as WM_CONTEXTMENU.
            let event = loword(lparam as usize) as u32;
            if event == WM_CONTEXTMENU {
                show_tray_menu(hwnd);
            } else if event == WM_LBUTTONDBLCLK {
                start_region_capture();
            } else if event == NIN_BALLOONUSERCLICK
                && with_app(|app| app.update_balloon_shown).unwrap_or(false)
            {
                install_update();
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
                let alt = key.flags & LLKHF_ALTDOWN != 0;
                let message = if alt {
                    mask_alt_release();
                    WM_CAPTURE_WINDOW
                } else if shift_pressed() {
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
                        // The size label is drawn on top at paint time, outside the selection.
                        for selection in [old, new].into_iter().flatten() {
                            invalidate_rect(hwnd, Some(overlay_label_rect(overlay, selection)));
                        }
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
        WM_RBUTTONUP => {
            ReleaseCapture();
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
            let font = OVERLAY.with(|overlay| {
                overlay
                    .borrow()
                    .as_ref()
                    .filter(|overlay| overlay.hwnd == hwnd)
                    .map(|overlay| overlay.font)
            });
            if let Some(font) = font {
                DeleteObject(font as _);
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

    let scale = GetDpiForWindow(hwnd).max(96) as f32 / 96.0;
    let font = CreateFontW(
        -(13.0 * scale).round() as i32,
        0,
        0,
        0,
        FW_SEMIBOLD as i32,
        0,
        0,
        0,
        DEFAULT_CHARSET as u32,
        0,
        0,
        CLEARTYPE_QUALITY as u32,
        0,
        wide("Segoe UI").as_ptr(),
    );

    OVERLAY.with(|overlay| {
        *overlay.borrow_mut() = Some(OverlayState {
            hwnd,
            font,
            scale,
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
            if overlay.fade != FadeDirection::Out {
                if let Some(selection) = current_selection(overlay) {
                    paint_size_label(hdc, overlay, selection);
                }
            }
        }
    });

    EndPaint(hwnd, &ps);
}

unsafe fn paint_size_label(hdc: HDC, overlay: &OverlayState, selection: Rect) {
    let label = overlay_label_rect(overlay, selection);
    let mut rect = RECT {
        left: label.x,
        top: label.y,
        right: label.x + label.width,
        bottom: label.y + label.height,
    };
    let brush = CreateSolidBrush(rgb(24, 24, 27));
    FillRect(hdc, &rect, brush);
    DeleteObject(brush as _);

    let old_font = SelectObject(hdc, overlay.font as _);
    SetBkMode(hdc, TRANSPARENT as i32);
    SetTextColor(hdc, rgb(255, 255, 255));
    let text = wide(&format!(
        "{} \u{00d7} {}",
        selection.width, selection.height
    ));
    DrawTextW(
        hdc,
        text.as_ptr(),
        -1,
        &mut rect,
        DT_CENTER | DT_VCENTER | DT_SINGLELINE,
    );
    SelectObject(hdc, old_font);
}

fn overlay_label_rect(overlay: &OverlayState, selection: Rect) -> Rect {
    label_rect(
        selection,
        overlay.scale,
        overlay.frozen.width as i32,
        overlay.frozen.height as i32,
    )
}

/// Where the "W × H" label goes: above the selection, else below it, else inside it,
/// always kept on screen.
fn label_rect(selection: Rect, scale: f32, screen_width: i32, screen_height: i32) -> Rect {
    let width = (112.0 * scale).round() as i32;
    let height = (24.0 * scale).round() as i32;
    let gap = (6.0 * scale).round() as i32;
    let above = selection.y - height - gap;
    let below = selection.y + selection.height + gap;
    let y = if above >= 0 {
        above
    } else if below + height <= screen_height {
        below
    } else {
        selection.y + gap
    };
    Rect {
        x: selection.x.min(screen_width - width).max(0),
        y,
        width,
        height,
    }
}

unsafe fn capture_fullscreen() {
    capture_rect(virtual_screen());
}

unsafe fn virtual_screen() -> Rect {
    Rect {
        x: GetSystemMetrics(SM_XVIRTUALSCREEN),
        y: GetSystemMetrics(SM_YVIRTUALSCREEN),
        width: GetSystemMetrics(SM_CXVIRTUALSCREEN),
        height: GetSystemMetrics(SM_CYVIRTUALSCREEN),
    }
}

unsafe fn capture_active_window() {
    let window = GetForegroundWindow();
    if window.is_null() {
        notify(
            "Window capture failed",
            "There is no active window to capture.",
        );
        return;
    }
    // The DWM frame bounds skip the invisible resize borders that GetWindowRect includes.
    let mut bounds: RECT = zeroed();
    if DwmGetWindowAttribute(
        window,
        DWMWA_EXTENDED_FRAME_BOUNDS as u32,
        &mut bounds as *mut RECT as *mut c_void,
        size_of::<RECT>() as u32,
    ) < 0
        && GetWindowRect(window, &mut bounds) == 0
    {
        notify(
            "Window capture failed",
            "Could not find the window's position.",
        );
        return;
    }
    let window_rect = Rect {
        x: bounds.left,
        y: bounds.top,
        width: bounds.right - bounds.left,
        height: bounds.bottom - bounds.top,
    };
    match intersect_rect(window_rect, virtual_screen()) {
        Some(rect) => capture_rect(rect),
        None => notify("Window capture failed", "The active window is off screen."),
    }
}

fn intersect_rect(a: Rect, b: Rect) -> Option<Rect> {
    let left = a.x.max(b.x);
    let top = a.y.max(b.y);
    let right = (a.x + a.width).min(b.x + b.width);
    let bottom = (a.y + a.height).min(b.y + b.height);
    (right > left && bottom > top).then_some(Rect {
        x: left,
        y: top,
        width: right - left,
        height: bottom - top,
    })
}

unsafe fn mask_alt_release() {
    let mut inputs: [INPUT; 2] = zeroed();
    for (index, input) in inputs.iter_mut().enumerate() {
        input.r#type = INPUT_KEYBOARD;
        input.Anonymous.ki.wVk = VK_MENU_MASK;
        if index == 1 {
            input.Anonymous.ki.dwFlags = KEYEVENTF_KEYUP;
        }
    }
    SendInput(2, inputs.as_ptr(), size_of::<INPUT>() as i32);
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
    if with_app(|app| app.sound_enabled).unwrap_or(false) {
        play_chime();
    }
    Ok(path)
}

fn play_chime() {
    static CHIME: OnceLock<Vec<u8>> = OnceLock::new();
    let wav = CHIME.get_or_init(chime_wav);
    // SND_ASYNC keeps reading the buffer after returning, so it must be 'static.
    unsafe {
        PlaySoundW(
            wav.as_ptr() as *const u16,
            null_mut(),
            SND_MEMORY | SND_ASYNC | SND_NODEFAULT,
        );
    }
}

/// A quiet two-note chime (E6 then A6), generated so no audio file has to ship.
fn chime_wav() -> Vec<u8> {
    const RATE: u32 = 44_100;
    const SECONDS: f32 = 0.3;
    const VOLUME: f32 = 0.12;
    let len = (RATE as f32 * SECONDS) as usize;
    let mut samples = vec![0f32; len];
    for (frequency, start) in [(1318.5f32, 0.0f32), (1760.0, 0.07)] {
        let offset = (start * RATE as f32) as usize;
        for (index, sample) in samples.iter_mut().enumerate().skip(offset) {
            let t = (index - offset) as f32 / RATE as f32;
            let envelope = (t * 400.0).min(1.0) * (-t * 20.0).exp();
            *sample += (std::f32::consts::TAU * frequency * t).sin() * envelope * VOLUME;
        }
    }

    let data_len = (len * 2) as u32;
    let mut wav = Vec::with_capacity(44 + len * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&RATE.to_le_bytes());
    wav.extend_from_slice(&(RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2u16.to_le_bytes());
    wav.extend_from_slice(&16u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for sample in samples {
        let value = (sample.clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
        wav.extend_from_slice(&value.to_le_bytes());
    }
    wav
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
    let (file, path) = create_unique_file(&dir, &format!("Screenshot {}", timestamp()))?;
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

/// Creates `<stem>.png`, or `<stem> (2).png` and so on, so captures taken within the same
/// second never overwrite each other.
fn create_unique_file(dir: &Path, stem: &str) -> Result<(File, PathBuf), String> {
    for attempt in 1.. {
        let name = if attempt == 1 {
            format!("{stem}.png")
        } else {
            format!("{stem} ({attempt}).png")
        };
        let path = dir.join(name);
        match OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(err) if err.kind() == ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(format!("Could not create PNG file: {err}")),
        }
    }
    unreachable!()
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
    // Needed for balloon-click notifications (the update prompt).
    nid.Anonymous.uVersion = NOTIFYICON_VERSION;
    Shell_NotifyIconW(NIM_SETVERSION, &nid);
}

unsafe fn remove_tray_icon(hwnd: HWND) {
    let mut nid: NOTIFYICONDATAW = zeroed();
    nid.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    nid.hWnd = hwnd;
    nid.uID = TRAY_UID;
    Shell_NotifyIconW(NIM_DELETE, &nid);
}

unsafe fn notify(title: &str, message: &str) {
    show_balloon(title, message);
    with_app(|app| app.update_balloon_shown = false);
}

unsafe fn show_balloon(title: &str, message: &str) {
    let Some(hwnd) = with_app(|app| app.hwnd) else {
        return;
    };
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
    let (update, installing) = with_app(|app| {
        (
            app.available_update
                .as_ref()
                .map(|update| update.version.clone()),
            app.installing_update,
        )
    })
    .unwrap_or((None, false));
    if let Some(version) = update {
        let (flags, label) = if installing {
            (MF_STRING | MF_GRAYED, "Installing update...".to_string())
        } else {
            (MF_STRING, format!("Install update {version}"))
        };
        AppendMenuW(menu, flags, MENU_UPDATE, wide(&label).as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, null());
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
    // No menu entry for window capture: opening the menu makes the taskbar the active window.
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
    let sound_flags = MF_STRING
        | if with_app(|app| app.sound_enabled).unwrap_or(false) {
            MF_CHECKED
        } else {
            MF_UNCHECKED
        };
    AppendMenuW(
        menu,
        sound_flags,
        MENU_SOUND,
        wide("Play capture sound").as_ptr(),
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

fn toggle_sound() {
    let Some(enabled) = with_app(|app| {
        app.sound_enabled = !app.sound_enabled;
        app.sound_enabled
    }) else {
        return;
    };
    let dir = settings_dir();
    let marker = dir.join(SOUND_OFF_MARKER);
    let _ = if enabled {
        fs::remove_file(marker)
    } else {
        fs::create_dir_all(&dir).and_then(|_| fs::write(marker, ""))
    };
    if enabled {
        play_chime();
    }
}

fn start_update_check(hwnd: HWND) {
    // shortcut: dev builds never self-update; test the flow with a release build.
    if cfg!(debug_assertions) {
        return;
    }
    let hwnd = hwnd as usize;
    std::thread::spawn(move || {
        if let Ok(Some(update)) = update::check() {
            if let Ok(mut found) = FOUND_UPDATE.lock() {
                *found = Some(update);
            }
            unsafe {
                PostMessageW(hwnd as HWND, WM_UPDATE_FOUND, 0, 0);
            }
        }
    });
}

unsafe fn on_update_found() {
    let Some(update) = FOUND_UPDATE.lock().ok().and_then(|mut found| found.take()) else {
        return;
    };
    let is_new = with_app(|app| {
        let is_new = app.available_update.as_ref() != Some(&update);
        app.available_update = Some(update.clone());
        is_new
    })
    .unwrap_or(false);
    if is_new {
        show_balloon(
            &format!("Copyshot {} is available", update.version),
            "Click here, or use the tray menu, to install it.",
        );
        with_app(|app| app.update_balloon_shown = true);
    }
}

unsafe fn install_update() {
    let Some((hwnd, update)) = with_app(|app| {
        if app.installing_update {
            return None;
        }
        let update = app.available_update.clone()?;
        app.installing_update = true;
        Some((app.hwnd as usize, update))
    })
    .flatten() else {
        return;
    };
    std::thread::spawn(move || {
        let succeeded = match update::download_and_launch(&update) {
            Ok(()) => 1,
            Err(err) => {
                if let Ok(mut error) = UPDATE_INSTALL_ERROR.lock() {
                    *error = Some(err);
                }
                0
            }
        };
        unsafe {
            PostMessageW(hwnd as HWND, WM_UPDATE_INSTALL_DONE, succeeded, 0);
        }
    });
}

fn with_app<T>(f: impl FnOnce(&mut AppState) -> T) -> Option<T> {
    APP.with(|app| app.borrow_mut().as_mut().map(f))
}

fn settings_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
        .join(APP_NAME)
}

// Only enable startup once, so turning it off in the tray menu sticks across launches.
unsafe fn enable_startup_on_first_run() {
    if std::env::var_os("LOCALAPPDATA").is_none() {
        return;
    }
    let dir = settings_dir();
    let marker = dir.join("startup-configured");
    if marker.exists() {
        return;
    }
    if !startup_enabled() && set_startup(true).is_err() {
        return;
    }
    let _ = fs::create_dir_all(&dir).and_then(|_| fs::write(&marker, ""));
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
    // build.rs embeds src/copyshot.ico as icon resource ID 1.
    let icon = LoadImageW(
        GetModuleHandleW(null()),
        1 as _,
        IMAGE_ICON,
        GetSystemMetrics(SM_CXSMICON),
        GetSystemMetrics(SM_CYSMICON),
        0,
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

pub(crate) fn wide(value: &str) -> Vec<u16> {
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

    #[test]
    fn clamps_rect_to_bounds() {
        let rect = Rect {
            x: -5,
            y: 8,
            width: 20,
            height: 20,
        };
        assert_eq!(
            clamp_rect_to_size(10, 10, rect),
            Some(Rect {
                x: 0,
                y: 8,
                width: 10,
                height: 2
            })
        );
        assert_eq!(
            clamp_rect_to_size(
                10,
                10,
                Rect {
                    x: 12,
                    y: 0,
                    width: 5,
                    height: 5
                }
            ),
            None
        );
    }

    #[test]
    fn crops_odd_sized_capture() {
        // 3x2 capture where each pixel's blue channel is its index.
        let capture = Capture {
            width: 3,
            height: 2,
            bgra: (0..6u8).flat_map(|i| [i, 0, 0, 255]).collect(),
        };
        let cropped = crop_capture(
            &capture,
            Rect {
                x: 1,
                y: 0,
                width: 2,
                height: 2,
            },
        )
        .unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 2));
        let blues: Vec<u8> = cropped.bgra.chunks(4).map(|p| p[0]).collect();
        assert_eq!(blues, vec![1, 2, 4, 5]);
    }

    fn rect(x: i32, y: i32, width: i32, height: i32) -> Rect {
        Rect {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn size_label_sits_above_then_below_then_inside_and_stays_on_screen() {
        // Room above: label goes above.
        let label = label_rect(rect(100, 200, 300, 100), 1.0, 1920, 1080);
        assert_eq!((label.x, label.y + label.height + 6), (100, 200));
        // Selection touches the top: label goes below.
        let label = label_rect(rect(100, 0, 300, 100), 1.0, 1920, 1080);
        assert_eq!(label.y, 106);
        // Selection fills the screen: label goes inside.
        let label = label_rect(rect(0, 0, 1920, 1080), 1.0, 1920, 1080);
        assert_eq!(label.y, 6);
        // Near the right edge: label is pulled back on screen.
        let label = label_rect(rect(1900, 500, 20, 20), 1.0, 1920, 1080);
        assert_eq!(label.x + label.width, 1920);
        // Label grows with display scaling.
        assert_eq!(
            label_rect(rect(100, 200, 10, 10), 2.0, 4000, 4000).height,
            48
        );
    }

    #[test]
    fn intersects_window_with_screen() {
        assert_eq!(
            intersect_rect(rect(-50, 10, 200, 100), rect(0, 0, 1920, 1080)),
            Some(rect(0, 10, 150, 100))
        );
        assert_eq!(
            intersect_rect(rect(2000, 0, 100, 100), rect(0, 0, 1920, 1080)),
            None
        );
    }

    #[test]
    fn chime_is_a_short_quiet_pcm_wav() {
        let wav = chime_wav();
        assert_eq!(&wav[0..4], b"RIFF");
        assert_eq!(&wav[8..16], b"WAVEfmt ");
        let data_len = u32::from_le_bytes(wav[40..44].try_into().unwrap()) as usize;
        assert_eq!(wav.len(), 44 + data_len);
        let peak = wav[44..]
            .chunks_exact(2)
            .map(|s| i16::from_le_bytes([s[0], s[1]]).unsigned_abs())
            .max()
            .unwrap();
        assert!(peak > 1000 && peak < i16::MAX as u16 / 4, "peak {peak}");
    }

    #[test]
    fn same_second_captures_get_numbered_names() {
        let dir = std::env::temp_dir().join(format!("copyshot-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        let names: Vec<String> = (0..3)
            .map(|_| {
                let (_, path) = create_unique_file(&dir, "Screenshot 2026").unwrap();
                path.file_name().unwrap().to_string_lossy().into_owned()
            })
            .collect();
        fs::remove_dir_all(&dir).unwrap();
        assert_eq!(
            names,
            [
                "Screenshot 2026.png",
                "Screenshot 2026 (2).png",
                "Screenshot 2026 (3).png"
            ]
        );
    }
}
