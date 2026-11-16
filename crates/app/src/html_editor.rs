//! Writing a message as HTML, in a web view.
//!
//! The composer's own editor is plain text and stays that way; this is what it
//! becomes when a message is sent as HTML. A `WKWebView` over the body, holding
//! an editable page: the writing, the signature, and the message being
//! answered exactly as it was formatted — which is the reason this exists.
//!
//! It differs from the reader's web view (see `webview.rs`) in one way that
//! matters: scripting is on. The page needs a little of its own to be an
//! editor — to report what changed, to run a formatting command, to catch ⌘B —
//! and that script is ours, written below. What else is in the page is either
//! typed by the person writing or the original, which went through the same
//! sanitizer as the reader (see `spool_core::sanitize`) and so carries no
//! script or handler of its own. Remote images in it are parked rather than
//! loaded (see `spool_core::markup::hold_remote`). And the view still never
//! navigates: the navigation guard is the reader's.
//!
//! The page talks back by posting messages, which land in an inbox the
//! composer empties on a timer. Rust talks to the page by calling the small
//! `spool` object the script defines.
//!
//! While the page has the keyboard, gpui's own view is kept out of the way of
//! its keys; see [`guard_keys`].

#![cfg(target_os = "macos")]

use std::{cell::RefCell, collections::VecDeque, rc::Rc, sync::OnceLock};

use gpui::{
    App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement, LayoutId,
    Pixels, Style, Window,
};
use objc2::{
    DefinedClass, MainThreadOnly, define_class, msg_send,
    rc::{Retained, Weak},
    runtime::{Bool, Imp, ProtocolObject, Sel},
    sel,
};
use objc2_app_kit::{NSEvent, NSView};
use objc2_foundation::{MainThreadMarker, NSObject, NSObjectProtocol, NSRect, NSString};
use objc2_web_kit::{
    WKScriptMessage, WKScriptMessageHandler, WKUserContentController, WKWebView,
    WKWebViewConfiguration,
};

use crate::webview::NavigationGuard;

/// The name the page posts under: `window.webkit.messageHandlers.spool`.
const CHANNEL: &str = "spool";

/// What the page has said since it was last asked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EditorEvent {
    /// The content changed: all of it, and the text of the writing part.
    Changed { html: String, writing: String },
    /// ⌘Return.
    Send,
    /// ⌘⇧⌫, which the composer binds to discarding the draft.
    Discard,
    /// Escape.
    Escape,
    /// ⇧Tab, out of the body and back to the fields above it.
    Back,
    /// The page took the keyboard, by a click or otherwise. gpui does not see
    /// that happen, and would otherwise go on offering keys to the field it
    /// last thought was focused.
    Focused,
    /// A file dropped or pasted on the page that is not a picture to show in
    /// the message — those the page puts in itself — and so is attached.
    Attach { name: String, bytes: Vec<u8> },
}

type Inbox = Rc<RefCell<VecDeque<EditorEvent>>>;

define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpoolEditorBridge"]
    #[ivars = Inbox]
    struct Bridge;

    unsafe impl NSObjectProtocol for Bridge {}

    unsafe impl WKScriptMessageHandler for Bridge {
        #[unsafe(method(userContentController:didReceiveScriptMessage:))]
        unsafe fn did_receive(&self, _controller: &WKUserContentController, message: &WKScriptMessage) {
            let body = unsafe { message.body() };
            let Ok(text) = body.downcast::<NSString>() else { return };
            if let Some(event) = parse_event(&text.to_string()) {
                self.ivars().borrow_mut().push_back(event);
            }
        }
    }
);

