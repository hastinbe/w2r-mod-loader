//! The host's main-thread state: the plugin registry, lobby presence, the handshake and the
//! user's `state.json` and `host.json`. Created on the first frame the game draws after
//! startup and driven by every `BeginNamedWindow` call from then on. Only the game's main
//! thread touches the session, which is why a `thread_local` holds it; messages that arrive
//! elsewhere wait in a queue for the next frame.

use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use w2r_abi::{
    W2R_EVT_LOBBY_ENTER, W2R_EVT_LOBBY_LEAVE, W2R_EVT_MATCH_END, W2R_EVT_MATCH_START,
    W2R_LOG_ERROR, W2R_LOG_INFO, W2R_LOG_WARN,
};
use windows_sys::Win32::System::Threading::GetCurrentThreadId;

use crate::chat::{self, Chat, NoChat};
use crate::dispatch::Registry;
use crate::entry;
use crate::frame;
use crate::guard::Fault;
use crate::handshake::{self, Handshake, Out};
use crate::host::{Game, Log, Services, State as SlotState};
use crate::hostcfg::{self, HostConfig};
use crate::hostsettings;
use crate::lobby::{self, Event, Lobby};
use crate::lobbylayout::{self, ModMark};
use crate::lobbypolicy::{self, Locks};
use crate::manifest::Build;
#[cfg(target_arch = "x86")]
use crate::options;
use crate::peer::{self, NoTransport, PluginMessage, Presence, Roster, Transport};
use crate::plugins;
use crate::settings::{self, Editor, Owner, SettingDef};
use crate::state::{self, State};
use crate::store::Val;
use crate::symbols::Symbols;
use crate::target::GAME_VERSION;
use crate::ui::{self, Action, PluginRow, Widgets};

struct Session {
    registry: Registry,
    lobby: Rc<RefCell<Lobby>>,
    /// As loaded at startup, plus the user's toggles (which take effect at the next launch).
    state: State,
    root: PathBuf,
    config: HostConfig,
    fingerprint: [u8; 4],
    /// The plugins in the fingerprint.
    simulation: Vec<String>,
    /// Each plugin's name, author and description, read once from the manifests.
    plugin_info: BTreeMap<String, plugins::PluginInfo>,
    /// The plugins `state.json` had enabled at startup.
    enabled_at_start: BTreeSet<String>,
    net: Rc<Net>,
    /// The options screen's settings: the game's, the host's and every plugin's
    /// (docs/settings-screen.md). Only the x86 screen draws it.
    #[cfg_attr(not(target_arch = "x86"), allow(dead_code))]
    editor: Editor,
    /// The game's own settings table, when it passed validation.
    #[cfg(target_arch = "x86")]
    native: Option<crate::nativesettings::game::Bridge>,
    #[cfg(target_arch = "x86")]
    nk: Option<options::Nk>,
}

/// One line for the title screen: how many plugins are running and how many are not.
pub fn plugin_summary() -> Option<String> {
    SESSION.with(|cell| {
        let slot = cell.try_borrow().ok()?;
        let s = slot.as_ref()?;
        let disabled = (0..s.registry.len())
            .filter(|&i| matches!(s.registry.slot(i).state(), SlotState::Disabled { .. }))
            .count();
        let active = s.registry.len() - disabled;
        let failed = disabled + s.enabled_at_start.len().saturating_sub(s.registry.len());
        let noun = |n: usize| if n == 1 { "plugin" } else { "plugins" };
        Some(if failed == 0 {
            format!("{active} {} loaded", noun(active))
        } else {
            format!("{active} {} loaded, {failed} failed", noun(active))
        })
    })
}

/// The handshake and the game's net session. Separate from [`Session`] because the packet
/// hook may hand a message in while the session is borrowed (the game can poll packets from
/// inside a plugin callback); the handshake must still see it.
struct Net {
    handshake: RefCell<Handshake>,
    transport: Rc<dyn Transport>,
}

impl Net {
    fn idle() -> Net {
        Net {
            handshake: RefCell::new(Handshake::new(handshake::Local {
                fingerprint: [0; 4],
            })),
            transport: Rc::new(NoTransport),
        }
    }

    /// Carries out what the handshake asked for. No handshake borrow is live here: the
    /// transport calls run game code under the guard.
    fn run(&self, outs: Vec<Out>) {
        for out in outs {
            let failed = match out {
                Out::Broadcast(m) => (!self.transport.broadcast(&m)).then_some("broadcast"),
                Out::SendTo(slot, m) => (!self.transport.send_to(slot, &m)).then_some("send"),
                Out::Log(level, msg) => {
                    StartupLog.line(level, "handshake", &msg);
                    None
                }
            };
            if let Some(what) = failed {
                StartupLog.line(W2R_LOG_WARN, "handshake", &format!("{what} failed"));
            }
        }
    }
}

/// The roster `peer_send` and peer delivery consult (docs/abi.md).
struct HandshakeRoster(Rc<Net>);

impl Roster for HandshakeRoster {
    fn announced(&self, slot: u32) -> bool {
        self.0
            .handshake
            .try_borrow()
            .is_ok_and(|h| h.announced(slot))
    }
    fn any_announced(&self) -> bool {
        self.0
            .handshake
            .try_borrow()
            .is_ok_and(|h| h.any_announced())
    }
}

thread_local! {
    static SESSION: RefCell<Option<Session>> = const { RefCell::new(None) };
    /// Set while `start` runs, so a nested frame does not start a second session.
    static STARTING: Cell<bool> = const { Cell::new(false) };
    /// The session's [`Net`], reachable without the session borrow.
    static NET: RefCell<Option<Rc<Net>>> = const { RefCell::new(None) };
}

/// A fault in host code under one of the host's own hooks (not a plugin's). Logged for the
/// first few and then sparsely: a hook runs every frame, and the same bug would repeat.
pub fn host_fault(site: &str, fault: &Fault) {
    static COUNT: AtomicU32 = AtomicU32::new(0);
    let n = COUNT.fetch_add(1, Ordering::Relaxed).saturating_add(1);
    if n <= 5 || n % 1000 == 0 {
        StartupLog.line(
            W2R_LOG_ERROR,
            "host",
            &format!("host fault in {site}: {fault} ({n} so far)"),
        );
    }
}

/// Routes to the startup log; the registry needs an `Rc<dyn Log>`.
pub(crate) struct StartupLog;

impl Log for StartupLog {
    fn line(&self, level: u32, plugin: &str, msg: &str) {
        if let Some(s) = entry::startup() {
            s.log.line(level, plugin, msg);
        }
    }
}

/// Lobby presence for the services, from the shared detector.
struct LobbyPresence(Rc<RefCell<Lobby>>);

