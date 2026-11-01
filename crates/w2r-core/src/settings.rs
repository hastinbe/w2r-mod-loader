//! The settings model behind the options screen (docs/settings-screen.md).
//!
//! Every setting the screen shows is one [`SettingDef`] in one [`Registry`], whoever owns it:
//! the game (`Warcraft2.ini`, read and written through the game's globals), the host
//! (`host.json`), or a plugin (declared with `settings_declare`, stored in `config\<id>.json`).
//! Nothing here touches the game or a file: game values go through [`Native`], and host and
//! plugin values are written back by the session through [`Owners`], so every rule here can
//! be tested on any target. Ported from wc2r-plus `core/settings/` (MIT).

use w2r_abi::{
    W2R_E_CONFLICT, W2R_E_INVALID, W2R_E_LIMIT, W2R_E_NOT_FOUND, W2R_MAX_SETTING_CATEGORY,
    W2R_MAX_SETTING_CHOICE, W2R_MAX_SETTING_CHOICE_COUNT, W2R_MAX_SETTING_DESCRIPTION,
    W2R_MAX_SETTING_LABEL, W2R_MAX_SETTING_STR, W2R_SETTING_BOOL, W2R_SETTING_CHOICE,
    W2R_SETTING_COLOR, W2R_SETTING_HIDDEN, W2R_SETTING_INT, W2R_SETTING_STRING, W2rStatus,
};

/// The subcategory a setting lands in when it names none.
pub const DEFAULT_SUBCATEGORY: &str = "General";

/// Heading order for a category, where a plugin's heading should sit between the host's own
/// (plugins register after the host, so by registration order theirs come last):
/// Pause Pro's "Pause Pro UI" goes right after "Fixes" in the UI category.
const HEADING_ORDER: &[(&str, &[&str])] = &[("UI", &["Enhanced UIs", "Fixes", "Pause Pro UI", "APM Counter"])];
/// The `active_when` prefix that points at one of the host's settings instead of the plugin's own.
pub const HOST_PREFIX: &str = "host:";

/// What the value is, and so which widget edits it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Bool,
    /// A slider when `min < max`, a read-only row otherwise.
    Int,
    /// A dropdown; the stored value is `choice_values[i]`, or `i` when that is empty.
    Choice,
    Str,
    /// A color picker; the value is six lowercase hex digits, `rrggbb`.
    Color,
}

impl Kind {
    /// Whether the value is text (stored with `settings_set_str`) rather than a number.
    pub fn is_text(self) -> bool {
        matches!(self, Kind::Str | Kind::Color)
    }
}

/// When a setting is active: while another setting's value passes `test`. Read live, so changing
/// that setting frees or greys the row at once.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ActiveWhen {
    pub owner: Owner,
    pub key: String,
    pub test: Condition,
    /// What a checkbox row shows while greyed (locked on or off); `None` shows its own value.
    /// A plugin's `key!=N:V` form.
    pub shown: Option<i64>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Condition {
    /// The value is not 0 (a checkbox is ticked).
    On,
    Is(i64),
    IsNot(i64),
}

impl Condition {
    pub fn holds(self, value: i64) -> bool {
        match self {
            Condition::On => value != 0,
            Condition::Is(v) => value == v,
            Condition::IsNot(v) => value != v,
        }
    }
}

/// Who owns a setting, which decides where its value lives.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Owner {
    /// A `Warcraft2.ini` setting. No value is kept here: reads and writes go to the game's
    /// globals through [`Native`], and the game's own writer saves them.
    Game,
    /// `host.json` (and `sharing.json` for `share_match_data`).
    Host,
    /// A plugin, by id; `config\<id>.json`.
    Plugin(String),
}

/// One setting: its schema. The value is kept separately (see [`Registry`]).
#[derive(Clone, Debug, PartialEq)]
pub struct SettingDef {
    pub owner: Owner,
    pub key: String,
    pub kind: Kind,
    /// The left-hand list entry; several owners may share one.
    pub category: String,
    /// The heading inside the category.
    pub subcategory: String,
    /// Plain text, or one of the game's localization keys.
    pub label: String,
    /// Hover text.
    pub description: String,
    pub default_int: i64,
    pub min: i64,
    pub max: i64,
    pub default_str: String,
    /// `Kind::Choice` only: the labels, in dropdown order (localization keys allowed).
    pub choices: Vec<String>,
    /// The stored value of each choice, when it is not the choice's index. Empty, or as long as
    /// `choices`.
    pub choice_values: Vec<i64>,
    /// Loaded, saved and readable as usual, but never drawn.
    pub hidden: bool,
    /// Greyed unless this holds.
    pub active_when: Option<ActiveWhen>,
    /// The declaring plugin's display name, shown in the tooltip.
    pub source: Option<String>,
}

impl SettingDef {
    /// A def with every optional field empty.
    pub fn new(
        owner: Owner,
        key: &str,
        kind: Kind,
        category: &str,
        subcategory: &str,
    ) -> SettingDef {
        SettingDef {
            owner,
            key: key.to_owned(),
            kind,
            category: category.to_owned(),
            subcategory: subcategory.to_owned(),
            label: key.to_owned(),
            description: String::new(),
            default_int: 0,
            min: 0,
            max: 0,
            default_str: String::new(),
            choices: Vec::new(),
            choice_values: Vec::new(),
            hidden: false,
            active_when: None,
            source: None,
        }
    }

    pub fn label(mut self, label: &str) -> SettingDef {
        self.label = label.to_owned();
        self
    }

    pub fn describe(mut self, description: &str) -> SettingDef {
        self.description = description.to_owned();
        self
    }

    pub fn default(mut self, value: i64) -> SettingDef {
        self.default_int = value;
        self
    }

    pub fn range(mut self, min: i64, max: i64) -> SettingDef {
        self.min = min;
        self.max = max;
        self
    }

    pub fn choices(mut self, labels: &[&str]) -> SettingDef {
        self.choices = labels.iter().map(|l| (*l).to_owned()).collect();
        self.min = 0;
        self.max = self.choices.len() as i64 - 1;
        self
    }

    pub fn hidden(mut self, hidden: bool) -> SettingDef {
        self.hidden = hidden;
        self
    }

    /// Greyed while the bool setting `key` is off.
    pub fn active_when(self, owner: Owner, key: &str) -> SettingDef {
        self.active_if(owner, key, Condition::On)
    }

    /// Greyed unless setting `key`'s value passes `test`.
    pub fn active_if(mut self, owner: Owner, key: &str, test: Condition) -> SettingDef {
        self.active_when = Some(ActiveWhen {
            owner,
            key: key.to_owned(),
            test,
            shown: None,
        });
        self
    }

