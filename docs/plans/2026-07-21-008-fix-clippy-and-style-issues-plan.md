---
status: active
created: 2026-07-21
plan_depth: lightweight
origin: code-review
---

# fix: Resolve clippy warnings and code-style issues

## Summary

Fix all blocking clippy errors and style issues identified in the 2026-07-21 code review: 31 `unused_must_use` suppression gaps, `FromStr` trait implementation for `Direction`, derivable `Default` impls, dead policy-worker code, `is_empty()` on `GestureBuffer`, `config_editor.rs` mechanical cleanups, and `config_path()` deduplication in `main.rs`. All 64 tests must continue to pass.

## Problem Frame

Clippy with `-D warnings` produces 40 errors across 7 source files. While none are correctness bugs, they block CI enforcement (`cargo clippy --all-targets --all-features -- -D warnings` fails) and mask real issues. The codebase already uses `let _ =` for Win32 return suppression in 50+ places — the remaining 31 sites simply missed the pattern. The `Direction::from_str` inherent method shadows `std::str::FromStr`, breaking generic usage. The policy-worker pipeline (`enqueue_policy_resolution`, `PolicyRequest`, `HOOK_POLICY_QUEUE`, the policy thread) is dead — process resolution moved synchronous (`resolve_pid_sync`) in a prior refactor. `GestureBuffer::len()` without `is_empty()` is an API-completeness gap. Several `Default` impls are manual when `#[derive(Default)]` suffices.

## Requirements

1. `cargo clippy --all-targets --all-features -- -D warnings` must pass with zero errors
2. `cargo test --all-features` must pass with all 64 tests (no regression)
3. No runtime behavior changes — all fixes are mechanical or structural
4. Follow existing codebase conventions: `let _ =` for must_use, section dividers, snake_case tests

## Key Technical Decisions

1. **`unused_must_use`: `let _ =` suppression, not `#[allow]`** — matches the existing 50+ instances in the codebase. Suppressing inline keeps the lint active for future code while acknowledging this specific call's best-effort nature.

2. **`Direction::from_str` → `impl std::str::FromStr`** — the inherent method shadows the standard trait. Implementing the trait directly is the Rust-idiomatic fix. The single call site at `config.rs:376` in `.map(Direction::from_str)` works identically through trait dispatch.

3. **Dead code: remove full policy-worker pipeline** — not just `enqueue_policy_resolution`, but also `PolicyRequest`, `HOOK_POLICY_QUEUE`, `policy_tx`/`policy_rx` channel, and the policy worker thread. Nothing feeds this pipeline since the synchronous resolution path (`resolve_pid_sync`) replaced it. Removing the dead thread also drops a `thread::JoinHandle` that `main.rs` joins at shutdown.

4. **`BlacklistMode` derives `Default`, `ConfigFile` does not** — `ConfigFile`'s `Settings` field has serde defaults that differ from `Default::default()` values (e.g., `activation_threshold_dip = 3.0`). Deriving `Default` on `ConfigFile` would produce a `Settings` with `i64::default()` = 0 for threshold fields, diverging from the existing manual impl. Leave `ConfigFile` manual `Default` as-is (suppress the clippy lint with `#[allow]`), derive `Default` on `BlacklistMode`.

---

## Implementation Units

### U1. Add `let _ =` suppression to remaining Win32 calls

**Goal:** Suppress all `unused_must_use` warnings on Win32 API calls where the return value is intentionally discarded as best-effort.

**Requirements:** Clippy `-D warnings` zero errors on must_use lints

**Dependencies:** None

