# Connect To Deployed Node

This document describes how to connect to the deployed NWC node and verify it with a `get_info` request.

## Deployment Endpoint

- Relay URL: `wss://ldk-cw.flowrate.dev`
- Service pubkey (hex): `db0a960a68b14fcd4bf81b7a456e5d94e122f0416db6d3e9cd5c6f2c945e06d7`

## Owner Key (Generated)

- Owner `nsec`: `nsec1djtfpawwjz5u6lasr40p5ssd0d6tdzqkw50m89en06229euh7edsnmflan`
- Owner `npub`: `npub1v2rcrxlgn8p4yt328xejqtpag4uz9l9t4kc4tmkydhhmv5gajurqfmtn93`

## Run Deployed `get_info` Test

From repository root:

```bash
DEPLOYED_SERVICE_PUBKEY=db0a960a68b14fcd4bf81b7a456e5d94e122f0416db6d3e9cd5c6f2c945e06d7 \
DEPLOYED_RELAY_URL=wss://ldk-cw.flowrate.dev \
DEPLOYED_EXPECTED_NETWORK=signet \
cargo test --test e2e deployed_nwc_get_info_roundtrip -- --ignored --nocapture
```

Notes:

- `DEPLOYED_CLIENT_SECRET` is optional for this test.
- If omitted, the test generates an ephemeral client key and publishes a grant for that key.

## Direct Relay Reachability Checks

```bash
curl -I https://ldk-cw.flowrate.dev
```

WebSocket upgrade check:

```bash
curl -i --http1.1 \
  -H 'Connection: Upgrade' \
  -H 'Upgrade: websocket' \
  -H 'Sec-WebSocket-Version: 13' \
  -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
  https://ldk-cw.flowrate.dev/
```