/// One message from the page, which it sends as JSON.
fn parse_event(json: &str) -> Option<EditorEvent> {
    let value: serde_json::Value = serde_json::from_str(json).ok()?;
    let text = |key: &str| value[key].as_str().unwrap_or_default().to_string();
    match value["kind"].as_str()? {
        "changed" => Some(EditorEvent::Changed { html: text("html"), writing: text("writing") }),
        "send" => Some(EditorEvent::Send),
        "discard" => Some(EditorEvent::Discard),
        "escape" => Some(EditorEvent::Escape),
        "back" => Some(EditorEvent::Back),
        "focus" => Some(EditorEvent::Focused),
        "attach" => Some(EditorEvent::Attach {
            name: text("name"),
            bytes: spool_net::encoding::base64_decode(value["data"].as_str()?)?,
        }),
        _ => None,
    }
}

/// A command run on the page.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Takes back what the writing tools put in.
    Undo,
}

impl Command {
    fn script(self) -> &'static str {
        match self {
            Command::Undo => "spool.exec('undo')",
        }
    }
}

pub struct HtmlEditor {
    view: Retained<WKWebView>,
    controller: Retained<WKUserContentController>,
    _guard: Retained<NavigationGuard>,
    _bridge: Retained<Bridge>,
    inbox: Inbox,
    placed: Option<Bounds<Pixels>>,
}

pub type SharedEditor = Rc<RefCell<HtmlEditor>>;

impl HtmlEditor {
    /// Builds the editor holding `content`, and adds it to the window hidden.
    /// `focus` puts the caret at the start of the writing once it has loaded.
    pub fn new(window: &Window, content: &str, focus: bool) -> Option<SharedEditor> {
        let mtm = MainThreadMarker::new()?;
        let host = crate::webview::host_view(window)?;
        let inbox: Inbox = Rc::default();

        let configuration = unsafe { WKWebViewConfiguration::new(mtm) };
        let bridge = Bridge::alloc(mtm).set_ivars(inbox.clone());
        let bridge: Retained<Bridge> = unsafe { msg_send![super(bridge), init] };
        let controller = unsafe { configuration.userContentController() };
        unsafe {
            configuration.preferences().setJavaScriptCanOpenWindowsAutomatically(false);
            controller.addScriptMessageHandler_name(
                ProtocolObject::from_ref(&*bridge),
                &NSString::from_str(CHANNEL),
            );
        }

        let frame = NSRect::new(
            objc2_foundation::NSPoint::new(0., 0.),
            objc2_foundation::NSSize::new(10., 10.),
        );
        let view = unsafe {
            WKWebView::initWithFrame_configuration(WKWebView::alloc(mtm), frame, &configuration)
        };
        let guard = NavigationGuard::alloc(mtm);
        let guard: Retained<NavigationGuard> = unsafe { msg_send![guard, init] };
        unsafe {
            view.setNavigationDelegate(Some(ProtocolObject::from_ref(&*guard)));
            view.setHidden(true);
            view.loadHTMLString_baseURL(&NSString::from_str(&document(content, focus)), None);
        }
        host.addSubview(&view);
        guard_keys(&host);
        EDITORS.with_borrow_mut(|editors| editors.push(Weak::from_retained(&view)));

        Some(Rc::new(RefCell::new(HtmlEditor {
            view,
            controller,
            _guard: guard,
            _bridge: bridge,
            inbox,
            placed: None,
        })))
    }

    /// Everything the page has said since the last call, oldest first.
    pub fn take_events(&mut self) -> Vec<EditorEvent> {
        self.inbox.borrow_mut().drain(..).collect()
    }

    pub fn command(&self, command: Command) {
        self.run(command.script());
        self.focus();
    }

    /// Replaces the writing part — what the writing tools produced — as one
    /// step the page's own ⌘Z can take back.
    pub fn set_writing(&self, html: &str) {
        let html = serde_json::to_string(html).unwrap_or_default();
        self.run(&format!("spool.setWriting({html})"));
    }

    /// Types `text` where the caret was, as one step ⌘Z takes back — for a
    /// button outside the page, which has taken the focus from it by then.
    pub fn insert_text(&self, text: &str) {
        let text = serde_json::to_string(text).unwrap_or_default();
        self.run(&format!("spool.exec('insertText', {text})"));
        self.focus();
    }