    /// A bounded int: drawn as a slider.
    pub fn is_slider(&self) -> bool {
        self.kind == Kind::Int && self.min < self.max
    }

    /// What the setting belongs to, for the tooltip: the game, the mod loader, or the plugin
    /// (its display name, or its id when it has none).
    pub fn source_name(&self) -> String {
        match &self.owner {
            Owner::Game => "Warcraft II: Remastered".to_owned(),
            Owner::Host => "W2R Mod Loader".to_owned(),
            Owner::Plugin(id) => self.source.clone().unwrap_or_else(|| id.clone()),
        }
    }

    /// The dropdown index that `value` corresponds to, or 0 when it matches none.
    pub fn choice_index(&self, value: i64) -> usize {
        if self.choice_values.is_empty() {
            return usize::try_from(value)
                .ok()
                .filter(|&i| i < self.choices.len())
                .unwrap_or(0);
        }
        self.choice_values
            .iter()
            .position(|&v| v == value)
            .unwrap_or(0)
    }

    /// What to store for dropdown index `i`.
    pub fn choice_value(&self, i: usize) -> i64 {
        if self.choice_values.is_empty() {
            i as i64
        } else {
            self.choice_values.get(i).copied().unwrap_or(0)
        }
    }

    /// `value` made valid for this setting: a bool becomes 0 or 1, a slider value is clamped, and
    /// an unknown choice becomes the default.
    pub fn clamp(&self, value: i64) -> i64 {
        match self.kind {
            Kind::Bool => i64::from(value != 0),
            Kind::Int if self.min < self.max => value.clamp(self.min, self.max),
            Kind::Int => value,
            Kind::Choice => {
                let legal = if self.choice_values.is_empty() {
                    (0..self.choices.len() as i64).contains(&value)
                } else {
                    self.choice_values.contains(&value)
                };
                if legal { value } else { self.default_int }
            }
            Kind::Str | Kind::Color => value,
        }
    }

    /// The default as a [`Value`].
    pub fn default_value(&self) -> Value {
        if self.kind.is_text() {
            Value::Str(self.default_str.clone())
        } else {
            Value::Int(self.default_int)
        }
    }
}

/// A value as the screen edits it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value {
    Int(i64),
    Str(String),
}

impl Value {
    pub fn int(&self) -> i64 {
        match self {
            Value::Int(n) => *n,
            Value::Str(_) => 0,
        }
    }

    pub fn str(&self) -> &str {
        match self {
            Value::Int(_) => "",
            Value::Str(s) => s,
        }
    }
}

/// The game's own settings, behind a trait so the registry is testable without the game.
/// `key` is the `Warcraft2.ini` key (or a derived setting's own key).
pub trait Native {
    fn get(&self, key: &str) -> i64;
    fn get_str(&self, key: &str) -> String;
    /// Writes the game's value and applies it, as the stock options tab does.
    fn set(&mut self, key: &str, value: i64);
    /// Saves `Warcraft2.ini` with the game's own writer.
    fn persist(&mut self);
}

/// No game settings: tests, or a game whose settings table failed validation.
pub struct NoNative;

impl Native for NoNative {
    fn get(&self, _: &str) -> i64 {
        0
    }
    fn get_str(&self, _: &str) -> String {
        String::new()
    }
    fn set(&mut self, _: &str, _: i64) {}
    fn persist(&mut self) {}
}

struct Record {
    def: SettingDef,
    /// The working value of a host or plugin setting; unused for a game setting.
    value: Value,
}

/// Every setting on the screen.
#[derive(Default)]
pub struct Registry {
    records: Vec<Record>,
}

impl Registry {
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Adds a setting holding `value` (ignored for a game setting) and returns its index.
    /// Registering the same owner and key again returns the existing index.
    pub fn register(&mut self, mut def: SettingDef, value: Value) -> usize {
        if let Some(i) = self.find(&def.owner, &def.key) {
            return i;
        }
        if def.subcategory.is_empty() {
            def.subcategory = DEFAULT_SUBCATEGORY.to_owned();
        }
        if def.label.is_empty() {
            def.label = def.key.clone();
        }
        let value = match def.kind {
            Kind::Str => Value::Str(value.str().to_owned()),
            Kind::Color => Value::Str(
                crate::color::normalize(value.str()).unwrap_or_else(|| def.default_str.clone()),
            ),
            _ => Value::Int(def.clamp(value.int())),
        };
        self.records.push(Record { def, value });
        self.records.len() - 1
    }

