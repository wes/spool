//! Who measured it, and what state they were in at the time.
//!
//! Two separate things, and the distinction matters.
//!
//! The **machine** is identity. Numbers from two machines are not comparable at
//! all — not by a scaling factor, not with a caveat — so the fingerprint is
//! what decides which history file a run is appended to and which runs it may
//! be held against. Getting this wrong does not produce a slightly wrong
//! answer; it produces confident nonsense.
//!
//! The **conditions** are the state of that machine while it was measuring: on
//! battery, thermally limited, busy with something else. These do not change
//! which history a run belongs to, because they are not a different computer —
//! they are the same computer having a bad afternoon. They are recorded so that
//! a run which disagrees with its neighbours can be explained rather than
//! puzzled over six weeks later.

use std::process::Command;

use serde::{Deserialize, Serialize};

/// Identity. Decides which history file a run belongs to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Machine {
    /// A filename-safe name for this machine: `apple-m4-max`.
    pub slug: String,
    /// What the CPU calls itself: `Apple M4 Max`.
    pub cpu: String,
    /// The hardware model: `Mac16,6`.
    pub model: String,
    pub cores: usize,
    pub memory: u64,
    /// `macOS 26.1`, or whatever the platform will admit to.
    pub os: String,
    pub arch: String,
}

impl Machine {
    pub fn detect() -> Self {
        let cpu = sysctl("machdep.cpu.brand_string")
            .unwrap_or_else(|| std::env::consts::ARCH.to_string());
        let model = sysctl("hw.model").unwrap_or_else(|| "unknown".into());

        let os = match sw_vers() {
            Some(version) => format!("macOS {version}"),
            None => std::env::consts::OS.to_string(),
        };

        Self {
            slug: slugify(&cpu),
            cpu,
            model,
            cores: sysctl("hw.ncpu").and_then(|n| n.parse().ok()).unwrap_or(0),
            memory: sysctl("hw.memsize").and_then(|n| n.parse().ok()).unwrap_or(0),
            os,
            arch: std::env::consts::ARCH.to_string(),
        }
    }

    /// One line for the top of a report.
    pub fn describe(&self) -> String {
        format!("{} / {} cores / {}", self.cpu, self.cores, self.os)
    }
}

/// State, not identity. Recorded so an odd run can be explained.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Conditions {
    /// Running on battery, where macOS will quietly cap clocks.
    pub on_battery: bool,
    /// `CPU_Speed_Limit` from `pmset -g therm`. 100 means unthrottled; less
    /// means the machine is hot and every number in the run is soft.
    pub speed_limit: Option<u32>,
    /// One-minute load average when the run started.
    pub load: Option<f64>,
}

impl Conditions {
    pub fn detect() -> Self {
        Self {
            on_battery: pmset("batt").is_some_and(|out| out.contains("Battery Power")),
            speed_limit: pmset("therm").and_then(|out| field(&out, "CPU_Speed_Limit")),
            load: load_average(),
        }
    }

    /// What is wrong with measuring right now, in sentences.
    ///
    /// Warnings rather than refusals. A benchmark that will not run on battery
    /// is one that does not get run, and a soft number carrying a note about
    /// why it is soft is worth more than no number — as long as the note is
    /// attached to it in the history rather than shouted once at the terminal.
    pub fn warnings(&self) -> Vec<String> {
        let mut warnings = Vec::new();

        if self.on_battery {
            warnings.push(
                "on battery — macOS caps clocks off AC, so these numbers are slow and \
                 not comparable with runs made plugged in"
                    .into(),
            );
        }
        if let Some(limit) = self.speed_limit
            && limit < 100
        {
            warnings.push(format!(
                "thermally limited to {limit}% — let it cool and run again, \
                 or treat everything here as a lower bound"
            ));
        }
        if let Some(load) = self.load
            && load > 2.0
        {
            warnings.push(format!(
                "load average {load:.1} — something else is using this machine, \
                 which lands in the tail of every benchmark below"
            ));
        }

        warnings
    }

    /// Whether the machine was in a state worth trusting.
    pub fn are_clean(&self) -> bool {
        self.warnings().is_empty()
    }
}

