use std::collections::BTreeMap;
use std::path::PathBuf;

#[derive(Clone, Copy)]
pub struct PanelVisibility {
    pub context: bool,
    pub quota: bool,
    pub tokens: bool,
    pub projects: bool,
    pub ports: bool,
    pub sessions: bool,
    pub mcp: bool,
}

impl Default for PanelVisibility {
    fn default() -> Self {
        Self {
            context: true,
            quota: true,
            tokens: true,
            projects: true,
            ports: true,
            sessions: true,
            mcp: true,
        }
    }
}

/// Parsed data for one user-defined `[themes.<name>]` config section.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CustomThemeConfig {
    pub name: String,
    pub values: BTreeMap<String, String>,
}

pub struct AppConfig {
    pub theme: String,
    /// Agent CLI names to exclude from the TUI (e.g. ["codex"] to hide Codex).
    /// Matched case-insensitively against each collector's agent_cli identifier.
    pub hidden_agents: Vec<String>,
    /// Additional Claude config directories to scan for sessions.
    /// Useful for multi-profile setups that use separate CLAUDE_CONFIG_DIR roots.
    pub claude_config_dirs: Vec<PathBuf>,
    pub panels: PanelVisibility,
    /// UI language override. Empty string means auto-detect from `LANG`.
    /// Recognized values: "en", "zh" (anything starting with "zh" maps to Simplified Chinese).
    pub language: String,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            theme: "btop".to_string(),
            hidden_agents: Vec::new(),
            claude_config_dirs: Vec::new(),
            panels: PanelVisibility::default(),
            language: String::new(),
        }
    }
}

#[derive(Default)]
pub(crate) struct LoadedConfig {
    pub(crate) config: AppConfig,
    pub(crate) custom_themes: Vec<CustomThemeConfig>,
}

const MAX_CUSTOM_THEME_NAME_LEN: usize = 64;

#[derive(Debug, PartialEq, Eq)]
enum ConfigTable {
    Root,
    CustomTheme(String),
    Unsupported,
}

/// Custom theme names are intentionally conservative: ASCII alphanumeric
/// first character, followed by ASCII alphanumeric, underscore, or hyphen.
/// This keeps names safe for TOML table suffixes and command-line use.
pub fn is_valid_custom_theme_name(name: &str) -> bool {
    if name.len() > MAX_CUSTOM_THEME_NAME_LEN {
        return false;
    }

    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };

    first.is_ascii_alphanumeric()
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
}

fn config_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("abtop").join("config.toml"))
}

pub fn load_config() -> AppConfig {
    load_config_with_custom_themes().config
}

pub(crate) fn load_config_with_custom_themes() -> LoadedConfig {
    let path = match config_path() {
        Some(p) => p,
        None => return LoadedConfig::default(),
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(_) => return LoadedConfig::default(),
    };

    parse_config_body(&content)
}

fn parse_config_body(content: &str) -> LoadedConfig {
    let mut loaded = LoadedConfig::default();
    let mut current_table = ConfigTable::Root;

    for raw_line in content.lines() {
        let line = raw_line.trim();
        if line.starts_with('#') || line.is_empty() {
            continue;
        }

        if let Some(section) = parse_section_header(line) {
            current_table = parse_config_table(section);
            continue;
        }

        if let Some((key, val)) = line.split_once('=') {
            let key = key.trim();
            let val = strip_inline_comment(val.trim());

            match &current_table {
                ConfigTable::CustomTheme(name) => {
                    if !key.is_empty() {
                        custom_theme_mut(&mut loaded.custom_themes, name)
                            .values
                            .insert(key.to_string(), val.to_string());
                    }
                    continue;
                }
                ConfigTable::Unsupported => continue,
                ConfigTable::Root => {}
            }

            if key == "hidden_agents" {
                loaded.config.hidden_agents = parse_string_array(val);
                continue;
            }
            if key == "claude_config_dirs" {
                loaded.config.claude_config_dirs = parse_path_array(val);
                continue;
            }
            let val = val.trim_matches('\"').trim_matches('\'');
            match key {
                "theme" => loaded.config.theme = val.to_string(),
                "language" => loaded.config.language = val.to_string(),
                "show_context" => loaded.config.panels.context = parse_bool(val).unwrap_or(true),
                "show_quota" => loaded.config.panels.quota = parse_bool(val).unwrap_or(true),
                "show_tokens" => loaded.config.panels.tokens = parse_bool(val).unwrap_or(true),
                "show_projects" => loaded.config.panels.projects = parse_bool(val).unwrap_or(true),
                "show_ports" => loaded.config.panels.ports = parse_bool(val).unwrap_or(true),
                "show_sessions" => loaded.config.panels.sessions = parse_bool(val).unwrap_or(true),
                "show_mcp" => loaded.config.panels.mcp = parse_bool(val).unwrap_or(true),
                _ => {}
            }
        }
    }
    loaded
}

