//! Colours. Every style the screen uses comes from the theme in use: `dark`, `light`, `terminal` (the terminal's
//! own sixteen colours), or one of yours from `[themes.<name>]` in the config, each colour over a built-in base:
//!
//! ```toml
//! theme = "mine"
//!
//! [themes.mine]
//! base = "dark"
//! accent = "#7fb4ff"
//! ```
//!
//! Without `theme`, rho picks `dark` or `light` from the terminal's background when the terminal says
//! (`COLORFGBG`), else `dark`. `/theme` switches; lines already in the scrollback keep the colours they had.

use std::collections::BTreeMap;
use std::sync::RwLock;

use ratatui::style::{Color, Modifier, Style};

/// The colours of one theme. `None` for `dim` dims the text instead of colouring it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Theme {
    /// Body text, and the brightest text: the answer, your messages.
    pub(crate) text: Color,
    pub(crate) bright: Color,
    pub(crate) dim: Option<Color>,
    /// rho's own colour: the prompt, what is selected, what asks for you.
    pub(crate) accent: Color,
    pub(crate) ok: Color,
    pub(crate) warn: Color,
    pub(crate) error: Color,
    /// Reads and searches, skills.
    pub(crate) info: Color,
    /// Subagents and the plan.
    pub(crate) agent: Color,
    /// Behind your messages, the selected row, added and removed lines; rules between parts of the pane.
    pub(crate) band: Color,
    pub(crate) select: Color,
    pub(crate) added: Color,
    pub(crate) removed: Color,
    pub(crate) rule: Color,
}

const fn rgb(hex: u32) -> Color {
    Color::Rgb((hex >> 16) as u8, (hex >> 8) as u8, hex as u8)
}

const DARK: Theme = Theme {
    text: rgb(0xdcd8cf),
    bright: rgb(0xf4f1ea),
    dim: Some(rgb(0x8d8a84)),
    accent: rgb(0xe3a857),
    ok: rgb(0x86c28d),
    warn: rgb(0xe5c07b),
    error: rgb(0xec7a72),
    info: rgb(0x82aee0),
    agent: rgb(0xb9a3e8),
    band: rgb(0x1f2024),
    select: rgb(0x2a2b30),
    added: rgb(0x1b3022),
    removed: rgb(0x3a1f22),
    rule: rgb(0x34353a),
};

const LIGHT: Theme = Theme {
    text: rgb(0x22211d),
    bright: rgb(0x000000),
    dim: Some(rgb(0x66645d)),
    accent: rgb(0x9c4124),
    ok: rgb(0x2f7d3e),
    warn: rgb(0x9a5b00),
    error: rgb(0xb3362f),
    info: rgb(0x2554c7),
    agent: rgb(0x6a45b8),
    band: rgb(0xece8df),
    select: rgb(0xe2ddd1),
    added: rgb(0xe3f1e4),
    removed: rgb(0xf8e1df),
    rule: rgb(0xdcd7cb),
};

const TERMINAL: Theme = Theme {
    text: Color::Reset,
    bright: Color::Reset,
    dim: None,
    accent: Color::Yellow,
    ok: Color::Green,
    warn: Color::Yellow,
    error: Color::Red,
    info: Color::Blue,
    agent: Color::Magenta,
    band: Color::Reset,
    select: Color::DarkGray,
    added: Color::Reset,
    removed: Color::Reset,
    rule: Color::DarkGray,
};

/// The built-in themes, with what each is for.
pub(crate) const BUILT_IN: [(&str, &str); 3] = [
    ("dark", "warm greys on a dark terminal"),
    ("light", "ink on a light terminal"),
    ("terminal", "your terminal's own sixteen colours"),
];

static CURRENT: RwLock<(Theme, String)> = RwLock::new((DARK, String::new()));

/// The theme in use.
pub(crate) fn theme() -> Theme {
    CURRENT.read().map_or(DARK, |current| current.0)
}

/// The name of the theme in use.
pub(crate) fn name() -> String {
    CURRENT
        .read()
        .map(|current| current.1.clone())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "dark".to_owned())
}

/// Whether the terminal says its background is light.
fn light_terminal() -> bool {
    let background = std::env::var("COLORFGBG").unwrap_or_default();
    matches!(background.rsplit(';').next(), Some("7" | "15"))
}

/// Your themes, from `[themes]`: a base and colours over it.
pub(crate) type Themes = BTreeMap<String, BTreeMap<String, String>>;

