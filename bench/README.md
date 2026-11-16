# Comparative benchmark

A harness for measuring Spool against other Mac mail clients on **one mailbox,
one machine, one instrument**.

> **This is not the regression harness.** For "did this change make it slower?",
> see [crates/bench](../crates/bench/README.md) — headless, microseconds, run in
> a minute, history committed in `bench/history/`. This directory is for "is it
> faster than what people use now?": external, milliseconds, screen capture,
> and an afternoon of setup. The two sets of numbers are both honest and are not
> interchangeable.

## Quickstart

```
bench/run.sh doctor                          # what this machine has and lacks
python3 bench/generate_corpus.py --count 50000 --out ~/spool-bench/maildir
bench/server.sh start                        # Dovecot on 127.0.0.1:10143, warmed
bench/run.sh calibrate                       # the instrument's overhead, here
bench/run.sh spool-install                   # the released Spool.dmg, into /Applications
bench/run.sh spool-up                        # Spool on the benchmark profile
# ...add the account to Apple Mail / MailMate by hand, once, and let them sync
bench/run.sh snapshot applemail              # check the coordinates by eye
bench/run.sh spool applemail mailmate        # measure, then print the table
bench/run.sh report --markdown               # the table again, for pasting
```

Each client pass measures, in order: open a message (click), next message
(↓), scroll, search, then memory, idle CPU and store size, and last launch and
quit (it quits and relaunches the client). `ACTIONS="open next"` runs a subset;
the names are `open next scroll search static launch`.

For a Spool change, before it ships:

```
bench/run.sh spool-build base                # the working tree, as Spool-base.app
# ...make the change...
bench/run.sh spool-build fix
TAG=base SPOOL_APP=~/spool-bench/apps/Spool-base.app bench/run.sh spool
TAG=fix  SPOOL_APP=~/spool-bench/apps/Spool-fix.app  bench/run.sh spool
```

`TAG` keeps each build's results apart (`spool@fix.jsonl`), and the report
puts them side by side. `spool-build` needs no Xcode: it builds with gpui's
`runtime_shaders`, so the Metal shaders compile when the app starts rather than
at build time. That costs a little at launch, so compare launch times between
two `spool-build` builds, not between one and the release.

A second, larger mailbox runs beside the first without touching it:

```
python3 bench/generate_corpus.py --count 250000 --out ~/spool-bench/maildir-250k
MAILDIR=~/spool-bench/maildir-250k PORT=10144 bench/server.sh start
CORPUS=250k bench/run.sh sync-time           # Spool's first sync, from an empty profile
CORPUS=250k bench/run.sh spool               # results in results/<machine>-250k/
```

`CORPUS=250k` gives Spool its own profile (`profile-250k`) on port 10144 and
writes to its own results directory. Another client needs an account of its
own pointed at port 10144.

Over SSH, everything that touches the screen is run in Terminal on the Mac's
own display, and the display is held awake for the run. A locked screen
captures black and takes no input, so `measure` refuses to run on one rather
than recording timeouts: unlock the Mac first.

Once per machine: grant **Screen Recording** and **Accessibility** to the
terminal that runs this (System Settings ▸ Privacy & Security). The first
`osascript` resize also asks for Automation permission over System Events.

Results go to `~/spool-bench/results/<machine>/`, one directory per CPU slug
(`apple-m1-ultra`, `apple-m1-max`) — the same slug `cargo perf` uses — so
numbers from two machines never end up in one table.

## The trap, first

The numbers Spool already publishes cannot go in a comparison table. They are
not the kind of number you can get out of another client, and putting them
beside a number that *is* would be dishonest in Spool's favour.

| Published number | What it actually measures | Obtainable from Apple Mail? |
|---|---|---|
| Frame build time 85µs | `perf.rs` instrumentation *inside* gpui's render pass | **No.** There is no such hook in a closed app. |
| Full-text search 15.2ms | `spool --bench`: a headless SQLite FTS5 query, no UI | **No.** Apple Mail searches via Spotlight, and you can only observe the whole click-to-pixels path. |
| Ingest 29,000 msg/s | `demo::populate` writing straight into the store, no IMAP | **No.** Every other client's ingest *is* an IMAP sync. |
| On disk 274 MB | Spool's store, which keeps parsed fields and an index | **Misleading.** Apple Mail and Thunderbird keep the original RFC822 as well. Spool's figure is smaller partly because it stores less. |

So a comparison has to **re-measure Spool from the outside**, the same way as
everything else: synthetic click, screen capture, count frames until the pixels
change. Those numbers will be in milliseconds, not microseconds, because they
include the window server, the display pipeline and the input stack — the parts
a person actually waits for.

