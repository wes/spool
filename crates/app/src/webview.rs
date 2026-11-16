//! A real web view for HTML mail, on macOS.
//!
//! The block renderer elsewhere in this app is faster, safer and portable, and
//! it is still what search, snippets and plain-text mail go through. But a
//! newsletter is a document its sender laid out in CSS, and no reduction of it
//! to headings and paragraphs is going to look like the thing they sent. This
//! is a `WKWebView`, positioned over the reader pane.
//!
//! Three things make that defensible rather than reckless:
//!
//! * **The document is sanitized before it is ever loaded.** See
//!   [`spool_core::sanitize`] — scripts, frames, event handlers and, until
//!   the reader says otherwise, every remote URL are gone from the source text
//!   rather than merely disallowed at run time.
//! * **Scripting is off** in the configuration, so the sanitizer is the second
//!   line rather than the only one.
//! * **The view never navigates.** The only load it performs is the one this
//!   code hands it; a clicked link goes to the browser, which is where a
//!   stranger's URL belongs.
//!
//! The mechanics are the awkward part. gpui draws into a Metal layer, so a
//! native subview cannot be composited *into* the scene — it sits on top of
//! it, in a rectangle this code has to keep in step with the layout. Hence
//! [`WebViewElement`], which exists to learn its own bounds during layout and
//! move the native view to match.

#![cfg(target_os = "macos")]

/// The earliest a hand-over can be called settled.
///
/// One frame at 60Hz. `loadHTMLString` returns before WebKit has necessarily
/// started, so asking `isLoading` immediately can answer "no" about a document
/// that has not begun to replace the one on screen.
const SETTLE_FLOOR: std::time::Duration = std::time::Duration::from_millis(16);

/// And the longest. A document that never finishes must not leave the reader
/// blank for ever; whatever WebKit has by then is better than nothing.
const SETTLE_LIMIT: std::time::Duration = std::time::Duration::from_millis(2_000);

use std::{cell::RefCell, rc::Rc};

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, Window,
};
use objc2::{
    MainThreadOnly, Message, define_class, msg_send,
    rc::Retained,
    runtime::ProtocolObject,
};
use objc2_app_kit::{NSView, NSWorkspace};
use objc2_foundation::{
    MainThreadMarker, NSObject, NSObjectProtocol, NSRect, NSString,
};
use objc2_web_kit::{
    WKNavigationAction, WKNavigationActionPolicy, WKNavigationDelegate, WKNavigationType,
    WKUIDelegate, WKWebView, WKWebViewConfiguration, WKWindowFeatures,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};

// The delegate that keeps the view from going anywhere.
//
// A web view's default answer to a click is to navigate, which would load a
// stranger's page inside the reader — the one thing this must never do. Every
// navigation but the initial `loadHTMLString` is refused, and a click is handed
// to the browser instead.
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "MailboxNavigationGuard"]
    pub(crate) struct NavigationGuard;

    unsafe impl NSObjectProtocol for NavigationGuard {}

    unsafe impl WKNavigationDelegate for NavigationGuard {
        #[unsafe(method(webView:decidePolicyForNavigationAction:decisionHandler:))]
        unsafe fn decide_policy(
            &self,
            _web_view: &WKWebView,
            action: &WKNavigationAction,
            handler: &block2::DynBlock<dyn Fn(WKNavigationActionPolicy)>,
        ) {
            // Only the load this code performed is allowed, and it is
            // recognised by its URL rather than by its type: a meta refresh
            // and a scripted redirect are also reported as `Other`, and
            // trusting the type would let a document move itself somewhere
            // remote. `loadHTMLString` navigates to `about:blank`.
            let policy = unsafe {
                let url = action.request().URL();
                let is_initial_load = url
                    .as_ref()
                    .and_then(|url| url.scheme())
                    .is_none_or(|scheme| scheme.to_string() == "about");

                if is_initial_load {
                    WKNavigationActionPolicy::Allow
                } else {
                    // A link the reader clicked goes to the browser; anything
                    // else the document tried on its own goes nowhere.
                    if action.navigationType() == WKNavigationType::LinkActivated
                        && let Some(url) = url
                    {
                        NSWorkspace::sharedWorkspace().openURL(&url);
                    }
                    WKNavigationActionPolicy::Cancel
                }
            };
            handler.call((policy,));
        }
    }

    // Every link the sanitizer lets through carries `target="_blank"`, and a
    // click on one of those is not a navigation of this view at all: WebKit
    // asks for a new window, and with no UI delegate to ask it quietly drops
    // the click. So the guard answers that too — the URL goes to the browser
    // and no window is made.
    unsafe impl WKUIDelegate for NavigationGuard {
        #[unsafe(method(webView:createWebViewWithConfiguration:forNavigationAction:windowFeatures:))]
        unsafe fn create_web_view(
            &self,
            _web_view: &WKWebView,
            _configuration: &WKWebViewConfiguration,
            action: &WKNavigationAction,
            _features: &WKWindowFeatures,
        ) -> *mut WKWebView {
            unsafe {
                if action.navigationType() == WKNavigationType::LinkActivated
                    && let Some(url) = action.request().URL()
                {
                    NSWorkspace::sharedWorkspace().openURL(&url);
                }
            }
            // nil: no new window. A raw pointer because `define_class!` cannot
            // yet return a `Retained` from a protocol method.
            std::ptr::null_mut()
        }
    }
);

