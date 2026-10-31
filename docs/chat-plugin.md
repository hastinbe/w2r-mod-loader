# Chat plugin (Phase 3, step 2)

Status: spec, not implemented. Port of wc2r-plus `src/features/chat.cpp` (1,104 lines) as a
first-party plugin over the public ABI ([abi.md](abi.md)). Decision recorded in
[PLAN.md](PLAN.md), Phase 3: chat is a plugin (approach A: raw hooks plus the game's own
Nuklear functions), not a host feature.

## Parity rule

wc2r-plus's `chat.cpp` is the reference implementation, and it is proven in the game. The plugin
is a port, not a redesign: the same hooks, the same order of operations inside each hook, the
same constants (colours, channel labels, scan codes, slot indices, strides, Nuklear offsets), the
same setting keys, defaults and ranges, the same log file name and line format. Where this
document and the C++ differ, the C++ wins and this document is wrong. Because messages leave
through the game's own `SendComposedChat`, nothing changes on the wire and players with
wc2r-plus, with w2rpm or with neither interoperate as before. Only the host-side changes (the
ABI additions below) are new.

## What it does (parity target)

All of it is in-match; the lobby chat is the game's own and stays untouched.

1. **History panel.** A scrollable box of past messages (default 8 wrapped lines high, backlog
   100 messages, 25 to 500) above the game's two-line prompt/sticky strip. Shown while paused,
   while typing, or always when fade is 0. Otherwise recent lines only, fading after
   `fade` seconds (default 7, 0 = never).
2. **Channels.** Every message is labelled and coloured: `[All]` grey, `[Team]` green,
   `[Allies]` blue, `[Enemies]`/`[Private]` grey, system lines gold. Enter opens `[Team]`,
   Ctrl+Enter `[Allies]`, Shift+Enter `[All]` (falling back to `[All]` when the channel would
   be empty). Tab cycles Team, All, Allies while typing, skipping empty channels. The prompt
   line carries the channel label and colour.
3. **Player-name colours.** The sender's name is drawn in their in-game colour (black player
   gets a dark grey so it stays readable). System lines get every player name coloured.
4. **Timestamps** (`[HH:MM]`, off by default).
5. **Pause announcements.** "X paused the game. (N pauses remaining)" and "X resumed the game."
   as system lines, replacing the game's sticky message for those two events.
6. **Click closes chat** (off by default): a click discards the draft and returns focus to
   the game so hotkeys work.
7. **Disk log** (off by default): `chat-<date>_<time>.log`, one per launch, plus a
   `=== new match ===` marker.
8. **Settings** on the plugin screen: history height, fade time, click closes chat, backlog
   limit, timestamps, log to disk, background opacity (0 to 255, default 100), colour names,
   enhanced channels, and a master enable.