    pub fn len(&self) -> usize {
        self.records.len()
    }

    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }

    pub fn def(&self, i: usize) -> &SettingDef {
        &self.records[i].def
    }

    pub fn find(&self, owner: &Owner, key: &str) -> Option<usize> {
        self.records
            .iter()
            .position(|r| &r.def.owner == owner && r.def.key.eq_ignore_ascii_case(key))
    }

    pub fn get(&self, i: usize, native: &dyn Native) -> Value {
        let r = &self.records[i];
        match (&r.def.owner, r.def.kind) {
            (Owner::Game, Kind::Str) => Value::Str(native.get_str(&r.def.key)),
            (Owner::Game, _) => Value::Int(native.get(&r.def.key)),
            _ => r.value.clone(),
        }
    }

    /// Sets a value, made valid with [`SettingDef::clamp`]. A game string cannot be set: there
    /// is no writer for it.
    pub fn set(&mut self, i: usize, value: Value, native: &mut dyn Native) {
        let r = &mut self.records[i];
        match (&r.def.owner, r.def.kind) {
            (Owner::Game, Kind::Str) => {}
            (Owner::Game, _) => native.set(&r.def.key, r.def.clamp(value.int())),
            (_, Kind::Str) => {
                let mut s = value.str().to_owned();
                truncate_utf8(&mut s, W2R_MAX_SETTING_STR as usize);
                r.value = Value::Str(s);
            }
            // Anything that is not six hex digits leaves the value as it was.
            (_, Kind::Color) => {
                if let Some(hex) = crate::color::normalize(value.str()) {
                    r.value = Value::Str(hex);
                }
            }
            _ => r.value = Value::Int(r.def.clamp(value.int())),
        }
    }

    /// Makes a setting a dropdown over `labels`, choice `i` storing `i`. An empty list does
    /// nothing.
    pub fn set_choices(&mut self, i: usize, labels: Vec<String>) {
        if labels.is_empty() {
            return;
        }
        let d = &mut self.records[i].def;
        d.kind = Kind::Choice;
        d.min = 0;
        d.max = labels.len() as i64 - 1;
        d.choices = labels;
        d.choice_values.clear();
    }

    /// The categories with at least one shown setting, sorted case-insensitively.
    pub fn categories(&self) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for r in self.records.iter().filter(|r| !r.def.hidden) {
            if !out.iter().any(|c| c.eq_ignore_ascii_case(&r.def.category)) {
                out.push(r.def.category.clone());
            }
        }
        out.sort_by_key(|c| c.to_ascii_lowercase());
        out
    }

    /// One category's shown settings, grouped by subcategory. Subcategories keep the order they
    /// were first registered in, which keeps related settings together, except where
    /// `HEADING_ORDER` places a heading a plugin registers between the host's own.
    pub fn groups(&self, category: &str) -> Vec<(String, Vec<usize>)> {
        let mut groups = self.groups_as_registered(category);
        if let Some((_, order)) = HEADING_ORDER.iter().find(|(c, _)| c.eq_ignore_ascii_case(category)) {
            let rank = |name: &str| {
                order.iter().position(|o| o.eq_ignore_ascii_case(name)).unwrap_or(order.len())
            };
            // Stable: headings not listed keep their registration order, after the listed ones.
            groups.sort_by_key(|(name, _)| rank(name));
        }
        groups
    }

    fn groups_as_registered(&self, category: &str) -> Vec<(String, Vec<usize>)> {
        let mut groups: Vec<(String, Vec<usize>)> = Vec::new();
        for (i, r) in self.records.iter().enumerate() {
            if r.def.hidden || !r.def.category.eq_ignore_ascii_case(category) {
                continue;
            }
            match groups
                .iter_mut()
                .find(|(name, _)| name.eq_ignore_ascii_case(&r.def.subcategory))
            {
                Some((_, items)) => items.push(i),
                None => groups.push((r.def.subcategory.clone(), vec![i])),
            }
        }
        groups
    }

    /// Every shown setting whose displayed label, stored label, key or category contains
    /// `query`, case-insensitively. An empty query matches nothing.
    pub fn search(&self, query: &str, shown_label: &dyn Fn(&SettingDef) -> String) -> Vec<usize> {
        if query.is_empty() {
            return Vec::new();
        }
        let q = query.to_ascii_lowercase();
        let hit = |text: &str| text.to_ascii_lowercase().contains(&q);
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| !r.def.hidden)
            .filter(|(_, r)| {
                hit(&shown_label(&r.def))
                    || hit(&r.def.label)
                    || hit(&r.def.key)
                    || hit(&r.def.category)
            })
            .map(|(i, _)| i)
            .collect()
    }

    /// Whether a setting is greyed: its `active_when` condition does not hold. A condition on a
    /// setting that is not registered never greys anything.
    pub fn inactive(&self, i: usize, native: &dyn Native) -> bool {
        let Some(when) = &self.records[i].def.active_when else {
            return false;
        };
        self.find(&when.owner, &when.key)
            .is_some_and(|s| !when.test.holds(self.get(s, native).int()))
    }

    /// What a greyed checkbox row shows instead of its own value (a locked row), if anything.
    pub fn locked_value(&self, i: usize, native: &dyn Native) -> Option<i64> {
        let when = self.records[i].def.active_when.as_ref()?;
        when.shown.filter(|_| self.inactive(i, native))
    }

    /// Every value as it is now, for undoing the screen's edits.
    pub fn snapshot(&self, native: &dyn Native) -> Vec<Value> {
        (0..self.records.len())
            .map(|i| self.get(i, native))
            .collect()
    }

    /// Puts back what changed since `snapshot`, and only that: setting a game value also applies
    /// it, and some of those restart things (`remastered_music` restarts the music).
    pub fn restore(&mut self, snapshot: &[Value], native: &mut dyn Native) {
        for (i, old) in snapshot.iter().enumerate().take(self.records.len()) {
            if self.get(i, native) != *old {
                self.set(i, old.clone(), native);
            }
        }
    }

    /// The host and plugin settings whose value differs from `baseline`, for the session to
    /// write back to their owners.
    pub fn changed_since(&self, baseline: &[Value]) -> Vec<usize> {
        self.records
            .iter()
            .enumerate()
            .filter(|(_, r)| r.def.owner != Owner::Game)
            .filter(|(i, r)| baseline.get(*i) != Some(&r.value))
            .map(|(i, _)| i)
            .collect()
    }
}

/// Where the host's and the plugins' values live, behind a trait so the editor is testable.
pub trait Owners {
    /// The stored value of a host or plugin setting; `None` means "use the default".
    fn load(&self, def: &SettingDef) -> Option<Value>;
    /// Writes the host and plugin values that changed back to their owners, once per apply.
    fn store(&mut self, changed: &[(&SettingDef, &Value)]);
}

/// The screen's edits. Host and plugin values reach their owners only on Apply, so a plugin
/// never sees an unfinished edit; game values are written at once, as in the stock menu, and
/// Cancel puts them back.
#[derive(Default)]
pub struct Editor {
    pub registry: Registry,
    /// What Cancel restores: the values at entry, or at the last Apply.
    snapshot: Vec<Value>,
    /// What the owners hold: the host and plugin values as last loaded or stored.
    baseline: Vec<Value>,
    /// Set by every edit, cleared on entry and on Apply.
    dirty: bool,
}

impl Editor {
    pub fn new(registry: Registry) -> Editor {
        Editor {
            registry,
            ..Editor::default()
        }
    }

    /// A new visit to the screen: host and plugin values are read again from their owners,
    /// since they may have changed, and become the baseline.
    pub fn enter(&mut self, native: &dyn Native, owners: &dyn Owners) {
        self.load_owned(owners);
        self.snapshot = self.registry.snapshot(native);
        self.baseline = self.snapshot.clone();
        self.dirty = false;
    }

    /// Reads every host and plugin value from its owner (the game's own values are read live).
    fn load_owned(&mut self, owners: &dyn Owners) {
        for i in 0..self.registry.len() {
            let def = self.registry.def(i);
            if def.owner == Owner::Game {
                continue;
            }
            let value = owners.load(def).unwrap_or_else(|| def.default_value());
            if def.kind == Kind::Color && crate::color::normalize(value.str()).is_none() {
                let fallback = def.default_value();
                self.registry.set(i, fallback, &mut NoNative);
                continue;
            }
            self.registry.set(i, value, &mut NoNative);
        }
    }

    pub fn get(&self, i: usize, native: &dyn Native) -> Value {
        self.registry.get(i, native)
    }

    /// One edit from the screen.
    pub fn set(&mut self, i: usize, value: Value, native: &mut dyn Native) {
        self.registry.set(i, value, native);
        self.dirty = true;
    }

    pub fn dirty(&self) -> bool {
        self.dirty
    }

