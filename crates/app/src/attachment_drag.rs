//! Dragging an attachment out of the reader and into Finder.
//!
//! gpui's drag and drop never leaves the window, so this is AppKit's own: a
//! dragging session started on the window's view, carrying a *file promise*
//! rather than a file. Nothing is written when the drag starts — most drags
//! are abandoned, and an attachment past the size cap is not on disk at all.
//! Finder asks for the file once it has been dropped somewhere, naming the
//! exact path, and only then are the bytes read from the store or fetched
//! from the server.

#![cfg(target_os = "macos")]

use std::{
    cell::RefCell,
    sync::{Arc, atomic::AtomicU64},
};

use gpui::Window;
use objc2::{
    AllocAnyThread, DefinedClass, MainThreadOnly, define_class, msg_send,
    rc::Retained,
    runtime::ProtocolObject,
};
use objc2_app_kit::{
    NSApplication, NSDragOperation, NSDraggingContext, NSDraggingItem, NSDraggingSession,
    NSDraggingSource, NSFilePromiseProvider, NSFilePromiseProviderDelegate, NSWorkspace,
};
use objc2_foundation::{
    MainThreadMarker, NSArray, NSError, NSObject, NSObjectProtocol, NSOperationQueue, NSPoint,
    NSRect, NSSize, NSString, NSURL, ns_string,
};
use spool_core::MessageId;
use spool_store::Store;

use crate::workspace::Workspace;

/// The icon under the pointer, in points.
const ICON: f64 = 32.;

/// Everything needed to produce the file, later and off the main thread.
pub(crate) struct DragFile {
    pub store: Store,
    pub accounts: Vec<spool_net::AccountConfig>,
    pub message: MessageId,
    pub index: usize,
    pub name: String,
}

// The promise's delegate: names the file and, once Finder has chosen where it
// goes, writes it there.
define_class!(
    #[unsafe(super(NSObject))]
    #[name = "SpoolAttachmentPromise"]
    #[ivars = DragFile]
    struct Promise;

    unsafe impl NSObjectProtocol for Promise {}

    unsafe impl NSFilePromiseProviderDelegate for Promise {
        // Raw pointers rather than `Retained`: `define_class!` cannot yet
        // return one from a protocol method, so the result is autoreleased.
        #[unsafe(method(filePromiseProvider:fileNameForType:))]
        fn file_name(&self, _provider: &NSFilePromiseProvider, _type: &NSString) -> *mut NSString {
            let name = crate::workspace::sanitize_filename(&self.ivars().name);
            Retained::autorelease_return(NSString::from_str(&name))
        }

        // Without this the write happens on the main queue, and a fetch from
        // the server would freeze the app for as long as it takes.
        #[unsafe(method(operationQueueForFilePromiseProvider:))]
        fn queue(&self, _provider: &NSFilePromiseProvider) -> *mut NSOperationQueue {
            Retained::autorelease_return(NSOperationQueue::new())
        }

        #[unsafe(method(filePromiseProvider:writePromiseToURL:completionHandler:))]
        fn write(
            &self,
            _provider: &NSFilePromiseProvider,
            url: &NSURL,
            done: &block2::DynBlock<dyn Fn(*mut NSError)>,
        ) {
            match self.write_to(url) {
                Ok(()) => done.call((std::ptr::null_mut(),)),
                Err(e) => {
                    log::warn!("dragging out attachment {}: {e:#}", self.ivars().index);
                    let error = unsafe {
                        NSError::errorWithDomain_code_userInfo(ns_string!("Spool"), 1, None)
                    };
                    done.call((Retained::as_ptr(&error) as *mut NSError,));
                }
            }
        }
    }
);

impl Promise {
    fn new(file: DragFile) -> Retained<Self> {
        let this = Self::alloc().set_ivars(file);
        unsafe { msg_send![super(this), init] }
    }

    fn write_to(&self, url: &NSURL) -> anyhow::Result<()> {
        use anyhow::Context as _;

        let file = self.ivars();
        let path = url.path().context("the drop is not a file path")?.to_string();
        // This queue is AppKit's, not an executor's, so the fetch is driven
        // here; it is on a background thread and blocking it is the point.
        let bytes = smol::block_on(Workspace::attachment_bytes_anywhere(
            file.store.clone(),
            file.accounts.clone(),
            file.message,
            file.index,
            Arc::new(AtomicU64::new(0)),
        ))?;
        std::fs::write(&path, bytes).with_context(|| format!("writing {path}"))
    }
}

// The session's source, which only has to say the drop is a copy: a move
// would read to Finder as permission to take the file away from the message.
define_class!(
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "SpoolAttachmentDragSource"]
    struct Source;

    unsafe impl NSObjectProtocol for Source {}

    unsafe impl NSDraggingSource for Source {
        #[unsafe(method(draggingSession:sourceOperationMaskForDraggingContext:))]
        fn operation_mask(
            &self,
            _session: &NSDraggingSession,
            _context: NSDraggingContext,
        ) -> NSDragOperation {
            NSDragOperation::Copy
        }
    }
);

thread_local! {
    static SOURCE: RefCell<Option<Retained<Source>>> = const { RefCell::new(None) };
    /// A promise's delegate is held weakly, and Finder can ask for the file
    /// well after the drag has ended, so the last few are kept here.
    static PROMISES: RefCell<Vec<Retained<Promise>>> = const { RefCell::new(Vec::new()) };
}

/// How many recent promises are kept alive. Far more than can be in flight.
const KEPT: usize = 8;

/// Starts dragging `file` out of the window, from the mouse event being
/// handled. Returns whether a drag began.
pub(crate) fn begin(window: &Window, file: DragFile) -> bool {
    let Some(mtm) = MainThreadMarker::new() else { return false };
    let Some(view) = crate::webview::host_view(window) else { return false };
    // gpui handles a mouse event inside AppKit's delivery of it, so this is
    // the drag that crossed the threshold — which is what a session must be
    // started from.
    let Some(event) = NSApplication::sharedApplication(mtm).currentEvent() else { return false };

    let extension = std::path::Path::new(&file.name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_string();

    let promise = Promise::new(file);
    let provider = NSFilePromiseProvider::initWithFileType_delegate(
        NSFilePromiseProvider::alloc(),
        ns_string!("public.data"),
        ProtocolObject::from_ref(&*promise),
    );
    let item = NSDraggingItem::initWithPasteboardWriter(
        NSDraggingItem::alloc(),
        ProtocolObject::from_ref(&*provider),
    );

    #[allow(deprecated)]
    let icon = NSWorkspace::sharedWorkspace().iconForFileType(&NSString::from_str(&extension));
    let at = view.convertPoint_fromView(event.locationInWindow(), None);
    unsafe {
        item.setDraggingFrame_contents(
            NSRect::new(NSPoint::new(at.x - ICON / 2., at.y - ICON / 2.), NSSize::new(ICON, ICON)),
            Some(&icon),
        );
    }

    let source = SOURCE.with_borrow_mut(|source| {
        source.get_or_insert_with(|| unsafe { msg_send![Source::alloc(mtm), init] }).clone()
    });
    view.beginDraggingSessionWithItems_event_source(
        &NSArray::from_retained_slice(&[item]),
        &event,
        ProtocolObject::from_ref(&*source),
    );

    PROMISES.with_borrow_mut(|kept| {
        if kept.len() >= KEPT {
            kept.remove(0);
        }
        kept.push(promise);
    });
    true
}
