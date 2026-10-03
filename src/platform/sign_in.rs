//! usagio's own sign-in window: the OS's web engine via wry (WKWebView on
//! macOS, WebView2 on Windows, WebKitGTK on Linux), one persistent cookie
//! store per account. Each account keeps its own vendor session here — apart
//! from the user's everyday browser, which is usually signed in as someone
//! else — so renewing is normally a single "Authorize" click.
//!
//! Must be opened on the UI thread (menu clicks already run there). The window
//! closes itself once the flow finishes; closing it first cancels the flow.
//! Reached through `Platform::open_sign_in_window`.

use std::sync::Arc;

use anyhow::{bail, Context, Result};

use crate::login::Progress;

pub(super) fn available() -> bool {
    imp::available()
}

pub(super) fn open(
    title: &str,
    url: &str,
    slug: &str,
    key: &str,
    progress: Arc<Progress>,
) -> Result<()> {
    imp::open(title, url, slug, key, progress)
}

/// Open `url` in the user's default browser.
pub(super) fn open_url(url: &str) -> Result<()> {
    #[cfg(target_os = "macos")]
    let mut cmd = std::process::Command::new("open");
    #[cfg(target_os = "linux")]
    let mut cmd = std::process::Command::new("xdg-open");
    // Not `cmd /C start`: cmd.exe would split the URL at its `&`s.
    #[cfg(target_os = "windows")]
    let mut cmd = {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    };
    let status = cmd.arg(url).status().context("opening the browser")?;
    if !status.success() {
        bail!("opening the browser failed ({status})");
    }
    Ok(())
}

const WIDTH: f64 = 520.0;
const HEIGHT: f64 = 760.0;
const POLL_MS: u64 = 300;

#[cfg(target_os = "macos")]
mod imp {
    use std::cell::RefCell;
    use std::ffi::c_void;
    use std::ptr::NonNull;
    use std::sync::Arc;

    use anyhow::{Context, Result};
    use block2::RcBlock;
    use objc2::MainThreadMarker;
    use objc2_app_kit::{NSApplication, NSBackingStoreType, NSWindow, NSWindowStyleMask};
    use objc2_foundation::{
        NSOperatingSystemVersion, NSPoint, NSProcessInfo, NSRect, NSSize, NSString, NSTimer,
    };
    use raw_window_handle::{
        AppKitWindowHandle, HandleError, HasWindowHandle, RawWindowHandle, WindowHandle,
    };
    use wry::{WebViewBuilder, WebViewBuilderExtDarwin};

    use super::{Progress, HEIGHT, POLL_MS, WIDTH};

    struct ViewHandle(NonNull<c_void>);

    impl HasWindowHandle for ViewHandle {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let raw = RawWindowHandle::AppKit(AppKitWindowHandle::new(self.0));
            // SAFETY: the view outlives the builder call that borrows it.
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    /// Per-identifier WebKit data stores need macOS 14; before that wry would
    /// silently share one store across every account.
    pub fn available() -> bool {
        NSProcessInfo::processInfo().isOperatingSystemAtLeastVersion(NSOperatingSystemVersion {
            majorVersion: 14,
            minorVersion: 0,
            patchVersion: 0,
        })
    }

    pub fn open(
        title: &str,
        url: &str,
        slug: &str,
        key: &str,
        progress: Arc<Progress>,
    ) -> Result<()> {
        let mtm =
            MainThreadMarker::new().context("the sign-in window must open on the main thread")?;
        let rect = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(WIDTH, HEIGHT));
        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        // SAFETY: plain AppKit window construction on the main thread.
        let window = unsafe {
            NSWindow::initWithContentRect_styleMask_backing_defer(
                mtm.alloc(),
                rect,
                style,
                NSBackingStoreType::Buffered,
                false,
            )
        };
        // We own the window's lifetime (dropped below), not AppKit's close.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setTitle(&NSString::from_str(title));
        window.center();
        let view = window
            .contentView()
            .context("sign-in window has no content view")?;
        let handle = ViewHandle(NonNull::from(&*view).cast());
        let webview = WebViewBuilder::new()
            .with_url(url)
            .with_data_store_identifier(crate::login::store_id(slug, key))
            .build(&handle)
            .context("creating the sign-in web view")?;
        window.makeKeyAndOrderFront(None);
        // A menu-bar (accessory) app must activate to bring a window forward.
        #[allow(deprecated)]
        NSApplication::sharedApplication(mtm).activateIgnoringOtherApps(true);