impl Presence for LobbyPresence {
    fn in_lobby(&self) -> bool {
        self.0.borrow().in_lobby(chat::now_ms())
    }
}

fn on_main_thread() -> bool {
    let current = unsafe { GetCurrentThreadId() };
    current == frame::main_thread_id()
}

/// A nonce for one stay in a lobby: unique enough to tell our own broadcast from a peer's and
/// a rejoin from the last stay. Not a secret (docs/peer-protocol.md).
fn nonce() -> u32 {
    use std::hash::{BuildHasher, Hasher, RandomState};
    let mut h = RandomState::new().build_hasher();
    h.write_u64(chat::now_ms());
    h.write_u32(std::process::id());
    let n = h.finish() as u32;
    if n == 0 { 1 } else { n }
}

/// The game is starting a match (`ResetMapMessageSlots`): tell the plugins that subscribed.
pub fn on_match_start() {
    if !on_main_thread() {
        return;
    }
    #[cfg(target_arch = "x86")]
    crate::matchend::rearm();
    #[cfg(target_arch = "x86")]
    crate::replayrec::rearm();
    #[cfg(target_arch = "x86")]
    crate::alliancesscreen::on_match_start();
    #[cfg(target_arch = "x86")]
    crate::ping::on_match_start();
    #[cfg(target_arch = "x86")]
    crate::camerashare::on_match_start();
    SESSION.with(|cell| {
        // Busy means a plugin callback is on the stack and drew something; the event is lost
        // rather than delivered into a half-finished callback.
        let Ok(slot) = cell.try_borrow() else {
            return;
        };
        if let Some(session) = slot.as_ref() {
            session
                .registry
                .dispatch(W2R_EVT_MATCH_START, core::ptr::null());
        }
    });
}

/// The local player's match is over (`CheckMatchEnd` wrote their result): tell the plugins that
/// subscribed.
pub fn on_match_end() {
    if !on_main_thread() {
        return;
    }
    SESSION.with(|cell| {
        let Ok(slot) = cell.try_borrow() else {
            return;
        };
        if let Some(session) = slot.as_ref() {
            session
                .registry
                .dispatch(W2R_EVT_MATCH_END, core::ptr::null());
        }
    });
}

/// One `BeginNamedWindow` call on the main thread. `name` is the window the game is starting,
/// if it gave one.
pub fn on_window(name: Option<&str>) {
    #[cfg(target_arch = "x86")]
    {
        crate::matchend::note_frame();
        crate::replayplay::poll();
        crate::ingamekick::poll();
        crate::ping::poll();
        crate::camerashare::poll();
    }
    let now = chat::now_ms();
    ensure_started();
    // Queued messages first, at the top level only: a nested call (a plugin callback made the
    // game draw a window, which lands back here) sees the session borrowed and must not touch
    // it.
    if SESSION.with(|c| c.try_borrow().is_ok()) {
        drain_queue();
    }
    SESSION.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return;
        };
        let Some(session) = slot.as_mut() else {
            return;
        };
        let event = {
            let mut lobby = session.lobby.borrow_mut();
            match name {
                Some(n) => lobby.on_window(n, now),
                None => lobby.on_frame(now),
            }
        };
        match event {
            Some(Event::Enter) => {
                session.net.handshake.borrow_mut().enter(nonce());
                #[cfg(target_arch = "x86")]
                crate::matchend::on_lobby_enter();
                #[cfg(target_arch = "x86")]
                crate::replayplay::on_lobby_enter();
                session
                    .registry
                    .dispatch(W2R_EVT_LOBBY_ENTER, core::ptr::null());
            }
            Some(Event::Leave) => {
                session.net.handshake.borrow_mut().leave();
                session
                    .registry
                    .dispatch(W2R_EVT_LOBBY_LEAVE, core::ptr::null());
            }
            None => {}
        }
        if name == Some(lobby::WINDOW) {
            let net = session.net.clone();
            let chosen = Locks {
                teams: session.config.lock_team_changes,
                slots: session.config.lock_slot_swaps,
            };
            lobbypolicy::with(|p| p.frame(now, net.transport.is_host(), chosen));
            #[cfg(target_arch = "x86")]
            crate::replayplay::lobby_frame();
            while let Some(e) = lobbypolicy::with(|p| p.next_event()) {
                StartupLog.line(
                    W2R_LOG_INFO,
                    "lobbypolicy",
                    &format!(
                        "{} {}",
                        if e.teams {
                            "team changes"
                        } else {
                            "slot swaps"
                        },
                        if e.locked { "LOCKED" } else { "unlocked" }
                    ),
                );
                #[cfg(target_arch = "x86")]
                crate::lobbyscreen::push_notice(
                    lobbylayout::lock_notice(e.teams, e.locked).to_owned(),
                );
            }
            #[cfg(target_arch = "x86")]
            crate::mapdownload::frame(
                now,
                session.config.map_sharing,
                !session.config.auto_download,
            );
            if net.transport.is_host() && net.transport.connected() {
                lobbypolicy::frame_work();
            }
            if net.transport.is_host()
                && net.transport.connected()
                && lobbypolicy::with(|p| p.take_send(now))
            {
                let locks = lobbypolicy::with(|p| p.locks());
                if let Some(msg) = lobbypolicy::encode(locks, &lobbypolicy::host_password()) {
                    for slot in 1..peer::MAX_SLOTS {
                        let announced = net.handshake.try_borrow().is_ok_and(|h| h.announced(slot));
                        if announced && !net.transport.send_to(slot, &msg) {
                            StartupLog.line(W2R_LOG_WARN, "lobbypolicy", "send failed");
                        }
                    }
                }
            }
            let f = handshake::Frame {
                now,
                connected: net.transport.connected(),
                is_host: net.transport.is_host(),
                lobby_slots: net.transport.lobby_slots(),
            };
            let outs = match net.handshake.try_borrow_mut() {
                Ok(mut h) => {
                    #[cfg(target_arch = "x86")]
                    h.set_observer(crate::observer::available());
                    h.frame(f, || net.transport.peers())
                }
                Err(_) => Vec::new(),
            };
            net.run(outs);
            #[cfg(target_arch = "x86")]
            {
                let everyone = net.handshake.try_borrow().is_ok_and(|h| h.everyone_has());
                crate::observer::frame(
                    everyone,
                    net.transport.is_host() && net.transport.connected(),
                );
            }
            // The people who run the mod, kept for the match that follows (the roster is cleared
            // when the lobby is left, and pings travel during the match).
            if let Ok(h) = net.handshake.try_borrow() {
                let mask = (0..peer::MAX_SLOTS)
                    .filter(|&n| h.announced(n))
                    .fold(0u8, |m, n| m | 1 << n);
                crate::ping::note_roster(mask);
                // Tell the other mod users how good our links are and look for a bad one among
                // everybody's (docs/link-sharing.md). Only while connected to a lobby.
                #[cfg(target_arch = "x86")]
                if net.transport.connected()
                    && let Some(lobby_slots) = net.transport.lobby_slots()
                {
                    crate::linkshare::frame(
                        now,
                        mask,
                        h.self_slot(),
                        lobby_slots,
                        &crate::netquality::game::latest_by_net(),
                        |slot, msg| net.transport.send_to(slot, msg),
                    );
                }
            }
        }
    });
}