**Files:**
- `src/overlay.rs` — 9 sites (PatBlt, ShowWindow ×2, DeleteObject ×2, DeleteDC, DestroyWindow)
- `src/tray.rs` — 10 sites (DestroyMenu, AppendMenuW ×5, SetForegroundWindow, PostMessageW)
- `src/main.rs` — 5 sites (TranslateMessage, ShowWindowAsync ×3, SetWindowPos, PostMessageW ×2)
- `src/launch.rs` — 3 sites (ShowWindowAsync, FlashWindowEx, and the `SetForegroundWindow` call)
- `src/app_policy.rs` — 1 site (CloseHandle)
- `src/input_hook.rs` — 2 sites (PeekMessageW, CloseHandle)

**Approach:** Prefix each return value with `let _ =`. No restructuring — these are all already in `unsafe {}` blocks where appropriate.

**Patterns to follow:** Existing `let _ =` instances: `main.rs:819`, `config_editor.rs:143`, `input_hook.rs:206`

**Test scenarios:**
- Run `cargo clippy --all-targets --all-features -- -D warnings` — verify zero `unused_must_use` errors
- Run `cargo test --all-features` — verify all 64 tests still pass

**Verification:** Clippy exits 0 with `-D warnings`; all existing tests pass.

---

### U2. Implement `FromStr` for `Direction` and add `is_empty()` to `GestureBuffer`

**Goal:** Replace the inherent `Direction::from_str` method with a proper `std::str::FromStr` implementation. Add `GestureBuffer::is_empty()` for API completeness.

**Requirements:** Clippy `-D warnings` zero errors on `should_implement_trait` and `len_without_is_empty`

**Dependencies:** None

**Files:**
- `src/config.rs` — replace `impl Direction { pub fn from_str(...)` with `impl std::str::FromStr for Direction`
- `src/gesture.rs` — add `pub fn is_empty(&self) -> bool`

**Approach:**
- `Direction`: add `type Err = anyhow::Error;` to the `FromStr` impl. The body stays identical. Remove the inherent `impl Direction` block that contains only `from_str`. The call site at `config.rs:376` (`.map(Direction::from_str)`) continues to work through trait dispatch.
- `GestureBuffer`: add `pub fn is_empty(&self) -> bool { self.len == 0 }` after the existing `len()` method. Optionally add a unit test in the `#[cfg(test)] mod tests` block.

**Patterns to follow:** Existing `pub fn len()` at `gesture.rs:151` — match the doc-comment style.

**Test scenarios:**
- `Direction::from_str("N")` returns `Ok(Direction::N)` through the trait
- `Direction::from_str("X")` returns `Err(...)`
- `GestureBuffer::new(2).is_empty()` → `true`
- `GestureBuffer` with one point after `add_point()` → `is_empty()` → `false`
- `GestureBuffer` after `clear()` → `is_empty()` → `true`

**Verification:** Clippy passes; `config.rs` tests (`invalid_direction_rejected`, `parse_minimal_config`) pass; `gesture.rs` tests pass.

---

### U3. Remove dead policy-worker pipeline

**Goal:** Remove all dead code associated with the background policy resolution path that was superseded by synchronous resolution.

**Requirements:** Clippy `-D warnings` zero errors on `dead_code`

**Dependencies:** None (U1 and U2 can proceed independently of this)

**Files:**
- `src/input_hook.rs` — remove `PolicyRequest` struct, `enqueue_policy_resolution` function, `HOOK_POLICY_QUEUE` thread-local, `policy_queue` parameter from `create_hook_proc`, and the `policy_tx`/`policy_rx` channel from `spawn_hook_thread`
- `src/main.rs` — remove `_policy_handle` binding, drop the policy join from cleanup

**Approach:**
1. Delete `PolicyRequest` struct (line ~871) and `enqueue_policy_resolution` function (line ~939)
2. Remove `HOOK_POLICY_QUEUE` thread-local (line ~428)
3. Remove `policy_queue` parameter from `create_hook_proc` signature and its `OnceCell::set` call
4. Remove `policy_tx`/`policy_rx` channel creation and the policy worker thread spawn from `spawn_hook_thread`
5. Update the return type of `spawn_hook_thread` (drop one `JoinHandle` from the 7-tuple)
6. In `main.rs`: remove `_policy_handle` from the destructure; remove the `_policy_handle.join()` call in cleanup

