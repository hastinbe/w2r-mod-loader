# Plugin ABI (version 1)

The plugin interface is a C ABI so plugin authors can use any language. The header is
[`crates/w2r-abi/include/w2r_abi.h`](../crates/w2r-abi/include/w2r_abi.h); this document gives the
rules and limits the header cannot express. The ABI is **add-only**.

Capabilities are a consent prompt and a guardrail, **not a sandbox**. A native plugin runs inside
the game process and can do anything that process can. The host enforces the limits below on the
functions it provides; it cannot stop a plugin from using the operating system directly.

## Conventions

- Calling convention `__cdecl` on 32-bit Windows (`W2R_CALL`). Only fixed-width C types cross the
  boundary; no C++ or Rust types.
- **Threading:** every host function and every plugin callback runs on the game's main thread. A
  host function called from any other thread returns `W2R_E_STATE`.
- Status codes (`W2rStatus`, `int32_t`): `0` is success, every failure is negative.

| Code | Meaning |
|---|---|
| `W2R_OK` 0 | success |
| `W2R_E_DENIED` -1 | the capability was not approved |
| `W2R_E_INVALID` -2 | bad argument, or a game-memory access failed |
| `W2R_E_LIMIT` -3 | a size or count limit was exceeded |
| `W2R_E_STATE` -4 | not allowed right now (wrong thread, not in a lobby, ...) |
| `W2R_E_UNSUPPORTED` -5 | the host knows the function but cannot do it |
| `W2R_E_NOT_FOUND` -6 | no such symbol, handle or event |
| `W2R_E_RATE` -7 | rate limit hit; the call had no effect |
| `W2R_E_CONFLICT` -8 | the hook overlaps an existing patch |
| `W2R_E_BUSY` -9 | not ready yet, ask again on a later frame |

## Loading and versioning

1. The host loads the DLL (after the checks in [load-policy.md](load-policy.md)) and calls its one
   export, `w2r_plugin_main(const W2rHost *host)`.
2. The plugin returns a pointer to a static `W2rPlugin` table. Null means "refuse to load".
3. The host calls `on_load(host)`. A non-zero return fails the load (`E_LOAD`).

`W2rHost`, `W2rPlugin`, `W2rUi` and `W2rEvent` begin with `struct_size`. Fields are only ever
appended; nothing is removed, reordered or resized.

- A plugin must check that `host->struct_size` covers a field before using it (and that the
  function pointer is non-null). A newer plugin on an older host sees a smaller `struct_size`.
- The host calls a plugin callback only if the plugin's `struct_size` covers it and it is non-null.
- A function the host provides but whose capability was not approved returns `W2R_E_DENIED`; the
  host logs this once per function per plugin.
- `W2R_ABI_VERSION` increases only for a change that cannot be made by appending. The manifest `abi`
  field is the minimum ABI the plugin needs; the host refuses a greater value (`E_ABI`).
- Each plugin receives its own `W2rHost` instance. Pass `host->token` back as the first argument of
  every call; it tells the host which plugin is calling.

## Host functions

Implicit capabilities need no approval: `log`, `settings`, `ui`, `fs.data`.

