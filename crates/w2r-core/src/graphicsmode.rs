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

/// The flags word after a toggle that left the mode at `mode`.
pub fn flags_after(flags: u32, mode: u32) -> u32 {
    if mode == MODE_TO_CLASSIC {
        flags | CLASSIC_BIT
    } else {
        flags & !CLASSIC_BIT
    }
}

#[cfg(all(windows, target_arch = "x86"))]
pub use win::{install, watch};

#[cfg(all(windows, target_arch = "x86"))]
mod win {
    use core::ffi::c_void;
    use std::panic::{AssertUnwindSafe, catch_unwind};
    use std::sync::atomic::{AtomicU32, AtomicUsize, Ordering};

    use w2r_abi::{W2R_LOG_DEBUG, W2R_LOG_ERROR, W2R_LOG_INFO, W2R_LOG_WARN};

    use super::{CLASSIC_BIT, flags_after, mode_text, next_launch_text};
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

    /// Once per UI window, from the frame hook: logs a change of the mode or the bit. The first
    /// call logs what this launch started in.
    pub fn watch() {
        let (mode, flags) = (MODE.load(Ordering::Relaxed), FLAGS.load(Ordering::Relaxed));
        if mode == 0 || flags == 0 {
            return;
        }
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
    fn a_toggle_to_classic_sets_the_bit_and_to_hd_clears_it() {
        assert_eq!(flags_after(0x07, MODE_TO_CLASSIC), 0x17);
        assert_eq!(flags_after(0x17, 3), 0x07);
        assert_eq!(flags_after(0x17, MODE_TO_CLASSIC), 0x17);
        assert_eq!(flags_after(0x07, 3), 0x07);
    }
}
