# ADR 0002: One JWT crypto backend, `aws_lc_rs`

- **Status:** Accepted
- **Date:** 2026-08-11
- **Refs:** #34, adelie-ai/command-mcp#20

## Context

`cargo audit` failed on this repo. It reported RUSTSEC-2023-0071 against `rsa` 0.9.10:
the Marvin attack, a timing sidechannel that can leak an RSA private key. The advisory
carries an empty `patched` list, so no upstream release fixes it.

`rsa` reached this crate through one path, `jsonwebtoken -> rsa`. `jsonwebtoken` is
optional and sits behind the `auth` feature, which validates websocket Bearer tokens.

`jsonwebtoken` puts its cryptography behind a `CryptoProvider`. Two backends ship with
the crate, and each one is a crate feature:

- `rust_crypto` uses the RustCrypto crates. It pulls in `rsa`, `p256`, `p384`,
  `ed25519-dalek`, `hmac`, `sha2` and `rand`.
- `aws_lc_rs` uses `aws-lc-rs`. It implements the same HMAC, ECDSA, EdDSA and RSA
  algorithms, and it does not use `rsa`.

This crate already builds `aws-lc-rs`, because `jwtk` depends on it and `jwtk` sits on
the same `auth` feature. The second backend therefore costs no new crate.
`desktop-assistant` already selects `aws_lc_rs`.

## Decision

Select `aws_lc_rs` as the `jsonwebtoken` backend. Never select `rust_crypto`.

Exactly one backend may be enabled in a built graph. `jsonwebtoken` resolves the
process-wide provider from its own crate features, and that resolution needs exactly one
of them. `CryptoProvider::from_crate_features` tests for `rust_crypto` and not
`aws_lc_rs`, then for `aws_lc_rs` and not `rust_crypto`. When both features are on,
neither arm matches, and the crate installs a provider that panics on the first sign or
verify.

Cargo unions features across a dependency graph. A downstream crate that names
`jsonwebtoken` with `rust_crypto` therefore turns both backends on, and breaks JWT
validation at run time in a build that compiles clean. Every crate in this group that
names `jsonwebtoken` must select `aws_lc_rs`.

## Consequences

- `cargo audit` on this repo reports no vulnerability and no warning.
- The lockfile loses 34 crates, among them `rsa` and the yanked `spin` 0.9.8.
- The `auth` feature signs and verifies through `aws-lc-rs`, the library that `jwtk` and
  `rustls` already use here.
- A downstream crate that selects `rust_crypto` breaks this crate's auth path at run
  time, not at build time. The symptom is a panic on the first token check. The auth
  tests in `src/auth.rs` catch it in any crate that runs them.

## Alternatives considered

**Keep `rust_crypto` and record an accepted risk.** The reachability argument holds.
`src/auth.rs` is the only caller of `jsonwebtoken`, and it calls `decode` with a
`DecodingKey::from_secret` HMAC key and a `Validation` built from `Validation::default()`,
whose algorithm list is `[HS256]`. `decode` rejects a token whose header algorithm is
absent from that list before it verifies any signature, so no `rsa` code runs. No RSA
private key is ever built, and the Marvin attack needs private-key operations to time.
The vulnerable code was therefore already unreachable. Rejected because the backend swap
removes the crate outright and costs nothing. An argument that a reader must re-check
every release is worse than a dependency that is not there.

**Drop `jsonwebtoken` and verify HS256 through `jwtk`.** A larger change to the auth
surface and its tests, for the same result.

## Revisit when

- `jsonwebtoken` changes how it resolves the default provider.
- A crate in this group needs an algorithm that `aws-lc-rs` does not implement.
- `aws-lc-rs` becomes hard to build for a target this group must support. It needs a C
  toolchain and CMake.
