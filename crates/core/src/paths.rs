//! Where Spool keeps its files.
//!
//! Shared by the config loader and the settings store so there is one answer
//! to "where does this live", and one place to fix it per platform.

use std::path::PathBuf;

/// `~/.config/spool`, or the platform equivalent.
pub fn config_dir() -> PathBuf {
    resolve(
        "XDG_CONFIG_HOME",
        ".config",
        |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from),
    )
    .join("spool")
}

/// `~/.local/share/spool`, or the platform equivalent.
pub fn data_dir() -> PathBuf {
    resolve(
        "XDG_DATA_HOME",
        ".local/share",
        |name| std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from),
    )
    .join("spool")
}

/// The lookup is a parameter so this is testable without mutating the process
/// environment, which tests share.
///
/// A variable that is set but *empty* counts as unset. Taking it literally
/// yields a relative path, which would quietly put the user's mail and
/// credentials in whatever directory the app happened to start in.
fn resolve(
    xdg_var: &str,
    home_suffix: &str,
    var: impl Fn(&str) -> Option<PathBuf>,
) -> PathBuf {
    var(xdg_var)
        .or_else(|| var("HOME").map(|home| home.join(home_suffix)))
        .unwrap_or_else(|| PathBuf::from("."))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Stands in for the environment. An empty value models "set but empty",
    /// which the real lookup collapses to "unset".
    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<PathBuf> + use<> {
        let pairs: Vec<(String, String)> = pairs
            .iter()
            .filter(|(_, value)| !value.is_empty())
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect();

        move |name| pairs.iter().find(|(k, _)| k == name).map(|(_, v)| PathBuf::from(v))
    }

    #[test]
    fn the_xdg_variable_wins_when_set() {
        let dir = resolve("XDG_CONFIG_HOME", ".config", env(&[
            ("XDG_CONFIG_HOME", "/xdg"),
            ("HOME", "/u/me"),
        ]));
        assert_eq!(dir, PathBuf::from("/xdg"));
    }

    #[test]
    fn an_empty_variable_falls_through_instead_of_going_relative() {
        let dir = resolve("XDG_CONFIG_HOME", ".config", env(&[
            ("XDG_CONFIG_HOME", ""),
            ("HOME", "/u/me"),
        ]));
        assert_eq!(
            dir,
            PathBuf::from("/u/me/.config"),
            "an empty XDG_CONFIG_HOME must not yield a relative path"
        );
    }

    #[test]
    fn data_and_config_use_different_home_suffixes() {
        let vars = [("HOME", "/u/me")];
        assert_eq!(
            resolve("XDG_CONFIG_HOME", ".config", env(&vars)),
            PathBuf::from("/u/me/.config")
        );
        assert_eq!(
            resolve("XDG_DATA_HOME", ".local/share", env(&vars)),
            PathBuf::from("/u/me/.local/share")
        );
    }

    #[test]
    fn with_no_environment_at_all_the_path_is_still_usable() {
        assert_eq!(resolve("XDG_CONFIG_HOME", ".config", env(&[])), PathBuf::from("."));
    }
}