    /// Writes what changed to its owners and the game's settings to `Warcraft2.ini`. The applied
    /// state becomes what Cancel returns to.
    pub fn apply(&mut self, native: &mut dyn Native, owners: &mut dyn Owners) {
        let changed = self.registry.changed_since(&self.baseline);
        if !changed.is_empty() {
            let current = self.registry.snapshot(native);
            let pairs: Vec<(&SettingDef, &Value)> = changed
                .iter()
                .map(|&i| (self.registry.def(i), &current[i]))
                .collect();
            owners.store(&pairs);
            // Owners may change values while they store (a plugin reacting to
            // W2R_EVT_SETTINGS_CHANGED, for example clearing a one-shot checkbox or applying a
            // profile's colors): show what they hold now, instead of the screen's own copy until
            // the next visit.
            self.load_owned(&*owners);
        }
        native.persist();
        self.snapshot = self.registry.snapshot(native);
        self.baseline = self.snapshot.clone();
        self.dirty = false;
    }

    /// Puts back everything changed since entry or the last Apply, then applies.
    pub fn cancel(&mut self, native: &mut dyn Native, owners: &mut dyn Owners) {
        let snapshot = std::mem::take(&mut self.snapshot);
        self.registry.restore(&snapshot, native);
        self.snapshot = snapshot;
        self.apply(native, owners);
    }
}

/// Cuts `s` to at most `max` bytes on a character boundary.
pub fn truncate_utf8(s: &mut String, max: usize) {
    if s.len() <= max {
        return;
    }
    let mut end = max;
    while !s.is_char_boundary(end) {
        end -= 1;
    }
    s.truncate(end);
}

// ---- what a plugin declares ----------------------------------------------------------------

/// A `W2rSettingDef` with its strings copied out.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeclaredSetting {
    pub kind: u32,
    pub flags: u32,
    pub key: String,
    pub label: String,
    pub description: String,
    pub category: String,
    pub subcategory: String,
    pub default_int: i64,
    pub min: i64,
    pub max: i64,
    pub default_str: String,
    pub choices: Vec<String>,
    pub active_when: String,
}

/// `key!=N`, `key=N`, either with `:V`: the key, the condition the row is active under, and the
/// value a checkbox row shows while greyed. `None` for anything else (a plain key).
fn parse_compare(s: &str) -> Option<(&str, Condition, Option<i64>)> {
    let (cond, shown) = match s.split_once(':') {
        Some((c, v)) => (c, Some(v.trim().parse::<i64>().ok()?)),
        None => (s, None),
    };
    let (key, test) = if let Some((k, n)) = cond.split_once("!=") {
        (k, Condition::IsNot(n.trim().parse().ok()?))
    } else {
        let (k, n) = cond.split_once('=')?;
        (k, Condition::Is(n.trim().parse().ok()?))
    };
    let key = key.trim();
    (!key.is_empty()).then_some((key, test, shown))
}

