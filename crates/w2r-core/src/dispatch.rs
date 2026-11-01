//! The plugin registry: runs load-policy checks 9 (`E_ENTRY`) and 10 (`E_LOAD`), dispatches
//! events, and disables a plugin the first time one of its callbacks faults.
//!
//! Checks 1 to 8 (manifest, build, hash pin, capabilities, tag, dependencies) run before a
//! plugin reaches this type; see docs/load-policy.md. A refused plugin keeps its slot, marked
//! `Disabled`, so the in-game list can show why.

use core::ffi::c_void;
use std::fmt;
use std::rc::Rc;

use w2r_abi::*;

use crate::guard::guarded;
use crate::host::{Game, Log, Services, Shared, Slot, caps};
use crate::peer::PluginMessage;

/// A load-policy refusal: `code` is the table's code, `detail` the logged explanation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LoadError {
    pub code: &'static str,
    pub detail: String,
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.code, self.detail)
    }
}

pub struct Registry {
    /// Boxed on purpose: a slot's address is the plugin's `token`, so slots must not move
    /// when the vector grows.
    #[allow(clippy::vec_box)]
    slots: Vec<Box<Slot>>,
    shared: Rc<Shared>,
    /// Loaded DLLs, kept for the session. There is no live unload in version 1, and freeing a
    /// module that faulted is not safe anyway.
    #[cfg(windows)]
    modules: Vec<crate::loader::Dll>,
}

impl Registry {
    /// A registry with no game behind it (tests, tooling): `game.read` functions report
    /// `W2R_E_STATE`, chat and peers are never available.
    pub fn new(log: Rc<dyn Log>) -> Self {
        Self::with(log, None, Services::none())
    }

    pub fn with_game(log: Rc<dyn Log>, game: Game) -> Self {
        Self::with(log, Some(game), Services::none())
    }

    pub fn with(log: Rc<dyn Log>, game: Option<Game>, services: Services) -> Self {
        Registry {
            slots: Vec::new(),
            shared: Rc::new(Shared::new(log, game, services)),
            #[cfg(windows)]
            modules: Vec::new(),
        }
    }

    pub fn shared(&self) -> &Rc<Shared> {
        &self.shared
    }

    pub fn len(&self) -> usize {
        self.slots.len()
    }

    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    pub fn slot(&self, index: usize) -> &Slot {
        &self.slots[index]
    }

    pub fn find(&self, id: &str) -> Option<&Slot> {
        self.slots.iter().map(|s| &**s).find(|s| s.id == id)
    }

    /// Opens, pins and loads a plugin DLL (docs/load-policy.md, "No check-then-swap gap"), then
    /// runs [`Registry::load_entry_with_caps`].
    #[cfg(windows)]
    pub fn load_dll(
        &mut self,
        id: &str,
        path: &std::path::Path,
        pin: &[u8; 32],
        caps: &[&str],
    ) -> Result<usize, LoadError> {
        let dll = crate::loader::load(path, pin).inspect_err(|e| {
            self.shared.log.line(W2R_LOG_ERROR, id, &e.to_string());
        })?;
        let entry = dll.entry;
        self.modules.push(dll);
        self.load_entry_with_caps(id, entry, caps)
    }

    /// [`Registry::load_entry_with_caps`] with only the implicit capabilities.
    pub fn load_entry(&mut self, id: &str, entry: W2rPluginMain) -> Result<usize, LoadError> {
        self.load_entry_with_caps(id, entry, &[])
    }

