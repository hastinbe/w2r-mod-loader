//! The game's own settings on the options screen (docs/settings-screen.md): the 39 entries of
//! `Warcraft2.ini`.
//!
//! The game keeps them in one table, `SettingsTable`: per entry an ini key, a type, a range, a
//! default and the address of the global that holds the live value. Values are written to that
//! global, applied with the function the stock options tab calls, and saved with the game's own
//! ini writer, so nothing of the game's is stored by the mod loader.
//!
//! The catalog (each key's category, label and description) is plain data; reading the table and
//! calling the game is x86 only ([`game`]). Ported from wc2r-plus
//! `core/settings/native_settings_*` (MIT).

use crate::settings::{Condition, Kind, Owner, Registry, SettingDef, Value};

/// One row of the game's settings table, as read from memory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Entry {
    /// 0 string, 1 uint, 2 flag (one bit of a shared bitfield).
    pub kind: u32,
    pub key: String,
    pub min: u32,
    /// uint: the max. flag: the bit mask.
    pub max_or_mask: u32,
    /// uint: the default. string: the buffer's capacity.
    pub def_or_cap: u32,
}

/// Where an ini key goes on the screen. The categories follow the stock options tabs. Where the
/// stock menu shows the setting, `label` is the game's localization key for it, so it is shown in
/// the player's language.
struct Placement {
    key: &'static str,
    category: &'static str,
    subcategory: &'static str,
    label: &'static str,
    description: &'static str,
    /// Not shown; the stock options menu does not show these either.
    hidden: bool,
}

const fn p(
    key: &'static str,
    category: &'static str,
    subcategory: &'static str,
    label: &'static str,
    description: &'static str,
    hidden: bool,
) -> Placement {
    Placement {
        key,
        category,
        subcategory,
        label,
        description,
        hidden,
    }
}

