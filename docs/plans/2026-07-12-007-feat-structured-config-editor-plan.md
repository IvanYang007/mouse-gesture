---
title: feat: Structured config editor with gesture form, ListView, and settings panel
type: feat
status: active
date: 2026-07-12
---

# feat: Structured Config Editor

## Summary

Replace the raw-TOML text editor with a structured Win32 GUI that displays gestures in a ListView, provides form fields for adding/editing/deleting gesture mappings, and exposes a settings panel — all without requiring the user to edit TOML directly. Uses `toml_edit` to preserve comments and formatting on save.

---

## Problem Frame

The current config editor (`src/config_editor.rs`) is a single multiline EDIT control showing raw TOML — functionally identical to opening `config.toml` in Notepad, which was the old tray right-click behavior before the context menu was added. Users need a structured interface to browse, add, modify, and delete gesture mappings without touching TOML syntax. Settings (thresholds, blacklist, start-with-windows) should be editable through form fields with immediate validation.

---

## Requirements

- R1. Display all gestures in a multi-column ListView (Name, Pattern, Action Type) with full-row selection.
- R2. Add new gesture mappings via a form panel with fields: gesture name, direction pattern, action type (dropdown: Window Command / Keyboard Combo / Launch), and action-specific parameters.
- R3. Edit an existing gesture by selecting it in the list and modifying its fields in the form panel.
- R4. Delete a gesture by selecting it in the list and clicking Delete.
- R5. Edit settings (activation threshold, sample distance, RDP epsilon, min gesture length, debug logging, start-with-windows) through numeric inputs and checkboxes.
- R6. Edit blacklist (mode dropdown, app list) through form fields.
- R7. Validating and saving generates valid TOML that preserves existing comments and formatting for unchanged sections.
- R8. Invalid input is rejected with an error dialog before saving — field-level validation (non-empty name, valid pattern syntax, valid action parameters).
- R9. Resizing the editor window reflows controls using anchor-based layout.
- R10. Closing the editor with unsaved changes prompts the user.
- R11. `cargo fmt`, `cargo check`, and `cargo clippy` pass without new warnings.
- R12. Existing 66 tests continue to pass.

---

## Scope Boundaries

- Excluded: Key-capture picker for keyboard combo editing — combo is entered as text (e.g., `Ctrl+W`).
- Excluded: Inline gesture testing or preview.
- Excluded: Drag-and-drop gesture reordering.
- Excluded: Multi-select or batch gesture operations.
- Excluded: The old raw-TOML editor is removed entirely — no dual-mode or tab switching.
- Excluded: Adding a GUI framework dependency — stays pure Win32.

---

## Context & Research

### Relevant Code and Patterns

- **`src/config_editor.rs`** — current raw-TOML editor. Window class registration, `EditorState` via `GWLP_USERDATA`, child control creation, `WM_SIZE` layout, `WM_COMMAND` for button clicks, atomic save pattern. Will be replaced.
- **`src/config.rs`** — `ConfigFile`, `GestureDef`, `ActionDef`, `WindowCommand`, `Settings`, `Blacklist`. `ConfigFile::parse(text)` and `ConfigFile::compile(gen, dpi)` for validation.
- **`src/tray.rs`** — control ID constants pattern (`IDM_CONFIGURE = 1001`, etc.).
- **`src/main.rs`** — `WM_APP_RELOAD_CONFIG` constant for posting reload after save.
- **`src/overlay.rs`** — window class pattern, `CreateWindowExW`, GDI font creation/cleanup.

### Technology Decisions

- **`toml_edit` crate** (v0.22) for comment-preserving TOML round-trip. Parse with `DocumentMut`, modify only changed keys, serialize back — existing comments and formatting survive.
- **`Win32_UI_Controls` feature** needed for `InitCommonControlsEx`, `ICC_LISTVIEW_CLASSES`, `WC_LISTVIEW`, `LVITEMW`, `LVCOLUMNW`, `LVM_*` messages, `NMLISTVIEW`, `WM_NOTIFY` handling.
- **ComboBox** (`WC_COMBOBOX`) does NOT need `InitCommonControlsEx` — it's a USER32 control, always available.
- **ListView notifications** arrive via `WM_NOTIFY` (not `WM_COMMAND`), requiring a new handler in the editor's window proc.

### External References

