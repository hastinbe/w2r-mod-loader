//! Makes the HUD's classic/HD button remember its choice, as F5 does. x86 only.
//!
//! The game keeps the choice for the next launch in bit 0x10 of `GameSettingsFlags` (set: classic;
//! `FUN_006146c0` reads it when it creates the window). F5 (`FUN_004c8ab0`, scancode 0x3f) flips
//! the mode with `GraphicsToggle` and then updates that bit; the HUD button (`FUN_0052d930`) only
//! calls `GraphicsToggle`, so its choice is lost. Hooking `GraphicsToggle` itself keeps the bit in
//! step for both: when F5 runs its own update afterwards it writes the same value. The stock game
//! does this without the mod loader too.
//!
//! The game writes Warcraft2.ini only when the player exits straight from a match; after quitting
//! to the menu and then exiting it never calls `PersistSettings`, so a choice made in the match was
//! lost on that path (F5 too, with or without the loader). The hook therefore also calls
//! `PersistSettings`, the game's own ini writer, right after it updates the bit.
//!
//! Lines starting "graphics:" in the log show each toggle, each save and, once per UI window, any
//! change of the mode or the bit, so a lost choice can be traced.
//!
//! The bit is the ini key `using_classic`, shown on the options screen as Classic Graphics
//! (nativesettings.rs). A change there ([`choose`]) is switched to on the first frame of the next
//! match, with the options screen closed: the mode is toggled once, through the hooked
//! `GraphicsToggle` as F5 would, if it differs from the bit.

/// Bit of `GameSettingsFlags` that starts the next launch in classic graphics.
pub const CLASSIC_BIT: u32 = 0x10;
/// `GraphicsMode` after a toggle to classic. A toggle to HD leaves 3 (0 and 1 are the settled
/// classic and HD; 2 and 3 are the switches in progress).
pub const MODE_TO_CLASSIC: u32 = 2;

/// For the log.
pub fn mode_text(mode: u32) -> &'static str {
    match mode {
        0 => "classic",
        1 => "HD",
        2 => "switching to classic",
        3 => "switching to HD",
        _ => "unknown",
    }
}

/// For the log: what the next launch starts in.
pub fn next_launch_text(flags: u32) -> &'static str {
    if flags & CLASSIC_BIT != 0 {
        "classic"
    } else {
        "HD"
    }
}

/// `GameStateCurrent` during a match (single player, campaign, custom scenario, multiplayer).
pub const STATE_IN_MATCH: u16 = 3;

/// A settled `mode` (0 classic, 1 HD) that differs from the next-launch bit in `flags`: the
/// switch the options screen asked for has not happened yet.
pub fn switch_due(mode: u32, flags: u32) -> bool {
    mode <= 1 && (mode == 0) != (flags & CLASSIC_BIT != 0)
}

/// The flags word after a toggle that left the mode at `mode`.
pub fn flags_after(flags: u32, mode: u32) -> u32 {
    if mode == MODE_TO_CLASSIC {
        flags | CLASSIC_BIT
    } else {
        flags & !CLASSIC_BIT
    }
}

#[cfg(all(windows, target_arch = "x86"))]
pub use win::{choose, install, watch};