#[rustfmt::skip]
const PLACEMENTS: &[Placement] = &[
    // --- Sound ---
    p("music", "Sound", "Volume", "option_music_volume",
      "Background music volume, 0 (silent) to 100.", false),
    p("sfx", "Sound", "Volume", "option_sfx_volume",
      "Sound effects volume, 0 (silent) to 100.", false),
    p("remastered_music", "Sound", "Music Version", "option_musicversion_title",
      "Which soundtrack to play: the original DOS music, or the remastered version.", false),
    p("unitspeech", "Sound", "Effects", "option_sound_unit_speech",
      "Spoken alerts, such as the warning that you are under attack.", false),
    p("unitnoise", "Sound", "Effects", "option_sound_unit_acknowledgement",
      "The sounds units make when you select them or give them an order.", false),
    p("bldgnoise", "Sound", "Effects", "option_sound_building_sound",
      "The sounds buildings make when you select them.", false),
    p("dispmsgs", "Sound", "Effects", "option_sound_subtitles",
      "Show subtitles for spoken lines.", false),
    // --- Speed ---
    p("speed", "Speed", "Speeds", "option_speed_game_speed",
      "How fast the game world runs, 0 to 8. The game's own default is 6.", false),
    p("mscroll", "Speed", "Speeds", "option_speed_mouse_scroll",
      "How fast the view scrolls when the mouse is at a screen edge, 0 to 6.", false),
    p("kscroll", "Speed", "Speeds", "option_speed_key_scroll",
      "How fast the view scrolls from the keyboard, 0 to 6.", false),
    // --- Preferences ---
    p("screen_mode", "Preferences", "Display", "option_display_mode",
      "Whether the game runs in a window or full screen.", false),
    p("display_monitor", "Preferences", "Display", "option_display_screen",
      "Which monitor the game opens on. The list is the game's own display count.", false),
    // Not in the stock menu, and changing them from the screen has no effect.
    p("resolution_width", "Preferences", "Display", "Resolution Width",
      "Width in pixels, 640 to 3840. A value outside that range falls back to 1920, it is not clamped.",
      true),
    p("resolution_height", "Preferences", "Display", "Resolution Height",
      "Height in pixels, 480 to 2160. A value outside that range falls back to 1080, it is not clamped.",
      true),
    // Bit 0x10 of the flags: the mode the next launch starts in, which F5 and the HUD button
    // also set (graphicsmode.rs). The mode follows on the next match frame (graphicsmode.rs).
    p("using_classic", "Preferences", "Display", "Classic Graphics",
      "Uses classic graphics instead of HD. F5 and the HUD button change it too.",
      false),
    p("interface", "Preferences", "Mouse", "option_preference_mouse_interface",
      "Mouse scheme: on is the Warcraft II scheme, off the Warcraft I one.", false),
    // Works (RebuildMouseCursors applies it), but hidden by choice.
    p("mouse_size", "Preferences", "Mouse", "option_preference_mouse_size",
      "Size of the mouse cursor as a percentage of the cursor art, 15 to 100. Default 50.", true),
    p("mouse_trapped", "Preferences", "Mouse", "option_preference_mouse_trapped",
      "Keep the mouse cursor inside the game window.", false),
    p("ui_scale_mode", "Preferences", "UI Scale", "option_preference_ui_scale",
      "How the interface is scaled: Automatic, Fixed, or Classic.", false),
    p("ui_scale_fixed", "Preferences", "UI Scale", "UI Scale (Fixed)",
      "The interface scale factor, 1 to 4. Used only when UI scale is set to Fixed.", false),
    p("ui_unlock_aspect", "Preferences", "UI Scale", "Use Window Aspect Ratio",
      "Unlocks the UI's aspect ratio so it follows the window instead of a fixed one.", false),
    // The table's only string. There is no writer for it.
    p("name", "Preferences", "Player", "Player Name",
      "The multiplayer player name. Edit it in Warcraft2.ini: the game's buffer holds 12 characters and this screen has no writer for it.",
      true),
    p("tip", "Preferences", "Other", "option_preference_tips",
      "Show the game's tips.", false),
    p("intro", "Preferences", "Other", "option_preference_intro",
      "Play the intro movie on startup, after the Blizzard logo.", false),
    p("hide_objectives", "Preferences", "Other", "option_preference_hide_objectives",
      "Hide the mission objectives display.", false),
    // --- Gameplay ---
    p("show_healthbars", "Gameplay", "Health Bars", "option_gameplay_healthbar_show",
      "Draw health bars on units. Pairs with 'Only when damaged' to give the stock menu's three choices.",
      false),
    p("healthbars_only_when_damaged", "Gameplay", "Health Bars", "Only When Damaged",
      "Limit health bars to units that are not at full health. Needs health bars on.", false),
    p("healthbars_below", "Gameplay", "Health Bars", "option_gameplay_healthbar_location",
      "Whether each health bar is drawn above or below its unit.", false),
    p("resourcebar_align_left", "Gameplay", "Resource Bar", "Aligned Left",
      "Aligns the resource bar left. With 'Aligned Right' it encodes the stock left / center / right choice.",
      false),
    p("resourcebar_align_right", "Gameplay", "Resource Bar", "Aligned Right",
      "Aligns the resource bar right. With 'Aligned Left' it encodes the stock left / center / right choice.",
      false),
    p("portrait_style_scaled", "Gameplay", "Portraits", "Scaled",
      "Draws unit portraits scaled. Paired with 'Cropped'; the stock menu offers one or the other.",
      false),
    p("portrait_style_cropped", "Gameplay", "Portraits", "Cropped",
      "Draws unit portraits cropped. Paired with 'Scaled'; the stock menu offers one or the other.",
      false),
    p("show_button_hotkeys", "Gameplay", "Controls", "option_gameplay_show_button_hotkeys",
      "Print each command button's hotkey on the button itself.", false),
    p("use_grid_keys", "Gameplay", "Controls", "option_use_grid_keys",
      "Use a grid hotkey layout for the command buttons instead of the game's lettered keys.", false),
    // Not in the stock Gameplay tab.
    p("center_on_cluster", "Gameplay", "Camera", "Center on Unit Cluster",
      "Center the view on a cluster of units rather than on one of them. Uses 'Max Cluster Distance'.",
      true),
    p("max_cluster_distance", "Gameplay", "Camera", "Max Cluster Distance",
      "How far apart units may be and still count as one cluster, 0 to 12.", true),
    // --- Advanced ---
    // The game overwrites it as it shows tips.
    p("tipnum", "Advanced", "Misc", "Next Tip Number",
      "Index of the next tip the game will show. The game writes this itself as tips are shown.", true),
    // Nothing in this build reads these; registered so the ini keeps them.
    p("univbe", "Advanced", "Legacy", "Legacy: UniVBE",
      "DOS-era key: 1 forced the UniVBE video driver to load even on a VESA-capable card. Nothing in this build reads it.",
      true),
    p("vidmouse", "Advanced", "Legacy", "Legacy: Video Mouse",
      "DOS-era key: a mouse-and-video workaround, 0 off / 1 partial / 2 full. Nothing in this build reads it.",
      true),
];