- `toml_edit` crate docs — comment-preserving TOML manipulation, used by `cargo-edit`
- MSDN ListView documentation — `LVM_INSERTITEM`, `LVN_ITEMCHANGED`, `NMLISTVIEW`

---

## Key Technical Decisions

- **`toml_edit` over `toml` for persistence**: Using `DocumentMut` preserves comments, whitespace, and section ordering. The editor parses into `DocumentMut` on open, reads/writes individual keys, and serializes on save — unchanged sections pass through verbatim.

- **ListView over ListBox**: ListView provides native multi-column report view with Name / Pattern / Action Type columns. ListBox is single-column and would require custom formatting. The `Win32_UI_Controls` feature addition is justified.

- **Anchor-based layout**: Controls are positioned/sized in `WM_SIZE` using a simple anchor system (Left, Right, Top, Bottom, Fill). This avoids hard-coded pixel positions on resize.

- **Replace, don't extend**: The old `config_editor.rs` is replaced entirely. A dual-mode (raw + structured) editor adds complexity without user benefit — the structured editor is strictly better for the use case.

---

## Implementation Units

### U1. Add dependencies and replace editor skeleton

**Goal:** Add `toml_edit` and `Win32_UI_Controls` dependencies. Replace `src/config_editor.rs` with a new scaffold: window class, empty `EditorState`, layout skeleton with placeholder controls. The old editor is removed.

**Requirements:** R11, R12

**Dependencies:** None

**Files:**
- Modify: `Cargo.toml` (add `toml_edit = "0.22"`, add `"Win32_UI_Controls"` to windows features)
- Rewrite: `src/config_editor.rs`

**Approach:**
1. Add `toml_edit = "0.22"` to Cargo.toml dependencies.
2. Add `"Win32_UI_Controls"` to the windows crate features array.
3. Call `InitCommonControlsEx` with `ICC_LISTVIEW_CLASSES` in the editor's `open()` before creating any controls.
4. Create a new `EditorState` with `config: DocumentMut`, `gesture_names: Vec<String>` (for list ordering), `selected_index: Option<usize>`, `dirty: bool`, and HWNDs for all child controls.
5. Window layout: ListView on the left (2/3 width), edit form panel on the right (1/3), settings section at the bottom, Save/Cancel buttons at bottom-right.
6. The window proc skeleton handles `WM_CLOSE`, `WM_NCDESTROY`, `WM_SIZE`, `WM_COMMAND`, `WM_NOTIFY`.
7. `WM_DESTROY` does NOT call `PostQuitMessage`.

**Patterns to follow:**
- `config_editor.rs` current window class registration and GWLP_USERDATA patterns
- `tray.rs` control ID constant naming (`ID_LISTVIEW = 1000`, etc.)
- `main.rs` `WM_APP_RELOAD_CONFIG` constant (import from or match)

**Test scenarios:**
- Happy path: `open()` creates window with empty ListView, disabled edit panel placeholder, settings section with current values, Save/Cancel buttons.
- Edge case: `open()` called while editor already open → existing window foregrounded (existing `AtomicIsize` pattern preserved).
- Edge case: Config file missing → editor opens with empty/default config.

**Verification:**
- `cargo check` — `toml_edit` and `Win32_UI_Controls` resolve.
- `cargo test` — all 66 existing tests pass (no config.rs or gesture.rs changes).

---

### U2. Load and display gesture list in ListView

**Goal:** Parse config into `DocumentMut` on open, extract gestures, and populate the ListView with Name / Pattern / Action Type columns.

**Requirements:** R1

**Dependencies:** U1

**Files:**
- Modify: `src/config_editor.rs`

**Approach:**
1. In `open()`, read the config file with `std::fs::read_to_string`. Parse into `toml_edit::DocumentMut`.
2. Extract gesture entries from `doc["gestures"]` as a `BTreeMap`-ordered list of `(name, pattern, action_type_label)` tuples.
3. Store gesture names in `EditorState.gesture_names` for index-to-name mapping.
4. Add three columns to ListView: "Name" (200px), "Pattern" (120px), "Action" (150px).
5. Populate rows with `LVITEMW` / `LVM_INSERTITEMW` for column 0, `LVM_SETITEMW` for sub-items.
6. Enable `LVS_EX_FULLROWSELECT` extended style.
7. On `WM_SIZE`, resize ListView to fill its allocated area.