        let slot = RefCell::new(Some((window, webview)));
        let tick = RcBlock::new(move |timer: NonNull<NSTimer>| {
            let mut slot = slot.borrow_mut();
            let Some((window, _)) = slot.as_ref() else {
                return;
            };
            // Hidden (minimized, or the app hidden with ⌘H) isn't closed.
            let closed_by_user = !window.isVisible()
                && !window.isMiniaturized()
                && !NSApplication::sharedApplication(mtm).isHidden();
            if !progress.is_finished() && !closed_by_user {
                return;
            }
            if closed_by_user {
                progress.cancel();
            }
            if let Some((window, webview)) = slot.take() {
                drop(webview);
                window.close();
            }
            // SAFETY: the run loop retains the timer while it is scheduled.
            unsafe { timer.as_ref().invalidate() };
        });
        // SAFETY: scheduled on the main run loop, which retains it.
        unsafe {
            NSTimer::scheduledTimerWithTimeInterval_repeats_block(
                POLL_MS as f64 / 1000.0,
                true,
                &tick,
            )
        };
        Ok(())
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::cell::{Cell, RefCell};
    use std::rc::Rc;
    use std::sync::Arc;
    use std::time::Duration;

    use anyhow::{bail, Context, Result};
    use gtk::glib;
    use gtk::prelude::*;
    use wry::{WebContext, WebViewBuilder, WebViewBuilderExtUnix};

    use super::{Progress, HEIGHT, POLL_MS, WIDTH};

    pub fn available() -> bool {
        gtk::is_initialized_main_thread()
    }

    pub fn open(
        title: &str,
        url: &str,
        slug: &str,
        key: &str,
        progress: Arc<Progress>,
    ) -> Result<()> {
        if !gtk::is_initialized_main_thread() {
            bail!("GTK isn't running on this thread");
        }
        let window = gtk::Window::new(gtk::WindowType::Toplevel);
        window.set_title(title);
        window.set_default_size(WIDTH as i32, HEIGHT as i32);
        window.set_position(gtk::WindowPosition::Center);
        let container = gtk::Box::new(gtk::Orientation::Vertical, 0);
        window.add(&container);
        let mut context = WebContext::new(Some(crate::login::store_dir(slug, key)?));
        let webview = WebViewBuilder::new_with_web_context(&mut context)
            .with_url(url)
            .build_gtk(&container)
            .context("creating the sign-in web view")?;
        window.show_all();
        window.present();

        let closed = Rc::new(Cell::new(false));
        {
            let closed = Rc::clone(&closed);
            window.connect_delete_event(move |_, _| {
                closed.set(true);
                glib::Propagation::Proceed
            });
        }
        let slot = RefCell::new(Some((window, webview, context)));
        glib::timeout_add_local(Duration::from_millis(POLL_MS), move || {
            if !progress.is_finished() && !closed.get() {
                return glib::ControlFlow::Continue;
            }
            if closed.get() {
                progress.cancel();
            }
            if let Some((window, webview, context)) = slot.borrow_mut().take() {
                drop(webview);
                drop(context);
                if !closed.get() {
                    window.close();
                }
            }
            glib::ControlFlow::Break
        });
        Ok(())
    }
}

#[cfg(target_os = "windows")]
mod imp {
    use std::cell::RefCell;
    use std::collections::HashMap;
    use std::num::NonZeroIsize;
    use std::sync::{Arc, Once};

    use anyhow::{Context, Result};
    use raw_window_handle::{
        HandleError, HasWindowHandle, RawWindowHandle, Win32WindowHandle, WindowHandle,
    };
    use windows::core::{w, HSTRING};
    use windows::Win32::Foundation::{HWND, LPARAM, LRESULT, WPARAM};
    use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, KillTimer, LoadCursorW, RegisterClassW,
        SetForegroundWindow, SetTimer, ShowWindow, CW_USEDEFAULT, IDC_ARROW, SW_SHOW,
        WINDOW_EX_STYLE, WM_CLOSE, WM_DESTROY, WM_TIMER, WNDCLASSW, WS_OVERLAPPEDWINDOW,
    };
    use wry::{WebContext, WebView, WebViewBuilder};

