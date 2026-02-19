NIP-XXX
======

Nostr Wallet Connect Controller
--------------------------------

`draft` `optional`

This NIP defines a protocol for controlling Lightning Network nodes through Nostr Wallet Connect (NWC), enabling remote
node management via Nostr relay communication.

## Motivation

TODO: Describe the problem this NIP solves and why it is needed.

## Definitions

- **Controller**: A Nostr client that sends commands to manage a Lightning node.
- **Node**: A Lightning Network node (e.g., LDK-based) that listens for and executes commands from the controller.
- **Relay**: A Nostr relay used for communication between the controller and the node.

## Protocol Flow

1. The node generates a connection URI containing its pubkey and preferred relay.
2. The controller connects using the URI and authenticates via NIP-44 encrypted messages.
3. The controller sends JSON-RPC requests to the node.
4. The node processes the requests and returns JSON-RPC responses.

## Access Control and Execution Flow

This section defines the expected behavior of the access layer in the node.

### Granting Access via Replaceable Events

Access grants are published as a parameterized replaceable Nostr event of kind `30078`. The event **content** MUST be a
JSON-encoded `UsageProfile`.

The node listens for these events and uses the latest replaceable event per controller pubkey as the active access
grant.

Generation:

- The owner constructs a parameterized replaceable event (kind `30078`).
- The event `pubkey` is the owner's pubkey.
- The event `content` is the JSON-encoded `UsageProfile`.
- The event includes a `d` tag whose value is `node_pubkey:user_pubkey` (the node being targeted and the user being
  granted access).
- The event `tags` MUST include a `p` tag with the relay's pubkey so the relay receives it.
- The event `tags` MAY include auxiliary metadata (e.g., label, scope, or policy identifiers).
- The event is signed by the owner and published to the relay.
- Subsequent updates use the same kind, pubkey, and `d` tag; the newest `created_at` replaces earlier grants.

### Revocation Procedure

- Publish an access update whose `methods` is an empty object `{}` (explicitly no permissions).
- Publish a Nostr deletion event (kind `5`) that references the grant event id.
- Subscribers that listen for deletion events will receive the revocation and SHOULD remove the grant immediately. Relay
  deletion propagation is not guaranteed, so implementations MUST also treat an empty `methods` object as no access.

### UsageProfile JSON

The `UsageProfile` defines per-controller permissions and limits. All numeric values are unsigned integers.

```json
{
  "methods": {
    "get_info": {},
    "get_balance": {
      "rate": {
        "rate_per_micro": 10,
        "max_capacity": 1000
      }
    },
    "pay_invoice": {}
  },
  "control": {
    "connect_peer": {},
    "open_channel": {},
    "close_channel": {},
    "list_channels": {}
  },
  "quota": {
    "rate_per_micro": 1,
    "max_capacity": 1000000
  }
}
```

Fields:

- `methods` (object, optional): Map of method name to a `MethodAccessRule`.
    - Missing `methods` means no restrictions apply to the user.
    - An empty `methods` object means the user has no method permissions.
- `control` (object, optional): Map of controller/admin method name to a `MethodAccessRule`.
    - Missing `control` means no control permissions are granted.
    - An empty `control` object means no control permissions are granted.
    - A control method MUST be explicitly present to be allowed.
- `methods.<method>.rate` (object, optional): Per-method rate limit, if missing no rate limit is applied.
    - `rate_per_micro` (u64, optional): Tokens refilled per microsecond. Default `0`.
    - `max_capacity` (u64, optional): Maximum token capacity. Default `u64::MAX`.
- `control.<method>.rate` (object, optional): Per-control-method rate limit, if missing no rate limit is applied.
    - `rate_per_micro` (u64, optional): Tokens refilled per microsecond. Default `0`.
    - `max_capacity` (u64, optional): Maximum token capacity. Default `u64::MAX`.
- `quota` (object, optional): Controller-wide spend quota, if missing no quota is applied.
    - `rate_per_micro` (u64, optional): Quota refill per microsecond. Default `0`.
    - `max_capacity` (u64, optional): Maximum quota capacity. Default `u64::MAX`.

Defaults:

- Missing numeric fields use their defaults (`rate_per_micro = 0`, `max_capacity = u64::MAX`).
- Missing optional objects are treated as absent limits.

### Request Handling Steps

1. **Decode**: Decrypt the event (NIP-44) and parse the JSON-RPC request into a structured request object.
2. **Validate**: Validate the request parameters for the given method. If validation fails, return an error response.
3. **Authorize**: Check whether the controller pubkey is permitted to call the method. Authorization is based on:
    - Ownership: Owners bypass access checks.
    - Method permissions: A controller must have an explicit permission entry for the method to proceed.
    - Control permissions: For control-kind events, a controller must have an explicit entry in `control`.
4. **Enforce Limits**: Apply rate and quota checks using the controller's access state.
    - Limits are evaluated without mutating state first.
    - If any check fails (missing permission, insufficient rate, insufficient quota), return an error response.
5. **Execute**: Dispatch the request to the method executor.
6. **Commit Usage**: If execution succeeds (or once execution is accepted), apply the rate/quota usage to state.

### Authorize