#[cfg(all(windows, target_arch = "x86"))]
mod win {
    use core::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};

    use w2r_abi::{W2R_LOG_DEBUG, W2R_LOG_ERROR, W2R_LOG_INFO, W2R_LOG_WARN};

    use super::{
        CLASSIC_BIT, STATE_IN_MATCH, flags_after, mode_text, next_launch_text, switch_due,
    };
    use crate::hooks::{self, Owner};
    use crate::host::Log;
    use crate::pe::Image;
    use crate::symbols::Symbols;

    const SYMBOL: &str = "GraphicsToggle";

    type ToggleFn = unsafe extern "C" fn() -> u8;
    type PersistFn = unsafe extern "C" fn();

    static ORIGINAL: AtomicUsize = AtomicUsize::new(0);
    static MODE: AtomicUsize = AtomicUsize::new(0);
    static FLAGS: AtomicUsize = AtomicUsize::new(0);
    static PERSIST: AtomicUsize = AtomicUsize::new(0);
    /// The mode and the bit last logged, `mode << 1 | bit`; `u32::MAX` before the first.
    static SEEN: AtomicU32 = AtomicU32::new(u32::MAX);
    /// `GraphicsToggle` itself (through the hook), once hooked; 0 before.
    static TARGET: AtomicUsize = AtomicUsize::new(0);
    /// `GameStateCurrent` (u16); 0 when missing.
    static STATE: AtomicUsize = AtomicUsize::new(0);
    /// The options screen changed the bit; the mode follows at the next match frame.
    static PENDING: AtomicBool = AtomicBool::new(false);

    fn line(level: u32, text: &str) {
        if let Some(s) = crate::entry::startup() {
            s.log.line(level, "host", text);
        }
    }

    fn seen(mode: u32, flags: u32) -> u32 {
        (mode << 1) | u32::from(flags & CLASSIC_BIT != 0)
    }

    unsafe extern "C" fn detour() -> u8 {
        let original: ToggleFn = unsafe { core::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
        let changed = unsafe { original() };
        if changed != 0 {
            // A toggle is the newest choice; nothing is left for the options screen's switch.
            PENDING.store(false, Ordering::Relaxed);
            let _ = catch_unwind(AssertUnwindSafe(|| {
                let (mode, flags) = (MODE.load(Ordering::Relaxed), FLAGS.load(Ordering::Relaxed));
                if mode != 0 && flags != 0 {
                    // SAFETY: the game's mode word and settings flags, on the game's thread.
                    let (mode, after) = unsafe {
                        let mode = (mode as *const u32).read_volatile();
                        let flags = flags as *mut u32;
                        let after = flags_after(flags.read_volatile(), mode);
                        flags.write_volatile(after);
                        (mode, after)
                    };
                    SEEN.store(seen(mode, after), Ordering::Relaxed);
                    line(
                        W2R_LOG_INFO,
                        &format!(
                            "graphics: toggled ({}); next launch {}",
                            mode_text(mode),
                            next_launch_text(after)
                        ),
                    );
                    let persist = PERSIST.load(Ordering::Relaxed);
                    if persist != 0 {
                        line(
                            W2R_LOG_INFO,
                            &format!(
                                "graphics: saving settings; next launch {}",
                                next_launch_text(after)
                            ),
                        );
                        // SAFETY: the game's ini writer (cdecl, no arguments), the same call the
                        // options screen makes, on the game's thread. It writes the flags word and
                        // never reads `GraphicsMode`, so a toggle still in progress (mode 2 or 3)
                        // cannot end up in the ini (Ghidra: FUN_0058a230 and what it calls have
                        // no reference to 0x8cb9c0).
                        let persist: PersistFn = unsafe { core::mem::transmute(persist) };
                        unsafe { persist() };
                    }
                }
            }));
        }
        changed
    }

    /// The options screen's Classic Graphics changed the bit (it has already written it): the
    /// mode follows on the first match frame with the options screen closed ([`switch_if_due`]).
    pub fn choose(classic: bool) {
        PENDING.store(true, Ordering::Relaxed);
        line(
            W2R_LOG_INFO,
            &format!(
                "graphics: options screen set {}; the mode follows at the next match if it differs",
                if classic { "classic" } else { "HD" }
            ),
        );
    }

    /// After the options screen changed the bit: on the first frame of a match, with the options
    /// screen closed, toggles the mode once if it differs, as F5 would. The toggle goes through
    /// the hook, which keeps the bit and writes Warcraft2.ini as for F5.
    fn switch_if_due(mode: usize, flags: usize) {
        if !PENDING.load(Ordering::Relaxed) {
            return;
        }
        let (state, target) = (
            STATE.load(Ordering::Relaxed),
            TARGET.load(Ordering::Acquire),
        );
        if state == 0 || target == 0 {
            return;
        }
        // SAFETY: the game's state word (u16), mode word and settings flags, on its thread.
        let (s, m, f) = unsafe {
            (
                (state as *const u16).read_unaligned(),
                (mode as *const u32).read_volatile(),
                (flags as *const u32).read_volatile(),
            )
        };
        if s != STATE_IN_MATCH || m > 1 || crate::options::settings_screen_open() {
            return;
        }
        PENDING.store(false, Ordering::Relaxed);
        if !switch_due(m, f) {
            return;
        }
        line(
            W2R_LOG_INFO,
            &format!(
                "graphics: switching to {} for the match (the options screen's choice)",
                next_launch_text(f)
            ),
        );
        // SAFETY: the hooked GraphicsToggle, cdecl `unsigned char()`, on the game's thread in a
        // match, where F5 and the HUD button call it.
        let toggle: ToggleFn = unsafe { core::mem::transmute(target) };
        unsafe { toggle() };
    }

    /// Once per UI window, from the frame hook: switches the mode when the options screen asked
    /// for it, and logs a change of the mode or the bit. The first call logs what this launch
    /// started in.
    pub fn watch() {
        let (mode, flags) = (MODE.load(Ordering::Relaxed), FLAGS.load(Ordering::Relaxed));
        if mode == 0 || flags == 0 {
            return;
        }
        switch_if_due(mode, flags);
        // SAFETY: the game's mode word and settings flags, on the game's thread.
        let (m, f) = unsafe {
            (
                (mode as *const u32).read_volatile(),
                (flags as *const u32).read_volatile(),
            )
        };
        let now = seen(m, f);
        let before = SEEN.swap(now, Ordering::Relaxed);
        if before != now {
            // The first call says what this launch started in; later changes are for debugging.
            let level = if before == u32::MAX {
                W2R_LOG_INFO
            } else {
                W2R_LOG_DEBUG
            };
            line(
                level,
                &format!(
                    "graphics: mode {}; next launch {}",
                    mode_text(m),
                    next_launch_text(f)
                ),
            );
        }
    }

    pub fn install(image: &Image, symbols: &Symbols, log: &dyn Log) -> bool {
        let (Some(target), Some(mode), Some(flags)) = (
            symbols.address(SYMBOL),
            symbols.address("GraphicsMode"),
            symbols.address("GameSettingsFlags"),
        ) else {
            log.line(
                W2R_LOG_ERROR,
                "host",
                "graphics button: a symbol is missing; the button stays as it is",
            );
            return false;
        };
        MODE.store(mode, Ordering::Release);
        FLAGS.store(flags, Ordering::Release);
        match symbols.address("GameStateCurrent") {
            Some(state) => STATE.store(state, Ordering::Release),
            None => log.line(
                W2R_LOG_WARN,
                "host",
                "graphics: GameStateCurrent is missing; the options screen's choice waits for the next launch",
            ),
        }
        match symbols.address("PersistSettings") {
            Some(persist) => PERSIST.store(persist, Ordering::Release),
            None => log.line(
                W2R_LOG_WARN,
                "host",
                "graphics button: PersistSettings is missing; only a direct exit saves the choice",
            ),
        }
        // SAFETY: `detour` matches GraphicsToggle's cdecl `unsigned char()` signature (build 2818:
        // no arguments, the changed flag in AL).
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
                TARGET.store(target, Ordering::Release);
                log.line(
                    W2R_LOG_INFO,
                    "host",
                    &format!("hooked {SYMBOL} @ {target:#010x} (the HUD button saves its choice)"),
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn log_texts() {
        assert_eq!(mode_text(0), "classic");
        assert_eq!(mode_text(3), "switching to HD");
        assert_eq!(mode_text(9), "unknown");
        assert_eq!(next_launch_text(0x17), "classic");
        assert_eq!(next_launch_text(0x07), "HD");
    }

    #[test]
    fn a_switch_is_due_only_when_a_settled_mode_differs_from_the_bit() {
        assert!(switch_due(1, 0x17), "HD, classic wanted");
        assert!(switch_due(0, 0x07), "classic, HD wanted");
        assert!(!switch_due(0, 0x17));
        assert!(!switch_due(1, 0x07));
        assert!(!switch_due(2, 0x07), "mid-switch");
        assert!(!switch_due(3, 0x17), "mid-switch");
    }

    #[test]
    fn a_toggle_to_classic_sets_the_bit_and_to_hd_clears_it() {
        assert_eq!(flags_after(0x07, MODE_TO_CLASSIC), 0x17);
        assert_eq!(flags_after(0x17, 3), 0x07);
        assert_eq!(flags_after(0x17, MODE_TO_CLASSIC), 0x17);
        assert_eq!(flags_after(0x07, 3), 0x07);
    }
}
