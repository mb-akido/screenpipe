# Repository structure

The repository is organized by runtime and release boundary:

| Path | Purpose |
| --- | --- |
| `apps/screenpipe-app-tauri/` | Desktop application: Next.js UI, Tauri shell, and desktop-only enterprise behavior |
| `crates/` | Shared Rust capture, storage, search, sync, privacy, and engine crates |
| `sdk/` | Independently released Node, Electron, Tauri, and Swift SDK |
| `packages/ai-gateway/` | Hosted AI gateway service |
| `packages/browser-extension/` | Browser extension |
| `packages/cli/` | npm distribution packages for the Rust CLI |
| `packages/screenpipe-mcp/` | MCP server package |
| `packages/e2e/` | Cross-runtime end-to-end test harness |
| `docs/` | Architecture, product specifications, and supporting assets |
| `scripts/` | Repository development and diagnostic tooling |

## Ownership rules

- Code that only ships with the desktop application belongs under
  `apps/screenpipe-app-tauri/`, including enterprise policy and telemetry code.
- Reusable Rust code belongs in a focused crate under `crates/`.
- The SDK stays at `sdk/` because it has its own workspace, lockfiles, examples,
  tests, and release workflow.
- Hosted services and independently distributed JavaScript packages keep their
  own manifests and lockfiles. There is intentionally no root JavaScript
  workspace.

When adding a new top-level directory, document its runtime, owner, test entry
point, and release boundary here.
