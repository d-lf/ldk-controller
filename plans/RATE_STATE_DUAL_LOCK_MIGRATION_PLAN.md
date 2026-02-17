# RateState Dual-Lock Migration Plan

## Goal

Move access accounting from clone-and-reinsert to a dual-lock model:

- map lock for structural operations (insert/remove/clear)
- per-state lock for mutation (`Arc<Mutex<RateState>>`)

This aligns runtime behavior with the agreed accounting order:

1. check (simulate refill + verify)
2. refill + debit (commit)
3. refund on execution failure

## Step 1: Define Target Types (No Behavior Change)

- Change `AccessState` map value type from `RateState` to `Arc<Mutex<RateState>>`.
- Keep current key model and map split (`access_rate`, `quota`) unchanged.
- Limit this step to type migration and compile fixes only.

**Testable outcome**

- Project compiles.
- Existing behavior remains unchanged.

## Step 2: Add Store-Level Helpers

- Add helper APIs in `state/store` for state-handle lifecycle:
  - get existing handle
  - insert known state
  - retain/remove/clear by key/pubkey
- Keep map-locking centralized in store helpers.
- Do not add lazy-create helpers for access path (`get_or_insert` style).

**Testable outcome**

- Store APIs exist for lifecycle operations without lazy-create semantics.
- Existing tests pass.

## Step 3: Make Profile Service the Sole State Creator

- Refactor `usage_profile/service.rs` to use store helpers for all structural operations.
- On profile upsert:
  - remove prior states for target pubkey
  - create fresh states from incoming rules
- Ensure this is the only state creation/reset path.

**Testable outcome**

- Profile update/reset behavior remains correct.
- Existing profile/grant tests continue passing.

## Step 4: Migrate `verify_access` to No-Lazy-Create Dual-Lock Phases

- Replace clone-and-reinsert flow with handle-based flow:
  - fetch existing `Arc<Mutex<RateState>>` (no lazy creation)
  - check phase: lock state and call `check_withdraw_after_refill`
  - commit phase: lock state and call `withdraw_after_refill`
- For multi-state operations (method + quota), use deterministic lock ordering to avoid deadlocks.

**Testable outcome**

- Access checks/commits work without map-value replacement or lazy state creation.
- Missing state in access path is handled as a deterministic error.

## Step 5: Integrate Refund Path

- After commit, on downstream execution failure:
  - lock the same state handle
  - apply `refund(amount, rule)` (capped by `max_capacity`)
- Ensure no refund on pre-check rejection.

**Testable outcome**

- Failure paths correctly restore state up to cap.
- Dedicated tests pass for refund correctness.

## Step 6: Remove Deprecated Accounting Calls From Access Path

- Remove use of deprecated methods in access flow:
  - `refill`
  - `withdraw`
  - `withdraw_force`
- Use only phased methods:
  - `check_withdraw_after_refill`
  - `withdraw_after_refill`
  - `refund`

**Testable outcome**

- Access path has no deprecated accounting calls.
- Compile warnings from this path are removed.

## Step 7: Add Concurrency-Focused Tests

- Add tests under integration subfolders for:
  - concurrent requests on same key
  - combined method + quota accounting
  - commit-then-refund behavior
  - non-negative balance invariant under contention

**Testable outcome**

- New tests pass consistently across repeated runs.
- No duplicate-reset or race regressions observed.

## Step 8: Add Lock-Hold Instrumentation

- Add temporary timing metrics/logging around:
  - map lock hold time
  - per-state lock hold time
- Validate p99 hold times under synthetic load.

**Testable outcome**

- Metrics captured and reviewed.
- Lock contention profile documented.

## Step 9: Cleanup and Documentation

- Remove dead helpers and unused deferred-update artifacts.
- Update architecture docs:
  - `STATE_ACCOUNTING_MODEL.md`
  - `BALANCE_I64_MIGRATION.md`
- Confirm code/docs reflect final flow.

**Testable outcome**

- Full test suite passes.
- Docs match implemented behavior and lifecycle.