Non-goals: lobby chat changes, new chat features, changing what is sent on the wire (messages
go out through the game's own `SendComposedChat`; players without the plugin interoperate).

## Why this does not fit the ABI yet

The chat code needs four things the ABI does not offer today. Everything else it uses is
already there.

| Need | Today | Required change |
|---|---|---|
| Call game functions that are not hooked (about 14 Nuklear/layout helpers: `OverrideNextRect`, `BeginNamedGroup`, `CountWrappedLines`, `NkFillRect`, ...) | `symbol_lookup` returns an opaque handle; only `symbol_read` (read-only) and `hook_install` use it | `symbol_address(token, handle, out *usize)` |
| Read and write game globals (`ChatTargetMode`, `ChatRecipientMask`, message slots, the Nuklear context's style fields) | `symbol_read` only reads, 4,096 bytes at a time | the same `symbol_address`; the plugin writes through the pointer |
| Settings | `settings_*` exist in the ABI but the host returns `W2R_E_UNSUPPORTED` | implement them (stored in `config\<plugin-id>.json`, rules already in abi.md) |
| Disk log | `data_*` (`fs.data`) exist in the ABI, host returns `W2R_E_UNSUPPORTED` | implement them |

**`symbol_address`.** Appended at the end of `W2rHost` (ABI is still pre-release, so no
`W2R_ABI_VERSION` bump; a later host appends the same way and reports a larger `struct_size`).
The handle still comes from `symbol_lookup`, so the plugin declares `game.read` as well as
`hooks.raw`. It needs `hooks.raw`, not `game.read`: a raw address
lets a plugin read and write any game memory, which is exactly what `hooks.raw` is already
shown as **dangerous** for, and the consent prompt must say so. It returns the runtime
address (image base added) of a symbol of any kind, always inside the game image, and
`W2R_E_NOT_FOUND` for a bad handle.
Because the plugin then calls into the game and writes game memory outside the host's guard,
the chat plugin carries its own protection: every detour wraps its body in `catch_unwind`, and
every access to Nuklear structure offsets is bounds-checked against the structure sizes the
plugin was built for. The manifest pins `game_builds` to `1.0.2.2818` (the offsets are
build-specific, like wc2r-plus).

`MATCH_START` is now fired by the host from the same call wc2r-plus uses as its match-start signal
(`ResetMapMessageSlots`, see `matchstart.rs`), which is all chat needs: it clears the backlog and
writes the log marker on match start. `MATCH_END` has no proven source in wc2r-plus and stays
reserved and unfired (docs/abi.md).

## Hooks the plugin installs

All are `cdecl`, all by `symbol_lookup` then `hook_install`, so each is inside the game
module and cannot overlap a host patch. The detour always forwards to the trampoline unless
noted. Signatures are the game's, as in `src/target/addresses.h` of wc2r-plus.

| Symbol (our table) | Signature | The plugin's change |
|---|---|---|
| `PushMapMessage` | `(const char* text, int highlight, unsigned ms)` | capture the line into the backlog (with channel and name spans), then forward |
| `DisplayChatOrGameMessage` | `(const char* text, uint8_t who, unsigned ms, char targetMode)` | publish the sender context (who, raw text, target mode) for the nested `PushMapMessage`, restore after |
| `BuildMapMessagesPanel` | `(void* ctx, float x, float y, float w, float h, int lineHeight)` | replaces the panel: history group, recent-lines group, strip group; temporarily flattens scrollbar style fields and restores them |
| `DrawMapMessageList` | `(void* ctx, int lineHeight)` | draws the prompt line in its channel colour, then the sticky line |
| `WriteStickyMessage` | `(const char* text, unsigned ms)` | drop it while inside the pause/resume handlers (the plugin announces instead) |
| `ChatTextboxEventProc` | `(void* dlg, uint8_t* event)` | on Enter-open, choose the channel from the modifier keys; swallow Tab while composing |
| `MinimapTerrainKeyProc` | `(void* dlg, uint8_t* event)` | Tab while composing cycles the channel |
| `SendComposedChat` | `(const char* text)` | set target mode and recipient mask for the chosen channel around the call, restore after |
| `WriteChatPromptLine` | `(const char* fmt, const char* a1, const char* a2)` | prefix the channel label, remember the prompt channel |
| `BuildInMatchHud` | `(char whichView)` | **replaced outright** (never forwards): the stock HUD pieces in the stock order plus the chat block always. The stock builder draws no chat block while paused, so without this there is no history and no prompt in a paused game. This is wc2r-plus `features/ui/in_match_hud.cpp`, not `chat.cpp`, and was missing from the first version of this spec |
| `EditControlEventProc` | `(uint8_t* control, uint8_t* event)` | after the original: while paused (`MatchPauseFlags == 1`) append the typed character to the chat edit control by hand, since the stock control refuses typing |
| `HandlePauseCommand`, `HandleResumeCommand` | `()` | set an "in handler" flag around the call; announce when the pause flag changed |

Game globals the plugin reads or writes: `MapMessageSlots` (16 slots, stride from wc2r-plus
`wc2r::kMessageSlotStride`), `ChatTargetMode`, `ChatRecipientMask`, `ChatDialog`,
`MatchPauseFlags`, `CommandIssuer`, `NetworkedMatch`, `PausesRemaining`, `LocalPlayer`, plus
the Nuklear context offsets in wc2r-plus `struct_offsets.h` (`kCtxScrollH`, `kCtxScrollV`,
`kCtxScrollbarSize`, panel bounds, mouse buttons). All symbols named above are already in
`target/symbols.rs` except possibly the pause handlers (ours are `HandlePauseCommand` and
`HandleResumeCommand`); the spec step verifies each by name and signature against the table.

## Data the plugin needs from elsewhere (settled)

Everything slices 3 and 4 need is a plain game global that wc2r-plus reads (its `team_colours`
and `lobby_teams` features are only thin wrappers), and every one is already in `target/symbols.rs`.
The plugin reads them through `symbol_address`; nothing waits on the lobby-policy step. Offsets are
wc2r-plus's (`struct_offsets.h`); the plugin logs them at every match start (`players:` lines, a
diagnostic to remove after slice 4) so they can be checked against a real match.

**Verified in the game (2026-10-04, build 1.0.2.2818, a skirmish against seven computers):** names,
owner (0 human, 1 computer, 3 empty slot), lobby team and the diplomacy row all read as
documented. Names and colour indices are *not* filled in yet when `MATCH_START` fires (all
colours read 0); by the first frame they are (indices 0 to 7 in slot order, black is 5). So
names and colours are read when a message arrives, as wc2r-plus does, and only the team mask is
taken at match start.

| Need | Symbol | Layout |
|---|---|---|
| Player name | `PlayerNames` | 8 slots of 0x38 bytes, NUL-terminated |
| Name colour index | `TeamColourReorder` | `u8` per player; 8 or more means no colour |
| Human or computer | `PlayerOwner` | `u8` per player: 0 human, 1 computer; "in the match" is owner 0 or 1 |
| Lobby team | `MpLobbySlots` | 0x26 bytes per slot, team byte at +0x0a (slot index = player index) |
| Alliance | `DiplomacyStance` | 0x10 bytes per player (row of the player), non-zero entry means allied |
| Local player | `LocalPlayer` | `u8` |
| Chat mode and recipients | `ChatTargetMode`, `ChatRecipientMask` | `u8` each; modes 2 all, 3 allies, 4 enemies, 5 private, 6 default team, 8 custom team |

- **Channel colours** (wc2r-plus 1.1.0): `[Team]`, `[All]` and `[Allies]` are settings `team_hex`
  (default `25e65f`), `all_hex` (`929292`) and `allies_hex` (`69a7ff`), six hex digits, edited in the
  plugin screen through `W2rUi.text_edit` and greyed out with `set_active` while "Enhanced chat
  channels" is off. A value that is not exactly six hex digits (no `#`) is logged once at load and the
  channel keeps its default, so a half-typed value never shows a wrong colour. `[Enemies]` and
  `[Private]` stay grey; system lines keep the highlight colour. With channels (or the plugin) off,
  every chat line uses the game's own text colour instead. `model::Palette` builds this once per
  frame, as wc2r-plus `LinePalette` does.
- **Colours** (8, RGB): (196,40,40), (48,72,200), (0,152,152), (128,48,168), (232,132,24),
  (32,32,32), (236,236,236), (232,212,48). Index 5 is the black player and is drawn as
  (0x20,0x20,0x20) grey in chat. wc2r-plus can recolour black (its "Black2Pink" option); that is the
  separate `w2r-unit-color` plugin (which replaced `w2r-black-recolor`). Chat reads the black entry of `TeamColourRgba` each frame:
  the stock value keeps the grey, any other value (a recolour) is used as is.
- **Team mask** (for `[Team]`): computed once at match start, as `TeammatesAtMatchStart` does: the
  bit of every other in-match player whose diplomacy entry from the local player is non-zero and
  whose lobby team equals the local player's.
- **Allied mask** (for `[Allies]`): computed live, the same without the lobby-team test.
- **Name spans** for colouring: the sender's name found in the line followed by `": "` (not
  preceded by a name byte), or every unique human player name of two or more characters in
  system lines, longest first, with word boundaries (`FindPlayerNames`); `<name>` whisper
  brackets become `[name]`.

