//! What is installed, and what is switched on.
//!
//! One directory per extension, under `~/.local/share/spool/extensions`:
//!
//! ```text
//! extensions/
//!   state.toml              which ones are switched off
//!   unsubscribe/
//!     spool.toml
//!     config.toml           what you told it, if it asked for anything
//!     extension.wasm
//! ```
//!
//! The directory name *is* the id. That is checked rather than assumed, because
//! a directory called `unsubscribe` holding a manifest that calls itself
//! `mail-rules` is either a mistake or an attempt to shadow something, and both
//! are worth refusing by name.
//!
//! Loading is deliberately forgiving in one direction only: one unreadable
//! extension is reported and skipped, never fatal, because the alternative is a
//! mail client that will not start until you find the bad file. It is
//! unforgiving about what it accepts — a manifest that does not parse does not
//! load at all, rather than loading with the parts that happened to be valid.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::{
    config::{CONFIG_FILE, Config},
    manifest::{MANIFEST_FILE, Manifest, WASM_FILE},
};

/// An extension that is installed and whose manifest made sense.
#[derive(Debug, Clone)]
pub struct Installed {
    pub manifest: Manifest,
    /// The extension's own directory — also where its `extension.wasm` is.
    pub dir: PathBuf,
    /// Whether it is switched on. A disabled extension stays installed, keeps
    /// its stored state, and contributes nothing.
    pub enabled: bool,
    /// What `config.toml` gave it, checked against what it declared.
    ///
    /// Read here rather than at load so the settings pane can show an
    /// extension that is installed but not yet configured — which is a
    /// different thing from one that is broken, and reads differently.
    pub config: Config,
}

impl Installed {
    pub fn wasm(&self) -> PathBuf {
        self.dir.join(WASM_FILE)
    }

    pub fn id(&self) -> &str {
        self.manifest.id.as_str()
    }

    /// Where the values it was given live.
    pub fn config_file(&self) -> PathBuf {
        self.dir.join(CONFIG_FILE)
    }

    /// Required settings it has not been given.
    ///
    /// Empty for the overwhelming majority of extensions, which declare none.
    pub fn unconfigured(&self) -> &[String] {
        self.config.missing()
    }
}

/// An extension directory that could not be loaded, and why.
///
/// Kept rather than logged and dropped: the settings pane shows these, because
/// an extension that is installed and invisible is worse than one that is
/// installed and visibly broken.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Problem {
    pub dir: PathBuf,
    pub why: String,
}

/// Everything installed, in id order.
#[derive(Debug, Clone, Default)]
pub struct Registry {
    root: PathBuf,
    installed: Vec<Installed>,
    problems: Vec<Problem>,
    state: State,
}

impl Registry {
    /// Reads the extensions directory.
    ///
    /// A missing directory is an empty registry, not an error — that is simply
    /// what a machine with no extensions looks like, and creating it eagerly
    /// would litter the data directory of everyone who never installs one.
    pub fn load(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let state = State::read(&root);

        let mut installed = Vec::new();
        let mut problems = Vec::new();

        let entries = match std::fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(_) => return Self { root, installed, problems, state },
        };

        for entry in entries.flatten() {
            let dir = entry.path();
            if !dir.is_dir() {
                continue;
            }
            match load_one(&dir) {
                Ok((manifest, config)) => {
                    let enabled = !state.disabled.contains(manifest.id.as_str());
                    installed.push(Installed { manifest, dir, enabled, config });
                }
                Err(why) => problems.push(Problem { dir, why }),
            }
        }

        // Sorted so the settings list and the palette are in the same order
        // every launch. `read_dir` promises nothing about ordering, and a list
        // that reshuffles itself between launches is one nobody can learn.
        installed.sort_by(|a, b| a.manifest.id.cmp(&b.manifest.id));
        problems.sort_by(|a, b| a.dir.cmp(&b.dir));

        Self { root, installed, problems, state }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Everything installed, working or not switched on.
    pub fn all(&self) -> &[Installed] {
        &self.installed
    }

    /// Everything that will actually run.
    pub fn enabled(&self) -> impl Iterator<Item = &Installed> {
        self.installed.iter().filter(|e| e.enabled)
    }

