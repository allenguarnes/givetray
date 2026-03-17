# Hardening Follow-Up Design

Date: 2026-03-18

## Goals

- Prevent concurrent `givetray -c <profile>` instances from racing over one profile's runtime-state.
- Bound log-transport memory growth for noisy child processes.
- Make config, runtime-state, and desktop-file writes atomic.
- Reuse command validation when saving configuration from the UI.
- Make sudo prompt behavior consistent for interactive vs non-interactive sudo modes.

## Non-Goals

- Full multi-instance support for the same persistent profile.
- Redesign of tray/window ownership beyond profile-level locking.
- Changes to ephemeral mode concurrency behavior.

## Recommended Approach

Use single-instance-per-profile locking for persistent profile mode.

This is the safest fit for the current architecture because the app already treats one profile as having one command, one runtime-state file, and one tray-owned running/stopped state. Adding multi-instance ownership would require a broader redesign of recovery, stop semantics, and UI behavior.

## Architecture

### Profile Locking

- Add a per-profile lock file path derived from the sanitized profile name under the runtime data directory.
- Acquire the lock during startup preparation for persistent profile mode.
- Hold the lock for the process lifetime in app state.
- Only the lock owner may load, reconcile, write, or clear runtime-state for that profile.
- Ephemeral mode does not use profile locking.

### Second-Instance Behavior

- If a second `givetray -c <profile>` instance cannot acquire the profile lock, it starts as a non-owning session.
- Non-owning sessions log a startup message indicating the profile is already open elsewhere.
- Non-owning sessions do not load or reconcile runtime-state.
- Non-owning sessions do not allow `Start/Stop` actions.
- Configuration saving is disabled for non-owning sessions to avoid misleading edits while another instance is active.

### Runtime Recovery Interaction

- Runtime-state remains the source of truth for managed-process recovery.
- The profile lock becomes the source of truth for which UI instance is allowed to manage that profile.
- On startup, only the lock owner performs restored/stale/invalid runtime-state handling.
- On exit, dropping the process-held lock allows a later launch to become owner and reconcile normally.

## Supporting Hardening Changes

### Bounded Log Transport

- Replace the unbounded UI event channel with a bounded channel sized for bursty output.
- Introduce a log-overflow/coalescing path so producers do not grow memory without bound.
- If lines are dropped under pressure, emit one synthetic overflow message summarizing the loss.

### Atomic Writes

- Add a shared helper that writes to a temp file in the destination directory and renames into place.
- Use it for config saves, runtime-state saves, and desktop-file writes.
- Preserve existing caller-facing error behavior while reducing corruption risk from partial writes.

### Config Validation Reuse

- Reuse the same command validation rules already applied to CLI command overrides when saving from the configuration window.
- Reject empty or malformed commands before persisting them.
- Surface the error in the existing UI/log messaging path.

### Sudo Mode Handling

- Parse sudo mode once before launch.
- Treat plain `sudo` and explicit stdin-password mode as interactive.
- Treat `sudo -A`, `sudo --askpass`, and `sudo -n` as non-interactive/externally-managed and skip the GTK password prompt.
- Preserve existing `-S` behavior and avoid inserting conflicting flags.

## State Flow

### Persistent Profile Startup

1. Resolve config and runtime-state paths.
2. Attempt to acquire the profile lock.
3. If lock acquisition succeeds:
   - load runtime-state
   - reconcile restored/stale/invalid state
   - allow normal `Start/Stop` and configuration save behavior
4. If lock acquisition fails:
   - skip runtime-state load/reconcile
   - set startup message to "profile already open"
   - disable ownership-changing actions in this instance

### Start/Stop

- `Start` is blocked in non-owning persistent sessions.
- `Stop` is blocked in non-owning persistent sessions.
- Lock-owning sessions retain current runtime-state persistence and process-group stop behavior.

## Testing Plan

### Profile Locking

- Add tests for profile lock path generation.
- Add tests for successful first acquisition and failed second acquisition on the same profile.
- Add a regression test proving a non-owning second instance neither loads nor mutates runtime-state.

### Log Backpressure

- Add unit tests for overflow coalescing behavior.
- Keep a manual stress check with a noisy command such as `givetray -- yes`.

### Atomic Writes

- Add helper-level tests for atomic write success and overwrite behavior.
- Smoke-test config/runtime-state/desktop-file writes through existing save paths.

### Config Validation

- Add tests covering empty command text and malformed shell syntax in UI save paths.

### Sudo Parsing

- Add tests for `sudo`, `sudo -S`, `sudo --stdin`, `sudo -A`, `sudo --askpass`, and `sudo -n`.

## Risks And Trade-Offs

- Single-instance-per-profile intentionally rejects a previously possible but unsafe workflow.
- Bounded logging may drop lines under sustained overload, but this is preferable to unbounded memory growth.
- Atomic writes add small implementation complexity but reduce corruption risk significantly.

## Rollout Notes

- Normal single-instance users should see no behavior change except better safety and clearer messages.
- Users attempting two persistent instances for the same profile will now get a deterministic "already open" experience instead of undefined races.