/// Builds the session on the first frame. It is built *outside* the cell's borrow: loading a
/// plugin runs its `DllMain` and `on_load`, either of which may draw a window and land back in
/// `on_window`, and a fault in there is caught by the frame hook's guard, which abandons this
/// frame; a live `RefMut` would then leave the cell borrowed for the rest of the session.
fn ensure_started() {
    let needed = SESSION.with(|c| c.try_borrow().map(|s| s.is_none()).unwrap_or(false));
    if !needed || STARTING.replace(true) {
        return;
    }
    let session = start();
    SESSION.with(|c| {
        if let Ok(mut slot) = c.try_borrow_mut() {
            *slot = Some(session);
        }
    });
    STARTING.set(false);
}

// ---- inbound messages ---------------------------------------------------------------------

/// A W2R message taken off the wire, owned so it can wait.
enum Inbound {
    Plugin {
        sender: u32,
        tag: [u8; 4],
        payload: Vec<u8>,
    },
    Handshake {
        sender: u32,
        body: Vec<u8>,
    },
}

/// Messages that could not be handled where they arrived (another thread, or the session was
/// busy) wait here for the next frame. Bounded: a flood drops the excess, counted.
const QUEUE_CAP: usize = 256;
static QUEUE: Mutex<VecDeque<Inbound>> = Mutex::new(VecDeque::new());
static QUEUE_DROPPED: AtomicU32 = AtomicU32::new(0);

fn enqueue(item: Inbound) {
    static OFF_THREAD: std::sync::Once = std::sync::Once::new();
    if !on_main_thread() {
        OFF_THREAD.call_once(|| {
            StartupLog.line(
                W2R_LOG_INFO,
                "host",
                "peer messages arrive off the main thread; queued for the next frame",
            );
        });
    }
    let mut q = QUEUE.lock().unwrap_or_else(|e| e.into_inner());
    if q.len() >= QUEUE_CAP {
        drop(q);
        let n = QUEUE_DROPPED.fetch_add(1, Ordering::Relaxed) + 1;
        if n == 1 || n % 1000 == 0 {
            StartupLog.line(
                W2R_LOG_WARN,
                "host",
                &format!("inbound queue full ({QUEUE_CAP}); dropped a message ({n} so far)"),
            );
        }
        return;
    }
    q.push_back(item);
}

/// Hands one message on. False when the receiver is busy right now (the caller queues it).
/// Main thread only.
fn deliver_now(item: &Inbound) -> bool {
    match item {
        Inbound::Plugin {
            sender,
            tag,
            payload,
        } => SESSION.with(|cell| {
            let Ok(slot) = cell.try_borrow() else {
                return false;
            };
            if let Some(session) = slot.as_ref() {
                session
                    .registry
                    .dispatch_peer(*sender, PluginMessage { tag: *tag, payload });
            }
            true
        }),
        Inbound::Handshake { sender, body } => {
            let Some(net) = NET.with(|n| n.borrow().clone()) else {
                // No session yet: nobody is in a lobby, so there is nothing to answer.
                return true;
            };
            let outs = match net.handshake.try_borrow_mut() {
                Ok(mut h) => h.receive(chat::now_ms(), *sender, body),
                Err(_) => return false,
            };
            net.run(outs);
            true
        }
    }
}

/// Main thread, top level: delivers what waited. Bounded per frame so a flood from another
/// thread cannot keep this loop running.
fn drain_queue() {
    for _ in 0..QUEUE_CAP {
        let item = QUEUE.lock().unwrap_or_else(|e| e.into_inner()).pop_front();
        let Some(item) = item else {
            return;
        };
        if !deliver_now(&item) {
            QUEUE
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push_front(item);
            return;
        }
    }
}

/// A plugin peer message taken off the wire. Delivered now when possible, otherwise on the
/// next frame.
pub fn deliver_peer(sender: u32, msg: PluginMessage<'_>) {
    let item = Inbound::Plugin {
        sender,
        tag: msg.tag,
        payload: msg.payload.to_vec(),
    };
    if !on_main_thread() || !deliver_now(&item) {
        enqueue(item);
    }
}

/// A handshake message (`'H'`) taken off the wire; `body` is everything after the envelope.
pub fn deliver_handshake(sender: u32, body: &[u8]) {
    let item = Inbound::Handshake {
        sender,
        body: body.to_vec(),
    };
    if !on_main_thread() || !deliver_now(&item) {
        enqueue(item);
    }
}

// ---- UI -----------------------------------------------------------------------------------

/// Runs `f` with the game's UI functions, if the session has them.
#[cfg(target_arch = "x86")]
pub fn with_nk(f: impl FnOnce(&options::Nk)) {
    let nk = SESSION.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|s| s.as_ref().and_then(|s| s.nk))
    });
    if let Some(nk) = nk {
        f(&nk);
    }
}

/// The chat line `/plugins` is replaced by: the plugins that are running, with versions. `None` before the session exists.
pub fn plugins_chat_line() -> Option<String> {
    SESSION.with(|cell| {
        let slot = cell.try_borrow().ok()?;
        let session = slot.as_ref()?;
        let entries: Vec<String> = session
            .state
            .plugins
            .iter()
            .filter(|p| !crate::plugincmd::is_dev_only(&p.id))
            .filter(|p| {
                session
                    .registry
                    .find(&p.id)
                    .is_some_and(|s| matches!(s.state(), SlotState::Active))
            })
            .map(|p| format!("{} {}", p.id, state_version(p)))
            .collect();
        Some(crate::plugincmd::line(&entries))
    })
}

