//! Camera sharing with observers. A player's camera exists only on their own machine (the game
//! sends commands, not views), so an observer who wants to follow a player needs it told. In a
//! match with an observer, each player who runs the mod sends the centre of their view, a few times
//! a second and only when it moved, as one small message of our own subsystem (`'V'`) straight to
//! the observers' machines over the game's reliable channel. Nobody else is sent anything, and a
//! player who is not observed sends nothing at all.
//!
//! The observer keeps the latest centre per player; `replayplay.rs` eases the map view to it while
//! that player is the one being watched.
//!
//! The wire format, the send throttle and the per-player store are pure and tested here; the game
//! side is x86 only.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::peer;

/// The envelope's subsystem byte.
pub const SUBSYSTEM: u8 = b'V';
const PROTOCOL: u8 = 1;
/// The longest a coordinate can be: 128 tiles of 32 pixels.
const MAX_COORD: u16 = 4096;
/// The shortest time between two messages.
pub const MIN_GAP_MS: u64 = 100;
/// A view that has not moved is still repeated this often, so a newcomer or a lost message
/// does not leave an observer on stale data for long.
pub const REPEAT_MS: u64 = 2_000;
/// Movement smaller than this many pixels (each way) is not worth a message.
pub const MIN_MOVE_PX: u16 = 3;
/// A view this old is not followed any more (the player left or stopped sending).
pub const STALE_MS: u64 = 6_000;

/// A whole message: the envelope, the protocol, the centre of the view in map pixels.
pub fn encode(x: u16, y: u16) -> [u8; 10] {
    let (xb, yb) = (x.to_le_bytes(), y.to_le_bytes());
    [
        peer::TYPE,
        peer::MAGIC[0],
        peer::MAGIC[1],
        peer::MAGIC[2],
        SUBSYSTEM,
        PROTOCOL,
        xb[0],
        xb[1],
        yb[0],
        yb[1],
    ]
}

/// The centre in a message body (everything after the envelope). Longer bodies are accepted so a
/// later version can append fields; another protocol or a point off the largest map is refused.
pub fn decode(body: &[u8]) -> Option<(u16, u16)> {
    match body {
        [PROTOCOL, x0, x1, y0, y1, ..] => {
            let (x, y) = (u16::from_le_bytes([*x0, *x1]), u16::from_le_bytes([*y0, *y1]));
            (x <= MAX_COORD && y <= MAX_COORD).then_some((x, y))
        }
        _ => None,
    }
}

/// When to send: at most every [`MIN_GAP_MS`], and then only if the view moved or [`REPEAT_MS`]
/// passed.
#[derive(Default)]
pub struct Throttle {
    last_ms: u64,
    last: Option<(u16, u16)>,
}

impl Throttle {
    /// Whether the view at `pos` is to be sent now; if so it is taken as sent.
    pub fn due(&mut self, now_ms: u64, pos: (u16, u16)) -> bool {
        let send = match self.last {
            None => true,
            Some((lx, ly)) => {
                let waited = now_ms.saturating_sub(self.last_ms);
                let moved = lx.abs_diff(pos.0) >= MIN_MOVE_PX || ly.abs_diff(pos.1) >= MIN_MOVE_PX;
                waited >= MIN_GAP_MS && (moved || waited >= REPEAT_MS)
            }
        };
        if send {
            self.last_ms = now_ms;
            self.last = Some(pos);
        }
        send
    }
}

/// The latest view centre heard from each player.
#[derive(Default)]
pub struct Cameras {
    seen: [Option<((u16, u16), u64)>; 8],
}

impl Cameras {
    pub fn set(&mut self, player: u8, pos: (u16, u16), now_ms: u64) {
        if let Some(slot) = self.seen.get_mut(usize::from(player)) {
            *slot = Some((pos, now_ms));
        }
    }

    /// Where `player` was last looking, if it was heard from recently enough to follow.
    pub fn get(&self, player: u8, now_ms: u64) -> Option<(u16, u16)> {
        let (pos, at) = (*self.seen.get(usize::from(player))?)?;
        (now_ms.saturating_sub(at) <= STALE_MS).then_some(pos)
    }
}

