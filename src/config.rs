use std::{
    env, fmt, fs, io,
    path::{Path, PathBuf},
    sync::atomic::{AtomicUsize, Ordering},
};

use ratatui::style::Color;
use serde::Deserialize;

static CONFIG_WRITE_COUNTER: AtomicUsize = AtomicUsize::new(0);

const DEFAULT_CONFIG_TEMPLATE: &str = "# p2pmux local configuration\n#\n# display_name is visible to peers.\n# display_name = \"your-name\"\n\n# UI chrome is local to this client and is never shared with session peers.\n[ui]\n# Panes you are not typing into are drawn dimmed, so a glance finds the one you are.\n# Turn it off if your terminal renders reduced intensity too hard to read.\n# dim_unfocused_panes = true\n\n[ui.theme]\n# Named colors: white, yellow, gray, dark_gray\n# Hex colors: #RRGGBB\n#\n# footer_background = \"#1e1e1e\"\n# footer_muted = \"white\"\n# footer_accent = \"#dc322f\"\n# footer_orange = \"#ff7846\"\n# tab_active_background = \"#dc322f\"\n# tab_foreground = \"white\"\n# tab_separator = \"dark_gray\"\n# copy_feedback_accent = \"#ff4500\"\n# agent_overlay_chrome = \"#ff4500\"\n# agent_overlay_selected_background = \"#2a2a2a\"\n# agent_overlay_muted = \"#919db4\"\n# agent_overlay_warm = \"#ffb84d\"\n# agent_overlay_attention = \"#ffb000\"\n# agent_overlay_error = \"#dc322f\"\n# agent_overlay_foreground = \"white\"\n# agent_overlay_secondary = \"gray\"\n# pane_border_free_focused = \"white\"\n# pane_border_unknown_focused = \"yellow\"\n# pane_border_chord_focused = \"#ff1a1a\"\n# pane_border_hovered = \"gray\"\n# pane_border_idle = \"dark_gray\"\n# pane_border_remote_control = \"#ff4500\"\n#\n# One color per member, in join order, identifying who is watching which tab and pane,\n# and coloring the border of any pane that member is driving.\n# List up to 8; the slots you leave out keep their built-in color.\n# member_colors = [\"#ff6a13\", \"#7ed67e\", \"#f06292\", \"#7986cb\", \"#4db6ac\", \"#ba68c8\", \"#c0ca33\", \"#90a4ae\"]\n";

#[derive(Debug)]
pub enum ConfigError {
    MissingHome,
    InvalidName(&'static str),
    InvalidColor { key: &'static str, value: String },
    Io(io::Error),
    Parse(toml::de::Error),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::MissingHome => f.write_str("could not resolve config directory"),
            Self::InvalidName(message) => f.write_str(message),
            Self::InvalidColor { key, value } => write!(f, "invalid color for {key}: {value}"),
            Self::Io(error) => error.fmt(f),
            Self::Parse(error) => error.fmt(f),
        }
    }
}

impl std::error::Error for ConfigError {}