    /// Hands the keyboard to the page.
    pub fn focus(&self) {
        if let Some(window) = self.view.window() {
            window.makeFirstResponder(Some(&self.view));
        }
    }

    fn run(&self, script: &str) {
        unsafe { self.view.evaluateJavaScript_completionHandler(&NSString::from_str(script), None) };
    }

    /// Moves the view to where the layout put it, and shows it. See
    /// `WebView::place` for the flip.
    pub fn place(&mut self, bounds: Bounds<Pixels>) {
        if self.placed == Some(bounds) && !self.view.isHidden() {
            return;
        }
        self.placed = Some(bounds);
        let Some(host) = (unsafe { self.view.superview() }) else { return };
        let height = host.frame().size.height;
        let frame = NSRect::new(
            objc2_foundation::NSPoint::new(
                f64::from(f32::from(bounds.origin.x)),
                f64::from(height as f32 - f32::from(bounds.origin.y + bounds.size.height)),
            ),
            objc2_foundation::NSSize::new(
                f64::from(f32::from(bounds.size.width)),
                f64::from(f32::from(bounds.size.height)),
            ),
        );
        self.view.setFrame(frame);
        self.view.setHidden(false);
    }

    /// Takes it off screen, for a frame where something is drawn over the
    /// body: a native view is not part of the scene and would cover it.
    pub fn hide(&mut self) {
        if !self.view.isHidden() {
            self.view.setHidden(true);
        }
    }
}

impl Drop for HtmlEditor {
    fn drop(&mut self) {
        // Taken out of the window with the keyboard, the page would leave it
        // with the window itself, which never passes a key on to gpui: the
        // composer would stop hearing anything that was typed.
        if let Some(window) = self.view.window()
            && let Some(host) = unsafe { self.view.superview() }
            && has_keyboard(&window, &self.view)
        {
            window.makeFirstResponder(Some(&host));
        }
        EDITORS.with_borrow_mut(|editors| {
            editors.retain(|editor| editor.load().is_some_and(|editor| !std::ptr::eq(&*editor, &*self.view)));
        });
        unsafe {
            // The controller holds the bridge; letting go of it here is what
            // lets both be freed with the view.
            self.controller.removeScriptMessageHandlerForName(&NSString::from_str(CHANNEL));
            self.view.setNavigationDelegate(None);
            self.view.removeFromSuperview();
        }
    }
}

/// Gives the keyboard back to the window's own view, from the editor.
///
/// gpui's focus lives inside that view, so moving focus to a field above the
/// body means making the view AppKit's first responder again first.
pub fn release_keyboard(window: &Window) {
    if let Some(host) = crate::webview::host_view(window)
        && let Some(ns_window) = host.window()
    {
        ns_window.makeFirstResponder(Some(&host));
    }
}

thread_local! {
    /// Every editor's web view, so a key can be told to be the page's.
    static EDITORS: RefCell<Vec<Weak<WKWebView>>> = const { RefCell::new(Vec::new()) };
}

/// Whether the keyboard is in `view`: the view itself, or anything WebKit has
/// put inside it.
fn has_keyboard(window: &objc2_app_kit::NSWindow, view: &NSView) -> bool {
    window
        .firstResponder()
        .as_deref()
        .and_then(|responder| responder.downcast_ref::<NSView>())
        .is_some_and(|responder| responder.isDescendantOf(view))
}

/// Whether one of the editors has the keyboard of `view`'s window.
fn editor_has_keyboard(view: &NSView) -> bool {
    let Some(window) = view.window() else { return false };
    EDITORS.with_borrow(|editors| {
        editors
            .iter()
            .filter_map(Weak::load)
            .any(|editor| has_keyboard(&window, &editor))
    })
}

type KeyEquivalentFn = unsafe extern "C-unwind" fn(&NSView, Sel, &NSEvent) -> Bool;
type KeyDownFn = unsafe extern "C-unwind" fn(&NSView, Sel, &NSEvent);

