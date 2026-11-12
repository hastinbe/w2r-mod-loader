//! In-game plugin host. Loaded into `Warcraft II.exe` (32-bit) as a `version.dll` proxy: the
//! built `w2r_core.dll` is installed under that name in the game's `x86` folder.
//!
//! Nothing here touches the network; only the manager downloads. See docs/PLAN.md.

pub use w2r_formats::{json, manifest, semver, state};
pub mod alliancelayout;
pub mod apm;
pub mod camerashare;
mod director;
mod obsjoin;
mod liveapm;
pub mod ping;
pub mod mapdraw;
pub mod pingtable;
pub mod build_guard;
pub mod chat;
pub mod chatclick;
pub mod menukeys;
pub mod color;
pub mod createpick;
pub mod dispatch;
pub mod downloadclient;
pub mod downloadhost;
pub mod fingerprint;
pub mod fogmaps;
pub mod gamepath;
pub mod guard;
pub mod handshake;
pub mod hooks;
pub mod host;
pub mod hostcfg;
pub mod hostsettings;
pub mod lobby;
pub mod lobbylayout;
pub mod lobbyratings;
pub mod lobbypolicy;
pub mod log;
pub mod mapbrowser;
pub mod mapindex;
pub mod maplimits;
pub mod mapname;
pub use w2r_preview::mappreview;
pub mod maproles;
pub mod mapsearchcache;
pub mod mapstore;
pub mod linkshare;
pub mod netpanel;
pub mod netquality;
pub mod waitinglist;
pub mod mapwire;
pub mod observer;
pub mod pe;
pub mod peer;
pub use w2r_preview::pud;
pub mod pudvalidate;
pub mod scoreboard;
pub mod graphicsmode;
pub mod scorebars;
pub mod kickpanel;
pub mod nativesettings;
pub mod screenkit;
pub mod settings;
pub mod store;
pub mod symbols;
pub mod target;
pub mod ui;
pub mod uploader;
pub mod viewmemory;
#[cfg(windows)]
pub mod watchdog;

#[cfg(all(windows, target_arch = "x86"))]
pub mod alliancesscreen;
#[cfg(all(windows, target_arch = "x86"))]
pub mod browserview;
#[cfg(all(windows, target_arch = "x86"))]
pub mod createscreen;
#[cfg(windows)]
pub mod entry;
#[cfg(windows)]
pub mod frame;
#[cfg(windows)]
pub mod loader;
#[cfg(all(windows, target_arch = "x86"))]
pub mod chatrec;
#[cfg(all(windows, target_arch = "x86"))]
pub mod combatrec;
#[cfg(all(windows, target_arch = "x86"))]
pub mod hudpanel;
#[cfg(all(windows, target_arch = "x86"))]
pub mod ingamekick;
#[cfg(all(windows, target_arch = "x86"))]
pub mod replaypicker;
#[cfg(all(windows, target_arch = "x86"))]
pub mod fogtrack;
#[cfg(all(windows, target_arch = "x86"))]
pub mod viewmem;
#[cfg(all(windows, target_arch = "x86"))]
pub mod viewer;
#[cfg(all(windows, target_arch = "x86"))]
pub mod gameinfopatch;
#[cfg(all(windows, target_arch = "x86"))]
pub mod joinpassword;
#[cfg(all(windows, target_arch = "x86"))]
pub mod lobbycreate;
#[cfg(all(windows, target_arch = "x86"))]
pub mod lobbyscreen;
#[cfg(all(windows, target_arch = "x86"))]
pub mod mapdownload;
#[cfg(all(windows, target_arch = "x86"))]
pub mod matchend;
#[cfg(all(windows, target_arch = "x86"))]
pub mod matchstart;
#[cfg(all(windows, target_arch = "x86"))]
pub mod options;
#[cfg(all(windows, target_arch = "x86"))]
pub mod settingsscreen;
pub mod plugincmd;
#[cfg(windows)]
pub mod paths;
#[cfg(windows)]
pub mod plugins;
#[cfg(all(windows, target_arch = "x86"))]
pub mod previewdraw;
#[cfg(all(windows, target_arch = "x86"))]
pub mod replayplay;
#[cfg(all(windows, target_arch = "x86"))]
pub mod replayrec;
#[cfg(all(windows, target_arch = "x86", target_env = "msvc"))]
pub mod proxy;
#[cfg(windows)]
pub mod session;