/// The theme called `name`, built in or yours; no name picks dark or light for the terminal.
pub(crate) fn resolve(name: Option<&str>, yours: &Themes) -> Result<(Theme, String), String> {
    let name = match name {
        Some(name) => name.to_owned(),
        None if light_terminal() => "light".to_owned(),
        None => "dark".to_owned(),
    };
    match name.as_str() {
        "dark" => return Ok((DARK, name)),
        "light" => return Ok((LIGHT, name)),
        "terminal" => return Ok((TERMINAL, name)),
        _ => {}
    }
    let Some(colours) = yours.get(&name) else {
        return Err(format!(
            "no theme {name}: use dark, light, terminal or one under [themes]"
        ));
    };
    let mut theme = match colours.get("base").map(String::as_str) {
        None | Some("dark") => DARK,
        Some("light") => LIGHT,
        Some("terminal") => TERMINAL,
        Some(other) => {
            return Err(format!(
                "theme {name}: base {other} is not dark, light or terminal"
            ));
        }
    };
    for (key, value) in colours {
        if key == "base" {
            continue;
        }
        let colour = parse_colour(value).ok_or_else(|| {
            format!(
                "theme {name}: {key} = \"{value}\" is not a colour like \"#e3a857\" or \"yellow\""
            )
        })?;
        let slot = match key.as_str() {
            "text" => &mut theme.text,
            "bright" => &mut theme.bright,
            "dim" => {
                theme.dim = Some(colour);
                continue;
            }
            "accent" => &mut theme.accent,
            "ok" => &mut theme.ok,
            "warn" => &mut theme.warn,
            "error" => &mut theme.error,
            "info" => &mut theme.info,
            "agent" => &mut theme.agent,
            "band" => &mut theme.band,
            "select" => &mut theme.select,
            "added" => &mut theme.added,
            "removed" => &mut theme.removed,
            "rule" => &mut theme.rule,
            other => return Err(format!("theme {name}: no colour {other}")),
        };
        *slot = colour;
    }
    Ok((theme, name))
}

/// `#rrggbb`, or a terminal colour by name (`yellow`, `bright-blue`, `reset`).
fn parse_colour(text: &str) -> Option<Color> {
    if let Some(hex) = text.strip_prefix('#') {
        return (hex.len() == 6)
            .then(|| u32::from_str_radix(hex, 16).ok())
            .flatten()
            .map(rgb);
    }
    text.to_lowercase()
        .replace(['-', ' ', '_'], "")
        .parse()
        .ok()
}

/// Uses `theme`, named `name`, from now on.
pub(crate) fn set(theme: Theme, name: &str) {
    if let Ok(mut current) = CURRENT.write() {
        *current = (theme, name.to_owned());
    }
}

pub(crate) fn fg(colour: Color) -> Style {
    Style::new().fg(colour)
}

/// Text that matters less: dimmed, or the theme's dim colour.
pub(crate) fn dim() -> Style {
    match theme().dim {
        Some(colour) => fg(colour),
        None => Style::new().add_modifier(Modifier::DIM),
    }
}

pub(crate) fn accent() -> Style {
    fg(theme().accent).add_modifier(Modifier::BOLD)
}

pub(crate) fn bright() -> Style {
    fg(theme().bright)
}

pub(crate) fn ok() -> Style {
    fg(theme().ok)
}

pub(crate) fn warn() -> Style {
    fg(theme().warn)
}

pub(crate) fn error() -> Style {
    fg(theme().error)
}

pub(crate) fn info() -> Style {
    fg(theme().info)
}

pub(crate) fn agent() -> Style {
    fg(theme().agent)
}

/// The selected row of a list.
pub(crate) fn selected() -> Style {
    Style::new().bg(theme().select)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_theme_of_yours_changes_colours_over_its_base() {
        let yours: Themes = BTreeMap::from([(
            "mine".to_string(),
            BTreeMap::from([
                ("base".to_string(), "light".to_string()),
                ("accent".to_string(), "#7fb4ff".to_string()),
                ("ok".to_string(), "bright-green".to_string()),
            ]),
        )]);
        let (theme, name) = resolve(Some("mine"), &yours).unwrap();
        assert_eq!(name, "mine");
        assert_eq!(theme.accent, Color::Rgb(0x7f, 0xb4, 0xff));
        assert_eq!(theme.ok, Color::LightGreen);
        assert_eq!(theme.text, LIGHT.text);
        assert_eq!(resolve(Some("terminal"), &yours).unwrap().0, TERMINAL);
        assert!(resolve(Some("nope"), &yours).is_err());
        let wrong: Themes = BTreeMap::from([(
            "bad".to_string(),
            BTreeMap::from([("accent".to_string(), "#12".to_string())]),
        )]);
        assert!(resolve(Some("bad"), &wrong).unwrap_err().contains("accent"));
    }
}
