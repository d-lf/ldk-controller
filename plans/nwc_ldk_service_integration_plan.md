# NWC + LDK Service Integration Plan

## Goal

Run NWC on top of a single live `LdkService` and replace stubbed wallet actions with real LDK behavior, validated by regtest integration tests.

## Scope

- LDK-only backend (no backend switch abstraction).
- Keep current auth/rate/quota behavior unchanged.
- Keep current NIP-47 method surface and fill it with real execution incrementally.

## Progress (as of 2026-02-17)

- [x] `LdkService` introduced and wired.
- [x] Process-singleton LDK context added to NWC runtime.
- [x] `get_info` uses live LDK node identity.
- [x] `get_balance` uses live LDK wallet balance.
- [x] Regtest integration suite scaffolded under `tests/nwc_ldk_integration/`.
- [x] `get_balance_after_onchain_funding` integration test passes.
- [x] `make_invoice` handler uses live LDK call.
- [x] `pay_invoice` handler uses live LDK call.
- [x] `pay_keysend` handler uses live LDK call.
- [x] Centralized LDK -> NIP47 mapping helper for `make_invoice`/`pay_invoice`/`pay_keysend`.
- [x] Phase-4 integration tests added:
  - `make_invoice_happy_path`
  - `pay_invoice_invalid_invoice_returns_error`
  - `pay_keysend_invalid_pubkey_returns_error`
- [x] Phase-5 mapping coverage added:
  - message+code assertions for `pay_invoice` invalid invoice
  - message+code assertions for `pay_keysend` invalid pubkey
  - `make_invoice_invalid_description_hash_returns_error`
- [x] Phase-6 payment coverage added:
  - `pay_keysend_zero_amount_returns_error`
  - `pay_invoice_zero_amount_returns_error`
  - `pay_invoice` validation rejects `amount=0`
- [x] Phase-9.2 protocol routing added:
  - control request kind `23196`, control response kind `23197`
  - separate control dispatcher using same NIP-04 transport pipeline
  - deterministic errors for unknown/malformed control requests
  - control roundtrip integration tests
- [x] Full `cargo test -- --nocapture` green in Docker-enabled run.

## Current Design Decisions

- One `Arc<LdkService>` instance per process, shared across handlers.
- On execution errors, existing refund-on-failure flow remains in place.
- For now, deterministic per-handler error mapping is used (`PaymentFailed` for payment paths, `Other` elsewhere).
- Cross-node channel/payment happy-path stays in `bitcoin_integration` for now; `nwc_ldk_integration` keeps deterministic tests.

## Next Phases

### Phase 5: Centralize LDK -> NIP47 Error Mapping (Done)

1. Add one helper used by all LDK-backed handlers (likely in `src/lib.rs` or `src/lightning/`).
2. Normalize mapping for:
   - parse/validation errors
   - payment execution failures
   - transient service errors
3. Update `MakeInvoiceHandler`, `PayInvoiceHandler`, `PayKeysendHandler` to use helper.
4. Add focused tests asserting stable error codes/messages.

### Phase 6: Strengthen Payment Coverage (Done)

1. Add stable NWC-level negative-path tests for:
   - invalid amount
   - malformed destination pubkey
   - malformed invoice
2. Keep heavy cross-node success flow in `bitcoin_integration` (already present and passing).
3. Optionally add one NWC happy-path payment test if startup/channel reliability can be made deterministic.

### Phase 7: Cleanup and Hardening

1. Remove remaining legacy stub branches that are no longer needed.
2. Review and trim dead code warnings.
3. Document runtime requirements (Docker/regtest) for integration tests in project docs.
4. Update grant identity key format from `d = relay_pubkey:user_pubkey` to `d = node_pubkey:user_pubkey` across docs, publishing, and grant lookup logic.

### Phase 8: Hold Invoice Support

1. Wire `make_hold_invoice` to live LDK behavior.
2. Wire `settle_hold_invoice` to live LDK behavior.
3. Wire `cancel_hold_invoice` to live LDK behavior.
4. Centralize error mapping for hold-invoice paths (invalid hash/preimage, unknown invoice, invalid state transitions).
5. Add integration tests in `tests/nwc_ldk_integration/` covering:
   - create hold invoice success
   - settle hold invoice success
   - cancel hold invoice success
   - negative cases (unknown payment hash, double settle/cancel, invalid transition)
6. Remove hold-invoice stub fallback branches once live wiring is stable and test-covered.

### Phase 9: Control Kind + `control` Profile Section

1. Extend `UsageProfile` with optional `control` map for admin/control methods.
2. Add a separate control request/response `Kind` pair while reusing current NWC transport/encryption pipeline.
3. Implement control authorization rules:
   - missing `control` => deny all control methods
   - empty `control` => deny all control methods
   - method must exist explicitly in `control`
4. Keep wallet-method authorization unchanged under `methods`.
5. Add admin handler registry for control methods (`connect_peer`, `open_channel`, `close_channel`, `list_channels`, `get_channel`, `list_peers`, `disconnect_peer`).
6. Add integration tests for:
   - control method denied when `control` missing
   - control method denied when not explicitly listed
   - control method allowed when explicitly listed
7. Update grant identity key task in same stream: change `d` tag format from `relay_pubkey:user_pubkey` to `node_pubkey:user_pubkey` if not already completed.

#### Phase 9.1 Status: Done

- `UsageProfile` now contains optional `control` section.
- Existing profile literals/tests updated with `control: None`.

#### Phase 9.2 Status: Done

- Control transport is live on custom kinds:
  - request: `Kind::Custom(23196)`
  - response: `Kind::Custom(23197)`
- Control requests are decrypted and routed through a dedicated dispatcher.
- Current control handlers are protocol-ready and return deterministic `NOT_IMPLEMENTED` until method wiring is added.
- Integration coverage added in `tests/control_kind_roundtrip.rs`.

#### Phase 9.3 Status: Done

- Control authorization now enforced from `UsageProfile.control`.
- Policy implemented:
  - missing `control` => deny (`RESTRICTED`)
  - empty `control` => deny (`RESTRICTED`)
  - method not explicitly listed => deny (`RESTRICTED`)
  - listed method => proceeds to control dispatcher
- Integration coverage in `tests/control_kind_roundtrip.rs`:
  - `control_denied_when_control_missing`
  - `control_denied_when_method_not_listed`
  - `control_allowed_when_method_listed_returns_channels_array`

#### Phase 9.4 Status: Done (`list_channels`)

- First real control method wired: `list_channels`.
- `LdkService` now exposes typed channel summaries.
- Control dispatcher now returns:
  - `error: null`
  - `result: [ ...channels ]`
  for authorized `list_channels` requests.
- Integration coverage updated:
  - `control_allowed_when_method_listed_returns_channels_array`

## Validation Command

- `cargo test -- --nocapture`

## Latest Validation Snapshot

- Date: 2026-02-17
- Command: `cargo test --test nwc_ldk_integration -- --nocapture`
- Result: `ok` (6 passed, 0 failed)

## Immediate Next Action

- Execute Phase 6 by adding deterministic negative-path payment coverage for invalid amount handling.

## Done Criteria

- NWC runtime is fully backed by live `LdkService` for implemented methods.
- Error behavior is consistent and test-covered.
- Regtest integration suites stay green and deterministic enough for CI/local usage.
