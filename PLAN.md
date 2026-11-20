# Pruning the maki fork into a nexus TUI client

Goal: keep the rendering-quality TUI, delete everything that nexus now owns.
Upstream contracts: `nexus/docs/plans/environments.md` (client protocol, tool
plane) and `nexus/docs/plans/tui-pairing.md` (pairing, state file).

## What the client must do (the whole product)

1. **Pair**: `nexus-tui <pair-url>` → `POST /nodes/pair` → node token →
   persist state file. Bare relaunch uses stored credential; `--reset` drops
   it; `--role viewer` for view-only.
2. **Connect**: outbound WS to `/node/ws?node=&token=&thread=&role=`; reconnect
   with backoff (the Gleam PoC halts on close — we build the real thing);
   replace-on-reconnect is handled server-side, we just tolerate the old
   socket dying.
3. **Render** the event plane: transcript lines, streaming deltas, tool chips,
   metrics, roster status lines (`[system] attached targets: …` rendered as
   status lines, not chat bubbles), full replay + mid-turn `turn_view` on
   (re)join. Unknown frame KINDs are skipped, never fatal (`"v":1`,
   additive-only).
4. **Send** user input (`send` frames) and pings.
5. **Serve the tool plane** (env role): execute `bash`, `read_file`,
   `write_file`, `list_dir` locally when `tools/call` arrives; answer
   `call_response`; honor `notifications/cancelled`. The `target` arg names
   this node — rewrite to the local path and run. No sandbox, no per-thread
   directory: the TUI is the operator's own host (the watching human is the
   control surface), so tools run in the launch directory and paths resolve
   anywhere on it. Server-side floors still apply: wall-clock timeout
   (`NEXUS_ENV_TIMEOUT_MS`), kill-tree on cancel/timeout, byte caps enforced
   *while reading* (`NEXUS_ENV_MAX_OUTPUT_BYTES`), non-zero exit is data
   (`"exit status N\n" <> output`), over-cap kill returns Ok with an
   omission note.
6. **Exit codes**: 4 on WS 401 (credential rejected → re-pair if we hold a
   pairing URL), 2 on other connect failures.

## What gets deleted outright

| Thing | LOC / size | Why |
|---|---|---|
| `maki-agent` | ~42k | The agent loop, tool registry, permissions, prompts, compaction, MCP client, subagents — nexus owns all of it. |
| `maki-providers` | ~39k | LLM providers, models cache, pricing, retry — nexus talks to the model. |
| `maki-lua` + `plugins/` | ~66k + 1.7M | Luau plugin system, 35 tree-sitter grammars, neovim-mirrored API. See decision D1 — this is the big one. |
| `maki-interpreter` | ~2k | Python sandbox; `code_execution` runs on nexus. |
| `maki-acp` | ~4k | Explicit non-goal in the environments plan. |
| `maki-otel` | ~2k | No OTel for a thin client; plain tracing is enough. |
| `maki-docgen` + `site/` | ~3k + 11M | No product docs site. README only. |
| `maki-pack` | ~3k | Package/skill manager — no skills in v1. |
| `src/sdk_mode.rs`, `cmd/acp.rs`, `print.rs`, `provider_scripts.rs`, `setup.rs`, `resume.rs`, `subagent_tasks.rs`, `cmd/migrate.rs` | most of `src/` | Headless/SDK/ACP/provider plumbing for the agent we no longer embed. |
| `.github/workflows/python.yml`, `scripts/` | — | Agent analysis scripts. |
| Nix machinery beyond `nix develop/build` | — | No site/docgen/flake-check surface. |

## What is kept, and how it shrinks

- **`maki-ui` (~79k)** — the reason this fork exists. Keep: event loop, ratatui
  app, `text_buffer`, markdown/highlight pipeline, messages/tool display,
  input, themes, wrap, clipboard, selection, status bar. Replace:
  `src/agent/` (AgentLoop/AgentHandles) with a nexus client task that decodes
  event-plane frames into the UI's event enum. Delete components that have no
  client-side meaning: `model_picker`, `login_picker`, `mcp_picker`,
  `permission_prompt` (no approvals in the Pi model — the human watching IS
  the control surface), `usage_modal`, `pack_review`, `lua_float`,
  `rewind_picker` (server owns the transcript). `file_picker`/`code_view` may
  survive if we want local file browsing; decide when we get there.
- **`maki-highlight` + `maki-markdown` (~4k)** — kept as-is; they're the
  rendering pipeline and have no agent coupling.
- **`maki-config` (+macro)** — shrinks to: server URL, node name, role,
  thread, theme, keybindings. Drop providers/models/tools/permissions/policies.
- **`maki-storage`** — shrinks to: the pairing state file
  (`${XDG_STATE_HOME:-$HOME/.local/state}/nexus/tui-<server>-<node>-<thread>.json`,
  0600, `{server, node, token, thread}` — **same format and key as the Gleam
  PoC launcher**, so a host can switch clients without re-pairing) and input
  history if we want it. Sessions/auth/trusted-folders/plans go: the server
  owns the transcript; trust lives server-side.
