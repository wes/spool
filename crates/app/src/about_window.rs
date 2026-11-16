//! About Spool, in a window of its own.
//!
//! The one every Mac app has: small, fixed in size, opened from the app menu,
//! and brought forward rather than opened twice. The icon, the name, the
//! version — the thing someone reporting a problem is asked for first — and
//! the way to a newer one.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, SharedString, TitlebarOptions, Window,
    WindowBounds, WindowOptions, div, img, prelude::*, px, size,
};

use crate::{
    actions::Cancel,
    menus::CloseWindow,
    theme::ThemeAccess,
    update::Stage,
    workspace::Workspace,
};

/// Where the releases, and what changed in each, are published.
const RELEASES_URL: &str = "https://github.com/wes/spool-releases/releases";
/// Where problems are reported: the public repository's tracker.
const ISSUES_URL: &str = "https://github.com/wes/spool-releases/issues";

pub struct AboutWindow {
    workspace: Entity<Workspace>,
    focus_handle: FocusHandle,
    /// Repaints as an update check moves along, since that state lives on
    /// the workspace.
    _observation: gpui::Subscription,
}

impl AboutWindow {
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.workspace.update(cx, |workspace, _| workspace.about_window = None);
        window.remove_window();
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
    }

    fn close_action(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
    }
}

impl Focusable for AboutWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AboutWindow {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let workspace = self.workspace.clone();
        let update = workspace.read(cx).update.as_ref();

        // What checking found, when it has found anything: the same state the
        // foot of the sidebar shows, so the two never disagree.
        let (update_line, update_action) = match update {
            Some(state) => (
                Some(SharedString::from(state.label())),
                state.action().map(SharedString::from),
            ),
            None => (None, None),
        };
        let failed = update.is_some_and(|s| matches!(s.stage, Stage::Failed(_)));
        let busy = update.is_some_and(|s| {
            matches!(s.stage, Stage::Checking | Stage::Downloading(_) | Stage::Installing(_))
        });

        let link = |id: &'static str, label: &'static str, url: &'static str| {
            div()
                .id(id)
                .cursor_pointer()
                .text_color(theme.accent)
                .hover(|s| s.underline())
                .on_click(move |_, _, cx| cx.open_url(url))
                .child(label)
        };

        div()
            .key_context("About")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::close_action))
            .size_full()
            .flex()
            .flex_col()
            .items_center()
            // Clear of the traffic lights, which sit over the content: the
            // title bar is part of the window here, as it is in every About.
            .pt(px(36.))
            .px_6()
            .pb_5()
            .gap_1()
            .bg(theme.background)
            .font_family(theme.font)
            .text_size(theme.text_size)
            .text_color(theme.text)
            .child(img(crate::icons::APP_ICON).size(px(112.)).mb_2())
            .child(div().text_size(px(20.)).font_weight(gpui::FontWeight::SEMIBOLD).child("Spool"))
            .child(
                div()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_muted)
                    .child(SharedString::from(format!("Version {}", crate::update::current()))),
            )
            .child(
                div()
                    .mt_3()
                    .text_size(theme.small_text_size)
                    .text_color(theme.text_faint)
                    .text_center()
                    .child("Mail for the Mac, from the keyboard."),
            )
            .child(div().flex_1())
            // The way to a newer one: the button while nothing is under way,
            // and what checking found once it has.
            .child(
                div()
                    .flex()
                    .flex_col()
                    .items_center()
                    .gap_1p5()
                    .children((!busy && update_action.is_none()).then(|| {
                        crate::settings_ui::toggle("about-check", &theme, false)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.workspace.update(cx, |workspace, cx| workspace.check_for_updates(true, cx));
                            }))
                            .child("Check for Updates\u{2026}")
                    }))
                    .children(update_line.map(|line| {
                        div()
                            .text_size(theme.small_text_size)
                            .text_center()
                            .text_color(if failed { theme.red } else { theme.text_muted })
                            .child(line)
                    }))
                    .children(update_action.map(|label| {
                        crate::settings_ui::toggle("about-update", &theme, true)
                            .on_click(cx.listener(|this, _, _, cx| {
                                this.workspace.update(cx, |workspace, cx| workspace.advance_update(cx));
                            }))
                            .child(label)
                    })),
            )
            .child(
                div()
                    .mt_4()
                    .flex()
                    .gap_3()
                    .text_size(theme.small_text_size)
                    .child(link("about-notes", "Release Notes", RELEASES_URL))
                    .child(link("about-issues", "Report an Issue", ISSUES_URL)),
            )
            .child(
                div()
                    .mt_2()
                    .text_size(px(10.))
                    .text_color(theme.text_faint)
                    .child("\u{a9} 2026 Wes Edling \u{b7} MIT License"),
            )
    }
}

/// Opens the About window, or brings it forward if it is already open.
///
/// A free function taking the workspace, like `settings_window::open`, for
/// the same reason: opening a window draws it once, and this one reads the
/// workspace while it draws.
pub fn open(workspace: Entity<Workspace>, cx: &mut App) {
    let existing = workspace.read(cx).about_window;
    if let Some(open) = existing
        && open.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return;
    }

    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            // No title: an About window's title bar is part of the window,
            // with the traffic lights sitting over its top edge.
            title: None,
            appears_transparent: true,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(None, size(px(300.), px(400.)), cx))),
        // Fixed, as About windows are: nothing in it grows, so there is
        // nothing to resize it for, and nothing to minimise it to.
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };

    let opened = cx.open_window(options, {
        let workspace = workspace.clone();
        move |_, cx| {
            cx.new(|cx| AboutWindow {
                _observation: cx.observe(&workspace, |_, _, cx| cx.notify()),
                workspace,
                focus_handle: cx.focus_handle(),
            })
        }
    });

    if let Ok(handle) = opened {
        handle
            .update(cx, |about, window, cx| {
                window.focus(&about.focus_handle);
                // The red button closes it without going through `close`.
                let workspace = about.workspace.clone();
                window.on_window_should_close(cx, move |_, cx| {
                    workspace.update(cx, |workspace, _| workspace.about_window = None);
                    true
                });
            })
            .ok();
        workspace.update(cx, |workspace, _| workspace.about_window = Some(handle));
    }
}