/// Ini keys shown as a dropdown, with the labels and stored values the stock tabs use.
struct OptionList {
    key: &'static str,
    labels: &'static [&'static str],
    /// The stored value of each label; empty means label `i` stores `i`.
    values: &'static [i64],
}

const OPTION_LISTS: &[OptionList] = &[
    // The ini stores 1 for windowed.
    OptionList {
        key: "screen_mode",
        labels: &["option_windowed", "option_full_screen"],
        values: &[1, 0],
    },
    OptionList {
        key: "remastered_music",
        labels: &["option_musicversion_dos", "option_musicversion_remastered"],
        values: &[],
    },
    OptionList {
        key: "healthbars_below",
        labels: &[
            "option_gameplay_healthbar_location_above",
            "option_gameplay_healthbar_location_below",
        ],
        values: &[],
    },
    OptionList {
        key: "ui_scale_mode",
        labels: &[
            "option_preference_ui_scale_classic",
            "option_preference_ui_scale_auto",
            "option_preference_ui_scale_fixed",
        ],
        values: &[2, 0, 1],
    },
];

// ---- derived settings ------------------------------------------------------------------------
//
// Three stock Gameplay options are each one dropdown over two bits of the game's settings flags.
// The screen shows the dropdown, with the stock tab's bit mapping, and hides the two bits, so a
// combination the stock menu cannot make cannot be picked.

pub const FLAG_SHOW_HEALTHBARS: u32 = 0x0020_0000;
pub const FLAG_HEALTHBARS_NOT_FULL: u32 = 0x0040_0000;
pub const FLAG_RESOURCE_LEFT: u32 = 0x0200_0000;
pub const FLAG_RESOURCE_RIGHT: u32 = 0x0400_0000;
pub const FLAG_PORTRAIT_SCALED: u32 = 0x1000_0000;
pub const FLAG_PORTRAIT_CROPPED: u32 = 0x2000_0000;

/// The keys of the derived settings.
pub const DERIVED: &[&str] = &["healthbar_mode", "resourcebar_align", "portrait_style"];

/// A derived setting's dropdown index, from the game's flags.
pub fn derived_get(key: &str, flags: u32) -> i64 {
    let on = |m: u32| flags & m != 0;
    match key {
        // 0 always, 1 never, 2 only when damaged.
        "healthbar_mode" if !on(FLAG_SHOW_HEALTHBARS) => 1,
        "healthbar_mode" if on(FLAG_HEALTHBARS_NOT_FULL) => 2,
        "healthbar_mode" => 0,
        // Center is neither bit set.
        "resourcebar_align" if on(FLAG_RESOURCE_LEFT) => 0,
        "resourcebar_align" if on(FLAG_RESOURCE_RIGHT) => 2,
        "resourcebar_align" => 1,
        "portrait_style" if on(FLAG_PORTRAIT_SCALED) => 0,
        "portrait_style" => 1,
        _ => 0,
    }
}

/// The game's flags with a derived setting set to dropdown index `i`.
pub fn derived_set(key: &str, flags: u32, i: i64) -> u32 {
    let write = |clear: u32, set: u32| (flags & !clear) | set;
    match (key, i) {
        ("healthbar_mode", 1) => write(FLAG_SHOW_HEALTHBARS | FLAG_HEALTHBARS_NOT_FULL, 0),
        ("healthbar_mode", 2) => write(0, FLAG_SHOW_HEALTHBARS | FLAG_HEALTHBARS_NOT_FULL),
        ("healthbar_mode", _) => write(FLAG_HEALTHBARS_NOT_FULL, FLAG_SHOW_HEALTHBARS),
        ("resourcebar_align", 0) => write(FLAG_RESOURCE_RIGHT, FLAG_RESOURCE_LEFT),
        ("resourcebar_align", 2) => write(FLAG_RESOURCE_LEFT, FLAG_RESOURCE_RIGHT),
        ("resourcebar_align", _) => write(FLAG_RESOURCE_LEFT | FLAG_RESOURCE_RIGHT, 0),
        ("portrait_style", 0) => write(
            FLAG_PORTRAIT_SCALED | FLAG_PORTRAIT_CROPPED,
            FLAG_PORTRAIT_SCALED,
        ),
        ("portrait_style", _) => write(
            FLAG_PORTRAIT_SCALED | FLAG_PORTRAIT_CROPPED,
            FLAG_PORTRAIT_CROPPED,
        ),
        _ => flags,
    }
}

