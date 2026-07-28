# ADR 0001: The MCP protocol dialect seam

- **Status:** Accepted
- **Date:** 2026-07-27
- **Refs:** #23 (epic), #28 (2026-07-28 assessment), adelie-ai/desktop-assistant#931

## Context

MCP revisions are `YYYY-MM-DD` strings, defined by the spec as the last date a
backwards-incompatible change was made. The string therefore tells you that *something*
broke. It does not tell you how much, and it cannot express partial support.

The difference is not academic:

- `2025-06-18` to `2025-11-25` was, for what we implement, additive. Nothing we emit
  changed shape.
- `2025-11-25` to `2026-07-28` removes the `initialize` / `notifications/initialized`
  handshake, removes `ping` and `Mcp-Session-Id`, makes `server/discover` a required
  RPC, adds a mandatory `resultType` to every result, adds required `ttlMs` and
  `cacheScope` to list results, renumbers error codes, and moves the protocol version
  into `_meta` on every request.

Both are one step in the version list. One is a doc change for us; the other is a
different protocol. A consumer cannot distinguish them from the version strings alone.

The spec itself concedes this: `2026-07-28` introduces an `extensions` field on client
and server capabilities, moving feature negotiation off the version string.

### We must run two generations concurrently, in both directions

- **Our servers** must keep speaking older revisions because third-party clients connect
  to them and will lag.
- **Our client** must keep speaking older revisions because third-party servers lag.
  This is the longer tail, and the remote-MCP work depends on those servers.

Neither side moves first. The spec's feature-lifecycle policy sets a twelve-month
minimum deprecation window, which is the floor on how long this lasts, not the estimate.

### Where the code stands today

`mcp-core` is closer to correct than it looks. `McpService` is `tools()`, `call_tool()`
and `shutdown()` - domain operations. Every byte of wire JSON is constructed inside the
crate's dispatcher (`handle_initialize`, `tools_json`, `ToolReply::to_result_json`,
`tool_error_result`). No server in the fleet builds a protocol message.

`desktop-assistant`'s `mcp-client` is the weak side: it requests a hardcoded version and
discards the negotiated result entirely, so nothing downstream can know which generation
a connection is speaking.

## Decision

**1. Two axes, not one.** The version string selects the wire **dialect**. Capabilities
and extensions select **features**. Never infer a feature from a version number, and
never infer a wire shape from a capability. This mirrors where the spec is going and
keeps a feature gate from silently becoming a protocol gate.

**2. A dialect is a property of a connection.** It is resolved once, when the connection
is established, and carried with it for that connection's life. It is not global
configuration and it is not re-derived per call. A process may hold connections of
different dialects at the same time, and routinely will.

**3. `McpService` stays dialect-free.** Server authors provide domain types; all wire
construction stays in the `mcp-core` dispatcher.

The operative corollary, because this is what gets violated by accident: **no
dialect-specific wire field may be added to `ToolDef`, `ToolReply` or `CallError` in a
form that requires a server to populate it.** Optional metadata that is genuinely the
server's to know (an icon, a description) may live on those types. Protocol bookkeeping
that only the wire cares about (`resultType`, `ttlMs`, `cacheScope`) must be applied by
the dispatcher from the connection's dialect.

**4. Do not build the abstraction yet.** Two dialects do not earn a framework, and the
second is still a release candidate. What is not premature is retaining the negotiated
version per connection, since every possible dispatch strategy needs it and nothing can
start without it. Introduce a dialect type when the second dialect is real, and shape it
against the final spec rather than the candidate.

## Consequences

### What this buys

- The next fleet rollout stays a pin bump across 16 repos instead of 13 server rewrites.
  This is the whole point of the ADR; it is the difference between about a day and about
  a quarter.
- A dialect branch has one home. Reviewers know where to look, and where it must not
  appear.
- Server authors never learn there are two protocols.

### What it costs

- The dispatcher gains a branch on dialect and will grow. Accepted: one crate absorbing
  the complexity is the trade being made.
- Per-connection dialect is state we do not carry today; both `mcp-core` and `mcp-client`
  need somewhere to put it.
- Deferring the abstraction means the first `2026-07-28` change will be a larger, more
  invasive PR than if a seam already existed. Accepted, because designing that seam
  against a candidate risks fitting a shape that still moves.

### How this gets violated

Named so reviewers can watch for it:

- **#27 (SEP-973 icons) is the near-term risk.** Icons are genuinely server-supplied, so
  they are permitted on `ToolDef` under decision 3. That makes it precedent, and the
  *next* field proposed for `ToolDef` will cite it. Check that one against decision 3 on
  its merits, not against this one.
- Adding `ttlMs` or `cacheScope` to `ToolDef` because "the server knows best." It does
  not; those are cache directives about a wire response.
- Resolving the dialect from configuration rather than from the connection, which breaks
  the moment one process talks to peers of two generations.

## Alternatives considered

**Support one dialect and drop the old.** Rejected. It breaks every third-party peer in
both directions, and the deprecation window makes it wrong for at least a year.

**Version-keyed conditionals inline in the dispatcher, no dialect type.** Not rejected -
decision 4 accepts exactly this shape in the short term. It is tolerable at two dialects
and unmaintainable at three. The trigger to replace it is a third dialect, or the first
time a conditional appears outside the dispatcher.

**A crate or module per dialect.** Premature at two, and it would duplicate the large
unchanged middle (framing, transports, tool dispatch) to isolate a small changed edge.

## Revisit when

- `2026-07-28` ships final and stops being a candidate.
- A third concurrent dialect appears.
- A dialect conditional is needed outside the `mcp-core` dispatcher or the `mcp-client`
  connection setup - that is the signal decision 4's deferral has expired.
