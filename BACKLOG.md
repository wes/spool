# Spool backlog

Proposed Linear issues for the **spool** project. Grouped by priority; each
entry is scoped to be independently shippable.

---

## P0 — Correctness

### 1. Reader shows the wrong message for the selected row
The detail pane sometimes renders a different message than the one selected.

One cause was found and fixed on 5 Sep: a race in the body loader. Selecting A,
then B, then A again returned early on the third step (because `open` was still
A) while B's load was *still in flight*; B then landed on top of a selection
that had gone back to A. The fix discards a completed load whose message is no
longer selected.

**This issue tracks verifying that fix against real mail and finding any
remaining cause.** Slow-loading messages (60KB+ marketing HTML) widen any
remaining window, so test with those.

### 2. Message actions never reach the server
`Archive`, `Delete`, flag and read/unread changes are written to the **local
SQLite store only**. Nothing issues IMAP `STORE`, `MOVE`, `COPY` or `EXPUNGE`.

Consequence: every change silently reverts on the next sync, and the local
store drifts from the server. This is worse than the feature being absent,
because it looks like it worked.

Needs: IMAP write-back, plus a queue so actions taken offline are replayed
rather than lost.

### 3. Attachments — done, except forwarding
Was "metadata only", and was already out of date when it was written: bytes
are captured during sync into `attachment_blobs` and capped at 12 MB, so the
question of fetch-on-open versus fetch-on-sync had already been answered in
favour of sync.

What was actually missing was the UI, and that has landed: a chip previews in
Quick Look, and a control on it saves to Downloads. Quick Look rather than
`open`, so a crafted attachment does not get to pick which application
processes it.

The size cap is no longer a dead end either. Anything past 12 MB still keeps
its name and loses its bytes at sync time — that is what stops a mailbox of
large attachments becoming a database nobody wants — but the reader now fetches
it from the server when you ask for it, so both halves of the original question
are answered: kept on disk when small, fetched on open when not.

Done as of the attachment work: files go on a new message or a reply from
**Attach…**, a drop onto the composer window, or a pasted image; and a forward
carries the original's, fetching anything past the 12 MB cap from the server
before the composer opens. Size is advisory — the strip under the body warns
past 25 MB encoded and never refuses, because what a server accepts is the
server's business.

---

## P1 — Core mail functions

### 4. Compose a new message
No composer exists. The send engine underneath is built and tested
(`core::draft`, `net::compose`, `net::smtp::send`).

Blocked by #5.

### 5. Multi-line text editor
`TextInput` is single-line only. A message body needs wrapping, vertical cursor
movement, selection across lines, and paste. This is the blocker for the whole
compose feature and is the largest single piece of work in it.

### 6. Reply and Reply All
Addressing rules are implemented and tested in `core::draft` — `Reply-To`
override, excluding yourself, de-duplicating across To/Cc, `References`
threading. What's missing is the UI and the actions to invoke it.

Blocked by #4.

### 7. Forward
`core::draft::forward` exists; needs UI, and is more useful once #3 lands so
attachments come along.

### 8. Save sent mail to the Sent folder
After a successful send, `APPEND` the message to the account's Sent mailbox.
Without this, sent mail exists nowhere — not locally, not on the server.

### 9. Drafts
Save a composer's contents so closing the window doesn't lose the message.
Local first; syncing to the IMAP Drafts folder is a follow-up.

---

## P2 — Platform and interaction

### 10. Standard macOS menu bar
No menus at all. Needs File / Edit / View / Message / Window / Help with the
expected items, wired to the existing actions — every operation is already a
gpui `Action`, so this is mostly mapping rather than new behaviour.

Note that Edit → Cut/Copy/Paste and ⌘W / ⌘M are conspicuous by their absence
right now.

### 11. Resizable panes
Sidebar and message-list widths are fixed constants (`SIDEBAR_WIDTH`,
`LIST_WIDTH`). Add draggable dividers and persist the widths to
`settings.toml` alongside the window geometry and theme.

