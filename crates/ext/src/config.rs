//! `config.toml` — the one thing an extension cannot be told any other way.
//!
//! An extension runs with WASI linked empty: no environment, no filesystem, no
//! sockets it did not declare. The UI vocabulary it describes itself in has a
//! button and a line of text and nothing that accepts typing. Put those two
//! facts together and there is no path at all by which an API key could reach
//! the inside of a sandbox — not a bad path, none.
//!
//! So the host carries it. Each extension may declare settings in its manifest;
//! the values live in `config.toml` beside `spool.toml`, and the host seeds them
//! into the extension's own key/value store, under keys the extension may read
//! and may not write. Three things follow from doing it this way rather than
//! adding a host function:
//!
//! * It costs no change to the WIT. Before 1.0 a minor bump is a breaking
//!   change, so a new import would retire every extension built against 0.1 in
//!   order to deliver a string.
//! * The values are visible before anything runs. A file you can read and a
//!   manifest that says what belongs in it is a better answer to "what did I
//!   give this thing" than a value buried in a database.
//! * It is the same shape the settings pane will write when there is one. The
//!   pane edits the file; nothing else has to change.
//!
//! What it is not is a secret store. A value here is readable by anyone who can
//! read the directory, and an extension that can read its own key can send it
//! anywhere its manifest lets it reach. `secret = true` masks a field in the UI
//! and keeps it out of the log. That is the whole promise, and it is worth
//! stating plainly rather than implying a stronger one.

use std::{
    collections::BTreeMap,
    fmt,
    path::{Path, PathBuf},
};

use crate::manifest::Manifest;

/// The file this module reads.
pub const CONFIG_FILE: &str = "config.toml";

/// The namespace within an extension's storage that the host owns.
///
/// Reserved rather than merely conventional: `store-set` refuses a key under
/// it, so an extension cannot write itself a setting it was never given and
/// then read it back as though a user had put it there.
pub const PREFIX: &str = "config.";

/// The storage key one setting is seeded under.
pub fn storage_key(key: &str) -> String {
    format!("{PREFIX}{key}")
}

/// Whether a storage key belongs to the host rather than to the extension.
pub fn is_reserved(key: &str) -> bool {
    key.starts_with(PREFIX)
}

/// An extension's settings, checked against what its manifest declared.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Config {
    values: BTreeMap<String, String>,
    missing: Vec<String>,
}

impl Config {
    /// Reads an extension's `config.toml`, or says why it will not.
    ///
    /// A missing file is not an error. An extension with no required settings
    /// works without one, and an extension with required settings is better
    /// described by "needs `api-key`" than by "has no config.toml" — which is
    /// true of every extension ever installed and tells nobody what to do.
    pub fn read(dir: &Path, manifest: &Manifest) -> Result<Self, ConfigError> {
        let path = dir.join(CONFIG_FILE);
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(e) => return Err(ConfigError::Unreadable { path, why: e.to_string() }),
        };

        Self::parse(&text, manifest)
    }

    /// The same, from text already in hand.
    pub fn parse(text: &str, manifest: &Manifest) -> Result<Self, ConfigError> {
        let table: toml::Table =
            toml::from_str(text).map_err(|e| ConfigError::Syntax(e.to_string()))?;

        let mut values = BTreeMap::new();

        for (key, value) in table {
            // Refused rather than ignored. A typo in a key is the failure mode
            // this file has — `api_key` for `api-key` — and silently ignoring
            // it produces an extension that is configured, says it is not, and
            // gives no hint which of the two spellings it wanted.
            if manifest.setting(&key).is_none() {
                return Err(ConfigError::Unknown {
                    key,
                    known: manifest.settings().iter().map(|c| c.key.to_string()).collect(),
                });
            }
            let declared = manifest.setting(&key).expect("checked above");
            if declared.is_list() {
                values.insert(key.clone(), rows(declared, &value)?);
                continue;
            }
            let value = scalar(&key, &value)?;
            if !declared.allows(&value) {
                return Err(ConfigError::NotAChoice { key, value, choices: declared.choices.clone() });
            }
            values.insert(key.clone(), value);
        }

        // Defaults fill in after the file, never over it.
        for declared in manifest.settings() {
            if declared.is_list() {
                values.entry(declared.key.to_string()).or_insert_with(|| declared.default_rows());
                continue;
            }
            if let Some(default) = &declared.default {
                values.entry(declared.key.to_string()).or_insert_with(|| default.clone());
            }
        }

        let missing = manifest
            .settings()
            .iter()
            .filter(|c| c.required && !values.contains_key(c.key.as_str()))
            .map(|c| c.key.to_string())
            .collect();

        Ok(Self { values, missing })
    }

    pub fn get(&self, key: &str) -> Option<&str> {
        self.values.get(key).map(String::as_str)
    }

    /// Required settings with no value, in declaration order.
    pub fn missing(&self) -> &[String] {
        &self.missing
    }

    /// Whether this extension has everything it said it needs.
    pub fn complete(&self) -> bool {
        self.missing.is_empty()
    }

    /// Every setting to seed, as `(key, value)`.
    pub fn entries(&self) -> impl Iterator<Item = (&str, &str)> {
        self.values.iter().map(|(k, v)| (k.as_str(), v.as_str()))
    }
}

