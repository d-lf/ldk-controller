NIP-XXX
======

Nostr Wallet Connect Controller
--------------------------------

`draft` `optional`

This NIP defines a protocol for controlling Lightning Network nodes through Nostr Wallet Connect (NWC), enabling remote node management via Nostr relay communication.

## Motivation

TODO: Describe the problem this NIP solves and why it is needed.

## Definitions

- **Controller**: A Nostr client that sends commands to manage a Lightning node.
- **Node**: A Lightning Network node (e.g., LDK-based) that listens for and executes commands from the controller.
- **Relay**: A Nostr relay used for communication between the controller and the node.

## Protocol Flow

1. The node generates a connection URI containing its pubkey and preferred relay.
2. The controller connects using the URI and authenticates via NIP-04 encrypted messages.
3. The controller sends JSON-RPC requests to the node.
4. The node processes the requests and returns JSON-RPC responses.

## Event Kinds

| Kind  | Description          |
|-------|----------------------|
| XXXXX | Controller Request   |
| XXXXX | Node Response        |

## Request Format

Requests are NIP-04 encrypted JSON-RPC payloads:

```json
{
  "method": "<method_name>",
  "params": {}
}
```

## Methods

| Method             | Description                          |
|--------------------|--------------------------------------|
| `get_info`         | Get node information                 |
| `get_balance`      | Get node balance                     |
| `make_invoice`     | Create a new invoice                 |
| `pay_invoice`      | Pay a Lightning invoice              |
| `list_channels`    | List open channels                   |
| `open_channel`     | Open a new channel                   |
| `close_channel`    | Close an existing channel            |
| `list_payments`    | List payment history                 |

## Response Format

Responses are NIP-04 encrypted JSON-RPC payloads:

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

| Code           | Description              |
|----------------|--------------------------|
| `UNAUTHORIZED` | Authentication failed    |
| `NOT_FOUND`    | Resource not found       |
| `INTERNAL`     | Internal node error      |
| `RATE_LIMITED`  | Too many requests       |
| `NOT_IMPLEMENTED` | Method not supported  |

## Security Considerations

- All messages MUST be NIP-04 encrypted.
- Nodes SHOULD implement rate limiting per connected controller.
- Nodes SHOULD support granular permission scoping per controller pubkey.
- Connection URIs MUST be treated as secrets and shared securely.

## Relation to Other NIPs

- [NIP-04](https://github.com/nostr-protocol/nips/blob/master/04.md): Encrypted Direct Messages
- [NIP-47](https://github.com/nostr-protocol/nips/blob/master/47.md): Nostr Wallet Connect

## Reference Implementation

TODO: Link to reference implementation.
