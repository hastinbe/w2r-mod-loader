//! The per-plugin host table (`W2rHost`) and the host functions it serves: `log`, events,
//! `symbol_lookup`/`symbol_read` (`game.read`), `hook_install`/`hook_remove` (`hooks.raw`),
//! `chat_send` (`chat.write`) and `peer_send` (`net.send`), plus the implicit settings and data-file
//! functions (backed by `store.rs`).
//!
//! Rule for every callback here: validate first, borrow a `RefCell` last, and never hold a
//! borrow across anything that can fault. A fault abandons frames without running destructors,
//! so a live `RefMut` would leave the cell borrowed for the rest of the session.

use core::ffi::{c_char, c_void};
use std::cell::{Cell, RefCell};
use std::collections::BTreeSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;
use std::rc::Rc;

use w2r_abi::*;

use crate::chat::{self, Chat, NoChat};
use crate::guard::guarded;
use crate::hooks::{self, HookError, Owner};
use crate::pe::Image;
use crate::peer::{
    self, NoPeers, NoTransport, Nowhere, Presence, Roster, SlidingWindow, Target, TargetError,
    Transport,
};
use crate::settings::{self, DeclaredSetting, SettingDef};
use crate::store::{DataDir, DataError, SetError, Settings, Val};
use crate::symbols::Symbols;
use crate::target::Kind;

/// Capability names as they appear in `plugin.json` and `state.json` (docs/layout.md).
pub mod caps {
    pub const GAME_READ: &str = "game.read";
    pub const CHAT_READ: &str = "chat.read";
    pub const CHAT_WRITE: &str = "chat.write";
    pub const NET_SEND: &str = "net.send";
    pub const HOOKS_RAW: &str = "hooks.raw";
}

/// Where host and plugin log lines go. Single-threaded: everything runs on the game's main
/// thread.
pub trait Log {
    fn line(&self, level: u32, plugin: &str, msg: &str);
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum State {
    Active,
    /// Never called again this session. `reason` is what was logged.
    Disabled {
        reason: String,
    },
}

/// The game module as the host found it. `None` outside the game (tests).
pub struct Game {
    pub image: Image,
    pub symbols: Symbols,
}

/// The game-facing services behind traits, so every rule is testable with fakes.
pub struct Services {
    pub chat: Rc<dyn Chat>,
    pub transport: Rc<dyn Transport>,
    pub roster: Rc<dyn Roster>,
    pub presence: Rc<dyn Presence>,
    /// The host root (docs/layout.md), for the settings and data stores. `None` outside the
    /// game's startup path (tests); those host functions then report `W2R_E_STATE`.
    pub root: Option<PathBuf>,
}

impl Services {
    /// No game: nothing available.
    pub fn none() -> Services {
        Services {
            chat: Rc::new(NoChat),
            transport: Rc::new(NoTransport),
            roster: Rc::new(NoPeers),
            presence: Rc::new(Nowhere),
            root: None,
        }
    }
}

/// What every slot shares.
pub struct Shared {
    pub log: Rc<dyn Log>,
    pub game: Option<Game>,
    pub services: Services,
    /// `peer_send` across all plugins: 128/s.
    pub peer_rate: RefCell<SlidingWindow>,
}

impl Shared {
    pub fn new(log: Rc<dyn Log>, game: Option<Game>, services: Services) -> Shared {
        Shared {
            log,
            game,
            services,
            peer_rate: RefCell::new(SlidingWindow::new(peer::OVERALL_PER_SECOND, 1000)),
        }
    }
}

const EMPTY_TABLE: W2rPlugin = W2rPlugin {
    struct_size: 0,
    on_load: None,
    on_event: None,
    on_ui: None,
    on_unload: None,
};

/// First field of every [`Slot`]: a `token` that does not point at one (the classic mistake
/// is passing `host` instead of `host->token`, whose first field is `struct_size`) is refused
/// instead of being read as a slot.
const SLOT_MAGIC: u32 = 0x5732_5250; // "W2RP"

/// docs/abi.md: `log` is throttled to 20 lines/s per plugin with a burst of 40, which is 40 in
/// any 2 s window; excess lines are dropped and one summary line per second says how many.
const LOG_BURST: usize = 40;
const LOG_WINDOW_MS: u64 = 2000;
const LOG_NOTICE_EVERY_MS: u64 = 1000;

/// One loaded (or refused) plugin. Boxed so its address, which is the plugin's `token`, never
/// moves.
pub struct Slot {
    magic: u32,
    /// Position in the registry; also the hook owner id.
    pub index: usize,
    pub id: String,
    /// Empty until `w2r_plugin_main` returned a valid table.
    pub table: Cell<W2rPlugin>,
    caps: BTreeSet<String>,
    state: RefCell<State>,
    subs: RefCell<BTreeSet<W2rEventId>>,
    /// Functions already refused for a missing capability (logged once each).
    denied: RefCell<BTreeSet<&'static str>>,
    chat_rate: RefCell<SlidingWindow>,
    peer_rate: RefCell<SlidingWindow>,
    log_rate: RefCell<SlidingWindow>,
    /// Plugin log lines dropped since the last summary line.
    log_dropped: Cell<u32>,
    log_notice_ms: Cell<u64>,
    /// Opened on first use: `config\<id>.json` and `plugins\<id>\data\`.
    settings: RefCell<Option<Settings>>,
    data: RefCell<Option<DataDir>>,
    /// The plugin's peer-message tag (docs/abi.md).
    pub tag: [u8; 4],
    /// Consecutive callbacks over [`Slot::SLOW_MS`].
    slow: Cell<u32>,
    /// The callback from `create_ui_register`, if any.
    create_ui: Cell<Option<CreateUiFn>>,
    /// True only while the plugin's `on_load` runs: `settings_declare` is refused elsewhere.
    loading: Cell<bool>,
    /// What the plugin declared with `settings_declare`, in order. Its `category` is empty
    /// where the plugin named none; the session fills the plugin's display name in.
    declared: RefCell<Vec<SettingDef>>,
    shared: Rc<Shared>,
    host: W2rHost,
}

/// A plugin's create-screen callback.
pub type CreateUiFn = unsafe extern "C" fn(ui: *const W2rUi);

impl Slot {
    pub fn new(index: usize, id: &str, caps: &[&str], shared: Rc<Shared>) -> Box<Slot> {
        let mut slot = Box::new(Slot {
            magic: SLOT_MAGIC,
            index,
            id: id.to_owned(),
            table: Cell::new(EMPTY_TABLE),
            caps: caps.iter().map(|c| (*c).to_owned()).collect(),
            state: RefCell::new(State::Active),
            subs: RefCell::new(BTreeSet::new()),
            denied: RefCell::new(BTreeSet::new()),
            chat_rate: RefCell::new(SlidingWindow::new(chat::MAX_PER_WINDOW, chat::WINDOW_MS)),
            peer_rate: RefCell::new(SlidingWindow::new(peer::PER_PLUGIN_PER_SECOND, 1000)),
            log_rate: RefCell::new(SlidingWindow::new(LOG_BURST, LOG_WINDOW_MS)),
            log_dropped: Cell::new(0),
            log_notice_ms: Cell::new(0),
            settings: RefCell::new(None),
            data: RefCell::new(None),
            tag: peer::peer_tag(id),
            slow: Cell::new(0),
            create_ui: Cell::new(None),
            loading: Cell::new(false),
            declared: RefCell::new(Vec::new()),
            shared,
            host: host_table(core::ptr::null_mut()),
        });
        let token: *mut Slot = &mut *slot;
        slot.host.token = token.cast();
        slot
    }

    /// docs/abi.md: callbacks over this are logged...
    pub const SLOW_MS: u128 = 50;
    /// ...and after this many in a row the plugin list shows a warning.
    pub const SLOW_STREAK: u32 = 10;

    /// Records how long a callback took.
    pub fn note_duration(&self, ms: u128, what: &str) {
        if ms > Self::SLOW_MS {
            self.slow.set(self.slow.get().saturating_add(1));
            self.log(W2R_LOG_WARN, &format!("{what} took {ms} ms"));
        } else {
            self.slow.set(0);
        }
    }

    pub fn is_slow(&self) -> bool {
        self.slow.get() >= Self::SLOW_STREAK
    }