/// A scalar, spelled the way the extension will read it.
///
/// Everything crosses the boundary as a string, so a number in the file is a
/// kindness rather than a type: `threshold = 3` and `threshold = "3"` should
/// not be a difference anybody has to know about. An array or a table is
/// refused, because a setting the extension reads as a string is not a place to
/// put structure the host would have to invent a spelling for.
/// A list setting's rows, as the JSON the extension is given: each row
/// checked against the declared fields and filled out with their defaults.
fn rows(declared: &crate::manifest::ConfigDecl, value: &toml::Value) -> Result<String, ConfigError> {
    let key = declared.key.to_string();
    let toml::Value::Array(items) = value else {
        return Err(ConfigError::NotAList { key, found: value.type_str() });
    };
    let mut rows = Vec::new();
    for item in items {
        let toml::Value::Table(table) = item else {
            return Err(ConfigError::NotAList { key, found: item.type_str() });
        };
        let mut row = std::collections::BTreeMap::new();
        for (field, value) in table {
            if !declared.fields.iter().any(|f| f.key == *field.as_str()) {
                return Err(ConfigError::Unknown {
                    key: format!("{key}.{field}"),
                    known: declared.fields.iter().map(|f| f.key.to_string()).collect(),
                });
            }
            row.insert(field.clone(), scalar(&format!("{key}.{field}"), value)?);
        }
        rows.push(declared.complete_row(&row));
    }
    Ok(serde_json::to_string(&rows).expect("strings serialize"))
}

