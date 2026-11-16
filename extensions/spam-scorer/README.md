# Spam Scorer

Scores and classifies mail with [Jev](https://docs.typesafe.ai), TypeSafe's
System One model. Jev answers typed questions — a level on a scale, an option
from a set, a probability — rather than returning text, which is why the whole
extension is about three hundred lines and none of them parse prose.

Three questions go out per message, in one request, evaluated in parallel:

| Question | Type | What comes back |
|---|---|---|
| `spam` | Score, 5 levels | `0.0`–`4.0`, probability-weighted, so a genuinely ambiguous message reads as `2.4` rather than being rounded into a bucket |
| `category` | Choice, 6 options | `personal`, `transactional`, `newsletter`, `promotional`, `outreach`, `fraud`, with a confidence |
| `impersonation` | Noul | The probability the sender is pretending to be someone else |

They are three questions rather than one because a level that measures
unwantedness *and* category *and* forgery measures none of them well. A targeted
phishing message is not bulk — a single number scores it low and a separate
question catches it.

## What it does with the answer

Badges a row and banners the open message, and each badge is a filter above
the list: **Clean**, **Spam**, **Forged**, and one per label that shows, with a
count on each. It does not move your mail. `modify-mail` is in the manifest for exactly one command —
**File the selection as spam** — which you run by name, from the palette or from
the button on the banner.

That is a deliberate limit rather than an unfinished one. A classifier that
files on a threshold it picked is one you discover by missing something, and
this host has no undo stack yet to discover it with.

| Reading | What you see |
|---|---|
| Impersonation ≥ 0.7 | `Forged` badge, and a banner saying how sure it is |
| Score ≥ threshold | `Spam 3.4` badge, and a banner with the score |
| A label that shows, category confidence ≥ 0.5 | The label's name as a badge — `Promotion`, `Newsletter` |
| Anything else | `✓` — scored, and passed |

Every scored message also gets a line at the top of the reader with what Jev
said: what it thinks the message is, as badges — every category it gives 40%
or more, so a message it reads as half newsletter, half promotion shows both —
then `Jev · unwanted 2.1 of 4 · impersonation 3%`.

A message with no mark at all has not been scored yet, which is a different
thing from one that was scored and came back fine. That distinction is most of
what makes a quiet classifier legible: without it, an extension doing its job
perfectly looks like an extension that is not running.

## Labels

The categories Jev sorts mail into are yours to set, in Settings → Extensions →
Spam Scorer → **Labels**. Each has a name, a **Show** switch, and a definition.
The name and definition are what Jev is asked — they go out as the options of
the category question, and the name is what comes back — so a label can be
anything you can describe: `Invoice`, `School`, `Recruiter`. `Other` is always
offered too, for mail that is none of them.

| Default | Shown | |
|---|---|---|
| Personal | no | Someone writing to you about something specific to you |
| Receipt | no | Receipts, password resets, delivery notices, alerts |
| Newsletter | yes | Digests, product updates, mailing list posts |
| Promotion | yes | Offers, sales, launches, webinar invitations |
| Cold outreach | yes | Sales pitches, recruiters, partnership requests |
| Fraud | yes | Phishing, impersonation, advance-fee fraud, malware |

A label that does not show is still asked about — a receipt is marked `✓`
rather than being forced into Promotion — and it is named in the reader line.
Every label that shows is also a filter above the list.

Changing a label's name or definition changes the question, so the mail in
switched-on mailboxes is scored again, back as far as **How far back** says.
Switching **Show** changes only what is drawn, and costs nothing.

The `Spam` and `Forged` rows are gated on confidence ≥ 0.25. A label is gated on
the **category** confidence instead, which is the one that is actually high on
bulk mail: a sale email comes back as `category Promotion, confidence 0.99`
alongside `score 2.06, confidence 0.21`, because the model is certain what it
is and genuinely split on whether that is a level 2 or a level 3.

## Install

```
scripts/build-extension.sh spam-scorer
cp -r target/extensions/spam-scorer ~/.local/share/spool/extensions/
cp extensions/spam-scorer/config.example.toml \
   ~/.local/share/spool/extensions/spam-scorer/config.toml
$EDITOR ~/.local/share/spool/extensions/spam-scorer/config.toml
```

The key goes in `config.toml`, beside the manifest, because there is nowhere
else it could go: WASI is linked empty, so the sandbox has no environment and no
filesystem, and the UI vocabulary an extension describes itself in has a button
and a line of text and nothing that accepts typing. The host reads the file,
checks it against the `[[config]]` table in `spool.toml`, and seeds the values
into the extension's own key/value store before `init` runs — under keys it may
read and may not write.

Without `api-key` the extension does not start, and says so naming the key and
the file. That is better than starting and failing on the first request.

## What leaves the machine

One HTTPS request per scored message, to `api.typesafe.ai` — the only host the
manifest grants, and the URL is resolved before the request goes out rather than
matched as text. It carries the sender, the reply-to when it disagrees with the
sender, the subject, a recipient count, whether the message is a reply, the
preview line, and the first 2,000 characters of the plain-text body. Set
`body = "none"` to withhold that last one.

Nothing is sent twice. A verdict is written to the extension's store under the
message id and read back from there, which is also what makes the badge
affordable: `decorate` has a fifty-millisecond deadline and must be a pure
function of its argument, so it can look a verdict up and cannot go and get one.

## Switching it on

Scoring is off in every mailbox until you switch it on, from the **Spam
scoring** switch in the bar above that mailbox's list. Switched on, the mail
from as far back as **How far back** says is scored straight away — 24 hours,
7 days (the default), 30 days or 365 days, chosen in Settings → Extensions —
and new mail is scored as it arrives. Mail in a mailbox that is not switched on
is never sent to Jev. Switching off stops any scoring still under way and keeps
the verdicts already made.

Mail already in Sent, Drafts, Junk or Trash is never sent at all.

## Cost and rate

Jev bills input tokens only — there is no generated text to bill for. A message
with a 2,000-character excerpt is roughly 600–800 input tokens, so a
quarter-million-message mailbox swept end to end is on the order of 150M tokens.
Check the current rate on TypeSafe's pricing page before pointing this at
everything, and think about what 365 days of a busy mailbox comes to.

Each call scores at most 24 messages. A batch and a command each get thirty
seconds and every message is a round trip, so switching on hands the backlog
over 24 at a time, with progress shown beside the switch. There is no retry:
nothing in the sandbox can sleep, so a backoff would be a spin against the
deadline. Three failures in a row — a bad key, a rate limit — end the call
rather than spending it being told the same thing; what was missed is picked up
the next time the app starts, or by the sweep command.

## Commands

| Command | What it does |
|---|---|
| Score the selection | Scores the selected messages, skipping any already scored |
| Score unscored mail here | Sweeps the open mailbox — or the current search, if the box has something in it |
| File the selection as spam | Moves the selection to the account's junk mailbox |

## Tests

`crates/ext/tests/spam_scorer.rs` drives the real component against a `Services`
implementation that answers `fetch` and records what was asked — so the
questions, the request shape, which messages are sent, the caching, the badges
and the refusal to start without a key are all checked without a key and without
a network.

```
cargo test -p spool-ext --test spam_scorer
```