- **New crate or module: `nexus` client** — WS transport (tokio-tungstenite;
  none exists yet), frame codec with round-trip tests pinned to the same wire
  shapes as nexus's `node_protocol_test`, pairing exchange, the four env
  tools in Rust with the v1-floor semantics above, state file, reconnect
  loop. Pure additive — buildable and testable offline before any UI surgery.

Rough end state: ~85k LOC across `maki-ui` (trimmed), `nexus` client,
`config`, `storage`, `highlight`, `markdown` — from ~256k plus plugins.

## Decisions to settle as we go

- **D1 — when to strip `maki-lua`.** It is load-bearing for UI layout today:
  `Split`/floats/window commands, keymaps, input decoration, autocmds, and
  the plugin host all live there, wired through ~40 UI files. Stripping means
  porting `Split` + a default layout (chat + below-split for tool output) and
  keymap handling into `maki-ui`, config-file themes/keybindings instead of
  Lua. That kills mlua, vendored Luau C, and 35 tree-sitter grammars — the
  single biggest build/maintenance win. Recommendation: land the client and
  UI rewiring first with lua still present but its agent-coupled plugins
  inert, then strip it as its own phase before calling v1 done. (Alternatively
  strip first if UI surgery turns out to fight the lua wiring at every step.)
- **D2 — type rehoming.** `Message`/`ContentBlock`/`AgentError` live in
  `maki-providers` and `AgentEvent` in `maki-agent`; the UI imports them
  widely. The nexus event projection replaces the transcript model with its
  own shapes — do not keep `maki-providers` alive just for types; move the
  few that survive (if any) into the client crate or `maki-ui`.
- **D3 — naming.** Binary becomes `nexus-tui`, which collides with the Gleam
  PoC launcher in the nexus repo. Coordinate: either the PoC stays
  `bin/nexus-tui` until this replaces it, or this ships as `nexus-tui` and
  the PoC is retired on release. Crate renames (`maki-*` → something) are
  cosmetic; do them in the final polish pass, not mid-surgery.
- **D4 — images / paste.** The event plane v1 carries no images and Send is
  text-only; the UI's image/terminal-image/paste-image code is dead until the
  protocol grows. Strip now, re-add when nexus does.
- **D5 — release surface.** Keep maki's cross-platform release pipeline
  (the TUI runs on the machines users pair from) but it builds far less once
  the crates above are gone.

## Phases

1. **Dead weight** (mechanical, one PR): delete `site/`, `maki-docgen`,
   `maki-acp`, `maki-interpreter`, `maki-otel`, `maki-pack`, `scripts/`,
   python workflow, the dead `src/cmd` surface and `sdk_mode`; update
   justfile/CI/AGENTS.md (drop the agent/lua invariants that no longer apply).
   The workspace still contains agent/providers/lua — only what nothing
   transitively needs from the binary dies here; anything the UI still
   imports stays until phase 3.
2. **Client crate** (additive): WS client, codec, pairing, state file, the
   four env tools, reconnect/backoff, exit-code contract. Offline tests
   pinning the exact wire shapes (mirror nexus's `node_protocol_test`);
   an integration mode against a local nexus with the fake-provider.
3. **UI rewiring**: replace `maki-ui/src/agent/` with the client task; map
   event-plane frames to UI events (streaming deltas, tool chips, roster
   status lines); Send path from the input box; delete the dead components
   listed above; shrink `maki-config`. Delete `maki-agent`/`maki-providers`
   (per D2) once nothing imports them.
4. **Lua strip** (D1): port layout/keymaps/decor into `maki-ui`, delete
   `maki-lua`, `plugins/`, mlua, tree-sitter.
5. **Polish**: crate/binary renames (D3), README rewrite, release pipeline
   trim (D5), image/paste decision (D4), e2e lane in nexus that drives this
   binary the way `tui.spec.mjs` drives the Gleam PoC (piped stdin, offline
   beyond loopback).

Phase 1 and 2 are independent of each other and can land in parallel or
either order.

## Compat checklist (from the nexus plans — do not break)

- State file path/key/format identical to the Gleam launcher's; `--reset`;
  `NEXUS_NODE`/`NEXUS_ROLE`/`NEXUS_THREAD`/`NEXUS_PAIR_URL`/`NEXUS_URL`
  overrides with the same precedence; env wins over the grant's pin.
- `POST /nodes/pair` request/response as specced; 404 merged unknown/expired;
  409 name-taken is the friendly path; a code never authorizes name takeover.
- Auth query params or `Authorization: Bearer`; pre-upgrade failures are
  plain HTTP statuses — distinguish 401 (→ exit 4) from other failures
  (→ exit 2).
- Frames: `"v":1`; unknown KIND skipped; malformed known shape can close.
- `tools/call` arguments arrive as a proven JSON object and are embedded
  verbatim on the wire; results flatten to one text; a JSON-RPC error object
  decodes as a failed call.
- Roster lines are status lines; they never trigger a turn.
- Node bytes are external content: cap, truncate, render inertly.
