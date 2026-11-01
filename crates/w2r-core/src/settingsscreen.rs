//! The options screen (docs/settings-screen.md): every setting the game, the host and the
//! plugins have, in one screen. Categories down the left, the selected category's settings on the
//! right under subcategory headings, a search page, hover descriptions, and Cancel / Apply /
//! Save and Exit along the bottom.
//!
//! It replaces the game's options screen on the main menu (variant 0) and in the pause menu
//! (variant 1). The model is [`crate::settings`]; this file only draws it. Layout numbers are in
//! the game's 3840x2160 UI space. Ported from wc2r-plus `core/settings/settings_screen.cpp`
//! (MIT). x86 only.

use core::ffi::{CStr, c_char, c_void};
use std::cell::RefCell;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use w2r_abi::{W2R_LOG_ERROR, W2R_LOG_INFO, W2R_LOG_WARN, W2R_MAX_SETTING_STR};

use crate::color::{self, Hsv, Rgb};
use crate::hooks::{self, Owner};
use crate::host::Log;
use crate::pe::Image;
use crate::settings::{Editor, Kind, Native, Owners, SettingDef, Value};
use crate::symbols::Symbols;

// ---- Nuklear context, window and panel offsets (wc2r-plus target/struct_offsets.h) ----------
const CTX_MOUSE_X: usize = 0x1bc;
const CTX_MOUSE_Y: usize = 0x1c0;
const CTX_FONT: usize = 0x1e0;
const CTX_TEXT_COLOUR: usize = 0x20c;
/// style.window.spacing.y. Nuklear sizes a widget as `row.height - spacing.y` when it places it,
/// so the value must be the same when a row is declared and when its widget is drawn.
const CTX_SPACING_Y: usize = 0x2004;
/// style.window.scrollbar_size, .x here and .y at +4.
const CTX_SCROLLBAR_SIZE: usize = 0x2008;
const CTX_GROUP_PADDING_Y: usize = 0x201c;
const CTX_CURRENT_WINDOW: usize = 0x3d28;
const WINDOW_BUFFER: usize = 0x64;
const WINDOW_PANEL: usize = 0x8c;
const PANEL_BOUNDS_X: usize = 0x08;
const PANEL_BOUNDS_W: usize = 0x10;
/// A pointer to the group's scroll offset, not the offset itself.
const PANEL_OFFSET_Y: usize = 0x1c;
const PANEL_AT_Y: usize = 0x24;
const PANEL_CLIP_X: usize = 0x3c;
const PANEL_CLIP_Y: usize = 0x40;
const PANEL_CLIP_W: usize = 0x44;
const PANEL_CLIP_H: usize = 0x48;
const PANEL_ROW_HEIGHT: usize = 0x6c;
const USER_FONT_HEIGHT: usize = 0x04;
/// The left mouse button: non-zero while held, and whether it went down this frame.
const CTX_MOUSE_LEFT_DOWN: usize = 0x17c;
const CTX_MOUSE_LEFT_CLICKED: usize = 0x180;
const SCROLLBAR_SHOW_BUTTONS: usize = 0xe8;

/// NK_EDIT_FIELD, the flags the game passes for its own search box.
const TEXT_EDIT_FLAGS: u32 = 0x260;
/// Nuklear text alignment: centered for titles, left and vertically centered for rows.
const ALIGN_TITLE: i32 = 0x12;
const ALIGN_ROW: i32 = 0x11;
const GROUP_NO_SCROLLBAR: u32 = 0x20;
/// Nuklear window flags: kept behind other windows, and read-only (drawn, but takes no input).
const WINDOW_BACKGROUND: u32 = 0x100;
const WINDOW_ROM: u32 = 0x1000;
const WINDOW_FLAGS: usize = 0x48;
const PANEL_FLAGS: usize = 0x04;

const GAME_STATE_MAIN_MENU: i32 = 0x04;
/// Values of the pause menu's sub-screen selector. (0 means no pause menu and is never written.)
const PAUSE_SCREEN_ROOT: u32 = 1;
const PAUSE_SCREEN_OPTIONS: u32 = 7;

// ---- layout, in UI units ---------------------------------------------------------------------
const UI_HEIGHT: f32 = 2160.0;
const PANEL_TOP: f32 = 320.0;
const PANEL_HEIGHT: f32 = 1580.0;
const FOOTER_HEIGHT: f32 = 150.0;
const GAP: f32 = 20.0;
const PANEL_WIDTH_PERCENT: f32 = 80.0;
const CATEGORY_PANEL_PERCENT: f32 = 30.0;
const PANEL_PAD: f32 = 34.0;
const PANEL_PAD_TOP: f32 = 28.0;
const ROW_HEIGHT: f32 = 90.0;
const CATEGORY_ROW_HEIGHT: f32 = 120.0;
/// The gap between rows. The context's own spacing is 0, so this is set around our rows only.
const ROW_SPACING: f32 = 16.0;
/// A text box fills its whole row, so it gets a wider gap on both sides.
const TEXT_ROW_SPACING: f32 = 40.0;
/// Height of a text box row, the same as the game's own search box.
const SEARCH_ROW_HEIGHT: f32 = 120.0;
const TITLE_TOP: f32 = 150.0;
const TITLE_HEIGHT: f32 = 120.0;

const COMBO_ITEM_HEIGHT: f32 = 110.0;
/// Shortest row a long dropdown's entries shrink to (still well above the font's line height).
const COMBO_ITEM_MIN_HEIGHT: f32 = 76.0;
const COMBO_POPUP_PAD: f32 = 40.0;
/// Tallest dropdown popup. The game's combo popup does not scroll, so a list longer than this
/// gets shorter rows instead of losing its last entries off the bottom.
const COMBO_POPUP_MAX_HEIGHT: f32 = 1300.0;

/// Row height and popup height for a dropdown of `n` entries: full-height rows while they fit
/// under the cap, then rows shrunk (down to `COMBO_ITEM_MIN_HEIGHT`) so every entry shows. Up to
/// `W2R_MAX_SETTING_CHOICE_COUNT` (16) entries fit without reaching the minimum.
fn combo_layout(n: usize) -> (f32, f32) {
    let n = n.max(1) as f32;
    let fit = (COMBO_POPUP_MAX_HEIGHT - COMBO_POPUP_PAD) / n;
    // Whole units: a fractional row height puts the seams between rows at uneven positions, so
    // some gaps between entries look wider than others.
    let item = fit.floor().clamp(COMBO_ITEM_MIN_HEIGHT, COMBO_ITEM_HEIGHT);
    (item, n * item + COMBO_POPUP_PAD)
}

const TIP_MAX_WIDTH: f32 = 1100.0;
const TIP_PAD: f32 = 26.0;
const TIP_BORDER: f32 = 4.0;
const TIP_ROUNDING: f32 = 6.0;
const TIP_CURSOR_OFFSET: f32 = 44.0;
const TIP_MAX_LINES: i32 = 8;
const TIP_FALLBACK_LINE_HEIGHT: f32 = 44.0;

const fn rgba(r: u32, g: u32, b: u32, a: u32) -> u32 {
    (a << 24) | (b << 16) | (g << 8) | r
}
const TIP_FILL: u32 = rgba(12, 10, 8, 244);
const TIP_BORDER_COLOUR: u32 = rgba(120, 96, 48, 255);
const TIP_TEXT_FALLBACK: u32 = rgba(255, 255, 255, 255);
/// The tooltip's last line, naming what the setting belongs to.
const TIP_SOURCE_COLOUR: u32 = rgba(165, 160, 150, 255);
const TIP_SOURCE_GAP: f32 = 16.0;
/// The pause-menu variant's backdrop. The game's in-match window art has its own panel layout
/// drawn into it, which does not fit this screen, so a plain panel is drawn instead.
const BACKDROP: u32 = rgba(26, 20, 14, 246);
const BACKDROP_BORDER: u32 = rgba(120, 96, 48, 255);
const BACKDROP_PAD: f32 = 28.0;
const BACKDROP_BORDER_W: f32 = 5.0;

// The color editor.
/// Room between the popup frame's edge and its contents. The frame art is ragged and does not
/// fill its rectangle, so this is generous.
const POPUP_PAD_X: f32 = 160.0;
const POPUP_PAD_Y: f32 = 150.0;
const PICKER_SIDE: f32 = 760.0;
const HUE_WIDTH: f32 = 90.0;
const HUE_GAP: f32 = 40.0;
const COLUMN_GAP: f32 = 80.0;
const SWATCH: f32 = 220.0;
const SWATCH_GAP: f32 = 40.0;
const SECTION_GAP: f32 = 50.0;
const BUTTON_WIDTH: f32 = 380.0;
const BUTTON_HEIGHT: f32 = 120.0;
const BUTTON_GAP: f32 = 60.0;
/// Between a color row's swatch and its button.
const SWATCH_ROW_GAP: f32 = 24.0;
/// Dims the screen behind the color popup.
const POPUP_SHADE: u32 = rgba(0, 0, 0, 150);
const MARKER: f32 = 14.0;
const MARKER_LINE: f32 = 4.0;
const BLACK: u32 = rgba(0, 0, 0, 255);
const WHITE: u32 = rgba(255, 255, 255, 255);
/// Texture sizes; drawn larger with linear filtering, which keeps the gradients smooth.
const SQUARE_TEXELS: i32 = 64;
const HUE_TEXELS: i32 = 128;
const HUE_TEXEL_WIDTH: i32 = 4;
const GL_RGBA: u32 = 0x1908;
const GL_UNSIGNED_BYTE: u32 = 0x1401;
const FORMAT_RGBA8: i32 = 0;
const FILTER_LINEAR: i32 = 1;

/// A text box's buffer: the longest string setting, and the terminator.
const EDIT_BUFFER: usize = W2R_MAX_SETTING_STR as usize + 1;
const SEARCH_BUFFER: usize = 128;
/// The page listed after the categories.
const SEARCH_CATEGORY: &str = "Search";
/// After this long without the screen being built, the next build is a fresh entry.
const REENTRY_GAP_MS: u64 = 250;

type BeginWindowFn =
    unsafe extern "C" fn(*mut c_void, *const c_char, f32, f32, f32, f32, u32) -> i32;