impl From<io::Error> for ConfigError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Config {
    pub display_name: Option<String>,
    pub ui: UiConfig,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiConfig {
    pub theme: UiTheme,
    /// Whether panes that do not have focus are drawn dimmed.
    ///
    /// On by default: with four panes open the question a glance has to answer
    /// is "which one am I typing into", and a border color a few cells wide is
    /// a poor answer next to three screenfuls of equally bright text.
    ///
    /// A setting rather than a constant because it is the one piece of chrome
    /// that touches every cell a user reads. Terminals disagree about how hard
    /// they render reduced intensity, and on the ones that lean into it a busy
    /// pane can end up genuinely hard to read while you are watching it work.
    pub dim_unfocused_panes: bool,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: UiTheme::default(),
            dim_unfocused_panes: true,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct UiTheme {
    pub footer_background: Color,
    pub footer_muted: Color,
    pub footer_accent: Color,
    pub footer_orange: Color,
    pub tab_active_background: Color,
    pub tab_foreground: Color,
    pub tab_separator: Color,
    pub copy_feedback_accent: Color,
    pub agent_overlay_chrome: Color,
    pub agent_overlay_selected_background: Color,
    pub agent_overlay_muted: Color,
    pub agent_overlay_warm: Color,
    pub agent_overlay_attention: Color,
    pub agent_overlay_error: Color,
    pub agent_overlay_foreground: Color,
    pub agent_overlay_secondary: Color,
    pub pane_border_free_focused: Color,
    pub pane_border_unknown_focused: Color,
    pub pane_border_chord_focused: Color,
    pub pane_border_hovered: Color,
    pub pane_border_idle: Color,
    pub pane_border_remote_control: Color,
    /// One color per member slot, used to identify who is where and who is driving
    /// what: a pane held by a member takes that member's color for its border. No entry
    /// here may collide with `pane_border_remote_control`, which is what a pane falls
    /// back to when its controller has left the member list and has no slot left.
    pub member_colors: [Color; MEMBER_COLOR_SLOTS],
}

/// One color per admitted member, so a live session never has to recycle a slot.
/// Kept in step with `crate::layout::MAX_MEMBERS` by a unit test.
pub const MEMBER_COLOR_SLOTS: usize = 8;

/// Slot one is a vivid red-orange so the session host stands out; the rest are cool hues,
/// which keeps them clear of the colors that mean something else: the active tab is
/// `#dc322f`, a departed controller's pane border is `#ff4500`, and a chord-armed pane
/// border is `#ff1a1a`. Slot one stays a readable step off all three. Every color here
/// also carries the member's initial at the call site, so the palette never has to
/// survive on hue alone.
const DEFAULT_MEMBER_COLORS: [Color; MEMBER_COLOR_SLOTS] = [
    Color::Rgb(255, 106, 19),
    Color::Rgb(126, 214, 126),
    Color::Rgb(240, 98, 146),
    Color::Rgb(121, 134, 203),
    Color::Rgb(77, 182, 172),
    Color::Rgb(186, 104, 200),
    Color::Rgb(192, 202, 51),
    Color::Rgb(144, 164, 174),
];

impl Default for UiTheme {
    fn default() -> Self {
        Self {
            footer_background: Color::Rgb(30, 30, 30),
            footer_muted: Color::White,
            footer_accent: Color::Rgb(220, 50, 47),
            footer_orange: Color::Rgb(255, 120, 70),
            tab_active_background: Color::Rgb(220, 50, 47),
            tab_foreground: Color::White,
            tab_separator: Color::DarkGray,
            copy_feedback_accent: Color::Rgb(255, 69, 0),
            agent_overlay_chrome: Color::Rgb(255, 69, 0),
            agent_overlay_selected_background: Color::Rgb(42, 42, 42),
            agent_overlay_muted: Color::Rgb(145, 157, 180),
            agent_overlay_warm: Color::Rgb(255, 184, 77),
            agent_overlay_attention: Color::Rgb(255, 176, 0),
            agent_overlay_error: Color::Rgb(220, 50, 47),
            agent_overlay_foreground: Color::White,
            agent_overlay_secondary: Color::Gray,
            pane_border_free_focused: Color::White,
            pane_border_unknown_focused: Color::Yellow,
            pane_border_chord_focused: Color::Rgb(255, 26, 26),
            pane_border_hovered: Color::Gray,
            pane_border_idle: Color::DarkGray,
            pane_border_remote_control: Color::Rgb(255, 69, 0),
            member_colors: DEFAULT_MEMBER_COLORS,
        }
    }
}

#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    display_name: Option<String>,
    #[serde(default)]
    ui: UiConfigFile,
}

#[derive(Debug, Default, Deserialize)]
struct UiConfigFile {
    #[serde(default)]
    theme: UiThemeFile,
    dim_unfocused_panes: Option<bool>,
}

#[derive(Debug, Default, Deserialize)]
struct UiThemeFile {
    footer_background: Option<String>,
    footer_muted: Option<String>,
    footer_accent: Option<String>,
    footer_orange: Option<String>,
    tab_active_background: Option<String>,
    tab_foreground: Option<String>,
    tab_separator: Option<String>,
    copy_feedback_accent: Option<String>,
    agent_overlay_chrome: Option<String>,
    agent_overlay_selected_background: Option<String>,
    agent_overlay_muted: Option<String>,
    agent_overlay_warm: Option<String>,
    agent_overlay_attention: Option<String>,
    agent_overlay_error: Option<String>,
    agent_overlay_foreground: Option<String>,
    agent_overlay_secondary: Option<String>,
    pane_border_free_focused: Option<String>,
    pane_border_unknown_focused: Option<String>,
    pane_border_chord_focused: Option<String>,
    pane_border_hovered: Option<String>,
    pane_border_idle: Option<String>,
    pane_border_remote_control: Option<String>,
    member_colors: Option<Vec<String>>,
}

pub fn config_path() -> Result<PathBuf, ConfigError> {
    let base = env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
        .ok_or(ConfigError::MissingHome)?;
    Ok(base.join("p2pmux").join("config.toml"))
}

pub fn validate_display_name(name: &str) -> Result<String, ConfigError> {
    let name = name.trim();
    if name.is_empty() {
        return Err(ConfigError::InvalidName("display name must not be empty"));
    }
    if name.chars().count() > 32 {
        return Err(ConfigError::InvalidName(
            "display name must be at most 32 characters",
        ));
    }
    if name.chars().any(char::is_control) {
        return Err(ConfigError::InvalidName(
            "display name must not contain control characters",
        ));
    }
    Ok(name.to_owned())
}

pub fn load_from(path: &Path) -> Result<Option<String>, ConfigError> {
    Ok(load_config_from(path)?.display_name)
}

pub fn load_config_from(path: &Path) -> Result<Config, ConfigError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Config::default()),
        Err(error) => return Err(error.into()),
    };
    let config: ConfigFile = toml::from_str(&text).map_err(ConfigError::Parse)?;
    let display_name = config
        .display_name
        .map(|name| validate_display_name(&name))
        .transpose()?;
    let mut theme = UiTheme::default();
    apply_color(
        &mut theme.footer_background,
        config.ui.theme.footer_background,
        "footer_background",
    )?;
    apply_color(
        &mut theme.footer_muted,
        config.ui.theme.footer_muted,
        "footer_muted",
    )?;
    apply_color(
        &mut theme.footer_accent,
        config.ui.theme.footer_accent,
        "footer_accent",
    )?;
    apply_color(
        &mut theme.footer_orange,
        config.ui.theme.footer_orange,
        "footer_orange",
    )?;
    apply_color(
        &mut theme.tab_active_background,
        config.ui.theme.tab_active_background,
        "tab_active_background",
    )?;
    apply_color(
        &mut theme.tab_foreground,
        config.ui.theme.tab_foreground,
        "tab_foreground",
    )?;
    apply_color(
        &mut theme.tab_separator,
        config.ui.theme.tab_separator,
        "tab_separator",
    )?;
    apply_color(
        &mut theme.copy_feedback_accent,
        config.ui.theme.copy_feedback_accent,
        "copy_feedback_accent",
    )?;
    apply_color(
        &mut theme.agent_overlay_chrome,
        config.ui.theme.agent_overlay_chrome,
        "agent_overlay_chrome",
    )?;
    apply_color(
        &mut theme.agent_overlay_selected_background,
        config.ui.theme.agent_overlay_selected_background,
        "agent_overlay_selected_background",
    )?;
    apply_color(
        &mut theme.agent_overlay_muted,
        config.ui.theme.agent_overlay_muted,
        "agent_overlay_muted",
    )?;
    apply_color(
        &mut theme.agent_overlay_warm,
        config.ui.theme.agent_overlay_warm,
        "agent_overlay_warm",
    )?;
    apply_color(
        &mut theme.agent_overlay_attention,
        config.ui.theme.agent_overlay_attention,
        "agent_overlay_attention",
    )?;
    apply_color(
        &mut theme.agent_overlay_error,
        config.ui.theme.agent_overlay_error,
        "agent_overlay_error",
    )?;
    apply_color(
        &mut theme.agent_overlay_foreground,
        config.ui.theme.agent_overlay_foreground,
        "agent_overlay_foreground",
    )?;
    apply_color(
        &mut theme.agent_overlay_secondary,
        config.ui.theme.agent_overlay_secondary,
        "agent_overlay_secondary",
    )?;
    apply_color(
        &mut theme.pane_border_free_focused,
        config.ui.theme.pane_border_free_focused,
        "pane_border_free_focused",
    )?;
    apply_color(
        &mut theme.pane_border_unknown_focused,
        config.ui.theme.pane_border_unknown_focused,
        "pane_border_unknown_focused",
    )?;
    apply_color(
        &mut theme.pane_border_chord_focused,
        config.ui.theme.pane_border_chord_focused,
        "pane_border_chord_focused",
    )?;
    apply_color(
        &mut theme.pane_border_hovered,
        config.ui.theme.pane_border_hovered,
        "pane_border_hovered",
    )?;
    apply_color(
        &mut theme.pane_border_idle,
        config.ui.theme.pane_border_idle,
        "pane_border_idle",
    )?;
    apply_color(
        &mut theme.pane_border_remote_control,
        config.ui.theme.pane_border_remote_control,
        "pane_border_remote_control",
    )?;
    apply_member_colors(&mut theme.member_colors, config.ui.theme.member_colors)?;
    Ok(Config {
        display_name,
        ui: UiConfig {
            theme,
            dim_unfocused_panes: config.ui.dim_unfocused_panes.unwrap_or(true),
        },
    })
}

