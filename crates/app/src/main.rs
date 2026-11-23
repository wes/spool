//! Spool — an email client built on gpui, the GPU-accelerated UI framework
//! Zed is written in.
//!
//! Usage:
//!   spool                    open the store at ~/.local/share/spool
//!   spool --demo 100000      generate a synthetic mailbox and open it
//!   spool --db path.db       use a specific store
//!   spool --bench 100000     measure ingest and query, print, exit

mod badge;
mod folder_menu;
mod notify;
mod accounts_ui;
mod actions;
mod assist;
mod activity;
mod caret;
mod chrome;
mod client_sync;
mod clients;
mod clients_board;
mod clients_import;
mod clients_mail;
mod clients_profile;
mod clients_window;
mod crash;
mod default_mail;
mod demo;
mod extension_flow;
mod extension_ui;
mod extensions;
mod clock;
mod compose;
mod flight;
mod folder_tree;
mod folder_ui;
mod icons;
mod images;
mod message_list;
mod menus;
mod message_view;
mod palette;
mod perf;
mod priority;
mod follow_up;
mod query_worker;
mod recipients;
mod scrollbar;
mod settings;
mod settings_ui;
mod settings_window;
mod about_window;
mod sidebar;
mod sidebar_nav;
mod sync_flow;
mod text_area;
mod text_motion;
mod text_input;
mod theme;
mod unsubscribe;
mod update;
#[cfg(target_os = "macos")]
mod webview;
#[cfg(target_os = "macos")]
mod attachment_drag;
mod html_editor;
mod workspace;

use anyhow::{Context as _, Result};
use gpui::{
    App, Application, Bounds, Pixels, TitlebarOptions, WindowBounds, WindowHandle, WindowOptions,
    point, prelude::*, px, size,
};
use spool_store::Store;
use std::{path::PathBuf, time::Instant};

use crate::{
    settings::{Settings, WindowState},
    theme::{ActiveTheme, Theme},
    workspace::Workspace,
};

struct Args {
    db: Option<PathBuf>,
    demo: Option<usize>,
    bench: Option<usize>,
    write_config: bool,
    /// Open Clients at launch: `Some(true)` on the board.
    clients: Option<bool>,
}

fn parse_args() -> Result<Args> {
    let mut args = Args { db: None, demo: None, bench: None, write_config: false, clients: None };
    let mut argv = std::env::args().skip(1);

    while let Some(arg) = argv.next() {
        match arg.as_str() {
            "--db" => {
                args.db = Some(
                    argv.next().context("--db needs a path")?.into(),
                );
            }
            "--demo" => {
                let n = argv.next().context("--demo needs a message count")?;
                args.demo = Some(n.parse().context("--demo needs a number")?);
            }
            "--bench" => {
                let n = argv.next().context("--bench needs a message count")?;
                args.bench = Some(n.parse().context("--bench needs a number")?);
            }
            "--write-config" => args.write_config = true,
            "--clients" => args.clients = Some(false),
            "--board" => args.clients = Some(true),
            "-h" | "--help" => {
                println!("{}", include_str!("../help.txt"));
                std::process::exit(0);
            }
            other => anyhow::bail!("unrecognized argument: {other}"),
        }
    }
    Ok(args)
}