/// Keys registered but not shown: the bits the derived settings replace.
const HIDDEN_AFTER_DERIVED: &[&str] = &[
    "show_healthbars",
    "healthbars_only_when_damaged",
    "resourcebar_align_left",
    "resourcebar_align_right",
    "portrait_style_scaled",
    "portrait_style_cropped",
];

/// The game's settings, from the table's entries, plus the derived settings. A key missing from
/// the catalog is shown under Advanced with its ini name.
pub fn catalog(entries: &[Entry]) -> Vec<SettingDef> {
    let mut defs: Vec<SettingDef> = entries.iter().map(entry_def).collect();
    if entries.iter().any(|e| e.kind == 2) {
        let derived = [
            (
                "healthbar_mode",
                "Health Bars",
                "option_gameplay_healthbar_show",
                "When to draw health bars on units.",
                &[
                    "option_gameplay_healthbar_show_always",
                    "option_gameplay_healthbar_show_never",
                    "option_gameplay_healthbar_show_not_full",
                ][..],
            ),
            (
                "resourcebar_align",
                "Resource Bar",
                "option_resourcebar_align_title",
                "Where the resource bar sits along the top of the screen.",
                &[
                    "option_resourcebar_align_left",
                    "option_resourcebar_align_center",
                    "option_resourcebar_align_right",
                ][..],
            ),
            (
                "portrait_style",
                "Portraits",
                "option_portrait_style_title",
                "How unit portraits are fitted to their frame.",
                &[
                    "option_portrait_style_scaled",
                    "option_portrait_style_cropped",
                ][..],
            ),
        ];
        for (key, sub, label, description, choices) in derived {
            defs.push(
                SettingDef::new(Owner::Game, key, Kind::Choice, "Gameplay", sub)
                    .label(label)
                    .describe(description)
                    .choices(choices),
            );
        }
        for d in defs.iter_mut() {
            if HIDDEN_AFTER_DERIVED.contains(&d.key.as_str()) {
                d.hidden = true;
            }
            // The health bar position means nothing when bars are never drawn (choice 1).
            if d.key == "healthbars_below" {
                *d = d
                    .clone()
                    .active_if(Owner::Game, "healthbar_mode", Condition::IsNot(1));
            }
        }
    }
    defs
}

fn entry_def(e: &Entry) -> SettingDef {
    let placement = PLACEMENTS
        .iter()
        .find(|p| p.key.eq_ignore_ascii_case(&e.key));
    let (category, subcategory, label, description, hidden) = match placement {
        Some(p) => (p.category, p.subcategory, p.label, p.description, p.hidden),
        None => (
            "Advanced",
            "Unsorted",
            e.key.as_str(),
            "In Warcraft2.ini but not in the mod loader's placement table, so it has no label or description yet.",
            false,
        ),
    };
    let mut d = SettingDef::new(Owner::Game, &e.key, Kind::Bool, category, subcategory)
        .label(label)
        .describe(description)
        .hidden(hidden);
    match e.kind {
        // A uint with the range 0..1 is a checkbox.
        1 if e.min == 0 && e.max_or_mask == 1 => d.default_int = i64::from(e.def_or_cap),
        1 => {
            d.kind = Kind::Int;
            d.min = i64::from(e.min);
            d.max = i64::from(e.max_or_mask);
            d.default_int = i64::from(e.def_or_cap);
        }
        2 => {}
        _ => d.kind = Kind::Str,
    }
    // The fixed scale factor is used only in Fixed mode, 1 in the ini's numbering.
    if e.key.eq_ignore_ascii_case("ui_scale_fixed") {
        d = d.active_if(Owner::Game, "ui_scale_mode", Condition::Is(1));
    }
    // After the type above, which it overrides.
    if placement.is_some() {
        if let Some(list) = OPTION_LISTS
            .iter()
            .find(|l| l.key.eq_ignore_ascii_case(&e.key))
        {
            d = d.choices(list.labels);
            d.kind = Kind::Choice;
            d.choice_values = list.values.to_vec();
        }
    }
    d
}