type CtxFn = unsafe extern "C" fn(*mut c_void);
type UiWidthFn = unsafe extern "C" fn() -> f32;
type RectFn = unsafe extern "C" fn(*mut c_void, f32, f32, f32, f32);
type GroupFn = unsafe extern "C" fn(*mut c_void, *const c_char, u32) -> i32;
type SpaceBeginFn = unsafe extern "C" fn(*mut c_void, i32, f32, i32);
type RowFn = unsafe extern "C" fn(*mut c_void, f32, i32);
type LabelFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32);
type ButtonFn = unsafe extern "C" fn(*mut c_void, *const c_char) -> i8;
type VoidFn = unsafe extern "C" fn();
type SliderFn = unsafe extern "C" fn(*mut u32, *mut u32, *mut u32, *mut u32) -> bool;
type CheckboxFn = unsafe extern "C" fn(*const c_char, *mut u32, u32) -> bool;
type TextEditFn = unsafe extern "C" fn(*mut c_void, u32, *mut c_char, i32, *mut c_void) -> u32;
type ComboBeginFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32, f32, f32) -> i32;
type ComboItemFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32) -> i32;
type DisplayCountFn = unsafe extern "C" fn() -> i32;
type FillRectFn = unsafe extern "C" fn(*mut c_void, f32, f32, f32, f32, f32, u32);
type CountLinesFn = unsafe extern "C" fn(*mut c_void, *const c_char, i32, f32) -> i32;
type WrappedLabelFn = unsafe extern "C" fn(*mut c_void, *const c_char, u32);
type SetSkinFn = unsafe extern "thiscall" fn(*mut c_void, u32) -> u32;
type LocalizedFn = unsafe extern "C" fn(*const c_char) -> *const c_char;
type TransitionFn = unsafe extern "C" fn(i32);
type WidgetSpaceFn = unsafe extern "C" fn(*mut f32, *mut c_void, *mut c_void, i32);
type CreateTextureFn =
    unsafe extern "C" fn(*mut *mut GameTexture, i32, i32, *const c_void, i32, i32) -> *mut c_void;
type DrawImageFn = unsafe extern "C" fn(*mut c_void, f32, f32, f32, f32, *const NkImage, u32);
type SubImageFn = unsafe extern "system" fn(u32, i32, i32, i32, i32, i32, u32, u32, *const c_void);

/// The game's texture object, as `CreateGameTexture` fills it in.
#[repr(C)]
struct GameTexture {
    w: i32,
    h: i32,
    gl_name: u32,
    format: i32,
    filter: i32,
}

/// Nuklear's image: the texture and the part of it to draw.
#[repr(C)]
struct NkImage {
    handle: *mut c_void,
    w: u16,
    h: u16,
    region: [u16; 4],
}

type ScrollbarFn = unsafe extern "C" fn(
    *mut c_void,
    *mut c_void,
    f32,
    f32,
    f32,
    f32,
    i32,
    f32,
    f32,
    f32,
    f32,
    *mut c_void,
    *mut c_void,
    *mut c_void,
) -> f32;

/// The game functions and globals the screen draws with, resolved once.
#[derive(Clone, Copy)]
pub struct Fns {
    ctx_slot: *const *mut c_void,
    begin_window: BeginWindowFn,
    end_window: CtxFn,
    ui_width: UiWidthFn,
    rect: RectFn,
    begin_group: GroupFn,
    end_group: CtxFn,
    space_begin: SpaceBeginFn,
    space_end: CtxFn,
    row: RowFn,
    label: LabelFn,
    button: ButtonFn,
    banner: VoidFn,
    slider: SliderFn,
    checkbox: CheckboxFn,
    text_edit: TextEditFn,
    text_edit_filter: *mut c_void,
    combo_begin: ComboBeginFn,
    combo_item: ComboItemFn,
    combo_close: CtxFn,
    combo_end: CtxFn,
    display_count: DisplayCountFn,
    skin_combo_closed: *const u32,
    skin_combo_closed_alt: *const u32,
    skin_combo_open: *const u32,
    skin_combo_open_alt: *const u32,
    fill_rect: FillRectFn,
    count_lines: CountLinesFn,
    wrapped_label: WrappedLabelFn,
    disable_begin: CtxFn,
    disable_end: CtxFn,
    set_skin: SetSkinFn,
    skin_manager: *mut c_void,
    skin_window: *const u32,
    skin_window_alt: *const u32,
    skin_alt_flag: *const u8,
    skin_content: *const u32,
    skin_text_edit: *const u32,
    skin_popup: *const u32,
    skin_popup_alt: *const u32,
    localized: LocalizedFn,
    transition: TransitionFn,
    pause_screen: *mut u32,
    widget_space: WidgetSpaceFn,
    skip_cell: CtxFn,
    create_texture: CreateTextureFn,
    draw_image: DrawImageFn,
    /// The game's slot for glTextureSubImage2D; zero until its renderer is up.
    sub_image_slot: *const usize,
}

// SAFETY: game function and data addresses, fixed for the session; used on the main thread only.
unsafe impl Send for Fns {}
unsafe impl Sync for Fns {}

macro_rules! sym {
    ($symbols:expr, $name:literal, $t:ty) => {
        unsafe { core::mem::transmute::<usize, $t>($symbols.address($name)?) }
    };
}

impl Fns {
    pub fn new(symbols: &Symbols) -> Option<Fns> {
        Some(Fns {
            ctx_slot: symbols.address("NkContextMenus")? as *const *mut c_void,
            begin_window: sym!(symbols, "BeginNamedWindow", BeginWindowFn),
            end_window: sym!(symbols, "EndCurrentWindow", CtxFn),
            ui_width: sym!(symbols, "GetUiSpaceWidth", UiWidthFn),
            rect: sym!(symbols, "OverrideNextRect", RectFn),
            begin_group: sym!(symbols, "BeginNamedGroup", GroupFn),
            end_group: sym!(symbols, "EndGroup", CtxFn),
            space_begin: sym!(symbols, "LayoutSpaceBegin", SpaceBeginFn),
            space_end: sym!(symbols, "LayoutSpaceEnd", CtxFn),
            row: sym!(symbols, "LayoutRowDynamic", RowFn),
            label: sym!(symbols, "DrawLabel", LabelFn),
            button: sym!(symbols, "DrawMenuButton", ButtonFn),
            banner: sym!(symbols, "DrawBanner", VoidFn),
            slider: sym!(symbols, "SliderU32", SliderFn),
            checkbox: sym!(symbols, "FlagCheckbox", CheckboxFn),
            text_edit: sym!(symbols, "TextEdit", TextEditFn),
            text_edit_filter: symbols.address("TextEditFilter")? as *mut c_void,
            combo_begin: sym!(symbols, "ComboBegin", ComboBeginFn),
            combo_item: sym!(symbols, "ComboItem", ComboItemFn),
            combo_close: sym!(symbols, "ComboClose", CtxFn),
            combo_end: sym!(symbols, "ComboEnd", CtxFn),
            display_count: sym!(symbols, "DisplayCount", DisplayCountFn),
            skin_combo_closed: symbols.address("SkinComboClosed")? as *const u32,
            skin_combo_closed_alt: symbols.address("SkinComboClosedAlt")? as *const u32,
            skin_combo_open: symbols.address("SkinComboOpen")? as *const u32,
            skin_combo_open_alt: symbols.address("SkinComboOpenAlt")? as *const u32,
            fill_rect: sym!(symbols, "NkFillRect", FillRectFn),
            count_lines: sym!(symbols, "CountWrappedLines", CountLinesFn),
            wrapped_label: sym!(symbols, "DrawWrappedLabel", WrappedLabelFn),
            disable_begin: sym!(symbols, "DisableBegin", CtxFn),
            disable_end: sym!(symbols, "DisableEnd", CtxFn),
            set_skin: sym!(symbols, "SetSkin", SetSkinFn),
            skin_manager: symbols.address("SkinManager")? as *mut c_void,
            skin_window: symbols.address("SkinWindow")? as *const u32,
            skin_window_alt: symbols.address("SkinWindowAlt")? as *const u32,
            skin_alt_flag: symbols.address("SkinAltFlag")? as *const u8,
            skin_content: symbols.address("SkinContentPanel")? as *const u32,
            skin_text_edit: symbols.address("SkinTextEdit")? as *const u32,
            skin_popup: symbols.address("SkinPopupFrame")? as *const u32,
            skin_popup_alt: symbols.address("SkinPopupFrameAlt")? as *const u32,
            localized: sym!(symbols, "LookupLocalized", LocalizedFn),
            transition: sym!(symbols, "TransitionGameState", TransitionFn),
            pause_screen: symbols.address("PauseMenuScreen")? as *mut u32,
            widget_space: sym!(symbols, "NkLayoutWidgetSpace", WidgetSpaceFn),
            skip_cell: sym!(symbols, "Spacing", CtxFn),
            create_texture: sym!(symbols, "CreateGameTexture", CreateTextureFn),
            draw_image: sym!(symbols, "NkDrawImage", DrawImageFn),
            sub_image_slot: symbols.address("GlTextureSubImage2DSlot")? as *const usize,
        })
    }

    fn ctx(&self) -> *mut c_void {
        unsafe { *self.ctx_slot }
    }

    fn alt(&self) -> bool {
        unsafe { *self.skin_alt_flag == 1 }
    }

    fn push_skin(&self, id: u32) -> u32 {
        unsafe { (self.set_skin)(self.skin_manager, id) }
    }

    /// The main menu's options window skin, used for both variants: it draws no window art, and
    /// text boxes render correctly under it.
    fn window_skin(&self) -> u32 {
        unsafe {
            if self.alt() {
                *self.skin_window_alt
            } else {
                *self.skin_window
            }
        }
    }

    fn combo_skin(&self, open: bool) -> u32 {
        unsafe {
            match (open, self.alt()) {
                (true, true) => *self.skin_combo_open_alt,
                (true, false) => *self.skin_combo_open,
                (false, true) => *self.skin_combo_closed_alt,
                (false, false) => *self.skin_combo_closed,
            }
        }
    }

    /// Translates a localization key; any other text comes back unchanged.
    fn localize(&self, text: &str) -> String {
        if text.is_empty() {
            return String::new();
        }
        let key = cstr(text);
        let shown = unsafe { (self.localized)(key.as_ptr().cast()) };
        if shown.is_null() {
            return text.to_owned();
        }
        unsafe { CStr::from_ptr(shown) }
            .to_string_lossy()
            .into_owned()
    }
}

/// NUL-terminated copy for the game's C-string widgets.
fn cstr(text: &str) -> Vec<u8> {
    let mut v: Vec<u8> = text.bytes().filter(|&b| b != 0).collect();
    v.push(0);
    v
}

fn read_f32(base: *mut c_void, offset: usize) -> f32 {
    unsafe { base.cast::<u8>().add(offset).cast::<f32>().read_unaligned() }
}

fn write_f32(base: *mut c_void, offset: usize, value: f32) {
    unsafe {
        base.cast::<u8>()
            .add(offset)
            .cast::<f32>()
            .write_unaligned(value)
    }
}

fn set_row_spacing(ctx: *mut c_void, spacing: f32) {
    write_f32(ctx, CTX_SPACING_Y, spacing);
}

fn current_window(ctx: *mut c_void) -> *mut c_void {
    unsafe {
        ctx.cast::<u8>()
            .add(CTX_CURRENT_WINDOW)
            .cast::<*mut c_void>()
            .read_unaligned()
    }
}

/// The current window's panel, or null outside a window.
fn current_panel(ctx: *mut c_void) -> *mut c_void {
    let win = current_window(ctx);
    if win.is_null() {
        return core::ptr::null_mut();
    }
    unsafe {
        win.cast::<u8>()
            .add(WINDOW_PANEL)
            .cast::<*mut c_void>()
            .read_unaligned()
    }
}

