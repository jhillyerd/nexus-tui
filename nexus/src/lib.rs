//! Nexus node client: everything a paired TUI node needs to talk to a
//! nexus server. Upstream contracts: `nexus/docs/plans/environments.md`
//! (the `/node/ws` client protocol, the tool plane) and
//! `nexus/docs/plans/tui-pairing.md` (pairing grants, state file).
//!
//! The crate is additive and UI-free so it builds and tests offline:
//!
//! - [`protocol`]: the `"v":1` wire codec, both planes, pinned byte-exact
//!   against the server's `node_protocol_test` shapes.
//! - [`env`]: the four environment tools (`bash`, `read_file`,
//!   `write_file`, `list_dir`) with the v1-floor execution semantics —
//!   cwd default, wall-clock timeout, kill-tree on cancel/timeout, byte
//!   caps enforced while reading, lexical path confinement.
//! - [`pair`]: pairing-URL parsing and the public `POST /nodes/pair`
//!   exchange.
//! - [`state`]: the 0600 credential state file, keyed per
//!   (server, node, thread) exactly like the Gleam launcher, so a host
//!   can switch clients without re-pairing.
//! - [`client`]: the outbound WebSocket session — reconnect with
//!   backoff, event-plane fan-out, tool-plane serving, exit-code
//!   contract (4 on WS 401, 2 on other connect failures).

pub mod client;
pub mod env;
pub mod pair;
pub mod protocol;
pub mod state;
