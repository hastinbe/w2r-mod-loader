# The options screen

Every setting the game, the host and the plugins have, in one screen. Ported from wc2r-plus
`core/settings/` (MIT), where it has shipped since 1.0.0, so it looks and behaves the same:

- **Categories** down the left, sorted alphabetically, then the **Search** page. The selected
  category is greyed ("you are here").
- The selected category's settings on the right, **grouped under subcategory headings** in the
  order they were first registered, so a plugin can add a setting to a subcategory that already
  exists.
- **Search** matches every shown setting's label (as displayed, so a translated game label is
  found in the player's language), its key and its category.
- **Hover** shows the setting's description in a box beside the cursor, with what the setting
  belongs to on a last line in a dimmer color: "Warcraft II: Remastered" for the game's own
  settings, "W2R Mod Loader" for the host's, and the plugin's name for a plugin's.
- **Cancel / Apply / Save and Exit** along the bottom. Apply is greyed until something changed.
  Cancel puts back everything changed since the screen opened (or since the last Apply), the
  game's own settings included.
- A row is **greyed**, not hidden, while the checkbox it depends on is off (`active_when`), or
  where the stock tabs grey it (the UI scale factor outside Fixed mode, the health-bar position
  when bars are never drawn).
- A **color** setting shows a swatch and a button with its hex value. The button opens the color
  editor in place of the settings pane: a saturation / brightness square with a hue strip beside
  it (click or drag in either), the new and the current color, a hex box, and OK / Cancel. OK
  puts the color in the setting; the screen's own Apply or Save and Exit then saves it, as for
  any other setting. The category list and the footer are greyed while the editor is open. The
  square and the strip are two small game textures; if the game cannot make them, the editor
  shows Hue, Saturation and Brightness sliders instead.

It replaces the game's options screen for both entries: the main menu's, and the pause menu's in a
match (on a backdrop of its own; leaving returns to the pause menu's root, which has the game's
"Return to game"). The game's own screen is still drawn if the screen's symbols are missing or the
session has not started.