fn panel_f32(ctx: *mut c_void, offset: usize) -> f32 {
    let panel = current_panel(ctx);
    if panel.is_null() {
        -1.0
    } else {
        read_f32(panel, offset)
    }
}

/// The top of the next row Nuklear will place, in layout space.
fn next_row_top(ctx: *mut c_void) -> f32 {
    panel_f32(ctx, PANEL_AT_Y) + panel_f32(ctx, PANEL_ROW_HEIGHT)
}

/// What the screen remembers between frames.
struct Screen {
    category: String,
    categories: Vec<String>,
    last_frame_ms: u64,
    search: [u8; SEARCH_BUFFER],
    /// Text box buffers by setting index, cleared on each entry.
    edits: Vec<TextBox>,
    /// Whether each dropdown was open last frame: its header is drawn with a different skin.
    combo_open: Vec<u8>,
    /// Dropdowns open this frame. No tooltip is drawn while one is open.
    combos_this_frame: u32,
    tip: Option<Tip>,
    /// The color being picked, while the color editor is open.
    color: Option<ColorEdit>,
    picker: Picker,
}

/// A string setting's text box.
struct TextBox {
    index: usize,
    buf: Vec<u8>,
    /// The box had focus last frame.
    focused: bool,
}

/// The color editor's state.
struct ColorEdit {
    index: usize,
    hsv: Hsv,
    original: Rgb,
    hex: [u8; 8],
    drag: Drag,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    None,
    Square,
    Hue,
}

/// The editor's two gradient textures, made on first use and kept for the session.
struct Picker {
    square: usize,
    hue: usize,
    /// The hue the square was last drawn for.
    square_hue: f32,
    failed: bool,
}

/// The hovered setting's tooltip. Captured while its row is drawn inside the scrolling group, and
/// drawn after every group has ended so the group does not clip it.
struct Tip {
    description: String,
    /// What the setting belongs to, on its own line at the bottom.
    source: String,
    mouse_x: f32,
    mouse_y: f32,
}

thread_local! {
    static SCREEN: RefCell<Screen> = const {
        RefCell::new(Screen {
            category: String::new(),
            categories: Vec::new(),
            last_frame_ms: 0,
            search: [0; SEARCH_BUFFER],
            edits: Vec::new(),
            combo_open: Vec::new(),
            combos_this_frame: 0,
            tip: None,
            color: None,
            picker: Picker {
                square: 0,
                hue: 0,
                square_hue: -1.0,
                failed: false,
            },
        })
    };
}

/// What the screen needs from the session.
pub trait ScreenHost {
    fn parts(&mut self) -> (&mut Editor, &mut dyn Native, &mut dyn Owners);
}

static FNS: OnceLock<Fns> = OnceLock::new();
static SCROLLBAR_ORIGINAL: AtomicUsize = AtomicUsize::new(0);
static HIDE_ARROWS: AtomicBool = AtomicBool::new(true);
/// Set when the footer closes the screen; [`finish`] acts on it.
static CLOSE_PENDING: AtomicBool = AtomicBool::new(false);

/// Fix Overscroll: hides the arrow buttons on every vertical scrollbar. Takes effect at once.
pub fn set_hide_arrows(on: bool) {
    HIDE_ARROWS.store(on, Ordering::Release);
}

/// Resolves the screen's game functions and hooks `nk_do_scrollbarv` for Fix Overscroll. The
/// options screen hook is in `options.rs`.
pub fn install(image: &Image, symbols: &Symbols, log: &dyn Log) -> bool {
    let Some(fns) = Fns::new(symbols) else {
        log.line(
            W2R_LOG_ERROR,
            "settings",
            "screen symbols missing; stock options screen",
        );
        return false;
    };
    let _ = FNS.set(fns);
    let Some(target) = symbols.address("DoScrollbarV") else {
        return true;
    };
    // SAFETY: `scrollbar_detour` matches nk_do_scrollbarv's cdecl signature (symbol table).
    let result = unsafe {
        hooks::global().install(
            Owner::Host,
            image,
            target,
            scrollbar_detour as *mut c_void,
            "host:DoScrollbarV",
            |original| SCROLLBAR_ORIGINAL.store(original as usize, Ordering::Release),
        )
    };
    if let Err(e) = result {
        log.line(
            W2R_LOG_WARN,
            "settings",
            &format!("could not hook DoScrollbarV (Fix Overscroll off): {e:?}"),
        );
    }
    true
}

/// Whether [`install`] resolved the screen.
pub fn available() -> bool {
    FNS.get().is_some()
}

/// Applies Fix Overscroll. It is set here, where the scrollbar reads it, because the game
/// restores the style (and `show_buttons` with it) between beginning a group and drawing its
/// scrollbar.
unsafe extern "C" fn scrollbar_detour(
    state: *mut c_void,
    out: *mut c_void,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    has_scrolling: i32,
    offset: f32,
    target: f32,
    step: f32,
    button_pixel_inc: f32,
    style: *mut c_void,
    input: *mut c_void,
    font: *mut c_void,
) -> f32 {
    let original: ScrollbarFn =
        unsafe { core::mem::transmute(SCROLLBAR_ORIGINAL.load(Ordering::Acquire)) };
    let call = || unsafe {
        original(
            state,
            out,
            x,
            y,
            w,
            h,
            has_scrolling,
            offset,
            target,
            step,
            button_pixel_inc,
            style,
            input,
            font,
        )
    };
    if style.is_null() || !HIDE_ARROWS.load(Ordering::Acquire) {
        return call();
    }
    // Hidden for this one call; the style keeps its own value for everything else.
    let show = unsafe { style.cast::<u8>().add(SCROLLBAR_SHOW_BUTTONS).cast::<i32>() };
    let before = unsafe { show.read_unaligned() };
    unsafe { show.write_unaligned(0) };
    let result = call();
    unsafe { show.write_unaligned(before) };
    result
}

/// Draws the screen for `variant` (0 main menu, 1 pause menu). False when the screen is not
/// available.
pub fn draw(variant: i32, host: &mut dyn ScreenHost) -> bool {
    let Some(fns) = FNS.get().copied() else {
        return false;
    };
    let ctx = fns.ctx();
    if ctx.is_null() {
        return true;
    }
    // Ctrl+C / X / V / A in the screen's text boxes and the color editor's hex box.
    crate::menukeys::feed(ctx);
    SCREEN.with(|cell| {
        let Ok(mut screen) = cell.try_borrow_mut() else {
            return;
        };
        if draw_screen(&fns, ctx, variant, &mut screen, host) {
            CLOSE_PENDING.store(true, Ordering::Release);
        }
    });
    true
}

/// Leaves the screen if the footer asked to. Called after the session is released, because
/// leaving runs game code that may re-enter the host's hooks.
pub fn finish(variant: i32) {
    if CLOSE_PENDING.swap(false, Ordering::AcqRel) {
        if let Some(fns) = FNS.get() {
            leave(fns, variant);
        }
    }
}

/// From the main menu, a game state transition. From the pause menu, its selector goes back to
/// the pause menu's root, but only if it still shows the options.
fn leave(fns: &Fns, variant: i32) {
    if variant == 1 {
        let selector = unsafe { fns.pause_screen.read() };
        if selector == PAUSE_SCREEN_OPTIONS {
            unsafe { fns.pause_screen.write(PAUSE_SCREEN_ROOT) };
            crate::session::host_line(W2R_LOG_INFO, "settings", "leaving to the pause menu root");
        } else {
            crate::session::host_line(
                W2R_LOG_WARN,
                "settings",
                &format!(
                    "NOT leaving: pause selector is {selector}, expected {PAUSE_SCREEN_OPTIONS}"
                ),
            );
        }
    } else {
        crate::session::host_line(W2R_LOG_INFO, "settings", "leaving to the main menu");
        unsafe { (fns.transition)(GAME_STATE_MAIN_MENU) };
    }
}

