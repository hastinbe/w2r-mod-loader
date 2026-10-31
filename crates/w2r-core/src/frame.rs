//! The host's own hook on the game's main thread. `BeginNamedWindow` (the game's Nuklear window
//! entry) runs once per UI window per frame; wc2r-plus hooks the same spot. From here, later
//! steps load plugins on first call and derive the lobby events from which windows are drawn.

use core::ffi::{c_char, c_void};
use std::sync::Once;
use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

use w2r_abi::{W2R_LOG_ERROR, W2R_LOG_INFO};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use crate::guard::guarded;
use crate::hooks::{self, Owner};
use crate::host::Log;
use crate::pe::Image;
use crate::symbols::Symbols;

pub const SYMBOL: &str = "BeginNamedWindow";

type BeginNamedWindowFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, f32, f32, f32, f32, u32) -> i32;

static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static MAIN_THREAD: AtomicU32 = AtomicU32::new(0);
static FIRST: Once = Once::new();

/// The game's main thread id, once the first frame has been seen; 0 before.
pub fn main_thread_id() -> u32 {
    MAIN_THREAD.load(Ordering::Acquire)
}

/// Installs the frame hook. The trampoline is published before the hook goes live, so a frame
/// that races the install still reaches the game.
pub fn install(image: &Image, symbols: &Symbols, log: &dyn Log) -> bool {
    let Some(target) = symbols.address(SYMBOL) else {
        log.line(
            W2R_LOG_ERROR,
            "host",
            &format!("{SYMBOL} is not in the symbol table"),
        );
        return false;
    };
    // SAFETY: `detour` matches BeginNamedWindow's cdecl signature from the symbol table.
    let result = unsafe {
        hooks::global().install(
            Owner::Host,
            image,
            target,
            detour as *mut c_void,
            &format!("host:{SYMBOL}"),
            |original| ORIGINAL.store(original as usize, Ordering::Release),
        )
    };
    match result {
        Ok(_) => {
            log.line(
                W2R_LOG_INFO,
                "host",
                &format!("hooked {SYMBOL} @ {target:#010x}"),
            );
            true
        }
        Err(e) => {
            log.line(
                W2R_LOG_ERROR,
                "host",
                &format!("could not hook {SYMBOL}: {e:?}"),
            );
            false
        }
    }
}

unsafe extern "C" fn detour(
    ctx: *mut c_void,
    name: *const c_char,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    flags: u32,
) -> i32 {
    let original: BeginNamedWindowFn =
        unsafe { core::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
    #[cfg(target_arch = "x86")]
    if !name.is_null() && unsafe { core::ffi::CStr::from_ptr(name) }.to_bytes() == b"game_hud" {
        // SAFETY: `original` is the game's own BeginNamedWindow; `ctx` is its argument.
        unsafe { crate::hudpanel::before_hud(ctx, original) };
        // SAFETY: as above.
        unsafe { crate::ingamekick::before_hud(ctx, original) };
        // SAFETY: as above.
        unsafe { crate::apm::before_hud(ctx, original) };
        // SAFETY: as above.
        unsafe { crate::netpanel::before_hud(ctx, original) };
        // SAFETY: as above.
        unsafe { crate::ping::before_hud(ctx, original) };
    }
    crate::watchdog::mark(crate::watchdog::FRAME_GAME);
    let opened = unsafe { original(ctx, name, x, y, w, h, flags) };
    crate::watchdog::mark(crate::watchdog::FRAME_HOST);
    // Host work runs after the game's own and under the crash guard: a host bug must not take
    // the game down, and the game must never see a different return value because of it.
    if let Err(fault) = guarded(|| on_window(name, opened)) {
        crate::session::host_fault(SYMBOL, &fault);
    }
    // The one exception to "the game never sees a different return value": the replacement
    // lobby screen (docs/lobby-screen.md) draws inside the lobby window, ends it, and reports it
    // closed so the game skips its own content.
    #[cfg(target_arch = "x86")]
    if opened != 0
        && !name.is_null()
        && unsafe { core::ffi::CStr::from_ptr(name) }.to_bytes() == crate::lobby::WINDOW.as_bytes()
        && crate::lobbyscreen::try_draw()
    {
        return 0;
    }
    // The replacement Alliances dialog (with the host's Kick buttons) does the same.
    #[cfg(target_arch = "x86")]
    if opened != 0
        && !name.is_null()
        && unsafe { core::ffi::CStr::from_ptr(name) }.to_bytes() == crate::alliancesscreen::WINDOW
        && crate::alliancesscreen::try_draw()
    {
        return 0;
    }
    opened
}

fn on_window(name: *const c_char, _opened: i32) {
    FIRST.call_once(|| {
        let tid = unsafe { GetCurrentThreadId() };
        MAIN_THREAD.store(tid, Ordering::Release);
        if let Some(s) = crate::entry::startup() {
            s.log.line(
                W2R_LOG_INFO,
                "host",
                &format!("main thread reached (thread {tid})"),
            );
        }
    });
    #[cfg(target_arch = "x86")]
    crate::graphicsmode::watch();
    let name = if name.is_null() {
        None
    } else {
        unsafe { core::ffi::CStr::from_ptr(name) }.to_str().ok()
    };
    crate::session::on_window(name);
}
