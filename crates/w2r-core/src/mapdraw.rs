//! Drawing over the map view for plugins. A plugin that exports
//! `w2r_map_draw` is called every frame of a match or replay, just before the game opens its HUD
//! window, inside a window that covers the map view, the way the ping overlay draws its rings.
//! The table gives the screen point of world pixel (0, 0) and two drawing calls: a filled
//! rectangle and centred text, both in any colour with alpha.
//!
//! Stacking: both this window and `game_hud` are Nuklear background windows, and a background
//! window goes to the bottom of the stack when it is created. `game_hud` holds the resource bar,
//! whose text must stay on top of the bars as it does over the stock ones, so this window is only
//! created once `game_hud` already exists: on the first frame the HUD comes back after a gap
//! (match start, or any frame it was not drawn) this window is skipped, and Nuklear frees it, so
//! the next frame creates it again underneath `game_hud`. Checked in the live window list.

#[cfg(target_arch = "x86")]
pub use live::{before_hud, install};

#[cfg(not(target_arch = "x86"))]
pub fn install(_symbols: &crate::symbols::Symbols, _log: &dyn crate::host::Log) -> bool {
    false
}

#[cfg(target_arch = "x86")]
mod live {
    use core::ffi::c_void;
    use std::sync::OnceLock;
    use std::sync::atomic::{AtomicI32, AtomicU64, Ordering};
    use std::time::Instant;

    use w2r_abi::{W2R_LOG_ERROR, W2R_MAP_DRAW_V1_SIZE, W2rMapDraw, W2rStr};

    use crate::guard::guarded;
    use crate::host::Log;
    use crate::hudpanel::BeginFn;
    use crate::screenkit::game::Kit;
    use crate::screenkit::{ALIGN_CENTRE, Rect};
    use crate::symbols::Symbols;

    /// No title, no background, no input (the map below still takes the clicks).
    const WINDOW_FLAGS: u32 = 0x120;

    struct Game {
        view_origin: usize,
        view_rect: usize,
        camera: usize,
        kit: Kit,
    }

    // SAFETY: addresses of the game's globals, fixed for the session; used from the main thread.
    unsafe impl Send for Game {}
    unsafe impl Sync for Game {}

    static GAME: OnceLock<Game> = OnceLock::new();
    /// The map-view window's top-left on screen this frame: labels are placed relative to it,
    /// rectangles are not.
    static WINDOW_X: AtomicI32 = AtomicI32::new(0);
    static WINDOW_Y: AtomicI32 = AtomicI32::new(0);
    /// Milliseconds (since the first call) of the last `game_hud` frame, plus one; 0 before any.
    static LAST_HUD_MS: AtomicU64 = AtomicU64::new(0);
    /// A longer wait between `game_hud` frames means the HUD may have been freed and made again.
    const HUD_GAP_MS: u64 = 100;

    /// True when the previous `game_hud` frame was recent, so `game_hud` already exists and a
    /// window created now goes underneath it.
    fn hud_was_drawn_last_frame() -> bool {
        static START: OnceLock<Instant> = OnceLock::new();
        let now = START.get_or_init(Instant::now).elapsed().as_millis() as u64 + 1;
        let last = LAST_HUD_MS.swap(now, Ordering::Relaxed);
        last != 0 && now - last <= HUD_GAP_MS
    }

    pub fn install(symbols: &Symbols, log: &dyn Log) -> bool {
        let get = |n: &str| symbols.address(n);
        let (Some(view_origin), Some(view_rect), Some(camera), Some(kit)) = (
            get("ViewportOrigin"),
            get("ViewportRect"),
            get("CameraScrollX"),
            Kit::new_in(symbols, "HudCtx"),
        ) else {
            log.line(W2R_LOG_ERROR, "host", "map draw: symbols missing");
            return false;
        };
        let _ = GAME.set(Game { view_origin, view_rect, camera, kit });
        true
    }

    fn i32_at(addr: usize) -> i32 {
        // SAFETY: small values in the game's data section; callers run under `guarded`.
        unsafe { core::ptr::read_unaligned(addr as *const i32) }
    }

    fn i16_at(addr: usize) -> i32 {
        // SAFETY: as above.
        i32::from(unsafe { core::ptr::read_unaligned(addr as *const i16) })
    }

    unsafe extern "C" fn fill_rect(ctx: *mut c_void, x: f32, y: f32, w: f32, h: f32, colour: u32) {
        let Some(g) = GAME.get() else { return };
        if ctx.is_null() || !(w > 0.0 && h > 0.0) {
            return;
        }
        g.kit.fill_rect(Rect::new(x, y, w, h), colour);
    }

    unsafe extern "C" fn text(ctx: *mut c_void, x: f32, y: f32, w: f32, h: f32, text: W2rStr, colour: u32) {
        let Some(g) = GAME.get() else { return };
        if ctx.is_null() || text.ptr.is_null() || text.len == 0 || text.len > 64 {
            return;
        }
        let bytes = unsafe { core::slice::from_raw_parts(text.ptr.cast::<u8>(), text.len as usize) };
        let Ok(s) = core::str::from_utf8(bytes) else { return };
        // The caller gives screen coordinates; the layout space places labels from the window's
        // corner (seen in the game: numbers drawn one sidebar-width too far right).
        let (wx, wy) = (WINDOW_X.load(Ordering::Relaxed) as f32, WINDOW_Y.load(Ordering::Relaxed) as f32);
        g.kit.label_coloured_at(Rect::new(x - wx, y - wy, w, h), s, ALIGN_CENTRE, colour);
    }

    /// Opens the map-view window and calls the plugins. Called by the window hook just before
    /// the game starts its `game_hud` window.
    ///
    /// # Safety
    /// `ctx` is the context the game is about to begin `game_hud` in; `begin` is the game's own
    /// `BeginNamedWindow`.
    pub unsafe fn before_hud(ctx: *mut c_void, begin: BeginFn) {
        let Some(g) = GAME.get() else { return };
        // Before the presence check, so the HUD's frames are tracked whatever is installed.
        let hud_existed = hud_was_drawn_last_frame();
        if !crate::session::map_draw_present() || !hud_existed {
            return;
        }
        let result = guarded(|| {
            let v = [
                i16_at(g.view_rect),
                i16_at(g.view_rect + 2),
                i16_at(g.view_rect + 4),
                i16_at(g.view_rect + 6),
            ];
            if v[2] <= v[0] || v[3] <= v[1] {
                return;
            }
            let (w, h) = ((v[2] - v[0] + 1) as f32, (v[3] - v[1] + 1) as f32);
            if unsafe { begin(ctx, c"w2r_map_draw".as_ptr(), v[0] as f32, v[1] as f32, w, h, WINDOW_FLAGS) } == 0 {
                return;
            }
            WINDOW_X.store(v[0], Ordering::Relaxed);
            WINDOW_Y.store(v[1], Ordering::Relaxed);
            g.kit.space_begin();
            let draw = W2rMapDraw {
                struct_size: W2R_MAP_DRAW_V1_SIZE,
                ctx,
                origin_x: i32_at(g.view_origin) - i32_at(g.camera),
                origin_y: i32_at(g.view_origin + 4) - i32_at(g.camera + 4),
                view_x: v[0] as f32,
                view_y: v[1] as f32,
                view_w: w,
                view_h: h,
                fill_rect,
                text,
            };
            crate::session::draw_map(&draw);
            g.kit.space_end();
            g.kit.end_window();
        });
        if let Err(fault) = result {
            crate::session::host_fault("map draw", &fault);
        }
    }
}