fn draw_screen(
    fns: &Fns,
    ctx: *mut c_void,
    variant: i32,
    screen: &mut Screen,
    host: &mut dyn ScreenHost,
) -> bool {
    let now = crate::chat::now_ms();
    let fresh = now.saturating_sub(screen.last_frame_ms) > REENTRY_GAP_MS;
    screen.last_frame_ms = now;
    if fresh {
        let (editor, native, owners) = host.parts();
        refresh_monitor_list(fns, editor);
        editor.enter(native, owners);
        screen.edits.clear();
        screen.search = [0; SEARCH_BUFFER];
        screen.color = None;
        screen.categories = editor.registry.categories();
        screen.categories.push(SEARCH_CATEGORY.to_owned());
        screen.category = screen.categories[0].clone();
    }
    screen.tip = None;
    screen.combos_this_frame = 0;

    // Floored, with every edge derived from these, so neighbouring panels share exact edges.
    // Fractional edges leave a hairline gap between panels.
    let ui_w = unsafe { (fns.ui_width)() }.floor();
    let panel_w = (ui_w * PANEL_WIDTH_PERCENT * 0.01).floor();
    let panel_x = ((ui_w - panel_w) * 0.5).floor();
    let left_w = (panel_w * CATEGORY_PANEL_PERCENT * 0.01).floor();
    let right_x = panel_x + left_w + GAP;
    let right_w = panel_w - left_w - GAP;
    let content_h = PANEL_HEIGHT - FOOTER_HEIGHT - GAP;
    let footer_y = PANEL_TOP + content_h + GAP;

    // While the color popup is open, the screen stays behind it and takes no input.
    let editing = screen.color.is_some();
    let flags = if editing {
        GROUP_NO_SCROLLBAR | WINDOW_BACKGROUND
    } else {
        GROUP_NO_SCROLLBAR
    };
    let prev_skin = fns.push_skin(fns.window_skin());
    let name = c"w2r_settings";
    if unsafe { (fns.begin_window)(ctx, name.as_ptr(), 0.0, 0.0, ui_w, UI_HEIGHT, flags) } == 0 {
        unsafe { (fns.end_window)(ctx) };
        fns.push_skin(prev_skin);
        return false;
    }
    set_read_only(ctx, editing);
    unsafe { (fns.space_begin)(ctx, 1, 0.0, 1000) };

    // Commands draw in the order they are pushed, so the backdrop goes first.
    if variant == 1 {
        draw_backdrop(fns, ctx, panel_x, panel_w, footer_y);
    }
    // The stock options screen draws the banner on the main menu only.
    if variant == 0 {
        unsafe { (fns.banner)() };
    }
    rect(fns, ctx, panel_x, TITLE_TOP, panel_w, TITLE_HEIGHT);
    label(fns, ctx, &fns.localize("option_title"), ALIGN_TITLE);

    let fits = category_list_fits(ctx, screen.categories.len(), content_h);
    if begin_padded_panel(
        fns,
        ctx,
        c"w2r_settings_categories",
        c"w2r_settings_categories_inner",
        [panel_x, PANEL_TOP, left_w, content_h],
        0,
        fits,
    ) {
        draw_category_panel(fns, ctx, screen);
        end_padded_panel(fns, ctx);
    }

    let searching = screen.category.eq_ignore_ascii_case(SEARCH_CATEGORY);
    let mut content_top = PANEL_TOP;
    let mut content_space = content_h;
    // The search box is placed directly in the window, not in a group: a text box inside a group
    // that does not take input can never get focus.
    if searching {
        rect(
            fns,
            ctx,
            right_x + PANEL_PAD,
            content_top + PANEL_PAD_TOP,
            right_w - PANEL_PAD * 2.0,
            SEARCH_ROW_HEIGHT,
        );
        let prev = fns.push_skin(unsafe { *fns.skin_text_edit });
        unsafe {
            (fns.text_edit)(
                ctx,
                TEXT_EDIT_FLAGS,
                screen.search.as_mut_ptr().cast(),
                SEARCH_BUFFER as i32,
                fns.text_edit_filter,
            )
        };
        fns.push_skin(prev);
        let strip = SEARCH_ROW_HEIGHT + PANEL_PAD_TOP + GAP;
        content_top += strip + GAP;
        content_space -= strip + GAP;
    }

    if begin_padded_panel(
        fns,
        ctx,
        c"w2r_settings_content",
        c"w2r_settings_content_inner",
        [right_x, content_top, right_w, content_space],
        0,
        false,
    ) {
        let saved = read_f32(ctx, CTX_SPACING_Y);
        set_row_spacing(ctx, ROW_SPACING);
        let (editor, native, _) = host.parts();
        if searching {
            draw_search_pane(fns, ctx, screen, editor, native);
        } else {
            draw_settings_pane(fns, ctx, screen, editor, native);
        }
        set_row_spacing(ctx, saved);
        end_padded_panel(fns, ctx);
    }

    let mut close = false;
    rect(fns, ctx, panel_x, footer_y, panel_w, FOOTER_HEIGHT);
    if unsafe { (fns.begin_group)(ctx, c"w2r_settings_footer".as_ptr(), GROUP_NO_SCROLLBAR) } != 0 {
        let (editor, native, owners) = host.parts();
        close = draw_footer(fns, ctx, FOOTER_HEIGHT - GAP, editor, native, owners) && !editing;
        unsafe { (fns.end_group)(ctx) };
    }

    // Last, so it is drawn over everything.
    if !editing {
        draw_tooltip(fns, ctx, ui_w, screen);
    }

    unsafe {
        (fns.space_end)(ctx);
        (fns.end_window)(ctx);
    }
    fns.push_skin(prev_skin);
    // Drawn after the screen, so it is on top. Also when it was opened during this frame.
    if screen.color.is_some() {
        let (editor, native, _) = host.parts();
        draw_setting_color_popup(fns, ctx, ui_w, screen, editor, native);
    }
    close
}

/// The color editor for a setting: OK stores the picked color in the setting.
fn draw_setting_color_popup(
    fns: &Fns,
    ctx: *mut c_void,
    ui_w: f32,
    screen: &mut Screen,
    editor: &mut Editor,
    native: &mut dyn Native,
) {
    let Screen { color, picker, .. } = screen;
    let Some(edit) = color.as_mut() else {
        return;
    };
    let d = editor.registry.def(edit.index).clone();
    let title = fns.localize(&d.label);
    let picked = draw_color_popup(
        fns,
        ctx,
        ui_w,
        c"w2r_settings_color",
        edit,
        picker,
        &title,
        Some(&d.default_str),
    );
    match picked {
        Picked::Open => {}
        Picked::Ok(hex) => {
            let index = edit.index;
            editor.set(index, Value::Str(hex), native);
            *color = None;
        }
        Picked::Cancel => *color = None,
    }
}

/// Builds the monitor dropdown from the game's display count. Done on each entry, because the
/// count is not known yet at startup.
fn refresh_monitor_list(fns: &Fns, editor: &mut Editor) {
    let Some(i) = editor
        .registry
        .find(&crate::settings::Owner::Game, "display_monitor")
    else {
        return;
    };
    let count = unsafe { (fns.display_count)() };
    // Keep the slider when the count is unavailable or implausible.
    if !(1..=16).contains(&count) {
        return;
    }
    let names = (1..=count).map(|n| format!("Display {n}")).collect();
    editor.registry.set_choices(i, names);
}

fn rect(fns: &Fns, ctx: *mut c_void, x: f32, y: f32, w: f32, h: f32) {
    unsafe { (fns.rect)(ctx, x, y, w, h) };
}

fn row(fns: &Fns, ctx: *mut c_void, height: f32, cols: i32) {
    unsafe { (fns.row)(ctx, height, cols) };
}

fn label(fns: &Fns, ctx: *mut c_void, text: &str, align: i32) {
    let t = cstr(text);
    unsafe { (fns.label)(ctx, t.as_ptr().cast(), align) };
}

fn button(fns: &Fns, ctx: *mut c_void, text: &str) -> bool {
    let t = cstr(text);
    unsafe { (fns.button)(ctx, t.as_ptr().cast()) != 0 }
}

/// Opens a group with the content-panel frame.
fn begin_panel_group(fns: &Fns, ctx: *mut c_void, name: &CStr, flags: u32) -> bool {
    let prev = fns.push_skin(unsafe { *fns.skin_content });
    let opened = unsafe { (fns.begin_group)(ctx, name.as_ptr(), flags) } != 0;
    fns.push_skin(prev);
    opened
}

/// A framed panel with a content group inset inside it, so the content does not touch the frame.
///
/// Nuklear reserves a scrollbar's width in a group even when no scrollbar is drawn.
/// `suppress_scrollbar_width` removes it; pass it only when the content fits without scrolling.
fn begin_padded_panel(
    fns: &Fns,
    ctx: *mut c_void,
    frame: &CStr,
    content: &CStr,
    [x, y, w, h]: [f32; 4],
    content_flags: u32,
    suppress_scrollbar_width: bool,
) -> bool {
    let top = PANEL_PAD + PANEL_PAD_TOP;
    rect(fns, ctx, x, y, w, h);
    if !begin_panel_group(fns, ctx, frame, GROUP_NO_SCROLLBAR) {
        return false;
    }
    unsafe { (fns.space_begin)(ctx, 1, 0.0, 1000) };
    rect(
        fns,
        ctx,
        PANEL_PAD,
        top,
        w - PANEL_PAD * 2.0,
        h - top - PANEL_PAD,
    );
    let saved = read_f32(ctx, CTX_SCROLLBAR_SIZE);
    if suppress_scrollbar_width {
        write_f32(ctx, CTX_SCROLLBAR_SIZE, 0.0);
    }
    let opened = unsafe { (fns.begin_group)(ctx, content.as_ptr(), content_flags) } != 0;
    write_f32(ctx, CTX_SCROLLBAR_SIZE, saved);
    if !opened {
        unsafe {
            (fns.space_end)(ctx);
            (fns.end_group)(ctx);
        }
        return false;
    }
    true
}

fn end_padded_panel(fns: &Fns, ctx: *mut c_void) {
    unsafe {
        (fns.end_group)(ctx);
        (fns.space_end)(ctx);
        (fns.end_group)(ctx);
    }
}

/// Whether the category buttons fit their panel without scrolling.
fn category_list_fits(ctx: *mut c_void, count: usize, h: f32) -> bool {
    if count == 0 {
        return true;
    }
    let spacing = read_f32(ctx, CTX_SPACING_Y);
    let padding = read_f32(ctx, CTX_GROUP_PADDING_Y);
    let needed = count as f32 * CATEGORY_ROW_HEIGHT + (count - 1) as f32 * spacing;
    let available = h - (PANEL_PAD + PANEL_PAD_TOP) - PANEL_PAD - padding * 2.0;
    needed <= available
}

fn draw_category_panel(fns: &Fns, ctx: *mut c_void, screen: &mut Screen) {
    row(fns, ctx, CATEGORY_ROW_HEIGHT, 1);
    let mut picked = None;
    for c in &screen.categories {
        // The current category's button is disabled.
        let current = c.eq_ignore_ascii_case(&screen.category);
        if current {
            unsafe { (fns.disable_begin)(ctx) };
        }
        if button(fns, ctx, c) && !current {
            picked = Some(c.clone());
        }
        if current {
            unsafe { (fns.disable_end)(ctx) };
        }
    }
    if let Some(c) = picked {
        screen.category = c;
    }
}

/// The rectangle of the next cell of the current row, on screen, without using it up.
fn next_cell(fns: &Fns, ctx: *mut c_void) -> Option<[f32; 4]> {
    let mut b = [0f32; 4];
    unsafe { (fns.widget_space)(b.as_mut_ptr(), ctx, core::ptr::null_mut(), 0) };
    (b[2] > 0.0 && b[3] > 0.0).then_some(b)
}

/// A filled rectangle on the current window's canvas.
fn fill(ctx: *mut c_void, fns: &Fns, [x, y, w, h]: [f32; 4], colour: u32) {
    let win = current_window(ctx);
    if win.is_null() || w <= 0.0 || h <= 0.0 {
        return;
    }
    let canvas = unsafe { win.cast::<u8>().add(WINDOW_BUFFER).cast::<c_void>() };
    unsafe { (fns.fill_rect)(canvas, x, y, w, h, 0.0, colour) };
}

/// A rectangle's outline, `t` thick, inside its edges.
fn outline(ctx: *mut c_void, fns: &Fns, [x, y, w, h]: [f32; 4], t: f32, colour: u32) {
    fill(ctx, fns, [x, y, w, t], colour);
    fill(ctx, fns, [x, y + h - t, w, t], colour);
    fill(ctx, fns, [x, y, t, h], colour);
    fill(ctx, fns, [x + w - t, y, t, h], colour);
}

fn inside([x, y, w, h]: [f32; 4], px: f32, py: f32) -> bool {
    px >= x && px < x + w && py >= y && py < y + h
}

fn open_color_editor(screen: &mut Screen, index: usize, hex: &str, default: &str) {
    let original = color::parse_hex(hex)
        .or_else(|| color::parse_hex(default))
        .unwrap_or(Rgb { r: 0, g: 0, b: 0 });
    let mut edit = ColorEdit {
        index,
        hsv: color::to_hsv(original),
        original,
        hex: [0; 8],
        drag: Drag::None,
    };
    set_hex_buffer(&mut edit.hex, original);
    screen.color = Some(edit);
}

fn set_hex_buffer(buf: &mut [u8; 8], c: Rgb) {
    *buf = [0; 8];
    buf[..6].copy_from_slice(color::to_hex(c).as_bytes());
}