/// A `WKWebView` living over the reader pane.
pub struct WebView {
    view: Retained<WKWebView>,
    /// Kept alive for as long as the view is: the delegate is held weakly on
    /// the other side, and a dropped one means an unguarded web view.
    _delegate: Retained<NavigationGuard>,
    /// What is currently loaded. Reloading the same document would throw away
    /// the reader's scroll position on every frame.
    showing: Option<u64>,
    /// Where it was last put, so a frame that did not move it does not call
    /// into AppKit sixty times a second to say so.
    placed: Option<Bounds<Pixels>>,
    /// When the current document was handed over, while it is still loading.
    ///
    /// `loadHTMLString` is asynchronous: the view keeps painting the document
    /// it already has until WebKit commits the new one. Left on screen, that
    /// is the last message's body under this message's header — which is
    /// exactly what it looks like, and worse than showing nothing.
    pending_since: Option<std::time::Instant>,
    attached: bool,
}

/// Shared with the element, which is rebuilt every frame.
pub type SharedWebView = Rc<RefCell<WebView>>;

impl WebView {
    /// Builds a web view and adds it to the window, hidden.
    ///
    /// Returns `None` where there is no window to add it to, which is every
    /// platform that is not macOS and any window gpui has not finished
    /// creating.
    pub fn new(window: &Window) -> Option<SharedWebView> {
        let mtm = MainThreadMarker::new()?;
        let host = host_view(window)?;

        let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
        unsafe {
            // The sanitizer takes scripts out of the document; this makes sure
            // there is nothing to run even if one is missed.
            configuration.defaultWebpagePreferences().setAllowsContentJavaScript(false);
            configuration.preferences().setJavaScriptCanOpenWindowsAutomatically(false);
        }

        let frame = NSRect::new(
            objc2_foundation::NSPoint::new(0., 0.),
            objc2_foundation::NSSize::new(10., 10.),
        );
        let view = unsafe {
            WKWebView::initWithFrame_configuration(
                WKWebView::alloc(mtm),
                frame,
                &configuration,
            )
        };

        let delegate = NavigationGuard::alloc(mtm);
        let delegate: Retained<NavigationGuard> = unsafe { msg_send![delegate, init] };
        unsafe {
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            view.setUIDelegate(Some(ProtocolObject::from_ref(&*delegate)));
            view.setHidden(true);
        }

        host.addSubview(&view);

        Some(Rc::new(RefCell::new(WebView {
            view,
            _delegate: delegate,
            showing: None,
            placed: None,
            pending_since: None,
            attached: true,
        })))
    }

    /// Loads a message, unless it is already the one on screen.
    ///
    /// The document is built by the closure and only when the key changes: the
    /// reader re-renders every frame, and both sanitizing a 40 KB newsletter
    /// and reloading it — which would throw the scroll position back to the
    /// top — are things to do once per message, not sixty times a second.
    /// Returns whether this call started a load, which is the caller's cue to
    /// keep asking for frames until it settles.
    pub fn show(&mut self, key: u64, document: impl FnOnce() -> String) -> bool {
        if self.showing == Some(key) {
            return false;
        }
        self.showing = Some(key);
        self.pending_since = Some(std::time::Instant::now());
        self.view.setHidden(true);

        unsafe {
            // No base URL: a relative path in the document has nothing to
            // resolve against, which is the point. Everything that may load
            // was already made absolute or removed.
            self.view.loadHTMLString_baseURL(&NSString::from_str(&document()), None);
        }
        true
    }