Code: `src/settings.rs` (the model: registry, editor, declarations; tested everywhere),
`src/settingsscreen.rs` (drawing, x86), `src/nativesettings.rs` (the game's settings),
`src/hostsettings.rs` (the host's), `options.rs` (the hook, and the plugin manager below).

## Where the settings come from

| Owner | Where the values live | Written | Registered by |
|---|---|---|---|
| The game | its own globals, saved to `Warcraft2.ini` by the game's own writer | at once, and applied with the function the stock tab calls (Cancel puts it back) | `nativesettings.rs` from the game's settings table |
| The host | `config\host.json` (`share_match_data`: `sharing.json`) | on Apply or Save and Exit, then re-applied to the parts of the host that read it | `hostsettings.rs` |
| A plugin | `config\<plugin-id>.json`, read with `settings_get_*` | on Apply or Save and Exit, then `W2R_EVT_SETTINGS_CHANGED` | the plugin, with `settings_declare` in `on_load` ([abi.md](abi.md), "Declared settings") |

**The mod loader never becomes the system of record for a setting the base game owns.** A game
setting has no value of ours anywhere: reads and writes go to the game's global, and
`Warcraft2.ini` is written by the game's own writer, so removing the mod loader loses none of the
player's settings.

### The game's settings

`SettingsTable` (0x008cb498) is 39 entries of 24 bytes: type (string / uint / flag bit), ini key,
range, default and the address of the live global. Every entry is validated before any is trusted
(types, every pointer inside the game module, entry 0 is `speed` with max 8); a table that fails
leaves the game's categories out and logs why.

The catalog in `nativesettings.rs` places each key under a category that mirrors the stock tabs
(Sound, Speed, Preferences, Gameplay), with the game's own localization key as the label wherever
the stock menu shows the setting. A uint ranging 0..1 is a checkbox. Four keys are dropdowns over
named values (`screen_mode` lists Windowed first but stores 1 for it; `ui_scale_mode` lists
Classic / Automatic / Fixed and stores 2 / 0 / 1). Three stock options are one dropdown over two
flag bits each (health bars, resource bar alignment, portrait style); their bits stay registered
and hidden so the ini still round-trips them. Keys the stock menu never shows are registered and
hidden (the resolution pair, `mouse_size` on request, `name`, `tipnum`, the camera cluster pair,
the two DOS-era keys). An unknown key is shown under Advanced / Unsorted rather than dropped.
`using_classic` (flag bit 0x10, the graphics mode the next launch starts in, which F5 and the HUD
button also set) is shown as Preferences / Display / **Classic Graphics**; a change is passed
to the graphics hook (`graphicsmode.rs`), which, on the first frame of the next match (any kind,
with the options screen closed), switches the mode once if it differs, as F5 would. The
next launch starts in it too.

The appliers, exactly as the stock tabs call them: `music` (ApplyMusicVolume with the scalar
block), `remastered_music` (RestartMusic), `speed` (ApplyGameSpeed), `mouse_size`
(RebuildMouseCursors), `use_grid_keys` (ApplyGridKeys), `ui_unlock_aspect` (ApplyUiAspect), the
`ui_scale_*` pair (ApplyUiScale, with the ini-to-argument remap) and the `screen_mode` /
`display_monitor` pair (ApplyDisplayMode). Everything else is read where the game uses it. Cancel
re-applies only what changed: a blind restore restarted the soundtrack on every Cancel.

The monitor dropdown is rebuilt on each entry from the game's own display count.

### The host's settings

| Category / subcategory | Settings |
|---|---|
| UI / Enhanced UIs | Enhanced Create Multiplayer Game UI, Enhanced Multiplayer Lobby UI |
| UI / Fixes | Fix Overscroll |
| UI / APM Counter | Display APM In Game; Alert When APM Falls Below, Color APM Text Red When Low, Play Sound When APM Is Low (greyed while the display is off) |
| Multiplayer / Create Game | Auto-Name Games by Map |
| Multiplayer / Map Sharing | Enable Map Sharing, Enable Auto Downloading |
| Multiplayer / Replays | Save Replays |
| Multiplayer / In Game | Minimap Pings (Alt+G, Click), Show Connection Table In Game (Ctrl+Shift+N) |
| Multiplayer / Statistics | Contribute Match Data |

The lobby locks are set on the Create Multiplayer Game screen and are not here. **Fix Overscroll**
(`hide_scrollbar_arrows`, on by default) hides the arrow buttons of every vertical scrollbar in the
game, which removes the empty strip under scrolling lists; it is applied in a hook on
`nk_do_scrollbarv`, the one point every vertical scrollbar passes through.

## The plugin manager

Not a setting, so not on this screen: the title screen's version lines have a **Manage** button
after the plugin count, which opens a popup over the title screen: its own window, dimming the
screen, on a dark panel with a gold border like the in-match settings backdrop. The game's own
menu is not built while it is open, so nothing behind it can be clicked. It lists the session's
simulation fingerprint, then one card per installed plugin:

- The name, with the version against the right edge, and under it `Author: <author>` (the author
  in gold). The name, author and description come from the plugin's `plugin.json`; without one,
  the id alone.
- The status, with an **Enable** box against the right edge: `Status: Running` or
  `Status: Disabled`, followed in brackets by `slow callbacks` (after 10 slow callbacks in a row),
  `error` (it faulted or was refused at load), and `restart to update` (the box changed since
  launch). The box is `enabled` in `state.json`, which takes effect at the next launch.
- `Error: <reason>` when there is an error: the fault or the refusal, or that it was not loaded
  (`w2r.log` says why).
- `Description: <description>`, wrapped.
- Under them, any rows the plugin still draws itself through `on_ui`.

Close returns to the title screen; leaving the title screen closes it too.

## Style for declared settings

So that a plugin's settings read like the rest of the screen:

- **Title Case, American spelling** for labels, categories and subcategories ("Color", "Behavior").
- **Reuse a category** when the setting belongs with others ("Chat", "Multiplayer", "UI",
  "Accessibility", or the game's own "Gameplay"); a new category only for something that is
  genuinely its own.
- **One sentence of description** that says what the setting does, not how it is implemented.
- **Every default a named constant** in the plugin, passed to the declaration.
