Maki is being pruned into a nexus TUI client (see PLAN.md). Phase 3 (UI
rewiring) has landed: `maki-ui` renders the nexus event plane and the
binary is `nexus-tui` (pair → connect → render). `maki-ui` has NO
`maki-agent`/`maki-providers` dependency; `maki-lua` stays for keymaps,
input decor and config (D1) and still imports the agent crates, which are
vestigial and die with it in phase 4 — do not grow them. Upstream
contracts: `nexus/docs/plans/environments.md` (client protocol, tool
plane) and `nexus/docs/plans/tui-pairing.md` (pairing, state file).

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

Before pushing: `just ci` — the same gates CI runs (fmt-check, lint, test,
machete). Don't let CI be the first thing that runs rustfmt.

Read `justfile` for more.

Dev builds skip debug info for deps and vendored C (our crates keep it); to debug into a dep set `[profile.dev.package.<name>] debug = true`.

## Nix

`nix develop` (dev shell), `nix build`, `nix fmt` (nixfmt).

## Architecture

Rust workspace. Key crates in the root dir:

- maki-ui: ratatui interactive UI (elm like architecture) — wired onto
  the nexus event plane in phase 3 (`src/node.rs` projects `ServerFrame`
  into `NodeEvent`; the app renders one thread, one transcript)
- maki-highlight, maki-markdown: the rendering pipeline, no agent coupling
- maki-config: user config (shrinks to theme/keybindings in phase 4)
- maki-storage: input history + paths; the nexus credential state file
  lives in the nexus crate
- nexus (phase 2): WS client, frame codec, pairing exchange, the four env
  tools, reconnect loop
- binary (`src/`): `nexus-tui` — pair/discover → connect → UI, exit codes
  0/2/4 per tui-pairing.md

Vestigial until phase 4 deletes them: maki-agent, maki-providers (only
maki-lua imports them now), maki-lua (+ plugins/), maki-pack.

Built-in lua plugins in ./plugins load through maki-lua. Keymaps, input
decor and config load stay live; agent-coupled plugin APIs are inert
(the UI feeds no agent events and answers session/model requests with a
refusal).