/// Turns one declaration into a [`SettingDef`] for plugin `id` (display name `name`), or the status
/// `settings_declare` returns (docs/abi.md, "Declared settings"). `earlier` are the plugin's
/// declarations so far; `valid_key` is the settings-key rule.
pub fn validate(
    d: DeclaredSetting,
    id: &str,
    name: &str,
    earlier: &[SettingDef],
    host_bools: &[&str],
    valid_key: impl Fn(&str) -> bool,
) -> Result<SettingDef, W2rStatus> {
    if !valid_key(&d.key) {
        return Err(W2R_E_INVALID);
    }
    if earlier.iter().any(|e| e.key == d.key) {
        return Err(W2R_E_CONFLICT);
    }
    if d.flags & !W2R_SETTING_HIDDEN != 0 {
        return Err(W2R_E_INVALID);
    }
    let text = |s: &str, max: u32, required: bool| -> Result<(), W2rStatus> {
        if s.len() > max as usize {
            return Err(W2R_E_LIMIT);
        }
        // The screen draws through the game's font atlas, which has no glyphs past ASCII.
        if (required && s.is_empty()) || !s.bytes().all(|b| (0x20..0x7f).contains(&b)) {
            return Err(W2R_E_INVALID);
        }
        Ok(())
    };
    text(&d.label, W2R_MAX_SETTING_LABEL, true)?;
    text(&d.description, W2R_MAX_SETTING_DESCRIPTION, false)?;
    text(&d.category, W2R_MAX_SETTING_CATEGORY, false)?;
    text(&d.subcategory, W2R_MAX_SETTING_CATEGORY, false)?;
    let kind = match d.kind {
        W2R_SETTING_BOOL => Kind::Bool,
        W2R_SETTING_INT => Kind::Int,
        W2R_SETTING_CHOICE => Kind::Choice,
        W2R_SETTING_STRING => Kind::Str,
        W2R_SETTING_COLOR => Kind::Color,
        _ => return Err(W2R_E_INVALID),
    };
    match kind {
        Kind::Bool if !(0..=1).contains(&d.default_int) => return Err(W2R_E_INVALID),
        Kind::Int if d.min > d.max => return Err(W2R_E_INVALID),
        Kind::Int if d.min < d.max && !(d.min..=d.max).contains(&d.default_int) => {
            return Err(W2R_E_INVALID);
        }
        Kind::Choice => {
            if d.choices.len() > W2R_MAX_SETTING_CHOICE_COUNT as usize {
                return Err(W2R_E_LIMIT);
            }
            if d.choices.len() < 2 {
                return Err(W2R_E_INVALID);
            }
            for c in &d.choices {
                text(c, W2R_MAX_SETTING_CHOICE, true)?;
            }
            if !(0..d.choices.len() as i64).contains(&d.default_int) {
                return Err(W2R_E_INVALID);
            }
        }
        Kind::Str if d.default_str.len() > W2R_MAX_SETTING_STR as usize => {
            return Err(W2R_E_LIMIT);
        }
        Kind::Color if crate::color::normalize(&d.default_str).is_none() => {
            return Err(W2R_E_INVALID);
        }
        _ => {}
    }
    let active_when = if d.active_when.is_empty() {
        None
    } else if let Some(host_key) = d.active_when.strip_prefix(HOST_PREFIX) {
        if !host_bools.contains(&host_key) {
            return Err(W2R_E_NOT_FOUND);
        }
        Some(ActiveWhen {
            owner: Owner::Host,
            key: host_key.to_owned(),
            test: Condition::On,
            shown: None,
        })
    } else if d.active_when.contains('=') {
        let Some((key, test, shown)) = parse_compare(&d.active_when) else {
            return Err(W2R_E_INVALID);
        };
        // `key!=N` (or `key=N`) on an earlier bool, int or choice, and `:V` for
        // what a checkbox row shows while greyed (Pause Pro: Pause Box locked on with Recolor).
        let Some(e) = earlier.iter().find(|e| e.key == key) else {
            return Err(W2R_E_NOT_FOUND);
        };
        let n = match test {
            Condition::Is(n) | Condition::IsNot(n) => n,
            Condition::On => return Err(W2R_E_INVALID),
        };
        let fits = match e.kind {
            Kind::Bool => (0..=1).contains(&n),
            Kind::Int => true,
            Kind::Choice => (0..e.choices.len() as i64).contains(&n),
            _ => false,
        };
        if !fits || shown.is_some_and(|v| kind != Kind::Bool || !(0..=1).contains(&v)) {
            return Err(W2R_E_INVALID);
        }
        Some(ActiveWhen {
            owner: Owner::Plugin(id.to_owned()),
            key: key.to_owned(),
            test,
            shown,
        })
    } else {
        match earlier.iter().find(|e| e.key == d.active_when) {
            Some(e) if e.kind == Kind::Bool => Some(ActiveWhen {
                owner: Owner::Plugin(id.to_owned()),
                key: e.key.clone(),
                test: Condition::On,
                shown: None,
            }),
            Some(_) => return Err(W2R_E_INVALID),
            None => return Err(W2R_E_NOT_FOUND),
        }
    };
    let category = if d.category.is_empty() {
        name.to_owned()
    } else {
        d.category
    };
    let mut def = SettingDef::new(
        Owner::Plugin(id.to_owned()),
        &d.key,
        kind,
        &category,
        &d.subcategory,
    )
    .label(&d.label)
    .describe(&d.description)
    .default(d.default_int)
    .hidden(d.flags & W2R_SETTING_HIDDEN != 0);
    match kind {
        Kind::Int => def = def.range(d.min, d.max),
        Kind::Choice => {
            def.min = 0;
            def.max = d.choices.len() as i64 - 1;
            def.choices = d.choices;
        }
        Kind::Str => def.default_str = d.default_str,
        Kind::Color => {
            def.default_str = crate::color::normalize(&d.default_str).unwrap_or_default();
        }
        Kind::Bool => {}
    }
    def.active_when = active_when;
    def.source = Some(name.to_owned());
    Ok(def)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A game behind the trait: values by key, with the writes recorded.
    #[derive(Default)]
    struct FakeGame {
        values: BTreeMap<String, i64>,
        writes: Vec<(String, i64)>,
        persisted: u32,
    }

    impl Native for FakeGame {
        fn get(&self, key: &str) -> i64 {
            self.values.get(key).copied().unwrap_or(0)
        }
        fn get_str(&self, key: &str) -> String {
            format!("str:{key}")
        }
        fn set(&mut self, key: &str, value: i64) {
            self.values.insert(key.to_owned(), value);
            self.writes.push((key.to_owned(), value));
        }
        fn persist(&mut self) {
            self.persisted += 1;
        }
    }

    fn plugin(id: &str) -> Owner {
        Owner::Plugin(id.to_owned())
    }

    fn sample() -> Registry {
        let mut r = Registry::new();
        r.register(
            SettingDef::new(Owner::Game, "music", Kind::Int, "Sound", "Volume").range(0, 100),
            Value::Int(0),
        );
        r.register(
            SettingDef::new(plugin("w2r-chat"), "fade", Kind::Int, "Chat", "Behavior")
                .range(0, 60)
                .default(7),
            Value::Int(7),
        );
        r.register(
            SettingDef::new(plugin("w2r-chat"), "channels", Kind::Bool, "Chat", "Style").default(1),
            Value::Int(1),
        );
        r.register(
            SettingDef::new(
                Owner::Host,
                "map_sharing",
                Kind::Bool,
                "Multiplayer",
                "Map Sharing",
            ),
            Value::Int(1),
        );
        r.register(
            SettingDef::new(plugin("w2r-chat"), "lines", Kind::Int, "chat", "behavior")
                .range(1, 20),
            Value::Int(8),
        );
        r.register(
            SettingDef::new(plugin("w2r-chat"), "secret", Kind::Bool, "Debug", "X").hidden(true),
            Value::Int(0),
        );
        r
    }

    #[test]
    fn categories_are_sorted_case_insensitively_and_skip_hidden_ones() {
        let r = sample();
        assert_eq!(r.categories(), vec!["Chat", "Multiplayer", "Sound"]);
    }

    #[test]
    fn groups_merge_a_subcategory_whatever_the_registration_order() {
        let r = sample();
        let g = r.groups("CHAT");
        assert_eq!(g.len(), 2);
        assert_eq!(g[0].0, "Behavior");
        assert_eq!(g[0].1, vec![1, 4]);
        assert_eq!(g[1].0, "Style");
        assert_eq!(g[1].1, vec![2]);
    }

    #[test]
    fn pause_ui_sits_after_fixes_whatever_the_registration_order() {
        let mut r = Registry::new();
        for (owner, key, sub) in [
            (Owner::Host, "enhanced_create_screen", "Enhanced UIs"),
            (Owner::Host, "hide_scrollbar_arrows", "Fixes"),
            (Owner::Host, "apm_display", "APM Counter"),
            (plugin("w2r-other"), "x", "Other Plugin"),
            (plugin("w2r-pause-pro"), "paused_screen", "Pause Pro UI"),
            (plugin("w2r-pause-pro"), "pause_box", "Pause Pro UI"),
        ] {
            r.register(SettingDef::new(owner, key, Kind::Bool, "UI", sub), Value::Int(0));
        }
        let names: Vec<String> = r.groups("ui").into_iter().map(|(n, _)| n).collect();
        assert_eq!(names, ["Enhanced UIs", "Fixes", "Pause Pro UI", "APM Counter", "Other Plugin"]);
        assert_eq!(r.groups("UI")[2].1.len(), 2, "both Pause Pro UI settings under the one heading");
        let chat: Vec<String> = sample().groups("Chat").into_iter().map(|(n, _)| n).collect();
        assert_eq!(chat, ["Behavior", "Style"], "other categories keep registration order");
    }

    #[test]
    fn registering_twice_returns_the_first_index() {
        let mut r = sample();
        let n = r.len();
        let again = r.register(
            SettingDef::new(plugin("w2r-chat"), "FADE", Kind::Int, "Other", "Other"),
            Value::Int(1),
        );
        assert_eq!(again, 1);
        assert_eq!(r.len(), n);
    }

    #[test]
    fn values_are_clamped_on_register_and_set() {
        let mut r = sample();
        let mut g = FakeGame::default();
        let fade = r.find(&plugin("w2r-chat"), "fade").unwrap();
        r.set(fade, Value::Int(500), &mut g);
        assert_eq!(r.get(fade, &g), Value::Int(60));
        let channels = r.find(&plugin("w2r-chat"), "channels").unwrap();
        r.set(channels, Value::Int(7), &mut g);
        assert_eq!(r.get(channels, &g), Value::Int(1));
        let i = r.register(
            SettingDef::new(plugin("p"), "n", Kind::Int, "C", "S").range(0, 10),
            Value::Int(-4),
        );
        assert_eq!(r.get(i, &g), Value::Int(0));
    }

    #[test]
    fn game_values_go_through_the_game_and_strings_are_read_only() {
        let mut r = Registry::new();
        let mut g = FakeGame::default();
        let music = r.register(
            SettingDef::new(Owner::Game, "music", Kind::Int, "Sound", "Volume").range(0, 100),
            Value::Int(0),
        );
        let name = r.register(
            SettingDef::new(Owner::Game, "name", Kind::Str, "Preferences", "Player"),
            Value::Str(String::new()),
        );
        r.set(music, Value::Int(140), &mut g);
        assert_eq!(g.writes, vec![("music".to_owned(), 100)]);
        assert_eq!(r.get(music, &g), Value::Int(100));
        r.set(name, Value::Str("x".into()), &mut g);
        assert_eq!(g.writes.len(), 1);
        assert_eq!(r.get(name, &g), Value::Str("str:name".into()));
    }

    #[test]
    fn restore_writes_only_what_changed() {
        let mut r = sample();
        let mut g = FakeGame::default();
        g.values.insert("music".into(), 50);
        let snap = r.snapshot(&g);
        r.restore(&snap, &mut g);
        assert!(g.writes.is_empty(), "nothing edited, nothing written");
        let music = r.find(&Owner::Game, "music").unwrap();
        let fade = r.find(&plugin("w2r-chat"), "fade").unwrap();
        r.set(music, Value::Int(10), &mut g);
        r.set(fade, Value::Int(3), &mut g);
        r.restore(&snap, &mut g);
        assert_eq!(g.get("music"), 50);
        assert_eq!(r.get(fade, &g), Value::Int(7));
        assert_eq!(g.writes.len(), 2);
    }

    #[test]
    fn changed_since_lists_host_and_plugin_settings_only() {
        let mut r = sample();
        let mut g = FakeGame::default();
        let base = r.snapshot(&g);
        let music = r.find(&Owner::Game, "music").unwrap();
        let sharing = r.find(&Owner::Host, "map_sharing").unwrap();
        r.set(music, Value::Int(10), &mut g);
        r.set(sharing, Value::Int(0), &mut g);
        assert_eq!(r.changed_since(&base), vec![sharing]);
    }

    #[test]
    fn active_when_reads_the_live_switch() {
        let mut r = sample();
        let mut g = FakeGame::default();
        let fade = r.register(
            SettingDef::new(plugin("p"), "a", Kind::Int, "C", "S")
                .active_when(Owner::Host, "map_sharing"),
            Value::Int(0),
        );
        assert!(!r.inactive(fade, &g));
        let sharing = r.find(&Owner::Host, "map_sharing").unwrap();
        r.set(sharing, Value::Int(0), &mut g);
        assert!(r.inactive(fade, &g));
    }

    #[test]
    fn conditions_compare_the_live_value() {
        let mut r = Registry::new();
        let mut g = FakeGame::default();
        let mode = || SettingDef::new(Owner::Game, "mode", Kind::Choice, "P", "S");
        r.register(mode(), Value::Int(0));
        let only_one = r.register(
            SettingDef::new(Owner::Game, "a", Kind::Int, "P", "S").active_if(
                Owner::Game,
                "mode",
                Condition::Is(1),
            ),
            Value::Int(0),
        );
        let not_one = r.register(
            SettingDef::new(Owner::Game, "b", Kind::Int, "P", "S").active_if(
                Owner::Game,
                "mode",
                Condition::IsNot(1),
            ),
            Value::Int(0),
        );
        let orphan = r.register(
            SettingDef::new(Owner::Game, "c", Kind::Int, "P", "S").active_if(
                Owner::Game,
                "missing",
                Condition::Is(1),
            ),
            Value::Int(0),
        );
        assert!(r.inactive(only_one, &g));
        assert!(!r.inactive(not_one, &g));
        g.set("mode", 1);
        assert!(!r.inactive(only_one, &g));
        assert!(r.inactive(not_one, &g));
        assert!(!r.inactive(orphan, &g), "an unknown switch greys nothing");
    }

    #[test]
    fn search_matches_label_key_and_category_and_skips_hidden() {
        let r = sample();
        let shown = |d: &SettingDef| {
            if d.label == "music" {
                "Music Volume".to_owned()
            } else {
                d.label.clone()
            }
        };
        assert!(r.search("", &shown).is_empty());
        assert_eq!(r.search("volume", &shown), vec![0]);
        assert_eq!(r.search("CHAT", &shown), vec![1, 2, 4]);
        assert!(r.search("secret", &shown).is_empty());
    }

    #[test]
    fn choices_map_between_index_and_stored_value() {
        let mut d = SettingDef::new(Owner::Game, "ui_scale_mode", Kind::Choice, "P", "U")
            .choices(&["classic", "auto", "fixed"]);
        d.choice_values = vec![2, 0, 1];
        assert_eq!(d.choice_index(0), 1);
        assert_eq!(d.choice_index(9), 0);
        assert_eq!(d.choice_value(2), 1);
        assert_eq!(d.clamp(1), 1);
        assert_eq!(d.clamp(5), d.default_int);
        let plain = SettingDef::new(plugin("p"), "c", Kind::Choice, "C", "S").choices(&["a", "b"]);
        assert_eq!(plain.choice_index(1), 1);
        assert_eq!(plain.choice_index(-1), 0);
        assert_eq!(plain.choice_value(1), 1);
    }

    #[test]
    fn set_choices_turns_a_setting_into_an_identity_dropdown() {
        let mut r = Registry::new();
        let i = r.register(
            SettingDef::new(
                Owner::Game,
                "display_monitor",
                Kind::Int,
                "Preferences",
                "Display",
            )
            .range(0, 8),
            Value::Int(0),
        );
        r.set_choices(i, vec![]);
        assert_eq!(r.def(i).kind, Kind::Int);
        r.set_choices(i, vec!["Display 1".into(), "Display 2".into()]);
        assert_eq!(r.def(i).kind, Kind::Choice);
        assert_eq!(r.def(i).max, 1);
    }

    #[test]
    fn the_source_names_the_game_the_loader_or_the_plugin() {
        let game = SettingDef::new(Owner::Game, "music", Kind::Int, "Sound", "Volume");
        assert_eq!(game.source_name(), "Warcraft II: Remastered");
        let host = SettingDef::new(Owner::Host, "map_sharing", Kind::Bool, "M", "S");
        assert_eq!(host.source_name(), "W2R Mod Loader");
        let mut d = SettingDef::new(plugin("w2r-chat"), "fade", Kind::Int, "Chat", "Behavior");
        assert_eq!(d.source_name(), "w2r-chat");
        d.source = Some("Chat".into());
        assert_eq!(d.source_name(), "Chat");
    }

    fn decl(kind: u32, key: &str) -> DeclaredSetting {
        DeclaredSetting {
            kind,
            key: key.into(),
            label: "Label".into(),
            ..DeclaredSetting::default()
        }
    }

    fn check(d: DeclaredSetting, earlier: &[SettingDef]) -> Result<SettingDef, W2rStatus> {
        validate(
            d,
            "w2r-chat",
            "Chat",
            earlier,
            &["enhanced_create_screen"],
            crate::store::valid_key,
        )
    }

    #[test]
    fn a_declaration_becomes_a_def() {
        let mut d = decl(W2R_SETTING_INT, "fade");
        d.category = "Chat".into();
        d.subcategory = "Behavior".into();
        d.min = 0;
        d.max = 60;
        d.default_int = 7;
        d.description = "Seconds.".into();
        let def = check(d, &[]).unwrap();
        assert_eq!(def.owner, plugin("w2r-chat"));
        assert!(def.is_slider());
        assert_eq!(def.default_int, 7);
        assert_eq!(def.source.as_deref(), Some("Chat"));
        let bare = check(decl(W2R_SETTING_BOOL, "x"), &[]).unwrap();
        assert_eq!(bare.category, "Chat", "empty category is the plugin's name");
        assert_eq!(bare.subcategory, "", "the registry fills General in");
    }

    #[test]
    fn declarations_are_validated() {
        assert_eq!(
            check(decl(W2R_SETTING_BOOL, "Bad Key"), &[]),
            Err(W2R_E_INVALID)
        );
        assert_eq!(check(decl(9, "k"), &[]), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_BOOL, "k");
        d.label.clear();
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_BOOL, "k");
        d.label = "x".repeat(65);
        assert_eq!(check(d, &[]), Err(W2R_E_LIMIT));
        let mut d = decl(W2R_SETTING_BOOL, "k");
        d.description = "caf\u{e9}".into();
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID), "ASCII only");
        let mut d = decl(W2R_SETTING_BOOL, "k");
        d.default_int = 2;
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_INT, "k");
        d.min = 5;
        d.max = 1;
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_INT, "k");
        d.min = 0;
        d.max = 10;
        d.default_int = 11;
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_CHOICE, "k");
        d.choices = vec!["one".into()];
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_CHOICE, "k");
        d.choices = (0..17).map(|i| i.to_string()).collect();
        assert_eq!(check(d, &[]), Err(W2R_E_LIMIT));
        let mut d = decl(W2R_SETTING_CHOICE, "k");
        d.choices = vec!["a".into(), "b".into()];
        d.default_int = 2;
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_BOOL, "k");
        d.flags = 2;
        assert_eq!(check(d, &[]), Err(W2R_E_INVALID));
    }

    #[test]
    fn a_key_is_declared_once() {
        let first = check(decl(W2R_SETTING_BOOL, "k"), &[]).unwrap();
        assert_eq!(
            check(decl(W2R_SETTING_INT, "k"), &[first]),
            Err(W2R_E_CONFLICT)
        );
    }

    #[test]
    fn active_when_names_an_earlier_bool_or_a_host_bool() {
        let on = check(decl(W2R_SETTING_BOOL, "on"), &[]).unwrap();
        let num = check(decl(W2R_SETTING_INT, "num"), &[]).unwrap();
        let earlier = [on, num];
        let mut d = decl(W2R_SETTING_INT, "a");
        d.active_when = "on".into();
        assert_eq!(
            check(d, &earlier).unwrap().active_when,
            Some(ActiveWhen {
                owner: plugin("w2r-chat"),
                key: "on".to_owned(),
                test: Condition::On,
                shown: None,
            })
        );
        let mut d = decl(W2R_SETTING_INT, "b");
        d.active_when = "num".into();
        assert_eq!(check(d, &earlier), Err(W2R_E_INVALID));
        let mut d = decl(W2R_SETTING_INT, "c");
        d.active_when = "later".into();
        assert_eq!(check(d, &earlier), Err(W2R_E_NOT_FOUND));
        let mut d = decl(W2R_SETTING_INT, "d");
        d.active_when = "host:enhanced_create_screen".into();
        assert_eq!(
            check(d, &earlier).unwrap().active_when,
            Some(ActiveWhen {
                owner: Owner::Host,
                key: "enhanced_create_screen".to_owned(),
                test: Condition::On,
                shown: None,
            })
        );
        let mut d = decl(W2R_SETTING_INT, "e");
        d.active_when = "host:nope".into();
        assert_eq!(check(d, &earlier), Err(W2R_E_NOT_FOUND));
    }

    #[test]
    fn active_when_compares_an_earlier_choice_and_can_lock_a_checkbox() {
        let mut screen = decl(W2R_SETTING_CHOICE, "screen");
        screen.choices = vec!["Default".into(), "Recolor".into(), "Tint".into()];
        let earlier = [check(screen, &[]).unwrap()];
        let mut d = decl(W2R_SETTING_BOOL, "box");
        d.active_when = "screen!=1:1".into();
        assert_eq!(
            check(d, &earlier).unwrap().active_when,
            Some(ActiveWhen {
                owner: plugin("w2r-chat"),
                key: "screen".to_owned(),
                test: Condition::IsNot(1),
                shown: Some(1),
            })
        );
        let mut d = decl(W2R_SETTING_INT, "level");
        d.active_when = "screen=2".into();
        assert_eq!(check(d, &earlier).unwrap().active_when.unwrap().test, Condition::Is(2));
        for bad in ["screen!=3", "screen!=x", "screen!=1:2"] {
            let mut d = decl(W2R_SETTING_BOOL, "b");
            d.active_when = bad.into();
            assert_eq!(check(d, &earlier), Err(W2R_E_INVALID), "{bad}");
        }
        let mut d = decl(W2R_SETTING_INT, "i");
        d.active_when = "screen!=1:1".into();
        assert_eq!(check(d, &earlier), Err(W2R_E_INVALID), "only a checkbox can be locked");
        let mut d = decl(W2R_SETTING_BOOL, "c");
        d.active_when = "later!=1".into();
        assert_eq!(check(d, &earlier), Err(W2R_E_NOT_FOUND));
    }

    /// Owners behind the trait: stored values by key, with every store call recorded.
    #[derive(Default)]
    struct FakeOwners {
        stored: BTreeMap<String, Value>,
        stores: Vec<Vec<(String, Value)>>,
    }

    impl Owners for FakeOwners {
        fn load(&self, def: &SettingDef) -> Option<Value> {
            self.stored.get(&def.key).cloned()
        }
        fn store(&mut self, changed: &[(&SettingDef, &Value)]) {
            for (d, v) in changed {
                self.stored.insert(d.key.clone(), (*v).clone());
            }
            self.stores.push(
                changed
                    .iter()
                    .map(|(d, v)| (d.key.clone(), (*v).clone()))
                    .collect(),
            );
        }
    }

    #[test]
    fn entering_loads_owners_values_and_defaults() {
        let mut e = Editor::new(sample());
        let g = FakeGame::default();
        let mut o = FakeOwners::default();
        o.stored.insert("fade".into(), Value::Int(30));
        e.enter(&g, &o);
        let fade = e.registry.find(&plugin("w2r-chat"), "fade").unwrap();
        let lines = e.registry.find(&plugin("w2r-chat"), "lines").unwrap();
        assert_eq!(e.get(fade, &g), Value::Int(30));
        assert_eq!(
            e.get(lines, &g),
            Value::Int(1),
            "no stored value: the default, clamped"
        );
        assert!(!e.dirty());
    }

    #[test]
    fn apply_stores_only_what_changed_and_persists_the_game() {
        let mut e = Editor::new(sample());
        let mut g = FakeGame::default();
        let mut o = FakeOwners::default();
        e.enter(&g, &o);
        e.apply(&mut g, &mut o);
        assert!(o.stores.is_empty(), "nothing changed, nothing stored");
        assert_eq!(g.persisted, 1);
        let fade = e.registry.find(&plugin("w2r-chat"), "fade").unwrap();
        e.set(fade, Value::Int(20), &mut g);
        assert!(e.dirty());
        e.apply(&mut g, &mut o);
        assert!(!e.dirty());
        assert_eq!(o.stores, vec![vec![("fade".to_owned(), Value::Int(20))]]);
        e.apply(&mut g, &mut o);
        assert_eq!(o.stores.len(), 1, "already stored");
    }

    /// Owners that, like a plugin handling the settings event, change another value while
    /// storing: storing "fade" sets "lines" to 3.
    #[derive(Default)]
    struct ReactingOwners(FakeOwners);

    impl Owners for ReactingOwners {
        fn load(&self, def: &SettingDef) -> Option<Value> {
            self.0.load(def)
        }
        fn store(&mut self, changed: &[(&SettingDef, &Value)]) {
            self.0.store(changed);
            if changed.iter().any(|(d, _)| d.key == "fade") {
                self.0.stored.insert("lines".into(), Value::Int(3));
            }
        }
    }

    #[test]
    fn apply_shows_what_owners_changed_while_storing() {
        let mut e = Editor::new(sample());
        let mut g = FakeGame::default();
        let mut o = ReactingOwners::default();
        e.enter(&g, &o);
        let fade = e.registry.find(&plugin("w2r-chat"), "fade").unwrap();
        let lines = e.registry.find(&plugin("w2r-chat"), "lines").unwrap();
        e.set(fade, Value::Int(20), &mut g);
        e.apply(&mut g, &mut o);
        assert_eq!(
            e.get(lines, &g),
            Value::Int(3),
            "the owner's change shows at once"
        );
        assert!(!e.dirty());
        // It is the new baseline: applying again stores nothing more.
        e.apply(&mut g, &mut o);
        assert_eq!(o.0.stores.len(), 1);
    }

    #[test]
    fn cancel_goes_back_to_the_last_apply() {
        let mut e = Editor::new(sample());
        let mut g = FakeGame::default();
        let mut o = FakeOwners::default();
        g.values.insert("music".into(), 40);
        e.enter(&g, &o);
        let fade = e.registry.find(&plugin("w2r-chat"), "fade").unwrap();
        let music = e.registry.find(&Owner::Game, "music").unwrap();
        e.set(fade, Value::Int(20), &mut g);
        e.apply(&mut g, &mut o);
        e.set(fade, Value::Int(50), &mut g);
        e.set(music, Value::Int(90), &mut g);
        e.cancel(&mut g, &mut o);
        assert_eq!(e.get(fade, &g), Value::Int(20));
        assert_eq!(g.get("music"), 40, "the game's value is put back too");
        assert_eq!(
            o.stores.len(),
            1,
            "the cancelled edit never reached the owner"
        );
        assert!(!e.dirty());
    }

    #[test]
    fn colors_are_declared_with_a_hex_default_and_stored_lowercase() {
        let mut d = decl(W2R_SETTING_COLOR, "team_hex");
        d.default_str = "25E65F".into();
        let def = check(d, &[]).unwrap();
        assert_eq!(def.kind, Kind::Color);
        assert_eq!(def.default_str, "25e65f");
        let mut bad = decl(W2R_SETTING_COLOR, "bad");
        bad.default_str = "#25e65f".into();
        assert_eq!(check(bad, &[]), Err(W2R_E_INVALID));

        let mut r = Registry::new();
        let mut g = FakeGame::default();
        let i = r.register(def, Value::Str("not hex".into()));
        assert_eq!(
            r.get(i, &g),
            Value::Str("25e65f".into()),
            "a bad value is the default"
        );
        r.set(i, Value::Str("ABCDEF".into()), &mut g);
        assert_eq!(r.get(i, &g), Value::Str("abcdef".into()));
        r.set(i, Value::Str("nope".into()), &mut g);
        assert_eq!(
            r.get(i, &g),
            Value::Str("abcdef".into()),
            "a bad edit is ignored"
        );
    }

    #[test]
    fn entering_replaces_a_stored_color_that_is_not_hex() {
        let mut d = decl(W2R_SETTING_COLOR, "c");
        d.default_str = "112233".into();
        let mut r = Registry::new();
        let i = r.register(check(d, &[]).unwrap(), Value::Str("445566".into()));
        let mut e = Editor::new(r);
        let mut o = FakeOwners::default();
        o.stored.insert("c".into(), Value::Str("purple".into()));
        e.enter(&FakeGame::default(), &o);
        assert_eq!(e.get(i, &FakeGame::default()), Value::Str("112233".into()));
    }

    #[test]
    fn truncation_keeps_whole_characters() {
        let mut s = "ab\u{e9}".to_owned();
        truncate_utf8(&mut s, 3);
        assert_eq!(s, "ab");
    }
}