fn parse_config_table(section: &str) -> ConfigTable {
    match section.strip_prefix("themes.") {
        Some(name) if is_valid_custom_theme_name(name) => {
            ConfigTable::CustomTheme(name.to_string())
        }
        _ => ConfigTable::Unsupported,
    }
}

fn parse_section_header(line: &str) -> Option<&str> {
    let line = strip_inline_comment(line);
    let inner = line.strip_prefix('[')?.strip_suffix(']')?.trim();
    if inner.is_empty() {
        None
    } else {
        Some(inner)
    }
}

fn custom_theme_mut<'a>(
    themes: &'a mut Vec<CustomThemeConfig>,
    name: &str,
) -> &'a mut CustomThemeConfig {
    if let Some(pos) = themes.iter().position(|theme| theme.name == name) {
        return &mut themes[pos];
    }
    themes.push(CustomThemeConfig {
        name: name.to_string(),
        values: BTreeMap::new(),
    });
    themes.last_mut().expect("just pushed custom theme")
}

fn strip_inline_comment(raw: &str) -> &str {
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for (idx, ch) in raw.char_indices() {
        match ch {
            '\\' if in_double => {
                escaped = !escaped;
                continue;
            }
            '"' if !in_single && !escaped => in_double = !in_double,
            '\'' if !in_double => in_single = !in_single,
            '#' if !in_single && !in_double => return raw[..idx].trim(),
            _ => {}
        }
        escaped = false;
    }

    raw.trim()
}

