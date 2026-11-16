# The regression harness

Measures Spool against itself, over time. One question: **did this change make
it slower?**

```
cargo perf
```

`cargo perf` is an alias defined in `.cargo/config.toml`. It works from anywhere
in the repository and builds release, which is the only build worth measuring —
a debug build measures a different program. If you would rather have the bare
command, `cargo install --path crates/bench` puts `spool-bench` on your PATH;
the binary is otherwise just `target/release/spool-bench`.

That builds a deterministic 100,000-message corpus, measures about thirty
things, compares them with the last run on this machine, and appends a record
to `bench/history/<machine>.jsonl`. Takes about a minute.

## Not the other benchmark

`bench/` is a **comparative** harness: Spool against Apple Mail, Thunderbird and
MailMate, measured from the outside with synthetic clicks and screen capture.
Its numbers are milliseconds and include the window server, the compositor and
a ~21ms instrument overhead. It exists to answer "is this faster than what
people use now?"

This one is **internal**. Headless, no UI, no window server, microseconds. It
exists to answer "is this faster than it was yesterday?"

Both are honest, and their numbers are not interchangeable. Do not put them in
the same table.

## Reading the output

```
                               median         tail   vs base
store.list.all                97.36ms      99.31ms      +0.3%
store.list.flagged            16.18ms      17.30ms      -7.6% (noise)
html.blocks                   15.40µs      15.70µs      -2.5%
```

**median**, not mean. A benchmark on a laptop has a long right tail — the
scheduler, a background process, a thermal event — and a mean is dragged by
exactly the samples that measured something other than the code.

**tail** is the pessimistic end: the 95th percentile of a duration, the *5th* of
a throughput. For anything on a frame path it is the number that corresponds to
a visible stutter, which a median hides.

**vs base** is only shown when it means something. A row inside 5% prints
nothing. A row outside 5% but inside its own noise is marked `(noise)` rather
than reported — a benchmark that swings 8% on its own has not detected an 8%
change, and a tool that says otherwise is one nobody reads by the second week.

## What it measures

| Suite | Covers |
|---|---|
| `store` | Ingest throughput, listing at every scope, conversation grouping, four search shapes, body and thread reads, flag writes, disk size |
| `parse` | HTML→blocks, snippet extraction, threading's subject normalization, and the date/size formatting a row does every frame |

`--list` prints every benchmark id. Ids are stable: renaming one starts its
history over.

Two of these are on paths that scale in ways the rest do not. `html.snippet`
runs once per message at ingest, so a microsecond added there is 3% of ingest
throughput. `core.format_date_parts` runs once per visible row per frame — 2,400
calls a second at 120Hz, inside a frame budget of 8.3ms.

## Seeing the trend

The table above answers "did *this* change make it slower". After a week of
changes the question is different — where has it been going? — and a column of
percentages cannot show that.

```
cargo perf --report --open
```

Writes `target/bench-report.html`: one self-contained file, no server and
nothing to install, built from the history committed in the repository. It
works on a machine that has just cloned the repo and never run a benchmark —
it will simply have nothing to draw.

A grid of stat tiles, one per benchmark: the id, the current value, the change
since last time, and a sparkline of every run before it. Hovering a point names
the commit and the time it was taken, which is how you get from "this got
slower on Tuesday" to the commit that did it.

Each tile has its own scale. A benchmark measured in nanoseconds and one
measured in messages per second share no axis, and putting them on one would
make twenty-nine of the thirty a flat line along the bottom.

Direction is a **word** — "faster" or "slower" — not just a colour. Partly
because red and green are 4.1 ΔE apart under deuteranopia and would say nothing
to a reader who cannot separate them, and partly because "up" is not "better":
`store.ingest` improves by rising and `store.list.all` by falling, so an arrow
would mean opposite things on adjacent tiles.

**Only what moved** hides everything inside 5% of its last reading, which on a
thirty-benchmark page is usually everything. **Table** is the same data as
numbers, which is what to read when you want the exact figures or want to copy
them somewhere.

Runs at a different corpus size are left out and the page says so, for the same
reason the terminal report never compares across sizes. Runs taken on a busy or
throttled machine, and runs taken against an uncommitted tree, are called out at
the top — a soft number in the middle of a trend looks exactly like a regression
that recovered.

## Everyday use

```
cargo perf                     measure, compare, record
cargo perf --quick             rougher and faster, for iterating
cargo perf --suite parse       just the pure-computation suite
cargo perf --no-save           measure without writing history
cargo perf --baseline clean    compare against the last committed run
cargo perf --show              recent history, measuring nothing
cargo perf --report --open     write the history out as charts and open it
```

While iterating on one thing, `cargo perf --suite parse --quick` takes a few
seconds.
`--baseline clean` is what to use after an afternoon of uncommitted attempts,
when "the previous run" has stopped being a useful reference point.

## The history

One JSON object per run, one file per machine, committed to the repository.

Appending is the only write that ever happens, so a run that dies halfway
cannot damage what is already there — and a line that will not parse costs that
line rather than the file.

**Runs are only ever compared with runs from the same machine at the same corpus
size.** Numbers from two machines are not comparable by a scaling factor or with
a caveat; keeping them in separate files makes the mistake awkward to make.

Each record carries the machine's state at the time — on battery, thermally
limited, load average — so a run that disagrees with its neighbours can be
explained rather than puzzled over six weeks later. The tool warns about all
three before it measures anything, and warns rather than refuses: a benchmark
that will not run on battery is a benchmark that does not get run.

## Getting a number worth trusting

Plugged in, nothing else running, and check the warnings. A run made while a
compiler is going will be 20% slow across the board and will sit in the history
looking like a regression.

Build release. A debug build measures a different program, and one debug run in
the history poisons every comparison after it — the tool refuses unless you pass
`--allow-debug`. `cargo perf` bakes `--release` in for exactly this reason.

## The tools beside it

```
cargo run --release -p spool-bench --example terms   # which search terms to use
cargo run --release -p spool-bench --example plan    # what SQLite does with a query
```

`terms` exists because the first version of the search suite got its own labels
wrong: it called `postmortem` rare and `deploy` common, and both are
subject-line words drawn from the same uniform list, so both matched about 4%
and the two benchmarks measured the same thing under different names. Re-run it
after changing `spool_store::demo`.

`plan` is for when a benchmark says a query is slow and you need to know
whether it is doing real work or scanning a table it should be seeking into.