/// The options screen's settings, built once the plugins have loaded: the game's own (when its
/// table validated), the host's, then each active plugin's declarations. A plugin that named no
/// category gets one named after it, and every plugin setting carries the plugin's name for its
/// tooltip.
fn settings_registry(
    native: Option<&[crate::nativesettings::Entry]>,
    plugins: &Registry,
    names: &BTreeMap<String, plugins::PluginInfo>,
) -> settings::Registry {
    let mut r = settings::Registry::new();
    if let Some(entries) = native {
        crate::nativesettings::register(&mut r, entries);
    }
    for d in hostsettings::defs() {
        let v = settings::Value::Int(d.default_int);
        r.register(d, v);
    }
    for slot in plugins.slots().filter(|s| s.is_active()) {
        let name = names
            .get(&slot.id)
            .map_or_else(|| slot.id.clone(), |i| i.name.clone());
        for mut d in slot.declared() {
            if d.category.is_empty() {
                d.category = name.clone();
            }
            d.source = Some(name.clone());
            let v = d.default_value();
            r.register(d, v);
        }
    }
    r
}

/// Re-applies `host.json` to the parts of the host that read it, after the options screen
/// changed it. Each of these is cheap and idempotent, so all of them run.
// Called only by the x86 options screen; kept on every target so the tests below run on all.
#[cfg_attr(not(target_arch = "x86"), allow(dead_code))]
fn apply_host_config(config: &HostConfig, net: &Net) {
    if let Ok(mut h) = net.handshake.try_borrow_mut() {
        h.set_map_sharing(config.map_sharing);
    }
    #[cfg(target_arch = "x86")]
    {
        crate::replayrec::set_enabled(config.record_replays);
        crate::createscreen::set_auto_game_name(config.auto_game_name);
        crate::apm::configure(config);
        crate::netpanel::configure(config);
        crate::ping::configure(config);
        crate::settingsscreen::set_hide_arrows(config.hide_scrollbar_arrows);
    }
}

/// Where the screen's host and plugin values live: `host.json`, `sharing.json` and each plugin's
/// `config\<id>.json`.
// Built only by the x86 options screen; kept on every target so the tests below run on all.
#[cfg_attr(not(target_arch = "x86"), allow(dead_code))]
struct SessionOwners<'a> {
    config: &'a mut HostConfig,
    root: &'a Path,
    plugins: &'a Registry,
    net: &'a Net,
}

impl settings::Owners for SessionOwners<'_> {
    fn load(&self, def: &SettingDef) -> Option<settings::Value> {
        match &def.owner {
            Owner::Host if def.key == hostsettings::SHARE_MATCH_DATA => Some(
                settings::Value::Int(i64::from(crate::uploader::enabled())),
            ),
            Owner::Host => hostsettings::read(self.config, &def.key).map(settings::Value::Int),
            Owner::Plugin(id) => {
                match (self.plugins.find(id)?.stored_setting(&def.key)?, def.kind) {
                    (Val::Str(v), k) if k.is_text() => Some(settings::Value::Str(v)),
                    (Val::Int(n), k) if !k.is_text() => Some(settings::Value::Int(n)),
                    // The wrong type in the file: the plugin wrote something else under the key.
                    _ => None,
                }
            }
            Owner::Game => None,
        }
    }

    fn store(&mut self, changed: &[(&SettingDef, &settings::Value)]) {
        let mut host_changed = false;
        let mut notify: Vec<&str> = Vec::new();
        for (d, v) in changed {
            match &d.owner {
                Owner::Host if d.key == hostsettings::SHARE_MATCH_DATA => {
                    let on = v.int() != 0;
                    let line = match crate::uploader::set_enabled(self.root, on) {
                        Ok(()) => format!("share_match_data {}", if on { "on" } else { "off" }),
                        Err(e) => format!("cannot write sharing.json: {e}"),
                    };
                    StartupLog.line(W2R_LOG_INFO, "host", &line);
                }
                Owner::Host => {
                    if hostsettings::write(self.config, &d.key, v.int()) {
                        host_changed = true;
                        StartupLog.line(
                            W2R_LOG_INFO,
                            "host",
                            &format!("{} = {}", d.key, v.int()),
                        );
                    }
                }
                Owner::Plugin(id) => {
                    let Some(slot) = self.plugins.find(id) else {
                        continue;
                    };
                    let value = match v {
                        settings::Value::Int(n) => Val::Int(*n),
                        settings::Value::Str(t) => Val::Str(t.clone()),
                    };
                    if let Err(e) = slot.store_setting(&d.key, value) {
                        slot.log(W2R_LOG_ERROR, &e);
                    }
                    if !notify.contains(&id.as_str()) {
                        notify.push(id);
                    }
                }
                Owner::Game => {}
            }
        }
        if host_changed {
            if let Err(e) = hostcfg::write(self.root, self.config) {
                StartupLog.line(
                    W2R_LOG_ERROR,
                    "host",
                    &format!("cannot write {}: {e}", hostcfg::FILE),
                );
            }
            apply_host_config(self.config, self.net);
        }
        // After every value is stored, so a plugin reading one setting on the event sees the
        // others too.
        for id in notify {
            if let Some(slot) = self.plugins.find(id) {
                self.plugins.settings_changed(slot);
            }
        }
    }
}

/// The plugin manager's rows: every plugin `state.json` lists, with what it says about itself and
/// what the host did with it this session.
fn plugin_rows(
    state: &State,
    registry: &Registry,
    info: &BTreeMap<String, plugins::PluginInfo>,
    enabled_at_start: &BTreeSet<String>,
) -> Vec<PluginRow> {
    state
        .plugins
        .iter()
        .map(|p| {
            let about = info.get(&p.id);
            let launched = enabled_at_start.contains(&p.id);
            let (running, slow, error) = match registry.find(&p.id) {
                Some(slot) => match slot.state() {
                    SlotState::Active => (true, slot.is_slow(), None),
                    SlotState::Disabled { reason } => (false, false, Some(reason)),
                },
                // Enabled at launch but refused before it was loaded.
                None if launched => (
                    false,
                    false,
                    Some("not loaded; w2r.log says why".to_owned()),
                ),
                None => (false, false, None),
            };
            PluginRow {
                id: p.id.clone(),
                name: about.map_or_else(|| p.id.clone(), |a| a.name.clone()),
                version: state_version(p),
                author: about.map(|a| a.author.clone()),
                description: about.map(|a| a.description.clone()),
                enabled: p.enabled,
                enabled_at_launch: launched,
                running,
                slow,
                error,
            }
        })
        .collect()
}

