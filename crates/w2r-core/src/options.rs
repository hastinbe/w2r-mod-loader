//! The game's options screen hook: `BuildOptionsMenuScreen` hands both its entries (the main
//! menu's and the pause menu's) to the settings screen (`settingsscreen.rs`,
//! docs/settings-screen.md), and the game's own screen is not built at all. Also the title
//! screen's version lines with the Manage button, the plugin manager popup it opens, and the
//! game's menu widgets behind [`Widgets`], which the popup draws plugin cards and `on_ui` rows
//! with. Layout numbers are in the game's 2160-high UI space. x86 only.

use core::ffi::{c_char, c_void};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use w2r_abi::{W2R_LOG_ERROR, W2R_LOG_INFO};

use crate::guard::guarded;
use crate::hooks::{self, Owner};
use crate::host::Log;
use crate::pe::Image;
use crate::symbols::Symbols;
use crate::ui::Widgets;

pub const SYMBOL: &str = "BuildOptionsMenuScreen";
const ALIGN_ROW: i32 = 0x11;
const ALIGN_TITLE: i32 = 0x12;
const ALIGN_RIGHT: i32 = 0x14;
const WINDOW_NO_SCROLLBAR: u32 = 0x20;
/// Nuklear's NK_WINDOW_NO_INPUT: the window never takes the mouse from what is under it.
const WINDOW_NO_INPUT: u32 = 0x400;
/// Nuklear's NK_WINDOW_BACKGROUND: kept behind every other window, so it never overlaps (and
/// turns read-only) the game's own screen drawn before it.
const WINDOW_BACKGROUND: u32 = 0x100;
/// A group without scrollbars, as the create screen's frames.
const GROUP_CARD: u32 = 0x20;
/// The edit box's flags and the row spacing offset in the game's Nuklear context, as the other
/// screens use them (`screenkit.rs`, wc2r-plus `struct_offsets.h`).
const TEXT_EDIT_FLAGS: u32 = 0x260;
const CTX_SPACING_X: usize = 0x2000;
const CTX_SPACING_Y: usize = 0x2004;
const CTX_FONT: usize = 0x1e0;
const CTX_TEXT_COLOUR: usize = 0x20c;
const CTX_CURRENT_WINDOW: usize = 0x3d28;
const WINDOW_BUFFER: usize = 0x64;
const WINDOW_PANEL: usize = 0x8c;
const PANEL_BOUNDS_W: usize = 0x10;
const USER_FONT_HEIGHT: usize = 0x04;
const CTX_MOUSE_X: usize = 0x1bc;
const CTX_MOUSE_Y: usize = 0x1c0;
/// The left mouse button: non-zero while held, and whether it went down this frame.
const CTX_MOUSE_LEFT_DOWN: usize = 0x17c;
const CTX_MOUSE_LEFT_CLICKED: usize = 0x180;
/// The plugin cards' breathing room: a left margin, a fraction of the row taken by an empty
/// first cell.
const ROW_INSET: f32 = 0.02;
const CARD_INSET: f32 = 0.05;

/// Column ratios for rows of 1 to 8 widgets after the margin cell. The game's layout keeps the
/// pointer it is given and reads it as each widget of the row is placed, so these must outlive
/// the call (a stack array was freed by then and the next widget faulted).
const fn ratio_table(inset: f32) -> [[f32; 10]; 8] {
    let mut table = [[0.0; 10]; 8];
    let mut i = 0;
    while i < 8 {
        let cols = i + 1;
        table[i][0] = inset;
        let mut c = 1;
        while c <= cols {
            table[i][c] = (1.0 - 2.0 * inset) / cols as f32;
            c += 1;
        }
        table[i][cols + 1] = inset;
        i += 1;
    }
    table
}
static ROW_RATIOS: [[f32; 10]; 8] = ratio_table(ROW_INSET);
/// Inside a plugin card the frame's border is drawn over the content, so the margin is wider.
static CARD_RATIOS: [[f32; 10]; 8] = ratio_table(CARD_INSET);
/// After this long without the options screen being built, the next build is a fresh entry.
const REENTRY_GAP_MS: u64 = 250;