/// `~/.local/share/spool/mail.db`, or the platform equivalent.
///
/// A variable that is set but empty counts as unset: taking it literally
/// would put the mail store in whatever directory the app started in.
fn default_db_path() -> PathBuf {
    let non_empty = |name: &str| {
        std::env::var_os(name).filter(|v| !v.is_empty()).map(PathBuf::from)
    };

    non_empty("XDG_DATA_HOME")
        .or_else(|| non_empty("HOME").map(|home| home.join(".local/share")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("spool/mail.db")
}

fn main() -> Result<()> {
    // The clock start-up is measured against. Taken first so that everything
    // between here and the first frame is inside it, including the parts that
    // are nobody's code — reading preferences, opening a database whose pages
    // may or may not still be in the page cache.
    //
    // Start-up is the one timing this app did not measure, and the one users
    // notice first: it is a number that swings by a factor of thirty between
    // a cold page cache and a warm one, and without the phases broken out
    // there is no way to tell which phase swung. `RUST_LOG=spool=debug` prints
    // them.
    let launched = Instant::now();

    // Before anything else: most of this app runs inside Cocoa callbacks,
    // where a panic cannot unwind and the process aborts with the message
    // lost. The hook is the last place it exists.
    crash::install();

    env_logger::Builder::from_env(
        env_logger::Env::default().default_filter_or("spool=info,warn"),
    )
    .init();

    let args = parse_args()?;

    if args.write_config {
        return write_example_config();
    }

    if let Some(count) = args.bench {
        return run_benchmark(count);
    }

    // Preferences are read before the window exists, so the first frame is
    // already painted in the chosen theme rather than flashing the default —
    // and before the store, because they say whether that is the demo.
    let mut settings = Settings::load();

    // The demo somebody chose on the welcome screen is where Spool opens until
    // they leave it, unless a store was asked for by name.
    let named = args.db.is_some() || args.demo.is_some();
    if !named && settings.demo && !demo::store_path().exists() {
        // Deleted from under it: back to the real mailbox, not an empty demo.
        settings.demo = false;
        if let Err(e) = settings.save() {
            log::warn!("could not forget the missing demo: {e:#}");
        }
    }
    let tried_demo = !named && settings.demo;
    if !named && !tried_demo {
        demo::clear_away_later();
    }

    // A demo corpus goes to its own database so it can never be confused with,
    // or write into, real mail.
    let (path, seed_with) = match args.demo {
        Some(count) => (
            args.db.unwrap_or_else(|| std::env::temp_dir().join("spool-demo.db")),
            Some(count),
        ),
        None if tried_demo => (demo::store_path(), None),
        None => (args.db.unwrap_or_else(default_db_path), None),
    };

    let store = if let Some(count) = seed_with {
        // Regenerating is the point of the flag, so it starts from empty.
        let started = Instant::now();
        let (store, clients) = spool_store::demo::create(&path, count, |_| {})?;
        let elapsed = started.elapsed();
        println!(
            "generated {count} messages and {clients} clients in {:.2}s ({:.0} msg/s) -> {}",
            elapsed.as_secs_f64(),
            count as f64 / elapsed.as_secs_f64(),
            path.display(),
        );
        store
    } else {
        let opening = Instant::now();
        let store = Store::open(&path)?;
        log::debug!("store opened in {:?}", opening.elapsed());
        store
    };

    // Before anything can sync: mail stored from here on is indexed by who is
    // on it only if Clients has been added, and not at all otherwise.
    store.set_people_index(settings.clients)?;

    if seed_with.is_none() && !tried_demo && store.is_empty()? {
        println!(
            "The store at {} is empty. Try:\n    spool --demo 100000",
            path.display()
        );
    }

    // Where extensions are installed, worked out from the store's own path so
    // that `--db somewhere/else.db` gets that directory's extensions rather
    // than the ones belonging to the real mailbox.
    let extensions_root = extensions::directory(&path);

    let open_clients = args.clients;
    if open_clients.is_some() && !settings.clients {
        println!("Clients is not added. Add it in Settings \u{2192} Extensions, then try again.");
    }

    log::debug!("ready to open a window {:?} after launch", launched.elapsed());

    // Links macOS hands over — a `mailto:` clicked in a browser, once Spool is
    // the app that opens them. Registered before the app runs, because a
    // click is what launched it when Spool was not already open, and that
    // link arrives during launch rather than after it. A channel, because
    // the callback is given no `App` to act with, and the mailbox it is meant
    // for may not exist yet when it fires.
    let (links, arrived_links) = smol::channel::unbounded::<Vec<String>>();
    let application = Application::new().with_assets(icons::Icons);
    application.on_open_urls(move |urls| {
        let _ = links.try_send(urls);
    });

    application.run(move |cx: &mut App| {
        actions::bind_keys(cx);
        text_input::bind_keys(cx);
        text_area::bind_keys(cx);
        menus::bind_keys(cx);
        menus::init(settings.clients, cx);
        cx.set_global(ActiveTheme(Theme::new(settings.theme)));
        assist::init(&settings, cx);

        // Size to the display rather than to a fixed guess: a three-pane
        // layout needs width, but a window larger than the screen is worse
        // than a small one.
        let display = cx.primary_display().map(|d| d.bounds());

        // Come back to where you left the window. Falls back to a centred
        // default when nothing is remembered yet, or when what is remembered
        // no longer fits the displays that are actually attached.
        let window_bounds = restore_bounds(settings.window, display);

        let mailbox = cx.open_window(
            WindowOptions {
                window_bounds: Some(window_bounds),
                titlebar: Some(TitlebarOptions {
                    title: Some("Spool".into()),
                    // Draw our own title bar, the way Zed does, so the search
                    // field and scope live in the same strip as the traffic
                    // lights instead of below a second bar.
                    appears_transparent: true,
                    traffic_light_position: Some(point(px(12.), px(12.))),
                }),
                window_min_size: Some(size(px(760.), px(480.))),
                ..Default::default()
            },
            |window, cx| {
                cx.new(|cx| {
                    window.set_rem_size(px(16.));
                    Workspace::new(
                        store.clone(),
                        settings.clone(),
                        extensions_root.clone(),
                        seed_with.is_some() || tried_demo,
                        window,
                        cx,
                    )
                })
            },
        )
        .expect("failed to open window");

        // The end of the part anyone is waiting on. Everything after this line
        // — the sync, the update check, the handlers below — happens with a
        // window already on screen.
        log::debug!("window open {:?} after launch", launched.elapsed());

        // The workspace entity, taken once and held.
        //
        // Not the window handle: these handlers run *during* the front
        // window's own update, and a window on the update stack has been taken
        // out of the app. Reaching back through the handle for the workspace
        // therefore fails — always, and only from inside a handler like this
        // one, which is exactly where these run. The entity has no such
        // problem, and no handler below needs the window anyway.
        let workspace = mailbox.entity(cx).expect("the mailbox window was just opened");

        // Quit is handled here rather than on the workspace's own element,
        // because a composer window has no workspace in its chain and ⌘Q has
        // to work from whichever window happens to be in front.
        // Settings, like Quit, has to work from whichever window is in front —
        // a composer has no workspace in its dispatch chain, and only the
        // mailbox knows how to open the settings window.
        cx.on_action({
            let workspace = workspace.clone();
            move |_: &crate::actions::OpenSettings, cx: &mut App| {
                crate::settings_window::open(workspace.clone(), cx);
            }
        });

        // The client book, beside the store, and its background work: the
        // people index fills in and sources are pulled whether or not the
        // window is open. Only once Clients has been added; otherwise none of
        // it exists, and the handlers below have nothing to open.
        if settings.clients {
            clients::init(store.clone(), cx);
        }
        workspace.update(cx, |_, cx| {
            // The mailbox draws a client's stage beside their name, so it
            // repaints when the book changes.
            cx.observe_global::<clients::ClientBook>(|_, cx| cx.notify()).detach();
        });

        // Clients, like Settings, from whichever window is in front — on the
        // sender of the message being read in the mailbox.
        cx.on_action({
            let workspace = workspace.clone();
            move |_: &crate::actions::OpenClients, cx: &mut App| {
                let workspace_now = workspace.read(cx);
                if !workspace_now.settings.clients {
                    return;
                }
                let person = workspace_now.selected_person();
                crate::clients_window::open(workspace.clone(), person, false, cx);
            }
        });
        cx.on_action({
            let workspace = workspace.clone();
            move |_: &crate::actions::OpenClientBoard, cx: &mut App| {
                if workspace.read(cx).settings.clients {
                    crate::clients_window::open(workspace.clone(), None, true, cx);
                }
            }
        });

        if let Some(board) = open_clients.filter(|_| settings.clients) {
            crate::clients_window::open(workspace.clone(), None, board, cx);
        }

        // A link handed over by macOS opens a composer from the mailbox,
        // whichever window is in front.
        cx.spawn({
            let workspace = workspace.clone();
            async move |cx| {
                while let Ok(urls) = arrived_links.recv().await {
                    let handled = workspace.update(cx, |workspace, cx| {
                        for url in &urls {
                            workspace.open_link(url, cx);
                        }
                    });
                    if handled.is_err() {
                        break;
                    }
                }
            }
        })
        .detach();

        // Making Spool that app, from the palette, whichever window is in
        // front.
        cx.on_action({
            let workspace = workspace.clone();
            move |_: &crate::actions::MakeDefaultMailApp, cx: &mut App| {
                workspace.update(cx, |workspace, cx| workspace.make_default_mail_app(cx));
            }
        });

        // Who opens email links, asked now that the window is up, and again
        // whenever it comes forward — that is how a change made in another
        // app's settings while this one was running is noticed.
        mailbox
            .update(cx, |workspace, window, cx| {
                workspace.refresh_default_mail(cx);
                cx.observe_window_activation(window, |workspace, window, cx| {
                    if window.is_window_active() {
                        workspace.refresh_default_mail(cx);
                    }
                })
                .detach();
            })
            .expect("the mailbox window was just opened");

        // About, like Settings, from whichever window is in front.
        cx.on_action({
            let workspace = workspace.clone();
            move |_: &crate::actions::AboutSpool, cx: &mut App| {
                crate::about_window::open(workspace.clone(), cx);
            }
        });

        cx.on_action({
            let workspace = workspace.clone();
            move |_: &crate::actions::Quit, cx: &mut App| {
                // The composers' autosave is debounced, so the last sentence
                // typed before ⌘Q has not necessarily been written yet.
                workspace.update(cx, |workspace, cx| workspace.save_open_drafts(cx));
                cx.quit();
            }
        });

        // The red button does not go through this app's close action, and it
        // is the last moment the window can be measured — after this the
        // handle is gone and its geometry with it.
        mailbox
            .update(cx, |_, window, cx| {
                let workspace = workspace.clone();
                window.on_window_should_close(cx, move |window, cx| {
                    workspace.update(cx, |workspace, cx| workspace.window_closing(window, cx));
                    true
                });
            })
            .expect("the mailbox window was just opened");

        // Spool has no way to reopen a mailbox it has closed — no dock menu,
        // no Window item — so an app left running without one is a process
        // you can only reach with ⌘Q. Closing the last window quits, which is
        // what ⌘W already promises and what the red button should mean too.
        //
        // Deferred rather than quit on the spot: this observer runs part-way
        // through the closing window's own update, and starting the app's
        // teardown from inside that is asking gpui to unwind two things at
        // once. A turn later the close has finished and the quit is an
        // ordinary one.
        cx.on_window_closed(|cx: &mut App| {
            if cx.windows().is_empty() {
                cx.defer(|cx| {
                    // Checked again on the way in: a window opened in the
                    // meantime is a reason not to quit after all.
                    if cx.windows().is_empty() {
                        cx.quit();
                    }
                });
            }
        })
        .detach();

        // Geometry is read at quit rather than tracked every frame: it only
        // has to be right once, and polling the platform for a window rect at
        // 120Hz to catch a value that changes when you drag is wasteful.
        //
        // Quitting with the mailbox already closed is not one of those times:
        // it was measured on the way out, and there is nothing here to read.
        cx.on_app_quit(move |cx: &mut App| {
            let geometry = window_geometry(mailbox, cx);
            // Cloned rather than moved because this handler is an `FnMut`:
            // the store has to survive being called again. It is a handle, so
            // the clone costs nothing.
            let store = store.clone();
            async move {
                if let Some(window) = geometry {
                    let mut settings = Settings::load();
                    if settings.window != Some(window) {
                        settings.window = Some(window);
                        if let Err(e) = settings.save() {
                            log::warn!("could not remember the window position: {e:#}");
                        }
                    }
                }

                // Tidy the write-ahead log away, so the next launch opens a
                // database rather than recovering one. Failure is not worth
                // reporting: nothing is lost by skipping it, the mail is
                // already committed, and the only cost is that the next start
                // is the slow kind.
                if let Err(e) = store.checkpoint() {
                    log::debug!("could not check the write-ahead log in: {e:#}");
                }
            }
        })
        .detach();

        cx.activate(true);
    });

    Ok(())
}

/// Turns remembered geometry into the bounds to open with.
fn restore_bounds(
    remembered: Option<WindowState>,
    display: Option<Bounds<Pixels>>,
) -> WindowBounds {
    let usable = remembered.filter(|state| match display {
        Some(display) => state.is_usable_on(settings::Bounds {
            x: f32::from(display.origin.x),
            y: f32::from(display.origin.y),
            width: f32::from(display.size.width),
            height: f32::from(display.size.height),
        }),
        // With no display to check against, trust what was stored rather than
        // discarding it — the alternative is ignoring the setting entirely.
        None => true,
    });

    match usable {
        Some(state) => {
            let bounds = Bounds::new(
                point(px(state.x), px(state.y)),
                size(px(state.width), px(state.height)),
            );
            if state.maximized {
                WindowBounds::Maximized(bounds)
            } else {
                WindowBounds::Windowed(bounds)
            }
        }
        None => WindowBounds::Windowed(default_bounds(display)),
    }
}

/// Where a window goes when nothing is remembered: a comfortable fraction of
/// the display, centred.
fn default_bounds(display: Option<Bounds<Pixels>>) -> Bounds<Pixels> {
    let Some(display) = display else {
        return Bounds::new(point(px(40.), px(40.)), size(px(1_280.), px(820.)));
    };

    let window = size(
        (display.size.width * 0.88).min(px(1_500.)),
        (display.size.height * 0.88).min(px(950.)),
    );
    // Centred by hand: `Bounds::centered` lands the window partly off-screen
    // on a scaled display.
    let origin = point(
        display.origin.x + (display.size.width - window.width).max(px(0.)) / 2.,
        display.origin.y + (display.size.height - window.height).max(px(0.)) / 2.,
    );
    Bounds::new(origin, window)
}

/// Reads the mailbox window's geometry, or `None` once it has closed.
///
/// The mailbox by handle rather than whichever window comes first: a composer
/// or the settings sheet is also a window, and remembering one of those as
/// where the mailbox goes is how the app reopens the size of a dialog.
fn window_geometry(mailbox: WindowHandle<Workspace>, cx: &mut App) -> Option<WindowState> {
    mailbox.update(cx, |_, window, _| window_state(window)).ok()
}

/// A window's geometry, in the form it is remembered in.
pub(crate) fn window_state(window: &gpui::Window) -> WindowState {
    let (bounds, maximized) = match window.window_bounds() {
        WindowBounds::Windowed(bounds) => (bounds, false),
        // Both remember the rectangle to un-maximize back to; only the
        // maximized case reopens filling the screen.
        WindowBounds::Maximized(bounds) => (bounds, true),
        WindowBounds::Fullscreen(bounds) => (bounds, false),
    };

    WindowState {
        x: f32::from(bounds.origin.x),
        y: f32::from(bounds.origin.y),
        width: f32::from(bounds.size.width),
        height: f32::from(bounds.size.height),
        maximized,
    }
}

/// Writes a commented example `accounts.toml`, without clobbering a real one.
fn write_example_config() -> Result<()> {
    let path = spool_net::Config::default_path();

    if path.exists() {
        println!("{} already exists; leaving it alone.", path.display());
        return Ok(());
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("creating {}", parent.display()))?;
    }
    std::fs::write(&path, spool_net::Config::example())
        .with_context(|| format!("writing {}", path.display()))?;

    println!("Wrote {}. Edit it, then run spool and press cmd-r.", path.display());
    Ok(())
}

/// Headless timing run. Prints the numbers the UI advertises so they can be
/// checked in CI rather than taken on faith.
fn run_benchmark(count: usize) -> Result<()> {
    use spool_store::{Query, Scope};

    let path = std::env::temp_dir().join("spool-bench.db");
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }

    let store = Store::open(&path)?;

    let started = Instant::now();
    spool_store::demo::populate(&store, count, spool_store::demo::SEED)?;
    let ingest = started.elapsed();

    println!("ingest        {count} messages in {:.2}s", ingest.as_secs_f64());
    println!("              {:.0} messages/sec", count as f64 / ingest.as_secs_f64());
    println!("on disk       {}", spool_core::format_size(
        store.size_on_disk()?.min(u32::MAX as u64) as u32
    ));

    let time = |label: &str, f: &dyn Fn() -> Result<usize>| -> Result<()> {
        // Run twice and report the second: the first pays for page-cache
        // misses that a running app would have paid at startup.
        f()?;
        let started = Instant::now();
        let rows = f()?;
        println!(
            "{label:<14}{:>8} in {}",
            crate::perf::format_count(rows),
            crate::perf::format_duration(started.elapsed())
        );
        Ok(())
    };

    time("list all", &|| Ok(store.envelopes(Scope::Everything, &Query::All, 500_000)?.len()))?;
    time("list unread", &|| {
        Ok(store.envelopes(Scope::Everything, &Query::Unread, 500_000)?.len())
    })?;
    // Terms drawn from the generated corpus: one common, one rare, one phrase.
    for term in ["deploy", "postmortem", "security audit"] {
        time(&format!("search {term:.9}"), &|| {
            Ok(store
                .envelopes(Scope::Everything, &Query::Text(term.into()), 500_000)?
                .len())
        })?;
    }

    Ok(())
}