Caveat: the message list's ellipsis relies on a **fixed** `ROW_TEXT_WIDTH`,
because gpui only truncates text whose width is already definite when it is
measured. Making the list resizable means solving that properly.

### 12. Mailbox management — create, rename, delete
No way to create or delete an IMAP mailbox. Needs `CREATE`, `RENAME`, `DELETE`
plus subscribe/unsubscribe, and a confirmation step, since deleting a mailbox
destroys server-side mail.

### 13. Drag and drop messages into folders
Drag from the list onto a sidebar folder to move. Depends on #2 — without
server-side moves, a drag would appear to work and then revert.

### 14. Move to folder / Mark as spam / Delete, as first-class actions
Keyboard and menu equivalents for the same operations, including Junk. Also
depends on #2.

---

## P3 — Making it a fuller client

### 15. Conversation view in the message list
The list shows individual messages. Threading already works in the store
(`thread_id`, JWZ-style), and the reader has a conversation mode — the list
itself has not caught up, so a busy thread appears as N separate rows.

### 16. Recipient autocomplete
Composing means typing addresses by hand. Addresses harvested from the mail
already synced are the source (`Store::address_suggestions`), ranked by how
recently and often you have written to them. An address book would be a
second source, and there is no longer one.

### 17. IMAP IDLE for new mail
Sync only happens on launch and on ⌘R. IDLE would make new mail arrive without
being asked.

### 18. Notifications for new mail
Depends on #17 to be worth much.

### 19. Search operators
Search is a single FTS query across subject, sender and body. Add `from:`,
`to:`, `subject:`, `has:attachment`, date ranges.

### 20. Signatures
Per-account, appended on compose and reply.

### 21. Undo
At minimum for archive, delete and move — the operations that are alarming
precisely because they are instant. Pairs with the offline queue in #2.

### 22. Sync more than the newest 2,000 per folder
`initial_limit` caps the first sync. There's no way to backfill older mail
short of editing `accounts.toml` and resyncing.

## P1.5 — Wiring the extension host into the window

The host in `crates/ext` is written and tested: it loads real components, calls
them on all four surfaces, enforces capabilities before an extension starts, and
stops one that will not stop itself. None of it is reachable from the app yet.
These are in the order that makes each one useful on its own.

### 23. Services over the real store
`spool_ext::services::Services` is the trait the host calls back through, and
only a test fake implements it. Wants a real one over `Store` for the mail
reads, a per-extension file for `store_get`/`store_set`, and `spool_net` for
`fetch` — which also needs a timeout of its own, since the extension's deadline
does not run while the host is the one waiting. `header` can only answer for the
columns the store has; see #27.

### 24. Commands in the palette and the keymap
`Registry::commands()` already returns them, qualified and without loading a
single component. The palette needs to merge them in and dispatch through one
dynamic `RunExtensionCommand(String)` action, because `actions!` is a
compile-time macro and an extension cannot add to it. Two snags: `Command`
holds `keystroke: &'static str` and will need a `SharedString`, and a manifest's
`key` must be bound only if nothing already has it — an extension that takes
`cmd-z` away from undo by being installed is an extension that breaks the app by
existing.

### 25. Effects applied against the window
`api::Effect` is a small enum and every variant already has a path in the app:
flags, move, archive and trash go through the same code a keystroke does —
optimistic update, background write, undo entry — rather than a parallel
mutation path. `Status` is the toast, `Notify` is `notify.rs`, `OpenUrl` is
already there for links.

### 26. Decoration cache, then banners, badges and panels
The ordering matters. `decorate` is a background call whose answer is cached per
message, and the cache is what a row reads — a row that called an extension
would break the one rule `message_list.rs` has. Banners sit above the message in
the reader; badges are a column in the list; a panel is a `Pane` and a sidebar
row, rendering `api::Panel` into gpui elements. The panel work is the largest of
the four and the only one that needs new UI rather than a hook into existing UI.

