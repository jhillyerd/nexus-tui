#[cfg(unix)]
use std::env;
#[cfg(unix)]
use std::fs;
use std::io::{Read, Write};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::path::Path;
#[cfg(unix)]
use std::process::Command;
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use maki_agent::AgentMode;
use maki_agent::agent::tool_dispatch;
use maki_agent::tools::interpreter_bridge;
use maki_agent::tools::test_support::stub_ctx;
use maki_agent::tools::{CallOrigin, ToolContext, ToolRegistry};
use maki_config::{DEFAULT_BUILTINS, PluginFileConfig, PluginsConfig};
use maki_lua::test_support::runtime;
use maki_lua::{PluginHost, set_allowed_private_hosts};
use serde_json::{Value, json};

const TOOLS: &[&str] = &["bash", "read", "write", "grep", "glob", "batch", "webfetch"];
const MODEL_BYTES: usize = 1024;
const MODEL_LINES: usize = 8;
const LATIN1_HTML: &[u8] = b"<h1>caf\xe9</h1>";
const LOSSY_TEXT: &str = "caf\u{fffd}";
const LOSSY_MARKDOWN: &str = "# caf\u{fffd}";
const LEGACY_HTML_TYPE: &str = "text/html; charset=ISO-8859-1";
const LATIN1_E_ACUTE: u8 = 0xe9;
const BINARY_TYPE: &str = "application/pdf";
const BINARY_PAYLOAD: &[u8] = b"%PDF-1.7\n\0\x01\x02";
const BINARY_ERROR: &str = "error: binary content (application/pdf) cannot be displayed as text";
const CALL_ID: &str = "output-test";
#[cfg(unix)]
const RTK_FIXTURE_DIR: &str = "MAKI_TEST_RTK_DIR";
#[cfg(unix)]
const RTK_LOG: &str = "MAKI_TEST_RTK_LOG";
#[cfg(unix)]
const RTK_COMMAND: &str = "printf original";

fn setup() -> (Arc<ToolRegistry>, PluginHost, ToolContext) {
    let registry = Arc::new(ToolRegistry::new());
    let mut host = PluginHost::new(Arc::clone(&registry)).unwrap();
    let plugins = DEFAULT_BUILTINS
        .iter()
        .filter(|name| !TOOLS.contains(name))
        .map(|name| {
            (
                (*name).to_owned(),
                PluginFileConfig {
                    enabled: Some(false),
                    ..Default::default()
                },
            )
        })
        .collect();
    host.load_builtins(&PluginsConfig::from_plugins(plugins))
        .unwrap();
    let mut ctx = stub_ctx(&AgentMode::Build);
    ctx.registry = Arc::clone(&registry);
    ctx.config.rtk = false;
    ctx.config.max_output_bytes = MODEL_BYTES;
    ctx.config.max_output_lines = MODEL_LINES;
    (registry, host, ctx)
}

fn run(ctx: &ToolContext, name: &str, input: Value) -> Result<String, String> {
    let done = runtime().block_on(tool_dispatch::run(
        CALL_ID.into(),
        name,
        &input,
        ctx,
        CallOrigin::Model,
    ));
    interpreter_bridge::flatten(&done)
}

fn serve_once(content_type: &str, payload: &[u8]) -> (String, JoinHandle<()>) {
    set_allowed_private_hosts(&["127.0.0.1".to_owned()]);
    let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    )
    .into_bytes();
    response.extend_from_slice(payload);
    let server = thread::spawn(move || {
        let (mut stream, _) = listener.accept().unwrap();
        let mut request = Vec::new();
        while !request.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).unwrap();
            request.push(byte[0]);
        }
        stream.write_all(&response).unwrap();
    });
    (url, server)
}

#[test_case::test_case("markdown", LEGACY_HTML_TYPE, LATIN1_HTML, Ok(LOSSY_MARKDOWN) ; "legacy_html_to_markdown")]
#[test_case::test_case("text", LEGACY_HTML_TYPE, LATIN1_HTML, Ok(LOSSY_TEXT) ; "legacy_html_to_text")]
#[test_case::test_case("text", BINARY_TYPE, BINARY_PAYLOAD, Err(BINARY_ERROR) ; "binary_rejected")]
fn webfetch_decodes_legacy_bytes_for_display(
    format: &str,
    content_type: &str,
    payload: &[u8],
    expected: Result<&str, &str>,
) {
    let (_registry, _host, ctx) = setup();
    let (url, server) = serve_once(content_type, payload);
    let output = run(&ctx, "webfetch", json!({ "url": url, "format": format }));
    server.join().unwrap();
    assert_eq!(
        output.as_deref().map(str::trim).map_err(String::as_str),
        expected
    );
}

#[test_case::test_case("text" ; "stripped_html")]
#[test_case::test_case("html" ; "raw_html")]
fn webfetch_legacy_bytes_stay_within_the_output_budget(format: &str) {
    let (_registry, _host, ctx) = setup();
    let (url, server) = serve_once(LEGACY_HTML_TYPE, &[LATIN1_E_ACUTE; MODEL_BYTES * 2]);
    let output = run(&ctx, "webfetch", json!({ "url": url, "format": format })).unwrap();
    server.join().unwrap();
    assert!(output.contains("[truncated"));
    assert!(output.len() < MODEL_BYTES * 2);
}

#[test]
#[cfg(unix)]
fn programmatic_bash_skips_automatic_rtk_rewriting() {
    if let Some(dir) = env::var_os(RTK_FIXTURE_DIR) {
        let (_registry, _host, mut ctx) = setup();
        ctx.config.rtk = true;
        assert_eq!(
            run(&ctx, "bash", json!({ "command": RTK_COMMAND })).unwrap(),
            "rewritten"
        );
        let log_path = Path::new(&dir).join("calls.txt");
        let before = fs::read_to_string(&log_path).unwrap();
        assert!(before.contains("--version"));
        assert!(before.contains("rewrite"));
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let rtk = dir.path().join("rtk");
    fs::write(
        &rtk,
        r#"#!/bin/sh
printf '%s\n' "$*" >> "$MAKI_TEST_RTK_LOG"
case "$1" in
  --version) printf 'rtk fixture\n' ;;
  rewrite) printf 'printf rewritten\n' ;;
  emit) printf 'explicit\n' ;;
  *) exit 1 ;;
esac
"#,
    )
    .unwrap();
    fs::set_permissions(&rtk, fs::Permissions::from_mode(0o755)).unwrap();
    let mut paths = vec![dir.path().to_owned()];
    paths.extend(env::split_paths(&env::var_os("PATH").unwrap_or_default()));
    let output = Command::new(env::current_exe().unwrap())
        .args([
            "--exact",
            "programmatic_bash_skips_automatic_rtk_rewriting",
            "--nocapture",
        ])
        .env(RTK_FIXTURE_DIR, dir.path())
        .env(RTK_LOG, dir.path().join("calls.txt"))
        .env("PATH", env::join_paths(paths).unwrap())
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
}