    /// The table handed to the plugin. Valid for as long as the slot lives.
    pub fn host(&self) -> *const W2rHost {
        &self.host
    }

    pub fn state(&self) -> State {
        self.state.borrow().clone()
    }

    /// The create-screen callback, only while the plugin is active.
    pub fn create_ui(&self) -> Option<CreateUiFn> {
        if self.is_active() {
            self.create_ui.get()
        } else {
            None
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(&*self.state.borrow(), State::Active)
    }

    /// Marks the span of `on_load`, the only time `settings_declare` is accepted.
    pub fn set_loading(&self, loading: bool) {
        self.loading.set(loading);
    }

    /// The settings the plugin declared (docs/abi.md, "Declared settings").
    pub fn declared(&self) -> Vec<SettingDef> {
        self.declared.borrow().clone()
    }

    /// One of the plugin's stored values, for the options screen. `None` when the key was never
    /// stored, or there is no settings folder.
    pub fn stored_setting(&self, key: &str) -> Option<Val> {
        cb::with_settings(self, |s| Ok(s.get(key).cloned()))
            .ok()
            .flatten()
    }

    /// Stores one value for the plugin, as `settings_set_*` would, when the player applies the
    /// options screen.
    pub fn store_setting(&self, key: &str, value: Val) -> Result<(), String> {
        cb::with_settings(self, |s| s.set(key, value).map_err(cb::set_status))
            .map_err(|status| format!("cannot save setting {key} ({status})"))
    }

    pub fn subscribed(&self, id: W2rEventId) -> bool {
        self.subs.borrow().contains(&id)
    }

    pub fn has_cap(&self, cap: &str) -> bool {
        self.caps.contains(cap)
    }

    pub fn owner(&self) -> Owner {
        Owner::Plugin(self.index)
    }

    /// The host's own lines about this plugin; never throttled.
    pub fn log(&self, level: u32, msg: &str) {
        self.shared.log.line(level, &self.id, msg);
    }

    /// A line the plugin asked for, under the throttle.
    fn plugin_log(&self, level: u32, msg: &str) {
        let now = chat::now_ms();
        if self.log_rate.borrow_mut().allow(now) {
            self.log(level, msg);
            return;
        }
        self.log_dropped
            .set(self.log_dropped.get().saturating_add(1));
        if now.saturating_sub(self.log_notice_ms.get()) >= LOG_NOTICE_EVERY_MS {
            self.log_notice_ms.set(now);
            let dropped = self.log_dropped.replace(0);
            self.log(
                W2R_LOG_WARN,
                &format!("{dropped} log lines dropped (over 20 lines/s)"),
            );
        }
    }

    /// Logs the refusal once per function and returns `W2R_E_DENIED`.
    fn deny(&self, function: &'static str, cap: &str) -> W2rStatus {
        if self.denied.borrow_mut().insert(function) {
            self.log(
                W2R_LOG_WARN,
                &format!("{function}: capability {cap} not approved"),
            );
        }
        W2R_E_DENIED
    }

    /// Stops every future call into the plugin, drops its subscriptions and removes its hooks.
    pub fn disable(&self, reason: String) {
        self.log(W2R_LOG_ERROR, &format!("disabled: {reason}"));
        *self.state.borrow_mut() = State::Disabled { reason };
        self.subs.borrow_mut().clear();
        self.create_ui.set(None);
        let removed = hooks::global().remove_all(self.owner());
        if !removed.is_empty() {
            self.log(
                W2R_LOG_INFO,
                &format!("removed hooks: {}", removed.join(", ")),
            );
        }
    }
}

fn host_table(token: *mut c_void) -> W2rHost {
    W2rHost {
        struct_size: W2R_HOST_SETTINGS_DECLARE_SIZE,
        abi_version: W2R_ABI_VERSION,
        token,
        log: cb::log,
        settings_get_i64: cb::settings_get_i64,
        settings_set_i64: cb::settings_set_i64,
        settings_get_str: cb::settings_get_str,
        settings_set_str: cb::settings_set_str,
        data_open: cb::data_open,
        data_read: cb::data_read,
        data_write: cb::data_write,
        data_close: cb::data_close,
        subscribe: cb::subscribe,
        unsubscribe: cb::unsubscribe,
        symbol_lookup: cb::symbol_lookup,
        symbol_read: cb::symbol_read,
        chat_send: cb::chat_send,
        peer_send: cb::peer_send,
        hook_install: cb::hook_install,
        hook_remove: cb::hook_remove,
        symbol_address: cb::symbol_address,
        create_ui_register: cb::create_ui_register,
        create_map_count: cb::create_map_count,
        create_map_select: cb::create_map_select,
        create_map_folder: cb::create_map_folder,
        settings_declare: cb::settings_declare,
    }
}

mod cb {
    use super::*;

    /// The checks every host function starts with (docs/abi.md): a token that is a slot, the
    /// game's main thread, and a plugin that is still active. The thread check comes before
    /// any `RefCell` is touched, since those are not thread-safe.
    fn enter<'a>(token: *mut c_void) -> Result<&'a Slot, W2rStatus> {
        let slot = unsafe { token.cast::<Slot>().as_ref() }.ok_or(W2R_E_INVALID)?;
        if slot.magic != SLOT_MAGIC {
            return Err(W2R_E_INVALID);
        }
        if !on_main_thread() {
            return Err(W2R_E_STATE);
        }
        if !slot.is_active() {
            return Err(W2R_E_STATE);
        }
        Ok(slot)
    }

    /// True until the game's main thread is known (tests, and nothing is loaded before then
    /// in the game), then only on it.
    fn on_main_thread() -> bool {
        #[cfg(windows)]
        {
            let main = crate::frame::main_thread_id();
            main == 0
                || main == unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
        }
        #[cfg(not(windows))]
        {
            true
        }
    }

    /// A panic in host code must not unwind into the plugin (that would abort the game).
    fn run(f: impl FnOnce() -> W2rStatus) -> W2rStatus {
        catch_unwind(AssertUnwindSafe(f)).unwrap_or(W2R_E_INVALID)
    }

    /// Borrows the plugin's string. A bad pointer faults here, inside the plugin's guard, and
    /// disables the plugin.
    unsafe fn text<'a>(s: W2rStr) -> Result<&'a str, W2rStatus> {
        if s.len == 0 {
            return Ok("");
        }
        if s.ptr.is_null() {
            return Err(W2R_E_INVALID);
        }
        let bytes = unsafe { core::slice::from_raw_parts(s.ptr.cast::<u8>(), s.len as usize) };
        core::str::from_utf8(bytes).map_err(|_| W2R_E_INVALID)
    }

    fn known_event(id: W2rEventId) -> bool {
        (W2R_EVT_LOBBY_ENTER..=W2R_EVT_SHUTDOWN).contains(&id)
    }

    /// docs/abi.md: events that need a capability to subscribe.
    fn event_cap(id: W2rEventId) -> Option<&'static str> {
        match id {
            W2R_EVT_CHAT_RECEIVED => Some(caps::CHAT_READ),
            W2R_EVT_PEER_MESSAGE => Some(caps::NET_SEND),
            _ => None,
        }
    }

    fn require(slot: &Slot, function: &'static str, cap: &str) -> Result<(), W2rStatus> {
        if slot.has_cap(cap) {
            Ok(())
        } else {
            Err(slot.deny(function, cap))
        }
    }

    fn game(slot: &Slot) -> Result<&Game, W2rStatus> {
        slot.shared.game.as_ref().ok_or(W2R_E_STATE)
    }