    /// Whether what the view is painting is the document it was last given.
    ///
    /// `isLoading` is the answer, with a floor under it: it is read on the
    /// frame after the hand-over, because immediately after `loadHTMLString`
    /// returns WebKit has not necessarily started, and "not loading" then
    /// means the *old* document is still up.
    pub fn settled(&mut self) -> bool {
        let Some(since) = self.pending_since else { return true };
        if since.elapsed() >= SETTLE_FLOOR && !unsafe { self.view.isLoading() } {
            self.pending_since = None;
            return true;
        }
        // A load that never finishes must not hide the reader for good.
        if since.elapsed() >= SETTLE_LIMIT {
            self.pending_since = None;
            return true;
        }
        false
    }

    /// Moves the view to where the layout put it, and shows it.
    ///
    /// gpui's coordinates run from the top left; AppKit's run from the bottom
    /// left, so the y has to be flipped against the host view's height.
    pub fn place(&mut self, bounds: Bounds<Pixels>) {
        if !self.attached {
            return;
        }

        // Still loading: keep it off screen rather than let it paint the
        // message before this one under this one's header.
        if !self.settled() {
            self.view.setHidden(true);
            self.placed = None;
            return;
        }

        if self.placed == Some(bounds) && !self.view.isHidden() {
            return;
        }
        self.placed = Some(bounds);

        let Some(host) = (unsafe { self.view.superview() }) else { return };
        let host_height = host.frame().size.height;

        let frame = NSRect::new(
            objc2_foundation::NSPoint::new(
                f64::from(f32::from(bounds.origin.x)),
                f64::from(host_height as f32 - f32::from(bounds.origin.y + bounds.size.height)),
            ),
            objc2_foundation::NSSize::new(
                f64::from(f32::from(bounds.size.width)),
                f64::from(f32::from(bounds.size.height)),
            ),
        );

        self.view.setFrame(frame);
        self.view.setHidden(false);
    }

    /// Takes the view off screen.
    ///
    /// Called on every frame that does not draw it, because a native view is
    /// not part of the scene: nothing else will cover it, and a modal opening
    /// over the reader would otherwise open *under* the message.
    pub fn hide(&mut self) {
        if !self.view.isHidden() {
            self.view.setHidden(true);
        }
    }
}

impl Drop for WebView {
    fn drop(&mut self) {
        unsafe {
            self.view.setNavigationDelegate(None);
            self.view.setUIDelegate(None);
            self.view.removeFromSuperview();
        }
        self.attached = false;
    }
}

/// The window's content view, which is what a native subview is added to.
pub(crate) fn host_view(window: &Window) -> Option<Retained<NSView>> {
    // Disambiguated: gpui's `Window` has an inherent `window_handle` of its
    // own that answers a different question.
    let handle = HasWindowHandle::window_handle(window).ok()?;
    let RawWindowHandle::AppKit(handle) = handle.as_raw() else { return None };

    // SAFETY: gpui hands out a pointer to the window's own NSView, which lives
    // as long as the window does.
    let view: &NSView = unsafe { &*(handle.ns_view.as_ptr() as *const NSView) };
    Some(view.retain())
}

/// The element that holds the reader's space and keeps the native view in it.
///
/// It draws nothing. Its whole job is to be laid out like any other element so
/// that the web view ends up exactly where the message would have been.
pub struct WebViewElement {
    web_view: SharedWebView,
}

impl WebViewElement {
    pub fn new(web_view: SharedWebView) -> Self {
        Self { web_view }
    }
}

impl IntoElement for WebViewElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for WebViewElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let style = Style {
            flex_grow: 1.,
            size: gpui::Size {
                width: gpui::relative(1.).into(),
                height: gpui::relative(1.).into(),
            },
            ..Default::default()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // Layout is finished, so this is the first moment the rectangle the
        // message occupies is actually known.
        self.web_view.borrow_mut().place(bounds);
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        // Nothing to paint: the pixels in this rectangle belong to AppKit.
    }
}

