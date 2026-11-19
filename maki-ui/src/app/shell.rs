//! The `!` shell escape: run a line locally, print its output. No transcript
//! plumbing — the server never sees it.

/// A `!` (or `!`-in-visible-mode) prefix on an input line.
pub(crate) struct ShellPrefix {
    /// From the line's start through the bang and its spaces, so a rule can
    /// mark the command from here.
    pub prefix_len: usize,
    pub command: String,
    pub visible: bool,
}

pub(crate) fn parse_shell_prefix(text: &str) -> Option<ShellPrefix> {
    let (sigil_len, visible) = if text.starts_with("!!") {
        (2, false)
    } else if text.starts_with('!') {
        (1, true)
    } else {
        return None;
    };
    let rest = &text[sigil_len..];
    let prefix_len = if rest.starts_with(' ') {
        sigil_len + 1
    } else {
        sigil_len
    };
    let command = rest.trim();
    if command.is_empty() {
        return None;
    }
    Some(ShellPrefix {
        prefix_len,
        command: command.to_owned(),
        visible,
    })
}

#[cfg(test)]
mod tests {
    use super::parse_shell_prefix;
    use test_case::test_case;

    #[test_case("! ls",                    Some("ls".to_owned())        ; "simple")]
    #[test_case("!  ls -la  ",             Some("ls -la".to_owned())    ; "trims")]
    #[test_case("!! ls",                   Some("ls".to_owned())        ; "hidden")]
    #[test_case("ls",                      None                         ; "no_bang")]
    #[test_case("!",                       None                         ; "empty_command")]
    fn parse_prefixes(input: &str, expected: Option<String>) {
        let parsed = parse_shell_prefix(input).map(|p| p.command);
        assert_eq!(parsed, expected);
    }
}