/// A filename-safe, stable name.
///
/// Lowercase, and anything that is not a letter or a digit becomes a hyphen —
/// so `Apple M4 Max` is `apple-m4-max`, which is a filename, a directory entry
/// and a thing to say out loud.
fn slugify(text: &str) -> String {
    let mut slug = String::new();
    let mut pending_hyphen = false;

    for c in text.chars() {
        match c.is_ascii_alphanumeric() {
            true => {
                if pending_hyphen && !slug.is_empty() {
                    slug.push('-');
                }
                pending_hyphen = false;
                slug.push(c.to_ascii_lowercase());
            }
            // Runs of punctuation collapse into one hyphen, and a trailing run
            // produces none at all.
            false => pending_hyphen = true,
        }
    }

    match slug.is_empty() {
        true => "unknown".into(),
        false => slug,
    }
}

fn sysctl(name: &str) -> Option<String> {
    run("sysctl", &["-n", name])
}

fn sw_vers() -> Option<String> {
    run("sw_vers", &["-productVersion"])
}

fn pmset(what: &str) -> Option<String> {
    run("pmset", &["-g", what])
}

/// The one-minute load average, out of `vm.loadavg`'s `{ 1.60 1.77 1.83 }`.
fn load_average() -> Option<f64> {
    let raw = sysctl("vm.loadavg")?;
    raw.split_whitespace().find_map(|word| word.parse::<f64>().ok())
}

/// `Name = 100` out of a block of them.
fn field(text: &str, name: &str) -> Option<u32> {
    text.lines()
        .find(|line| line.trim_start().starts_with(name))
        .and_then(|line| line.split('=').nth(1))
        .and_then(|value| value.trim().parse().ok())
}

/// Runs a command and takes its trimmed stdout, or nothing.
///
/// Every caller here is asking the system a question it may not answer — a
/// different platform, a command that is not installed, a field that moved. A
/// benchmark harness that fails to start because `pmset` is missing would be a
/// harness nobody can run anywhere else.
fn run(program: &str, args: &[&str]) -> Option<String> {
    let output = Command::new(program).args(args).output().ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8(output.stdout).ok()?.trim().to_string();
    (!text.is_empty()).then_some(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_cpu_name_becomes_a_filename() {
        assert_eq!(slugify("Apple M4 Max"), "apple-m4-max");
        assert_eq!(slugify("Intel(R) Core(TM) i9-9880H CPU @ 2.30GHz"), "intel-r-core-tm-i9-9880h-cpu-2-30ghz");
        assert_eq!(slugify("aarch64"), "aarch64");
    }

    #[test]
    fn slugs_never_end_in_punctuation_or_come_out_empty() {
        // The output is a path component, so a trailing hyphen or an empty
        // string would produce a file nobody meant to make.
        assert_eq!(slugify("Apple M4 Max   "), "apple-m4-max");
        assert_eq!(slugify("...!!!"), "unknown");
        assert_eq!(slugify(""), "unknown");
        assert!(!slugify("Weird -- Name --").ends_with('-'));
    }

    #[test]
    fn a_named_field_is_read_out_of_a_block_of_them() {
        let therm = "Note: No thermal warning level has been recorded\n  \
                     CPU_Speed_Limit \t= 87\n  CPU_Available_CPUs \t= 16\n";
        assert_eq!(field(therm, "CPU_Speed_Limit"), Some(87));
        assert_eq!(field(therm, "CPU_Available_CPUs"), Some(16));
        assert_eq!(field(therm, "Nothing_Like_This"), None);
    }

    #[test]
    fn conditions_explain_themselves_rather_than_refusing_to_run() {
        let hot = Conditions { on_battery: true, speed_limit: Some(62), load: Some(4.2) };
        let warnings = hot.warnings();

        assert_eq!(warnings.len(), 3, "{warnings:#?}");
        assert!(!hot.are_clean());
        assert!(warnings.iter().any(|w| w.contains("battery")));
        assert!(warnings.iter().any(|w| w.contains("62%")));
        assert!(warnings.iter().any(|w| w.contains("4.2")));
    }

    #[test]
    fn a_quiet_machine_on_ac_power_has_nothing_to_say() {
        let quiet = Conditions { on_battery: false, speed_limit: Some(100), load: Some(0.4) };
        assert!(quiet.are_clean(), "{:?}", quiet.warnings());

        // And a machine that would not answer the question is not accused of
        // anything.
        let unknown = Conditions::default();
        assert!(unknown.are_clean());
    }

    #[test]
    fn this_machine_identifies_itself() {
        let machine = Machine::detect();
        assert!(!machine.slug.is_empty());
        assert!(!machine.slug.contains('/'), "the slug is a filename: {}", machine.slug);
        assert!(!machine.describe().is_empty());
    }
}