### 27. Rules on new mail
Call `on_messages` after `Store::ingest` commits, in batches, on the background
executor — never inside ingest, which runs at 30,000 messages a second and is
not a thing to put a sandbox in the middle of. Only extensions declaring
`hooks = ["messages"]` are called at all.

### 28. Managing extensions in Settings
A section listing what is installed with its capabilities in the sentence form
`Capability::describe` already produces, an on/off switch per extension
(`Registry::set_enabled`, which persists immediately), and the load failures
`Registry::problems()` collects — an extension that is installed and invisible
is worse than one that is installed and visibly broken.

### 29. Retaining headers
The store keeps parsed mail, so `host::header` answers for nine header names and
returns `none` for everything else — including `List-Unsubscribe`, `List-Id` and
every `X-` header, which is most of what a mail rule wants to match on. Closing
this is a store change: a raw-header blob or a key/value table, written at
ingest. Worth deciding deliberately, since it grows every message on disk.

## Performance

`cargo run --release -p spool-bench` measures these and records them. Numbers
below are from a 100,000-message corpus on an M1 Max, taken while the machine
was busy — re-measure on a quiet one before trusting the absolute values.

### 30. The unified list has no index to sort by
`EXPLAIN QUERY PLAN` (`--example plan`) on `Scope::Everything`:

```
SELECT m.id FROM messages m ORDER BY m.date_ts DESC LIMIT 200
  SCAN m USING COVERING INDEX idx_messages_thread
  USE TEMP B-TREE FOR ORDER BY
```

A full scan and a sort of the whole table, to return two hundred rows — and the
plan is identical whether the limit is 200 or 500,000, which is why
`store.list.page` costs 13.9ms against `store.list.all`'s 97ms rather than a
five-hundredth of it. The scoped queries are fine: mailbox and account both
seek into `idx_messages_mailbox_date` / `idx_messages_account_date` and stop.

The missing piece is an index on `date_ts` alone, which is what the unified view
needs and what nothing else provides. Worth measuring the write cost of a fourth
index on `messages` against the read win — `store.ingest` is the benchmark that
would notice.

### 31. The unread filter scans without a covering index
Same probe, `WHERE (m.flags & 1) = 0`: a bare `SCAN m` plus a temp B-tree. An
expression over `flags` cannot use an index, so this is either a partial index
on the expression or a stored column. `store.list.unread` is 19.9ms at 100k.

### 32. Frame building is not in the regression harness
The headline number — 85µs to build a frame — is measured only by `perf.rs`
while the app is running, so it is the one claim in the README that nothing
checks. Wants a mode that opens a window against a corpus, scripts scrolling and
selection, and writes `perf.rs`'s ring out as JSON for the harness to merge. It
cannot run headless, so it is its own opt-in tier rather than part of the
regular sweep.

### 33. End-to-end sync is not in the regression harness either
`bench/dovecot` already brings up a server and `generate_corpus.py` already
fills it. A first-full-sync timing against it would catch IMAP pipeline
regressions that nothing else can see. Minutes rather than seconds, and needs
Docker, so it is a separate tier that runs deliberately.

---

## Notes

- OAuth works for Gmail. There is no OAuth for other providers yet; everything
  else needs an app password.
- Sending is engine-complete and UI-absent: 50 tests cover draft construction,
  MIME rendering and the SMTP conversation, and none of it is reachable from
  the app.
- Performance work has a harness now: `crates/bench` measures about thirty
  things against a deterministic corpus and keeps every run in
  `bench/history/`. The frame-building and sync tiers (#32, #33) are not built.
- Extensions are in the same state, for the same reason: 64 tests cover the
  manifest, the capability model, the boundary and a real component being
  loaded and driven, and the window contributes none of it yet. See P1.5.