fn apply_color(
    color: &mut Color,
    value: Option<String>,
    key: &'static str,
) -> Result<(), ConfigError> {
    if let Some(value) = value {
        *color = parse_color(&value).ok_or(ConfigError::InvalidColor { key, value })?;
    }
    Ok(())
}

/// Override member colors from the front of the list, so a short list keeps the
/// built-in colors for the slots it does not mention -- the same "omitted keys keep
/// today's colors" rule every other theme key follows.
fn apply_member_colors(
    colors: &mut [Color; MEMBER_COLOR_SLOTS],
    values: Option<Vec<String>>,
) -> Result<(), ConfigError> {
    let Some(values) = values else {
        return Ok(());
    };
    if values.len() > MEMBER_COLOR_SLOTS {
        return Err(ConfigError::InvalidColor {
            key: "member_colors",
            value: format!("at most {MEMBER_COLOR_SLOTS} colors, got {}", values.len()),
        });
    }
    for (slot, value) in colors.iter_mut().zip(values) {
        *slot = parse_color(&value).ok_or(ConfigError::InvalidColor {
            key: "member_colors",
            value,
        })?;
    }
    Ok(())
}

fn parse_color(value: &str) -> Option<Color> {
    match value {
        "white" => Some(Color::White),
        "yellow" => Some(Color::Yellow),
        "gray" => Some(Color::Gray),
        "dark_gray" => Some(Color::DarkGray),
        _ => {
            let hex = value.strip_prefix('#')?;
            if hex.len() != 6 {
                return None;
            }
            Some(Color::Rgb(
                u8::from_str_radix(&hex[0..2], 16).ok()?,
                u8::from_str_radix(&hex[2..4], 16).ok()?,
                u8::from_str_radix(&hex[4..6], 16).ok()?,
            ))
        }
    }
}

