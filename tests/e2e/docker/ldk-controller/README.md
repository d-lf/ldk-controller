# ldk-controller E2E Container

This folder contains the E2E-focused container build for `ldk-controller`.

## Build

From repository root:

```bash
docker build -f tests/e2e/docker/ldk-controller/Dockerfile -t ldk-controller:e2e .
```

## Runtime Contract

The container runs as non-root user `ldk-controller` and uses:

- Working directory: `/var/lib/ldk-controller`
- Binary: `/usr/local/bin/ldk-controller`

The current binary expects `config.toml` in the working directory.

Required at runtime:

1. Mount a writable state/config volume to `/var/lib/ldk-controller`.
2. Provide `/var/lib/ldk-controller/config.toml`.
3. Ensure relay and bitcoind endpoints in config are reachable from the container network.

Example run:

```bash
docker run --rm \
  -v "$PWD/tests/e2e/runtime:/var/lib/ldk-controller" \
  --network host \
  ldk-controller:e2e
```

## Quick Checks

Check image was built:

```bash
docker image inspect ldk-controller:e2e >/dev/null
```

Check runtime user is non-root:

```bash
docker run --rm --entrypoint id ldk-controller:e2e -u
```