    pub unsafe extern "C" fn log(token: *mut c_void, level: u32, msg: W2rStr) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if level > W2R_LOG_ERROR {
                return W2R_E_INVALID;
            }
            if msg.len > W2R_MAX_LOG_BYTES {
                return W2R_E_LIMIT;
            }
            let text = match unsafe { text(msg) } {
                Ok(t) => t,
                Err(e) => return e,
            };
            slot.plugin_log(level, text);
            W2R_OK
        })
    }

    pub unsafe extern "C" fn subscribe(token: *mut c_void, id: W2rEventId) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if !known_event(id) {
                return W2R_E_NOT_FOUND;
            }
            if let Some(cap) = event_cap(id) {
                if let Err(e) = require(slot, "subscribe", cap) {
                    return e;
                }
            }
            slot.subs.borrow_mut().insert(id);
            W2R_OK
        })
    }

    pub unsafe extern "C" fn unsubscribe(token: *mut c_void, id: W2rEventId) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if !known_event(id) {
                return W2R_E_NOT_FOUND;
            }
            slot.subs.borrow_mut().remove(&id);
            W2R_OK
        })
    }

    pub unsafe extern "C" fn symbol_lookup(
        token: *mut c_void,
        name: W2rStr,
        out: *mut W2rHandle,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if let Err(e) = require(slot, "symbol_lookup", caps::GAME_READ) {
                return e;
            }
            if out.is_null() {
                return W2R_E_INVALID;
            }
            if name.len > W2R_MAX_SYMBOL_NAME {
                return W2R_E_LIMIT;
            }
            let name = match unsafe { text(name) } {
                Ok(t) => t,
                Err(e) => return e,
            };
            if let Err(e) = game(slot) {
                return e;
            }
            match Symbols::handle(name) {
                Some(h) => {
                    unsafe { *out = h };
                    W2R_OK
                }
                None => W2R_E_NOT_FOUND,
            }
        })
    }

    pub unsafe extern "C" fn symbol_read(
        token: *mut c_void,
        symbol: W2rHandle,
        offset: u32,
        dst: *mut c_void,
        len: u32,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if let Err(e) = require(slot, "symbol_read", caps::GAME_READ) {
                return e;
            }
            if dst.is_null() {
                return W2R_E_INVALID;
            }
            if len > W2R_MAX_READ_BYTES {
                return W2R_E_LIMIT;
            }
            if len == 0 {
                return W2R_OK;
            }
            let game = match game(slot) {
                Ok(g) => g,
                Err(e) => return e,
            };
            let Some(sym) = Symbols::by_handle(symbol) else {
                return W2R_E_NOT_FOUND;
            };
            let Some(addr) = game.symbols.address_of(sym).checked_add(offset as usize) else {
                return W2R_E_INVALID;
            };
            if !game.image.contains(addr, len as usize) {
                return W2R_E_INVALID;
            }
            // Both sides can fault (the game page could be unmapped, dst is plugin memory); the
            // guard turns either into E_INVALID.
            match guarded(|| unsafe {
                core::ptr::copy_nonoverlapping(addr as *const u8, dst.cast::<u8>(), len as usize)
            }) {
                Ok(()) => W2R_OK,
                Err(_) => W2R_E_INVALID,
            }
        })
    }

    /// `target` is a `symbol_lookup` handle of a function symbol; `original_out` receives the
    /// trampoline before the hook goes live.
    pub unsafe extern "C" fn hook_install(
        token: *mut c_void,
        target: u32,
        detour: *mut c_void,
        original_out: *mut *mut c_void,
        out: *mut W2rHandle,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if let Err(e) = require(slot, "hook_install", caps::HOOKS_RAW) {
                return e;
            }
            if detour.is_null() || out.is_null() {
                return W2R_E_INVALID;
            }
            let game = match game(slot) {
                Ok(g) => g,
                Err(e) => return e,
            };
            let Some(sym) = Symbols::by_handle(target) else {
                return W2R_E_NOT_FOUND;
            };
            if sym.kind != Kind::Function {
                return W2R_E_INVALID;
            }
            let addr = game.symbols.address_of(sym);
            let label = format!("{}:{}", slot.id, sym.name);
            let publish = |original: *mut c_void| {
                if !original_out.is_null() {
                    unsafe { *original_out = original };
                }
            };
            let result = install(slot.owner(), &game.image, addr, detour, &label, publish);
            match result {
                Ok(handle) => {
                    unsafe { *out = handle };
                    slot.log(W2R_LOG_INFO, &format!("hooked {} @ {addr:#010x}", sym.name));
                    W2R_OK
                }
                Err(HookError::NotCode) => W2R_E_INVALID,
                Err(HookError::Conflict(with)) => {
                    slot.log(
                        W2R_LOG_WARN,
                        &format!("hook_install {}: overlaps {with}", sym.name),
                    );
                    W2R_E_CONFLICT
                }
                Err(HookError::Limit) => W2R_E_LIMIT,
                Err(HookError::NotFound) => W2R_E_NOT_FOUND,
                Err(HookError::Publish) => {
                    slot.log(
                        W2R_LOG_WARN,
                        &format!("hook_install {}: original_out is not writable", sym.name),
                    );
                    W2R_E_INVALID
                }
                Err(HookError::Engine(status)) => {
                    slot.log(
                        W2R_LOG_ERROR,
                        &format!("hook_install {}: MH_STATUS {status}", sym.name),
                    );
                    W2R_E_INVALID
                }
            }
        })
    }

    #[cfg(windows)]
    fn install(
        owner: Owner,
        image: &Image,
        addr: usize,
        detour: *mut c_void,
        label: &str,
        publish: impl FnOnce(*mut c_void),
    ) -> Result<W2rHandle, HookError> {
        // SAFETY: the plugin asked for it with `hooks.raw`; a wrong detour is its own crash
        // (docs/abi.md, detours are outside the guard).
        unsafe { hooks::global().install(owner, image, addr, detour, label, publish) }
    }

    #[cfg(not(windows))]
    fn install(
        _: Owner,
        _: &Image,
        _: usize,
        _: *mut c_void,
        _: &str,
        _: impl FnOnce(*mut c_void),
    ) -> Result<W2rHandle, HookError> {
        Err(HookError::Engine(-1))
    }

    /// The runtime address of any symbol, for a plugin that calls game functions or writes game
    /// memory itself. Needs `hooks.raw`, not `game.read`: the address makes every access the
    /// plugin's own, outside the host's guard.
    pub unsafe extern "C" fn symbol_address(
        token: *mut c_void,
        symbol: W2rHandle,
        address_out: *mut usize,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if let Err(e) = require(slot, "symbol_address", caps::HOOKS_RAW) {
                return e;
            }
            if address_out.is_null() {
                return W2R_E_INVALID;
            }
            let game = match game(slot) {
                Ok(g) => g,
                Err(e) => return e,
            };
            let Some(sym) = Symbols::by_handle(symbol) else {
                return W2R_E_NOT_FOUND;
            };
            let addr = game.symbols.address_of(sym);
            if !game.image.contains(addr, 1) {
                return W2R_E_INVALID;
            }
            unsafe { *address_out = addr };
            W2R_OK
        })
    }

    pub unsafe extern "C" fn hook_remove(token: *mut c_void, hook: W2rHandle) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if let Err(e) = require(slot, "hook_remove", caps::HOOKS_RAW) {
                return e;
            }
            match hooks::global().remove(slot.owner(), hook) {
                Ok(()) => W2R_OK,
                Err(HookError::NotFound) => W2R_E_NOT_FOUND,
                Err(e) => {
                    slot.log(W2R_LOG_ERROR, &format!("hook_remove: {e:?}"));
                    W2R_E_INVALID
                }
            }
        })
    }

    /// docs/abi.md, "Create screen": registers (or with null removes) the plugin's one callback.
    pub unsafe extern "C" fn create_ui_register(
        token: *mut c_void,
        callback: Option<CreateUiFn>,
    ) -> W2rStatus {
        run(|| match enter(token) {
            Ok(slot) => {
                slot.create_ui.set(callback);
                W2R_OK
            }
            Err(e) => e,
        })
    }

    /// docs/abi.md, "Create screen": maps with `players` maximum players, only inside the callback.
    pub unsafe extern "C" fn create_map_count(
        token: *mut c_void,
        players: u32,
        count_out: *mut u32,
    ) -> W2rStatus {
        run(|| {
            if let Err(e) = enter(token) {
                return e;
            }
            if count_out.is_null() {
                return W2R_E_INVALID;
            }
            match crate::createpick::count(players) {
                Ok(n) => {
                    unsafe { count_out.write(n) };
                    W2R_OK
                }
                Err(e) => e,
            }
        })
    }

    /// docs/abi.md, "Create screen": selects the nth such map, only inside the callback.
    pub unsafe extern "C" fn create_map_select(
        token: *mut c_void,
        players: u32,
        nth: u32,
    ) -> W2rStatus {
        run(|| match enter(token) {
            Ok(_) => crate::createpick::select(players, nth),
            Err(e) => e,
        })
    }

    /// docs/abi.md, "Create screen": limits this callback's map calls to one folder.
    pub unsafe extern "C" fn create_map_folder(token: *mut c_void, folder: W2rStr) -> W2rStatus {
        run(|| {
            if let Err(e) = enter(token) {
                return e;
            }
            match unsafe { text(folder) } {
                Ok(f) => crate::createpick::set_folder(f),
                Err(e) => e,
            }
        })
    }

    /// The default of a declared key, for a read of a key that was never stored.
    fn declared_default(slot: &Slot, key: &str) -> Option<Val> {
        let declared = slot.declared.borrow();
        let d = declared.iter().find(|d| d.key == key)?;
        Some(match d.default_value() {
            settings::Value::Str(s) => Val::Str(s),
            settings::Value::Int(n) => Val::Int(n),
        })
    }

    /// Copies one `W2rStr` out of a declaration, refusing anything over `max` bytes before a
    /// byte of it is read.
    unsafe fn owned(s: W2rStr, max: u32) -> Result<String, W2rStatus> {
        if s.len > max {
            return Err(W2R_E_LIMIT);
        }
        unsafe { text(s) }.map(str::to_owned)
    }

    /// docs/abi.md, "Declared settings": only inside `on_load`; every string is copied here.
    pub unsafe extern "C" fn settings_declare(
        token: *mut c_void,
        def: *const W2rSettingDef,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if !slot.loading.get() {
                return W2R_E_STATE;
            }
            let Some(raw) = (unsafe { def.as_ref() }) else {
                return W2R_E_INVALID;
            };
            if raw.struct_size < W2R_SETTING_DEF_V1_SIZE || raw.reserved != 0 {
                return W2R_E_INVALID;
            }
            if slot.declared.borrow().len() >= crate::store::MAX_KEYS {
                return W2R_E_LIMIT;
            }
            let declared = match unsafe { copy_declaration(raw) } {
                Ok(d) => d,
                Err(e) => return e,
            };
            let key_text = declared.key.clone();
            let host_bools = crate::hostsettings::bool_keys();
            let result = {
                let earlier = slot.declared.borrow();
                settings::validate(
                    declared,
                    &slot.id,
                    "",
                    &earlier,
                    &host_bools,
                    crate::store::valid_key,
                )
            };
            match result {
                Ok(d) => {
                    slot.declared.borrow_mut().push(d);
                    W2R_OK
                }
                Err(status) => {
                    slot.log(
                        W2R_LOG_WARN,
                        &format!("settings_declare({key_text}) refused ({status})"),
                    );
                    status
                }
            }
        })
    }

    /// The strings of a `W2rSettingDef`, copied. Lengths are checked before any byte is read,
    /// so an absurd length is a status rather than a read past the plugin's buffer. Only the
    /// fields the kind uses are read, so a plugin may leave the others uninitialised.
    unsafe fn copy_declaration(raw: &W2rSettingDef) -> Result<DeclaredSetting, W2rStatus> {
        if raw.kind == W2R_SETTING_CHOICE && raw.choice_count > W2R_MAX_SETTING_CHOICE_COUNT {
            return Err(W2R_E_LIMIT);
        }
        let choices = if raw.kind == W2R_SETTING_CHOICE && raw.choice_count > 0 {
            if raw.choices.is_null() {
                return Err(W2R_E_INVALID);
            }
            let list =
                unsafe { core::slice::from_raw_parts(raw.choices, raw.choice_count as usize) };
            list.iter()
                .map(|c| unsafe { owned(*c, W2R_MAX_SETTING_CHOICE) })
                .collect::<Result<Vec<_>, _>>()?
        } else {
            Vec::new()
        };
        unsafe {
            Ok(DeclaredSetting {
                kind: raw.kind,
                flags: raw.flags,
                key: owned(raw.key, W2R_MAX_SETTING_KEY)?,
                label: owned(raw.label, W2R_MAX_SETTING_LABEL)?,
                description: owned(raw.description, W2R_MAX_SETTING_DESCRIPTION)?,
                category: owned(raw.category, W2R_MAX_SETTING_CATEGORY)?,
                subcategory: owned(raw.subcategory, W2R_MAX_SETTING_CATEGORY)?,
                default_int: raw.default_int,
                min: raw.min,
                max: raw.max,
                default_str: if raw.kind == W2R_SETTING_STRING || raw.kind == W2R_SETTING_COLOR {
                    owned(raw.default_str, W2R_MAX_SETTING_STR)?
                } else {
                    String::new()
                },
                choices,
                active_when: owned(
                    raw.active_when,
                    W2R_MAX_SETTING_KEY + settings::HOST_PREFIX.len() as u32,
                )?,
            })
        }
    }

    /// docs/abi.md: printable ASCII, at most 200 bytes, 4 per 5 s, only while chat is available.
    pub unsafe extern "C" fn chat_send(token: *mut c_void, text: W2rStr) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if let Err(e) = require(slot, "chat_send", caps::CHAT_WRITE) {
                return e;
            }
            if text.ptr.is_null() || text.len == 0 || text.len > W2R_MAX_CHAT_BYTES {
                return W2R_E_INVALID;
            }
            let bytes =
                unsafe { core::slice::from_raw_parts(text.ptr.cast::<u8>(), text.len as usize) };
            if !chat::valid_text(bytes) {
                return W2R_E_INVALID;
            }
            let line = core::str::from_utf8(bytes).expect("printable ASCII");
            let chat = &slot.shared.services.chat;
            if !chat.available() {
                return W2R_E_STATE;
            }
            // Charged only once the line went out: a refused call has no effect (abi.md).
            let now = chat::now_ms();
            if !slot.chat_rate.borrow_mut().would_allow(now) {
                return W2R_E_RATE;
            }
            if !chat.send(line) {
                return W2R_E_STATE;
            }
            slot.chat_rate.borrow_mut().allow(now);
            W2R_OK
        })
    }

    /// docs/abi.md: 1 to 960 bytes, lobby only with the session connected, the target (0-7 or
    /// `W2R_SLOT_ALL`) must have announced the handshake, 32/s per plugin and 128/s overall.
    pub unsafe extern "C" fn peer_send(
        token: *mut c_void,
        slot_or_all: u32,
        payload: W2rBuf,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if let Err(e) = require(slot, "peer_send", caps::NET_SEND) {
                return e;
            }
            if payload.ptr.is_null() || payload.len == 0 {
                return W2R_E_INVALID;
            }
            if payload.len > W2R_MAX_PEER_PAYLOAD {
                return W2R_E_LIMIT;
            }
            if slot_or_all != W2R_SLOT_ALL && slot_or_all >= peer::MAX_SLOTS {
                return W2R_E_INVALID;
            }
            let services = &slot.shared.services;
            // Personal build: in a networked match, plugin messages go to the other machines
            // that run the mod, over the game's own send (as camera sharing's do).
            if !services.presence.in_lobby() && crate::camerashare::in_networked_match() {
                let only = (slot_or_all != W2R_SLOT_ALL).then_some(slot_or_all);
                if only.is_some_and(|n| !crate::camerashare::match_peer(n)) {
                    return W2R_E_STATE;
                }
                let now = chat::now_ms();
                if !slot.peer_rate.borrow_mut().would_allow(now)
                    || !slot.shared.peer_rate.borrow_mut().would_allow(now)
                {
                    return W2R_E_RATE;
                }
                let bytes = unsafe { core::slice::from_raw_parts(payload.ptr, payload.len as usize) };
                let msg = peer::encode(slot.tag, bytes).expect("length checked");
                if crate::camerashare::send_in_match(only, &msg) == 0 {
                    return W2R_E_STATE;
                }
                slot.peer_rate.borrow_mut().allow(now);
                slot.shared.peer_rate.borrow_mut().allow(now);
                return W2R_OK;
            }
            if !services.presence.in_lobby() || !services.transport.connected() {
                return W2R_E_STATE;
            }
            let target = match peer::resolve_target(&*services.roster, slot_or_all) {
                Ok(t) => t,
                Err(TargetError::Invalid) => return W2R_E_INVALID,
                Err(TargetError::NotAnnounced) => return W2R_E_STATE,
            };
            // Both windows are checked first and charged only after the send, so a refusal
            // by either (or a failed send) costs no budget. No borrow is live across the
            // transport call.
            let now = chat::now_ms();
            if !slot.peer_rate.borrow_mut().would_allow(now)
                || !slot.shared.peer_rate.borrow_mut().would_allow(now)
            {
                return W2R_E_RATE;
            }
            let bytes = unsafe { core::slice::from_raw_parts(payload.ptr, payload.len as usize) };
            let msg = peer::encode(slot.tag, bytes).expect("length checked");
            let sent = match target {
                Target::All => services.transport.broadcast(&msg),
                Target::Slot(n) => services.transport.send_to(n, &msg),
            };
            if !sent {
                return W2R_E_STATE;
            }
            slot.peer_rate.borrow_mut().allow(now);
            slot.shared.peer_rate.borrow_mut().allow(now);
            W2R_OK
        })
    }

    // ---- settings and data files (implicit capabilities) ----------------------------------------

    /// Runs `f` on the plugin's settings, opening them on first use. The borrow ends before this
    /// returns and nothing in `f` can reach the plugin or the game.
    pub(super) fn with_settings<R>(
        slot: &Slot,
        f: impl FnOnce(&mut Settings) -> Result<R, W2rStatus>,
    ) -> Result<R, W2rStatus> {
        let Some(root) = slot.shared.services.root.as_ref() else {
            return Err(W2R_E_STATE);
        };
        let mut guard = slot.settings.borrow_mut();
        if guard.is_none() {
            let (settings, warning) =
                Settings::open(root.join("config").join(format!("{}.json", slot.id)));
            if let Some(w) = warning {
                slot.log(W2R_LOG_WARN, &w);
            }
            *guard = Some(settings);
        }
        f(guard.as_mut().expect("just opened"))
    }

    pub(super) fn set_status(e: SetError) -> W2rStatus {
        match e {
            SetError::BadKey => W2R_E_INVALID,
            SetError::TooManyKeys | SetError::TooLarge => W2R_E_LIMIT,
            SetError::Io => W2R_E_STATE,
        }
    }

    fn key<'a>(k: W2rStr) -> Result<&'a str, W2rStatus> {
        if k.len > W2R_MAX_SETTING_KEY {
            return Err(W2R_E_LIMIT);
        }
        let k = unsafe { text(k) }?;
        if crate::store::valid_key(k) {
            Ok(k)
        } else {
            Err(W2R_E_INVALID)
        }
    }

    pub unsafe extern "C" fn settings_get_i64(
        token: *mut c_void,
        key_: W2rStr,
        out: *mut i64,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if out.is_null() {
                return W2R_E_INVALID;
            }
            let k = match key(key_) {
                Ok(k) => k,
                Err(e) => return e,
            };
            let got = with_settings(slot, |s| match s.get(k) {
                None => match declared_default(slot, k) {
                    Some(Val::Int(i)) => Ok(i),
                    Some(Val::Str(_)) => Err(W2R_E_INVALID),
                    None => Err(W2R_E_NOT_FOUND),
                },
                Some(Val::Int(i)) => Ok(*i),
                Some(Val::Str(_)) => Err(W2R_E_INVALID),
            });
            match got {
                Ok(v) => {
                    unsafe { *out = v };
                    W2R_OK
                }
                Err(e) => e,
            }
        })
    }

    pub unsafe extern "C" fn settings_set_i64(
        token: *mut c_void,
        key_: W2rStr,
        value: i64,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            let k = match key(key_) {
                Ok(k) => k,
                Err(e) => return e,
            };
            match with_settings(slot, |s| s.set(k, Val::Int(value)).map_err(set_status)) {
                Ok(()) => W2R_OK,
                Err(e) => e,
            }
        })
    }

    /// Copies the value without a terminator. `len_out` always receives the full length; a
    /// value longer than `cap` leaves `buf` untouched and returns `W2R_E_LIMIT`.
    pub unsafe extern "C" fn settings_get_str(
        token: *mut c_void,
        key_: W2rStr,
        buf: *mut c_char,
        cap: u32,
        len_out: *mut u32,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if len_out.is_null() || (buf.is_null() && cap > 0) {
                return W2R_E_INVALID;
            }
            let k = match key(key_) {
                Ok(k) => k,
                Err(e) => return e,
            };
            let got = with_settings(slot, |s| match s.get(k) {
                None => match declared_default(slot, k) {
                    Some(Val::Str(v)) => Ok(v),
                    Some(Val::Int(_)) => Err(W2R_E_INVALID),
                    None => Err(W2R_E_NOT_FOUND),
                },
                Some(Val::Str(v)) => Ok(v.clone()),
                Some(Val::Int(_)) => Err(W2R_E_INVALID),
            });
            let value = match got {
                Ok(v) => v,
                Err(e) => return e,
            };
            unsafe { *len_out = value.len() as u32 };
            if value.len() > cap as usize {
                return W2R_E_LIMIT;
            }
            if !value.is_empty() {
                unsafe {
                    core::ptr::copy_nonoverlapping(value.as_ptr(), buf.cast::<u8>(), value.len())
                };
            }
            W2R_OK
        })
    }

    pub unsafe extern "C" fn settings_set_str(
        token: *mut c_void,
        key_: W2rStr,
        value: W2rStr,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            let k = match key(key_) {
                Ok(k) => k,
                Err(e) => return e,
            };
            if value.len > W2R_MAX_SETTING_STR {
                return W2R_E_LIMIT;
            }
            let v = match unsafe { text(value) } {
                Ok(v) => v.to_owned(),
                Err(e) => return e,
            };
            match with_settings(slot, |s| s.set(k, Val::Str(v)).map_err(set_status)) {
                Ok(()) => W2R_OK,
                Err(e) => e,
            }
        })
    }

    fn with_data<R>(
        slot: &Slot,
        f: impl FnOnce(&mut DataDir) -> Result<R, DataError>,
    ) -> Result<R, W2rStatus> {
        let Some(root) = slot.shared.services.root.as_ref() else {
            return Err(W2R_E_STATE);
        };
        let mut guard = slot.data.borrow_mut();
        let dir = guard
            .get_or_insert_with(|| DataDir::new(root.join("plugins").join(&slot.id).join("data")));
        f(dir).map_err(|e| match e {
            DataError::BadName | DataError::BadFlags => W2R_E_INVALID,
            DataError::TooManyOpen | DataError::TooManyFiles | DataError::TooLarge => W2R_E_LIMIT,
            DataError::NotFound | DataError::BadHandle => W2R_E_NOT_FOUND,
            DataError::WrongMode | DataError::Io => W2R_E_STATE,
        })
    }

    /// `flags` is `W2R_FILE_READ` (the file must exist) or `W2R_FILE_WRITE` (create or truncate).
    pub unsafe extern "C" fn data_open(
        token: *mut c_void,
        name: W2rStr,
        flags: u32,
        out: *mut W2rHandle,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if out.is_null() {
                return W2R_E_INVALID;
            }
            if name.len > W2R_MAX_FILE_NAME {
                return W2R_E_LIMIT;
            }
            let name = match unsafe { text(name) } {
                Ok(n) => n,
                Err(e) => return e,
            };
            match with_data(slot, |d| d.open(name, flags)) {
                Ok(h) => {
                    unsafe { *out = h };
                    W2R_OK
                }
                Err(e) => e,
            }
        })
    }

    /// Reads up to `cap` bytes from the current position; `read_out` is 0 at end of file.
    pub unsafe extern "C" fn data_read(
        token: *mut c_void,
        file: W2rHandle,
        dst: *mut c_void,
        cap: u32,
        read_out: *mut u32,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if read_out.is_null() || (dst.is_null() && cap > 0) {
                return W2R_E_INVALID;
            }
            if cap > W2R_MAX_IO_BYTES {
                return W2R_E_LIMIT;
            }
            match with_data(slot, |d| d.read(file, cap as usize)) {
                Ok(bytes) => {
                    if !bytes.is_empty() {
                        unsafe {
                            core::ptr::copy_nonoverlapping(
                                bytes.as_ptr(),
                                dst.cast::<u8>(),
                                bytes.len(),
                            )
                        };
                    }
                    unsafe { *read_out = bytes.len() as u32 };
                    W2R_OK
                }
                Err(e) => e,
            }
        })
    }

    pub unsafe extern "C" fn data_write(
        token: *mut c_void,
        file: W2rHandle,
        src: *const c_void,
        len: u32,
    ) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            if src.is_null() && len > 0 {
                return W2R_E_INVALID;
            }
            if len > W2R_MAX_IO_BYTES {
                return W2R_E_LIMIT;
            }
            // Copied first: a bad plugin pointer faults here, inside the plugin's guard, before
            // any host borrow is live.
            let bytes = if len == 0 {
                Vec::new()
            } else {
                unsafe { core::slice::from_raw_parts(src.cast::<u8>(), len as usize) }.to_vec()
            };
            match with_data(slot, |d| d.write(file, &bytes)) {
                Ok(()) => W2R_OK,
                Err(e) => e,
            }
        })
    }

    pub unsafe extern "C" fn data_close(token: *mut c_void, file: W2rHandle) -> W2rStatus {
        run(|| {
            let slot = match enter(token) {
                Ok(s) => s,
                Err(e) => return e,
            };
            match with_data(slot, |d| d.close(file)) {
                Ok(()) => W2R_OK,
                Err(e) => e,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dispatch::Registry;
    use crate::target::symbols::SYMBOLS;
    use std::cell::RefCell;

    #[derive(Default)]
    struct Lines(RefCell<Vec<String>>);
    impl Log for Lines {
        fn line(&self, level: u32, plugin: &str, msg: &str) {
            self.0
                .borrow_mut()
                .push(format!("[{level}] {plugin}: {msg}"));
        }
    }

    unsafe extern "C" fn on_load(_: *const W2rHost) -> W2rStatus {
        W2R_OK
    }
    static T: W2rPlugin = W2rPlugin {
        struct_size: W2R_PLUGIN_V1_SIZE,
        on_load: Some(on_load),
        on_event: None,
        on_ui: None,
        on_unload: None,
    };
    unsafe extern "C" fn entry(_: *const W2rHost) -> *const W2rPlugin {
        &T
    }

    fn s(text: &str) -> W2rStr {
        W2rStr {
            ptr: text.as_ptr().cast(),
            len: text.len() as u32,
        }
    }

    #[test]
    fn capabilities_gate_the_functions_and_are_logged_once() {
        let log = Rc::new(Lines::default());
        let mut reg = Registry::new(log.clone());
        let i = reg.load_entry("p", entry).unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        let mut handle = 0;
        let mut buf = [0u8; 4];
        unsafe {
            assert_eq!(
                (h.symbol_lookup)(h.token, s("ProcessNetworkTurn"), &mut handle),
                W2R_E_DENIED
            );
            assert_eq!(
                (h.symbol_lookup)(h.token, s("ProcessNetworkTurn"), &mut handle),
                W2R_E_DENIED
            );
            assert_eq!(
                (h.symbol_read)(h.token, 1, 0, buf.as_mut_ptr().cast(), 4),
                W2R_E_DENIED
            );
            assert_eq!(
                (h.hook_install)(
                    h.token,
                    1,
                    buf.as_mut_ptr().cast(),
                    core::ptr::null_mut(),
                    &mut handle
                ),
                W2R_E_DENIED
            );
            assert_eq!((h.hook_remove)(h.token, 1), W2R_E_DENIED);
            assert_eq!((h.subscribe)(h.token, W2R_EVT_CHAT_RECEIVED), W2R_E_DENIED);
            assert_eq!((h.subscribe)(h.token, W2R_EVT_PEER_MESSAGE), W2R_E_DENIED);
            assert_eq!((h.subscribe)(h.token, W2R_EVT_LOBBY_ENTER), W2R_OK);
        }
        let lines = log.0.borrow();
        let denials: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("not approved"))
            .collect();
        assert_eq!(denials.len(), 5, "{denials:?}");
        assert_eq!(
            denials[0],
            "[2] p: symbol_lookup: capability game.read not approved"
        );
        assert!(denials[4].contains("subscribe: capability chat.read"));
    }

    #[test]
    fn a_token_that_is_not_a_slot_or_a_disabled_plugin_is_refused() {
        let log = Rc::new(Lines::default());
        let mut reg = Registry::new(log.clone());
        let i = reg.load_entry("p", entry).unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        unsafe {
            // The classic mistake: `host` instead of `host->token`.
            let wrong = (h as *const W2rHost).cast_mut().cast::<c_void>();
            assert_eq!((h.log)(wrong, W2R_LOG_INFO, s("x")), W2R_E_INVALID);
            assert_eq!((h.subscribe)(wrong, W2R_EVT_LOBBY_ENTER), W2R_E_INVALID);
            assert_eq!(
                (h.log)(core::ptr::null_mut(), W2R_LOG_INFO, s("x")),
                W2R_E_INVALID
            );
            assert_eq!((h.log)(h.token, W2R_LOG_INFO, s("fine")), W2R_OK);
        }
        reg.slot(i).disable("test".into());
        unsafe {
            assert_eq!((h.log)(h.token, W2R_LOG_INFO, s("x")), W2R_E_STATE);
            assert_eq!((h.subscribe)(h.token, W2R_EVT_LOBBY_ENTER), W2R_E_STATE);
            assert_eq!((h.chat_send)(h.token, s("hi")), W2R_E_STATE);
        }
        let lines = log.0.borrow();
        assert!(lines.iter().any(|l| l == "[1] p: fine"), "{lines:?}");
        assert!(!lines.iter().any(|l| l.ends_with(": x")), "{lines:?}");
    }

    #[test]
    fn plugin_log_lines_are_throttled_with_a_summary() {
        let log = Rc::new(Lines::default());
        let mut reg = Registry::new(log.clone());
        let i = reg.load_entry("p", entry).unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        for _ in 0..(LOG_BURST + 10) {
            assert_eq!(unsafe { (h.log)(h.token, W2R_LOG_INFO, s("spam")) }, W2R_OK);
        }
        let lines = log.0.borrow();
        let spam = lines.iter().filter(|l| l.ends_with(": spam")).count();
        assert_eq!(spam, LOG_BURST);
        assert!(
            lines.iter().any(|l| l.contains("log lines dropped")),
            "{lines:?}"
        );
    }

    #[test]
    fn without_a_game_symbol_functions_report_state_after_validation() {
        let log = Rc::new(Lines::default());
        let mut reg = Registry::new(log);
        let i = reg
            .load_entry_with_caps("p", entry, &[caps::GAME_READ, caps::HOOKS_RAW])
            .unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        let mut handle = 0;
        let mut buf = [0u8; 4];
        let long = "x".repeat(W2R_MAX_SYMBOL_NAME as usize + 1);
        unsafe {
            assert_eq!(
                (h.symbol_lookup)(h.token, s("x"), core::ptr::null_mut()),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.symbol_lookup)(h.token, s(&long), &mut handle),
                W2R_E_LIMIT
            );
            assert_eq!(
                (h.symbol_lookup)(h.token, s("ProcessNetworkTurn"), &mut handle),
                W2R_E_STATE
            );
            assert_eq!(
                (h.symbol_read)(h.token, 1, 0, core::ptr::null_mut(), 4),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.symbol_read)(
                    h.token,
                    1,
                    0,
                    buf.as_mut_ptr().cast(),
                    W2R_MAX_READ_BYTES + 1
                ),
                W2R_E_LIMIT
            );
            assert_eq!(
                (h.symbol_read)(h.token, 1, 0, buf.as_mut_ptr().cast(), 0),
                W2R_OK
            );
            assert_eq!(
                (h.symbol_read)(h.token, 1, 0, buf.as_mut_ptr().cast(), 4),
                W2R_E_STATE
            );
            assert_eq!(
                (h.hook_install)(
                    h.token,
                    1,
                    buf.as_mut_ptr().cast(),
                    core::ptr::null_mut(),
                    &mut handle
                ),
                W2R_E_STATE
            );
            assert_eq!((h.hook_remove)(h.token, 99), W2R_E_NOT_FOUND);
            let mut addr = 0usize;
            assert_eq!(
                (h.symbol_address)(h.token, 1, core::ptr::null_mut()),
                W2R_E_INVALID
            );
            assert_eq!((h.symbol_address)(h.token, 1, &mut addr), W2R_E_STATE);
        }
    }

    #[test]
    fn symbol_address_needs_hooks_raw_not_game_read() {
        let log = Rc::new(Lines::default());
        let mut reg = Registry::new(log);
        let read_only = reg
            .load_entry_with_caps("ro", entry, &[caps::GAME_READ])
            .unwrap();
        let h = unsafe { &*reg.slot(read_only).host() };
        let mut addr = 0usize;
        unsafe {
            assert_eq!((h.symbol_address)(h.token, 1, &mut addr), W2R_E_DENIED);
        }
        assert_eq!(addr, 0);
    }

    /// Uses this test process as the "game": its own image, with the symbol table relocated to
    /// its base. Reads land inside the test binary, which is all `symbol_read` has to check.
    #[cfg(windows)]
    #[test]
    fn symbol_lookup_and_read_against_a_live_image() {
        use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
        let base = unsafe { GetModuleHandleW(core::ptr::null()) } as usize;
        let image = Image::from_module(base).unwrap();
        let log = Rc::new(Lines::default());
        let mut reg = Registry::with_game(
            log,
            Game {
                image: image.clone(),
                symbols: Symbols::new(base),
            },
        );
        let i = reg
            .load_entry_with_caps("p", entry, &[caps::GAME_READ, caps::HOOKS_RAW])
            .unwrap();
        let h = unsafe { &*reg.slot(i).host() };

        // The lowest symbol is the one most likely to fall inside a smaller image.
        let lowest = SYMBOLS.iter().min_by_key(|s| s.static_addr).unwrap();
        let rva = lowest.static_addr as usize - crate::target::PREFERRED_IMAGE_BASE;
        assert!(
            image.contains(base + rva, 16),
            "test binary smaller than {rva:#x}; pick another symbol"
        );

        let mut handle = 0;
        let mut buf = [0u8; 16];
        unsafe {
            assert_eq!(
                (h.symbol_lookup)(h.token, s("NoSuchSymbol"), &mut handle),
                W2R_E_NOT_FOUND
            );
            assert_eq!(
                (h.symbol_lookup)(h.token, s(lowest.name), &mut handle),
                W2R_OK
            );
            assert_eq!(Symbols::by_handle(handle).unwrap().name, lowest.name);
            assert_eq!(
                (h.symbol_read)(h.token, handle, 0, buf.as_mut_ptr().cast(), 16),
                W2R_OK
            );
            let expected = core::slice::from_raw_parts((base + rva) as *const u8, 16);
            assert_eq!(&buf, expected);
            assert_eq!(
                (h.symbol_read)(h.token, 0, 0, buf.as_mut_ptr().cast(), 16),
                W2R_E_NOT_FOUND
            );
            // The address is the image base plus the symbol's RVA, for a function and a global.
            let mut addr = 0usize;
            assert_eq!((h.symbol_address)(h.token, handle, &mut addr), W2R_OK);
            assert_eq!(addr, base + rva);
            assert_eq!((h.symbol_address)(h.token, 0, &mut addr), W2R_E_NOT_FOUND);
            let some_global = SYMBOLS.iter().find(|s| s.kind == Kind::Object).unwrap();
            let gh = Symbols::handle(some_global.name).unwrap();
            let grva = some_global.static_addr as usize - crate::target::PREFERRED_IMAGE_BASE;
            if image.contains(base + grva, 1) {
                assert_eq!((h.symbol_address)(h.token, gh, &mut addr), W2R_OK);
                assert_eq!(addr, base + grva);
            }
            assert_eq!(
                (h.symbol_read)(h.token, handle, 0x7FFF_FFFF, buf.as_mut_ptr().cast(), 16),
                W2R_E_INVALID,
                "past the image"
            );
            // Hooking a global is refused; a function handle gets as far as the code check.
            let global = SYMBOLS.iter().find(|s| s.kind == Kind::Object).unwrap();
            let gh = Symbols::handle(global.name).unwrap();
            assert_eq!(
                (h.hook_install)(
                    h.token,
                    gh,
                    buf.as_mut_ptr().cast(),
                    core::ptr::null_mut(),
                    &mut handle
                ),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.hook_install)(
                    h.token,
                    0,
                    buf.as_mut_ptr().cast(),
                    core::ptr::null_mut(),
                    &mut handle
                ),
                W2R_E_NOT_FOUND
            );
        }
    }

    fn registry_with_root(tag: &str) -> (Registry, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!("w2r-host-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let log = Rc::new(Lines::default());
        let services = Services {
            root: Some(root.clone()),
            ..Services::none()
        };
        (Registry::with(log, None, services), root)
    }

    fn setting_def(kind: u32, key: &'static str) -> W2rSettingDef {
        W2rSettingDef {
            struct_size: W2R_SETTING_DEF_V1_SIZE,
            kind,
            flags: 0,
            key: s(key),
            label: s("Label"),
            description: s(""),
            category: s("Chat"),
            subcategory: s("Behavior"),
            default_int: 0,
            min: 0,
            max: 0,
            default_str: s(""),
            choices: core::ptr::null(),
            choice_count: 0,
            reserved: 0,
            active_when: s(""),
        }
    }

    thread_local! {
        static DECLARE_RESULTS: RefCell<Vec<W2rStatus>> = const { RefCell::new(Vec::new()) };
    }

    const CHOICES: [&str; 3] = ["Off", "Some", "All"];

    unsafe extern "C" fn declaring_on_load(host: *const W2rHost) -> W2rStatus {
        let h = unsafe { &*host };
        let choices: Vec<W2rStr> = CHOICES.iter().map(|c| s(c)).collect();
        let mut fade = setting_def(W2R_SETTING_INT, "fade");
        fade.min = 0;
        fade.max = 60;
        fade.default_int = 7;
        let mut channels = setting_def(W2R_SETTING_BOOL, "channels");
        channels.default_int = 1;
        let mut style = setting_def(W2R_SETTING_CHOICE, "style");
        style.choices = choices.as_ptr();
        style.choice_count = choices.len() as u32;
        style.default_int = 2;
        style.active_when = s("channels");
        let mut tag = setting_def(W2R_SETTING_STRING, "tag");
        tag.default_str = s("[All]");
        let mut bad = setting_def(W2R_SETTING_INT, "bad");
        bad.min = 3;
        bad.max = 1;
        let mut short = setting_def(W2R_SETTING_BOOL, "short");
        short.struct_size = 4;
        let mut reserved = setting_def(W2R_SETTING_BOOL, "reserved");
        reserved.reserved = 1;
        // A field the kind does not use is never read, whatever it holds.
        let mut unused = setting_def(W2R_SETTING_BOOL, "unused");
        unused.default_str = W2rStr {
            ptr: core::ptr::dangling(),
            len: u32::MAX,
        };
        let results = [
            unsafe { (h.settings_declare)(h.token, &fade) },
            unsafe { (h.settings_declare)(h.token, &channels) },
            unsafe { (h.settings_declare)(h.token, &style) },
            unsafe { (h.settings_declare)(h.token, &tag) },
            unsafe { (h.settings_declare)(h.token, &fade) },
            unsafe { (h.settings_declare)(h.token, &bad) },
            unsafe { (h.settings_declare)(h.token, &short) },
            unsafe { (h.settings_declare)(h.token, core::ptr::null()) },
            unsafe { (h.settings_declare)(h.token, &reserved) },
            unsafe { (h.settings_declare)(h.token, &unused) },
        ];
        DECLARE_RESULTS.with(|r| r.borrow_mut().extend(results));
        W2R_OK
    }
    static DECLARING: W2rPlugin = W2rPlugin {
        struct_size: W2R_PLUGIN_V1_SIZE,
        on_load: Some(declaring_on_load),
        on_event: None,
        on_ui: None,
        on_unload: None,
    };
    unsafe extern "C" fn declaring_entry(_: *const W2rHost) -> *const W2rPlugin {
        &DECLARING
    }

    #[test]
    fn settings_declare_works_only_in_on_load_and_validates() {
        let (mut reg, root) = registry_with_root("declare");
        DECLARE_RESULTS.with(|r| r.borrow_mut().clear());
        let i = reg.load_entry("w2r-chat", declaring_entry).unwrap();
        let got = DECLARE_RESULTS.with(|r| r.borrow().clone());
        assert_eq!(
            got,
            vec![
                W2R_OK,
                W2R_OK,
                W2R_OK,
                W2R_OK,
                W2R_E_CONFLICT,
                W2R_E_INVALID,
                W2R_E_INVALID,
                W2R_E_INVALID,
                W2R_E_INVALID,
                W2R_OK
            ]
        );
        let slot = reg.slot(i);
        let declared = slot.declared();
        assert_eq!(declared.len(), 5);
        assert_eq!(declared[2].choices, vec!["Off", "Some", "All"]);
        assert_eq!(
            declared[2].active_when,
            Some(settings::ActiveWhen {
                owner: settings::Owner::Plugin("w2r-chat".into()),
                key: "channels".into(),
                test: settings::Condition::On,
            })
        );

        // After on_load: refused.
        let h = unsafe { &*slot.host() };
        let late = setting_def(W2R_SETTING_BOOL, "late");
        assert_eq!(unsafe { (h.settings_declare)(h.token, &late) }, W2R_E_STATE);

        // A declared key that was never stored reads as its default.
        let mut n = 0i64;
        let mut buf = [0u8; 16];
        let mut len = 0u32;
        unsafe {
            assert_eq!((h.settings_get_i64)(h.token, s("fade"), &mut n), W2R_OK);
            assert_eq!(n, 7);
            assert_eq!(
                (h.settings_get_str)(h.token, s("tag"), buf.as_mut_ptr().cast(), 16, &mut len),
                W2R_OK
            );
            assert_eq!(&buf[..len as usize], b"[All]");
            assert_eq!(
                (h.settings_get_i64)(h.token, s("tag"), &mut n),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.settings_get_i64)(h.token, s("missing"), &mut n),
                W2R_E_NOT_FOUND
            );
        }

        // The options screen stores through the slot; the plugin then reads the stored value.
        assert_eq!(slot.stored_setting("fade"), None);
        slot.store_setting("fade", Val::Int(12)).unwrap();
        assert_eq!(slot.stored_setting("fade"), Some(Val::Int(12)));
        unsafe {
            assert_eq!((h.settings_get_i64)(h.token, s("fade"), &mut n), W2R_OK);
        }
        assert_eq!(n, 12);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_functions_follow_the_abi_rules() {
        let (mut reg, root) = registry_with_root("settings");
        let i = reg.load_entry("chatty", entry).unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        let mut n = 0i64;
        let mut buf = [0u8; 8];
        let mut len = 0u32;
        let long_key = "k".repeat(W2R_MAX_SETTING_KEY as usize + 1);
        let long_val = "v".repeat(W2R_MAX_SETTING_STR as usize + 1);
        unsafe {
            assert_eq!(
                (h.settings_get_i64)(h.token, s("lines"), &mut n),
                W2R_E_NOT_FOUND
            );
            assert_eq!((h.settings_set_i64)(h.token, s("lines"), 12), W2R_OK);
            assert_eq!((h.settings_get_i64)(h.token, s("lines"), &mut n), W2R_OK);
            assert_eq!(n, 12);
            assert_eq!(
                (h.settings_set_i64)(h.token, s("Bad Key"), 1),
                W2R_E_INVALID
            );
            assert_eq!((h.settings_set_i64)(h.token, s(&long_key), 1), W2R_E_LIMIT);
            assert_eq!(
                (h.settings_get_i64)(h.token, s("lines"), core::ptr::null_mut()),
                W2R_E_INVALID
            );

            assert_eq!((h.settings_set_str)(h.token, s("name"), s("hello")), W2R_OK);
            assert_eq!(
                (h.settings_set_str)(h.token, s("name"), s(&long_val)),
                W2R_E_LIMIT
            );
            assert_eq!(
                (h.settings_get_str)(h.token, s("name"), buf.as_mut_ptr().cast(), 8, &mut len),
                W2R_OK
            );
            assert_eq!(&buf[..len as usize], b"hello");
            // Too small a buffer: the full length comes back and the buffer is untouched.
            buf = [0; 8];
            assert_eq!(
                (h.settings_get_str)(h.token, s("name"), buf.as_mut_ptr().cast(), 3, &mut len),
                W2R_E_LIMIT
            );
            assert_eq!((len, buf), (5, [0; 8]));
            // A value of the other type is refused, not converted.
            assert_eq!(
                (h.settings_get_i64)(h.token, s("name"), &mut n),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.settings_get_str)(h.token, s("lines"), buf.as_mut_ptr().cast(), 8, &mut len),
                W2R_E_INVALID
            );
        }
        // Stored under the plugin's own id, and read back by a fresh registry.
        assert!(root.join("config").join("chatty.json").is_file());
        drop(reg);
        let log = Rc::new(Lines::default());
        let services = Services {
            root: Some(root.clone()),
            ..Services::none()
        };
        let mut again = Registry::with(log, None, services);
        let j = again.load_entry("chatty", entry).unwrap();
        let h = unsafe { &*again.slot(j).host() };
        unsafe {
            assert_eq!((h.settings_get_i64)(h.token, s("lines"), &mut n), W2R_OK);
        }
        assert_eq!(n, 12);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn settings_and_data_report_state_without_a_root() {
        let log = Rc::new(Lines::default());
        let mut reg = Registry::new(log);
        let i = reg.load_entry("p", entry).unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        let mut n = 0i64;
        let mut handle = 0;
        unsafe {
            assert_eq!((h.settings_get_i64)(h.token, s("a"), &mut n), W2R_E_STATE);
            assert_eq!((h.settings_set_i64)(h.token, s("a"), 1), W2R_E_STATE);
            assert_eq!(
                (h.data_open)(h.token, s("a.log"), W2R_FILE_WRITE, &mut handle),
                W2R_E_STATE
            );
        }
    }

    #[test]
    fn data_functions_follow_the_abi_rules() {
        let (mut reg, root) = registry_with_root("data");
        let i = reg.load_entry("logger", entry).unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        let mut file = 0;
        let mut got = 0u32;
        let mut buf = [0u8; 16];
        let long_name = "n".repeat(W2R_MAX_FILE_NAME as usize + 1);
        unsafe {
            assert_eq!(
                (h.data_open)(h.token, s("a.log"), W2R_FILE_READ, &mut file),
                W2R_E_NOT_FOUND
            );
            assert_eq!(
                (h.data_open)(h.token, s("../a"), W2R_FILE_WRITE, &mut file),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.data_open)(h.token, s("con"), W2R_FILE_WRITE, &mut file),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.data_open)(h.token, s(&long_name), W2R_FILE_WRITE, &mut file),
                W2R_E_LIMIT
            );
            assert_eq!(
                (h.data_open)(h.token, s("a.log"), 3, &mut file),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.data_open)(h.token, s("a.log"), W2R_FILE_WRITE, core::ptr::null_mut()),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.data_open)(h.token, s("a.log"), W2R_FILE_WRITE, &mut file),
                W2R_OK
            );
            let w = file;
            assert_eq!(
                (h.data_write)(h.token, w, b"hello\n".as_ptr().cast(), 6),
                W2R_OK
            );
            assert_eq!(
                (h.data_write)(h.token, w, core::ptr::null(), 1),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.data_write)(h.token, w, buf.as_ptr().cast(), W2R_MAX_IO_BYTES + 1),
                W2R_E_LIMIT
            );
            assert_eq!(
                (h.data_read)(h.token, w, buf.as_mut_ptr().cast(), 16, &mut got),
                W2R_E_STATE,
                "a write handle cannot read"
            );
            assert_eq!((h.data_close)(h.token, w), W2R_OK);
            assert_eq!((h.data_close)(h.token, w), W2R_E_NOT_FOUND);
            assert_eq!(
                (h.data_open)(h.token, s("a.log"), W2R_FILE_READ, &mut file),
                W2R_OK
            );
            assert_eq!(
                (h.data_read)(h.token, file, buf.as_mut_ptr().cast(), 16, &mut got),
                W2R_OK
            );
            assert_eq!(&buf[..got as usize], b"hello\n");
            assert_eq!(
                (h.data_read)(h.token, file, buf.as_mut_ptr().cast(), 16, &mut got),
                W2R_OK
            );
            assert_eq!(got, 0, "end of file");
            assert_eq!((h.data_close)(h.token, file), W2R_OK);
        }
        // It landed in this plugin's own data folder.
        assert!(
            root.join("plugins")
                .join("logger")
                .join("data")
                .join("a.log")
                .is_file()
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