    pub fn problems(&self) -> &[Problem] {
        &self.problems
    }

    pub fn get(&self, id: &str) -> Option<&Installed> {
        self.installed.iter().find(|e| e.id() == id)
    }

    /// The extensions that have to be running before anybody asks for them.
    pub fn eager(&self) -> impl Iterator<Item = &Installed> {
        self.enabled().filter(|e| e.manifest.starts_eagerly())
    }

    /// Every command every enabled extension contributes, qualified with the
    /// extension that owns it.
    ///
    /// This is what the palette merges in, and it is available without loading
    /// a single component — which is what lets an extension be listed, found by
    /// search and given a keystroke while still costing nothing until it is
    /// actually run.
    pub fn commands(&self) -> Vec<Contributed> {
        self.enabled()
            .flat_map(|ext| {
                ext.manifest.commands.iter().map(move |cmd| Contributed {
                    qualified: ext.manifest.qualified(cmd.id.as_str()),
                    title: cmd.title.clone(),
                    extension: ext.manifest.name.clone(),
                    key: cmd.key.clone(),
                })
            })
            .collect()
    }

    /// Every panel every enabled extension contributes.
    pub fn panels(&self) -> Vec<ContributedPanel> {
        self.enabled()
            .flat_map(|ext| {
                ext.manifest.panels.iter().map(move |panel| ContributedPanel {
                    qualified: ext.manifest.qualified(panel.id.as_str()),
                    title: panel.title.clone(),
                    extension: ext.manifest.name.clone(),
                    glyph: panel.glyph.clone(),
                })
            })
            .collect()
    }

    /// Switches one on or off and writes the decision to disk.
    ///
    /// Persisted immediately rather than at quit: disabling an extension is
    /// very often what somebody does *because* it is misbehaving, and a
    /// decision that only survives a clean exit is one that does not survive
    /// the case it was made for.
    pub fn set_enabled(&mut self, id: &str, enabled: bool) -> std::io::Result<()> {
        let Some(ext) = self.installed.iter_mut().find(|e| e.id() == id) else {
            return Ok(());
        };
        ext.enabled = enabled;

        match enabled {
            true => self.state.disabled.remove(id),
            false => self.state.disabled.insert(id.to_string()),
        };
        self.state.write(&self.root)
    }
}

/// Reads and validates one extension directory.
fn load_one(dir: &Path) -> Result<(Manifest, Config), String> {
    let manifest_path = dir.join(MANIFEST_FILE);
    if !manifest_path.exists() {
        return Err(format!("no {MANIFEST_FILE}"));
    }

    let text = std::fs::read_to_string(&manifest_path)
        .map_err(|e| format!("{MANIFEST_FILE} could not be read: {e}"))?;
    let manifest = Manifest::parse(&text).map_err(|e| e.to_string())?;

    // The directory name is the id. Checked rather than trusted, because the
    // id is what namespaces the extension's storage and qualifies its
    // commands — two directories disagreeing about which one is `unsubscribe`
    // would have them sharing state.
    let folder = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if folder != manifest.id.as_str() {
        return Err(format!(
            "is in a folder called `{folder}` but calls itself `{}`",
            manifest.id
        ));
    }

    if !dir.join(WASM_FILE).exists() {
        return Err(format!("no {WASM_FILE}"));
    }

    // A `config.toml` that does not parse, or that sets something the manifest
    // never declared, is a problem with the extension rather than a reason to
    // start it with half its settings. A *missing* value is not: that one is
    // carried on `Installed` so the pane can say what to fill in.
    let config = Config::read(dir, &manifest).map_err(|e| e.to_string())?;

    Ok((manifest, config))
}

/// A command an extension contributes, as the app sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Contributed {
    /// `unsubscribe.open` — what the host dispatches on.
    pub qualified: String,
    pub title: String,
    /// The extension's display name, shown beside the title so two extensions
    /// with a command called "Refresh" are telling you which is which.
    pub extension: String,
    pub key: Option<String>,
}

/// A panel an extension contributes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContributedPanel {
    pub qualified: String,
    pub title: String,
    pub extension: String,
    pub glyph: Option<String>,
}