    /// Checks 9 and 10 for a plugin whose entry point is already resolved, with its approved
    /// capabilities (check 6 happened before). On failure the slot stays in the registry as
    /// `Disabled`, and the plugin is never called again.
    pub fn load_entry_with_caps(
        &mut self,
        id: &str,
        entry: W2rPluginMain,
        caps: &[&str],
    ) -> Result<usize, LoadError> {
        let index = self.slots.len();
        self.slots
            .push(Slot::new(index, id, caps, self.shared.clone()));
        let slot = &self.slots[index];
        let host = slot.host();

        // Copy the table inside the guard: a garbage pointer faults here, not later.
        let table = guarded(|| unsafe {
            let p = entry(host);
            if p.is_null() { None } else { Some(*p) }
        });
        let table = match table {
            Err(fault) => {
                return Err(self.refuse(
                    index,
                    "E_ENTRY",
                    format!("w2r_plugin_main faulted: {fault}"),
                ));
            }
            Ok(None) => {
                return Err(self.refuse(index, "E_ENTRY", "w2r_plugin_main returned null".into()));
            }
            Ok(Some(table)) => table,
        };
        if table.struct_size < W2R_PLUGIN_V1_SIZE {
            let detail = format!(
                "struct_size {} is below the version-1 minimum {}",
                table.struct_size, W2R_PLUGIN_V1_SIZE
            );
            return Err(self.refuse(index, "E_ENTRY", detail));
        }
        let Some(on_load) = table.on_load else {
            return Err(self.refuse(index, "E_ENTRY", "on_load is null".into()));
        };
        slot.table.set(table);

        slot.set_loading(true);
        let loaded = guarded(|| unsafe { on_load(host) });
        slot.set_loading(false);
        match loaded {
            Ok(W2R_OK) => Ok(index),
            Ok(status) => Err(self.refuse(index, "E_LOAD", format!("on_load returned {status}"))),
            Err(fault) => Err(self.refuse(index, "E_LOAD", format!("on_load faulted: {fault}"))),
        }
    }

    fn refuse(&self, index: usize, code: &'static str, detail: String) -> LoadError {
        self.slots[index].disable(format!("{code}: {detail}"));
        LoadError { code, detail }
    }

    /// Delivers one event to every active, subscribed plugin. A plugin that faults is disabled
    /// and the loop continues with the next one.
    pub fn dispatch(&self, id: W2rEventId, payload: *const c_void) {
        let event = W2rEvent {
            struct_size: size_of::<W2rEvent>() as u32,
            id,
            payload,
        };
        for slot in &self.slots {
            if slot.is_active() && slot.subscribed(id) {
                deliver(slot, &event);
            }
        }
    }

    /// Delivers a plugin peer message to the one plugin whose tag matches, if that plugin is
    /// active, subscribed and allowed to receive (`net.send`), and the sender announced the
    /// handshake. Anything else is dropped (docs/abi.md). Returns whether it was delivered.
    pub fn dispatch_peer(&self, sender: u32, msg: PluginMessage<'_>) -> bool {
        // In the lobby from an announced peer (docs/abi.md); also in a networked
        // match from a machine that runs the mod (announced in the lobby just left, still in).
        let in_lobby = self.shared.services.presence.in_lobby()
            && self.shared.services.roster.announced(sender);
        let in_match = !self.shared.services.presence.in_lobby()
            && crate::camerashare::in_networked_match()
            && crate::camerashare::match_peer(sender);
        if !(in_lobby || in_match) {
            return false;
        }
        let Some(slot) = self.slots.iter().find(|s| s.tag == msg.tag) else {
            return false;
        };
        if !slot.is_active()
            || !slot.subscribed(W2R_EVT_PEER_MESSAGE)
            || !slot.has_cap(caps::NET_SEND)
        {
            return false;
        }
        let Some(on_event) = slot.table.get().on_event else {
            return false;
        };
        let message = W2rEvtPeerMessage {
            struct_size: size_of::<W2rEvtPeerMessage>() as u32,
            net_slot: sender,
            data: W2rBuf {
                ptr: msg.payload.as_ptr(),
                len: msg.payload.len() as u32,
            },
        };
        let event = W2rEvent {
            struct_size: size_of::<W2rEvent>() as u32,
            id: W2R_EVT_PEER_MESSAGE,
            payload: (&message as *const W2rEvtPeerMessage).cast(),
        };
        if let Err(fault) = guarded(|| unsafe { on_event(&event) }) {
            slot.disable(format!("on_event({W2R_EVT_PEER_MESSAGE}): {fault}"));
        }
        true
    }

    /// `W2R_EVT_SETTINGS_CHANGED` to one plugin, if it is active and subscribed: the options
    /// screen stored new values for it.
    pub fn settings_changed(&self, slot: &Slot) {
        if !slot.is_active() || !slot.subscribed(W2R_EVT_SETTINGS_CHANGED) {
            return;
        }
        let event = W2rEvent {
            struct_size: size_of::<W2rEvent>() as u32,
            id: W2R_EVT_SETTINGS_CHANGED,
            payload: core::ptr::null(),
        };
        deliver(slot, &event);
    }