**Patterns to follow:**
- `tray.rs` `IDM_CONFIGURE` pattern for `HMENU(child_id as isize)` in `CreateWindowExW`.
- `config_editor.rs` existing string encoding pattern (`.encode_utf16().chain(once(0))`).

**Test scenarios:**
- Happy path: Open editor with 5 gestures → ListView shows 5 rows with correct Name/Pattern/Action columns.
- Edge case: Config has 0 gestures → ListView shows empty (no rows, headers visible).
- Edge case: Config has gestures with different action types (window, key, launch) → Action column shows correct type label.

**Verification:**
- `cargo check` — `LVITEMW`, `LVM_INSERTITEMW`, `LVM_INSERTCOLUMNW` resolve.
- Manual: Open editor, verify all gestures appear in ListView.

---

### U3. Implement gesture add/edit/delete with form panel

**Goal:** Build the right-side form panel. Selecting a ListView row populates the form. Add/Clear/Delete buttons work. Form fields update the in-memory `DocumentMut`.

**Requirements:** R2, R3, R4

**Dependencies:** U2

**Files:**
- Modify: `src/config_editor.rs`

**Approach:**
1. Create form panel controls: gesture name EDIT, pattern EDIT, action type COMBOBOX, and action-specific parameter controls:
   - Window Command → ComboBox of all `WindowCommand` variants (Maximize, Minimize, etc.)
   - Keyboard Combo → EDIT with placeholder hint (e.g., "Ctrl+W")
   - Launch → two EDIT fields (path, args)
   Show/hide the appropriate parameter control(s) when the action type ComboBox selection changes. Also create Add, Clear, and Delete buttons.
2. Handle `WM_NOTIFY` / `LVN_ITEMCHANGED`: when selection changes, populate form fields from the selected gesture's data. Clear selection via `LVM_SETITEMSTATE` with `LVIS_SELECTED` = 0.
3. Add button: clear form fields, disable Delete button, focus name field. On second click (or dedicated Save button), validate fields and insert new gesture row. Note: gesture names are TOML table keys — renaming an existing gesture requires cloning the table entry at the old key, inserting at the new key, and removing the old key from the `DocumentMut` gestures table.
4. Clear button: clear form fields, deselect list item.
5. Delete button: remove selected gesture from `DocumentMut` and ListView, clear form.
6. When form fields are edited, update the in-memory `DocumentMut` (for the selected gesture) and set `dirty = true`.
7. Form panel visibility: show when a gesture is selected or Add is clicked; show placeholder text ("Select a gesture or click Add") when nothing is selected.
8. Validation on field changes: name must be non-empty and unique, pattern must match valid direction tokens.

**Control IDs:** `ID_LISTVIEW = 1000`, `ID_EDIT_NAME = 1001`, `ID_EDIT_PATTERN = 1002`, `ID_COMBO_TYPE = 1003`, `ID_EDIT_ACTION = 1004`, `ID_BTN_ADD = 1005`, `ID_BTN_CLEAR = 1006`, `ID_BTN_DELETE = 1007`.

**Patterns to follow:**
- Existing `config_editor.rs` `WM_COMMAND` for button clicks.
- Existing `read_edit_text` / `show_error` helper patterns.
- ComboBox: create with `CBS_DROPDOWNLIST`, populate via `CB_ADDSTRING`, read selection via `CB_GETCURSEL`.

**Test scenarios:**
- Happy path: Select gesture in list → form populates with name, pattern, action type, action value.
- Happy path: Click Add, enter name/pattern/action, save → new row appears in ListView.
- Happy path: Select gesture, edit name, select different gesture → dirty flag set.
- Happy path: Select gesture, click Delete → row removed from ListView, form cleared.
- Error path: Add with empty name → validation error, no row added.
- Error path: Add with duplicate name → validation error.
- Error path: Pattern contains invalid direction token → validation error.
- Edge case: Clear button with no selection → no-op.

**Verification:**
- `cargo check` — ComboBox constants (`CBS_DROPDOWNLIST`, `CB_ADDSTRING`) resolve.
- Manual: Add a new gesture with valid fields, verify it appears in ListView. Delete a gesture, verify it's removed.

---

### U4. Implement settings and blacklist editing

**Goal:** Add a settings panel at the bottom with form fields for all config settings and blacklist.

**Requirements:** R5, R6

**Dependencies:** U1

**Files:**
- Modify: `src/config_editor.rs`