/// Makes the editor's textures, once. False if the game cannot make them yet or at all.
fn ensure_picker_textures(fns: &Fns, picker: &mut Picker) -> bool {
    if picker.square != 0 && picker.hue != 0 {
        return true;
    }
    if picker.failed || unsafe { *fns.sub_image_slot } == 0 {
        return false;
    }
    let make = |w: i32, h: i32, pixels: &[u8]| -> usize {
        let mut tex: *mut GameTexture = core::ptr::null_mut();
        unsafe {
            (fns.create_texture)(
                &mut tex,
                w,
                h,
                pixels.as_ptr().cast(),
                FORMAT_RGBA8,
                FILTER_LINEAR,
            )
        };
        let ok = !tex.is_null() && unsafe { (*tex).gl_name != 0 && (*tex).w == w && (*tex).h == h };
        if ok { tex as usize } else { 0 }
    };
    picker.square = make(
        SQUARE_TEXELS,
        SQUARE_TEXELS,
        &color::square_pixels(0.0, SQUARE_TEXELS as usize),
    );
    picker.square_hue = 0.0;
    picker.hue = make(
        HUE_TEXEL_WIDTH,
        HUE_TEXELS,
        &color::hue_pixels(HUE_TEXEL_WIDTH as usize, HUE_TEXELS as usize),
    );
    if picker.square == 0 || picker.hue == 0 {
        picker.failed = true;
        crate::session::host_line(
            W2R_LOG_WARN,
            "settings",
            "the game would not make the color picker's textures; using sliders",
        );
        return false;
    }
    true
}

/// Redraws the square's texture for `hue` when it changed.
fn update_square(fns: &Fns, picker: &mut Picker, hue: f32) {
    if (picker.square_hue - hue).abs() < 0.5 {
        return;
    }
    let upload = unsafe { *fns.sub_image_slot };
    if upload == 0 {
        return;
    }
    let upload: SubImageFn = unsafe { core::mem::transmute(upload) };
    let pixels = color::square_pixels(hue, SQUARE_TEXELS as usize);
    let tex = picker.square as *mut GameTexture;
    unsafe {
        upload(
            (*tex).gl_name,
            0,
            0,
            0,
            SQUARE_TEXELS,
            SQUARE_TEXELS,
            GL_RGBA,
            GL_UNSIGNED_BYTE,
            pixels.as_ptr().cast(),
        )
    };
    picker.square_hue = hue;
}

fn draw_texture(
    ctx: *mut c_void,
    fns: &Fns,
    texture: usize,
    size: (i32, i32),
    [x, y, w, h]: [f32; 4],
) {
    let win = current_window(ctx);
    if win.is_null() {
        return;
    }
    let canvas = unsafe { win.cast::<u8>().add(WINDOW_BUFFER).cast::<c_void>() };
    let image = NkImage {
        handle: texture as *mut c_void,
        w: size.0 as u16,
        h: size.1 as u16,
        region: [0, 0, size.0 as u16, size.1 as u16],
    };
    unsafe { (fns.draw_image)(canvas, x, y, w, h, &image, WHITE) };
}

/// Sets or clears Nuklear's read-only flag on the current window and its panel. The panel takes
/// its copy of the window's flags when the window begins, so both are written; groups begun
/// afterwards inherit it from the window.
fn set_read_only(ctx: *mut c_void, on: bool) {
    let win = current_window(ctx);
    let panel = current_panel(ctx);
    for (base, offset) in [(win, WINDOW_FLAGS), (panel, PANEL_FLAGS)] {
        if base.is_null() {
            continue;
        }
        let flags = unsafe { base.cast::<u8>().add(offset).cast::<u32>() };
        let value = unsafe { flags.read_unaligned() };
        let value = if on {
            value | WINDOW_ROM
        } else {
            value & !WINDOW_ROM
        };
        unsafe { flags.write_unaligned(value) };
    }
}

/// What the color editor's buttons did this frame.
enum Picked {
    /// Still open.
    Open,
    /// OK: the picked color, six lowercase hex digits.
    Ok(String),
    Cancel,
}

/// The color popup: a window of its own (`window`) over the whole screen, dimming it, with the
/// picker in a popup frame in the middle. A saturation / brightness square, a hue strip beside it,
/// the new and the current color, a hex box, Reset to Default (when there is a `default`), and
/// OK / Cancel. Falls back to three sliders if the game cannot make the textures. The screen
/// behind it is read-only while it is open. Used by the options screen and by plugin cards.
#[allow(clippy::too_many_arguments)]
fn draw_color_popup(
    fns: &Fns,
    ctx: *mut c_void,
    ui_w: f32,
    window: &CStr,
    edit: &mut ColorEdit,
    picker: &mut Picker,
    title: &str,
    default: Option<&str>,
) -> Picked {
    let prev_skin = fns.push_skin(fns.window_skin());
    let opened = unsafe {
        (fns.begin_window)(
            ctx,
            window.as_ptr(),
            0.0,
            0.0,
            ui_w,
            UI_HEIGHT,
            GROUP_NO_SCROLLBAR,
        )
    } != 0;
    let mut picked = Picked::Open;
    if opened {
        unsafe { (fns.space_begin)(ctx, 1, 0.0, 1000) };
        fill(ctx, fns, [0.0, 0.0, ui_w, UI_HEIGHT], POPUP_SHADE);
        let layout = PickerLayout::centred(ui_w);
        picked = draw_color_picker(fns, ctx, edit, picker, title, default, &layout);
        unsafe { (fns.space_end)(ctx) };
    }
    unsafe { (fns.end_window)(ctx) };
    fns.push_skin(prev_skin);
    picked
}

type Area = [f32; 4];

/// Where everything in the color popup goes, in UI units.
struct PickerLayout {
    frame: Area,
    title: Area,
    square: Area,
    hue: Area,
    swatch_labels: [Area; 2],
    swatches: [Area; 2],
    hex_label: Area,
    hex_box: Area,
    reset: Area,
    cancel: Area,
    ok: Area,
}

impl PickerLayout {
    /// The popup sized to its contents plus padding, centred on a screen `ui_w` wide.
    fn centred(ui_w: f32) -> PickerLayout {
        let column_w = SWATCH * 2.0 + SWATCH_GAP;
        let content_w = PICKER_SIDE + HUE_GAP + HUE_WIDTH + COLUMN_GAP + column_w;
        let content_h = ROW_HEIGHT + SECTION_GAP + PICKER_SIDE + SECTION_GAP + BUTTON_HEIGHT;
        let frame_w = content_w + POPUP_PAD_X * 2.0;
        let frame_h = content_h + POPUP_PAD_Y * 2.0;
        let fx = ((ui_w - frame_w) * 0.5).floor();
        let fy = ((UI_HEIGHT - frame_h) * 0.5).floor();
        let x = fx + POPUP_PAD_X;
        let y = fy + POPUP_PAD_Y;

        let top = y + ROW_HEIGHT + SECTION_GAP;
        let hue_x = x + PICKER_SIDE + HUE_GAP;
        let col_x = hue_x + HUE_WIDTH + COLUMN_GAP;
        let swatch_y = top + ROW_HEIGHT;
        let hex_y = swatch_y + SWATCH + SECTION_GAP;
        let reset_y = hex_y + ROW_HEIGHT + SEARCH_ROW_HEIGHT + SECTION_GAP;
        let buttons_y = top + PICKER_SIDE + SECTION_GAP;
        let buttons_x = x + (content_w - BUTTON_WIDTH * 2.0 - BUTTON_GAP) * 0.5;
        let second = col_x + SWATCH + SWATCH_GAP;
        PickerLayout {
            frame: [fx, fy, frame_w, frame_h],
            title: [x, y, content_w, ROW_HEIGHT],
            square: [x, top, PICKER_SIDE, PICKER_SIDE],
            hue: [hue_x, top, HUE_WIDTH, PICKER_SIDE],
            swatch_labels: [
                [col_x, top, SWATCH, ROW_HEIGHT],
                [second, top, SWATCH, ROW_HEIGHT],
            ],
            swatches: [
                [col_x, swatch_y, SWATCH, SWATCH],
                [second, swatch_y, SWATCH, SWATCH],
            ],
            hex_label: [col_x, hex_y, column_w, ROW_HEIGHT],
            hex_box: [col_x, hex_y + ROW_HEIGHT, column_w, SEARCH_ROW_HEIGHT],
            reset: [col_x, reset_y, column_w, BUTTON_HEIGHT],
            cancel: [buttons_x, buttons_y, BUTTON_WIDTH, BUTTON_HEIGHT],
            ok: [
                buttons_x + BUTTON_WIDTH + BUTTON_GAP,
                buttons_y,
                BUTTON_WIDTH,
                BUTTON_HEIGHT,
            ],
        }
    }
}

fn place(fns: &Fns, ctx: *mut c_void, [x, y, w, h]: Area) {
    rect(fns, ctx, x, y, w, h);
}

