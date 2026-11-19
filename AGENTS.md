Maki is being pruned into a nexus TUI client (see PLAN.md). The TUI
(maki-ui, maki-highlight, maki-markdown) is the product; `maki-agent`,
`maki-providers`, `maki-lua` are vestigial and die in phases 3-4 — do not
grow them. Upstream contracts: `nexus/docs/plans/environments.md` (client
protocol, tool plane) and `nexus/docs/plans/tui-pairing.md` (pairing,
state file).

## Code guidelines

- No trivial comments
- Minimal bloat (KISS, DRY, SRP)
- No unnecessary state (variables, fields, arguments)
- Each line of code should justify its existence
- Follow Rust idioms and best practices
- Latest Rust features can be used
- Descriptive variable and function names
- No wildcard imports
- Import types at top of file and use short names everywhere (e.g. `use std::sync::Arc;` then `Arc<T>`, never `std::sync::Arc<T>` inline)
- Keep consts at top of file, right after imports
- Explicit error handling with `Result<T, E>` over panics
- Use `color_eyre` when the specific error is not as important
- Use custom error types using `thiserror` for domain-specific errors
- Place unit tests in the same file using `#[cfg(test)]` modules
- Add dependencies to global `Cargo.toml`, and then set workspace=true in specific package
- Try solving with existing dependencies before adding new ones
- Prefer well-maintained crates from crates.io
- Be mindful of allocations in hot paths
- Prefer structured logging (wide logs with a bunch of useful fields)
- Provide helpful error messages
- Use #[test_case] when writing tests, and use snake_case for naming the tests
- No need for bullshit tests (e.g. tautology)
- Make sure tests are not flaky (no weird sleeps)
- No inline magic numbers or strings
- In tests const error/status messages and assert against the shared constant
- Add #[derive(Copy)] only on structs with 1 primitive field
- NO TRIVIAL COMMENTS

## Testing

Cheapest first, and scope to the crate you touched while iterating:

- `just check` (or `cargo check -p <crate> --all-targets`)
- `just lint` - `cargo clippy --all --all-targets -- -D warnings`
- `just test` - `cargo nextest run --workspace`

Read `justfile` for more.

Dev builds skip debug info for deps and vendored C (our crates keep it); to debug into a dep set `[profile.dev.package.<name>] debug = true`.

## Nix

`nix develop` (dev shell), `nix build`, `nix fmt` (nixfmt).

## Architecture

Rust workspace. Key crates in the root dir:

- maki-ui: ratatui interactive UI (elm like architecture) — the reason
  this fork exists; gets rewired onto the nexus event plane in phase 3
- maki-highlight, maki-markdown: the rendering pipeline, no agent coupling
- maki-config: user config (shrinks to server/node/role/thread/theme in phase 3)
- maki-storage: shrinks to the nexus pairing state file in phase 3
- nexus (phase 2): WS client, frame codec, pairing exchange, the four env
  tools, reconnect loop

Vestigial until their phase deletes them: maki-agent, maki-providers,
maki-lua (+ plugins/), maki-pack.

Built-in lua plugins in ./plugins load through maki-lua; agent-coupled
ones go inert as their tool plane moves to nexus.