/// gpui's own handlers, from before [`guard_keys`] stood in front of them.
struct GpuiKeys {
    key_equivalent: KeyEquivalentFn,
    key_down: KeyDownFn,
}

static GPUI_KEYS: OnceLock<Option<GpuiKeys>> = OnceLock::new();

/// Keeps gpui's view from taking keys that are the page's.
///
/// AppKit offers a key to every view in the window as a possible shortcut
/// before it gives it to the one with the keyboard — and not only ⌘-keys:
/// the arrows, Home and End, Page Up and Down and forward delete are offered
/// too. gpui's view answers that offer itself and never passes it to the
/// views inside it, so with the page holding the keyboard gpui still saw each
/// of those keys first, ran it through its keymap, and handed it to its own
/// text input context — a second one, beside WebKit's, for the same key.
/// Two input contexts working the same keystroke is what the page showed: a
/// box typed where the caret was, then marked text the next keys typed over
/// or deleted, until the editor stopped taking keys properly at all.
///
/// So while an editor has the keyboard gpui's view declines the offer, and
/// the key goes where AppKit sends it next: the menu bar, then the page. The
/// composer's keys that are not on a menu the page's script reports instead.
/// The same goes for a key the page lets go of, which WebKit passes up to
/// gpui's view: it is not gpui's either.
///
/// gpui has no hook for this, so its view's two handlers are replaced with
/// ones that check first and otherwise call the originals. Only gpui's own
/// methods are touched, never `NSView`'s.
fn guard_keys(host: &NSView) {
    GPUI_KEYS.get_or_init(|| {
        let class = host.class();
        // Declared on gpui's class itself — `instance_method` would also
        // find `NSView`'s, and replacing that would reach every view there is.
        let own = |name: Sel| class.instance_methods().iter().copied().find(|method| method.name() == name);
        let (Some(key_equivalent), Some(key_down)) = (own(sel!(performKeyEquivalent:)), own(sel!(keyDown:))) else {
            log::warn!("gpui's view has no key handlers of its own; the HTML editor's keys are unguarded");
            return None;
        };
        // SAFETY: each replacement has the signature of the method it
        // replaces, and the originals are called back with the same.
        unsafe {
            let guarded: Imp = std::mem::transmute(guarded_key_equivalent as KeyEquivalentFn);
            let key_equivalent: KeyEquivalentFn = std::mem::transmute(key_equivalent.set_implementation(guarded));
            let guarded: Imp = std::mem::transmute(guarded_key_down as KeyDownFn);
            let key_down: KeyDownFn = std::mem::transmute(key_down.set_implementation(guarded));
            Some(GpuiKeys { key_equivalent, key_down })
        }
    });
}

unsafe extern "C-unwind" fn guarded_key_equivalent(view: &NSView, cmd: Sel, event: &NSEvent) -> Bool {
    match GPUI_KEYS.get() {
        Some(Some(keys)) if !editor_has_keyboard(view) => unsafe { (keys.key_equivalent)(view, cmd, event) },
        _ => Bool::NO,
    }
}

unsafe extern "C-unwind" fn guarded_key_down(view: &NSView, cmd: Sel, event: &NSEvent) {
    if let Some(Some(keys)) = GPUI_KEYS.get()
        && !editor_has_keyboard(view)
    {
        unsafe { (keys.key_down)(view, cmd, event) }
    }
}