pub fn save_to(path: &Path, name: &str) -> Result<String, ConfigError> {
    let display_name = validate_display_name(name)?;
    let mut document = match fs::read_to_string(path) {
        Ok(text) => text
            .parse::<toml_edit::DocumentMut>()
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?,
        Err(error) if error.kind() == io::ErrorKind::NotFound => toml_edit::DocumentMut::new(),
        Err(error) => return Err(error.into()),
    };
    document["display_name"] = toml_edit::value(display_name.as_str());
    atomic_write(path, &document.to_string())?;
    Ok(display_name)
}

fn atomic_write(path: &Path, text: &str) -> Result<(), ConfigError> {
    let parent = path.parent().ok_or(ConfigError::MissingHome)?;
    fs::create_dir_all(parent)?;
    let file_name = path.file_name().ok_or(ConfigError::MissingHome)?;
    let temporary = parent.join(format!(
        ".{}.{}.{}",
        file_name.to_string_lossy(),
        std::process::id(),
        CONFIG_WRITE_COUNTER.fetch_add(1, Ordering::Relaxed),
    ));
    fs::write(&temporary, text)?;
    if let Err(error) = fs::rename(&temporary, path) {
        let _ = fs::remove_file(&temporary);
        return Err(error.into());
    }
    Ok(())
}

pub fn load() -> Result<Option<String>, ConfigError> {
    load_from(&config_path()?)
}

pub fn load_config() -> Result<Config, ConfigError> {
    load_config_from(&config_path()?)
}

pub fn save(name: &str) -> Result<String, ConfigError> {
    save_to(&config_path()?, name)
}

pub fn init() -> Result<(), ConfigError> {
    init_to(&config_path()?)
}