/// Draws the plugin manager's list with `w` and saves the user's enable toggles to state.json.
/// Called from the main menu's plugin popup.
pub fn draw_plugin_list(w: &dyn Widgets) {
    SESSION.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return;
        };
        let Some(s) = slot.as_mut() else {
            return;
        };
        let mut rows = plugin_rows(&s.state, &s.registry, &s.plugin_info, &s.enabled_at_start);
        let plugins = &s.registry;
        let ids: Vec<String> = rows.iter().map(|r| r.id.clone()).collect();
        let action = ui::draw_plugin_list(w, s.fingerprint, &s.simulation, &mut rows, |w, i| {
            if let Some(slot) = plugins.find(&ids[i]) {
                ui::with_owned_frame(w, &ids[i], |table| plugins.call_on_ui(slot, table));
            }
        });
        if let Action::Toggle(i) = action {
            let row = &rows[i];
            s.state.set_enabled(&row.id, row.enabled);
            let path = s.root.join("config").join("state.json");
            match state::write(&path, &s.state) {
                Ok(()) => StartupLog.line(
                    W2R_LOG_INFO,
                    "host",
                    &format!(
                        "{} {} for the next launch",
                        row.id,
                        if row.enabled { "enabled" } else { "disabled" }
                    ),
                ),
                Err(e) => StartupLog.line(
                    W2R_LOG_ERROR,
                    "host",
                    &format!("cannot write state.json: {e}"),
                ),
            }
        }
    });
}

/// What the options screen draws with: the session's parts, borrowed for one frame.
#[cfg(target_arch = "x86")]
struct SessionScreen<'a> {
    editor: &'a mut Editor,
    native: &'a mut dyn settings::Native,
    owners: SessionOwners<'a>,
}

#[cfg(target_arch = "x86")]
impl crate::settingsscreen::ScreenHost for SessionScreen<'_> {
    fn parts(&mut self) -> (&mut Editor, &mut dyn settings::Native, &mut dyn settings::Owners) {
        (&mut *self.editor, &mut *self.native, &mut self.owners)
    }
}

/// Draws the options screen for `variant`. False when there is no session or the screen is not
/// available, so the hook falls back to the game's own options screen.
#[cfg(target_arch = "x86")]
pub fn draw_settings_screen(variant: i32) -> bool {
    if !crate::settingsscreen::available() {
        return false;
    }
    SESSION.with(|cell| {
        let Ok(mut slot) = cell.try_borrow_mut() else {
            return false;
        };
        let Some(s) = slot.as_mut() else {
            return false;
        };
        let mut none = settings::NoNative;
        let native: &mut dyn settings::Native = match s.native.as_mut() {
            Some(bridge) => bridge,
            None => &mut none,
        };
        let mut screen = SessionScreen {
            editor: &mut s.editor,
            native,
            owners: SessionOwners {
                config: &mut s.config,
                root: &s.root,
                plugins: &s.registry,
                net: &s.net,
            },
        };
        crate::settingsscreen::draw(variant, &mut screen)
    })
}

/// What the map-sharing code needs from the handshake and the game's network, without the
/// session itself (which may be borrowed).
#[cfg(target_arch = "x86")]
pub(crate) struct NetView(Rc<Net>);

#[cfg(target_arch = "x86")]
pub(crate) fn net_view() -> Option<NetView> {
    NET.with(|n| n.borrow().clone().map(NetView))
}

#[cfg(target_arch = "x86")]
impl NetView {
    pub(crate) fn is_host(&self) -> bool {
        self.0.transport.is_host()
    }

    pub(crate) fn connected(&self) -> bool {
        self.0.transport.connected()
    }

    /// The player announced the handshake: they run the mod.
    pub(crate) fn peer_runs_mod(&self, slot: u32) -> bool {
        self.0
            .handshake
            .try_borrow()
            .is_ok_and(|h| h.announced(slot))
    }

    pub(crate) fn peer_shares_maps(&self, slot: u32) -> bool {
        self.0
            .handshake
            .try_borrow()
            .is_ok_and(|h| h.peer_shares_maps(slot))
    }

    pub(crate) fn send_to(&self, slot: u32, bytes: &[u8]) -> bool {
        self.0.transport.send_to(slot, bytes)
    }

    pub(crate) fn kick(&self, slot: u32) -> bool {
        self.0.transport.kick(slot)
    }

    /// The lobby slot (0 to 7) a net slot sits in, when the game says.
    pub(crate) fn lobby_slot_of(&self, net_slot: u32) -> Option<u8> {
        self.0
            .transport
            .lobby_slots()
            .and_then(|s| s.get(net_slot as usize).copied().flatten())
    }
}

/// The host's current lock choice, `(teams, slots)`, for the create screen's buttons.
pub fn lock_choice() -> Option<(bool, bool)> {
    SESSION.with(|cell| {
        let slot = cell.try_borrow().ok()?;
        let s = slot.as_ref()?;
        Some((s.config.lock_team_changes, s.config.lock_slot_swaps))
    })
}

/// Stores the lock choice in `host.json` (applied when hosting, see `on_window`).
pub fn set_lock_choice(teams: bool, slots: bool) {
    SESSION.with(|cell| {
        if let Ok(mut slot) = cell.try_borrow_mut()
            && let Some(s) = slot.as_mut()
        {
            save_locks(s, teams, slots);
        }
    });
}

/// `(map_sharing, ask)` from `host.json`: whether map sharing is on, and whether a missing map
/// asks first (`auto_download` off).
pub fn map_sharing_settings() -> (bool, bool) {
    SESSION.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|s| {
                s.as_ref()
                    .map(|s| (s.config.map_sharing, !s.config.auto_download))
            })
            .unwrap_or((false, true))
    })
}

/// Whether any active plugin exports `w2r_map_draw`.
pub fn map_draw_present() -> bool {
    SESSION.with(|cell| {
        cell.try_borrow().ok().is_some_and(|s| {
            s.as_ref().is_some_and(|s| s.registry.slots().any(|slot| slot.map_draw().is_some()))
        })
    })
}

/// Calls every plugin's `w2r_map_draw` with `draw`. Main thread, from the frame hook.
pub fn draw_map(draw: &w2r_abi::W2rMapDraw) {
    SESSION.with(|cell| {
        let Ok(session) = cell.try_borrow() else { return };
        let Some(session) = session.as_ref() else { return };
        for slot in session.registry.slots() {
            session.registry.call_map_draw(slot, draw);
        }
    });
}

/// Whether any active plugin has registered a Create-screen callback.
pub fn create_ui_present() -> bool {
    SESSION.with(|cell| {
        cell.try_borrow().ok().is_some_and(|s| {
            s.as_ref()
                .is_some_and(|s| s.registry.slots().any(|slot| slot.create_ui().is_some()))
        })
    })
}

