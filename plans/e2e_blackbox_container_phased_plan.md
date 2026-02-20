# E2E Black-box Container Plan (Phased)

## Phase 1: Container Artifact

Status: Done (issue #37)

### Scope
- Build `ldk-controller` as a Docker image from `tests/e2e/docker/ldk-controller/Dockerfile`.

### Work
1. Add multi-stage Dockerfile (builder + slim runtime).
2. Add `.dockerignore`.
3. Document required env vars and runtime volume path.

### Definition of Done
1. `docker build -f tests/e2e/docker/ldk-controller/Dockerfile -t ldk-controller:e2e .` succeeds.
2. `docker run --rm ldk-controller:e2e --help` (or equivalent startup command) succeeds.
3. No root user in runtime container (verify via image config or `id` in container).

## Phase 2: Black-box Boot Test (Infrastructure)

Status: Done (issue #38)

### Scope
- Prove stack boots: `bitcoind` + `strfry` + `ldk-controller` container.

### Work
1. Add test `tests/e2e_blackbox_container.rs`.
2. Start bitcoind + relay with `testcontainers`.
3. Start `ldk-controller` container with env/config connected to those services.
4. Add readiness wait strategy.

### Definition of Done
1. Test `e2e_container_stack_boots` passes.
2. It confirms controller process starts and stays alive for test duration.
3. It fails cleanly with actionable error if dependency startup fails.

## Phase 3: NWC Wallet Black-box Contract

### Scope
- Validate public wallet API over Nostr from outside the process.

### Work
1. In same test target, add scenario:
   - send NWC `get_info`
   - send NWC `get_balance`
2. Assert decrypted response payload correctness.

### Definition of Done
1. Test `e2e_nwc_get_info_get_balance_roundtrip` passes.
2. Assertions:
   - response event kind is wallet response kind
   - `get_info.network == "regtest"`
   - `get_info.pubkey` non-empty
   - `get_balance.balance` is numeric
3. No direct in-process Rust calls to service internals.

## Phase 4: Grant + Authorization Black-box

### Scope
- Validate grant ingestion and permission enforcement using node-based `d`.

### Work
1. Publish grant event with `d=node_pubkey:user_pubkey`.
2. Call allowed and disallowed methods from same controller key.
3. Verify expected allowed/denied responses.

### Definition of Done
1. Test `e2e_grant_authorization_enforced` passes.
2. Assertions:
   - allowed method succeeds
   - non-granted method returns `RESTRICTED`/expected code
   - grant is resolved only with node-based `d` format.

## Phase 5: Control Kind Black-box

### Scope
- Validate control-kind routing and at least one real control method in container mode.

### Work
1. Send control `list_channels` with valid control grant.
2. Optionally open channel (if stable in CI) then list again.
3. Validate control response kind and payload shape.

### Definition of Done
1. Test `e2e_control_list_channels_roundtrip` passes.
2. Assertions:
   - response kind is control response kind
   - auth via `control` section works
   - result is valid array/object structure.

## Phase 6: Full Scenario (Optional, heavier)

### Scope
- End-to-end value scenario in containerized black-box setup.

### Work
1. Alice controller opens channel via control API.
2. Run one payment each direction.
3. Verify payment success outcomes.

### Definition of Done
1. Test `e2e_control_open_channel_and_bidirectional_payment` passes.
2. Runtime budget and flakiness thresholds documented.
3. Marked optional/nightly if too heavy for default CI.

## Execution / CI Strategy

1. PR gating: phases 1-4 required.
2. Optional/nightly: phases 5-6 if runtime is high.
3. Final acceptance:
   - all required phase tests green in CI
   - reproducible local run documented in `tests/e2e/docker/ldk-controller/README.md`.
