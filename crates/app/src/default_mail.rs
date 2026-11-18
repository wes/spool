//! Being the app that opens email links.
//!
//! macOS keeps one handler per URL scheme, and whichever app holds `mailto:`
//! is the one a click on an address in a browser, a PDF or a calendar invite
//! opens. Spool declares the scheme in its Info.plist, which makes it a
//! candidate; this is what makes it the choice, and what answers the link when
//! it is.
//!
//! The switch is made through `NSWorkspace` rather than by writing
//! LaunchServices' preferences: the system owns that decision, and on recent
//! versions of macOS it may ask the person to confirm it. What comes back is
//! therefore checked rather than trusted — the answer to "did it work" is
//! whoever holds the scheme afterwards.

use gpui::{AnyElement, Context, FontWeight, SharedString, div, prelude::*, px};
use spool_core::mailto::Mailto;

use crate::{theme::Theme, workspace::Workspace};

/// Who opens `mailto:` links.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Handler {
    /// Not asked yet. The question is put once the window is up, so a
    /// LaunchServices lookup is never part of what start-up waits on.
    Unknown,
    /// Spool does.
    Spool,
    /// Another app, by the name it shows in Finder, when there is one.
    Other(Option<String>),
    /// The question has no answer from here: this process is not running as
    /// Spool.app — `cargo run`, say — so there is no app for macOS to point
    /// the scheme at.
    Unbundled,
}

impl Handler {
    /// Whether making Spool the default is something that can be offered.
    pub fn can_claim(&self) -> bool {
        matches!(self, Handler::Other(_))
    }
}

/// Asks LaunchServices who holds `mailto:` now.
#[cfg(target_os = "macos")]
pub fn current() -> Handler {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSBundle, NSString, NSURL};

    let Some(ours) = NSBundle::mainBundle().bundleIdentifier() else {
        return Handler::Unbundled;
    };
    // Any address will do: the handler is chosen by the scheme.
    let Some(probe) = NSURL::URLWithString(&NSString::from_str("mailto:someone@example.com")) else {
        return Handler::Other(None);
    };
    let Some(app) = NSWorkspace::sharedWorkspace().URLForApplicationToOpenURL(&probe) else {
        return Handler::Other(None);
    };

    // By bundle identifier rather than by path, so a copy built into
    // `target/` and the one in Applications count as the same app.
    let theirs = NSBundle::bundleWithURL(&app).and_then(|bundle| bundle.bundleIdentifier());
    if theirs.is_some_and(|theirs| theirs.to_string() == ours.to_string()) {
        return Handler::Spool;
    }
    let name = app.path().and_then(|path| {
        std::path::Path::new(&path.to_string()).file_stem().map(|stem| stem.to_string_lossy().into_owned())
    });
    Handler::Other(name)
}

#[cfg(not(target_os = "macos"))]
pub fn current() -> Handler {
    Handler::Unbundled
}

/// Asks macOS to make this app the handler for `mailto:`.
///
/// The answer arrives on whatever queue LaunchServices finishes on, so it is
/// sent back over a channel rather than acted on where it lands. An error is
/// either a refusal or the person saying no to the system's prompt; either
/// way the caller re-reads [`current`] for what actually happened.
#[cfg(target_os = "macos")]
pub fn claim() -> smol::channel::Receiver<Result<(), String>> {
    use objc2_app_kit::NSWorkspace;
    use objc2_foundation::{NSBundle, NSError, NSString};

    let (done, answer) = smol::channel::bounded(1);
    let handler = block2::RcBlock::new(move |error: *mut NSError| {
        // SAFETY: LaunchServices passes either nil or a valid NSError, alive
        // for the duration of this call.
        let result = match unsafe { error.as_ref() } {
            None => Ok(()),
            Some(error) => Err(error.localizedDescription().to_string()),
        };
        let _ = done.try_send(result);
    });

    let app = NSBundle::mainBundle().bundleURL();
    NSWorkspace::sharedWorkspace().setDefaultApplicationAtURL_toOpenURLsWithScheme_completionHandler(
        &app,
        &NSString::from_str("mailto"),
        Some(&handler),
    );
    answer
}

#[cfg(not(target_os = "macos"))]
pub fn claim() -> smol::channel::Receiver<Result<(), String>> {
    let (done, answer) = smol::channel::bounded(1);
    let _ = done.try_send(Err("Only available on macOS".to_string()));
    answer
}

impl Workspace {
    /// Asks again who opens email links, and repaints if the answer changed.
    ///
    /// Cheap — LaunchServices answers from memory — so it is asked whenever
    /// the window comes forward, which is how a change made in Mail's own
    /// settings while Spool was running shows up here.
    pub(crate) fn refresh_default_mail(&mut self, cx: &mut Context<Self>) {
        if self.claiming_default_mail {
            return;
        }
        let now = current();
        if now != self.default_mail {
            self.default_mail = now;
            cx.notify();
        }
    }

