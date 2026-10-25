# Releasing

The crate is published to crates.io as **`dumpling-cli`** (the name `dumpling`
is taken). It installs a binary named `dumpling`:

```bash
cargo install dumpling-cli
dumpling --help
```

## One-time setup

1. **crates.io account**: sign in at <https://crates.io> with GitHub and verify
   your email (publishing requires a verified email).
2. **API token**: crates.io → Account Settings → API Tokens → New Token.
   Scopes: `publish-new` and `publish-update`. Crate pattern `dumpling-cli`
   once the crate exists (for the very first publish it has to be unrestricted).
3. **GitHub secret**: repo → Settings → Secrets and variables → Actions →
   New repository secret named `CARGO_REGISTRY_TOKEN`.
4. Confirm `package.name`, `repository` and `version` in `Cargo.toml`.

## Each release

```bash
# 1. bump `version` in Cargo.toml, update docs/PROGRESS.md, then:
cargo fmt --check && cargo clippy --all-targets -- -D warnings
docker compose up -d --wait && cargo test
cargo publish --dry-run          # packages and builds exactly what would ship

# 2. commit, then tag and push; the Release workflow does the publish
git commit -am "release: v0.1.0"
git tag v0.1.0
git push origin main --tags
```

The `Release` workflow refuses to publish if the tag does not match the
`Cargo.toml` version, and it re-runs the full test suite against MySQL first.

A published version can be yanked (`cargo yank`) but never deleted or
overwritten, so check the dry run before tagging.

## Minimum Rust version

`rust-version = "1.94"` (set by sqlx 0.9). CI checks it on every push.
