# Versioning And Tags

This project uses Semantic Versioning (`MAJOR.MINOR.PATCH`) and git tags prefixed with `v`.

## Tag Format

- Correct format: `v0.0.1`
- Do not use: `v.0.0.1`

## Number Meanings

1. `MAJOR` (`X.0.0`)
- Increment when you introduce breaking changes.

2. `MINOR` (`0.X.0`)
- Increment when you add backward-compatible features.

3. `PATCH` (`0.0.X`)
- Increment for backward-compatible bug fixes only.

## Examples

- `v0.1.0`: first feature release
- `v0.1.1`: bugfix release
- `v0.2.0`: new feature release, backward-compatible
- `v1.0.0`: first stable major release
- `v2.0.0`: breaking changes

## Pre-1.0 Policy (`0.x.y`)

Before `1.0.0`, breaking changes can still happen.
Team policy recommendation:

- Use `MINOR` bump for potential breaking changes in `0.x.y`.
- Use `PATCH` only for safe fixes.

## Source Of Truth

- Version in `Cargo.toml` is the source of truth.
- Release tag must match `Cargo.toml` version.

Example:

- `Cargo.toml` version: `0.3.2`
- Release tag: `v0.3.2`

## Release Flow (Short)

1. Update `Cargo.toml` version.
2. Commit changes.
3. Create tag:

```bash
git tag v0.3.2
git push origin v0.3.2
```

4. CI uses the tag to build artifacts (`.deb`, binaries, etc.).
