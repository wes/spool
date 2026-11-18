//! The demo mailbox: made-up mail to try Spool on before trusting it with
//! real mail.
//!
//! A new install used to open on an empty window, and the only way to see the
//! app with mail in it was `spool --demo`, a flag nobody who installed from the
//! disk image would ever read. The welcome screen now offers the same corpus
//! behind a button.
//!
//! It lives in a directory of its own under the data directory. The client
//! book and the extensions are both found beside the store, so the demo gets
//! its own of those as well, and putting it away is one `remove_dir_all`.
//!
//! Going in and coming out are relaunches rather than a swap in place. Nearly
//! everything the window holds — the list's reader, the extension host, the
//! client book, the watchers — was built for one store, and a fresh process is
//! the one way to be sure none of it still points at the other.

use anyhow::{Context as _, Result, bail};
use gpui::{AnyElement, Context, FontWeight, SharedString, Window, div, prelude::*, px, relative};
use std::{
    path::PathBuf,
    process::Command,
    sync::{
        Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::{
    accounts_ui::{ButtonKind, button},
    perf::format_count,
    settings::WindowState,
    theme::Theme,
    workspace::Workspace,
};

/// How many messages the demo holds.
///
/// Enough that the list, the search and the scrollbar are doing the work the
/// app is built for, and few enough to be written in a few seconds on a
/// laptop. `spool --demo 250000` is there for anyone who wants the big one.
pub const MESSAGES: usize = 50_000;

/// Held while the demo's directory is being written or deleted, so a build
/// and the tidy-up after an earlier visit cannot interleave.
static DIRECTORY: Mutex<()> = Mutex::new(());

/// Set once a build starts, so a tidy-up still waiting to run knows the
/// directory is wanted after all.
static WANTED: AtomicBool = AtomicBool::new(false);

pub fn directory() -> PathBuf {
    spool_core::paths::data_dir().join("demo")
}

pub fn store_path() -> PathBuf {
    directory().join("mail.db")
}

/// Writes a fresh demo, replacing any earlier one.
fn build(progress: impl FnMut(usize)) -> Result<()> {
    WANTED.store(true, Ordering::SeqCst);
    let _held = DIRECTORY.lock().unwrap_or_else(|e| e.into_inner());

    let dir = directory();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    let (_store, clients) = spool_store::demo::create(&store_path(), MESSAGES, progress)?;
    log::info!("built a demo mailbox of {MESSAGES} messages and {clients} clients");
    Ok(())
}

/// Deletes a demo left behind, a few seconds from now.
///
/// Called at launch when the demo is not the mailbox being opened. Later
/// rather than at once, because the copy that has just left the demo is
/// quitting while this one starts, and it still has the store open for its
/// last checkpoint.
pub fn clear_away_later() {
    if !directory().exists() {
        return;
    }
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(5));
        let _held = DIRECTORY.lock().unwrap_or_else(|e| e.into_inner());
        if WANTED.load(Ordering::SeqCst) {
            return;
        }
        match std::fs::remove_dir_all(directory()) {
            Ok(()) => log::info!("removed the demo mailbox"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => log::warn!("could not remove the demo mailbox: {e}"),
        }
    });
}

/// Starts another copy of Spool, for this one to hand over to.
fn start_another() -> Result<()> {
    match crate::update::installed_bundle() {
        // `-n`, as for an update: without it `open` brings forward the copy
        // that is already running, which is this one, on its way out.
        Ok(app) => {
            let status = Command::new("/usr/bin/open")
                .arg("-n")
                .arg(&app)
                .status()
                .context("starting Spool again")?;
            if !status.success() {
                bail!("could not start Spool again ({status})");
            }
        }
        // A development build, run straight out of `target/`. Its environment
        // comes along, so a run pointed at other XDG directories stays
        // pointed at them.
        Err(_) => {
            let exe = std::env::current_exe().context("finding the running executable")?;
            Command::new(exe).spawn().context("starting Spool again")?;
        }
    }
    Ok(())
}

/// Where a move into or out of the demo has got to.
#[derive(Debug, Clone, PartialEq)]
pub enum Switch {
    /// Writing the demo, with how many messages are down so far.
    Building(usize),
    /// Handing over to a fresh copy of the app.
    Restarting,
    Failed(String),
}