    use super::{Progress, HEIGHT, POLL_MS, WIDTH};

    const CLASS: windows::core::PCWSTR = w!("usagio-sign-in");
    const TIMER_ID: usize = 1;

    struct Entry {
        _webview: WebView,
        _context: WebContext,
        progress: Arc<Progress>,
    }

    thread_local! {
        static OPEN: RefCell<HashMap<isize, Entry>> = RefCell::new(HashMap::new());
    }

    struct HwndHandle(HWND);

    impl HasWindowHandle for HwndHandle {
        fn window_handle(&self) -> Result<WindowHandle<'_>, HandleError> {
            let hwnd = NonZeroIsize::new(self.0 .0 as isize).ok_or(HandleError::Unavailable)?;
            let raw = RawWindowHandle::Win32(Win32WindowHandle::new(hwnd));
            // SAFETY: the window outlives the builder call that borrows it.
            Ok(unsafe { WindowHandle::borrow_raw(raw) })
        }
    }

    pub fn available() -> bool {
        true
    }

    pub fn open(
        title: &str,
        url: &str,
        slug: &str,
        key: &str,
        progress: Arc<Progress>,
    ) -> Result<()> {
        static REGISTER: Once = Once::new();
        // SAFETY: Win32 calls on the UI thread with valid arguments.
        unsafe {
            // WebView2 needs an STA thread; an already-initialized thread just
            // reports so, which is fine.
            let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
            let instance = GetModuleHandleW(None).context("GetModuleHandleW")?;
            REGISTER.call_once(|| {
                let class = WNDCLASSW {
                    lpfnWndProc: Some(wndproc),
                    hInstance: instance.into(),
                    lpszClassName: CLASS,
                    hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
                    ..Default::default()
                };
                RegisterClassW(&class);
            });
            let hwnd = CreateWindowExW(
                WINDOW_EX_STYLE(0),
                CLASS,
                &HSTRING::from(title),
                WS_OVERLAPPEDWINDOW,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                WIDTH as i32,
                HEIGHT as i32,
                None,
                None,
                Some(instance.into()),
                None,
            )
            .context("creating the sign-in window")?;
            let mut context = WebContext::new(Some(crate::login::store_dir(slug, key)?));
            let webview = match WebViewBuilder::new_with_web_context(&mut context)
                .with_url(url)
                .build(&HwndHandle(hwnd))
            {
                Ok(w) => w,
                Err(e) => {
                    let _ = DestroyWindow(hwnd);
                    return Err(e)
                        .context("creating the sign-in web view (is WebView2 installed?)");
                }
            };
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetForegroundWindow(hwnd);
            SetTimer(Some(hwnd), TIMER_ID, POLL_MS as u32, None);
            OPEN.with(|m| {
                m.borrow_mut().insert(
                    hwnd.0 as isize,
                    Entry {
                        _webview: webview,
                        _context: context,
                        progress,
                    },
                )
            });
        }
        Ok(())
    }

    fn progress_of(hwnd: HWND) -> Option<Arc<Progress>> {
        OPEN.with(|m| {
            m.borrow()
                .get(&(hwnd.0 as isize))
                .map(|e| Arc::clone(&e.progress))
        })
    }

    unsafe extern "system" fn wndproc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        match msg {
            WM_TIMER => {
                if progress_of(hwnd).is_some_and(|p| p.is_finished()) {
                    let _ = DestroyWindow(hwnd);
                }
                LRESULT(0)
            }
            WM_CLOSE => {
                if let Some(p) = progress_of(hwnd) {
                    if !p.is_finished() {
                        p.cancel();
                    }
                }
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            }
            WM_DESTROY => {
                let _ = KillTimer(Some(hwnd), TIMER_ID);
                let entry = OPEN.with(|m| m.borrow_mut().remove(&(hwnd.0 as isize)));
                drop(entry);
                LRESULT(0)
            }
            _ => DefWindowProcW(hwnd, msg, wparam, lparam),
        }
    }
}