pub fn init_to(path: &Path) -> Result<(), ConfigError> {
    if path.exists() {
        return Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            format!("config file already exists: {}", path.display()),
        )
        .into());
    }
    atomic_write(path, DEFAULT_CONFIG_TEMPLATE)
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        sync::atomic::{AtomicU64, Ordering},
        time::{SystemTime, UNIX_EPOCH},
    };

    use ratatui::style::Color;

    use super::*;

    fn temp_config_path() -> PathBuf {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock")
            .as_nanos();
        let seq = NEXT.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir()
            .join(format!("p2pmux-config-{unique}-{seq}"))
            .join("config.toml")
    }

    #[test]
    fn absent_file_uses_defaults() {
        let path = temp_config_path();
        assert_eq!(load_config_from(&path).unwrap(), Config::default());
    }

    #[test]
    fn partial_theme_override_keeps_other_defaults() {
        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[ui.theme]\nfooter_background = \"#010203\"\n").unwrap();

        let config = load_config_from(&path).unwrap();
        assert_eq!(config.ui.theme.footer_background, Color::Rgb(1, 2, 3));
        assert_eq!(
            config.ui.theme.footer_accent,
            UiTheme::default().footer_accent
        );

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn default_agent_overlay_selection_background_is_soft_gray() {
        assert_eq!(
            UiTheme::default().agent_overlay_selected_background,
            Color::Rgb(42, 42, 42)
        );
        assert!(
            DEFAULT_CONFIG_TEMPLATE.contains("agent_overlay_selected_background = \"#2a2a2a\"")
        );
    }

    #[test]
    fn chord_focused_pane_border_defaults_to_vivid_red_and_is_configurable() {
        assert_eq!(
            UiTheme::default().pane_border_chord_focused,
            Color::Rgb(255, 26, 26)
        );
        assert!(DEFAULT_CONFIG_TEMPLATE.contains("pane_border_chord_focused = \"#ff1a1a\""));

        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "[ui.theme]\npane_border_chord_focused = \"#010203\"\n",
        )
        .unwrap();

        assert_eq!(
            load_config_from(&path)
                .unwrap()
                .ui
                .theme
                .pane_border_chord_focused,
            Color::Rgb(1, 2, 3)
        );

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn member_color_slots_cover_every_admissible_member() {
        assert_eq!(MEMBER_COLOR_SLOTS, crate::layout::MAX_MEMBERS);
    }

    #[test]
    fn member_colors_override_from_the_front_and_keep_the_rest() {
        assert!(DEFAULT_CONFIG_TEMPLATE.contains("# member_colors = [\"#ff6a13\""));

        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "[ui.theme]\nmember_colors = [\"#010203\", \"yellow\"]\n",
        )
        .unwrap();

        let colors = load_config_from(&path).unwrap().ui.theme.member_colors;
        assert_eq!(colors[0], Color::Rgb(1, 2, 3));
        assert_eq!(colors[1], Color::Yellow);
        assert_eq!(
            colors[2],
            UiTheme::default().member_colors[2],
            "slots the file leaves out keep their built-in color"
        );

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn member_colors_reject_an_oversized_list_and_a_bad_entry() {
        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        let too_many = ["\"white\""; MEMBER_COLOR_SLOTS + 1].join(", ");
        fs::write(&path, format!("[ui.theme]\nmember_colors = [{too_many}]\n")).unwrap();
        assert!(matches!(
            load_config_from(&path),
            Err(ConfigError::InvalidColor {
                key: "member_colors",
                ..
            })
        ));

        fs::write(&path, "[ui.theme]\nmember_colors = [\"#gggggg\"]\n").unwrap();
        assert!(matches!(
            load_config_from(&path),
            Err(ConfigError::InvalidColor {
                key: "member_colors",
                ..
            })
        ));

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn named_and_hex_colors_parse() {
        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "[ui.theme]\nfooter_muted = \"white\"\nfooter_accent = \"#aBcDeF\"\n",
        )
        .unwrap();

        let config = load_config_from(&path).unwrap();
        assert_eq!(config.ui.theme.footer_muted, Color::White);
        assert_eq!(config.ui.theme.footer_accent, Color::Rgb(171, 205, 239));

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn invalid_color_names_its_key() {
        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "[ui.theme]\nfooter_background = \"blue\"\n").unwrap();

        let error = load_config_from(&path).unwrap_err();
        assert!(error.to_string().contains("footer_background"));

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn display_name_validation_still_works() {
        assert_eq!(validate_display_name("  pelazas  ").unwrap(), "pelazas");
        assert!(validate_display_name("\n").is_err());
    }

    #[test]
    fn save_name_preserves_theme_and_comments() {
        let path = temp_config_path();
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(
            &path,
            "# keep this comment\n[ui.theme]\nfooter_background = \"#010203\"\nunknown = \"value\"\n",
        )
        .unwrap();

        save_to(&path, "pelazas").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# keep this comment"));
        assert!(text.contains("[ui.theme]"));
        assert!(text.contains("footer_background = \"#010203\""));
        assert!(text.contains("unknown = \"value\""));

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }

    #[test]
    fn init_creates_default_template_only_when_missing() {
        let path = temp_config_path();

        init_to(&path).unwrap();
        assert_eq!(load_config_from(&path).unwrap(), Config::default());
        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("# footer_background"));

        let error = init_to(&path).unwrap_err();
        assert_eq!(
            error.to_string(),
            format!("config file already exists: {}", path.display())
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), text);

        fs::remove_dir_all(path.parent().unwrap()).unwrap();
    }
}
