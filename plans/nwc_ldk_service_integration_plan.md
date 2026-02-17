# NWC + LDK Service Integration Plan

## Goal

Integrate a single live `LdkService` instance into the NWC service so that NWC methods use real LDK state, starting with a first happy-path end-to-end test:

1. Start regtest `bitcoind`.
2. Start NWC service with live LDK wired to that `bitcoind`.
3. Fund the LDK wallet and confirm on-chain.
4. Call NWC `get_balance`.
5. Verify returned balance reflects the funded amount.

## Scope

- LDK-only backend (no backend switch abstraction).
- Preserve existing auth/rate/quota flow.
- First milestone only requires live `get_info`/`get_balance`.

## Step 1: Introduce `LdkService`

Create `src/lightning/ldk_service.rs` with a focused API around one `ldk_node::Node` instance.

### Responsibilities

- Build/start/stop node lifecycle.
- Expose typed operations needed by handlers.
- Surface stable service-level errors.

### Proposed API

- `start_from_config(cfg: &Config) -> Result<Arc<LdkService>, LdkServiceInitError>`
- `node_id() -> String`
- `network() -> &'static str`
- `sync_wallets() -> Result<(), LdkServiceError>`
- `get_balance_msat() -> Result<u64, LdkServiceError>`
- `new_onchain_address() -> Result<String, LdkServiceError>` (test/internal helper)
- `stop() -> Result<(), LdkServiceError>`

## Step 2: Add Config Fields

Extend config parsing/types with required LDK+bitcoind settings:

- `network` (regtest)
- `bitcoind_rpc_host`
- `bitcoind_rpc_port`
- `bitcoind_rpc_user`
- `bitcoind_rpc_password`
- `ldk_storage_dir`
- optional `ldk_listen_addr`

Add startup validation with clear error messages for missing/invalid fields.

## Step 3: Wire a Process-Singleton `LdkService`

Initialize one `Arc<LdkService>` at startup and pass it through service context.

### Requirements

- Created once in `main` before request handling starts.
- Shared by all NWC handlers via context.
- Stopped once during shutdown.

## Step 4: Hook `LdkService` into NWC Handlers

Replace dummy handler responses with live LDK data for the first methods:

- `GetInfo`: use live `node_id` and `network`.
- `GetBalance`: return live wallet balance in msat.

Keep access control + rate/quota checks unchanged and before execution.

## Step 5: Centralize Error Mapping

Map `LdkServiceError` to `NIP47Error` in one place.

- Use deterministic `ErrorCode::Other` for internal/service failures in first phase.
- Keep existing refund-on-execution-failure behavior intact.

## Step 6: Add NWC+LDK Integration Test Suite

Create separate test target:

- `tests/nwc_ldk_integration.rs`
- `tests/nwc_ldk_integration/mod.rs`
- `tests/nwc_ldk_integration/get_balance_after_onchain_funding.rs`

Reuse existing regtest harness (`tests/common/bitcoind.rs`).

## Step 7: First Happy-Path Test Case

`get_balance_after_onchain_funding`:

1. Start bitcoind (regtest).
2. Start NWC service configured with live `LdkService`.
3. Obtain LDK on-chain address.
4. Send 1 BTC from bitcoind to that address.
5. Mine confirmation block(s).
6. Sync/wait until LDK reflects funds.
7. Send NWC `get_balance` request.
8. Assert returned msat is expected (exact or lower-bounded by expected policy).

## Step 8: Validation

Run in order:

1. `cargo test --test nwc_ldk_integration get_balance_after_onchain_funding -- --nocapture`
2. `cargo test --test bitcoin_integration -- --nocapture`
3. `cargo test -- --nocapture`

## Done Criteria

- NWC process starts with exactly one live `LdkService`.
- `get_info` and `get_balance` use real LDK data.
- First happy-path test passes reliably on regtest.
- Existing test suites remain green.