/// Registers the game's settings into `registry`.
pub fn register(registry: &mut Registry, entries: &[Entry]) -> usize {
    let defs = catalog(entries);
    let n = defs.len();
    for d in defs {
        registry.register(d, Value::Int(0));
    }
    n
}

#[cfg(target_arch = "x86")]
pub mod game {
    //! Reading and writing the game's settings table. The table is validated before use, since
    //! values are written through its pointers; if anything is off, no game settings are shown.

    use core::ffi::{CStr, c_char, c_void};

    use super::{DERIVED, Entry, derived_get, derived_set};
    use crate::pe::Image;
    use crate::settings::Native;
    use crate::symbols::Symbols;

    /// The table's length, as `GetGameSettingsEntryCount` returns it.
    pub const ENTRY_COUNT: usize = 39;

    #[repr(C)]
    #[derive(Clone, Copy)]
    struct RawEntry {
        kind: u32,
        key: *const c_char,
        min: u32,
        max_or_mask: u32,
        def_or_cap: u32,
        backing: *mut c_void,
    }
    const _: () = assert!(size_of::<RawEntry>() == 24);

    type VoidFn = unsafe extern "C" fn();
    type MusicFn = unsafe extern "C" fn(*mut c_void);
    type SpeedFn = unsafe extern "C" fn(u32, i8);
    type CharFn = unsafe extern "C" fn(i8);
    type U32Fn = unsafe extern "C" fn(u32);
    type PairFn = unsafe extern "C" fn(u32, u32);

    /// The validated table, and the functions the stock tabs call to apply a change.
    pub struct Bridge {
        table: *mut RawEntry,
        persist: VoidFn,
        apply_music_volume: MusicFn,
        restart_music: VoidFn,
        apply_game_speed: SpeedFn,
        rebuild_mouse_cursors: VoidFn,
        apply_grid_keys: CharFn,
        apply_ui_scale: PairFn,
        apply_ui_aspect: U32Fn,
        apply_display_mode: PairFn,
        /// The game's settings flags, which every flag entry points into.
        flags: *mut u32,
    }

    macro_rules! sym {
        ($symbols:expr, $name:literal, $t:ty) => {
            unsafe { core::mem::transmute::<usize, $t>($symbols.address($name)?) }
        };
    }

    impl Bridge {
        /// Resolves and validates the table. `Err` says why it was refused.
        pub fn new(image: &Image, symbols: &Symbols) -> Result<Bridge, String> {
            Bridge::resolve(symbols)
                .ok_or_else(|| "settings symbols missing".to_owned())
                .and_then(|mut b| b.validate(image).map(|()| b))
        }

        fn resolve(symbols: &Symbols) -> Option<Bridge> {
            Some(Bridge {
                table: symbols.address("SettingsTable")? as *mut RawEntry,
                persist: sym!(symbols, "PersistSettings", VoidFn),
                apply_music_volume: sym!(symbols, "ApplyMusicVolume", MusicFn),
                restart_music: sym!(symbols, "RestartMusic", VoidFn),
                apply_game_speed: sym!(symbols, "ApplyGameSpeed", SpeedFn),
                rebuild_mouse_cursors: sym!(symbols, "RebuildMouseCursors", VoidFn),
                apply_grid_keys: sym!(symbols, "ApplyGridKeys", CharFn),
                apply_ui_scale: sym!(symbols, "ApplyUiScale", PairFn),
                apply_ui_aspect: sym!(symbols, "ApplyUiAspect", U32Fn),
                apply_display_mode: sym!(symbols, "ApplyDisplayMode", PairFn),
                flags: core::ptr::null_mut(),
            })
        }

        fn raw(&self, i: usize) -> RawEntry {
            // SAFETY: `i < ENTRY_COUNT` and the table is the game's static array.
            unsafe { self.table.add(i).read() }
        }

