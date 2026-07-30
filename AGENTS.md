# Agent Instructions - mcp-core

Shared standards live in [AGENTS.base.md](AGENTS.base.md), which is generated. This file holds the rules specific to this repo.

`mcp-core` is the shared core for the adelie-ai hand-rolled MCP servers: JSON-RPC 2.0
protocol and dispatch, stdio / websocket / unix transports, and a standard Clap CLI with a
per-server extension hook. It is a library crate (a `thiserror` error taxonomy, no binary)
that every `*-mcp` server depends on - keep it transport-agnostic and free of any single
server's domain logic. Transports past stdio are feature-gated: `unix` is on by default,
`websocket` pulls in axum, and `auth` adds JWT / JWKS bearer validation on top of `websocket`.
A new optional dependency stays behind its feature so a stdio-only server never compiles it.

Warnings are denied mechanically - `[lints] rust.warnings = "deny"` and `clippy.all = "deny"`
in `Cargo.toml` - so `cargo build` / `test` / `clippy` hard-fail on any warning.

## Architecture decisions

`docs/adr/` records the constraints this crate is held to. Read them before changing the
dispatcher or the public `McpService` surface; they exist because the decisions are easy
to violate by accident and expensive to walk back. A change that contradicts an Accepted
ADR needs a new ADR, not a quiet edit.

Currently binding: [ADR 0001 - The MCP protocol dialect seam](docs/adr/0001-mcp-protocol-dialect-seam.md),
which keeps protocol-version handling out of `McpService` so the fleet absorbs a spec
revision as a pin bump rather than 13 server rewrites.

## Rust Conventions

Apply these consistently. The repo gate in **Overrides and additions to the shared base** is the floor.

### Coding
- `?` for error propagation. Reserve `unwrap` / `expect` for tests and proven invariants. When `expect`ing in production, the message must explain the invariant — not just describe what would be unwrapped.
- Prefer `&str` / `&[T]` in argument position; take ownership only when storing.
- Newtype wrappers for invariant-bearing values (validated ids, paths constrained to a directory, etc.).
- `From` / `Into` for type conversions; don't write `to_*` methods when traits suffice.
- Combinators (`map`, `and_then`, `unwrap_or_else`, `?`) over `match` for short `Option` / `Result` chains. Use `match` when there's branching control flow with side effects.
- Avoid `.clone()` on hot paths. `Arc<T>` for shared immutable, `Arc<Mutex<T>>` / `Arc<RwLock<T>>` for shared mutable.

### `unsafe`
- Don't use `unsafe` unless it's necessary AND you've reasoned about soundness. The bar is high.
- Required cases: `std::env::set_var` / `remove_var` (Rust 2024 edition makes these `unsafe` because libc env-mutation is not threadsafe). Anything else needs a strong justification.
- Every `unsafe` block must have a `// SAFETY:` comment naming the invariant the caller is relying on. No "obvious" unsafe — write the soundness argument down. Example:

  ```rust
  // SAFETY: single-threaded test; unique env-var name; no other code touches it.
  unsafe { std::env::remove_var(&unused); }
  ```

### Testing
- Unit tests colocated as `#[cfg(test)] mod tests {}` in lib files.
- Integration tests in `tests/` next to `Cargo.toml`.
- `#[tokio::test]` for async; `#[tokio::test(flavor = "multi_thread")]` only when explicitly testing concurrent behavior.
- Mock at trait boundaries. For HTTP: `httpmock`. For time: an injected `Clock` trait.
- Determinism: sort outputs before assertion; never depend on hash iteration order.
- `expect("descriptive reason")` over `unwrap()` in tests so failure messages are self-explanatory.
- Test public behavior, not private implementation. If a private fn needs testing, surface as `pub(crate)` with a documented contract.
- Don't hold `std::sync::MutexGuard` across `.await`. Drop the guard explicitly before awaiting — `clippy::await_holding_lock` flags this.

### Generics
- `impl Trait` in argument position for single-bound, single-use parameters.
- Named generics with `where` clauses for multiple bounds, recursion, or readability.
- Avoid generic explosion: 3+ generic parameters usually indicates a missing struct or associated type.
- Prefer `Arc<dyn Trait>` over hand-rolled enum-dispatch when there are many implementors and no perf-critical specialization.
- Trait bounds: keep `Send + Sync + 'static` co-located on the trait def when the trait is only useful in async contexts.