type BuildOptionsMenuScreenFn = unsafe extern "C" fn(i32);
type BeginNamedWindowFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, f32, f32, f32, f32, u32) -> i32;
type CtxFn = unsafe extern "C" fn(*mut c_void);
type UiWidthFn = unsafe extern "C" fn() -> f32;
type RectFn = unsafe extern "C" fn(*mut c_void, f32, f32, f32, f32);
type SpaceBeginFn = unsafe extern "C" fn(*mut c_void, i32, f32, i32);
type BeginGroupFn = unsafe extern "C" fn(*mut c_void, *const c_char, u32) -> i32;
type RowFn = unsafe extern "C" fn(*mut c_void, f32, i32);
type RowRatiosFn = unsafe extern "C" fn(*mut c_void, i32, f32, i32, *const f32);
type LabelFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32);
type MenuButtonFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> i8;
type CheckboxFn = unsafe extern "C" fn(*const c_char, *mut u32, u32) -> u8;
type SliderFn = unsafe extern "C" fn(*mut u32, *mut u32, *mut u32, *mut u32) -> u8;
type TextEditFn = unsafe extern "C" fn(*mut c_void, u32, *mut c_char, i32, *mut c_void) -> u32;
type SetSkinFn = unsafe extern "thiscall" fn(*mut c_void, u32) -> u32;
type CountLinesFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32, f32) -> i32;
type WrappedLabelFn = unsafe extern "C" fn(*mut c_void, *const c_char, u32);
type FillRectFn = unsafe extern "C" fn(*mut c_void, f32, f32, f32, f32, f32, u32);
type WidgetSpaceFn = unsafe extern "C" fn(*mut f32, *mut c_void, *mut c_void, i32);

/// The game functions the screen needs, resolved once. Plain pointers, so copies are cheap and
/// the session need not stay borrowed while the screen draws.
#[derive(Clone, Copy)]
pub struct Nk {
    ctx_slot: *mut *mut c_void,
    begin_window: BeginNamedWindowFn,
    end_window: CtxFn,
    ui_width: UiWidthFn,
    rect: RectFn,
    space_begin: SpaceBeginFn,
    space_end: CtxFn,
    begin_group: BeginGroupFn,
    end_group: CtxFn,
    row: RowFn,
    row_ratios: RowRatiosFn,
    spacing: CtxFn,
    label: LabelFn,
    button: MenuButtonFn,
    checkbox: CheckboxFn,
    slider: SliderFn,
    disable_begin: CtxFn,
    disable_end: CtxFn,
    text_edit: TextEditFn,
    text_edit_filter: *mut c_void,
    skin_text_edit: *const u32,
    set_skin: SetSkinFn,
    skin_manager: *mut c_void,
    skin_card: *const u32,
    skin_window: *const u32,
    skin_window_alt: *const u32,
    skin_alt_flag: *const u8,
    skin_content: *const u32,
    count_lines: CountLinesFn,
    wrapped_label: WrappedLabelFn,
    fill_rect: FillRectFn,
    widget_space: WidgetSpaceFn,
}

macro_rules! sym {
    ($symbols:expr, $name:literal, $t:ty) => {
        unsafe { core::mem::transmute::<usize, $t>($symbols.address($name)?) }
    };
}

impl Nk {
    pub fn new(symbols: &Symbols) -> Option<Nk> {
        Some(Nk {
            ctx_slot: symbols.address("NkContextMenus")? as *mut *mut c_void,
            begin_window: sym!(symbols, "BeginNamedWindow", BeginNamedWindowFn),
            end_window: sym!(symbols, "EndCurrentWindow", CtxFn),
            ui_width: sym!(symbols, "GetUiSpaceWidth", UiWidthFn),
            rect: sym!(symbols, "OverrideNextRect", RectFn),
            space_begin: sym!(symbols, "LayoutSpaceBegin", SpaceBeginFn),
            space_end: sym!(symbols, "LayoutSpaceEnd", CtxFn),
            begin_group: sym!(symbols, "BeginNamedGroup", BeginGroupFn),
            end_group: sym!(symbols, "EndGroup", CtxFn),
            row: sym!(symbols, "LayoutRowDynamic", RowFn),
            row_ratios: sym!(symbols, "LayoutRowRatios", RowRatiosFn),
            spacing: sym!(symbols, "Spacing", CtxFn),
            label: sym!(symbols, "DrawLabel", LabelFn),
            button: sym!(symbols, "DrawMenuButton", MenuButtonFn),
            checkbox: sym!(symbols, "FlagCheckbox", CheckboxFn),
            slider: sym!(symbols, "SliderU32", SliderFn),
            disable_begin: sym!(symbols, "DisableBegin", CtxFn),
            disable_end: sym!(symbols, "DisableEnd", CtxFn),
            text_edit: sym!(symbols, "TextEdit", TextEditFn),
            text_edit_filter: symbols.address("TextEditFilter")? as *mut c_void,
            skin_text_edit: symbols.address("SkinTextEdit")? as *const u32,
            set_skin: sym!(symbols, "SetSkin", SetSkinFn),
            skin_manager: symbols.address("SkinManager")? as *mut c_void,
            skin_card: symbols.address("SkinLobbyCreationLeft")? as *const u32,
            skin_window: symbols.address("SkinWindow")? as *const u32,
            skin_window_alt: symbols.address("SkinWindowAlt")? as *const u32,
            skin_alt_flag: symbols.address("SkinAltFlag")? as *const u8,
            skin_content: symbols.address("SkinContentPanel")? as *const u32,
            count_lines: sym!(symbols, "CountWrappedLines", CountLinesFn),
            wrapped_label: sym!(symbols, "DrawWrappedLabel", WrappedLabelFn),
            fill_rect: sym!(symbols, "NkFillRect", FillRectFn),
            widget_space: sym!(symbols, "NkLayoutWidgetSpace", WidgetSpaceFn),
        })
    }