fn parse_bool(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

/// Parse a simple one-line TOML string array like `["a", "b"]`.
/// Returns an empty Vec for malformed input to keep config loading infallible.
fn parse_string_array(raw: &str) -> Vec<String> {
    let trimmed = raw.trim();
    let Some(inner) = trimmed.strip_prefix('[').and_then(|s| s.strip_suffix(']')) else {
        return Vec::new();
    };
    inner
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim_matches('\'').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn parse_path_array(raw: &str) -> Vec<PathBuf> {
    parse_string_array(raw)
        .into_iter()
        .map(|s| expand_home_path(&s))
        .collect()
}

fn expand_home_path(raw: &str) -> PathBuf {
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(raw)
}

pub fn save_theme(name: &str) -> Result<(), String> {
    write_with_updates(&[("theme", theme_name_value(name)?)])
}

fn theme_name_value(name: &str) -> Result<String, String> {
    if !is_valid_custom_theme_name(name) {
        return Err(format!(
            "invalid theme name '{}': use 1-64 ASCII letters, digits, underscores, or hyphens; start with a letter or digit",
            name
        ));
    }

    Ok(toml_basic_string(name))
}

fn toml_basic_string(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\u{:04X}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

pub fn save_panel_visibility(panels: &PanelVisibility) -> Result<(), String> {
    write_with_updates(&[
        ("show_context", panels.context.to_string()),
        ("show_quota", panels.quota.to_string()),
        ("show_tokens", panels.tokens.to_string()),
        ("show_projects", panels.projects.to_string()),
        ("show_ports", panels.ports.to_string()),
        ("show_sessions", panels.sessions.to_string()),
        ("show_mcp", panels.mcp.to_string()),
    ])
}

/// Read the config, replace or append each (key, value) pair, write it back.
/// Lines that don't match any key are preserved verbatim so unknown keys and
/// comments survive saves driven by unrelated parts of the UI.
fn write_with_updates(updates: &[(&str, String)]) -> Result<(), String> {
    let path = config_path().ok_or("no config directory")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e.to_string()),
    };
    let new_content = rewrite_kv_lines(&content, updates);
    std::fs::write(&path, new_content).map_err(|e| e.to_string())
}

/// Rewrite (or append) the listed `key = value` lines in a config body.
/// Every other line is preserved verbatim so keys set by the user or by a
/// different save_* helper survive.
fn rewrite_kv_lines(content: &str, updates: &[(&str, String)]) -> String {
    let mut found = vec![false; updates.len()];
    let mut out: Vec<String> = Vec::new();
    let mut in_section = false;
    let mut appended_missing = false;

    for line in content.lines() {
        let starts_section = parse_section_header(line.trim()).is_some();
        if starts_section && !appended_missing {
            append_missing_updates(&mut out, updates, &found);
            appended_missing = true;
        }

        let line_key = if in_section {
            None
        } else {
            line.split_once('=').map(|(k, _)| k.trim().to_string())
        };
        let mut replaced = false;
        if let Some(key) = line_key {
            if let Some(idx) = updates.iter().position(|(k, _)| *k == key) {
                out.push(format!("{} = {}", updates[idx].0, updates[idx].1));
                found[idx] = true;
                replaced = true;
            }
        }
        if !replaced {
            out.push(line.to_string());
        }
        if starts_section {
            in_section = true;
        }
    }

    if !appended_missing {
        append_missing_updates(&mut out, updates, &found);
    }

    out.join("\n") + "\n"
}

fn append_missing_updates(out: &mut Vec<String>, updates: &[(&str, String)], found: &[bool]) {
    for (idx, (k, v)) in updates.iter().enumerate() {
        if !found[idx] {
            out.push(format!("{} = {}", k, v));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_string_array_basic() {
        assert_eq!(parse_string_array(r#"["codex"]"#), vec!["codex"]);
        assert_eq!(
            parse_string_array(r#"["codex", "claude"]"#),
            vec!["codex", "claude"]
        );
    }

    #[test]
    fn parse_string_array_quote_styles_and_whitespace() {
        assert_eq!(
            parse_string_array(r#"[ 'codex' , "claude" ]"#),
            vec!["codex", "claude"]
        );
    }

    #[test]
    fn parse_string_array_empty_and_malformed() {
        assert!(parse_string_array("[]").is_empty());
        assert!(parse_string_array("not an array").is_empty());
        assert!(parse_string_array(r#"["a",,]"#)
            .iter()
            .all(|s| !s.is_empty()));
    }

    #[test]
    fn parse_path_array_expands_home_relative_entries() {
        let home = dirs::home_dir().unwrap();
        let paths = parse_path_array(r#"["~/.claude-personal", "/tmp/.claude-work"]"#);

        assert_eq!(paths[0], home.join(".claude-personal"));
        assert_eq!(paths[1], PathBuf::from("/tmp/.claude-work"));
    }

    #[test]
    fn app_config_struct_literal_fields_remain_source_compatible() {
        let cfg = AppConfig {
            theme: "btop".to_string(),
            hidden_agents: Vec::new(),
            claude_config_dirs: Vec::new(),
            panels: PanelVisibility::default(),
            language: String::new(),
        };

        assert_eq!(cfg.theme, "btop");
    }

    #[test]
    fn parse_config_body_loads_claude_config_dirs() {
        let home = dirs::home_dir().unwrap();
        let cfg = parse_config_body(r#"claude_config_dirs = ["~/.claude-personal"]"#).config;

        assert_eq!(cfg.claude_config_dirs, vec![home.join(".claude-personal")]);
    }

    #[test]
    fn parse_config_body_loads_custom_theme_sections() {
        let loaded = parse_config_body(
            r##"
theme = "midnight" # active custom theme
[themes.midnight]
base = "dracula"
main_bg = "#101010"
cpu_grad = ["#000000", "#111111", "#ffffff"]
"##,
        );

        assert_eq!(loaded.config.theme, "midnight");
        assert_eq!(loaded.custom_themes.len(), 1);
        assert_eq!(loaded.custom_themes[0].name, "midnight");
        assert_eq!(
            loaded.custom_themes[0]
                .values
                .get("main_bg")
                .map(String::as_str),
            Some("\"#101010\"")
        );
        assert_eq!(
            loaded.custom_themes[0]
                .values
                .get("cpu_grad")
                .map(String::as_str),
            Some("[\"#000000\", \"#111111\", \"#ffffff\"]")
        );
    }

    #[test]
    fn parse_config_body_ignores_root_keys_inside_unsupported_sections() {
        let cfg = parse_config_body(
            r#"
theme = "dracula"
hidden_agents = ["claude"]
language = "en"
show_context = false

[metadata]
theme = "nord"
hidden_agents = ["codex"]
language = "zh"
show_context = true

[other]
show_quota = false
show_tokens = false
"#,
        )
        .config;

        assert_eq!(cfg.theme, "dracula");
        assert_eq!(cfg.hidden_agents, vec!["claude"]);
        assert_eq!(cfg.language, "en");
        assert!(!cfg.panels.context);
        assert!(cfg.panels.quota);
        assert!(cfg.panels.tokens);
    }

    #[test]
    fn parse_config_body_parses_custom_themes_after_unsupported_sections() {
        let loaded = parse_config_body(
            r##"
[metadata]
theme = "nord"
main_bg = "#badbad"

[themes.midnight]
base = "dracula"
main_bg = "#101010"

[metadata.more]
title = "#ffffff"

[themes.dawn]
main_fg = "#eeeeee"
"##,
        );

        assert_eq!(loaded.config.theme, "btop");
        assert_eq!(loaded.custom_themes.len(), 2);
        assert_eq!(loaded.custom_themes[0].name, "midnight");
        assert_eq!(
            loaded.custom_themes[0]
                .values
                .get("main_bg")
                .map(String::as_str),
            Some("\"#101010\"")
        );
        assert_eq!(loaded.custom_themes[1].name, "dawn");
        assert_eq!(
            loaded.custom_themes[1]
                .values
                .get("main_fg")
                .map(String::as_str),
            Some("\"#eeeeee\"")
        );
    }

    #[test]
    fn parse_config_body_ignores_invalid_custom_theme_sections() {
        let loaded = parse_config_body(
            r##"
[themes.bad name]
theme = "nord"
main_bg = "#101010"

[themes.-bad]
main_bg = "#202020"

[themes. spaced]
main_bg = "#202020"

[themes.valid_name-1]
main_bg = "#303030"
"##,
        );

        assert_eq!(loaded.config.theme, "btop");
        assert_eq!(loaded.custom_themes.len(), 1);
        assert_eq!(loaded.custom_themes[0].name, "valid_name-1");
        assert_eq!(
            loaded.custom_themes[0]
                .values
                .get("main_bg")
                .map(String::as_str),
            Some("\"#303030\"")
        );
    }

    #[test]
    fn strip_inline_comment_preserves_hash_inside_quotes() {
        assert_eq!(
            strip_inline_comment(r##""#101010" # color"##),
            r##""#101010""##
        );
        assert_eq!(strip_inline_comment("true # comment"), "true");
    }

    #[test]
    fn custom_theme_name_validation_accepts_documented_grammar() {
        for name in ["midnight", "cga-lab", "CGA_LAB", "theme-1"] {
            assert!(is_valid_custom_theme_name(name), "{name} should be valid");
        }

        let max_len_name = format!("a{}", "b".repeat(63));
        assert_eq!(max_len_name.len(), 64);
        assert!(is_valid_custom_theme_name(&max_len_name));

        let too_long_name = format!("a{}", "b".repeat(64));
        assert_eq!(too_long_name.len(), 65);
        assert!(!is_valid_custom_theme_name(&too_long_name));

        for name in ["", "-bad", "_bad", "bad name", "bad.name", "naïve"] {
            assert!(
                !is_valid_custom_theme_name(name),
                "{name} should be invalid"
            );
        }
    }

    #[test]
    fn theme_name_value_rejects_invalid_names_before_persistence() {
        assert_eq!(theme_name_value("midnight").unwrap(), "\"midnight\"");
        assert!(theme_name_value("bad name").is_err());
        assert!(theme_name_value("bad\"name").is_err());
    }

    #[test]
    fn toml_basic_string_escapes_special_characters() {
        assert_eq!(
            toml_basic_string("quote\" slash\\ newline\n"),
            "\"quote\\\" slash\\\\ newline\\n\""
        );
    }

    fn theme_update(name: &str) -> Vec<(&'static str, String)> {
        vec![("theme", theme_name_value(name).unwrap())]
    }

    #[test]
    fn rewrite_theme_preserves_hidden_agents_line() {
        let before = "theme = \"btop\"\nhidden_agents = [\"codex\"]\n";
        let after = rewrite_kv_lines(before, &theme_update("dracula"));
        assert!(after.contains("theme = \"dracula\""));
        assert!(
            after.contains("hidden_agents = [\"codex\"]"),
            "hidden_agents line dropped:\n{after}"
        );
    }

    #[test]
    fn rewrite_theme_preserves_arbitrary_unknown_keys() {
        let before = "# user comment\nfuture_key = 42\ntheme = \"btop\"\n";
        let after = rewrite_kv_lines(before, &theme_update("nord"));
        assert!(after.contains("# user comment"));
        assert!(after.contains("future_key = 42"));
        assert!(after.contains("theme = \"nord\""));
    }

    #[test]
    fn rewrite_theme_appends_when_missing() {
        let before = "hidden_agents = [\"codex\"]\n";
        let after = rewrite_kv_lines(before, &theme_update("gruvbox"));
        assert!(after.contains("hidden_agents = [\"codex\"]"));
        assert!(after.contains("theme = \"gruvbox\""));
    }

    #[test]
    fn rewrite_theme_appends_before_custom_theme_section() {
        let before = "[themes.midnight]\nmain_bg = \"#101010\"\n";
        let after = rewrite_kv_lines(before, &theme_update("midnight"));
        assert!(after.starts_with("theme = \"midnight\"\n[themes.midnight]"));
        assert!(after.contains("main_bg = \"#101010\""));
    }

    #[test]
    fn rewrite_theme_ignores_theme_keys_inside_sections() {
        let before = "theme = \"btop\"\n[themes.midnight]\ntheme = \"not-a-root-key\"\n";
        let after = rewrite_kv_lines(before, &theme_update("dracula"));
        assert!(after.contains("theme = \"dracula\"\n[themes.midnight]"));
        assert!(after.contains("theme = \"not-a-root-key\""));
    }

    #[test]
    fn rewrite_panels_replaces_existing_and_appends_missing() {
        let before = "theme = \"btop\"\nshow_quota = true\n";
        let updates: Vec<(&str, String)> = vec![
            ("show_quota", "false".to_string()),
            ("show_projects", "false".to_string()),
        ];
        let after = rewrite_kv_lines(before, &updates);
        assert!(after.contains("show_quota = false"));
        assert!(!after.contains("show_quota = true"));
        assert!(after.contains("show_projects = false"));
        assert!(after.contains("theme = \"btop\""));
    }

    #[test]
    fn parse_bool_round_trips_visibility_keys() {
        assert_eq!(parse_bool("true"), Some(true));
        assert_eq!(parse_bool("False"), Some(false));
        assert_eq!(parse_bool("nope"), None);
    }

    #[test]
    fn rewrite_language_replaces_existing() {
        let before = "theme = \"btop\"\nlanguage = \"en\"\n";
        let updates: Vec<(&str, String)> = vec![("language", "\"zh\"".to_string())];
        let after = rewrite_kv_lines(before, &updates);
        assert!(after.contains("language = \"zh\""));
        assert!(!after.contains("language = \"en\""));
        assert!(after.contains("theme = \"btop\""));
    }
}