**Approach:**
1. Add a labeled "Settings" group with:
   - Activation threshold: EDIT + UpDown control (scale ×2 for 0.5 step: range 0–200, display by dividing by 2).
   - Sample distance: EDIT + UpDown (same ×2 scaling, range 0–50 → 0–100).
   - RDP epsilon: EDIT + UpDown (same ×2 scaling, range 0–50 → 0–100).
   - Min gesture length: EDIT + UpDown (integer, range 1–32).
   - Debug logging: CHECKBOX
   - Start with Windows: CHECKBOX
2. Add a "Blacklist" group with:
   - Mode: COMBOBOX ("Blacklist" / "Whitelist")
   - Apps: multiline EDIT (one app per line)
3. Populate settings from `DocumentMut` on open.
4. On settings field change, update `DocumentMut` and set `dirty = true`.
5. On `WM_SIZE`, anchor settings panel to the bottom of the window.
6. UpDown values: read the buddy EDIT control text on change via `WM_VSCROLL` / `EN_CHANGE` notification, or simply read on Save.

**Control IDs:** `ID_EDIT_THRESHOLD = 1010`, `ID_UPDOWN_THRESHOLD = 1011`, etc.

**Patterns to follow:**
- `CreateFontW` / `WM_SETFONT` pattern from existing editor for consistent font.
- `BS_AUTOCHECKBOX` style for checkboxes.

**Test scenarios:**
- Happy path: Open editor → settings show current values from config.
- Happy path: Change activation threshold to 5.0 → save → config.toml updated, daemon reloads.
- Happy path: Toggle debug_logging checkbox → save → reflected in config.
- Edge case: UpDown increments threshold → buddy EDIT text updates.

**Verification:**
- Manual: Edit settings, save, verify config.toml reflects changes and daemon reloads with new values.
- Manual: Edit blacklist, add an app, save, verify in config.toml.

---

### U5. Implement save with validation and dirty tracking

**Goal:** Save validates all fields, generates TOML from `DocumentMut`, writes atomically, and posts reload. Dirty tracking prompts on close.

**Requirements:** R7, R8, R10

**Dependencies:** U2, U3, U4

**Files:**
- Modify: `src/config_editor.rs`

**Approach:**
1. Validation before save:
   - All gesture names must be non-empty and unique.
   - All patterns must be parseable as valid direction tokens (reuse `Direction::from_str` logic or a simple regex).
   - Action parameters must be non-empty and match the action type (window command must be a valid `WindowCommand` variant, launch path must be non-empty).
   - Settings thresholds must be positive numbers.
   - Show `MessageBoxW` with the first validation error, focus the offending field.
2. On valid save:
   - Serialize `DocumentMut` to string via `.to_string()`.
   - Parse result through `ConfigFile::parse` + `compile(1, 96)` as a final safety check.
   - Write to `.tmp` file, rename to target path (atomic save).
   - Post `WM_APP_RELOAD_CONFIG` to owner.
   - Set `dirty = false`.
3. Dirty tracking:
   - Set `dirty = true` on any field edit (`WM_COMMAND` with `EN_CHANGE`, `CBN_SELCHANGE`, `BN_CLICKED`).
   - On `WM_CLOSE`: if `dirty`, show `MessageBoxW` with "Save changes?" (Yes/No/Cancel). Yes → save and close. No → close without saving. Cancel → do nothing.
   - On selecting a different gesture while current has unsaved edits: auto-save the current gesture's changes to `DocumentMut` (not to disk).

**Patterns to follow:**
- Existing `handle_save` function pattern from current `config_editor.rs` (atomic save, `MessageBoxW` for errors).
- Existing `show_error` helper.
- `config.rs` `Direction::from_str` for pattern validation.

**Test scenarios:**
- Happy path: Edit gesture, click Save → file written, daemon reloads, dirty flag cleared.
- Error path: Empty gesture name → Save shows error, editor stays open.
- Error path: Invalid pattern syntax → Save shows error, focuses pattern field.
- Edge case: Close window with unsaved changes → prompt appears. Click Yes → saves and closes.
- Edge case: Close window with unsaved changes → prompt appears. Click No → closes without saving.
- Edge case: Close window with unsaved changes → prompt appears. Click Cancel → stays open.