/// Calls every plugin's Create-screen callback with a table over `w`; inside them `picker`
/// serves `create_map_count` and `create_map_select`. Main thread, from the screen's draw.
pub fn draw_create_ui(w: &dyn Widgets, picker: &mut dyn crate::createpick::MapPicker) {
    SESSION.with(|cell| {
        let Ok(session) = cell.try_borrow() else {
            return;
        };
        let Some(session) = session.as_ref() else {
            return;
        };
        crate::createpick::with_picker(picker, || {
            for slot in session.registry.slots() {
                if slot.create_ui().is_some() {
                    ui::with_frame(w, |table| session.registry.call_create_ui(slot, table));
                }
            }
        });
    });
}

/// Whether the replacement Create Multiplayer Game screen is switched on (`host.json`).
pub fn enhanced_create_screen() -> bool {
    SESSION.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|s| s.as_ref().map(|s| s.config.enhanced_create_screen))
            .unwrap_or(false)
    })
}

/// Whether the replacement multiplayer lobby screen is switched on (`host.json`).
pub fn enhanced_lobby_screen() -> bool {
    SESSION.with(|cell| {
        cell.try_borrow()
            .ok()
            .and_then(|s| s.as_ref().map(|s| s.config.enhanced_lobby_screen))
            .unwrap_or(false)
    })
}

/// For the lobby screen's marks: the lobby seat of the host (net slot 0), and which lobby seats
/// hold a player running the mod (announced in the handshake, or ourselves). Empty off a lobby.
/// The third value is the version text each such player announced (ours for ourselves).
pub fn lobby_roles() -> (Option<u8>, [ModMark; 8], [String; 8]) {
    SESSION.with(|cell| {
        let none = || (None, [ModMark::None; 8], Default::default());
        let Ok(slot) = cell.try_borrow() else {
            return none();
        };
        let Some(s) = slot.as_ref() else {
            return none();
        };
        let (Some(table), Ok(h)) = (s.net.transport.lobby_slots(), s.net.handshake.try_borrow())
        else {
            return none();
        };
        if !h.in_lobby() {
            return (table[0], [ModMark::None; 8], Default::default());
        }
        let marks = lobbylayout::mod_slots(&table, |n| h.peer_matches(n), h.self_slot());
        let mut versions: [String; 8] = Default::default();
        for (net, lobby) in table.iter().enumerate() {
            let Some(l) = lobby.filter(|&l| (l as usize) < 8) else {
                continue;
            };
            versions[l as usize] = if h.self_slot() == Some(net as u32) {
                handshake::VERSION_TEXT.to_owned()
            } else {
                h.peer(net as u32).map(|p| p.version.clone()).unwrap_or_default()
            };
        }
        (table[0], marks, versions)
    })
}

/// A line in the host's log, at `level`.
pub fn host_line(level: u32, source: &str, msg: &str) {
    StartupLog.line(level, source, msg);
}

pub fn log_error(source: &str, msg: &str) {
    StartupLog.line(W2R_LOG_ERROR, source, msg);
}

/// A log line from another host module.
pub fn log_info(source: &str, msg: &str) {
    StartupLog.line(W2R_LOG_INFO, source, msg);
}

fn save_locks(session: &mut Session, teams: bool, slots: bool) {
    session.config.lock_team_changes = teams;
    session.config.lock_slot_swaps = slots;
    match hostcfg::write(&session.root, &session.config) {
        Ok(()) => StartupLog.line(
            W2R_LOG_INFO,
            "host",
            &format!(
                "lobby locks: team changes {}, slot swaps {}",
                if teams { "locked" } else { "open" },
                if slots { "locked" } else { "open" }
            ),
        ),
        Err(e) => StartupLog.line(
            W2R_LOG_ERROR,
            "host",
            &format!("cannot write {}: {e}", hostcfg::FILE),
        ),
    }
}

fn state_version(p: &state::PluginState) -> String {
    let v = &p.version;
    format!(
        "{}.{}.{}{}",
        v.major,
        v.minor,
        v.patch,
        if v.pre.is_empty() { "" } else { "-pre" }
    )
}

// ---- startup ------------------------------------------------------------------------------