/// `state.toml` — the only thing the host writes into the extensions directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct State {
    /// Ids that are installed and switched off.
    ///
    /// A list of the exceptions rather than of the enabled, so that installing
    /// an extension is what switches it on. The alternative — an `enabled` list
    /// — means every install is followed by a second step nobody expects, and
    /// an extension that quietly does nothing after being installed reads as
    /// broken.
    disabled: BTreeSet<String>,
}

impl State {
    fn read(root: &Path) -> Self {
        std::fs::read_to_string(root.join("state.toml"))
            .ok()
            .and_then(|text| toml::from_str(&text).ok())
            .unwrap_or_default()
    }

    fn write(&self, root: &Path) -> std::io::Result<()> {
        std::fs::create_dir_all(root)?;
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(root.join("state.toml"), text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A directory of its own per test: they run on threads of one process, so
    /// the pid alone would have them writing over each other.
    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("spool-ext-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn install(root: &Path, id: &str, manifest: &str) {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), manifest).unwrap();
        std::fs::write(dir.join(WASM_FILE), b"\0asm not really").unwrap();
    }

    fn manifest_for(id: &str) -> String {
        format!(
            r#"
            [extension]
            id = "{id}"
            name = "The {id} extension"
            api = "0.2"
            capabilities = ["ui"]

            [[commands]]
            id = "go"
            title = "Go"
            key = "cmd-shift-g"
            "#
        )
    }

    #[test]
    fn a_machine_with_no_extensions_directory_has_no_extensions() {
        let registry = Registry::load(std::env::temp_dir().join("spool-ext-does-not-exist"));
        assert!(registry.all().is_empty());
        assert!(registry.problems().is_empty(), "a missing directory is not a problem to report");
    }

    #[test]
    fn installed_extensions_are_found_and_ordered_by_id() {
        let root = scratch("found");
        install(&root, "zebra", &manifest_for("zebra"));
        install(&root, "apple", &manifest_for("apple"));

        let registry = Registry::load(&root);
        let ids: Vec<&str> = registry.all().iter().map(|e| e.id()).collect();
        assert_eq!(ids, ["apple", "zebra"], "read_dir order must not leak into the UI");
        assert!(registry.all().iter().all(|e| e.enabled), "installing is what enables");
    }

    #[test]
    fn one_broken_extension_does_not_take_the_others_down() {
        let root = scratch("broken");
        install(&root, "fine", &manifest_for("fine"));
        install(&root, "broken", "this is not toml at all {{{");

        let registry = Registry::load(&root);
        assert_eq!(registry.all().len(), 1);
        assert_eq!(registry.all()[0].id(), "fine");
        assert_eq!(registry.problems().len(), 1);
        assert!(registry.problems()[0].dir.ends_with("broken"));
    }

    #[test]
    fn a_manifest_that_disagrees_with_its_folder_is_refused() {
        // Two directories claiming the same id would share a storage
        // namespace, which is how one extension reads another's secrets.
        let root = scratch("shadow");
        install(&root, "pretender", &manifest_for("something-else"));

        let registry = Registry::load(&root);
        assert!(registry.all().is_empty());
        assert!(
            registry.problems()[0].why.contains("calls itself"),
            "{}",
            registry.problems()[0].why
        );
    }

    #[test]
    fn an_extension_with_no_wasm_is_a_problem_rather_than_a_listing() {
        let root = scratch("nowasm");
        let dir = root.join("empty");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(MANIFEST_FILE), manifest_for("empty")).unwrap();

        let registry = Registry::load(&root);
        assert!(registry.all().is_empty());
        assert!(registry.problems()[0].why.contains(WASM_FILE));
    }

    #[test]
    fn a_stray_file_in_the_extensions_directory_is_ignored() {
        let root = scratch("stray");
        install(&root, "real", &manifest_for("real"));
        std::fs::write(root.join("notes.txt"), "nothing to do with anything").unwrap();

        let registry = Registry::load(&root);
        assert_eq!(registry.all().len(), 1);
        assert!(registry.problems().is_empty(), "a loose file is not a broken extension");
    }

    #[test]
    fn disabling_survives_a_reload() {
        let root = scratch("disable");
        install(&root, "noisy", &manifest_for("noisy"));

        let mut registry = Registry::load(&root);
        registry.set_enabled("noisy", false).unwrap();
        assert!(!registry.get("noisy").unwrap().enabled);
        assert_eq!(registry.commands().len(), 0, "a disabled extension contributes nothing");

        let reloaded = Registry::load(&root);
        assert!(!reloaded.get("noisy").unwrap().enabled, "the decision outlives the process");

        // And back again.
        let mut reloaded = reloaded;
        reloaded.set_enabled("noisy", true).unwrap();
        assert!(Registry::load(&root).get("noisy").unwrap().enabled);
    }

    fn needs_a_key(id: &str) -> String {
        format!(
            r#"
            [extension]
            id = "{id}"
            api = "0.2"
            capabilities = ["storage"]

            [[config]]
            key = "api-key"
            title = "API key"
            required = true
            secret = true
            "#
        )
    }

    #[test]
    fn a_setting_is_read_from_the_directory_it_was_installed_in() {
        let root = scratch("configured");
        install(&root, "scorer", &needs_a_key("scorer"));
        std::fs::write(root.join("scorer").join(CONFIG_FILE), r#"api-key = "sk-test""#).unwrap();

        let registry = Registry::load(&root);
        let installed = registry.get("scorer").unwrap();
        assert_eq!(installed.config.get("api-key"), Some("sk-test"));
        assert!(installed.unconfigured().is_empty());
    }

    #[test]
    fn an_extension_waiting_to_be_configured_is_installed_rather_than_broken() {
        // The distinction the settings pane needs: "fill this in" and "this
        // does not work" are different sentences, and an extension nobody has
        // given a key to yet is the first one.
        let root = scratch("unconfigured");
        install(&root, "scorer", &needs_a_key("scorer"));

        let registry = Registry::load(&root);
        assert!(registry.problems().is_empty(), "{:?}", registry.problems());

        let installed = registry.get("scorer").expect("it is installed");
        assert_eq!(installed.unconfigured(), ["api-key"]);
        assert!(installed.enabled, "and switched on, waiting for a value");
    }

    #[test]
    fn a_config_file_that_sets_something_undeclared_is_a_problem() {
        let root = scratch("misconfigured");
        install(&root, "scorer", &needs_a_key("scorer"));
        std::fs::write(
            root.join("scorer").join(CONFIG_FILE),
            "api-key = \"sk\"\nendpoint = \"https://elsewhere.example\"",
        )
        .unwrap();

        let registry = Registry::load(&root);
        assert!(registry.all().is_empty(), "it does not load half-configured");
        assert!(registry.problems()[0].why.contains("endpoint"), "{}", registry.problems()[0].why);
    }

    #[test]
    fn commands_are_qualified_by_the_extension_that_owns_them() {
        let root = scratch("commands");
        install(&root, "one", &manifest_for("one"));
        install(&root, "two", &manifest_for("two"));

        let registry = Registry::load(&root);
        let commands = registry.commands();
        let ids: Vec<&str> = commands.iter().map(|c| c.qualified.as_str()).collect();

        // Both extensions call their command `go`; only the qualified form
        // tells the palette which one to run.
        assert_eq!(ids, ["one.go", "two.go"]);
        assert_eq!(commands[0].extension, "The one extension");
        assert_eq!(commands[0].key.as_deref(), Some("cmd-shift-g"));
    }

    #[test]
    fn only_the_extensions_that_need_to_be_live_are_eager() {
        let root = scratch("eager");
        install(&root, "lazy", &manifest_for("lazy"));
        install(
            &root,
            "watcher",
            r#"
            [extension]
            id = "watcher"
            api = "0.2"
            capabilities = ["read-mail"]
            hooks = ["messages"]
            "#,
        );

        let registry = Registry::load(&root);
        let eager: Vec<&str> = registry.eager().map(|e| e.id()).collect();
        assert_eq!(eager, ["watcher"], "an extension of commands waits to be asked");
    }
}