    pub fn slots(&self) -> impl Iterator<Item = &Slot> {
        self.slots.iter().map(|s| &**s)
    }

    /// Calls one active plugin's `on_ui` with `ui`, under the guard; a fault disables it.
    /// Returns whether the plugin has an `on_ui` and was called.
    pub fn call_on_ui(&self, slot: &Slot, ui: &W2rUi) -> bool {
        if !slot.is_active() {
            return false;
        }
        let Some(on_ui) = slot.table.get().on_ui else {
            return false;
        };
        let started = std::time::Instant::now();
        if let Err(fault) = guarded(|| unsafe { on_ui(ui) }) {
            slot.disable(format!("on_ui: {fault}"));
        }
        slot.note_duration(started.elapsed().as_millis(), "on_ui");
        true
    }

    /// Calls one active plugin's create-screen callback with `ui`, under the guard; a fault
    /// disables it. Returns whether the plugin had one.
    pub fn call_create_ui(&self, slot: &Slot, ui: &W2rUi) -> bool {
        let Some(callback) = slot.create_ui() else {
            return false;
        };
        let started = std::time::Instant::now();
        if let Err(fault) = guarded(|| unsafe { callback(ui) }) {
            slot.disable(format!("create_ui: {fault}"));
        }
        slot.note_duration(started.elapsed().as_millis(), "create_ui");
        true
    }

    /// `W2R_EVT_SHUTDOWN` to subscribers, then best-effort `on_unload` for every active plugin.
    pub fn shutdown(&self) {
        self.dispatch(W2R_EVT_SHUTDOWN, core::ptr::null());
        for slot in &self.slots {
            if !slot.is_active() {
                continue;
            }
            let Some(on_unload) = slot.table.get().on_unload else {
                continue;
            };
            if let Err(fault) = guarded(|| unsafe { on_unload() }) {
                slot.disable(format!("on_unload: {fault}"));
            }
        }
    }
}