The `resolve_pid_sync` function stays — it's the active inline resolution path called from the hook callback.

**Patterns to follow:** Existing channel setup in `spawn_hook_thread` for the recognition, replay workers.

**Test scenarios:**
- `cargo build --all-targets --all-features` succeeds
- `cargo clippy --all-targets --all-features -- -D warnings` passes (no dead_code)
- `cargo test --all-features` — all 64 tests pass (no test references `enqueue_policy_resolution` or `PolicyRequest` — confirmed by grep)

**Verification:** Build, clippy, and tests all pass. The hook callback still resolves unknown PIDs synchronously via `resolve_pid_sync`.

---

### U4. Fix `config_editor.rs` style issues and deduplicate `config_path()`

**Goal:** Resolve all remaining clippy style warnings in `config_editor.rs` and eliminate the duplicated config-path construction in `main.rs`.

**Requirements:** Clippy `-D warnings` zero errors on `field_reassign_with_default`, `unnecessary_cast`, `collapsible_if`, `unnecessary_map_or`, and `derivable_impls`

**Dependencies:** None

**Files:**
- `src/config_editor.rs` — 5× field_reassign_with_default, unnecessary_cast, collapsible_if, unnecessary_map_or
- `src/config.rs` — derive `Default` on `BlacklistMode`, add `#[allow(clippy::derivable_impls)]` on `ConfigFile`'s manual impl
- `src/main.rs` — replace inline config-path construction (lines 56–61) with call to `config_path()`

**Approach:**
- `config_editor.rs`:
  - `LVCOLUMNW`/`LVITEMW` init: use struct literal `LVCOLUMNW { mask: LVCF_TEXT, pszText: PWSTR(...), cx: ..., ..Default::default() }` instead of `Default::default()` + assignment
  - Line 705: remove outer `as u32` — `(wparam.0 as u32 >> 16)` is already `u32`
  - Line 1280: collapse `if next.0 == -1 { if !state.is_adding { ... } }` → `if next.0 == -1 && !state.is_adding { ... }`
  - Line 2285–2288: `.map_or(false, |arr| !arr.is_empty())` → `.is_some_and(|arr| !arr.is_empty())`
- `config.rs`: `#[derive(Default)]` on `BlacklistMode` with `#[default]` on `Blacklist`; add `#[allow(clippy::derivable_impls)]` on `ConfigFile`'s manual `Default` impl (serde defaults diverge from `Default::default()`)
- `main.rs`: replace lines 56–61 with `let config_path = config_path();` and move the `debug_logging` check after `config_path()` is available (the function is defined later in the file — either reorder or inline a call). The log path at lines 71–77 uses a different filename — keep separate.

**Patterns to follow:** Existing `#[allow(clippy::type_complexity)]` on `spawn_hook_thread` for acceptable lint suppression precedent.

**Test scenarios:**
- `cargo clippy --all-targets --all-features -- -D warnings` passes with zero errors
- `cargo test --all-features` — all 64 tests pass
- `cargo build --all-targets --all-features` succeeds (config_editor still compiles)

**Verification:** Full clippy pass; all tests green.

---

## Scope Boundaries

### In scope
- All 40 clippy errors that block `-D warnings`
- Dead policy-worker code removal
- Mechanical style fixes in `config_editor.rs`
- `config_path()` deduplication in `main.rs`

### Deferred to Follow-Up Work
- Adding `# Safety:` comments to undocumented `unsafe` blocks
- Refactoring `spawn_hook_thread`'s 7-tuple return type (suppressed with `#[allow(clippy::type_complexity)]`)
- Adding `#[derive(Default)]` to `Settings` — requires careful review of serde default divergence

### Outside scope
- Architecture changes
- New features
- Runtime behavior changes
- Test coverage expansion beyond the optional `is_empty()` unit test