## Plugin shape

- Crate `plugins/w2r-chat` in the `w2r-plugins` repository (cdylib, i686, Rust; builds against `w2r-abi`,
  with the SDK pieces it needs inline until `w2r-sdk` exists). It lived in `crates/chat-plugin` here until
  2026-10-08.
- Manifest: id `w2r-chat`, capabilities `game.read` and `hooks.raw`; `chat.read` is **not** needed (the plugin
  sees chat through its own hooks, with the game's raw text, and hands it straight back to
  the game's own renderer exactly as wc2r-plus does; it only cuts a message at a NUL),
  `affects_simulation: false` (it only changes display and the target mode of an outgoing
  message the game would send anyway).
- State is a main-thread value behind a mutex, as wc2r-plus does with a critical section: the
  game may call the message hooks off the main thread; the draw hooks run on it.

## Slices and exit criteria

Each slice ends with an in-game check by the user (they cannot easily run a second w2rpm
player, so checks are single-player or against a vanilla peer, and the host's watchdog
([watchdog.rs](../crates/w2r-core/src/watchdog.rs)) is on).

0. **Host prerequisites.** `symbol_address`; `settings_*`; `data_*`; fire `MATCH_START` (done in the same slice).
   Unit and integration tests in `w2r-core/tests`, an ABI doc update, and a
   capability-prompt wording update for `hooks.raw`. Exit: a fixture plugin round-trips each.
1. **Capture and history, and the paused HUD.** `PushMapMessage`, `DisplayChatOrGameMessage`,
   `BuildMapMessagesPanel`, `DrawMapMessageList`: the scrollable history works, including while
   paused, with fade and backlog settings. No channels, colours or timestamps yet.
2. **Pause and timestamps.** Pause/resume announcements, timestamps, disk log.
3. **Channels.** Compose-channel selection, Tab cycling, prompt label and colours.
4. **Name colours.**
5. **Click closes chat and polish.** Background opacity, scroll indicator, unfocus.

Exit for the step: feature parity with the list above on 1.0.2.2818, one regression test per
behaviour that has a pure core (backlog cap and fade, channel selection and cycling, name-span
finding, timestamp formatting), and a security pass focused on the detours (they are outside the
host's guard, see the abi.md "Hook detours" note).

## Risks

- **Render-path faults freeze or crash the game** (this session's lobby banner is the example:
  a Nuklear window in the wrong place hung the game's own combo code). The panel replacement
  runs every in-match frame; slices ship behind the settings master switch, and the plugin falls
  back to the game's original `BuildMapMessagesPanel` whenever any precondition fails.
- **Struct offsets are build-specific**; `game_builds` pins the plugin.
- **`symbol_address` weakens `game.read`'s guarantees**, hence `hooks.raw`.

## Open questions

1. (Settled, see above.) Which tables name colouring and the channels read.
2. (Settled.) `symbol_address` returns only the address, and the host checks it is inside the
   game image; the symbol table has no sizes, so the plugin bounds its own accesses.