impl Switch {
    fn busy(&self) -> bool {
        matches!(self, Switch::Building(_) | Switch::Restarting)
    }
}

impl Workspace {
    /// Whether this is a new install: no account set up, no mail stored, and
    /// not the demo. The one time the reader shows the welcome instead of
    /// "Select a message".
    pub(crate) fn is_first_run(&self) -> bool {
        !self.demo && !self.has_accounts && self.accounts.is_empty()
    }

    /// Builds the demo and restarts into it.
    pub(crate) fn try_demo(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.switching.as_ref().is_some_and(Switch::busy) {
            return;
        }
        self.switching = Some(Switch::Building(0));
        cx.notify();

        // Measured now, while there is a window to measure: the copy that
        // opens the demo should open it where this one is.
        let geometry = crate::window_state(window);

        let (written, progress) = smol::channel::unbounded::<usize>();
        let building = cx.background_executor().spawn(async move {
            build(move |done| {
                let _ = written.try_send(done);
            })
        });

        cx.spawn(async move |this, cx| {
            // Ends when the build drops its end of the channel, which is when
            // it has finished, one way or the other.
            while let Ok(done) = progress.recv().await {
                let shown = this.update(cx, |this, cx| {
                    this.switching = Some(Switch::Building(done));
                    cx.notify();
                });
                if shown.is_err() {
                    return;
                }
            }

            let built = building.await;
            this.update(cx, |this, cx| match built {
                Ok(()) => this.restart_into(true, Some(geometry), cx),
                Err(e) => {
                    log::error!("could not build the demo mailbox: {e:#}");
                    this.switching = Some(Switch::Failed(format!("Could not build the demo: {e:#}")));
                    cx.notify();
                }
            })
            .ok();
        })
        .detach();
    }

    /// Back to the real mailbox. The demo's files go at the next launch.
    pub(crate) fn leave_demo(&mut self, window: Option<&Window>, cx: &mut Context<Self>) {
        if self.switching.as_ref().is_some_and(Switch::busy) {
            return;
        }
        let geometry = window.map(crate::window_state);
        self.restart_into(false, geometry, cx);
    }

    /// Points the next launch into the demo or out of it, and relaunches.
    fn restart_into(&mut self, demo: bool, geometry: Option<WindowState>, cx: &mut Context<Self>) {
        let before = self.settings.clone();
        self.settings.demo = demo;
        if geometry.is_some() {
            self.settings.window = geometry;
        }
        // Written here and now rather than in the background, the way other
        // preferences are: the copy about to start reads this file first
        // thing, and the answer has to be in it by then.
        if let Err(e) = self.settings.save() {
            self.settings = before;
            self.switching = Some(Switch::Failed(format!("Could not save settings: {e:#}")));
            cx.notify();
            return;
        }

        self.save_open_drafts(cx);
        self.switching = Some(Switch::Restarting);
        cx.notify();

        cx.spawn(async move |this, cx| {
            let started = cx.background_executor().spawn(async { start_another() }).await;
            match started {
                Ok(()) => {
                    cx.update(|cx| cx.quit()).ok();
                }
                Err(e) => {
                    log::error!("could not relaunch: {e:#}");
                    this.update(cx, |this, cx| {
                        // Still here, so the file goes back to describing
                        // the mailbox on screen.
                        this.settings.demo = !demo;
                        this.settings.save().ok();
                        this.switching = Some(Switch::Failed(format!("{e:#}")));
                        cx.notify();
                    })
                    .ok();
                }
            }
        })
        .detach();
    }

