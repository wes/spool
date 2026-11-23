//! Settings, in a window of its own.
//!
//! The same argument as the composer: settings are a second thing you are
//! doing, usually *because* of something in the first. A pane covering the
//! mailbox is a pane you cannot check anything against — which account that
//! message came from, whether the theme you are trying reads at a glance.
//!
//! The state stays on the [`Workspace`]. Only the window is new: this view
//! owns nothing but a handle, and asks the workspace to draw its own settings
//! pane. That keeps every button in `settings_ui` and `accounts_ui` bound to
//! the entity whose state they change, rather than routing a thousand lines of
//! form handling through a second view.

use gpui::{
    App, Bounds, Context, Entity, FocusHandle, Focusable, TitlebarOptions, Window, WindowBounds,
    WindowOptions, div, prelude::*, px, size,
};

use crate::{
    actions::Cancel,
    menus::CloseWindow,
    theme::ThemeAccess,
    workspace::Workspace,
};

pub struct SettingsWindow {
    workspace: Entity<Workspace>,
    focus_handle: FocusHandle,
    /// Repaints this window whenever the workspace changes.
    ///
    /// Required, not an optimisation: every control in the pane mutates the
    /// workspace and notifies *it*, which repaints the mailbox. Without this
    /// the settings window would draw once and then never again — a section
    /// click that does nothing, a form that does not fill in.
    _observation: gpui::Subscription,
}

impl SettingsWindow {
    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        // The workspace owns what "closed" means, and is the thing that knows
        // what was saved.
        self.workspace.update(cx, |workspace, cx| workspace.settings_closed(cx));
        window.remove_window();
    }

    fn cancel(&mut self, _: &Cancel, window: &mut Window, cx: &mut Context<Self>) {
        // Escape unwinds one layer: the account editor first, then the window.
        let editing = self.workspace.read(cx).editing_account();
        if editing {
            self.workspace.update(cx, |workspace, cx| workspace.close_account_editor(cx));
        } else {
            self.close(window, cx);
        }
    }

    fn close_action(&mut self, _: &CloseWindow, window: &mut Window, cx: &mut Context<Self>) {
        self.close(window, cx);
    }
}

impl Focusable for SettingsWindow {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let theme = cx.theme().clone();
        let workspace = self.workspace.clone();

        // Drawn by the workspace, in the workspace's own context, so every
        // listener inside is bound to the entity it changes.
        let pane = workspace.update(cx, |workspace, cx| {
            workspace.render_settings(&theme, window, cx)
        });

        div()
            .key_context("Settings")
            .track_focus(&self.focus_handle)
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::close_action))
            .size_full()
            .flex()
            .flex_col()
            .bg(theme.background)
            .font_family(theme.font)
            .text_size(theme.text_size)
            .text_color(theme.text)
            .line_height(px(18.))
            .children(pane)
    }
}

/// Opens the settings window, or brings it forward if it is already open.
///
/// A free function taking the workspace rather than a method on it, and that
/// matters: `open_window` draws the new window once before it returns, and the
/// first thing this window draws is the workspace's own settings pane — which
/// borrows the workspace. Opening it from inside a workspace update is a
/// nested update, and gpui panics on those rather than aliasing. So the state
/// is prepared in one update, and the window is opened outside of it.
/// Opens the window on one section — from a composer that wants the writing
/// tools set up, say, where landing on Accounts would be a hunt.
pub fn open_at(workspace: Entity<Workspace>, section: crate::settings_ui::Section, cx: &mut App) {
    open(workspace.clone(), cx);
    workspace.update(cx, |workspace, cx| {
        if let Some(state) = workspace.settings_ui.as_mut() {
            state.section = section;
        }
        cx.notify();
    });
    if section == crate::settings_ui::Section::Ai {
        workspace.update(cx, |workspace, cx| workspace.prepare_ai_settings(cx));
    }
}

pub fn open(workspace: Entity<Workspace>, cx: &mut App) {
    let existing = workspace.read(cx).settings_window;
    if let Some(open) = existing
        && open.update(cx, |_, window, _| window.activate_window()).is_ok()
    {
        return;
    }

    workspace.update(cx, |workspace, cx| {
        workspace.palette = None;
        workspace.settings_ui = Some(crate::settings_ui::SettingsState::default());
        // The pane opens on Accounts, so the file has to be read now rather
        // than when the section is clicked.
        workspace.load_accounts(cx);
        // The settings window coming forward is not the mailbox coming
        // forward, so the answer is asked for here too.
        workspace.refresh_default_mail(cx);
    });

    let options = WindowOptions {
        titlebar: Some(TitlebarOptions {
            title: Some("Settings".into()),
            // The system's title bar, like the composer: this is an ordinary
            // window, not a sheet the mailbox is wearing.
            appears_transparent: false,
            traffic_light_position: None,
        }),
        window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
            None,
            size(px(760.), px(620.)),
            cx,
        ))),
        window_min_size: Some(size(px(560.), px(400.))),
        ..Default::default()
    };

    let opened = cx.open_window(options, {
        let workspace = workspace.clone();
        move |_, cx| {
            cx.new(|cx| SettingsWindow {
                _observation: cx.observe(&workspace, |_, _, cx| cx.notify()),
                workspace,
                focus_handle: cx.focus_handle(),
            })
        }
    });

    match opened {
        Ok(handle) => {
            handle
                .update(cx, |settings, window, cx| {
                    window.focus(&settings.focus_handle);

                    // Closed by the red button as well as by escape, and the
                    // workspace has to hear about it either way.
                    let workspace = settings.workspace.clone();
                    window.on_window_should_close(cx, move |_, cx| {
                        workspace.update(cx, |workspace, cx| workspace.settings_closed(cx));
                        true
                    });
                })
                .ok();
            workspace.update(cx, |workspace, _| workspace.settings_window = Some(handle));
        }
        Err(e) => {
            workspace.update(cx, |workspace, cx| {
                workspace.settings_ui = None;
                workspace.status = format!("Could not open settings: {e}");
                cx.notify();
            });
        }
    }
}

impl Workspace {
    /// What the window closing means here: the pane's state goes, and the
    /// workspace forgets the window it was in.
    ///
    /// The theme needs no undoing. Choosing one commits it, so what is on
    /// screen when the window closes is what was saved.
    pub fn settings_closed(&mut self, cx: &mut Context<Self>) {
        self.settings_ui = None;
        self.settings_window = None;
        cx.notify();
    }
}