static CAMERAS: Mutex<Option<Cameras>> = Mutex::new(None);
/// Camera sharing is on (the host setting; on by default).
static ENABLED: AtomicBool = AtomicBool::new(true);

/// Where the watched player's view was last seen (the observer's side).
pub fn latest(player: u8, now_ms: u64) -> Option<(u16, u16)> {
    CAMERAS.lock().ok()?.as_ref()?.get(player, now_ms)
}

#[cfg_attr(not(target_arch = "x86"), allow(dead_code))]
fn store(player: u8, pos: (u16, u16), now_ms: u64) {
    if let Ok(mut c) = CAMERAS.lock() {
        c.get_or_insert_with(Cameras::default).set(player, pos, now_ms);
    }
}

#[cfg_attr(not(target_arch = "x86"), allow(dead_code))]
fn forget() {
    if let Ok(mut c) = CAMERAS.lock() {
        *c = None;
    }
}

pub fn set_enabled(on: bool) {
    ENABLED.store(on, Ordering::Release);
}

#[cfg(target_arch = "x86")]
pub use live::{
    broadcast, deliver, in_networked_match, install, is_out, local_player, match_peer, on_match_start,
    player_of_net, poll, send_in_match,
};

/// Without the game (tests): never in a match, so plugin messages stay lobby only.
#[cfg(not(target_arch = "x86"))]
pub fn in_networked_match() -> bool {
    false
}

#[cfg(not(target_arch = "x86"))]
pub fn match_peer(_net: u32) -> bool {
    false
}

#[cfg(not(target_arch = "x86"))]
pub fn send_in_match(_only: Option<u32>, _msg: &[u8]) -> usize {
    0
}