    /// The reader's pane on a new install: add an account, or try the demo.
    pub(crate) fn render_welcome(&self, theme: &Theme, cx: &mut Context<Self>) -> AnyElement {
        let paragraph = |text: String| {
            div().text_color(theme.text_muted).line_height(relative(1.45)).child(SharedString::from(text))
        };

        let failure = match &self.switching {
            Some(Switch::Failed(why)) => Some(why.clone()),
            _ => None,
        };

        let actions = match &self.switching {
            Some(Switch::Building(done)) => self.render_demo_progress(*done, theme),
            Some(Switch::Restarting) => div()
                .text_size(theme.small_text_size)
                .text_color(theme.text_faint)
                .child("Opening the demo\u{2026}")
                .into_any_element(),
            None | Some(Switch::Failed(_)) => div()
                .flex()
                .gap_2()
                .child(
                    button("welcome-add-account", theme, ButtonKind::Primary)
                        .on_click(cx.listener(|_, _, window, cx| {
                            // Through the action rather than straight to the
                            // window: opening Settings reads this workspace,
                            // and this is the middle of an update to it.
                            window.dispatch_action(Box::new(crate::actions::OpenSettings), cx);
                        }))
                        .child("Add an Account"),
                )
                .child(
                    button("welcome-try-demo", theme, ButtonKind::Secondary)
                        .on_click(cx.listener(|this, _, window, cx| this.try_demo(window, cx)))
                        .child("Try the Demo Mailbox"),
                )
                .into_any_element(),
        };

        div()
            .flex()
            .flex_1()
            .items_center()
            .justify_center()
            .p_8()
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_3()
                    .max_w(px(420.))
                    .child(
                        div()
                            .text_size(px(20.))
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(theme.text)
                            .child("Welcome to Spool"),
                    )
                    .child(paragraph("Add an email account to start reading your mail here.".into()))
                    .child(paragraph(format!(
                        "Or look around first in a demo mailbox: {} made-up messages, kept \
                         apart from your own. Nothing in it syncs or sends, and you can \
                         leave it whenever you like.",
                        format_count(MESSAGES)
                    )))
                    .children(failure.map(|why| {
                        div()
                            .text_size(theme.small_text_size)
                            .text_color(theme.red)
                            .child(SharedString::from(why))
                    }))
                    .child(div().pt_2().child(actions)),
            )
            .into_any_element()
    }

    fn render_demo_progress(&self, done: usize, theme: &Theme) -> AnyElement {
        // The messages are most of the work; the client book after them is a
        // moment, and a bar sitting full says the rest is nearly done.
        let label = match done >= MESSAGES {
            true => "Finishing up\u{2026}".to_string(),
            false => format!(
                "Writing {} of {} messages\u{2026}",
                format_count(done),
                format_count(MESSAGES)
            ),
        };
        let fraction = (done as f32 / MESSAGES as f32).clamp(0., 1.);

        div()
            .flex()
            .flex_col()
            .gap_2()
            .w(px(280.))
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .child(SharedString::from(label)),
            )
            .child(
                div()
                    .h(px(4.))
                    .w_full()
                    .rounded_full()
                    .bg(theme.border)
                    .child(div().h_full().rounded_full().bg(theme.accent).w(relative(fraction))),
            )
            .into_any_element()
    }

    /// At the foot of the sidebar while in the demo: what this is, and the
    /// way back out.
    ///
    /// Only for a demo chosen on the welcome screen. One opened with
    /// `spool --demo` was asked for by name, and quitting is the way out.
    pub(crate) fn render_demo_card(&self, theme: &Theme, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !(self.demo && self.settings.demo) {
            return None;
        }
        let restarting = self.switching == Some(Switch::Restarting);
        let failure = match &self.switching {
            Some(Switch::Failed(why)) => Some(why.clone()),
            _ => None,
        };

        Some(
            div()
                .id("demo-card")
                .flex()
                .flex_col()
                .gap_1()
                .flex_shrink_0()
                .mx_2()
                .mb_1()
                .px_2p5()
                .py_2()
                .rounded_md()
                .border_1()
                .border_color(theme.border)
                .bg(theme.elevated)
                .text_size(theme.small_text_size)
                .child(div().font_weight(FontWeight::MEDIUM).text_color(theme.text).child("Demo mailbox"))
                .child(
                    div()
                        .text_color(theme.text_faint)
                        .child("Made-up mail. Nothing here syncs or sends."),
                )
                .children(failure.map(|why| {
                    div().text_color(theme.red).child(SharedString::from(why))
                }))
                .child(div().pt(px(2.)).child(match restarting {
                    true => div()
                        .text_color(theme.text_faint)
                        .child("Opening your mailbox\u{2026}")
                        .into_any_element(),
                    false => div()
                        .id("demo-leave")
                        .cursor_pointer()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.accent)
                        .hover(|s| s.underline())
                        .on_click(cx.listener(|this, _, window, cx| this.leave_demo(Some(window), cx)))
                        .child("Use My Own Mail")
                        .into_any_element(),
                }))
                .into_any_element(),
        )
    }
}