/// The page: the content, editable, and the script that makes it an editor.
fn document(content: &str, focus: bool) -> String {
    format!(
        r#"<!DOCTYPE html>
<html><head><meta charset="utf-8"><style>
html, body {{ margin: 0; background: #ffffff; }}
body {{ padding: 12px 16px; color: #1f2328;
       font: 14px/1.45 -apple-system, Helvetica, Arial, sans-serif; }}
#spool-body {{ outline: none; min-height: calc(100vh - 24px); }}
blockquote {{ margin: 0 0 0 .8ex; border-left: 1px solid #ccc; padding-left: 1ex; }}
.spool-signature {{ color: #57606a; }}
img[data-spool-src] {{ background: #eef0f2; min-width: 24px; min-height: 16px; }}
#spool-drop {{ position: fixed; width: 2px; background: #0969da; pointer-events: none; display: none; }}
</style></head>
<body><div id="spool-body" contenteditable="true">{content}</div>
<div id="spool-drop"></div>
<script>{SCRIPT}{focus}</script></body></html>"#,
        focus = if focus { "spool.focusStart();" } else { "" },
    )
}

/// The editor's own script. Everything in it is about editing: reporting
/// changes, running commands on the selection, and the shortcuts a Mac editor
/// is expected to have.
const SCRIPT: &str = r#"
(function () {
  const root = document.getElementById('spool-body');
  const post = (kind, extra) => {
    try {
      window.webkit.messageHandlers.spool.postMessage(JSON.stringify(Object.assign({ kind }, extra || {})));
    } catch (e) {}
  };
  const writing = () => {
    const part = document.getElementById('spool-writing');
    return (part || root).innerText;
  };

  // Where the selection was, so a command run from outside the page — which
  // takes focus away from it — applies to it.
  let saved = null;
  document.addEventListener('selectionchange', () => {
    const selection = window.getSelection();
    if (selection.rangeCount && root.contains(selection.anchorNode)) {
      saved = selection.getRangeAt(0).cloneRange();
    }
  });
  const restore = () => {
    root.focus();
    if (saved) {
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(saved);
    }
  };

  // Reported a moment after typing stops, and at once when the page loses
  // the keyboard: clicking Send is exactly that.
  let timer = null;
  const flush = () => {
    clearTimeout(timer);
    timer = null;
    post('changed', { html: root.innerHTML, writing: writing() });
  };
  const changed = () => {
    clearTimeout(timer);
    timer = setTimeout(flush, 200);
  };
  root.addEventListener('input', changed);
  root.addEventListener('blur', () => { if (timer) flush(); });
  root.addEventListener('focus', () => post('focus'));

  // What an arrow, Home, End or forward delete key is to AppKit: a character
  // in the private use area, U+F700 to U+F8FF. When WebKit is handed one of
  // those as text instead of as the movement it stands for, it types it, and
  // the arrows fill the message with boxes. Whatever route the key came by,
  // the page does here what the key means, and never types it.
  const FUNCTION_KEY = /[\uF700-\uF8FF]/;
  let modifiers = { shiftKey: false, altKey: false, metaKey: false };
  const navigate = (code) => {
    const selection = window.getSelection();
    const alter = modifiers.shiftKey ? 'extend' : 'move';
    const across = (direction) => {
      // A selection with nowhere to go collapses toward the arrow, as it
      // does in every Mac text field.
      if (alter === 'move' && !selection.isCollapsed && !modifiers.altKey && !modifiers.metaKey) {
        direction === 'backward' ? selection.collapseToStart() : selection.collapseToEnd();
        return;
      }
      selection.modify(alter, direction,
        modifiers.metaKey ? 'lineboundary' : modifiers.altKey ? 'word' : 'character');
    };
    const along = (direction) => selection.modify(alter, direction,
      modifiers.metaKey ? 'documentboundary' : modifiers.altKey ? 'paragraphboundary' : 'line');
    switch (code) {
      case 0xF700: along('backward'); break;
      case 0xF701: along('forward'); break;
      case 0xF702: across('backward'); break;
      case 0xF703: across('forward'); break;
      case 0xF729: selection.modify(alter, 'backward', 'documentboundary'); break;
      case 0xF72B: selection.modify(alter, 'forward', 'documentboundary'); break;
      case 0xF72C: window.scrollBy(0, -window.innerHeight * 0.9); break;
      case 0xF72D: window.scrollBy(0, window.innerHeight * 0.9); break;
      case 0xF728: document.execCommand('forwardDelete'); changed(); break;
    }
  };
  document.addEventListener('keypress', (e) => {
    const code = e.charCode || (e.key.length === 1 ? e.key.charCodeAt(0) : 0);
    if (code < 0xF700 || code > 0xF8FF) return;
    e.preventDefault();
    navigate(code);
  }, true);
  root.addEventListener('beforeinput', (e) => {
    if (!e.data || !FUNCTION_KEY.test(e.data)) return;
    e.preventDefault();
    if (e.data.length === 1) navigate(e.data.charCodeAt(0));
  });

  document.addEventListener('keydown', (e) => {
    modifiers = { shiftKey: e.shiftKey, altKey: e.altKey, metaKey: e.metaKey };
    const key = e.key.toLowerCase();
    if (e.metaKey && key === 'enter') { e.preventDefault(); flush(); post('send'); return; }
    if (e.metaKey && e.shiftKey && key === 'backspace') { e.preventDefault(); post('discard'); return; }
    // Undo is the page's own, which remembers formatting and what the
    // writing tools put in; the app's menu may not route ⌘Z here.
    if (e.metaKey && key === 'z') {
      e.preventDefault();
      document.execCommand(e.shiftKey ? 'redo' : 'undo');
      changed();
      return;
    }
    if (e.metaKey && !e.altKey && !e.ctrlKey && !e.shiftKey) {
      const command = { b: 'bold', i: 'italic', u: 'underline' }[key];
      if (command) { e.preventDefault(); document.execCommand(command); changed(); return; }
    }
    if (key === 'escape') { e.preventDefault(); flush(); post('escape'); return; }
    if (key === 'tab' && e.shiftKey) { e.preventDefault(); flush(); post('back'); }
  });

  // Files dropped or pasted on the page. A picture goes into the message
  // where it was dropped, as a data: URL — sending turns that into a part of
  // the message the HTML points at (see `spool_net::compose`). Anything else
  // is attached, which is what dropping it on the rest of the window does.
  const marker = document.getElementById('spool-drop');
  const hasFiles = (e) => e.dataTransfer && Array.from(e.dataTransfer.types).includes('Files');
  const isPicture = (file) => /^image\/(png|jpeg|gif|webp)$/.test(file.type);
  const escape = (text) => text.replace(/[&<>"]/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' })[c]);
  const read = (file) => new Promise((resolve, reject) => {
    const reader = new FileReader();
    reader.onload = () => resolve(reader.result);
    reader.onerror = () => reject(reader.error);
    reader.readAsDataURL(file);
  });
  // Where in the writing a point is, or nowhere if it is not over it.
  const rangeAt = (x, y) => {
    const range = document.caretRangeFromPoint(x, y);
    return range && root.contains(range.startContainer) ? range : null;
  };
  // Where the caret would be drawn for a collapsed range, which has no box of
  // its own when it sits between elements rather than in text.
  const caretBox = (range) => {
    const box = range.getClientRects()[0];
    if (box && box.height) return box;
    const container = range.startContainer;
    const node = container.nodeType === Node.ELEMENT_NODE
      ? (container.childNodes[range.startOffset] || container)
      : container.parentNode;
    return node && node.getBoundingClientRect ? node.getBoundingClientRect() : null;
  };
  const hideMarker = () => { marker.style.display = 'none'; };
  const insertFiles = async (files, range) => {
    for (const file of files) {
      let url;
      try { url = await read(file); } catch (e) { continue; }
      if (isPicture(file)) {
        root.focus();
        const selection = window.getSelection();
        selection.removeAllRanges();
        if (range) {
          selection.addRange(range);
        } else if (saved) {
          selection.addRange(saved);
        }
        document.execCommand('insertHTML', false,
          `<img src="${url}" alt="${escape(file.name || 'image')}" style="max-width: 100%;">`);
        range = null;
        flush();
      } else {
        post('attach', { name: file.name || 'attachment', data: url.slice(url.indexOf(',') + 1) });
      }
    }
  };
  document.addEventListener('dragover', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = 'copy';
    const range = rangeAt(e.clientX, e.clientY);
    const box = range && caretBox(range);
    if (!box) { hideMarker(); return; }
    marker.style.left = `${box.left}px`;
    marker.style.top = `${box.top}px`;
    marker.style.height = `${Math.max(box.height, 14)}px`;
    marker.style.display = 'block';
  });
  document.addEventListener('dragleave', (e) => { if (!e.relatedTarget) hideMarker(); });
  document.addEventListener('drop', (e) => {
    if (!hasFiles(e)) return;
    e.preventDefault();
    hideMarker();
    insertFiles(Array.from(e.dataTransfer.files), rangeAt(e.clientX, e.clientY));
  });
  root.addEventListener('paste', (e) => {
    const files = Array.from((e.clipboardData && e.clipboardData.files) || []);
    if (!files.length) return;
    e.preventDefault();
    const selection = window.getSelection();
    insertFiles(files, selection.rangeCount ? selection.getRangeAt(0) : null);
  });

  window.spool = {
    exec(command, argument) {
      restore();
      document.execCommand(command, false, argument === undefined ? null : argument);
      changed();
    },
    setWriting(html) {
      const part = document.getElementById('spool-writing') || root;
      root.focus();
      const range = document.createRange();
      range.selectNodeContents(part);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
      document.execCommand('insertHTML', false, html);
      flush();
    },
    focusStart() {
      const part = document.getElementById('spool-writing') || root;
      root.focus();
      const range = document.createRange();
      range.selectNodeContents(part);
      range.collapse(true);
      const selection = window.getSelection();
      selection.removeAllRanges();
      selection.addRange(range);
    },
  };
})();
"#;

/// Holds the body's space in the layout and keeps the editor in it, as
/// `WebViewElement` does for the reader.
pub struct EditorElement {
    editor: SharedEditor,
}

impl EditorElement {
    pub fn new(editor: SharedEditor) -> Self {
        Self { editor }
    }
}

impl IntoElement for EditorElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for EditorElement {
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
            size: gpui::Size { width: gpui::relative(1.).into(), height: gpui::relative(1.).into() },
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
        self.editor.borrow_mut().place(bounds);
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
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Registering the class is where the runtime checks the handler's
    /// signature against WebKit's protocol — a mismatch there is a crash the
    /// first time a reply is opened as HTML, so it is found here instead.
    #[test]
    fn the_bridge_registers_with_the_runtime() {
        use objc2::ClassType;
        let class = Bridge::class();
        assert_eq!(class.name().to_str().unwrap(), "SpoolEditorBridge");
        // What WebKit calls when the page posts: present, under the
        // selector the protocol names.
        assert!(class.instance_method(objc2::sel!(userContentController:didReceiveScriptMessage:)).is_some());
    }

    #[test]
    fn what_the_page_posts_is_read_back() {
        assert_eq!(
            parse_event(r#"{"kind":"changed","html":"<p>Hi</p>","writing":"Hi"}"#),
            Some(EditorEvent::Changed { html: "<p>Hi</p>".into(), writing: "Hi".into() })
        );
        assert_eq!(parse_event(r#"{"kind":"send"}"#), Some(EditorEvent::Send));
        assert_eq!(
            parse_event(r#"{"kind":"attach","name":"notes.txt","data":"aGk="}"#),
            Some(EditorEvent::Attach { name: "notes.txt".into(), bytes: b"hi".to_vec() })
        );
        assert_eq!(parse_event(r#"{"kind":"attach","name":"x","data":"not base64!"}"#), None);
        assert_eq!(parse_event(r#"{"kind":"nonsense"}"#), None);
        assert_eq!(parse_event("not json"), None);
    }

    #[test]
    fn the_page_holds_the_content_and_focuses_only_when_asked() {
        let page = document("<p>Hello</p>", true);
        assert!(page.contains(r#"<div id="spool-body" contenteditable="true"><p>Hello</p></div>"#));
        assert!(page.contains("spool.focusStart();"));
        assert!(!document("<p>Hello</p>", false).contains("spool.focusStart();"));
    }
}