/// Calls one plugin's `on_event` under the guard; a fault disables the plugin.
fn deliver(slot: &Slot, event: &W2rEvent) {
    let Some(on_event) = slot.table.get().on_event else {
        return;
    };
    let started = std::time::Instant::now();
    if let Err(fault) = guarded(|| unsafe { on_event(event) }) {
        slot.disable(format!("on_event({}): {fault}", event.id));
    }
    slot.note_duration(started.elapsed().as_millis(), "on_event");
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::State;
    use std::cell::RefCell;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct Lines(RefCell<Vec<String>>);
    impl Log for Lines {
        fn line(&self, level: u32, plugin: &str, msg: &str) {
            self.0
                .borrow_mut()
                .push(format!("[{level}] {plugin}: {msg}"));
        }
    }

    fn registry() -> (Registry, Rc<Lines>) {
        let log = Rc::new(Lines::default());
        (Registry::new(log.clone()), log)
    }

    fn table(
        on_load: unsafe extern "C" fn(*const W2rHost) -> W2rStatus,
        on_event: Option<unsafe extern "C" fn(*const W2rEvent)>,
    ) -> W2rPlugin {
        W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(on_load),
            on_event,
            on_ui: None,
            on_unload: None,
        }
    }

    unsafe fn subscribe_all(host: *const W2rHost) -> W2rStatus {
        let h = unsafe { &*host };
        unsafe { (h.subscribe)(h.token, W2R_EVT_LOBBY_ENTER) }
    }

    fn s(text: &str) -> W2rStr {
        W2rStr {
            ptr: text.as_ptr().cast(),
            len: text.len() as u32,
        }
    }

    #[test]
    fn null_entry_is_e_entry() {
        unsafe extern "C" fn entry(_: *const W2rHost) -> *const W2rPlugin {
            core::ptr::null()
        }
        let (mut reg, log) = registry();
        let err = reg.load_entry("p", entry).unwrap_err();
        assert_eq!(err.code, "E_ENTRY");
        assert!(matches!(reg.slot(0).state(), State::Disabled { .. }));
        assert_eq!(
            log.0.borrow()[0],
            "[3] p: disabled: E_ENTRY: w2r_plugin_main returned null"
        );
    }

    #[test]
    fn small_struct_size_is_e_entry() {
        unsafe extern "C" fn on_load(_: *const W2rHost) -> W2rStatus {
            W2R_OK
        }
        static T: W2rPlugin = W2rPlugin {
            struct_size: 8,
            on_load: Some(on_load),
            on_event: None,
            on_ui: None,
            on_unload: None,
        };
        unsafe extern "C" fn entry(_: *const W2rHost) -> *const W2rPlugin {
            &T
        }
        let (mut reg, _) = registry();
        let err = reg.load_entry("p", entry).unwrap_err();
        assert_eq!(err.code, "E_ENTRY");
        assert!(err.detail.contains("struct_size 8"));
    }

    #[test]
    fn missing_on_load_is_e_entry() {
        static T: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: None,
            on_event: None,
            on_ui: None,
            on_unload: None,
        };
        unsafe extern "C" fn entry(_: *const W2rHost) -> *const W2rPlugin {
            &T
        }
        let (mut reg, _) = registry();
        assert_eq!(
            reg.load_entry("p", entry).unwrap_err().detail,
            "on_load is null"
        );
    }

    #[test]
    fn nonzero_on_load_is_e_load() {
        unsafe extern "C" fn on_load(_: *const W2rHost) -> W2rStatus {
            -42
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
        let (mut reg, _) = registry();
        let err = reg.load_entry("p", entry).unwrap_err();
        assert_eq!(
            (err.code, err.detail.as_str()),
            ("E_LOAD", "on_load returned -42")
        );
        assert!(!reg.slot(0).is_active());
    }

    #[cfg(windows)]
    #[test]
    fn faulting_on_load_is_e_load_and_faulting_entry_is_e_entry() {
        unsafe extern "C" fn on_load(_: *const W2rHost) -> W2rStatus {
            let p: *mut u32 = std::hint::black_box(core::ptr::null_mut());
            unsafe { p.write_volatile(1) };
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
        unsafe extern "C" fn garbage_entry(_: *const W2rHost) -> *const W2rPlugin {
            std::hint::black_box(0x10 as *const W2rPlugin)
        }
        let (mut reg, _) = registry();
        let err = reg.load_entry("a", entry).unwrap_err();
        assert_eq!(err.code, "E_LOAD");
        assert!(err.detail.contains("access violation"), "{err}");
        let err = reg.load_entry("b", garbage_entry).unwrap_err();
        assert_eq!(err.code, "E_ENTRY");
        assert!(err.detail.contains("access violation"), "{err}");
    }

    #[test]
    fn events_reach_only_active_subscribers() {
        static CALLS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn on_load(host: *const W2rHost) -> W2rStatus {
            unsafe { subscribe_all(host) }
        }
        unsafe extern "C" fn on_event(ev: *const W2rEvent) {
            assert_eq!(unsafe { (*ev).id }, W2R_EVT_LOBBY_ENTER);
            CALLS.fetch_add(1, Ordering::Relaxed);
        }
        static T: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(on_load),
            on_event: Some(on_event),
            on_ui: None,
            on_unload: None,
        };
        unsafe extern "C" fn entry(_: *const W2rHost) -> *const W2rPlugin {
            &T
        }
        let (mut reg, _) = registry();
        let i = reg.load_entry("p", entry).unwrap();
        reg.dispatch(W2R_EVT_LOBBY_LEAVE, core::ptr::null()); // not subscribed
        reg.dispatch(W2R_EVT_LOBBY_ENTER, core::ptr::null());
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
        // Unsubscribe through the host table, as a plugin would.
        let h = unsafe { &*reg.slot(i).host() };
        assert_eq!(
            unsafe { (h.unsubscribe)(h.token, W2R_EVT_LOBBY_ENTER) },
            W2R_OK
        );
        reg.dispatch(W2R_EVT_LOBBY_ENTER, core::ptr::null());
        assert_eq!(CALLS.load(Ordering::Relaxed), 1);
    }

    #[cfg(windows)]
    #[test]
    fn a_faulting_plugin_is_disabled_and_the_others_keep_receiving() {
        static GOOD: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn on_load(host: *const W2rHost) -> W2rStatus {
            unsafe { subscribe_all(host) }
        }
        unsafe extern "C" fn crash(_: *const W2rEvent) {
            let p: *mut u32 = std::hint::black_box(core::ptr::null_mut());
            unsafe { p.write_volatile(1) };
        }
        unsafe extern "C" fn fine(_: *const W2rEvent) {
            GOOD.fetch_add(1, Ordering::Relaxed);
        }
        static BAD: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(on_load),
            on_event: Some(crash),
            on_ui: None,
            on_unload: None,
        };
        static OK: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(on_load),
            on_event: Some(fine),
            on_ui: None,
            on_unload: None,
        };
        unsafe extern "C" fn bad_entry(_: *const W2rHost) -> *const W2rPlugin {
            &BAD
        }
        unsafe extern "C" fn ok_entry(_: *const W2rHost) -> *const W2rPlugin {
            &OK
        }

        let (mut reg, log) = registry();
        reg.load_entry("bad", bad_entry).unwrap();
        reg.load_entry("good", ok_entry).unwrap();
        reg.dispatch(W2R_EVT_LOBBY_ENTER, core::ptr::null());
        reg.dispatch(W2R_EVT_LOBBY_ENTER, core::ptr::null());
        assert_eq!(GOOD.load(Ordering::Relaxed), 2);
        let State::Disabled { reason } = reg.find("bad").unwrap().state() else {
            panic!("bad still active")
        };
        assert!(
            reason.starts_with("on_event(1): access violation"),
            "{reason}"
        );
        assert!(reg.find("good").unwrap().is_active());
        let disabled_lines = log
            .0
            .borrow()
            .iter()
            .filter(|l| l.contains("disabled"))
            .count();
        assert_eq!(disabled_lines, 1);
    }

    #[cfg(windows)]
    #[test]
    fn settings_changed_reaches_only_its_subscribed_plugin_and_a_fault_disables_it() {
        static HEARD: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn listens(host: *const W2rHost) -> W2rStatus {
            let h = unsafe { &*host };
            unsafe { (h.subscribe)(h.token, W2R_EVT_SETTINGS_CHANGED) }
        }
        unsafe extern "C" fn deaf(_: *const W2rHost) -> W2rStatus {
            W2R_OK
        }
        unsafe extern "C" fn heard(event: *const W2rEvent) {
            assert_eq!(unsafe { (*event).id }, W2R_EVT_SETTINGS_CHANGED);
            HEARD.fetch_add(1, Ordering::Relaxed);
        }
        unsafe extern "C" fn crash(_: *const W2rEvent) {
            let p: *mut u32 = std::hint::black_box(core::ptr::null_mut());
            unsafe { p.write_volatile(1) };
        }
        static LISTENS: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(listens),
            on_event: Some(heard),
            on_ui: None,
            on_unload: None,
        };
        static DEAF: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(deaf),
            on_event: Some(heard),
            on_ui: None,
            on_unload: None,
        };
        static CRASHES: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(listens),
            on_event: Some(crash),
            on_ui: None,
            on_unload: None,
        };
        unsafe extern "C" fn listens_entry(_: *const W2rHost) -> *const W2rPlugin {
            &LISTENS
        }
        unsafe extern "C" fn deaf_entry(_: *const W2rHost) -> *const W2rPlugin {
            &DEAF
        }
        unsafe extern "C" fn crashes_entry(_: *const W2rHost) -> *const W2rPlugin {
            &CRASHES
        }

        let (mut reg, _) = registry();
        reg.load_entry("listens", listens_entry).unwrap();
        reg.load_entry("deaf", deaf_entry).unwrap();
        reg.load_entry("crashes", crashes_entry).unwrap();
        reg.settings_changed(reg.find("deaf").unwrap());
        assert_eq!(HEARD.load(Ordering::Relaxed), 0, "not subscribed");
        reg.settings_changed(reg.find("listens").unwrap());
        assert_eq!(HEARD.load(Ordering::Relaxed), 1);
        reg.settings_changed(reg.find("crashes").unwrap());
        let State::Disabled { reason } = reg.find("crashes").unwrap().state() else {
            panic!("still active")
        };
        assert!(reason.starts_with("on_event(9): access violation"), "{reason}");
        reg.settings_changed(reg.find("crashes").unwrap());
        assert!(reg.find("listens").unwrap().is_active());
    }

    #[test]
    fn host_log_and_subscribe_validate_their_arguments() {
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
        let (mut reg, log) = registry();
        let i = reg.load_entry("p", entry).unwrap();
        let h = unsafe { &*reg.slot(i).host() };
        assert_eq!(h.struct_size, W2R_HOST_SETTINGS_DECLARE_SIZE);
        assert_eq!(h.abi_version, W2R_ABI_VERSION);

        let long = "x".repeat(W2R_MAX_LOG_BYTES as usize + 1);
        let max = "y".repeat(W2R_MAX_LOG_BYTES as usize);
        unsafe {
            assert_eq!((h.log)(h.token, W2R_LOG_INFO, s("hi")), W2R_OK);
            assert_eq!((h.log)(h.token, 4, s("hi")), W2R_E_INVALID);
            assert_eq!((h.log)(h.token, W2R_LOG_INFO, s(&long)), W2R_E_LIMIT);
            assert_eq!((h.log)(h.token, W2R_LOG_INFO, s(&max)), W2R_OK);
            assert_eq!(
                (h.log)(
                    h.token,
                    W2R_LOG_INFO,
                    W2rStr {
                        ptr: core::ptr::null(),
                        len: 3
                    }
                ),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.log)(
                    h.token,
                    W2R_LOG_INFO,
                    W2rStr {
                        ptr: core::ptr::null(),
                        len: 0
                    }
                ),
                W2R_OK
            );
            let bad_utf8 = [0xFFu8, 0xFE];
            assert_eq!(
                (h.log)(
                    h.token,
                    W2R_LOG_INFO,
                    W2rStr {
                        ptr: bad_utf8.as_ptr().cast(),
                        len: 2
                    }
                ),
                W2R_E_INVALID
            );
            assert_eq!(
                (h.log)(core::ptr::null_mut(), W2R_LOG_INFO, s("hi")),
                W2R_E_INVALID
            );

            assert_eq!((h.subscribe)(h.token, 0), W2R_E_NOT_FOUND);
            assert_eq!(
                (h.subscribe)(h.token, W2R_EVT_SHUTDOWN + 1),
                W2R_E_NOT_FOUND
            );
            assert_eq!((h.subscribe)(h.token, W2R_EVT_MATCH_START), W2R_OK);
            assert_eq!((h.unsubscribe)(h.token, 0), W2R_E_NOT_FOUND);
            assert_eq!((h.chat_send)(h.token, s("x")), W2R_E_DENIED);
        }
        assert!(reg.slot(i).subscribed(W2R_EVT_MATCH_START));
        let lines = log.0.borrow();
        assert_eq!(lines[0], "[1] p: hi");
        assert_eq!(lines[1], format!("[1] p: {max}"));
        assert_eq!(lines[2], "[1] p: ");
        assert_eq!(
            lines[3],
            "[2] p: chat_send: capability chat.write not approved"
        );
        assert_eq!(lines.len(), 4);
    }

    #[test]
    fn shutdown_calls_on_unload_for_active_plugins_only() {
        static UNLOADS: AtomicUsize = AtomicUsize::new(0);
        unsafe extern "C" fn on_load(_: *const W2rHost) -> W2rStatus {
            W2R_OK
        }
        unsafe extern "C" fn refuse(_: *const W2rHost) -> W2rStatus {
            -1
        }
        unsafe extern "C" fn on_unload() {
            UNLOADS.fetch_add(1, Ordering::Relaxed);
        }
        static A: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(on_load),
            on_event: None,
            on_ui: None,
            on_unload: Some(on_unload),
        };
        static B: W2rPlugin = W2rPlugin {
            struct_size: W2R_PLUGIN_V1_SIZE,
            on_load: Some(refuse),
            on_event: None,
            on_ui: None,
            on_unload: Some(on_unload),
        };
        unsafe extern "C" fn a(_: *const W2rHost) -> *const W2rPlugin {
            &A
        }
        unsafe extern "C" fn b(_: *const W2rHost) -> *const W2rPlugin {
            &B
        }
        let (mut reg, _) = registry();
        reg.load_entry("a", a).unwrap();
        reg.load_entry("b", b).unwrap_err();
        reg.shutdown();
        assert_eq!(UNLOADS.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn load_error_display_matches_the_spec_line_shape() {
        let e = LoadError {
            code: "E_LOAD",
            detail: "on_load returned -1".into(),
        };
        assert_eq!(e.to_string(), "E_LOAD: on_load returned -1");
        let _ = table; // keep the helper for future tests
    }
}