fn draw_color_picker(
    fns: &Fns,
    ctx: *mut c_void,
    edit: &mut ColorEdit,
    picker: &mut Picker,
    title: &str,
    default: Option<&str>,
    layout: &PickerLayout,
) -> Picked {
    // The frame the game's own popups use.
    let skin = unsafe {
        if fns.alt() {
            *fns.skin_popup_alt
        } else {
            *fns.skin_popup
        }
    };
    place(fns, ctx, layout.frame);
    let prev = fns.push_skin(skin);
    let framed = unsafe {
        (fns.begin_group)(
            ctx,
            c"w2r_settings_color_frame".as_ptr(),
            GROUP_NO_SCROLLBAR,
        )
    } != 0;
    fns.push_skin(prev);
    if framed {
        unsafe { (fns.end_group)(ctx) };
    }
    place(fns, ctx, layout.title);
    label(fns, ctx, title, ALIGN_TITLE);

    let square = layout.square;
    let hue = layout.hue;
    let side = square[2];
    if ensure_picker_textures(fns, picker) {
        // Dragging: a press inside the square or the strip starts it, releasing ends it.
        let mx = read_f32(ctx, CTX_MOUSE_X);
        let my = read_f32(ctx, CTX_MOUSE_Y);
        let down = unsafe {
            ctx.cast::<u8>()
                .add(CTX_MOUSE_LEFT_DOWN)
                .cast::<i32>()
                .read_unaligned()
        } != 0;
        let clicked = unsafe {
            ctx.cast::<u8>()
                .add(CTX_MOUSE_LEFT_CLICKED)
                .cast::<u32>()
                .read_unaligned()
        } != 0;
        if down && clicked {
            edit.drag = if inside(square, mx, my) {
                Drag::Square
            } else if inside(hue, mx, my) {
                Drag::Hue
            } else {
                Drag::None
            };
        }
        if !down {
            edit.drag = Drag::None;
        }
        let fraction = |v: f32, from: f32, len: f32| ((v - from) / len).clamp(0.0, 1.0);
        match edit.drag {
            Drag::Square => {
                edit.hsv.s = fraction(mx, square[0], side);
                edit.hsv.v = 1.0 - fraction(my, square[1], side);
            }
            Drag::Hue => edit.hsv.h = (360.0 * fraction(my, hue[1], side)).min(359.9),
            Drag::None => {}
        }
        if edit.drag != Drag::None {
            set_hex_buffer(&mut edit.hex, color::to_rgb(edit.hsv));
        }

        update_square(fns, picker, edit.hsv.h);
        draw_texture(
            ctx,
            fns,
            picker.square,
            (SQUARE_TEXELS, SQUARE_TEXELS),
            square,
        );
        draw_texture(ctx, fns, picker.hue, (HUE_TEXEL_WIDTH, HUE_TEXELS), hue);
        outline(ctx, fns, square, 2.0, BLACK);
        outline(ctx, fns, hue, 2.0, BLACK);

        // Markers: a ring at the picked saturation and brightness, a bar at the picked hue.
        let rx = square[0] + edit.hsv.s * side;
        let ry = square[1] + (1.0 - edit.hsv.v) * side;
        let ring = if edit.hsv.v > 0.6 && edit.hsv.s < 0.4 {
            BLACK
        } else {
            WHITE
        };
        outline(
            ctx,
            fns,
            [rx - MARKER, ry - MARKER, MARKER * 2.0, MARKER * 2.0],
            MARKER_LINE,
            ring,
        );
        let hy = hue[1] + edit.hsv.h / 360.0 * side;
        outline(
            ctx,
            fns,
            [hue[0] - 8.0, hy - 6.0, hue[2] + 16.0, 12.0],
            MARKER_LINE,
            WHITE,
        );
    } else {
        // Hue, saturation and brightness as three sliders.
        let mut parts = [
            ("Hue", edit.hsv.h.round() as u32, 359u32),
            ("Saturation", (edit.hsv.s * 100.0).round() as u32, 100),
            ("Brightness", (edit.hsv.v * 100.0).round() as u32, 100),
        ];
        let mut changed = false;
        for (k, (name, value, max)) in parts.iter_mut().enumerate() {
            let ry = square[1] + k as f32 * (ROW_HEIGHT + GAP) * 2.0;
            rect(fns, ctx, square[0], ry, side, ROW_HEIGHT);
            label(fns, ctx, &format!("{name}: {value}"), ALIGN_ROW);
            rect(fns, ctx, square[0], ry + ROW_HEIGHT, side, ROW_HEIGHT);
            let (mut lo, mut step) = (0u32, 1u32);
            changed |= unsafe { (fns.slider)(value, &mut lo, max, &mut step) };
        }
        if changed {
            edit.hsv = Hsv {
                h: parts[0].1 as f32,
                s: parts[1].1 as f32 / 100.0,
                v: parts[2].1 as f32 / 100.0,
            };
            set_hex_buffer(&mut edit.hex, color::to_rgb(edit.hsv));
        }
    }

    // The new and the current color, side by side.
    let new = color::to_rgb(edit.hsv);
    for (k, (name, c)) in [("New", new), ("Current", edit.original)]
        .into_iter()
        .enumerate()
    {
        place(fns, ctx, layout.swatch_labels[k]);
        label(fns, ctx, name, ALIGN_ROW);
        fill(ctx, fns, layout.swatches[k], color::nk_color(c));
        outline(ctx, fns, layout.swatches[k], 2.0, BLACK);
    }

    place(fns, ctx, layout.hex_label);
    label(fns, ctx, "Hex", ALIGN_ROW);
    place(fns, ctx, layout.hex_box);
    let prev = fns.push_skin(unsafe { *fns.skin_text_edit });
    let state = unsafe {
        (fns.text_edit)(
            ctx,
            TEXT_EDIT_FLAGS,
            edit.hex.as_mut_ptr().cast(),
            7,
            fns.text_edit_filter,
        )
    };
    fns.push_skin(prev);
    // A typed color takes effect once it is six hex digits.
    if state & 1 != 0 && edit.drag == Drag::None {
        if let Some(c) = color::parse_hex(&buffer_text(&edit.hex)) {
            if c != color::to_rgb(edit.hsv) {
                edit.hsv = color::to_hsv(c);
            }
        }
    }

    if let Some(default) = default {
        place(fns, ctx, layout.reset);
        if button(fns, ctx, "Reset to Default") {
            if let Some(c) = color::parse_hex(default) {
                edit.hsv = color::to_hsv(c);
                set_hex_buffer(&mut edit.hex, c);
            }
        }
    }

    place(fns, ctx, layout.cancel);
    let cancel = button(fns, ctx, "Cancel");
    place(fns, ctx, layout.ok);
    let ok = button(fns, ctx, "OK");
    if ok {
        Picked::Ok(color::to_hex(color::to_rgb(edit.hsv)))
    } else if cancel {
        Picked::Cancel
    } else {
        Picked::Open
    }
}

// ---- color rows on plugin cards (W2rUi.color_edit) -------------------------------------------

/// The color editor opened from a plugin card's color row, and what its OK picked. Kept apart from
/// the options screen's, which is a different screen with its own state.
struct CardColor {
    /// The row being edited (`ui::Widgets::color_edit`'s key) and the editor's title.
    open: Option<(String, String, ColorEdit)>,
    /// OK's color for the row with this key, handed to the row the next time it is drawn.
    picked: Option<(String, String)>,
    picker: Picker,
}

thread_local! {
    static CARD_COLOR: RefCell<CardColor> = const {
        RefCell::new(CardColor {
            open: None,
            picked: None,
            picker: Picker {
                square: 0,
                hue: 0,
                square_hue: -1.0,
                failed: false,
            },
        })
    };
}

/// Whether a card's color editor is open.
pub(crate) fn card_color_open() -> bool {
    CARD_COLOR.with(|c| c.try_borrow().map(|c| c.open.is_some()).unwrap_or(false))
}

/// Closes a card's color editor without a pick (the plugin manager was opened or closed).
pub(crate) fn card_color_cancel() {
    CARD_COLOR.with(|c| {
        if let Ok(mut c) = c.try_borrow_mut() {
            c.open = None;
            c.picked = None;
        }
    });
}

/// Opens the editor for the row `key`, titled `title`, starting from `hex` (`rrggbb`). False when
/// the screen's functions are not available.
pub(crate) fn card_color_start(key: &str, title: &str, hex: &str) -> bool {
    if FNS.get().is_none() {
        return false;
    }
    let original = color::parse_hex(hex).unwrap_or(Rgb { r: 0, g: 0, b: 0 });
    let mut edit = ColorEdit {
        index: usize::MAX,
        hsv: color::to_hsv(original),
        original,
        hex: [0; 8],
        drag: Drag::None,
    };
    set_hex_buffer(&mut edit.hex, original);
    CARD_COLOR.with(|c| {
        if let Ok(mut c) = c.try_borrow_mut() {
            c.open = Some((key.to_owned(), title.to_owned(), edit));
            c.picked = None;
        }
    });
    true
}

/// The color OK picked for the row `key`, once.
pub(crate) fn card_color_take(key: &str) -> Option<String> {
    CARD_COLOR.with(|c| {
        let mut c = c.try_borrow_mut().ok()?;
        if c.picked.as_ref().is_some_and(|(k, _)| k == key) {
            c.picked.take().map(|(_, hex)| hex)
        } else {
            None
        }
    })
}

/// Draws a swatch of `hex` in the next cell of the current row and moves past the cell. False
/// when the screen's functions are not available (the cell is then left for the caller).
pub(crate) fn card_swatch(ctx: *mut c_void, hex: &str) -> bool {
    let Some(fns) = FNS.get() else {
        return false;
    };
    if let Some([x, y, w, h]) = next_cell(fns, ctx) {
        let rgb = color::parse_hex(hex).unwrap_or(Rgb { r: 0, g: 0, b: 0 });
        let inset = h * 0.15;
        let side = (h - inset * 2.0).min(w - SWATCH_ROW_GAP).max(0.0);
        let swatch = [x + w - side - SWATCH_ROW_GAP, y + inset, side, side];
        fill(ctx, fns, swatch, color::nk_color(rgb));
        outline(ctx, fns, swatch, 2.0, BLACK);
    }
    unsafe { (fns.skip_cell)(ctx) };
    true
}

thread_local! {
    /// Whether each card dropdown (by `ui::Widgets::combo`'s key) was open last frame: its header
    /// is drawn with the open skin, as on the options screen.
    static CARD_COMBOS: RefCell<Vec<(String, bool)>> = const { RefCell::new(Vec::new()) };
}

/// A plugin card's dropdown in the next cell of the current row, drawn as the options screen's:
/// the header shows `choices[*selected]`; the popup lists every choice, rows shrunk to fit as on
/// the options screen (up to 16). True on the frame a different entry was picked; None when the
/// screen's functions are not available (nothing drawn).
pub(crate) fn card_combo(
    ctx: *mut c_void,
    key: &str,
    choices: &[&str],
    selected: &mut usize,
) -> Option<bool> {
    let fns = FNS.get()?;
    if choices.is_empty() {
        return Some(false);
    }
    let current = (*selected).min(choices.len() - 1);
    let was_open = CARD_COMBOS.with(|m| {
        m.borrow()
            .iter()
            .find(|(k, _)| k == key)
            .is_some_and(|(_, open)| *open)
    });
    let width = next_cell(fns, ctx).map_or(600.0, |[_, _, w, _]| w);
    let (item_h, combo_h) = combo_layout(choices.len());
    let spacing = read_f32(ctx, CTX_SPACING_Y);
    let prev = fns.push_skin(fns.combo_skin(was_open));
    set_row_spacing(ctx, spacing);
    let header = cstr(choices[current]);
    let opened =
        unsafe { (fns.combo_begin)(ctx, header.as_ptr().cast(), ALIGN_ROW, width, combo_h) } != 0;
    fns.push_skin(prev);
    set_row_spacing(ctx, spacing);
    CARD_COMBOS.with(|m| {
        let mut m = m.borrow_mut();
        match m.iter_mut().find(|(k, _)| k == key) {
            Some(entry) => entry.1 = opened,
            None => m.push((key.to_owned(), opened)),
        }
    });
    let mut changed = false;
    if opened {
        // No spacing between the popup's items. Set before the first row.
        set_row_spacing(ctx, 0.0);
        for (o, choice) in choices.iter().enumerate() {
            row(fns, ctx, item_h, 1);
            let t = cstr(choice);
            if unsafe { (fns.combo_item)(ctx, t.as_ptr().cast(), ALIGN_ROW) } != 0 {
                if o != current {
                    *selected = o;
                    changed = true;
                }
                unsafe { (fns.combo_close)(ctx) };
            }
        }
        unsafe { (fns.combo_end)(ctx) };
        set_row_spacing(ctx, spacing);
    }
    Some(changed)
}

/// Makes the current window read-only, or not (the plugin manager while a card's editor is open).
pub(crate) fn set_window_read_only(ctx: *mut c_void, on: bool) {
    set_read_only(ctx, on);
}