#[cfg(target_arch = "x86")]
mod live {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Mutex, OnceLock};

    use w2r_abi::{W2R_LOG_ERROR, W2R_LOG_INFO};

    use super::*;
    use crate::chat::now_ms;
    use crate::guard::guarded;
    use crate::host::Log;
    use crate::symbols::Symbols;

    type SendFn = unsafe extern "C" fn(u32, *const u8, u32);

    /// The game state while a match is being played.
    const STATE_IN_MATCH: u16 = 3;
    /// `TurnSlotFlags` bit: in the match.
    const FLAG_IN_MATCH: u32 = 0x1_0000;
    const NO_SEAT: u32 = 8;
    /// The lobby slot table: stride, and where the team byte sits.
    const SLOT_STRIDE: usize = 0x26;
    const SLOT_TEAM: usize = 0x0a;
    const OWNER_HUMAN: u8 = 0;

    struct Game {
        state: usize,
        networked: usize,
        local_player: usize,
        player_by_net: usize,
        lobby_by_net: usize,
        turn_flags: usize,
        left_flags: usize,
        owner: usize,
        result: usize,
        lobby_slots: usize,
        camera: usize,
        view_w: usize,
        view_h: usize,
        send: SendFn,
    }

    // SAFETY: plain game function and data addresses, fixed for the session.
    unsafe impl Send for Game {}
    unsafe impl Sync for Game {}

    #[derive(Default)]
    struct State {
        throttle: Throttle,
    }

    static SENT_LOGGED: AtomicBool = AtomicBool::new(false);
    static GOT_LOGGED: AtomicBool = AtomicBool::new(false);
    static GAME: OnceLock<Game> = OnceLock::new();
    static STATE: Mutex<Option<State>> = Mutex::new(None);

    fn log(level: u32, text: &str) {
        crate::session::StartupLog.line(level, "camera", text);
    }

    pub fn install(symbols: &Symbols, log: &dyn Log) -> bool {
        let get = |n: &str| symbols.address(n);
        let (
            Some(state),
            Some(networked),
            Some(local_player),
            Some(player_by_net),
            Some(lobby_by_net),
            Some(turn_flags),
            Some(left_flags),
            Some(owner),
            Some(result),
            Some(lobby_slots),
            Some(camera),
            Some(view_w),
            Some(view_h),
            Some(send),
        ) = (
            get("GameStateCurrent"),
            get("NetworkedMatch"),
            get("LocalPlayer"),
            get("NetSlotPlayer"),
            get("LobbySlotByNetSlot"),
            get("TurnSlotFlags"),
            get("PeerLeftFlags"),
            get("PlayerOwner"),
            get("PlayerMatchResult"),
            get("MpLobbySlots"),
            get("CameraScrollX"),
            get("ViewportWidth"),
            get("ViewportHeight"),
            get("SendToNetSlot"),
        )
        else {
            log.line(W2R_LOG_ERROR, "host", "camera sharing: symbols missing");
            return false;
        };
        // SAFETY: `SendToNetSlot(netSlot, message, length)` (Ghidra, build 2818), as pings use it.
        let send = unsafe { core::mem::transmute::<usize, SendFn>(send) };
        let _ = GAME.set(Game {
            state,
            networked,
            local_player,
            player_by_net,
            lobby_by_net,
            turn_flags,
            left_flags,
            owner,
            result,
            lobby_slots,
            camera,
            view_w,
            view_h,
            send,
        });
        log.line(W2R_LOG_INFO, "host", "camera sharing ready (players tell observers where they look)");
        true
    }

    impl Game {
        fn u8_at(&self, base: usize, index: usize) -> u8 {
            // SAFETY: small per-player tables in the game's data section; callers use `guarded`.
            unsafe { core::ptr::read_unaligned((base + index) as *const u8) }
        }

        fn u32_at(&self, base: usize, index: usize) -> u32 {
            // SAFETY: per-slot tables of at least 8 entries in the game's data section.
            unsafe { core::ptr::read_unaligned((base + index * 4) as *const u32) }
        }

        fn in_match(&self) -> bool {
            if crate::replayplay::is_active() {
                return false;
            }
            // SAFETY: the state is a u16 in the game's data section.
            unsafe { core::ptr::read_unaligned(self.state as *const u16) == STATE_IN_MATCH }
        }

        fn local(&self) -> u8 {
            // An observer's LocalPlayer is swapped to the player being watched; the real seat is kept by the viewer.
            let real = crate::viewer::real();
            if real < 8 { real } else { self.u8_at(self.local_player, 0) }
        }

        /// The centre of the local view in map pixels, clamped to what a message can carry.
        fn centre(&self) -> (u16, u16) {
            // SAFETY: two i32 scroll values and two u16 view sizes in the game's data section.
            let (sx, sy, w, h) = unsafe {
                (
                    core::ptr::read_unaligned(self.camera as *const i32),
                    core::ptr::read_unaligned((self.camera + 4) as *const i32),
                    i32::from(core::ptr::read_unaligned(self.view_w as *const u16)),
                    i32::from(core::ptr::read_unaligned(self.view_h as *const u16)),
                )
            };
            let clamp = |v: i32| v.clamp(0, i32::from(MAX_COORD)) as u16;
            (clamp(sx + w / 2), clamp(sy + h / 2))
        }

        /// The net slots to send to: connected people in the match who run the mod and are on the
        /// observer team.
        fn observer_slots(&self, observers: u8, roster: u8) -> Vec<u32> {
            let me = self.local();
            (0..8usize)
                .filter(|&n| {
                    if roster & (1 << n) == 0 || self.u32_at(self.lobby_by_net, n) & 0xff >= NO_SEAT {
                        return false;
                    }
                    if self.u32_at(self.turn_flags, n) & FLAG_IN_MATCH == 0
                        || self.u32_at(self.left_flags, n) != 0
                    {
                        return false;
                    }
                    let player = (self.u32_at(self.player_by_net, n) & 0xff) as u8;
                    player < 8 && player != me && observers & (1 << player) != 0
                })
                .map(|n| n as u32)
                .collect()
        }
    }

    /// The local player's seat (the real one while an observer watches someone).
    pub fn local_player() -> Option<u8> {
        let g = GAME.get()?;
        let p = guarded(|| g.local()).ok()?;
        (p < 8).then_some(p)
    }

    pub fn in_networked_match() -> bool {
        let Some(g) = GAME.get() else { return false };
        guarded(|| g.in_match() && g.u8_at(g.networked, 0) != 0).unwrap_or(false)
    }

    /// The match is over for `player` (a result was written for them).
    pub fn is_out(player: u8) -> bool {
        let Some(g) = GAME.get() else { return false };
        guarded(|| g.u8_at(g.result, usize::from(player)) != 0).unwrap_or(false)
    }

    /// The player behind a net slot.
    pub fn player_of_net(net: u32) -> Option<u8> {
        let g = GAME.get()?;
        if net >= 8 {
            return None;
        }
        let p = guarded(|| (g.u32_at(g.player_by_net, net as usize) & 0xff) as u8).ok()?;
        (p < 8).then_some(p)
    }

    /// Net slot `n` is another machine in the match that runs the mod (announced in the lobby just
    /// left, seated, in the match, not gone).
    pub fn match_peer(n: u32) -> bool {
        let Some(g) = GAME.get() else { return false };
        let n = n as usize;
        n < 8
            && guarded(|| {
                crate::ping::roster() & (1 << n) != 0
                    && g.u32_at(g.lobby_by_net, n) & 0xff < NO_SEAT
                    && g.u32_at(g.turn_flags, n) & FLAG_IN_MATCH != 0
                    && g.u32_at(g.left_flags, n) == 0
            })
            .unwrap_or(false)
    }

    /// Sends `msg` to the machines `match_peer` accepts (all of them, or only net slot `only`);
    /// how many it went to. Plugin messages during a match.
    pub fn send_in_match(only: Option<u32>, msg: &[u8]) -> usize {
        let Some(g) = GAME.get() else { return 0 };
        let mut sent = 0;
        for n in 0..8u32 {
            if only.is_some_and(|o| o != n) || !match_peer(n) {
                continue;
            }
            // SAFETY: the game's own send, a reliable message to one connected peer.
            if guarded(|| unsafe { (g.send)(n, msg.as_ptr(), msg.len() as u32) }).is_ok() {
                sent += 1;
            }
        }
        sent
    }

    /// Sends `msg` to every other connected machine in the match that runs the mod.
    pub fn broadcast(msg: &[u8]) {
        let Some(g) = GAME.get() else { return };
        let _ = guarded(|| {
            for n in 0..8usize {
                if crate::ping::roster() & (1 << n) == 0
                    || g.u32_at(g.lobby_by_net, n) & 0xff >= NO_SEAT
                    || g.u32_at(g.turn_flags, n) & FLAG_IN_MATCH == 0
                    || g.u32_at(g.left_flags, n) != 0
                {
                    continue;
                }
                // SAFETY: the game's own send, a reliable message to one connected peer.
                unsafe { (g.send)(n as u32, msg.as_ptr(), msg.len() as u32) };
            }
        });
    }

    fn with_state<R>(f: impl FnOnce(&mut State) -> R) -> Option<R> {
        let mut guard = STATE.lock().ok()?;
        Some(f(guard.get_or_insert_with(State::default)))
    }

    /// A new match: nothing is known about anybody's view, and the lobby tells who observes.
    pub fn on_match_start() {
        let Some(g) = GAME.get() else { return };
        SENT_LOGGED.store(false, Ordering::Release);
        GOT_LOGGED.store(false, Ordering::Release);
        forget();
        let observers = guarded(|| {
            (0..8usize)
                .filter(|&i| {
                    g.u8_at(g.lobby_slots, i * SLOT_STRIDE + SLOT_TEAM) == crate::observer::OBSERVER_TEAM
                        && g.u8_at(g.owner, i) == OWNER_HUMAN
                })
                .fold(0u8, |m, i| m | 1 << i)
        })
        .unwrap_or(0);
        with_state(|s| *s = State::default());
        crate::observer::set_match_observers(observers);
        if observers != 0 {
            log(W2R_LOG_INFO, &format!("match with observers {observers:#010b}"));
        }
    }

    /// Once per game frame: a player in a match with observers tells them where they look.
    pub fn poll() {
        let Some(g) = GAME.get() else { return };
        if !ENABLED.load(Ordering::Acquire) {
            return;
        }
        if let Err(fault) = guarded(|| poll_inner(g)) {
            crate::session::host_fault("camera sharing", &fault);
        }
    }

    fn poll_inner(g: &Game) {
        if !g.in_match() || g.u8_at(g.networked, 0) == 0 {
            return;
        }
        let me = g.local();
        let observers = crate::observer::match_observers();
        // Nothing to say without observers, and observers do not send their own view.
        if observers == 0 || me >= 8 || observers & (1 << me) != 0 {
            return;
        }
        let pos = g.centre();
        let now = now_ms();
        if !with_state(|s| s.throttle.due(now, pos)).unwrap_or(false) {
            return;
        }
        let msg = encode(pos.0, pos.1);
        let slots = g.observer_slots(observers, crate::ping::roster());
        if !SENT_LOGGED.swap(true, Ordering::AcqRel) {
            log(
                W2R_LOG_INFO,
                &format!("sending view {pos:?} to net slots {slots:?} (roster {:#04x}, observers {observers:#04x})", crate::ping::roster()),
            );
        }
        for slot in slots {
            // SAFETY: the game's own send, a reliable message to one connected peer.
            unsafe { (g.send)(slot, msg.as_ptr(), msg.len() as u32) };
        }
    }

    /// A view centre arrived from net slot `sender` (called from the packet hook; `body` is what
    /// follows the envelope). Only an observer keeps it, and only from somebody who plays.
    pub fn deliver(sender: u32, body: &[u8]) {
        let Some(g) = GAME.get() else { return };
        if !ENABLED.load(Ordering::Acquire) || !g.in_match() || sender >= 8 {
            return;
        }
        let Some(pos) = decode(body) else { return };
        if !GOT_LOGGED.swap(true, Ordering::AcqRel) {
            log(W2R_LOG_INFO, &format!("first view from net slot {sender}: {pos:?}"));
        }
        let player = (g.u32_at(g.player_by_net, sender as usize) & 0xff) as u8;
        let me = g.local();
        if player >= 8 || me >= 8 {
            return;
        }
        let observers = crate::observer::match_observers();
        if observers & (1 << me) == 0 || observers & (1 << player) != 0 {
            return;
        }
        store(player, pos, now_ms());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_message_round_trips_after_its_envelope() {
        let msg = encode(1234, 4096);
        assert_eq!(&msg[..5], &[peer::TYPE, b'W', b'2', b'R', SUBSYSTEM]);
        assert_eq!(decode(&msg[5..]), Some((1234, 4096)));
    }

    #[test]
    fn points_off_the_largest_map_and_other_protocols_are_refused() {
        assert_eq!(decode(&encode(4097, 0)[5..]), None);
        assert_eq!(decode(&encode(0, 9000)[5..]), None);
        assert_eq!(decode(&[2, 0, 0, 0, 0]), None);
        assert_eq!(decode(&[1, 0, 0]), None);
        // A later version may append fields.
        assert_eq!(decode(&[1, 10, 0, 20, 0, 99, 99]), Some((10, 20)));
    }

    #[test]
    fn sending_waits_for_the_gap_and_for_movement_or_the_repeat() {
        let mut t = Throttle::default();
        assert!(t.due(1_000, (100, 100)), "the first view is always sent");
        assert!(!t.due(1_050, (500, 500)), "too soon");
        assert!(!t.due(1_300, (102, 100)), "moved too little");
        assert!(t.due(1_300, (120, 100)), "moved enough after the gap");
        assert!(!t.due(2_000, (120, 100)), "not moved, not due to repeat");
        assert!(t.due(3_400, (120, 100)), "repeated after a while");
    }

    #[test]
    fn a_view_goes_stale() {
        let mut c = Cameras::default();
        assert_eq!(c.get(2, 0), None);
        c.set(2, (300, 400), 1_000);
        assert_eq!(c.get(2, 1_000 + STALE_MS), Some((300, 400)));
        assert_eq!(c.get(2, 1_001 + STALE_MS), None);
        assert_eq!(c.get(3, 1_000), None);
        c.set(9, (1, 1), 0); // no such player: ignored
    }
}