fn start() -> Session {
    let log: Rc<dyn Log> = Rc::new(StartupLog);
    let lobby = Rc::new(RefCell::new(Lobby::new()));
    let running = Build::parse(GAME_VERSION).expect("GAME_VERSION is well-formed");

    let Some(startup) = entry::startup() else {
        // Cannot happen: the frame hook exists only after startup; keep an empty session.
        let net = Rc::new(Net::idle());
        NET.with(|n| *n.borrow_mut() = Some(net.clone()));
        return Session {
            registry: Registry::new(log),
            lobby,
            state: State::default(),
            root: PathBuf::new(),
            config: HostConfig::default(),
            fingerprint: [0; 4],
            simulation: Vec::new(),
            plugin_info: BTreeMap::new(),
            enabled_at_start: BTreeSet::new(),
            net,
            editor: Editor::default(),
            #[cfg(target_arch = "x86")]
            native: None,
            #[cfg(target_arch = "x86")]
            nk: None,
        };
    };
    let (config, warning) = hostcfg::read(&startup.root);
    if let Some(w) = warning {
        log.line(W2R_LOG_WARN, "host", &w);
    }
    let symbols = Symbols::new(startup.game_base);
    let game = startup.image.clone().map(|image| Game { image, symbols });
    let transport = transport(&symbols, &*log);
    let net = Rc::new(Net {
        handshake: RefCell::new(Handshake::new(handshake::Local {
            fingerprint: [0; 4],
        })),
        transport: transport.clone(),
    });
    net.handshake
        .borrow_mut()
        .set_map_sharing(config.map_sharing);
    NET.with(|n| *n.borrow_mut() = Some(net.clone()));
    let services = Services {
        chat: lobby_chat(&symbols, lobby.clone(), &*log),
        transport,
        roster: Rc::new(HandshakeRoster(net.clone())),
        presence: Rc::new(LobbyPresence(lobby.clone())),
        root: None,
    };
    if let Some(image) = &startup.image {
        peer::hook::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        lobbypolicy::hook::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::mapdownload::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::matchstart::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::matchend::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::matchend::install_run_game(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::replayrec::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::replayplay::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::chatrec::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::combatrec::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::plugincmd::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::hudpanel::install(&symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::scorebars::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::graphicsmode::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::apm::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::netpanel::install(&symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::ping::install(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::mapdraw::install(&symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::camerashare::install(&symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::ingamekick::install(&symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::ingamekick::install_alliances(image, &symbols, &*log);
        #[cfg(target_arch = "x86")]
        crate::alliancesscreen::install(&symbols, &*log);
    }
    #[cfg(target_arch = "x86")]
    let nk = {
        let nk = options::Nk::new(&symbols);
        match (&nk, &startup.image) {
            (Some(_), Some(image)) => {
                options::install(image, &symbols, &*log);
                options::install_main_menu(image, &symbols, &*log);
                crate::settingsscreen::install(image, &symbols, &*log);
                crate::lobbycreate::install(image, &symbols, &*log);
                crate::joinpassword::install(image, &symbols, &*log);
                crate::gameinfopatch::install(image, symbols.base(), &*log);
                crate::createscreen::install(&symbols, &*log);
                crate::lobbyscreen::install(image, &symbols, &*log);
            }
            _ => log.line(
                W2R_LOG_ERROR,
                "host",
                "UI symbols missing; no plugin screen",
            ),
        }
        nk
    };

    log.line(W2R_LOG_INFO, "host", "loading plugins on the main thread");
    #[cfg(target_arch = "x86")]
    let host_log = log.clone();
    let loaded = plugins::load_all(&startup.root, running, log, game, services);
    #[cfg(target_arch = "x86")]
    if loaded.simulation.iter().any(|id| id == "w2r-observer") {
        crate::lobbyscreen::add_observer_team_option(&symbols, &*host_log);
    }
    net.handshake
        .borrow_mut()
        .set_fingerprint(loaded.fingerprint);
    #[cfg(target_arch = "x86")]
    {
        crate::uploader::init(&startup.root);
        crate::lobbyratings::init(&startup.root);
        crate::matchend::init_reporter(
            &startup.root,
            loaded.fingerprint,
            crate::handshake::VERSION_TEXT,
            &*host_log,
        );
        crate::replayrec::init(
            &startup.root,
            loaded.fingerprint,
            crate::handshake::VERSION_TEXT,
            &*host_log,
        );
        crate::replayrec::set_enabled(config.record_replays);
        crate::replayrec::set_skip_computer(config.skip_computer_replays);
        crate::apm::configure(&config);
        crate::netpanel::configure(&config);
        crate::ping::configure(&config);
        crate::createscreen::set_auto_game_name(config.auto_game_name);
        crate::replayplay::init(&startup.root, loaded.fingerprint);
    }
    let plugin_info = plugins::infos(&startup.root, &loaded.state);
    let enabled_at_start: BTreeSet<String> = loaded
        .state
        .plugins
        .iter()
        .filter(|p| p.enabled)
        .map(|p| p.id.clone())
        .collect();
    #[cfg(target_arch = "x86")]
    let native = startup.image.as_ref().and_then(|image| {
        match crate::nativesettings::game::Bridge::new(image, &symbols) {
            Ok(bridge) => Some(bridge),
            Err(why) => {
                host_line(
                    W2R_LOG_WARN,
                    "settings",
                    &format!("the game's settings table was refused ({why}); no game settings"),
                );
                None
            }
        }
    });
    #[cfg(target_arch = "x86")]
    let entries = native.as_ref().map(|b| b.entries());
    #[cfg(not(target_arch = "x86"))]
    let entries: Option<Vec<crate::nativesettings::Entry>> = None;
    let editor = Editor::new(settings_registry(
        entries.as_deref(),
        &loaded.registry,
        &plugin_info,
    ));
    host_line(
        W2R_LOG_INFO,
        "settings",
        &format!(
            "{} settings on the options screen ({} categories)",
            editor.registry.len(),
            editor.registry.categories().len()
        ),
    );
    #[cfg(target_arch = "x86")]
    crate::settingsscreen::set_hide_arrows(config.hide_scrollbar_arrows);
    Session {
        registry: loaded.registry,
        lobby,
        state: loaded.state,
        root: startup.root.clone(),
        config,
        fingerprint: loaded.fingerprint,
        simulation: loaded.simulation,
        plugin_info,
        enabled_at_start,
        net,
        editor,
        #[cfg(target_arch = "x86")]
        native,
        #[cfg(target_arch = "x86")]
        nk,
    }
}

#[cfg(target_arch = "x86")]
fn lobby_chat(symbols: &Symbols, lobby: Rc<RefCell<Lobby>>, log: &dyn Log) -> Rc<dyn Chat> {
    match chat::game::LobbyChat::new(symbols, lobby) {
        Some(c) => Rc::new(c),
        None => {
            log.line(
                W2R_LOG_ERROR,
                "host",
                "lobby chat symbols missing; chat_send unavailable",
            );
            Rc::new(NoChat)
        }
    }
}

#[cfg(not(target_arch = "x86"))]
fn lobby_chat(_: &Symbols, _: Rc<RefCell<Lobby>>, _: &dyn Log) -> Rc<dyn Chat> {
    Rc::new(NoChat)
}

#[cfg(target_arch = "x86")]
fn transport(symbols: &Symbols, log: &dyn Log) -> Rc<dyn Transport> {
    match peer::game::GameTransport::new(symbols) {
        Some(t) => Rc::new(t),
        None => {
            log.line(
                W2R_LOG_ERROR,
                "host",
                "net session symbols missing; peer_send and the handshake are unavailable",
            );
            Rc::new(NoTransport)
        }
    }
}

#[cfg(not(target_arch = "x86"))]
fn transport(_: &Symbols, _: &dyn Log) -> Rc<dyn Transport> {
    Rc::new(NoTransport)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::Services;
    use crate::semver::Version;
    use crate::settings::{Kind, Owners, Value};
    use crate::state::PluginState;
    use w2r_abi::*;

    struct Quiet;
    impl Log for Quiet {
        fn line(&self, _: u32, _: &str, _: &str) {}
    }

    fn s(text: &str) -> W2rStr {
        W2rStr {
            ptr: text.as_ptr().cast(),
            len: text.len() as u32,
        }
    }

    fn int_setting(key: &'static str, category: &'static str) -> W2rSettingDef {
        W2rSettingDef {
            struct_size: W2R_SETTING_DEF_V1_SIZE,
            kind: W2R_SETTING_INT,
            default_int: 7,
            min: 0,
            max: 60,
            flags: 0,
            choice_count: 0,
            choices: core::ptr::null(),
            reserved: 0,
            key: s(key),
            label: s("Label"),
            description: s(""),
            category: s(category),
            subcategory: s(""),
            default_str: s(""),
            active_when: s(""),
        }
    }

    thread_local! {
        static HOST: Cell<*const W2rHost> = const { Cell::new(core::ptr::null()) };
        /// What the listening plugin read when it heard the settings changed.
        static HEARD: RefCell<Vec<(i64, i64)>> = const { RefCell::new(Vec::new()) };
    }

    fn read(h: &W2rHost, key: &str) -> i64 {
        let mut n = -1;
        unsafe { (h.settings_get_i64)(h.token, s(key), &mut n) };
        n
    }

    unsafe extern "C" fn listener_load(host: *const W2rHost) -> W2rStatus {
        HOST.with(|c| c.set(host));
        let h = unsafe { &*host };
        unsafe {
            (h.settings_declare)(h.token, &int_setting("fade", ""));
            (h.settings_declare)(h.token, &int_setting("lines", "Chat"));
            (h.subscribe)(h.token, W2R_EVT_SETTINGS_CHANGED)
        }
    }
    unsafe extern "C" fn listener_event(_: *const W2rEvent) {
        let h = unsafe { &*HOST.with(|c| c.get()) };
        HEARD.with(|v| v.borrow_mut().push((read(h, "fade"), read(h, "lines"))));
    }
    static LISTENER: W2rPlugin = W2rPlugin {
        struct_size: W2R_PLUGIN_V1_SIZE,
        on_load: Some(listener_load),
        on_event: Some(listener_event),
        on_ui: None,
        on_unload: None,
    };
    unsafe extern "C" fn listener_entry(_: *const W2rHost) -> *const W2rPlugin {
        &LISTENER
    }

    /// Declares a setting, then fails its load.
    unsafe extern "C" fn refused_load(host: *const W2rHost) -> W2rStatus {
        let h = unsafe { &*host };
        unsafe { (h.settings_declare)(h.token, &int_setting("never", "")) };
        W2R_E_STATE
    }
    static REFUSED: W2rPlugin = W2rPlugin {
        struct_size: W2R_PLUGIN_V1_SIZE,
        on_load: Some(refused_load),
        on_event: None,
        on_ui: None,
        on_unload: None,
    };
    unsafe extern "C" fn refused_entry(_: *const W2rHost) -> *const W2rPlugin {
        &REFUSED
    }

    fn root(tag: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("w2r-session-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("config")).unwrap();
        root
    }

    fn registry(root: &Path) -> Registry {
        let services = Services {
            root: Some(root.to_path_buf()),
            ..Services::none()
        };
        let mut reg = Registry::with(Rc::new(Quiet), None, services);
        reg.load_entry("w2r-chat", listener_entry).unwrap();
        assert!(reg.load_entry("w2r-broken", refused_entry).is_err());
        reg
    }

    fn info(name: &str) -> plugins::PluginInfo {
        plugins::PluginInfo {
            name: name.into(),
            author: "Ann".into(),
            description: "Does things.".into(),
        }
    }

    #[test]
    fn the_registry_has_the_hosts_and_active_plugins_settings() {
        let root = root("registry");
        let reg = registry(&root);
        let names = BTreeMap::from([("w2r-chat".to_owned(), info("Chat"))]);
        let r = settings_registry(None, &reg, &names);
        let chat = Owner::Plugin("w2r-chat".into());
        let fade = r.find(&chat, "fade").unwrap();
        assert_eq!(r.def(fade).category, "Chat", "no category: the plugin's name");
        assert_eq!(r.def(fade).source.as_deref(), Some("Chat"));
        assert_eq!(r.def(r.find(&chat, "lines").unwrap()).category, "Chat");
        assert!(
            r.find(&Owner::Plugin("w2r-broken".into()), "never").is_none(),
            "a plugin that failed to load contributes nothing"
        );
        assert!(r.find(&Owner::Host, "map_sharing").is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn owners_store_every_value_before_notifying_and_write_host_json_once() {
        let root = root("owners");
        let reg = registry(&root);
        let r = settings_registry(None, &reg, &BTreeMap::new());
        let chat = Owner::Plugin("w2r-chat".into());
        let fade = r.def(r.find(&chat, "fade").unwrap()).clone();
        let lines = r.def(r.find(&chat, "lines").unwrap()).clone();
        let sharing = r.def(r.find(&Owner::Host, "map_sharing").unwrap()).clone();
        let mut config = HostConfig::default();
        let net = Net::idle();
        let mut owners = SessionOwners {
            config: &mut config,
            root: &root,
            plugins: &reg,
            net: &net,
        };
        assert_eq!(owners.load(&fade), None, "never stored: the default is used");
        HEARD.with(|v| v.borrow_mut().clear());
        owners.store(&[
            (&fade, &Value::Int(12)),
            (&sharing, &Value::Int(0)),
            (&lines, &Value::Int(30)),
        ]);
        assert_eq!(
            HEARD.with(|v| v.borrow().clone()),
            vec![(12, 30)],
            "one event, after both values were stored"
        );
        assert_eq!(owners.load(&fade), Some(Value::Int(12)));
        assert!(!config.map_sharing);
        let (written, _) = hostcfg::read(&root);
        assert!(!written.map_sharing);
        // A value of the wrong type in the plugin's file is not used.
        reg.find("w2r-chat")
            .unwrap()
            .store_setting("fade", Val::Str("x".into()))
            .unwrap();
        let owners = SessionOwners {
            config: &mut config,
            root: &root,
            plugins: &reg,
            net: &net,
        };
        assert_eq!(owners.load(&fade), None);
        assert_eq!(fade.kind, Kind::Int);
        let _ = std::fs::remove_dir_all(&root);
    }

    fn listed(id: &str, enabled: bool) -> PluginState {
        PluginState {
            id: id.into(),
            version: Version {
                major: 1,
                minor: 2,
                patch: 0,
                pre: Vec::new(),
            },
            enabled,
            sha256: [0; 32],
            approved_capabilities: Vec::new(),
        }
    }

    #[test]
    fn plugin_rows_say_what_happened_to_each_plugin() {
        let root = root("rows");
        let reg = registry(&root);
        let state = State {
            plugins: vec![
                listed("w2r-chat", false),
                listed("w2r-broken", true),
                listed("w2r-missing", true),
                listed("w2r-off", false),
            ],
        };
        let info = BTreeMap::from([("w2r-chat".to_owned(), info("Chat"))]);
        let launched = BTreeSet::from([
            "w2r-chat".to_owned(),
            "w2r-broken".to_owned(),
            "w2r-missing".to_owned(),
        ]);
        let rows = plugin_rows(&state, &reg, &info, &launched);
        let chat = &rows[0];
        assert_eq!((chat.name.as_str(), chat.version.as_str()), ("Chat", "1.2.0"));
        assert!(chat.running && chat.enabled_at_launch && !chat.enabled);
        assert_eq!(chat.author.as_deref(), Some("Ann"));
        assert!(rows[1].error.as_deref().unwrap().starts_with("E_LOAD"));
        assert!(!rows[1].running);
        assert_eq!(rows[2].name, "w2r-missing", "no manifest: the id");
        assert_eq!(
            rows[2].error.as_deref(),
            Some("not loaded; w2r.log says why")
        );
        assert_eq!(rows[3].error, None, "switched off is not an error");
        let _ = std::fs::remove_dir_all(&root);
    }
}