| Function | Capability | Rules |
|---|---|---|
| `log` | implicit | message at most 512 bytes; throttled to 20 lines/s per plugin with a burst of 40; excess lines are dropped and one summary line per second is logged |
| `settings_get_i64`, `settings_set_i64`, `settings_get_str`, `settings_set_str` | implicit | the plugin's own settings only; key 1-64 bytes of `[a-z0-9_.]`; at most 128 keys; string value at most 1,024 bytes; stored in `config\<plugin-id>.json`, at most 256 KiB. Statuses: a missing key is `W2R_E_NOT_FOUND`, a key holding the other type is `W2R_E_INVALID`, a bad key `W2R_E_INVALID` (too long `W2R_E_LIMIT`), the 129th key or an oversized value `W2R_E_LIMIT`, a failed save `W2R_E_STATE` (and the old value stays). `settings_get_str` copies the value with no terminator and always sets `len_out` to the full length; a value longer than `cap` leaves `buf` untouched and returns `W2R_E_LIMIT`. Changes are written to disk immediately |
| `data_open`, `data_read`, `data_write`, `data_close` | implicit (`fs.data`) | flat files in the plugin's own `data\` folder; name 1-64 bytes of `[A-Za-z0-9_.-]`, not starting with `.`, not a device name; at most 64 files, 16 MiB in total, 8 open handles; at most 65,536 bytes per read or write call; the plugin never sees a path. `flags` is exactly `W2R_FILE_READ` (the file must exist, `W2R_E_NOT_FOUND` otherwise; read from the start) or `W2R_FILE_WRITE` (create or truncate; writes append from there); a handle cannot do the other (`W2R_E_STATE`), anything else is `W2R_E_INVALID`. Names ending in `.` are refused too (Windows drops the dot). A link or non-regular file in the folder is never followed. `data_read` sets `read_out` to 0 at end of file. Without a host root (tests) all of these report `W2R_E_STATE` |
| `subscribe`, `unsubscribe` | per event | see Events; unknown ids return `W2R_E_NOT_FOUND` |
| `symbol_lookup`, `symbol_read` | `game.read` | symbol name at most 64 bytes; the handle is valid for the session; at most 4,096 bytes per read; a read that faults returns `W2R_E_INVALID` and never crashes the game |
| `chat_send` | `chat.write` | at most 200 bytes of printable ASCII; at most 4 per 5 s per plugin; only when the game's chat is available |
| `peer_send` | `net.send` | payload 1 to 960 bytes (`W2R_E_INVALID` empty, `W2R_E_LIMIT` over); at most 32 messages/s per plugin and 128/s overall (`W2R_E_RATE`); lobby only, with the session connected (`W2R_E_STATE`); the target slot (0-7, else `W2R_E_INVALID`) must have announced the handshake, or `W2R_SLOT_ALL` for every such peer; no announced peer is `W2R_E_STATE` |
| `symbol_address` | `hooks.raw` (shown as **dangerous**) | the runtime address (image base plus the symbol's RVA) of any symbol, function or global, always inside the game module; `W2R_E_NOT_FOUND` for a bad handle; the handle still comes from `symbol_lookup`, which needs `game.read`, so a plugin that calls game functions or writes game memory declares both. The address makes every later access the plugin's own, outside the host's guard |
| `hook_install`, `hook_remove` | `hooks.raw` (shown as **dangerous**) | `target` is a `symbol_lookup` handle of a function symbol (`W2R_E_NOT_FOUND` for a bad handle, `W2R_E_INVALID` for a global), so a hook is always inside the game module's code; `original_out` receives the trampoline before the hook goes live; no overlap with any host patch or other hook (`W2R_E_CONFLICT`); at most 64 hooks per plugin |

The UI (`W2rUi`) is passed to `on_ui` and is valid only inside that call. It offers `label`,
`button`, `checkbox` and `slider_int`; each call draws one row in the plugin's card in the plugin
manager (opened from the title screen). A plugin's settings belong in its declarations instead (next section), so
they appear in the options screen's categories with everyone else's; `on_ui` remains for what a
declaration cannot express. Strings are at most 128 bytes of UTF-8;
a longer or invalid string, a null value pointer or `min >= max` makes the call a no-op that
reports 0. `button` returns 1 when pressed this frame; `checkbox` and `slider_int` return 1 when
they changed the value this frame (the slider clamps it to the range first). `ctx` is opaque.

Two calls were appended after the first release (`struct_size` grew; a plugin checks
`ui->struct_size >= W2R_UI_TEXT_EDIT_SIZE` before using them, so it still runs on an older host):

- `text_edit(ctx, text, buf, cap)`: a label and an edit box over the plugin's own NUL-terminated
  buffer of `cap` bytes (1 to 256; otherwise a no-op that reports 0). The game's edit widget
  changes the buffer in place while the player types; the call returns 1 on a frame where the text
  changed. The plugin keeps the buffer between frames and passes it again. A buffer with no NUL is
  terminated at its last byte before the game sees it. The text is whatever the player types (the
  game's edit filter applies); the plugin must validate it.
- `set_active(ctx, active)`: 0 greys out and disables the rows that follow, 1 restores them. A
  grey-out left open at the end of `on_ui` is closed by the host.

One more was appended after those (`ui->struct_size >= W2R_UI_COLOR_EDIT_SIZE`; on an older host a
plugin falls back to `text_edit`):

- `color_edit(ctx, text, hex)`: a color row like the options screen's: the caption, a swatch, and
  the value on a `#rrggbb` button that opens the host's color editor (the same one the options
  screen uses: saturation / brightness square, hue strip, the new and the current color, a hex box,
  OK / Cancel; no Reset to Default, since a plugin card has no declared default). `hex` is the
  plugin's own buffer of `W2R_COLOR_HEX_BYTES` (7) bytes, six hex digits and a NUL; anything else
  shows as black. The call returns 1 on the frame after the player pressed OK on a different color,
  with `hex` rewritten as six lowercase digits and a NUL; Cancel changes nothing. The row is known
  by the plugin and its caption, so captions of color rows should differ. While the editor is open
  the plugin manager behind it is read-only; opening the manager again closes an editor left open.

And one after `color_edit` (`ui->struct_size >= W2R_UI_COMBO_SIZE`):

- `combo(ctx, text, choices, count, selected)`: a dropdown row like the options screen's: the
  caption, then a dropdown showing `choices[*selected]`, the game's own dropdown widget. `count` is
  2 to `W2R_MAX_SETTING_CHOICE_COUNT` (16) and each choice at most `W2R_MAX_SETTING_CHOICE` (64)
  bytes; otherwise, or with a null pointer, a no-op that reports 0. Unlike a declared dropdown the
  list is the plugin's to change every frame. A `*selected` out of range shows the last entry. The
  call returns 1 on the frame the player picked a different entry, with `*selected` set to it.
  The open dropdown's rows shrink to fit, as on the options screen.

## Declared settings

Appended after `create_map_folder` (`host->struct_size >= W2R_HOST_SETTINGS_DECLARE_SIZE`; an
older host has a smaller table). Implicit, no capability. The options screen
([settings-screen.md](settings-screen.md)) shows every setting the game, the host and the plugins
have in one place: categories down the left, the selected category's settings on the right grouped
under subcategory headings, a search page, and the description on hover. A plugin puts its own
settings there by declaring them.

`settings_declare(token, def)` declares one setting. It is accepted only while the plugin's
`on_load` runs (`W2R_E_STATE` after), and every string in `def` is copied during the call.

| Field | Rule |
|---|---|
| `struct_size` | at least `W2R_SETTING_DEF_V1_SIZE` (`W2R_E_INVALID` otherwise) |
| `reserved` | 0 (`W2R_E_INVALID` otherwise) |
| `kind` | `W2R_SETTING_BOOL` (a checkbox, stored with `settings_set_i64` as 0 or 1), `_INT` (a slider when `min < max`, a read-only row otherwise), `_CHOICE` (a dropdown, stored as the chosen index), `_STRING` (a text box, `settings_set_str`) or `_COLOR` (a color picker, stored with `settings_set_str` as six lowercase hex digits, `rrggbb`); anything else `W2R_E_INVALID` |
| `flags` | 0, or `W2R_SETTING_HIDDEN`: stored and readable as usual, never shown; other bits `W2R_E_INVALID` |
| `key` | the settings-key rule above; declared once per plugin (`W2R_E_CONFLICT` the second time); at most 128 declarations, the same cap as stored keys |
| `label` | 1 to 64 bytes. Title Case, like the rest of the screen |
| `description` | at most 256 bytes; may be empty. The hover text. The tooltip's last line names the plugin (its manifest `name`) |
| `category`, `subcategory` | at most 32 bytes each. Several plugins may name the same category, which is the point: related settings end up together. An empty category is the plugin's name; an empty subcategory is "General" |
| `default_int` | 64-bit, like the stored value. 0 or 1 for a bool, a value in `[min, max]` for a slider, an index into `choices` for a choice (`W2R_E_INVALID` otherwise) |
| `min`, `max` | 64-bit; `_INT` only; `min > max` is `W2R_E_INVALID` |
| `default_str` | `_STRING`: at most 1,024 bytes. `_COLOR`: six hex digits without `#` (`W2R_E_INVALID` otherwise) |
| `choices`, `choice_count` | `_CHOICE` only: 2 to 16 labels of 1 to 64 bytes (`W2R_E_LIMIT` above 16, `W2R_E_INVALID` below 2) |
| `active_when` | empty, or the key of a `_BOOL` this plugin declared earlier, or `host:<key>` for one of the host's own bools (for example `host:enhanced_create_screen`). The row is greyed while that checkbox is off. An unknown key is `W2R_E_NOT_FOUND`, a non-bool `W2R_E_INVALID` |

The host reads only the fields the kind uses, so the others may be left uninitialised; the
field order keeps every offset the same under any 32-bit compiler. Every text field is printable
ASCII (the screen draws through the game's font atlas, which has no other glyphs): anything else
is `W2R_E_INVALID`, and an over-long field `W2R_E_LIMIT`. A refused declaration is logged with
its key and status.

Values stay where they always were, in `config\<plugin-id>.json`, and the plugin reads them with
`settings_get_i64` / `settings_get_str` as before. A declared key that was never stored reads as
its declared default (instead of `W2R_E_NOT_FOUND`). The screen writes a plugin's values only
when the player presses Apply or Save and Exit, so a plugin never sees a half-made edit; then it
sends the plugin `W2R_EVT_SETTINGS_CHANGED`, once per apply, after all of that plugin's changed
values are stored. A plugin that subscribed to it re-reads its settings there. Values the plugin
stores itself with `settings_set_*` are what the screen shows the next time it opens.

## Create screen

Appended after the first release (`host->struct_size >= W2R_HOST_CREATE_UI_SIZE`, `W2R_HOST_CREATE_UI_SIZE` in the header; an older host has a smaller table). Implicit, no capability: the plugin
can only change which map the local player is about to host, the same as picking one in the map
browser, and `affects_simulation` stays the plugin's own declaration.

- `create_ui_register(token, callback)` registers the plugin's one create-screen callback (null
  removes it; registering again replaces it). While the enhanced Create Multiplayer Game screen
  draws, the host calls it every frame with a `W2rUi`, under the same guard as `on_ui`. The rows
  appear under the map preview and the Change map button, in a strip reserved for them (room for a
  slider with its caption and one button; rows that do not fit are not drawn). `label`, `button`
  and `slider_int` work; `checkbox` shows its text and never changes; `text_edit` does nothing.
  Nothing is called, and no room is reserved, when no plugin registered one or when the stock
  create screen is in use.
- `create_map_count(token, players, count_out)` and `create_map_select(token, players, nth)` work
  only inside that callback (`W2R_E_STATE` elsewhere). They see the host's map index, the one the
  map browser's search uses: every map under the maps folder, up to the browser's caps. `players`
  is the map's maximum player count, 1 to 8, or 0 for any (`W2R_E_INVALID` above 8). The index is
  read from its cache or built a slice per frame the first time a plugin calls `create_map_count`;
  until it is ready the call returns `W2R_E_BUSY` and the plugin asks again next frame. Maps keep
  their index order while the index is not rebuilt (reload in the browser rebuilds it). `nth` is
  0-based and below the last count (`W2R_E_NOT_FOUND` otherwise, also for a map that vanished since
  indexing). A selection is exactly what Confirm in the map browser does: the game's map list is
  pointed at the map's folder and the map is selected for the game being created.
- `create_map_folder(token, folder)` (appended again; `host->struct_size >=
  W2R_HOST_MAP_FOLDER_SIZE`) limits `create_map_count` and `create_map_select` to the maps in
  `folder` and its subfolders. `folder` is relative to the maps folder (`\` or `/`, case-insensitive,
  whole folder names; empty removes the limit). The limit ends when the callback returns, so call it
  first, every frame. `W2R_E_STATE` outside the callback; `W2R_E_INVALID` for `..`, a drive colon or
  more than 200 bytes. A folder with no maps gives a count of 0.

## Drawing over the map

A plugin may export `w2r_map_draw` (`W2rMapDrawFn`, looked up with `GetProcAddress` when it
loads). The host then calls it every frame of a match or replay with a `W2rMapDraw` table, inside a
window that covers the map view: anything drawn is clipped to the map view and sits under the HUD
(the resource bar's text and the panels stay on top, as they do over the game's own unit bars).
It runs under the host's crash guard: a fault disables the plugin, as for its other callbacks.

| Field | Notes |
|---|---|
| `origin_x`, `origin_y` | the screen point of world pixel (0, 0): world (x, y) is on screen at `(origin_x + x, origin_y + y)` |
| `view_x`, `view_y`, `view_w`, `view_h` | the map view on screen |
| `fill_rect(ctx, x, y, w, h, colour)` | a filled rectangle in screen coordinates |
| `text(ctx, x, y, w, h, text, colour)` | `text` centred in the rectangle, in the HUD's font |

Colours are packed `0xAABBGGRR`, alpha honoured. The table is valid only during the call. A plugin
without the export costs nothing; a plugin that has it is called only while the map view is up.

## Strings and untrusted data

- `W2rStr` is UTF-8 and length-delimited, not NUL-terminated.
- Player names and chat text from other players are reduced to printable ASCII before the plugin
  sees them (anything else becomes `?`), as in wc2r-plus.
- Plugins never receive raw peer buffers except the bounded payload of `W2rEvtPeerMessage`, and
  never receive filesystem paths.
- Lifetime: pointers the host passes in an event or UI call are valid only during that call. The
  host copies plugin-provided data that it keeps.

## Events

Delivered to `on_event` on the main thread, only to plugins that subscribed. Peer messages are
delivered only while the local player is in a lobby, and a net slot that sends more than 128 W2R
messages per second has the excess dropped before any plugin sees them.

| Event | Payload | Capability | Fires |
|---|---|---|---|
| `W2R_EVT_LOBBY_ENTER` 1 | none | none | the local player enters a lobby |
| `W2R_EVT_LOBBY_LEAVE` 2 | none | none | the local player leaves a lobby |
| `W2R_EVT_LOBBY_PLAYER_JOIN` 3 | `W2rEvtPlayer` | none; `name` is empty without `chat.read` | another player joins |
| `W2R_EVT_LOBBY_PLAYER_LEAVE` 4 | `W2rEvtPlayer` | same | another player leaves |
| `W2R_EVT_CHAT_RECEIVED` 5 | `W2rEvtChat` | `chat.read` | a chat line arrives |
| `W2R_EVT_PEER_MESSAGE` 6 | `W2rEvtPeerMessage` | `net.send` | a plugin message with this plugin's tag arrives |
| `W2R_EVT_MATCH_START` 7 | none | none | a match begins (the game resets its map-message slots, as wc2r-plus detects it); delivered before the game's own reset runs |
| `W2R_EVT_MATCH_END` 8 | none | none | the local player's `PlayerMatchResult` became non-zero (victory, defeat or leaving), once per match; fired from a host hook on `CheckMatchEnd` after the game's own function ran |
| `W2R_EVT_SETTINGS_CHANGED` 9 | none | none | the options screen stored new values for this plugin's declared settings (Apply or Save and Exit), once per apply |
| `W2R_EVT_SHUTDOWN` 10 | none | none | the game is exiting |

`subscribe` for an event whose capability is not approved returns `W2R_E_DENIED`. New events are
only added, with new ids.

## Peer messages

Plugins send through `peer_send`; the host builds the envelope:

```
[0]      0xE7            message type (unused by the base game)
[1..3]   'W' '2' 'R'     magic
[4]      'P'             subsystem: plugin message
[5..8]   tag             first 4 bytes of SHA-256 of the plugin id (UTF-8)
[9..]    payload         at most 960 bytes
```

- The tag is the same on every machine, so peers agree without negotiating. The host drops a
  message whose tag matches no loaded plugin. Two plugins with the same tag cannot both load
  (`E_TAG`).
- A plugin receives only messages carrying its own tag, only from peers that announced the
  handshake, and only while in a lobby. The sender slot is the one the transport reports, never one
  named inside a message.
- Subsystems `'H'`, `'M'` and `'L'` belong to the first-party handshake, map-sharing and
  lobby-settings features, which are compiled into the host rather than shipped as plugins: the
  handshake's roster is the host's own policy input and its fingerprint covers what the host
  loaded, so no plugin could provide it ([peer-protocol.md](peer-protocol.md)). `'P'` is the only
  subsystem third-party plugins use.
- A whole message is at most 969 bytes, below the game's 989-byte limit for its reliable channel.
  Messages are removed from the stream before the game sees them, and the host never sends
  outside a lobby.

## Crash and unload contract

- Every call into a plugin runs under a host guard that contains panics and access violations. A
  plugin that faults is disabled for the rest of the session, and the reason is logged. The
  mechanism (settled by the Phase 2 spike, `crates/w2r-core/src/guard.rs`): a structured
  exception handling frame (`__try`/`__except` via `microseh`) outermost, with
  `std::panic::catch_unwind` inside it. A caught fault abandons the Rust frames between the
  handler and the faulting instruction without running destructors, so the guard owns no state
  that needs cleanup. Stack overflow is not recoverable.
- **Hook detours are outside this guard.** A detour installed with `hook_install` is called by the
  game, on whatever thread the game uses, not by the host. The host cannot wrap it, and the
  main-thread rule does not apply to it: a detour must be thread-safe and must not call host
  functions from a thread other than the main thread. A fault inside a detour can crash the game.
  This is part of why `hooks.raw` is shown as dangerous.
- When a plugin is disabled after a fault in one of its callbacks, the host also removes all of
  its hooks, cancels its event subscriptions and stops calling `on_ui`.
- A plugin must not unwind across the boundary. The Rust SDK catches panics; other languages must
  do the equivalent.
- `on_unload` is best effort and may be skipped after a fault. The host never relies on it to keep
  its own state consistent.
- The host cannot preempt a slow callback. It logs callbacks over 50 ms, and after 10 in a row the
  plugin manager shows the plugin's status as slow.

## Minimal plugin

```c
#include "w2r_abi.h"

static W2rStatus W2R_CALL on_load(const W2rHost *host) {
    W2rStr msg = {"hello", 5};
    return host->log(host->token, W2R_LOG_INFO, msg);
}
static const W2rPlugin kPlugin = {sizeof(W2rPlugin), on_load, 0, 0, 0};

W2R_EXPORT const W2rPlugin *W2R_CALL w2r_plugin_main(const W2rHost *host) {
    return host->abi_version >= 1 ? &kPlugin : 0;
}
```

This is compiled as part of `crates/w2r-abi/tests/abi_layout.c` by `bash tools/check_abi.sh`, which
also checks struct layout as C11, C++17 and, when a 32-bit toolchain is installed, 32-bit.