**Verification:**
- Manual: Make changes, save, verify config.toml updated. Open config.toml in Notepad, verify comments preserved.
- Manual: Make changes, close without saving, reopen editor — verify changes not persisted.
- Manual: Introduce invalid pattern, save — verify error dialog and editor stays open.

---

### U6. Anchor-based resize layout

**Goal:** All controls reflow correctly when the editor window is resized.

**Requirements:** R9

**Dependencies:** U1

**Files:**
- Modify: `src/config_editor.rs`

**Approach:**
1. Define an `Anchor` enum: `Left`, `Right`, `Top`, `Bottom`, `FillH`, `FillV`, `Fill`.
2. Define each control's anchors and margins in `EditorState` or a static layout table.
3. In `WM_SIZE`, compute new positions/dimensions:
   - ListView: anchors Left+Top+FillV, fixed width (or proportional), fills from top to settings panel.
   - Edit form panel: anchors Right+Top, fixed width (right side).
   - Settings panel: anchors Left+Right+Bottom, fixed height (bottom).
   - Save/Cancel buttons: anchors Right+Bottom, fixed size.
4. Use `SetWindowPos` with `SWP_NOZORDER` for repositioning.
5. Handle `WM_GETMINMAXINFO` to set minimum window size (600×400).

**Patterns to follow:**
- Existing `WM_SIZE` handler in `config_editor.rs` for layout pattern.
- Existing `SetWindowPos` usage in `main.rs` window action dispatch.

**Test scenarios:**
- Happy path: Resize window horizontally → ListView stretches, edit panel stays right-aligned.
- Happy path: Resize window vertically → ListView and edit panel stretch, settings panel stays bottom-anchored.
- Edge case: Window shrunk below minimum → `WM_GETMINMAXINFO` prevents further shrinking.

**Verification:**
- Manual: Resize editor window in all directions, verify controls reflow without overlapping.

---

## System-Wide Impact

- **Interaction graph:** The config editor window is a standalone top-level window with its own message pump (shared daemon thread). No changes to the daemon's `window_proc`, hook thread, or gesture engine. The `WM_APP_RELOAD_CONFIG` message path is unchanged.
- **Error propagation:** Validation errors display via `MessageBoxW`. Save errors display via `MessageBoxW` and `log::error!`. Failed file operations are surfaced to the user.
- **State lifecycle risks:** `EditorState` is stored via `GWLP_USERDATA` and freed in `WM_NCDESTROY`. `DocumentMut` is dropped with the state. `EDITOR_HWND` global handle is cleared in `WM_NCDESTROY`.
- **Unchanged invariants:** All existing modules are untouched except `config_editor.rs` (replaced). `ConfigFile`, `Direction`, `GestureDef`, `ActionDef`, `WindowCommand`, `Settings`, `Blacklist` types remain unchanged — the editor works with them through `DocumentMut` and validation helpers.
- **New dependency:** `toml_edit` v0.22 — a mature, widely-used crate maintained by the Rust project. No unsafe code in `toml_edit` itself.

---

## Risks & Dependencies

| Risk | Mitigation |
|------|------------|
| `toml_edit` comment preservation fails for complex TOML structures | Test with real user configs before merging; fall back to `toml` crate if needed (lose comments but correctness preserved) |
| ListView notification (`WM_NOTIFY`) is new to the codebase — no existing pattern | Thorough manual testing of selection events; the `WM_NOTIFY` handler pattern is well-documented |
| UpDown control initialization (`InitCommonControlsEx` with `ICC_UPDOWN_CLASS`) — first use in project | Test UpDown range, buddy attachment, and value reading |
| Form validation is client-side only — no server to double-check | Acceptable for a local config editor; `ConfigFile::parse` + `compile` on save provides the final safety net |
| Replacing `config_editor.rs` means the raw TOML view is gone | Explicitly scoped out; users who need raw TOML can open the file externally via "Open configuration folder" in the tray menu |

---

## Sources & References

- **Feature specification:** User request for structured config GUI instead of raw TOML text editor.
- Related code: `src/config_editor.rs` (to be replaced), `src/config.rs` (ConfigFile, GestureDef, ActionDef types), `src/tray.rs` (control ID pattern), `src/main.rs` (WM_APP_RELOAD_CONFIG).
- Related plan: `docs/plans/2026-07-12-006-feat-tray-context-menu-editor-plan.md` (original editor implementation).
- External: `toml_edit` crate (docs.rs/toml_edit), MSDN ListView documentation.