    pub(crate) fn ctx(&self) -> *mut c_void {
        unsafe { *self.ctx_slot }
    }

    pub(crate) fn push_skin(&self, id: u32) -> u32 {
        unsafe { (self.set_skin)(self.skin_manager, id) }
    }

    fn alt(&self) -> bool {
        unsafe { *self.skin_alt_flag == 1 }
    }

    fn window_skin(&self) -> u32 {
        unsafe {
            if self.alt() {
                *self.skin_window_alt
            } else {
                *self.skin_window
            }
        }
    }

    /// How wide `text` is drawn: the narrowest width the game's line counter fits it on one line
    /// in. Remembered per text.
    fn text_width(&self, text: &str) -> f32 {
        thread_local! {
            static WIDTHS: RefCell<HashMap<String, f32>> = RefCell::new(HashMap::new());
        }
        if let Some(w) = WIDTHS.with(|m| m.borrow().get(text).copied()) {
            return w;
        }
        let ctx = self.ctx();
        if ctx.is_null() || text.is_empty() {
            return 0.0;
        }
        let t = cstr(text);
        let lines = |width: f32| unsafe {
            (self.count_lines)(ctx, t.as_ptr().cast(), (t.len() - 1) as i32, width)
        };
        let (mut lo, mut hi) = (0.0f32, 4096.0f32);
        if lines(hi) > 1 {
            return hi;
        }
        while hi - lo > 1.0 {
            let mid = (lo + hi) * 0.5;
            if lines(mid) <= 1 {
                hi = mid;
            } else {
                lo = mid;
            }
        }
        let width = hi.ceil();
        WIDTHS.with(|m| m.borrow_mut().insert(text.to_owned(), width));
        width
    }
}

/// NUL-terminated copy for the game's C-string widgets.
fn cstr(text: &str) -> Vec<u8> {
    let mut v: Vec<u8> = text.bytes().filter(|&b| b != 0).collect();
    v.push(0);
    v
}

/// The game's menu widgets on the current context. Every call into the game runs under its
/// own guard: the plugin screen draws while the session is borrowed, and a fault caught only
/// by the hook's outer guard would abandon that borrow for good. A faulting widget reports
/// "nothing happened" and is logged by [`crate::session::host_fault`].
pub struct GameWidgets<'a> {
    nk: &'a Nk,
    ctx: *mut c_void,
    /// Inside a plugin card (wider margins).
    in_card: Cell<bool>,
    /// Column ratios of `row_cells` rows. The game's layout reads them while the row's widgets
    /// are placed, so they live as long as the widgets.
    ratios: RefCell<Vec<Box<[f32]>>>,
}