Keep the two sets apart on any page that shows them:

* **Internal** (µs) — what Spool costs itself. Its own section, clearly the
  app measuring itself.
* **External** (ms) — what a person waits for, measured identically for every
  client including Spool. The only set that belongs in a comparison table.

## What can be compared fairly

Everything below is observable from outside any client, with no cooperation
from the app:

| Measurement | Instrument |
|---|---|
| Cold launch → list painted | synthetic launch, capture until pixels settle |
| Click a message → body painted | synthetic click at *t*, capture until pixels change |
| Type a query → first results painted | synthetic keystrokes, capture until the list changes |
| Scroll 5,000 rows → frames dropped | capture during a fixed scroll, count repeated frames |
| First full sync of the corpus | wall clock, plus the server's own connection log |
| Resident memory, idle after sync | `ps -o rss` |
| Disk footprint after sync | `du -s` on the client's store |

Two caveats that belong next to the results rather than in a footnote:

* **Search is not engine-vs-engine.** Spool queries FTS5 in-process; Apple Mail
  asks Spotlight; Thunderbird has its own Gloda index. Measuring "how long
  until I see results" is a fair *user* comparison and an unfair *engine* one.
  Say which you are claiming.
* **Disk footprint is not like-for-like** unless you note what each client
  stores. See the table above.

## The corpus

`generate_corpus.py` writes a deterministic Maildir from the same word lists,
thread rate, flag distribution and HTML shapes as the app's own
`spool --demo` corpus (`crates/store/src/demo.rs`), drawn from the same
xorshift64* generator with the same seed. Regenerated as real RFC822, because
the store keeps parsed fields and every other client wants the original
message.

```
python3 bench/generate_corpus.py --count 50000 --out ~/spool-bench/maildir
```

Roughly 1 KB per message on disk and about 6,000 messages/second to generate.
A third of the corpus is `multipart/related` with an HTML alternative and an
inline PNG; 12% claim an attachment in a header but carry no payload — real
payloads at this count would be tens of gigabytes and would turn the benchmark
into a disk throughput test.

One account, six folders, weighted the way the demo corpus weights the work
account: INBOX 60, Sent 15, Archive 15, Lists/rust 5, Junk 3, Trash 2.

## The server

```
brew install dovecot
bench/server.sh start        # also: stop, status, log, reset
```

Dovecot 2.4, run natively as you — no Docker, no root — on `127.0.0.1:10143`,
plain IMAP, user `bench` password `bench`. `server.sh` renders
`dovecot/native.conf` into `~/spool-bench/server/` and keeps the server's
indexes there too, so the Maildir stays exactly what the generator wrote.

Native rather than a container: Docker on a Mac is a Linux VM, and serving
50,000 small files across the VM's file sharing puts a hypervisor between every
client and every message.

**The index is warmed on start.** Dovecot builds its own index on first access,
and whichever client connects first would otherwise pay for it and report it as
its own slowness. `start` reads every header once (`doveadm fetch … hdr`),
which builds the indexes and pulls the corpus into the page cache. Note that
`doveadm index`, which this README used to recommend, does nothing without an
FTS plugin.

The 2.3 container in `dovecot/` (Dockerfile, compose.yml, dovecot.conf) still
works where Docker is installed: `MAILDIR=~/spool-bench/maildir docker compose
-f bench/dovecot/compose.yml up -d --build`. It is built from Alpine because
the upstream images are amd64 only and crash under Rosetta.

## Running Spool against it, safely

Spool is measured the way the other clients are: the released build,
downloaded from the same URL a person uses and installed into /Applications.
`bench/run.sh spool-install` does that (and again to upgrade). Without it,
`spool-up` falls back to building this checkout, for measuring a change before
it ships — and the results record which one was measured.

`bench/run.sh spool-up` does everything below: writes the account if it is
not there, and launches the binary inside
the bundle (so the process has its bundle identifier) with the benchmark
profile's environment. `SPOOL_APP=/path/to/Spool.app` measures a released
build instead of this checkout, which is what a published comparison should
use. Building needs full Xcode: gpui compiles Metal shaders at build time and
the Command Line Tools have no Metal compiler — or `bench/run.sh spool-build`,
which needs neither. By hand:

Spool honours `XDG_CONFIG_HOME` and `XDG_DATA_HOME`, so the benchmark profile
is completely separate from your real one — different `accounts.toml`,
different settings, different store. Nothing here can touch real mail.

