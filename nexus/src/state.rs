//! The TUI credential state file (tui-pairing plan Phase B): the node
//! token persisted on the TUI host so a relaunch reconnects without
//! re-pairing.
//!
//! Path: `${XDG_STATE_HOME:-$HOME/.local/state}/nexus/tui-<server>-<node>-<thread>.json`,
//! mode `0600`, content `{server, node, token, thread}` — the SAME key
//! and format as the Gleam PoC launcher, so a host can switch clients
//! without re-pairing. `server` is the base URL; the pairing code never
//! rests anywhere (it dies at the exchange).
//!
//! Discovery is by node name: a bare relaunch knows only
//! `NEXUS_NODE`, so the scan filters `tui-*.json` on `.node`, with the
//! server (`NEXUS_URL`) and thread (`NEXUS_THREAD`) as optional
//! narrowing. One match connects; several are ambiguous; none means
//! pair first.

use std::fs;
use std::io;
use std::path::Path;
use std::path::PathBuf;

use serde::Deserialize;
use serde::Serialize;

/// The directory under the state root holding every `tui-*.json`.
const STATE_DIR: &str = "nexus";

/// The state file name template pieces.
const FILE_PREFIX: &str = "tui-";
const FILE_SUFFIX: &str = ".json";

/// `${XDG_STATE_HOME:-$HOME/.local/state}/nexus` — the same root the
/// Gleam launcher writes.
pub fn state_root() -> PathBuf {
    let base = match std::env::var("XDG_STATE_HOME") {
        Ok(dir) if !dir.is_empty() => PathBuf::from(dir),
        _ => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_owned());
            PathBuf::from(home).join(".local/state")
        }
    };
    base.join(STATE_DIR)
}

/// A stored node credential — the state file's exact shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credential {
    pub server: String,
    pub node: String,
    pub token: String,
    pub thread: String,
}

/// Filename-safe key: every character outside `[a-zA-Z0-9._-]` becomes
/// `-`, then one trailing `-` is dropped (the launcher's
/// `tr -c … '-' | sed 's/-$//'`). Authority and thread segments only —
/// the legacy collapse is kept byte-identical so stored files under
/// those keys keep resolving.
fn name_key(raw: &str) -> String {
    let mut key: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if key.ends_with('-') {
        key.pop();
    }
    key
}

/// Node segment key — injective over the server's name charset
/// (`[a-zA-Z0-9_@-]`, the launcher's `node_key`): `@` becomes `%40`
/// before the collapse, and `%` is never a name character, so
/// `user@host` and `user-host` cannot share a state file and one
/// pairing cannot overwrite the other's credential. No trailing-dash
/// strip either (`node-` must not share `node`'s key).
fn node_key(node: &str) -> String {
    node.replace('@', "%40")
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '%' | '~' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect()
}

/// The server base's filename key: scheme stripped, authority up to
/// the first path separator.
fn authority_key(server_base: &str) -> String {
    let without_scheme = server_base
        .strip_prefix("https://")
        .or_else(|| server_base.strip_prefix("http://"))
        .unwrap_or(server_base);
    let authority = without_scheme.split('/').next().unwrap_or("");
    name_key(authority)
}

/// `tui-<server>-<node>-<thread>.json` under `root` — keyed per
/// (server, node, thread) because node names are unique PER THREAD.
pub fn state_path(root: &Path, server: &str, node: &str, thread: &str) -> PathBuf {
    root.join(format!(
        "{FILE_PREFIX}{}-{}-{}{FILE_SUFFIX}",
        authority_key(server),
        node_key(node),
        name_key(thread)
    ))
}

/// Persist a credential atomically: the directory made 0700, the bytes
/// written to a same-directory 0600 temp file, then renamed over the
/// target — the plaintext token never rests world-readable.
pub fn save(root: &Path, credential: &Credential) -> io::Result<PathBuf> {
    let path = state_path(
        root,
        &credential.server,
        &credential.node,
        &credential.thread,
    );
    fs::create_dir_all(root)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(root, fs::Permissions::from_mode(0o700))?;
    }
    let tmp = root.join(format!(
        "{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("state")
    ));
    let bytes = serde_json::to_vec(credential).map_err(io::Error::other)?;
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        let mut file = fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(&bytes)?;
    }
    #[cfg(not(unix))]
    fs::write(&tmp, &bytes)?;
    fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Load one state file. Missing, unreadable, and malformed files all
/// read as absent — a bare relaunch falls through to pairing.
pub fn load(path: &Path) -> Option<Credential> {
    let bytes = fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

/// The result of scanning the state root for one node's credential.
#[derive(Debug, PartialEq)]
pub enum Discovery {
    /// Exactly one match — the relaunch path.
    Found {
        path: PathBuf,
        credential: Credential,
    },
    /// Several files carry the same node name; the caller lists them
    /// and disambiguates with the pairing URL, `NEXUS_URL`, or
    /// `NEXUS_THREAD`.
    Ambiguous(Vec<(PathBuf, Credential)>),
    /// No stored credential under the name (or under the narrowing).
    Missing,
}

/// Find this node's stored credential. With a server base given, only
/// the matching server's file qualifies; with a thread given, only the
/// matching thread's.
pub fn discover(root: &Path, node: &str, server: Option<&str>, thread: Option<&str>) -> Discovery {
    let mut matches: Vec<(PathBuf, Credential)> = fs::read_dir(root)
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let name = path.file_name()?.to_str()?;
            name.starts_with(FILE_PREFIX).then_some(path)
        })
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .filter_map(|path| {
            let credential = load(&path)?;
            (credential.node == node).then_some((path, credential))
        })
        .filter(|(_, credential)| server.is_none_or(|base| credential.server == base))
        .filter(|(_, credential)| thread.is_none_or(|t| credential.thread == t))
        .collect();
    // Deterministic order: the launcher's glob is shell-sorted.
    matches.sort_by(|a, b| a.0.file_name().cmp(&b.0.file_name()));
    match matches.len() {
        0 => Discovery::Missing,
        1 => {
            let (path, credential) = matches.remove(0);
            Discovery::Found { path, credential }
        }
        _ => Discovery::Ambiguous(matches),
    }
}

