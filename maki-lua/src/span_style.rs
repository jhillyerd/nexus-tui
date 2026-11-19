//! The span styles plugins and the decor pipeline speak: a small wire
//! format rehomed here from the agent crate (D2) so the UI carries no
//! agent dependency.

use serde::Deserialize;
use serde::Serialize;

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub enum SpanStyle {
    #[default]
    Default,
    Named(String),
    Inline(InlineStyle),
}

/// A color a plugin can ask for. `Ansi` and `Default` keep terminal-owned
/// colors symbolic all the way to the renderer, so the UI never has to guess
/// what a given index looks like on the user's terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum SpanColor {
    /// Serializes as `[r, g, b]`, matching sessions written before palette
    /// colors existed.
    Rgb((u8, u8, u8)),
    Ansi(u8),
    /// Serializes as the string `"default"`, the same spelling themes use.
    Default(DefaultColor),
}

/// Gives [`SpanColor::Default`] a distinct serialized shape under
/// `#[serde(untagged)]`, which has no room for a unit variant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DefaultColor {
    Default,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct InlineStyle {
    pub fg: Option<SpanColor>,
    pub bg: Option<SpanColor>,
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub dim: bool,
    pub strikethrough: bool,
    pub reversed: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotSpan {
    pub text: String,
    pub style: SpanStyle,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SnapshotLine {
    pub spans: Vec<SnapshotSpan>,
}

impl SnapshotLine {
    pub fn plain(text: String) -> Self {
        Self {
            spans: vec![SnapshotSpan {
                text,
                style: SpanStyle::Default,
            }],
        }
    }
}

/// The lua-facing parsers for the span wire format, on these rehomed types.
pub mod span_parse {
    use super::{DefaultColor, InlineStyle, SnapshotLine, SnapshotSpan, SpanColor, SpanStyle};
    use crate::api::util::convert::TableExt as _;
    use mlua::Value as LuaValue;

    type LuaResult<T> = Result<T, mlua::Error>;

    pub fn parse_line(arg: &LuaValue) -> LuaResult<SnapshotLine> {
        match arg {
            LuaValue::String(s) => Ok(SnapshotLine::plain(s.to_string_lossy())),
            LuaValue::Table(t) => {
                let mut spans = Vec::new();
                for i in 1..=t.raw_len() {
                    let entry: LuaValue = t.raw_get(i)?;
                    spans.push(parse_span(&entry)?);
                }
                Ok(SnapshotLine { spans })
            }
            _ => Err(mlua::Error::runtime(
                "line argument must be a string or table of spans",
            )),
        }
    }

    fn parse_span(val: &LuaValue) -> LuaResult<SnapshotSpan> {
        let LuaValue::Table(t) = val else {
            return Err(mlua::Error::runtime("span must be a table {text, style?}"));
        };
        let text_val: LuaValue = t.raw_get(1)?;
        let text = match &text_val {
            LuaValue::String(s) => s.to_string_lossy(),
            _ => return Err(mlua::Error::runtime("span[1] must be a string")),
        };
        let style_val: LuaValue = t.raw_get(2)?;
        let style = parse_style(&style_val)?;
        Ok(SnapshotSpan { text, style })
    }

    pub fn parse_style(val: &LuaValue) -> LuaResult<SpanStyle> {
        match val {
            LuaValue::Nil => Ok(SpanStyle::Default),
            v if v.is_null() => Ok(SpanStyle::Default),
            LuaValue::String(s) => {
                let name = s.to_str().map_err(mlua::Error::external)?.to_owned();
                Ok(SpanStyle::Named(name))
            }
            LuaValue::Table(t) => {
                let mut inline = InlineStyle::default();
                if let Ok(LuaValue::String(s)) = t.raw_get::<LuaValue>("fg") {
                    inline.fg = parse_span_color(&s.to_str().map_err(mlua::Error::external)?);
                }
                if let Ok(LuaValue::String(s)) = t.raw_get::<LuaValue>("bg") {
                    inline.bg = parse_span_color(&s.to_str().map_err(mlua::Error::external)?);
                }
                inline.bold = t.opt_bool("bold").unwrap_or(false);
                inline.italic = t.opt_bool("italic").unwrap_or(false);
                inline.underline = t.opt_bool("underline").unwrap_or(false);
                inline.dim = t.opt_bool("dim").unwrap_or(false);
                inline.strikethrough = t.opt_bool("strikethrough").unwrap_or(false);
                inline.reversed = t.opt_bool("reversed").unwrap_or(false);
                Ok(SpanStyle::Inline(inline))
            }
            _ => Err(mlua::Error::runtime(
                "style must be nil, a string name, or a table {fg?, bg?, bold?, ...}",
            )),
        }
    }

    fn parse_span_color(raw: &str) -> Option<SpanColor> {
        if raw == "default" {
            return Some(SpanColor::Default(DefaultColor::Default));
        }
        if let Ok(index) = raw.parse::<u8>() {
            return Some(SpanColor::Ansi(index));
        }
        let raw = raw.strip_prefix('#')?;
        if raw.len() != 6 {
            return None;
        }
        let r = u8::from_str_radix(&raw[0..2], 16).ok()?;
        let g = u8::from_str_radix(&raw[2..4], 16).ok()?;
        let b = u8::from_str_radix(&raw[4..6], 16).ok()?;
        Some(SpanColor::Rgb((r, g, b)))
    }
}