impl<'a> GameWidgets<'a> {
    /// The widgets on `ctx`, the game's menu context of this frame.
    pub fn new(nk: &'a Nk, ctx: *mut c_void) -> GameWidgets<'a> {
        GameWidgets {
            nk,
            ctx,
            in_card: Cell::new(false),
            ratios: RefCell::new(Vec::new()),
        }
    }

    fn read_f32(&self, offset: usize) -> f32 {
        unsafe { (self.ctx as *const u8).add(offset).cast::<f32>().read_unaligned() }
    }

    fn inset(&self) -> f32 {
        if self.in_card.get() {
            CARD_INSET
        } else {
            ROW_INSET
        }
    }

    /// The current panel's width, and the space a row of `cells` cells (margins included) has
    /// after the gaps between them.
    fn usable(&self, cells: usize) -> f32 {
        let win = unsafe {
            (self.ctx as *const u8)
                .add(CTX_CURRENT_WINDOW)
                .cast::<*const u8>()
                .read_unaligned()
        };
        if win.is_null() {
            return 0.0;
        }
        let panel = unsafe { win.add(WINDOW_PANEL).cast::<*const u8>().read_unaligned() };
        if panel.is_null() {
            return 0.0;
        }
        let width = unsafe { panel.add(PANEL_BOUNDS_W).cast::<f32>().read_unaligned() };
        (width - self.read_f32(CTX_SPACING_X) * (cells.saturating_sub(1)) as f32).max(0.0)
    }

    fn text_colour(&self) -> u32 {
        unsafe {
            (self.ctx as *const u8)
                .add(CTX_TEXT_COLOUR)
                .cast::<u32>()
                .read_unaligned()
        }
    }

    fn label_aligned(&self, text: &str, align: i32) {
        let t = cstr(text);
        Self::call("DrawLabel", || unsafe {
            (self.nk.label)(self.ctx, t.as_ptr().cast(), align)
        })
    }

    fn call<R: Default>(what: &str, f: impl FnOnce() -> R) -> R {
        match guarded(f) {
            Ok(r) => r,
            Err(fault) => {
                crate::session::host_fault(what, &fault);
                R::default()
            }
        }
    }
}

impl Widgets for GameWidgets<'_> {
    fn row(&self, height: f32, cols: i32) {
        if !(1..=8).contains(&cols) {
            return Self::call("LayoutRowDynamic", || unsafe {
                (self.nk.row)(self.ctx, height, cols)
            });
        }
        // An empty first cell is the left margin; the widgets share the rest.
        let table = if self.in_card.get() {
            &CARD_RATIOS
        } else {
            &ROW_RATIOS
        };
        let ratios = table[cols as usize - 1].as_ptr();
        Self::call("LayoutRowRatios", || unsafe {
            (self.nk.row_ratios)(self.ctx, 0, height, cols + 2, ratios);
            (self.nk.spacing)(self.ctx);
        })
    }
    fn label(&self, text: &str) {
        let t = cstr(text);
        Self::call("DrawLabel", || unsafe {
            (self.nk.label)(self.ctx, t.as_ptr().cast(), ALIGN_ROW)
        })
    }
    fn button(&self, text: &str) -> bool {
        let t = cstr(text);
        Self::call("DrawMenuButton", || unsafe {
            (self.nk.button)(self.ctx, t.as_ptr().cast()) != 0
        })
    }
    fn checkbox(&self, text: &str, value: &mut bool) -> bool {
        // A space keeps the text off the box.
        let t = cstr(&format!(" {text}"));
        let mut flags: u32 = *value as u32;
        let changed = Self::call("FlagCheckbox", || unsafe {
            (self.nk.checkbox)(t.as_ptr().cast(), &mut flags, 1) != 0
        });
        if changed {
            *value = flags & 1 != 0;
        }
        changed
    }
    fn slider(&self, text: &str, value: &mut i32, min: i32, max: i32) -> bool {
        // The game slider is unsigned; shift the range so min maps to 0.
        self.label(text);
        let mut v = (*value as i64 - min as i64) as u32;
        let mut lo = 0u32;
        let mut hi = (max as i64 - min as i64) as u32;
        let mut step = 1u32;
        let changed = Self::call("SliderU32", || unsafe {
            (self.nk.slider)(&mut v, &mut lo, &mut hi, &mut step) != 0
        });
        if changed {
            *value = (v as i64 + min as i64) as i32;
        }
        changed
    }
    fn text_edit(&self, text: &str, buf: &mut [u8]) -> bool {
        // wc2r-plus `TextEdit`: a label in the first cell, the game's edit box in the second,
        // with the text-edit skin pushed and the row spacing put back around the call.
        self.label(text);
        let before = buf.to_vec();
        let ctx = self.ctx;
        let max = buf.len() as i32;
        let typing = Self::call("TextEdit", || unsafe {
            let spacing_at = (ctx as *mut u8).add(CTX_SPACING_Y).cast::<f32>();
            let spacing = spacing_at.read_unaligned();
            let skin = self.nk.push_skin(*self.nk.skin_text_edit);
            spacing_at.write_unaligned(spacing);
            let state = (self.nk.text_edit)(
                ctx,
                TEXT_EDIT_FLAGS,
                buf.as_mut_ptr().cast(),
                max,
                self.nk.text_edit_filter,
            );
            self.nk.push_skin(skin);
            spacing_at.write_unaligned(spacing);
            state
        });
        let _ = typing;
        before != *buf
    }
    fn color_edit(&self, key: &str, text: &str, hex: &mut [u8; 6]) -> bool {
        // As the options screen's color row: the caption, a swatch, and the value on a button
        // that opens the color editor.
        let shown = String::from_utf8_lossy(hex).into_owned();
        self.label(text);
        let ctx = self.ctx;
        let drawn = Self::call("NkFillRect", || {
            crate::settingsscreen::card_swatch(ctx, &shown)
        });
        if !drawn {
            self.label("");
        }
        if self.button(&format!("#{shown}")) {
            crate::settingsscreen::card_color_start(key, text, &shown);
        }
        match crate::settingsscreen::card_color_take(key) {
            Some(new) if new.len() == 6 && new.as_bytes() != hex.as_slice() => {
                hex.copy_from_slice(new.as_bytes());
                true
            }
            _ => false,
        }
    }
    fn combo(&self, key: &str, text: &str, choices: &[&str], selected: &mut usize) -> bool {
        // As the options screen's dropdown row: the caption, then the dropdown.
        self.label(text);
        let ctx = self.ctx;
        let drawn = Self::call("ComboBegin", || {
            crate::settingsscreen::card_combo(ctx, key, choices, selected)
        });
        match drawn {
            Some(changed) => changed,
            None => {
                // No dropdown on this host: show the current entry.
                let shown = choices.get(*selected).copied().unwrap_or("");
                self.label(shown);
                false
            }
        }
    }
    fn card_begin(&self, key: usize, height: f32) -> bool {
        self.row(height, 1);
        let name = cstr(&format!("w2rpm_plugin_card_{key}"));
        let previous = self.nk.push_skin(unsafe { *self.nk.skin_card });
        let opened = Self::call("BeginNamedGroup", || unsafe {
            (self.nk.begin_group)(self.ctx, name.as_ptr().cast(), GROUP_CARD) != 0
        });
        self.nk.push_skin(previous);
        if opened {
            self.in_card.set(true);
            // The frame's top border covers the first row: start below it.
            self.row(crate::ui::CARD_TOP_H, 1);
            self.label("");
        }
        opened
    }
    fn card_end(&self) {
        self.in_card.set(false);
        Self::call("EndGroup", || unsafe { (self.nk.end_group)(self.ctx) })
    }
    fn disable_begin(&self) {
        Self::call("DisableBegin", || unsafe {
            (self.nk.disable_begin)(self.ctx)
        })
    }
    fn disable_end(&self) {
        Self::call("DisableEnd", || unsafe { (self.nk.disable_end)(self.ctx) })
    }
    fn text_width(&self, text: &str) -> f32 {
        Self::call("CountWrappedLines", || self.nk.text_width(text))
    }
    fn text_lines(&self, text: &str, width: f32) -> i32 {
        let t = cstr(text);
        Self::call("CountWrappedLines", || unsafe {
            (self.nk.count_lines)(self.ctx, t.as_ptr().cast(), (t.len() - 1) as i32, width)
        })
    }
    fn line_height(&self) -> f32 {
        let font = unsafe {
            (self.ctx as *const u8)
                .add(CTX_FONT)
                .cast::<*const u8>()
                .read_unaligned()
        };
        let h = if font.is_null() {
            0.0
        } else {
            unsafe { font.add(USER_FONT_HEIGHT).cast::<f32>().read_unaligned() }
        };
        if h > 1.0 && h < 500.0 { h } else { crate::ui::ROW_H }
    }
    fn row_width(&self) -> f32 {
        self.usable(3) * (1.0 - 2.0 * self.inset())
    }
    fn row_gap(&self) -> f32 {
        // The style's row spacing, which the card's skin sets inside a card.
        let gap = self.read_f32(CTX_SPACING_Y);
        if gap.is_finite() && (0.0..=200.0).contains(&gap) {
            gap
        } else {
            crate::ui::ROW_GAP
        }
    }
    fn row_cells(&self, height: f32, widths: &[f32]) {
        if widths.is_empty() || widths.len() > 8 {
            return self.row(height, widths.len() as i32);
        }
        // An empty cell on each side is the margin; fixed cells take their width, and cells of
        // width 0 share what is left.
        let cells = widths.len() + 2;
        let usable = self.usable(cells);
        if usable <= 0.0 {
            return self.row(height, widths.len() as i32);
        }
        let inset = self.inset();
        let inner = usable * (1.0 - 2.0 * inset);
        let fixed: f32 = widths.iter().sum();
        let fillers = widths.iter().filter(|&&w| w <= 0.0).count().max(1) as f32;
        let spare = ((inner - fixed) / fillers).max(0.0);
        let mut ratios = Vec::with_capacity(cells);
        ratios.push(inset);
        for &w in widths {
            ratios.push(if w <= 0.0 { spare } else { w } / usable);
        }
        ratios.push(inset);
        let ratios = ratios.into_boxed_slice();
        let ptr = ratios.as_ptr();
        self.ratios.borrow_mut().push(ratios);
        Self::call("LayoutRowRatios", || unsafe {
            (self.nk.row_ratios)(self.ctx, 0, height, cells as i32, ptr);
            (self.nk.spacing)(self.ctx);
        })
    }
    fn label_coloured(&self, text: &str, colour: u32) {
        let at = unsafe { (self.ctx as *mut u8).add(CTX_TEXT_COLOUR).cast::<u32>() };
        let before = unsafe { at.read_unaligned() };
        unsafe { at.write_unaligned(colour) };
        self.label(text);
        unsafe { at.write_unaligned(before) };
    }
    fn label_right(&self, text: &str) {
        self.label_aligned(text, ALIGN_RIGHT);
    }
    fn label_wrapped(&self, text: &str) {
        let t = cstr(text);
        let colour = self.text_colour();
        Self::call("DrawWrappedLabel", || unsafe {
            (self.nk.wrapped_label)(self.ctx, t.as_ptr().cast(), colour)
        })
    }
}