/// Forget every stored credential under the node name (`--reset`);
/// returns the removed paths.
pub fn reset(root: &Path, node: &str) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    if let Ok(entries) = fs::read_dir(root) {
        for entry in entries.flatten() {
            let path = entry.path();
            if load(&path).is_some_and(|c| c.node == node) && fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn credential(node: &str, server: &str, thread: &str) -> Credential {
        Credential {
            server: server.to_owned(),
            node: node.to_owned(),
            token: "tok".to_owned(),
            thread: thread.to_owned(),
        }
    }

    #[test]
    fn name_key_matches_the_launcher_rule() {
        assert_eq!(name_key("proxmox-2"), "proxmox-2");
        assert_eq!(name_key("boss.home.arpa:8080"), "boss.home.arpa-8080");
        assert_eq!(name_key("host with spaces"), "host-with-spaces");
        assert_eq!(name_key("trail!"), "trail");
    }

    #[test]
    fn authority_key_strips_scheme_and_path() {
        assert_eq!(
            authority_key("http://boss.home.arpa:8080"),
            "boss.home.arpa-8080"
        );
        assert_eq!(
            authority_key("https://boss.home.arpa:8080/base"),
            "boss.home.arpa-8080"
        );
    }

    #[test]
    fn state_path_keys_per_server_node_thread() {
        let root = Path::new("/state/nexus");
        let path = state_path(root, "http://boss:8080", "proxmox-2", "th-1");
        assert_eq!(
            path,
            Path::new("/state/nexus/tui-boss-8080-proxmox-2-th-1.json")
        );
        // The node segment is injective: `user@host` must not collapse
        // onto `user-host` (one pairing would overwrite the other).
        let at_host = state_path(root, "http://boss:8080", "james@vps", "th-1");
        assert_eq!(
            at_host,
            Path::new("/state/nexus/tui-boss-8080-james%40vps-th-1.json")
        );
        assert_ne!(
            at_host,
            state_path(root, "http://boss:8080", "james-vps", "th-1")
        );
    }

    #[test]
    fn save_persists_0600_and_loads_round_trip() {
        let root = tempfile::tempdir().unwrap();
        let stored = credential("proxmox-2", "http://boss:8080", "th-1");
        let path = save(root.path(), &stored).unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
            let dir_mode = fs::metadata(root.path()).unwrap().permissions().mode();
            assert_eq!(dir_mode & 0o777, 0o700);
        }
        // The exact shape the Gleam launcher writes.
        let bytes = fs::read_to_string(&path).unwrap();
        let value: serde_json::Value = serde_json::from_str(&bytes).unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "server": "http://boss:8080",
                "node": "proxmox-2",
                "token": "tok",
                "thread": "th-1",
            })
        );
        assert_eq!(load(&path), Some(stored));
    }

    #[test]
    fn load_reads_absent_and_malformed_files_as_absent() {
        let root = tempfile::tempdir().unwrap();
        assert_eq!(load(&root.path().join("missing.json")), None);
        let malformed = root.path().join("malformed.json");
        fs::write(&malformed, "{oops").unwrap();
        assert_eq!(load(&malformed), None);
    }

    #[test]
    fn discover_one_ambiguous_and_narrowed_matches() {
        let root = tempfile::tempdir().unwrap();
        save(
            root.path(),
            &credential("proxmox-2", "http://boss:8080", "th-1"),
        )
        .unwrap();
        assert!(matches!(
            discover(root.path(), "proxmox-2", None, None),
            Discovery::Found { .. }
        ));

        // A second server's file for the same name: bare discovery
        // turns ambiguous, the server narrows back to one.
        save(
            root.path(),
            &credential("proxmox-2", "http://other:9000", "th-1"),
        )
        .unwrap();
        assert!(matches!(
            discover(root.path(), "proxmox-2", None, None),
            Discovery::Ambiguous(_)
        ));
        assert!(matches!(
            discover(root.path(), "proxmox-2", Some("http://boss:8080"), None),
            Discovery::Found { .. }
        ));
        // The thread narrows too (names are per-thread).
        assert!(matches!(
            discover(root.path(), "proxmox-2", None, Some("th-2")),
            Discovery::Missing
        ));
        // Unrelated names never match.
        assert!(matches!(
            discover(root.path(), "other-node", None, None),
            Discovery::Missing
        ));
    }

    #[test]
    fn reset_removes_every_credential_under_the_name() {
        let root = tempfile::tempdir().unwrap();
        save(
            root.path(),
            &credential("proxmox-2", "http://boss:8080", "th-1"),
        )
        .unwrap();
        save(
            root.path(),
            &credential("proxmox-2", "http://other:9000", "th-1"),
        )
        .unwrap();
        save(
            root.path(),
            &credential("keep-me", "http://boss:8080", "th-1"),
        )
        .unwrap();
        let removed = reset(root.path(), "proxmox-2");
        assert_eq!(removed.len(), 2);
        assert!(matches!(
            discover(root.path(), "proxmox-2", None, None),
            Discovery::Missing
        ));
        assert!(matches!(
            discover(root.path(), "keep-me", None, None),
            Discovery::Found { .. }
        ));
    }
}