        fn validate(&mut self, image: &Image) -> Result<(), String> {
            let inside = |p: usize| image.contains(p, 1);
            for i in 0..ENTRY_COUNT {
                let e = self.raw(i);
                if e.kind > 2 {
                    return Err(format!("entry {i} has type {}", e.kind));
                }
                if !inside(e.key as usize) || !inside(e.backing as usize) {
                    return Err(format!("entry {i} points outside the game module"));
                }
                if e.kind == 1 && e.min > e.max_or_mask {
                    return Err(format!(
                        "entry {i} has min {} > max {}",
                        e.min, e.max_or_mask
                    ));
                }
            }
            // A known first entry, to be sure this is the right table.
            let first = self.raw(0);
            // SAFETY: checked inside the module above; the game's keys are C strings.
            let key = unsafe { CStr::from_ptr(first.key) };
            if key.to_bytes() != b"speed" || first.max_or_mask != 8 {
                return Err("entry 0 is not 'speed' (max 8)".to_owned());
            }
            self.flags = (0..ENTRY_COUNT)
                .map(|i| self.raw(i))
                .find(|e| e.kind == 2)
                .map_or(core::ptr::null_mut(), |e| e.backing.cast());
            Ok(())
        }

        /// The table's rows, for the catalog.
        pub fn entries(&self) -> Vec<Entry> {
            (0..ENTRY_COUNT)
                .map(|i| {
                    let e = self.raw(i);
                    Entry {
                        kind: e.kind,
                        // SAFETY: validated in `new`.
                        key: unsafe { CStr::from_ptr(e.key) }
                            .to_string_lossy()
                            .into_owned(),
                        min: e.min,
                        max_or_mask: e.max_or_mask,
                        def_or_cap: e.def_or_cap,
                    }
                })
                .collect()
        }

        fn find(&self, key: &str) -> Option<RawEntry> {
            (0..ENTRY_COUNT).map(|i| self.raw(i)).find(|e| {
                // SAFETY: validated in `new`.
                unsafe { CStr::from_ptr(e.key) }.to_bytes() == key.as_bytes()
            })
        }

        fn read_u32(e: &RawEntry) -> u32 {
            // SAFETY: a validated backing global inside the game module.
            unsafe { e.backing.cast::<u32>().read() }
        }