Authorization is based on the latest access grant event for the target relay and user.

Steps:

1. Extract the caller pubkey from the incoming request event.
2. If the caller pubkey is in the owner list, authorization succeeds immediately.
3. Look up the latest access grant event of kind `30078` with:
    - `pubkey` = owner pubkey
    - `d` tag = `node_pubkey:user_pubkey`
4. Parse the event `content` as a `UsageProfile`. If parsing fails, deny access.
5. Enforce the limits in the `UsageProfile` against the caller's access state.
6. Execute the method.
7. Update the caller's access state with the new usage.

#### Enforcing the User Profile

##### Step 1: Resolve the Grant

1. Resolve the caller's latest `UsageProfile` grant (kind `30078`) for `d = node_pubkey:user_pubkey`. If missing, deny
   access with `UNAUTHORIZED`.
2. Parse the grant content as `UsageProfile`. If parsing fails, deny access with `UNAUTHORIZED`.

##### Step 2: Method Authorization

1. Read `methods`. If `methods` does not exist, treat it as no restriction on methods and proceed.
2. If `methods` is present and empty, deny access with `RESTRICTED`.
3. If `methods` is present and the requested method is missing, deny access with `RESTRICTED`.

##### Step 2b: Control Method Authorization (control kind only)

1. Read `control`.
2. If `control` does not exist, deny access with `RESTRICTED`.
3. If `control` is present and empty, deny access with `RESTRICTED`.
4. If requested control method is missing from `control`, deny access with `RESTRICTED`.

##### Step 3: Rate Limit (only if methods are restricted)

1. Read the rate limit rule for the requested method.
2. If missing, no rate limit is applied and proceed.
3. If present, forecast current rate quota and check the rate limit. If above the limit, deny access with
   `RATE_LIMITED`.

##### Step 4: Quota

1. Read `quota`. If `quota` is missing or the method does not spend, treat it as no spending quota limit and proceed.
2. If `quota` is present, forecast current spending quota and check against the spending quota limit. If above the
   limit, deny access with `QUOTA_EXCEEDED`.

##### Step 5: Grant Access

1. Grant access.

### Error Behavior

- Missing or invalid request parameters return a `OTHER` error, with a validation error message.
- Missing permission returns an `UNAUTHORIZED` error.
- Rate limit exceed returns a `RATE_LIMITED` error.
- Quota exceed returns a `QUOTA_EXCEEDED` error.

### State Mutation Rules

- Authorization and limit checks are read-only.
- Usage is committed only after all checks have passed and the request is accepted for execution.

## Event Kinds

| Kind  | Description        |
|-------|--------------------|
| 23194 | Wallet Request (NWC-compatible methods) |
| 23195 | Wallet Response     |
| 23196 | Control Request (admin/channel methods) |
| 23197 | Control Response    |

## Request Format

Requests are NIP-44 encrypted JSON-RPC payloads:

```json
{
  "method": "<method_name>",
  "params": {}
}
```

## Methods

| Method          | Description               |
|-----------------|---------------------------|
| `get_info`      | Get node information      |
| `get_balance`   | Get node balance          |
| `make_invoice`  | Create a new invoice      |
| `pay_invoice`   | Pay a Lightning invoice   |
| `list_channels` | List open channels        |
| `open_channel`  | Open a new channel        |
| `close_channel` | Close an existing channel |
| `list_payments` | List payment history      |

### Admin / Channel Management Methods

The following controller methods are intended for node administration and channel lifecycle operations:

1. `connect_peer(pubkey, host, port)`
2. `open_channel(pubkey, host, port, capacity_sats, push_msat?)`
3. `close_channel(channel_id, force?)`
4. `list_channels()`
5. `get_channel(channel_id)`
6. `list_peers()`
7. `disconnect_peer(pubkey)`

Optional async operation tracking:

8. `get_operation_status(operation_id)` where mutating methods (e.g. `open_channel`, `close_channel`) return an `operation_id`.

## Response Format

Responses are NIP-44 encrypted JSON-RPC payloads:

```json
{
  "result_type": "<method_name>",
  "result": {},
  "error": {
    "code": "<error_code>",
    "message": "<error_message>"
  }
}
```

## Error Codes

| Code              | Description           |
|-------------------|-----------------------|
| `UNAUTHORIZED`    | Authentication failed |
| `NOT_FOUND`       | Resource not found    |
| `INTERNAL`        | Internal node error   |
| `RATE_LIMITED`    | Too many requests     |
| `NOT_IMPLEMENTED` | Method not supported  |

## Security Considerations

- All messages MUST be NIP-44 encrypted.
- Events MUST include an encryption tag indicating NIP-44 (e.g., `["encryption","nip44"]`).
- Nodes SHOULD implement rate limiting per connected controller.
- Nodes SHOULD support granular permission scoping per controller pubkey.
- Connection URIs MUST be treated as secrets and shared securely.

## Relation to Other NIPs

- [NIP-44](https://github.com/nostr-protocol/nips/blob/master/44.md): Encrypted Direct Messages
- [NIP-47](https://github.com/nostr-protocol/nips/blob/master/47.md): Nostr Wallet Connect

## Reference Implementation

TODO: Link to reference implementation.