```
export SPOOL_BENCH=~/spool-bench/profile
mkdir -p "$SPOOL_BENCH/config/spool"
cat > "$SPOOL_BENCH/config/spool/accounts.toml" <<'TOML'
[[accounts]]
name = "Bench"
email = "bench@example.com"
auth = "password"
imap_host = "127.0.0.1"
imap_port = 10143
imap_security = "plaintext"
smtp_host = "127.0.0.1"
smtp_port = 10025
smtp_security = "plaintext"
password = "bench"
initial_limit = 1000000
TOML

XDG_CONFIG_HOME="$SPOOL_BENCH/config" \
XDG_DATA_HOME="$SPOOL_BENCH/data" \
  target/Spool.app/Contents/MacOS/spool
```

`initial_limit` is deliberately larger than the corpus: the default of 2,000
would have Spool sync a fraction of what the other clients sync, which is the
easiest way to accidentally win a benchmark.

## Running the others

| Client | Local IMAP? | Notes |
|---|---|---|
| Apple Mail | yes | Ships with macOS. Add the account by hand; it will not accept plaintext without a nudge in the advanced pane. |
| Thunderbird | yes | `brew install --cask thunderbird`. Set `mail.server.default.max_cached_connections` to match the others. |
| MailMate | yes | `brew install --cask mailmate`. Trial is fully functional. |
| Mimestream | **no** | Gmail API only. Cannot be pointed at an IMAP server, so it cannot be in a corpus-parity comparison at all. |
| Superhuman | **no** | Gmail/Outlook only, and syncs through its own backend. Same exclusion. |

Excluding a client for this reason is worth stating plainly in any published
result — "we did not test Superhuman" reads as an omission; "Superhuman cannot
be pointed at a test server, so no like-for-like measurement exists" is the
actual finding.

## The instrument

`measure.swift` posts a synthetic event with `CGEventPost`, captures the target
window with ScreenCaptureKit at 120fps, and reports the presentation timestamp
of the first frame whose watched region differs from the baseline. It knows
nothing about the app under test.

```
swiftc -O bench/measure.swift  -o bench/measure     # run.sh does this for you
swiftc -O bench/calibrate.swift -o bench/calibrate

./bench/measure --list                 # every capturable window
./bench/measure --app com.apple.mail --window-title Inbox \
                --region 700,120,900,700 --click 400,300 --runs 20
```

Grant Screen Recording and Accessibility once, by hand, to whatever runs it.
The binaries are built on the machine that runs them and are not committed;
`run.sh` rebuilds them whenever the Swift source is newer.

Options that exist because the obvious version measured nothing:

* `--click-alternate DY` alternates the click between two rows. Clicking the
  same row twice repaints nothing the second time, and the run burns its whole
  timeout.
* `--prepare cmd+a,backspace` posts keys before each run's baseline. Without
  it, twenty search runs type the query twenty times into one field.
* A `--scroll` moves the cursor to the centre of `--region` before the
  baseline, because a scroll goes to whatever is under the cursor, and after a
  click in the search field that is the search field.
* `--snapshot out.png` saves one frame with the region outlined in red and the
  click points in blue, and posts nothing. `run.sh snapshot <client>` does it
  for every rectangle that client uses.

Two numbers per run. **first** is the first frame that differs from the
baseline — something happened. **settle** is the first frame after which the
region stays identical for `--settle-frames` — it finished happening. Quoting
only `first` would let a client that paints a spinner in 16ms and the message
in 900ms look fast, which is a way of lying with a true number.

`--region` matters more than anything else here. To measure *opening a
message*, watch the reader pane; watching the whole window catches the list
row highlighting instead, which every client does immediately and which is not
the thing anyone is waiting for. State the rectangle alongside the result.

It refuses to guess which window it is looking at: more than one match is an
error listing the candidates, because silently picking the largest is how a run
ends up measuring a different window and reporting plausible nonsense. That is
not hypothetical — it is what the first version of this file did.

### Calibration

`calibrate.swift` is a window that flips colour a known number of milliseconds
after it is clicked, and reports on stderr what it *actually* did rather than
what it was asked to do — macOS coalesces timers, and a target that fires late
otherwise shows up as an instrument that reads slow.

Measured on this machine (M-series, 5120x2160 at 1x, 120fps capture, 10 runs
each, medians):

| Asked | Target actually fired | Instrument reported | Overhead |
|---|---|---|---|
| 50 ms | 50.08 ms | 69.27 ms | **+19.2 ms** |
| 200 ms | 200.07 ms | 226.17 ms | **+26.1 ms** |
| 800 ms | 823.78 ms | 845.25 ms | **+21.5 ms** |

So the instrument adds a **constant ~21 ms (±4)**, flat across two orders of
magnitude rather than proportional. Capture delivery, compositor latency and
event posting, none of which any app under test can be blamed for.