    /// Makes Spool the app that opens email links.
    pub(crate) fn make_default_mail_app(&mut self, cx: &mut Context<Self>) {
        self.refresh_default_mail(cx);
        match &self.default_mail {
            Handler::Spool => {
                self.say("Spool already opens email links", cx);
                return;
            }
            Handler::Unbundled => {
                self.say("Only Spool.app can be the default email app, not a build run from cargo", cx);
                return;
            }
            Handler::Unknown | Handler::Other(_) => {}
        }
        if self.claiming_default_mail {
            return;
        }
        self.claiming_default_mail = true;
        cx.notify();

        let answer = claim();
        cx.spawn(async move |this, cx| {
            let result = answer.recv().await.unwrap_or_else(|_| Err("no answer from macOS".into()));
            this.update(cx, |this, cx| {
                this.claiming_default_mail = false;
                this.default_mail = current();
                match (&this.default_mail, result) {
                    (Handler::Spool, _) => this.say("Email links now open in Spool", cx),
                    (_, Err(why)) => {
                        log::info!("macOS did not make Spool the default email app: {why}");
                        this.say("Email links still open in another app", cx);
                    }
                    (_, Ok(())) => this.say("Email links still open in another app", cx),
                }
                cx.notify();
            })
            .ok();
        })
        .detach();
    }

    /// Puts the sidebar's offer away for good. Settings still has the button.
    pub(crate) fn decline_default_mail(&mut self, cx: &mut Context<Self>) {
        self.settings.default_mail_declined = true;
        self.persist_settings(cx);
        self.say("You can still do this in Settings \u{2192} Accounts", cx);
    }

    /// Whether the sidebar should be asking.
    ///
    /// Only once there is an account to send from: a link that opens a
    /// composer with nothing to send it from is worse than the app it took
    /// the job from.
    pub(crate) fn offers_default_mail(&self) -> bool {
        // Not from the demo: somebody trying Spool out has not decided to
        // keep it yet, and their links should not start opening in it.
        self.default_mail.can_claim()
            && !self.settings.default_mail_declined
            && !self.accounts.is_empty()
            && !self.demo
    }

    /// The offer, at the foot of the sidebar.
    ///
    /// There rather than in a dialog for the same reason an update is: it is
    /// a suggestion, and a modal in front of the mail on launch would make the
    /// one thing nobody asked for the first thing they have to deal with.
    /// Two answers and both are final — yes hands the job over, no puts the
    /// offer away for good.
    pub(crate) fn render_default_mail_offer(
        &self,
        theme: &Theme,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.offers_default_mail() {
            return None;
        }
        let now = match &self.default_mail {
            Handler::Other(Some(name)) => format!("Addresses you click elsewhere open in {name}."),
            _ => "Addresses you click elsewhere open in another app.".to_string(),
        };

        let answer = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(id)
                .cursor_pointer()
                .font_weight(if primary { FontWeight::MEDIUM } else { FontWeight::NORMAL })
                .text_color(if primary { theme.accent } else { theme.text_faint })
                .hover(|s| s.underline())
                .child(label)
        };

        Some(
            div()
                .id("default-mail-offer")
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
                .child(
                    div()
                        .font_weight(FontWeight::MEDIUM)
                        .text_color(theme.text)
                        .child("Open email links in Spool?"),
                )
                .child(div().text_color(theme.text_faint).child(SharedString::from(now)))
                .child(
                    div()
                        .flex()
                        .gap_3()
                        .pt(px(2.))
                        .children(self.claiming_default_mail.then(|| {
                            div().text_color(theme.text_faint).child("Waiting for macOS\u{2026}")
                        }))
                        .when(!self.claiming_default_mail, |row| {
                            row.child(
                                answer("default-mail-yes", "Make Default", true)
                                    .on_click(cx.listener(|this, _, _, cx| this.make_default_mail_app(cx))),
                            )
                            .child(
                                answer("default-mail-no", "No Thanks", false)
                                    .on_click(cx.listener(|this, _, _, cx| this.decline_default_mail(cx))),
                            )
                        }),
                )
                .into_any_element(),
        )
    }

    /// Answers a URL macOS handed over — a `mailto:` link somebody clicked
    /// elsewhere, now that Spool is the app that opens them.
    pub(crate) fn open_link(&mut self, url: &str, cx: &mut Context<Self>) {
        match Mailto::parse(url) {
            Some(mail) => self.compose_mailto(mail, cx),
            None => log::warn!("asked to open a link that is not mailto: {url}"),
        }
    }
}