/// Draws a card's color editor over everything, if one is open. Called after the plugin manager.
pub(crate) fn draw_card_color_popup() {
    let Some(fns) = FNS.get().copied() else {
        return;
    };
    let ctx = fns.ctx();
    if ctx.is_null() {
        return;
    }
    let ui_w = unsafe { (fns.ui_width)() }.floor();
    CARD_COLOR.with(|c| {
        let Ok(mut c) = c.try_borrow_mut() else {
            return;
        };
        let CardColor {
            open,
            picked,
            picker,
        } = &mut *c;
        let Some((key, title, edit)) = open.as_mut() else {
            return;
        };
        let result = draw_color_popup(
            &fns,
            ctx,
            ui_w,
            c"w2r_card_color",
            edit,
            picker,
            title,
            None,
        );
        match result {
            Picked::Open => {}
            Picked::Ok(hex) => {
                *picked = Some((key.clone(), hex));
                *open = None;
            }
            Picked::Cancel => *open = None,
        }
    });
}

/// Rows as tall as a text box, which get the wider gap.
fn is_text_row(d: &SettingDef) -> bool {
    d.kind.is_text()
}

/// The selected category's settings. Spacing is set before every row, because pushing a skin
/// resets it. A row's spacing is the gap below it, so a text box gets a wider gap above it by
/// widening the row before it.
fn draw_settings_pane(
    fns: &Fns,
    ctx: *mut c_void,
    screen: &mut Screen,
    editor: &mut Editor,
    native: &mut dyn Native,
) {
    let groups = editor.registry.groups(&screen.category);
    for (name, items) in groups {
        let first_text = items
            .first()
            .is_some_and(|&i| is_text_row(editor.registry.def(i)));
        set_row_spacing(
            ctx,
            if first_text {
                TEXT_ROW_SPACING
            } else {
                ROW_SPACING
            },
        );
        row(fns, ctx, ROW_HEIGHT, 1);
        label(fns, ctx, &name, ALIGN_TITLE);
        for (k, &i) in items.iter().enumerate() {
            let this_text = is_text_row(editor.registry.def(i));
            let next_text = items
                .get(k + 1)
                .is_some_and(|&n| is_text_row(editor.registry.def(n)));
            set_row_spacing(
                ctx,
                if this_text || next_text {
                    TEXT_ROW_SPACING
                } else {
                    ROW_SPACING
                },
            );
            let top = next_row_top(ctx);
            draw_setting_row(fns, ctx, screen, editor, native, i, false, true);
            capture_tooltip(ctx, screen, editor.registry.def(i), top, next_row_top(ctx));
        }
    }
}

/// Search results across every category. Text settings are read-only here: Nuklear identifies a
/// text box by its position in the frame, so results changing under the search box would take its
/// focus.
fn draw_search_pane(
    fns: &Fns,
    ctx: *mut c_void,
    screen: &mut Screen,
    editor: &mut Editor,
    native: &mut dyn Native,
) {
    let query = buffer_text(&screen.search);
    let shown = |d: &SettingDef| fns.localize(&d.label);
    let hits = editor.registry.search(&query, &shown);
    for &i in &hits {
        set_row_spacing(ctx, ROW_SPACING);
        let top = next_row_top(ctx);
        draw_setting_row(fns, ctx, screen, editor, native, i, true, false);
        capture_tooltip(ctx, screen, editor.registry.def(i), top, next_row_top(ctx));
    }
    if hits.is_empty() {
        row(fns, ctx, ROW_HEIGHT, 1);
        let text = if query.is_empty() {
            "Type to search all settings."
        } else {
            "No settings match."
        };
        label(fns, ctx, text, ALIGN_ROW);
    }
}

fn buffer_text(buf: &[u8]) -> String {
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end]).into_owned()
}

/// One setting: a caption and its control. `show_category` prefixes the category, for search
/// results.
#[allow(clippy::too_many_arguments)]
fn draw_setting_row(
    fns: &Fns,
    ctx: *mut c_void,
    screen: &mut Screen,
    editor: &mut Editor,
    native: &mut dyn Native,
    i: usize,
    show_category: bool,
    allow_text_edit: bool,
) {
    let d = editor.registry.def(i).clone();
    let shown = fns.localize(&d.label);
    let caption = if show_category {
        format!("{} / {shown}", d.category)
    } else {
        shown
    };
    let inactive = editor.registry.inactive(i, native);
    if inactive {
        unsafe { (fns.disable_begin)(ctx) };
    }
    // A locked checkbox: greyed, showing the value it is locked to, and a click
    // on it changes nothing.
    let locked = editor.registry.locked_value(i, native);
    let value = match locked {
        Some(v) => Value::Int(v),
        None => editor.get(i, native),
    };
    match d.kind {
        Kind::Choice if !d.choices.is_empty() => {
            row(fns, ctx, ROW_HEIGHT, 2);
            label(fns, ctx, &caption, ALIGN_ROW);
            let current = d.choice_index(value.int());
            let panel = current_panel(ctx);
            let combo_w = if panel.is_null() {
                600.0
            } else {
                read_f32(panel, PANEL_BOUNDS_W) * 0.5
            };
            let (item_h, combo_h) = combo_layout(d.choices.len());
            let header = fns.localize(&d.choices[current]);
            if combo_begin(fns, ctx, screen, i, &header, combo_w, combo_h) {
                // No spacing between the popup's items. Set before the first row.
                let pane_spacing = read_f32(ctx, CTX_SPACING_Y);
                set_row_spacing(ctx, 0.0);
                for (o, choice) in d.choices.iter().enumerate() {
                    row(fns, ctx, item_h, 1);
                    let t = cstr(&fns.localize(choice));
                    if unsafe { (fns.combo_item)(ctx, t.as_ptr().cast(), ALIGN_ROW) } != 0 {
                        editor.set(i, Value::Int(d.choice_value(o)), native);
                        unsafe { (fns.combo_close)(ctx) };
                    }
                }
                unsafe { (fns.combo_end)(ctx) };
                set_row_spacing(ctx, pane_spacing);
            }
        }
        Kind::Choice | Kind::Bool => {
            row(fns, ctx, ROW_HEIGHT, 1);
            let t = cstr(&caption);
            let mut flags: u32 = u32::from(value.int() != 0);
            if unsafe { (fns.checkbox)(t.as_ptr().cast(), &mut flags, 1) } && locked.is_none() {
                editor.set(i, Value::Int(i64::from(flags & 1 != 0)), native);
            }
        }
        Kind::Int => {
            let slider = d.is_slider();
            row(fns, ctx, ROW_HEIGHT, if slider { 2 } else { 1 });
            label(fns, ctx, &format!("{caption}: {}", value.int()), ALIGN_ROW);
            if slider {
                // The game's slider is unsigned: shift the range so min maps to 0.
                let mut v = (value.int() - d.min) as u32;
                let mut lo = 0u32;
                let mut hi = (d.max - d.min) as u32;
                let mut step = 1u32;
                if unsafe { (fns.slider)(&mut v, &mut lo, &mut hi, &mut step) } {
                    editor.set(i, Value::Int(i64::from(v) + d.min), native);
                }
            }
        }
        Kind::Color => {
            row(fns, ctx, SEARCH_ROW_HEIGHT, 3);
            label(fns, ctx, &caption, ALIGN_ROW);
            let hex = value.str().to_owned();
            if let Some([x, y, w, h]) = next_cell(fns, ctx) {
                let rgb = color::parse_hex(&hex).unwrap_or(Rgb { r: 0, g: 0, b: 0 });
                let inset = h * 0.15;
                let side = (h - inset * 2.0).min(w - SWATCH_ROW_GAP);
                let swatch = [x + w - side - SWATCH_ROW_GAP, y + inset, side, side];
                fill(ctx, fns, swatch, color::nk_color(rgb));
                outline(ctx, fns, swatch, 2.0, BLACK);
            }
            unsafe { (fns.skip_cell)(ctx) };
            if button(fns, ctx, &format!("#{hex}")) {
                open_color_editor(screen, i, &hex, &d.default_str);
            }
        }
        Kind::Str if !allow_text_edit => {
            row(fns, ctx, ROW_HEIGHT, 1);
            label(fns, ctx, &format!("{caption}: {}", value.str()), ALIGN_ROW);
        }
        Kind::Str => {
            row(fns, ctx, SEARCH_ROW_HEIGHT, 2);
            label(fns, ctx, &caption, ALIGN_ROW);
            let slot = match screen.edits.iter().position(|b| b.index == i) {
                Some(p) => p,
                None => {
                    let mut buf = vec![0u8; EDIT_BUFFER];
                    let bytes = value.str().as_bytes();
                    let n = bytes.len().min(EDIT_BUFFER - 1);
                    buf[..n].copy_from_slice(&bytes[..n]);
                    screen.edits.push(TextBox {
                        index: i,
                        buf,
                        focused: false,
                    });
                    screen.edits.len() - 1
                }
            };
            let tb = &mut screen.edits[slot];
            // The box keeps its own copy of the text. While it is not being typed in, follow the
            // value, so a change from elsewhere shows: a plugin filling it in during Apply, or
            // Cancel putting the old text back.
            if !tb.focused && buffer_text(&tb.buf) != value.str() {
                tb.buf.fill(0);
                let bytes = value.str().as_bytes();
                let n = bytes.len().min(EDIT_BUFFER - 1);
                tb.buf[..n].copy_from_slice(&bytes[..n]);
            }
            let typing = text_edit(fns, ctx, &mut tb.buf);
            // Stored when the box loses focus, so the value never holds half-typed text, and only
            // after it had focus, so merely showing a value never rewrites it.
            if tb.focused && !typing {
                let text = buffer_text(&tb.buf);
                if text != value.str() {
                    editor.set(i, Value::Str(text), native);
                }
            }
            tb.focused = typing;
        }
    }
    if inactive {
        unsafe { (fns.disable_end)(ctx) };
    }
}

/// The game's text box over `buf`. Pushing the edit skin resets the row spacing, which would
/// change the box's height, so the spacing is restored after the push and the pop. True while
/// the box has focus.
fn text_edit(fns: &Fns, ctx: *mut c_void, buf: &mut [u8]) -> bool {
    let spacing = read_f32(ctx, CTX_SPACING_Y);
    let prev = fns.push_skin(unsafe { *fns.skin_text_edit });
    set_row_spacing(ctx, spacing);
    let state = unsafe {
        (fns.text_edit)(
            ctx,
            TEXT_EDIT_FLAGS,
            buf.as_mut_ptr().cast(),
            buf.len() as i32,
            fns.text_edit_filter,
        )
    };
    fns.push_skin(prev);
    set_row_spacing(ctx, spacing);
    state & 1 != 0
}