What that means for the results, and what has to be said next to them:

* **Differences between clients are real.** The offset is identical for every
  client, so it cancels in a comparison.
* **Absolute floors are not.** Anything measuring under about 30 ms is at the
  instrument's floor. The honest phrasing is "within a frame or two of the
  click", not a number with a decimal point in it.
* **Report raw.** Publish what was measured and state the offset, rather than
  subtracting 21 ms and publishing a number no one can reproduce.

Re-run the calibration on whatever machine produces the published numbers, and
publish the table above alongside them: `bench/run.sh calibrate` runs all three
delays and prints the table, and `report` appends it under every comparison.

**The display's refresh rate is the instrument's resolution.** The table above
is from a 120Hz laptop panel. On a 60Hz display (the Mac Studio's 1080p
monitor, for one) nothing can be seen more often than every 16.7ms whatever
`--fps` asks for, so expect a larger offset and coarser steps. Every `window`
line in the results records `display_hz`. Compare clients on one display, and
never put numbers from a 60Hz and a 120Hz run in the same table.

## Still to do

Account setup in Apple Mail, Thunderbird and MailMate is by hand, once each —
there is no scripted path that is worth trusting more than doing it. After
that, `bench/run.sh` takes the measurements.

**Launch and quit** are scripted (`bench/run.sh launch <client>`, and the
last step of every pass): Launch Services opens the app, `window` is when its
window is on screen, `ready` is when the list stops changing, and quit is from
the Quit event to the process being gone. Warm, not cold — run 1 reads from
disk and is left out of the medians; a truly cold launch needs a reboot or
`sudo purge`.

**First full sync** is scripted for Spool only (`bench/run.sh sync-time`,
which counts rows in its store). The other clients need a per-client signal
that the sync finished; the server log is the most neutral one. By hand for now.

Adding a client is one `case` in `client_spec` in `run.sh`: its process name,
bundle id, a row, the reader and list rectangles, the search field and where
it keeps its store. Then `run.sh snapshot <client>` until the picture is right.

## Baseline: Spool, 50,000 messages

The first client through the harness, on the corpus described above. Machine:
M-series MacBook, macOS 26.6, 5120x2160 display at 1x. Server and client both
on this machine, so the network is loopback and no one is being penalised for
their internet.

| | |
|---|---|
| Full IMAP sync, empty store → 50,000 messages | **9.4 s** (~5,300 msg/s) |
| Store on disk afterwards | 89.4 MB |
| Resident memory, idle after sync | 265 MB |
| Corpus as served (apparent size) | 68.2 MB |

Two things worth noticing before this gets quoted.

**Sync is six times slower than ingest.** The page says 29,000 messages/second;
that is `demo::populate` writing straight into the store. Over IMAP the same
store fills at about 5,300/s, because the protocol — `FETCH`, parse, per-message
round trips — is the cost, not SQLite. Both numbers are true and they measure
different things, which is the whole argument of the section above.

**The store is bigger than the corpus.** 89.4 MB of store for 68.2 MB of mail,
because the FTS5 index and the parsed envelopes are additional to the bodies.
Spool's advertised 274 MB at 250,000 comes from a corpus with no raw RFC822 in
it at all; against real messages the ratio is different, and a comparison
against clients that keep the original source has to say so.

### Setting up the remaining clients

Each takes one pass by hand. Server: `127.0.0.1`, port `10143`, **no
encryption**, user `bench@example.com` (or just `bench`), password `bench`.

* **Apple Mail** — Mail ▸ Add Account ▸ Other Mail Account. It will fail to
  guess the settings; when it offers the manual sheet, set IMAP host and port
  and turn TLS off. Then Mailbox ▸ Rebuild is *not* wanted — let it sync on its
  own and time it.
* **Thunderbird** — Account Settings ▸ Add Mail Account ▸ Configure manually.
  Set Connection security to None and Authentication to Normal password. Under
  Synchronisation & Storage, turn on "Keep messages in all folders for this
  account on this computer", or it will sync headers only and win the benchmark
  by not doing the work.
* **MailMate** — Preferences ▸ Accounts ▸ +. Same host and port; set Security
  to None.
* **neomutt** — `neomutt -F bench/neomuttrc`. No setup.

The Thunderbird note matters more than it looks. Three of these clients can be
configured to fetch headers and leave bodies on the server, which makes them
look fast at everything except the measurement that matters. Every client has
to be set to keep full messages locally, and the result should say so.

Then: `bench/run.sh snapshot <client>` and look at the picture — the
coordinates in `run.sh` were set on a different machine and are unverified
for some clients — then `bench/run.sh <client>`.