thread_local! {
    static PAUSE_MS: Cell<u64> = const { Cell::new(0) };
    /// When the settings screen was last drawn, either entry.
    static SCREEN_MS: Cell<u64> = const { Cell::new(0) };
}

/// The settings screen is open: it was drawn within the re-entry gap.
pub fn settings_screen_open() -> bool {
    crate::chat::now_ms().saturating_sub(SCREEN_MS.with(|t| t.get())) < REENTRY_GAP_MS
}

static ORIGINAL: AtomicUsize = AtomicUsize::new(0);

pub fn install(image: &Image, symbols: &Symbols, log: &dyn Log) -> bool {
    let Some(target) = symbols.address(SYMBOL) else {
        log.line(
            W2R_LOG_ERROR,
            "host",
            &format!("{SYMBOL} is not in the symbol table"),
        );
        return false;
    };
    // SAFETY: `detour` matches BuildOptionsMenuScreen's cdecl signature from the symbol table.
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
                &format!("hooked {SYMBOL} @ {target:#010x}"),
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

unsafe extern "C" fn detour(variant: i32) {
    let original: BuildOptionsMenuScreenFn =
        unsafe { core::mem::transmute(ORIGINAL.load(Ordering::Acquire)) };
    let now = crate::chat::now_ms();
    // The game may build the main-menu variant (0) behind the pause menu's (1) in the same frame;
    // only the visible one gets the settings screen, or both would draw on top of each other.
    if variant == 1 {
        PAUSE_MS.with(|t| t.set(now));
    }
    let shadowed = variant == 0 && now.saturating_sub(PAUSE_MS.with(|t| t.get())) < REENTRY_GAP_MS;
    if (variant == 0 || variant == 1) && !shadowed {
        let mut drawn = false;
        if let Err(fault) = guarded(|| drawn = crate::session::draw_settings_screen(variant)) {
            crate::session::host_fault("settings screen", &fault);
        }
        if drawn {
            SCREEN_MS.with(|t| t.set(now));
            crate::settingsscreen::finish(variant);
            return;
        }
    }
    // Any other variant, a shadowed one, or no screen (symbols missing, no session yet): the
    // game's own, so there is always a working options screen.
    unsafe { original(variant) };
}

const MAIN_MENU_SYMBOL: &str = "MainMenuScreen";
/// The title screen's own "Build: ... Net:..." label is a 120-high row at the top-left corner of
/// the full-screen "main_menu" window, 20 units in; ours are the next rows down.
const VERSION_X: f32 = 20.0;
const VERSION_Y: f32 = 110.0;
const VERSION_W: f32 = 1400.0;
const VERSION_H: f32 = 120.0;

static MAIN_MENU_ORIGINAL: AtomicUsize = AtomicUsize::new(0);

/// Hooks the title screen so a "W2R Mod Loader <version>" line shows under the game's build label.
pub fn install_main_menu(image: &Image, symbols: &Symbols, log: &dyn Log) -> bool {
    let Some(target) = symbols.address(MAIN_MENU_SYMBOL) else {
        log.line(
            W2R_LOG_ERROR,
            "host",
            &format!("{MAIN_MENU_SYMBOL} is not in the symbol table"),
        );
        return false;
    };
    // SAFETY: `main_menu_detour` matches MainMenuScreen's cdecl `void()` signature.
    let result = unsafe {
        hooks::global().install(
            Owner::Host,
            image,
            target,
            main_menu_detour as *mut c_void,
            &format!("host:{MAIN_MENU_SYMBOL}"),
            |original| MAIN_MENU_ORIGINAL.store(original as usize, Ordering::Release),
        )
    };
    match result {
        Ok(_) => {
            log.line(
                W2R_LOG_INFO,
                "host",
                &format!("hooked {MAIN_MENU_SYMBOL} @ {target:#010x}"),
            );
            true
        }
        Err(e) => {
            log.line(
                W2R_LOG_ERROR,
                "host",
                &format!("could not hook {MAIN_MENU_SYMBOL}: {e:?}"),
            );
            false
        }
    }
}

/// The plugin manager popup is open.
static MANAGER_OPEN: AtomicBool = AtomicBool::new(false);
thread_local! {
    static MAIN_MENU_MS: Cell<u64> = const { Cell::new(0) };
}

const MANAGE_W: f32 = 300.0;
const MANAGE_GAP: f32 = 40.0;
const MANAGER_W: f32 = 2500.0;
const MANAGER_H: f32 = 1900.0;
const MANAGER_PAD_X: f32 = 100.0;
const MANAGER_PAD_Y: f32 = 90.0;
/// Room between the list's panel art and the list inside it.
const LIST_PAD_X: f32 = 50.0;
const LIST_PAD_TOP: f32 = 60.0;
const LIST_PAD_BOTTOM: f32 = 50.0;
const MANAGER_GAP: f32 = 40.0;
const CLOSE_W: f32 = 380.0;
const CLOSE_H: f32 = 120.0;
/// Dims the main menu behind the popup.
const MANAGER_SHADE: u32 = 0x6000_0000;
/// The popup's own background, as the in-match settings screen draws its backdrop: a gold border
/// around a dark, nearly opaque panel.
const MANAGER_FILL: u32 = 0xf60e_141a;
const MANAGER_BORDER: u32 = 0xff30_6078;
const MANAGER_BORDER_W: f32 = 5.0;
/// Lightens the Manage button under the mouse.
const MANAGE_HOVER: u32 = 0x28ff_ffff;
const UI_HEIGHT: f32 = 2160.0;

unsafe extern "C" fn main_menu_detour() {
    let original: unsafe extern "C" fn() =
        unsafe { core::mem::transmute(MAIN_MENU_ORIGINAL.load(Ordering::Acquire)) };
    // A new visit to the main menu starts with the popup closed.
    let now = crate::chat::now_ms();
    if now.saturating_sub(MAIN_MENU_MS.with(|t| t.replace(now))) > REENTRY_GAP_MS {
        MANAGER_OPEN.store(false, Ordering::Release);
    }
    // While the plugin manager is open the game's menu is not built, so nothing behind the popup
    // can be clicked; it is built afresh once the popup closes.
    if !MANAGER_OPEN.load(Ordering::Acquire) {
        unsafe { original() };
    }
    if let Err(fault) = guarded(|| crate::session::with_nk(draw_version)) {
        crate::session::host_fault("title screen version", &fault);
    }
    if MANAGER_OPEN.load(Ordering::Acquire) {
        if let Err(fault) = guarded(|| crate::session::with_nk(draw_manager)) {
            crate::session::host_fault("plugin manager", &fault);
            MANAGER_OPEN.store(false, Ordering::Release);
        }
    }
}

/// The version lines in the title screen's top-left corner, with a Manage button after the
/// plugin count.
fn draw_version(nk: &Nk) {
    let ctx = nk.ctx();
    if ctx.is_null() {
        return;
    }
    let width = unsafe { (nk.ui_width)() };
    let name = c"w2rpm_version";
    let version = match option_env!("W2R_GIT_HASH") {
        Some(hash) if !hash.is_empty() => {
            format!("W2R Mod Loader {} ({hash})", env!("CARGO_PKG_VERSION"))
        }
        _ => format!("W2R Mod Loader {}", env!("CARGO_PKG_VERSION")),
    };
    let summary = crate::session::plugin_summary();
    let lines = [Some(version), summary.clone()];
    let open = MANAGER_OPEN.load(Ordering::Acquire);
    unsafe {
        // Kept behind the game's own windows and taking no input, so it never takes focus from
        // the menu (a background window that is clicked turns read-only). The Manage button
        // reads the mouse itself.
        if (nk.begin_window)(
            ctx,
            name.as_ptr(),
            0.0,
            VERSION_Y,
            VERSION_W.min(width),
            VERSION_H * lines.len() as f32,
            WINDOW_NO_SCROLLBAR | WINDOW_NO_INPUT | WINDOW_BACKGROUND,
        ) != 0
        {
            (nk.space_begin)(ctx, 1, 0.0, 1000);
            for (i, line) in lines.iter().flatten().enumerate() {
                let text = cstr(line);
                (nk.rect)(ctx, VERSION_X, VERSION_H * i as f32, VERSION_W, VERSION_H);
                (nk.label)(ctx, text.as_ptr().cast(), ALIGN_ROW);
            }
            if let (Some(summary), false) = (&summary, open) {
                let x = VERSION_X + nk.text_width(summary) + MANAGE_GAP;
                (nk.rect)(ctx, x, VERSION_H + 10.0, MANAGE_W, VERSION_H - 20.0);
                // Where the button lands on screen.
                let mut b = [0f32; 4];
                (nk.widget_space)(b.as_mut_ptr(), ctx, core::ptr::null_mut(), 0);
                let label = cstr("Manage");
                (nk.button)(ctx, label.as_ptr().cast());
                let read = |offset: usize| (ctx as *const u8).add(offset);
                let mx = read(CTX_MOUSE_X).cast::<f32>().read_unaligned();
                let my = read(CTX_MOUSE_Y).cast::<f32>().read_unaligned();
                let hovered = mx >= b[0] && mx < b[0] + b[2] && my >= b[1] && my < b[1] + b[3];
                if hovered {
                    let win = read(CTX_CURRENT_WINDOW).cast::<*mut u8>().read_unaligned();
                    if !win.is_null() {
                        let canvas = win.add(WINDOW_BUFFER).cast::<c_void>();
                        (nk.fill_rect)(canvas, b[0], b[1], b[2], b[3], 0.0, MANAGE_HOVER);
                    }
                    let down = read(CTX_MOUSE_LEFT_DOWN).cast::<i32>().read_unaligned() != 0;
                    let clicked = read(CTX_MOUSE_LEFT_CLICKED).cast::<u32>().read_unaligned() != 0;
                    if down && clicked {
                        crate::settingsscreen::card_color_cancel();
                        MANAGER_OPEN.store(true, Ordering::Release);
                    }
                }
            }
            (nk.space_end)(ctx);
        }
        (nk.end_window)(ctx);
    }
}

/// The plugin manager: a window of its own over the whole title screen, dimming it, with the
/// plugin list in a popup frame and a Close button.
fn draw_manager(nk: &Nk) {
    let ctx = nk.ctx();
    if ctx.is_null() {
        return;
    }
    // Ctrl+C / X / V / A in the cards' text boxes.
    crate::menukeys::feed(ctx);
    let ui_w = unsafe { (nk.ui_width)() }.floor();
    let frame = [
        ((ui_w - MANAGER_W) * 0.5).floor(),
        ((UI_HEIGHT - MANAGER_H) * 0.5).floor(),
        MANAGER_W,
        MANAGER_H,
    ];
    let x = frame[0] + MANAGER_PAD_X;
    let y = frame[1] + MANAGER_PAD_Y;
    let w = MANAGER_W - MANAGER_PAD_X * 2.0;
    let h = MANAGER_H - MANAGER_PAD_Y * 2.0;
    let list_top = y + crate::ui::ROW_H + MANAGER_GAP;
    let close_y = y + h - CLOSE_H;
    let list_h = close_y - MANAGER_GAP - list_top;

    // While a card's color editor is open, the manager stays behind it and takes no input.
    let editing = crate::settingsscreen::card_color_open();
    let flags = if editing {
        WINDOW_NO_SCROLLBAR | WINDOW_BACKGROUND
    } else {
        WINDOW_NO_SCROLLBAR
    };
    let prev_skin = nk.push_skin(nk.window_skin());
    unsafe {
        let opened = (nk.begin_window)(
            ctx,
            c"w2r_plugin_manager".as_ptr(),
            0.0,
            0.0,
            ui_w,
            UI_HEIGHT,
            flags,
        ) != 0;
        if opened {
            crate::settingsscreen::set_window_read_only(ctx, editing);
            (nk.space_begin)(ctx, 1, 0.0, 1000);
            let win = (ctx as *const u8)
                .add(CTX_CURRENT_WINDOW)
                .cast::<*mut u8>()
                .read_unaligned();
            if !win.is_null() {
                let canvas = win.add(WINDOW_BUFFER).cast::<c_void>();
                (nk.fill_rect)(canvas, 0.0, 0.0, ui_w, UI_HEIGHT, 0.0, MANAGER_SHADE);
                let [fx, fy, fw, fh] = frame;
                let b = MANAGER_BORDER_W;
                (nk.fill_rect)(canvas, fx, fy, fw, fh, 0.0, MANAGER_BORDER);
                (nk.fill_rect)(canvas, fx + b, fy + b, fw - b * 2.0, fh - b * 2.0, 0.0, MANAGER_FILL);
            }

            (nk.rect)(ctx, x, y, w, crate::ui::ROW_H);
            let title = cstr("Plugins");
            (nk.label)(ctx, title.as_ptr().cast(), ALIGN_TITLE);

            // The list scrolls in a group inset inside a content panel, so it does not touch the
            // panel's frame.
            (nk.rect)(ctx, x, list_top, w, list_h);
            let prev = nk.push_skin(*nk.skin_content);
            let framed =
                (nk.begin_group)(ctx, c"w2r_plugin_manager_frame".as_ptr(), WINDOW_NO_SCROLLBAR)
                    != 0;
            nk.push_skin(prev);
            if framed {
                (nk.space_begin)(ctx, 1, 0.0, 1000);
                (nk.rect)(
                    ctx,
                    LIST_PAD_X,
                    LIST_PAD_TOP,
                    w - LIST_PAD_X * 2.0,
                    list_h - LIST_PAD_TOP - LIST_PAD_BOTTOM,
                );
                if (nk.begin_group)(ctx, c"w2r_plugin_manager_list".as_ptr(), 0) != 0 {
                    let gap_at = (ctx as *mut u8).add(CTX_SPACING_Y).cast::<f32>();
                    let gap_before = gap_at.read_unaligned();
                    gap_at.write_unaligned(crate::ui::ROW_GAP);
                    let widgets = GameWidgets::new(nk, ctx);
                    crate::session::draw_plugin_list(&widgets);
                    gap_at.write_unaligned(gap_before);
                    (nk.end_group)(ctx);
                }
                (nk.space_end)(ctx);
                (nk.end_group)(ctx);
            }

            (nk.rect)(ctx, x + (w - CLOSE_W) * 0.5, close_y, CLOSE_W, CLOSE_H);
            let close = cstr("Close");
            if (nk.button)(ctx, close.as_ptr().cast()) != 0 {
                MANAGER_OPEN.store(false, Ordering::Release);
            }
            (nk.space_end)(ctx);
        }
        (nk.end_window)(ctx);
    }
    nk.push_skin(prev_skin);
    // Drawn after the manager, so it is on top. Also when a card opened it during this frame.
    if crate::settingsscreen::card_color_open() {
        crate::settingsscreen::draw_card_color_popup();
    }
}