/// Opens a dropdown header. Calls the combo function directly rather than the game's wrapper,
/// so the row spacing can be restored after the skin push.
#[allow(clippy::too_many_arguments)]
fn combo_begin(
    fns: &Fns,
    ctx: *mut c_void,
    screen: &mut Screen,
    i: usize,
    text: &str,
    w: f32,
    h: f32,
) -> bool {
    if screen.combo_open.len() <= i {
        screen.combo_open.resize(i + 1, 0);
    }
    let spacing = read_f32(ctx, CTX_SPACING_Y);
    let prev = fns.push_skin(fns.combo_skin(screen.combo_open[i] != 0));
    set_row_spacing(ctx, spacing);
    let t = cstr(text);
    let opened = unsafe { (fns.combo_begin)(ctx, t.as_ptr().cast(), ALIGN_ROW, w, h) } != 0;
    fns.push_skin(prev);
    set_row_spacing(ctx, spacing);
    screen.combo_open[i] = u8::from(opened);
    if opened {
        screen.combos_this_frame += 1;
    }
    opened
}

/// Records the tooltip if the mouse is over the setting just drawn, between `y0` and `y1` in
/// layout space. The group's scroll offset is subtracted to compare them with the mouse.
fn capture_tooltip(ctx: *mut c_void, screen: &mut Screen, d: &SettingDef, y0: f32, y1: f32) {
    let panel = current_panel(ctx);
    if panel.is_null() {
        return;
    }
    let offset_ptr = unsafe {
        panel
            .cast::<u8>()
            .add(PANEL_OFFSET_Y)
            .cast::<*const u32>()
            .read_unaligned()
    };
    let scroll = if offset_ptr.is_null() {
        0.0
    } else {
        (unsafe { offset_ptr.read_unaligned() }) as f32
    };
    let (y0, y1) = (y0 - scroll, y1 - scroll);
    let mx = read_f32(ctx, CTX_MOUSE_X);
    let my = read_f32(ctx, CTX_MOUSE_Y);
    let x = read_f32(panel, PANEL_BOUNDS_X);
    let w = read_f32(panel, PANEL_BOUNDS_W);
    if mx < x || mx >= x + w || my < y0 || my >= y1 {
        return;
    }
    // Ignore the part of a row that is scrolled out of view.
    let (cx, cy) = (read_f32(panel, PANEL_CLIP_X), read_f32(panel, PANEL_CLIP_Y));
    let (cw, ch) = (read_f32(panel, PANEL_CLIP_W), read_f32(panel, PANEL_CLIP_H));
    if mx < cx || mx >= cx + cw || my < cy || my >= cy + ch {
        return;
    }
    screen.tip = Some(Tip {
        description: d.description.clone(),
        source: d.source_name(),
        mouse_x: mx,
        mouse_y: my,
    });
}

fn font_line_height(ctx: *mut c_void) -> f32 {
    let font = unsafe {
        ctx.cast::<u8>()
            .add(CTX_FONT)
            .cast::<*const u8>()
            .read_unaligned()
    };
    if font.is_null() {
        return TIP_FALLBACK_LINE_HEIGHT;
    }
    let h = unsafe { font.add(USER_FONT_HEIGHT).cast::<f32>().read_unaligned() };
    if h > 1.0 && h < 500.0 {
        h
    } else {
        TIP_FALLBACK_LINE_HEIGHT
    }
}

/// Draws the captured tooltip: a bordered box sized with the game's line counter, and the
/// wrapped text.
fn draw_tooltip(fns: &Fns, ctx: *mut c_void, ui_w: f32, screen: &Screen) {
    let Some(tip) = &screen.tip else { return };
    // An open dropdown's popup would cover it.
    if screen.combos_this_frame > 0 {
        return;
    }
    let win = current_window(ctx);
    if win.is_null() {
        return;
    }
    let canvas = unsafe { win.cast::<u8>().add(WINDOW_BUFFER).cast::<c_void>() };
    let text = cstr(&tip.description);
    let source = cstr(&tip.source);
    let line_h = font_line_height(ctx);
    let wrap_w = TIP_MAX_WIDTH - TIP_PAD * 2.0;
    let lines = if tip.description.is_empty() {
        0
    } else {
        unsafe { (fns.count_lines)(ctx, text.as_ptr().cast(), (text.len() - 1) as i32, wrap_w) }
            .clamp(1, TIP_MAX_LINES)
    };
    let text_h = lines as f32 * line_h;
    let source_gap = if lines > 0 { TIP_SOURCE_GAP } else { 0.0 };
    let tip_w = TIP_MAX_WIDTH;
    let tip_h = text_h + source_gap + line_h + TIP_PAD * 2.0;
    // Below and right of the cursor, moved back on screen if it would not fit.
    let mut x = tip.mouse_x + TIP_CURSOR_OFFSET;
    let mut y = tip.mouse_y + TIP_CURSOR_OFFSET;
    if x + tip_w > ui_w {
        x = tip.mouse_x - TIP_CURSOR_OFFSET - tip_w;
    }
    if y + tip_h > UI_HEIGHT {
        y = tip.mouse_y - TIP_CURSOR_OFFSET - tip_h;
    }
    let (x, y) = (x.max(0.0), y.max(0.0));
    unsafe {
        (fns.fill_rect)(canvas, x, y, tip_w, tip_h, TIP_ROUNDING, TIP_BORDER_COLOUR);
        (fns.fill_rect)(
            canvas,
            x + TIP_BORDER,
            y + TIP_BORDER,
            tip_w - TIP_BORDER * 2.0,
            tip_h - TIP_BORDER * 2.0,
            TIP_ROUNDING,
            TIP_FILL,
        );
    }
    let mut colour = unsafe {
        ctx.cast::<u8>()
            .add(CTX_TEXT_COLOUR)
            .cast::<u32>()
            .read_unaligned()
    };
    if colour >> 24 == 0 {
        colour = TIP_TEXT_FALLBACK;
    }
    if lines > 0 {
        rect(fns, ctx, x + TIP_PAD, y + TIP_PAD, wrap_w, text_h);
        unsafe { (fns.wrapped_label)(ctx, text.as_ptr().cast(), colour) };
    }
    rect(
        fns,
        ctx,
        x + TIP_PAD,
        y + TIP_PAD + text_h + source_gap,
        wrap_w,
        line_h,
    );
    unsafe { (fns.wrapped_label)(ctx, source.as_ptr().cast(), TIP_SOURCE_COLOUR) };
}

/// The pause-menu variant's backdrop, from the title down through the footer.
fn draw_backdrop(fns: &Fns, ctx: *mut c_void, panel_x: f32, panel_w: f32, footer_y: f32) {
    let win = current_window(ctx);
    if win.is_null() {
        return;
    }
    let canvas = unsafe { win.cast::<u8>().add(WINDOW_BUFFER).cast::<c_void>() };
    let x = panel_x - BACKDROP_PAD;
    let y = TITLE_TOP - BACKDROP_PAD;
    let w = panel_w + BACKDROP_PAD * 2.0;
    let h = (footer_y + FOOTER_HEIGHT + BACKDROP_PAD) - y;
    unsafe {
        (fns.fill_rect)(canvas, x, y, w, h, 0.0, BACKDROP_BORDER);
        (fns.fill_rect)(
            canvas,
            x + BACKDROP_BORDER_W,
            y + BACKDROP_BORDER_W,
            w - BACKDROP_BORDER_W * 2.0,
            h - BACKDROP_BORDER_W * 2.0,
            0.0,
            BACKDROP,
        );
    }
}

/// Cancel, Apply, Save and Exit. True when the screen should close.
fn draw_footer(
    fns: &Fns,
    ctx: *mut c_void,
    height: f32,
    editor: &mut Editor,
    native: &mut dyn Native,
    owners: &mut dyn Owners,
) -> bool {
    row(fns, ctx, height, 3);
    if button(fns, ctx, "Cancel") {
        editor.cancel(native, owners);
        return true;
    }
    // Apply is disabled until something changes.
    let dirty = editor.dirty();
    if !dirty {
        unsafe { (fns.disable_begin)(ctx) };
    }
    if button(fns, ctx, "Apply") && dirty {
        editor.apply(native, owners);
    }
    if !dirty {
        unsafe { (fns.disable_end)(ctx) };
    }
    if button(fns, ctx, "Save and Exit") {
        editor.apply(native, owners);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn long_dropdowns_show_every_entry() {
        // Short lists keep full-height rows and a popup just big enough.
        assert_eq!(
            combo_layout(3),
            (COMBO_ITEM_HEIGHT, 3.0 * COMBO_ITEM_HEIGHT + COMBO_POPUP_PAD)
        );
        // Eleven entries used to be cut off by the old 900 cap; now they all fit at full height.
        let (item, h) = combo_layout(11);
        assert_eq!(item, COMBO_ITEM_HEIGHT);
        assert!(h <= COMBO_POPUP_MAX_HEIGHT);
        // The most a choice may have: shorter rows, still every one inside the popup.
        let (item, h) = combo_layout(16);
        assert!(item < COMBO_ITEM_HEIGHT && item >= COMBO_ITEM_MIN_HEIGHT);
        assert!(16.0 * item + COMBO_POPUP_PAD <= COMBO_POPUP_MAX_HEIGHT + 0.01);
        assert_eq!(h, 16.0 * item + COMBO_POPUP_PAD);
        // Rows are whole units for every length.
        for n in 1..=16 {
            assert_eq!(combo_layout(n).0.fract(), 0.0, "{n} entries");
        }
        // No entries is not a division by zero.
        assert!(combo_layout(0).0.is_finite());
    }

    fn within(inner: Area, outer: Area) -> bool {
        inner[0] >= outer[0]
            && inner[1] >= outer[1]
            && inner[0] + inner[2] <= outer[0] + outer[2]
            && inner[1] + inner[3] <= outer[1] + outer[3]
    }

    #[test]
    fn the_color_popup_fits_its_padded_frame_and_the_screen() {
        for ui_w in [2880.0, 3840.0, 5120.0] {
            let l = PickerLayout::centred(ui_w);
            let f = l.frame;
            let content = [
                f[0] + POPUP_PAD_X,
                f[1] + POPUP_PAD_Y,
                f[2] - POPUP_PAD_X * 2.0,
                f[3] - POPUP_PAD_Y * 2.0,
            ];
            assert!(within(f, [0.0, 0.0, ui_w, UI_HEIGHT]));
            let parts = [
                l.title,
                l.square,
                l.hue,
                l.swatch_labels[0],
                l.swatch_labels[1],
                l.swatches[0],
                l.swatches[1],
                l.hex_label,
                l.hex_box,
                l.reset,
                l.cancel,
                l.ok,
            ];
            for p in parts {
                assert!(within(p, content), "{p:?} outside {content:?}");
            }
            // The right column ends at the content's edge: no dead space beside it.
            assert_eq!(l.reset[0] + l.reset[2], content[0] + content[2]);
            assert!(l.reset[1] + l.reset[3] <= l.square[1] + l.square[3]);
            // The buttons are centred.
            let left = l.cancel[0] - content[0];
            let right = content[0] + content[2] - (l.ok[0] + l.ok[2]);
            assert!((left - right).abs() < 0.01);
        }
    }
}