### Error handling
- Library crates: `thiserror` with structured variants.
- Binary crates: `anyhow` with `Context::context()` for narrative.
- **Never pattern-match on error message strings.** Pattern-match on variants. If you find yourself doing `error.to_string().contains("429")`, the upstream type is throwing away structured info that should be preserved.
- Surface enough context in `Display` for debugging without leaking secrets.

### Async
- Don't hold non-async locks (`std::sync::Mutex`, `parking_lot::Mutex`) across `.await`. Drop the guard explicitly, or use `tokio::sync::Mutex` if the lock genuinely needs to span the await.
- `tokio::join!` for independent parallel work; `tokio::try_join!` when both must succeed and the first error should cancel the rest.
- Long-running spawned tasks need cancellation — channel-based or `CancellationToken`. Don't leak.
- Cross-cutting context: `tokio::task_local!`.

### Documentation
- Doc comments (`///`) on every public item.
- Include rationale (`Why:` lines) for non-obvious choices, not just descriptions of behavior.
- Don't narrate PR / issue history in code comments. Reference issues only when the comment captures a non-obvious WHY tied to that issue.

## Overrides and additions to the shared base

Everything in [AGENTS.base.md](AGENTS.base.md) applies to this repo. This section
records only the points where this repo deliberately differs from the base, or adds a
rule the base does not have.

### 3.1 The gate for this repo (addition)

The `adelie-ai` repos have no CI, and this repo has no `justfile`. The gate is local and the
author runs it: `cargo fmt --check`, then `cargo clippy --all-targets -- -D warnings`, then
`cargo test`. The `[lints]` table denies warnings mechanically as well, so a plain
`cargo build` or `cargo test` also hard-fails on one.

### 4.3 Branch and pull request - merge when green (override, weaker than the base)

The base opens a pull request and waits for the user. In these repos the merge is delegated:
merge your own pull request as soon as it is green and independently shippable. Green here
means more than a clean build. The gate above passed, the tests cover the new behavior and
not only the absence of a panic, the security pass is done, and the change stands on its own.
Assign `dspadea` with `gh pr edit --add-assignee` and verify it; a review request from the
same account no-ops without an error, so never report a pull request as review-requested.
When in doubt, hold.

### 4.4 Worktrees - the group convention (addition)

Put the worktree at `.worktrees/<repo>/issue-N-slug/` under the group directory, on a branch
that mirrors the slug. Before you run tasks in parallel worktrees, look for shared files,
shared `Cargo.toml` dependency edits, and shared migration ordinals. Serialize the work where
they overlap, and tell each parallel agent the scope it owns.

### 6.1 Dependencies - the group's scan workflow (addition)

Base rule 6.1 sets the policy, including that a high or critical advisory blocks the change.
This group runs it with its own tooling:

1. Add the dependency (`cargo add <crate>`). This writes the lockfile but does not build.
2. Scan the updated lockfile with the `cve-mcp` server's `scan_packages` tool, or with
   `cargo audit`. Pass every (name, version, ecosystem) tuple.
3. Build only after the scan is clean, or after you have accepted the findings in writing.

### 9.1 Tracker for this project

GitHub Issues on `github.com/adelie-ai/mcp-core`, together with the shared `adelie-ai` project
board `Adelie AI Roadmap` (project number 1). Manage entries with the `gh` CLI
(`gh issue create`, `gh issue list`, `gh issue edit`, `gh pr create`). Put a new issue on the
board with `gh project item-add 1 --owner adelie-ai --url <issue-url>`, which lands it in
Todo. The board states are Todo, In Progress, and Done.

### Platform, not a single product (addition)

Adele is a platform, not one product. Solve for the general case at every seam that is
plural by domain: storage backends, LLM providers, transports, clients, MCP servers, speech
engines. When a requirement names two of something, ask whether the real requirement is N
of them, and build that one instead.

Put the abstraction at the port. Keep the conditional compilation and the selection in one
factory, so a new implementation costs a crate, a feature, and one arm - not an edit to
every implementation that already exists. A hand-rolled `AnyX` enum with a variant per
implementation is the shape that fails this test: it re-dispatches every trait method by
hand and grows with the set.

Base rule 7.3 still holds inside a component. Do not invent indirection that a single call
site does not need. It does not licence the narrow build at a platform seam, because there
the plurality is the product, and the seam is already past the three-call-site test.

Fail loudly and by name when a configured selection is not compiled in, or is unavailable.
Name what was asked for and what is actually present. Silent degradation to a lesser
backend hides the problem from the one person who could fix it.