        /// Makes a changed setting take effect, with the function the stock tab calls. Settings
        /// not listed here take effect without one.
        fn apply(&self, key: &str) {
            let get = |k: &str| self.find(k).map(|e| Self::read_u32(&e));
            // SAFETY: game functions from the symbol table, called as the stock tabs call them.
            unsafe {
                match key {
                    // Entry 0's global is the start of the settings block this function takes.
                    "music" => (self.apply_music_volume)(self.raw(0).backing),
                    "remastered_music" => (self.restart_music)(),
                    "speed" => {
                        if let Some(v) = get("speed") {
                            (self.apply_game_speed)(v, 0)
                        }
                    }
                    "mouse_size" => (self.rebuild_mouse_cursors)(),
                    "use_grid_keys" => {
                        if let Some(v) = get("use_grid_keys") {
                            (self.apply_grid_keys)(i8::from(v != 0))
                        }
                    }
                    // The game reads the bit at launch; the graphics hook switches the mode
                    // to it at the next match.
                    #[cfg(windows)]
                    "using_classic" => {
                        if let Some(e) = self.find("using_classic") {
                            crate::graphicsmode::choose(Self::read_u32(&e) & e.max_or_mask != 0)
                        }
                    }
                    "ui_unlock_aspect" => {
                        if let Some(e) = self.find("ui_unlock_aspect") {
                            (self.apply_ui_aspect)(u32::from(
                                Self::read_u32(&e) & e.max_or_mask != 0,
                            ))
                        }
                    }
                    // The applier numbers the modes differently from the ini: 0 auto -> (1, 0),
                    // 1 fixed -> (2, factor), 2 classic -> (0, 0).
                    "ui_scale_mode" | "ui_scale_fixed" => {
                        if let (Some(mode), Some(fixed)) =
                            (get("ui_scale_mode"), get("ui_scale_fixed"))
                        {
                            match mode {
                                0 => (self.apply_ui_scale)(1, 0),
                                1 => (self.apply_ui_scale)(2, fixed),
                                2 => (self.apply_ui_scale)(0, 0),
                                _ => {}
                            }
                        }
                    }
                    // The first argument is "full screen"; screen_mode 1 means windowed.
                    "screen_mode" | "display_monitor" => {
                        if let (Some(mode), Some(monitor)) =
                            (get("screen_mode"), get("display_monitor"))
                        {
                            (self.apply_display_mode)(u32::from(mode == 0), monitor)
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    impl Native for Bridge {
        fn get(&self, key: &str) -> i64 {
            if DERIVED.contains(&key) {
                // SAFETY: the validated flags dword (null when the table had no flag entry).
                return unsafe { self.flags.as_ref() }.map_or(0, |f| derived_get(key, *f));
            }
            match self.find(key) {
                Some(e) if e.kind == 1 => i64::from(Self::read_u32(&e)),
                Some(e) if e.kind == 2 => i64::from(Self::read_u32(&e) & e.max_or_mask != 0),
                _ => 0,
            }
        }

        fn get_str(&self, key: &str) -> String {
            match self.find(key) {
                // SAFETY: the game's NUL-terminated buffer, validated in `new`.
                Some(e) if e.kind == 0 => unsafe { CStr::from_ptr(e.backing.cast()) }
                    .to_string_lossy()
                    .into_owned(),
                _ => String::new(),
            }
        }

        fn set(&mut self, key: &str, value: i64) {
            if DERIVED.contains(&key) {
                // SAFETY: as in `get`. Nothing to apply: the game reads these bits where it uses
                // them.
                if let Some(f) = unsafe { self.flags.as_mut() } {
                    *f = derived_set(key, *f, value);
                }
                return;
            }
            let Some(e) = self.find(key) else { return };
            // SAFETY: a validated backing global.
            unsafe {
                let slot = e.backing.cast::<u32>();
                match e.kind {
                    // Clamped: the game's ini reader would replace an out-of-range value with
                    // the default.
                    1 => slot.write((value.max(0) as u32).clamp(e.min, e.max_or_mask)),
                    2 if value != 0 => slot.write(slot.read() | e.max_or_mask),
                    2 => slot.write(slot.read() & !e.max_or_mask),
                    _ => return,
                }
            }
            self.apply(key);
        }

        fn persist(&mut self) {
            // SAFETY: the game's own ini writer.
            unsafe { (self.persist)() }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(kind: u32, key: &str, min: u32, max: u32, def: u32) -> Entry {
        Entry {
            kind,
            key: key.into(),
            min,
            max_or_mask: max,
            def_or_cap: def,
        }
    }

    #[test]
    fn every_placement_key_is_unique() {
        for (i, a) in PLACEMENTS.iter().enumerate() {
            assert!(
                PLACEMENTS[i + 1..].iter().all(|b| b.key != a.key),
                "{} twice",
                a.key
            );
            assert!(a.description.is_ascii(), "{}", a.key);
        }
    }

    #[test]
    fn a_zero_to_one_uint_is_a_toggle_and_a_range_is_a_slider() {
        let defs = catalog(&[
            entry(1, "use_grid_keys", 0, 1, 0),
            entry(1, "music", 0, 100, 50),
        ]);
        assert_eq!(defs[0].kind, Kind::Bool);
        assert_eq!(defs[1].kind, Kind::Int);
        assert!(defs[1].is_slider());
        assert_eq!(defs[1].default_int, 50);
        assert_eq!(defs[1].category, "Sound");
        assert_eq!(defs[1].label, "option_music_volume");
    }

    #[test]
    fn option_lists_have_the_last_word() {
        let defs = catalog(&[
            entry(1, "ui_scale_mode", 0, 2, 0),
            entry(1, "screen_mode", 0, 1, 1),
        ]);
        assert_eq!(defs[0].kind, Kind::Choice);
        assert_eq!(defs[0].choice_values, vec![2, 0, 1]);
        assert_eq!(defs[0].choices.len(), 3);
        assert_eq!(defs[1].kind, Kind::Choice, "not flattened into a toggle");
        assert_eq!(defs[1].choice_values, vec![1, 0]);
    }

    #[test]
    fn unplaced_keys_go_to_advanced_and_strings_stay_strings() {
        let defs = catalog(&[entry(1, "mystery", 0, 9, 3), entry(0, "name", 0, 0, 12)]);
        assert_eq!(defs[0].category, "Advanced");
        assert_eq!(defs[0].subcategory, "Unsorted");
        assert!(!defs[0].hidden);
        assert_eq!(defs[1].kind, Kind::Str);
        assert!(defs[1].hidden);
    }

    #[test]
    fn derived_settings_replace_their_bits() {
        let defs = catalog(&[
            entry(2, "show_healthbars", 0, FLAG_SHOW_HEALTHBARS, 0),
            entry(2, "using_classic", 0, 1, 0),
            entry(1, "music", 0, 100, 50),
        ]);
        let by = |k: &str| defs.iter().find(|d| d.key == k).unwrap();
        assert!(by("show_healthbars").hidden);
        assert!(!by("using_classic").hidden);
        assert_eq!(by("using_classic").subcategory, "Display");
        assert!(!by("music").hidden);
        assert_eq!(by("healthbar_mode").kind, Kind::Choice);
        assert_eq!(by("resourcebar_align").choices.len(), 3);
        assert_eq!(by("portrait_style").category, "Gameplay");
    }

    #[test]
    fn the_stock_greying_rules_are_conditions() {
        let defs = catalog(&[
            entry(1, "ui_scale_fixed", 1, 4, 1),
            entry(1, "ui_scale_mode", 0, 2, 0),
            entry(1, "healthbars_below", 0, 1, 0),
            entry(2, "show_healthbars", 0, FLAG_SHOW_HEALTHBARS, 0),
        ]);
        let mut r = Registry::new();
        for d in defs {
            r.register(d, Value::Int(0));
        }
        let mut g = Game::default();
        let fixed = r.find(&Owner::Game, "ui_scale_fixed").unwrap();
        let below = r.find(&Owner::Game, "healthbars_below").unwrap();
        assert!(r.inactive(fixed, &g), "auto mode");
        g.0.insert("ui_scale_mode".into(), 1);
        assert!(!r.inactive(fixed, &g), "fixed mode");
        assert!(!r.inactive(below, &g));
        g.0.insert("healthbar_mode".into(), 1);
        assert!(r.inactive(below, &g), "never drawn");
    }

    #[derive(Default)]
    struct Game(std::collections::BTreeMap<String, i64>);

    impl crate::settings::Native for Game {
        fn get(&self, key: &str) -> i64 {
            self.0.get(key).copied().unwrap_or(0)
        }
        fn get_str(&self, _: &str) -> String {
            String::new()
        }
        fn set(&mut self, key: &str, value: i64) {
            self.0.insert(key.to_owned(), value);
        }
        fn persist(&mut self) {}
    }

    #[test]
    fn no_flag_entries_means_no_derived_settings() {
        let defs = catalog(&[entry(1, "music", 0, 100, 50)]);
        assert_eq!(defs.len(), 1);
    }

    #[test]
    fn health_bar_modes_round_trip_through_the_flags() {
        let other = 0x8000_0001;
        for i in 0..3 {
            let f = derived_set("healthbar_mode", other, i);
            assert_eq!(derived_get("healthbar_mode", f), i);
            assert_eq!(f & other, other, "other bits untouched");
        }
        // "Always" clears the not-full bit, as the tab does.
        let f = derived_set("healthbar_mode", FLAG_HEALTHBARS_NOT_FULL, 0);
        assert_eq!(f, FLAG_SHOW_HEALTHBARS);
    }

    #[test]
    fn resource_bar_centre_is_neither_bit() {
        for i in 0..3 {
            assert_eq!(
                derived_get("resourcebar_align", derived_set("resourcebar_align", 0, i)),
                i
            );
        }
        assert_eq!(
            derived_set(
                "resourcebar_align",
                FLAG_RESOURCE_LEFT | FLAG_RESOURCE_RIGHT,
                1
            ),
            0
        );
        assert_eq!(derived_get("resourcebar_align", 0), 1);
    }

    #[test]
    fn portrait_style_sets_exactly_one_bit() {
        let both = FLAG_PORTRAIT_SCALED | FLAG_PORTRAIT_CROPPED;
        assert_eq!(derived_set("portrait_style", both, 0), FLAG_PORTRAIT_SCALED);
        assert_eq!(
            derived_set("portrait_style", both, 1),
            FLAG_PORTRAIT_CROPPED
        );
        assert_eq!(derived_get("portrait_style", FLAG_PORTRAIT_CROPPED), 1);
    }
}
