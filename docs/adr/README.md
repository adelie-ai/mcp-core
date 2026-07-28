# Architecture Decision Records

Decisions that constrain how `mcp-core` may change, and why. An ADR is for a choice that
is expensive to reverse or easy to violate by accident - not for routine design.

## Format

One file per decision, `NNNN-kebab-case-title.md`, numbered sequentially from `0001`.
Numbers are never reused.

Each record carries a status, a date, and links to the issues it came from:

- **Proposed** - under discussion, not yet binding.
- **Accepted** - binding. A change that contradicts it needs a new ADR, not an edit.
- **Superseded by NNNN** - kept in place, with a pointer forward.

Sections: Context, Decision, Consequences, Alternatives considered, Revisit when.

Amend an accepted ADR only for corrections that do not change the decision. When the
decision itself changes, write the next one and mark this one superseded, so the reason
the old constraint existed stays readable.

## Index

| ADR | Title | Status |
|---|---|---|
| [0001](0001-mcp-protocol-dialect-seam.md) | The MCP protocol dialect seam | Accepted |