fn scalar(key: &str, value: &toml::Value) -> Result<String, ConfigError> {
    match value {
        toml::Value::String(s) => Ok(s.clone()),
        toml::Value::Integer(n) => Ok(n.to_string()),
        toml::Value::Float(n) => Ok(n.to_string()),
        toml::Value::Boolean(b) => Ok(b.to_string()),
        other => Err(ConfigError::NotAScalar {
            key: key.to_string(),
            found: other.type_str(),
        }),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConfigError {
    Unreadable { path: PathBuf, why: String },
    Syntax(String),
    Unknown { key: String, known: Vec<String> },
    NotAScalar { key: String, found: &'static str },
    NotAChoice { key: String, value: String, choices: Vec<String> },
    NotAList { key: String, found: &'static str },
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unreadable { path, why } => {
                write!(f, "{} could not be read: {why}", path.display())
            }
            Self::Syntax(why) => write!(f, "{CONFIG_FILE} is not valid toml: {why}"),
            Self::Unknown { key, known } => {
                write!(f, "{CONFIG_FILE} sets `{key}`, which this extension does not declare")?;
                match known.is_empty() {
                    true => write!(f, " — it declares no settings at all"),
                    false => write!(f, " — it declares {}", known.join(", ")),
                }
            }
            Self::NotAScalar { key, found } => write!(
                f,
                "`{key}` in {CONFIG_FILE} is {found}, and a setting has to be a \
                 string, number or boolean"
            ),
            Self::NotAList { key, found } => write!(
                f,
                "`{key}` in {CONFIG_FILE} is {found}, and it has to be a list of \
                 `[[{key}]]` tables"
            ),
            Self::NotAChoice { key, value, choices } => write!(
                f,
                "`{key}` in {CONFIG_FILE} is `{value}`, and it has to be one of {}",
                choices.join(", ")
            ),
        }
    }
}

impl std::error::Error for ConfigError {}

#[cfg(test)]
mod tests {
    use super::*;

    fn manifest(config: &str) -> Manifest {
        Manifest::parse(&format!(
            r#"
            [extension]
            id = "scorer"
            api = "0.2"
            capabilities = ["storage"]
            {config}
            "#
        ))
        .unwrap()
    }

    fn with_key() -> Manifest {
        manifest(
            r#"
            [[config]]
            key = "api-key"
            title = "API key"
            required = true
            secret = true

            [[config]]
            key = "model"
            title = "Model"
            default = "jev-latest"
            "#,
        )
    }

    #[test]
    fn a_declared_setting_is_read_and_an_undeclared_one_is_refused() {
        let manifest = with_key();

        let config = Config::parse(r#"api-key = "sk-test""#, &manifest).unwrap();
        assert_eq!(config.get("api-key"), Some("sk-test"));
        assert!(config.complete());

        // The failure this file actually has: a plausible misspelling.
        let why = Config::parse(r#"api_key = "sk-test""#, &manifest).unwrap_err().to_string();
        assert!(why.contains("api_key"), "{why}");
        assert!(why.contains("api-key"), "it says what the right spelling is: {why}");
    }

    #[test]
    fn a_list_setting_is_read_as_rows_and_defaults_to_its_own() {
        let manifest = manifest(
            r#"
            [[config]]
            key = "labels"
            title = "Labels"
            fields = [
                { key = "name", title = "Name" },
                { key = "show", title = "Show", kind = "switch", default = "true" },
            ]
            rows = [{ name = "Promotion" }, { name = "Personal", show = "false" }]
            "#,
        );

        let default = Config::parse("", &manifest).unwrap();
        assert_eq!(
            default.get("labels"),
            Some(r#"[{"name":"Promotion","show":"true"},{"name":"Personal","show":"false"}]"#)
        );

        let chosen = Config::parse("[[labels]]\nname = \"Invoices\"\nshow = false\n", &manifest).unwrap();
        assert_eq!(chosen.get("labels"), Some(r#"[{"name":"Invoices","show":"false"}]"#));

        let why = Config::parse("labels = \"Promotion\"", &manifest).unwrap_err().to_string();
        assert!(why.contains("[[labels]]"), "{why}");
        let why = Config::parse("[[labels]]\ncolour = \"red\"\n", &manifest).unwrap_err().to_string();
        assert!(why.contains("labels.colour"), "{why}");
    }

    #[test]
    fn a_setting_with_choices_takes_only_those() {
        let manifest = manifest(
            r#"
            [[config]]
            key = "lookback"
            title = "How far back"
            default = "7d"
            choices = ["24h", "7d", "30d"]
            "#,
        );

        assert_eq!(Config::parse("", &manifest).unwrap().get("lookback"), Some("7d"));
        assert_eq!(Config::parse(r#"lookback = "30d""#, &manifest).unwrap().get("lookback"), Some("30d"));

        let why = Config::parse(r#"lookback = "forever""#, &manifest).unwrap_err().to_string();
        assert!(why.contains("forever") && why.contains("24h, 7d, 30d"), "{why}");
    }

    #[test]
    fn a_default_fills_in_but_never_overrides() {
        let manifest = with_key();

        let config = Config::parse(r#"api-key = "sk-test""#, &manifest).unwrap();
        assert_eq!(config.get("model"), Some("jev-latest"));

        let config =
            Config::parse("api-key = \"sk\"\nmodel = \"jev-2\"", &manifest).unwrap();
        assert_eq!(config.get("model"), Some("jev-2"));
    }

    #[test]
    fn a_required_setting_with_no_value_is_named_rather_than_guessed_at() {
        let config = Config::parse("", &with_key()).unwrap();
        assert!(!config.complete());
        assert_eq!(config.missing(), ["api-key"]);
        // The one with a default is not missing, whatever else is.
        assert_eq!(config.get("model"), Some("jev-latest"));
    }

    #[test]
    fn numbers_and_booleans_are_read_as_the_strings_they_cross_the_boundary_as() {
        let manifest = manifest(
            r#"
            [[config]]
            key = "threshold"
            title = "Threshold"

            [[config]]
            key = "loud"
            title = "Loud"
            "#,
        );

        let config = Config::parse("threshold = 3\nloud = true", &manifest).unwrap();
        assert_eq!(config.get("threshold"), Some("3"));
        assert_eq!(config.get("loud"), Some("true"));
    }

    #[test]
    fn structure_is_refused_because_a_setting_is_a_string() {
        let manifest = manifest(
            r#"
            [[config]]
            key = "hosts"
            title = "Hosts"
            "#,
        );

        let why = Config::parse(r#"hosts = ["a", "b"]"#, &manifest).unwrap_err().to_string();
        assert!(why.contains("hosts"), "{why}");
    }

    #[test]
    fn an_extension_with_no_settings_and_no_file_is_configured() {
        let manifest = manifest("");
        let config = Config::parse("", &manifest).unwrap();
        assert!(config.complete());
        assert_eq!(config.entries().count(), 0);
    }

    #[test]
    fn the_hosts_namespace_is_recognisable() {
        assert_eq!(storage_key("api-key"), "config.api-key");
        assert!(is_reserved("config.api-key"));
        assert!(!is_reserved("verdict.41"), "an extension's own keys are its own");
    }

    #[test]
    fn declaring_settings_without_storage_is_refused_at_the_manifest() {
        // Config arrives through the key/value store, so an extension that
        // cannot read that store would be configured and never know it.
        let why = Manifest::parse(
            r#"
            [extension]
            id = "scorer"
            api = "0.2"
            capabilities = ["ui"]

            [[config]]
            key = "api-key"
            title = "API key"
            "#,
        )
        .unwrap_err()
        .to_string();

        assert!(why.contains("storage"), "{why}");
        assert!(why.contains("settings"), "{why}");
    }
}
