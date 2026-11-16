# Writing a Spool extension

An extension is a WebAssembly component plus a manifest. Spool loads it in a
sandbox, calls it off the UI thread, and draws what it describes.

`inbox-stats/` is a working example that uses every surface there is. It is
about 150 lines and is the thing to copy.

`spam-scorer/` is a real one: it scores and classifies mail with a hosted model,
so it is the example to read for network access, settings, caching a verdict
across calls, and what to do when the thing you depend on returns a 429.

## Build one

```
cargo new --lib my-extension
```

```toml
# Cargo.toml
[lib]
crate-type = ["cdylib"]

[dependencies]
spool-extension = { path = "../sdk" }
```

```rust
// src/lib.rs
use spool_extension::{CommandContext, Effect, Extension, export_extension, host};

struct Mine;

impl Extension for Mine {
    fn run_command(id: String, _: CommandContext) -> Vec<Effect> {
        host::log(host::Level::Info, &format!("running {id}"));
        vec![Effect::Status("Hello from an extension".into())]
    }
}

export_extension!(Mine);
```

```toml
# spool.toml
[extension]
id = "my-extension"
name = "My Extension"
api = "0.2"
capabilities = ["ui"]

[[commands]]
id = "greet"
title = "Say hello"
```

Then:

```
scripts/build-extension.sh my-extension
cp -r target/extensions/my-extension ~/.local/share/spool/extensions/
```

The directory name has to match the manifest's `id`. Spool checks, and refuses
a directory that disagrees with the manifest inside it — two directories both
calling themselves `my-extension` would share a storage namespace.

## The four surfaces

Implement only what you contribute; the rest default to doing nothing.

| Method | When | Declared by | Where it shows |
|---|---|---|---|
| `run_command` | The user ran one of your commands | `[[commands]]` | The command palette, and the keystroke you asked for if it is free |
| `on_messages` | New mail landed, in batches | `hooks = ["messages"]` | After each sync commits, with the messages it stored |
| `decorate` | A message needs its badges and banners | `hooks = ["decorate"]` | A mark on the list row, a strip above the open message |
| `render_panel` | Your panel is on screen | `[[panels]]` | A section at the foot of the sidebar |

`decorate` **must be a pure function of its argument.** The host calls it in the
background, caches the answer, and draws rows from the cache — an answer that
varies between calls appears as flicker, because it is being asked far less
often than it is being drawn. It also has the shortest deadline of any call, for
the same reason: a list row is waiting on the cache it fills.

An extension that declares no hooks is loaded the first time one of its commands
is run. One that declares a hook has to be live to do its job, so it starts with
the app.

## Asking the host things

`host::query_envelopes`, `host::read_body`, `host::header`, `host::mailboxes`,
`host::accounts`, `host::store_get` / `store_set`, `host::fetch`, `host::log`,
`host::now`.

Each costs a capability. Calling one you did not declare is not an empty
answer — the host refuses to start the extension at all, naming the function and
the capability it needs. That check happens before any of your code runs.

`host::header` answers only for the headers Spool keeps a column for
(`message-id`, `in-reply-to`, `references`, `from`, `to`, `cc`, `reply-to`,
`subject`, `date`). Spool stores parsed mail, not raw source, so anything else
is `None` — and `None` means "never kept", which you cannot tell from "was not
there".

## Settings

An extension has no way to ask you for anything. WASI is linked empty — no
environment, no filesystem — and the UI vocabulary has a button and a line of
text and nothing that accepts typing. So an API key cannot arrive from inside;
the host has to carry it.

Declare what you need:

```toml
# spool.toml
capabilities = ["storage"]      # required: settings arrive through your store

[[config]]
key = "api-key"
title = "API key"
description = "Shown beside the field."
required = true
secret = true

[[config]]
key = "model"
title = "Model"
default = "jev-latest"
```

The values go in `config.toml`, beside `spool.toml` in the installed directory:

```toml
api-key = "sk-..."
```

The host reads it, checks every key against what you declared, and seeds the
values into your own key/value store before `init` runs:

```rust
let key = host::store_get("config.api-key");
```

Three things follow from the `config.` namespace belonging to the host:

* You can read it and you cannot write it. `store_set` on a `config.` key is
  refused, so a setting is always something you were given.
* A key in `config.toml` that you never declared fails the extension at load,
  naming both spellings. The failure this file actually has is `api_key` for
  `api-key`, and ignoring it silently produces an extension that is configured,
  says it is not, and gives no hint which of the two it wanted.
* A **required** setting with no value stops the extension loading, with a
  sentence naming the key and the file. A *missing* value is not a broken
  extension, though — it stays installed and listed, waiting to be filled in.

A number or a boolean in the file is read as the string you will get back, so
`threshold = 3` and `threshold = "3"` are not a difference anybody has to know
about. Arrays and tables are refused.

`secret = true` masks the field and keeps it out of the log. It does not make
the value safer inside the sandbox — an extension that can read its own key can
send it anywhere its manifest lets it reach — and that is the whole promise.
`config.toml` is plain text on disk, unlike an account password, which goes to
the keychain. A `secret` setting should end up there too; it does not yet.

A setting with a short list of values declares them, and the settings pane
offers them as a pick-one instead of a text field. A value in `config.toml` that
is not one of them is refused at load, with the list:

```toml
[[config]]
key = "lookback"
title = "How far back"
default = "7d"
choices = ["24h", "7d", "30d", "365d"]
```

A setting that is a list — labels, rules, anything someone may want several
of — declares its columns as `fields` and its starting rows as `rows`. The pane
draws a card per row, with a switch for a `switch` field and the full width for
a `passage`, and lets rows be added and removed:

```toml
[[config]]
key = "labels"
title = "Labels"
fields = [
    { key = "name", title = "Label" },
    { key = "show", title = "Show", kind = "switch", default = "true" },
    { key = "definition", title = "What it means", kind = "passage" },
]
rows = [{ name = "Promotion", definition = "Marketing for a product or service." }]
```

In `config.toml` it is a `[[labels]]` table per row. You get JSON: an array of
objects, every field present and every value a string —
`[{"definition":"Marketing…","name":"Promotion","show":"true"}]`.

## A switch per mailbox, and filters over the list

An extension that works on mail — scoring it, sorting it — can ask to be off
until someone switches it on for a mailbox:

```toml
hooks = ["messages", "decorate"]   # `scan` needs messages, `filters` decorate

[scan]
title = "Spam scoring"             # the switch's name, shown on hover
glyph = "⚖"                        # its button above the list
since = "lookback"                 # a declared setting holding a span: 24h, 7d, 2w

[[filters]]
id = "spam"
title = "Spam"                     # the chip's label
badge = "Spam"                     # matches `Spam`, and `Spam 3.4`
```

The switch belongs to the host. Off, `on_messages` is never handed mail from
that mailbox — the promise is about what goes into the sandbox, so only the side
doing the sending can keep it. Switched on, the mailbox's mail from as far back
as `since` says is handed to `on_messages` in batches, newest first, and then
new mail as it lands. You skip what you have already seen; the backlog is
handed over again whenever the app starts or your settings change, so doing
nothing for a message you know is what makes that cheap.

`badge-filters = true` in `[extension]` makes every badge a filter as well,
named as the badge is (less a trailing number, so `Spam 3.4` and `Spam 2.9` are
one) — for badges that come from settings and are not known in advance.
`[[filters]]` still names the ones that are.

Once it is on, each `[[filters]]` is a chip above the list with a count of the
rows wearing its badge, and choosing one narrows the list to them. A filter is
matched against your own badges only — never another extension's.

## Changing things

You do not. You return effects, and the host applies them:

```rust
use spool_extension::{flags, Effect, MessageFlags};

vec![
    flags::add(message_id, MessageFlags::FLAGGED),
    Effect::Archive(other_id),
    Effect::Status("filed 2 messages".into()),
]
```

An effect asking for something your manifest did not declare is dropped and
counted, and the count goes in the log. It is not an error you can catch, so the
manifest and the code have to agree.

## Panels

A panel crosses the boundary as an arena — a flat list of nodes naming their
children by index — because WIT has no recursive types. `ui::Builder` does the
bookkeeping:

```rust
use spool_extension::{ui, Emphasis, Tree};

fn render_panel(_: String) -> Tree {
    let mut panel = ui::Builder::new();
    let title = panel.text("Unread", Emphasis::Heading);
    let count = panel.text("14", Emphasis::Accent);
    let action = panel.button("Refresh", "refresh");
    let root = panel.column([title, count, action]);
    panel.finish(root)
}
```

A node may be used as a child exactly once. The host rejects a tree where one is
reachable twice — a cycle would never finish drawing, and a shared subtree
multiplies rather than shares, so twenty layers of it is a million elements from
a tiny arena. Trees are also capped at 2,048 nodes and 24 deep.

Buttons run one of your commands by name. That is the only thing they can do, so
a button can never do something the keyboard cannot.

A badge whose label is a **single character** is drawn bare — no border, no
pill. That is the difference between a mark and a label, and it exists for the
quiet case: an extension that looked at a message and found nothing worth
saying still wants to say it looked, or "checked, fine" and "not checked yet"
are the same blank row. That mark lands on nearly every row, and a box around a
dot is a box around a dot.

You name an `Emphasis`, not a colour. An extension that could name a colour
looks right in the theme it was written against and wrong in the other one,
forever.

## What you cannot do

No filesystem, no environment, no sockets — WASI is linked empty. Network access
is https only, to hosts named in the manifest, and the host is resolved from the
URL rather than matched as a string, so `https://declared.example@evil.example/`
is a request to `evil.example` and is refused as one.

On macOS, a signed Spool needs the
`com.apple.security.cs.allow-unsigned-executable-memory` entitlement for any of
this to run: compiling a component produces native code, and the hardened
runtime kills a process that executes code nobody signed. `scripts/bundle.sh`
signs with it and verifies it took. A build without it opens normally and is
killed the instant the first extension loads.

Every call runs under a deadline. An extension that loops forever is interrupted,
its call fails, and the failure names the extension so the user knows which one
to switch off.

## The contract

`crates/ext/wit/spool.wit` is the API, versioned as `spool:extension@0.2.0`. It
is commented and it is the file to read when this one is not enough.

Before 1.0, a minor bump is a breaking change — the rule cargo applies to a
`0.x` dependency — and a host refuses a component from a version it does not
implement by name, rather than trapping partway through a call.
