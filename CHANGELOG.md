# Changelog

A section here for a version, if there is one, is its release notes;
`scripts/release.sh` posts it verbatim as the release's notes. Without one, the
notes are the commit subjects since the last tag. Write for someone who has the
app and not the source: what changed for them, not which files moved.

## 0.1.16

Spool is open source, at https://github.com/wes/spool. Releases come from
there now too, and so do updates.

**If you have 0.1.15 or earlier, install this one by hand.** Spool is now
signed by JOE DESIGNS LLC, and older copies only accept updates signed the old
way, so they will not offer it: download it below and drag it over the copy
in Applications. Your accounts, mail and settings stay where they are. Updates
after this one arrive as usual.

**Try it without an account.** A new install opens on a welcome screen that
offers a demo mailbox: 50,000 made-up messages, kept apart from your own mail.
Nothing in it syncs or sends. Use My Own Mail, at the foot of the sidebar,
takes you back out and deletes it.

**A new account syncs straight away.** Saving one in Settings → Accounts now
starts fetching its mail, rather than waiting for the next launch.

**Fixed:** the corner of the message list showed past the rounded edge of the
pane it sits in ([#1](https://github.com/wes/spool/issues/1)).

**Fixed:** the signature box in Settings → Accounts drew its text in black,
unreadable on the dark theme.

## 0.1.15

Spool can be the app that opens email links. Click an address in a browser,
a PDF or another app and a new message opens in Spool, with the address,
subject and anything else the link carried already filled in.

**Making it the default.** A card at the foot of the sidebar offers to
switch, once: Make Default hands email links to Spool, and No Thanks puts the
card away for good. Settings → Accounts → Email links shows which app opens
them now and has the same button, and so does the command palette ("Make
Spool the default email app"). macOS may ask you to confirm the change.

## 0.1.14

Clients: a client book and sales board built from your mail. It is off unless
you add it, and until then nothing about Spool changes.

**Adding it.** Settings → Extensions → Clients → Add. It opens in a window of
its own from the person button at the end of the toolbar, the Clients row at
the foot of the sidebar, or Window → Clients. ⌘⇧C, or the person mark on any
message, opens it on whoever sent that message, or for mail you sent, whoever
it went to.

**Everyone you write to.** Clients lists the people in your mail across every
account, with the whole history of each, and nothing has to be typed in
first. It starts with the people you have written to, which leaves out
newsletters and receipts. Sign-up and contact-form notices your own site sends
you count as mail from the person who filled in the form.

**Notes, tags and a board.** Give someone a stage, a tag or a note and they
become a client. The board shows your clients in columns by stage: drag a card,
or press [ and ], and ⌘Z puts it back. Edit stages… adds, renames, reorders
and deletes columns. Every account shares one set of stages until you give it
its own. ⌘-click and ⇧-click in the list pick several people to move at once.

**Writing to a group.** Email… writes to everyone the list is showing, in the
usual compose window. Each person gets a message of their own, addressed to
them alone, with {first_name}, {name}, {company} or {email} filled in for
them. You can see each person's copy and leave anyone out, and Send asks to be
pressed twice.

**Bringing people in.** Import a CSV file (Google and Outlook exports
included), a vCard or JSON file into an account's book, or give Clients an
https address that lists your sign-ups and it checks it on a schedule.

Your notes, tags and board are kept in a file of their own beside your mail,
so resyncing or rebuilding your mail never touches them.

## 0.1.13

A fix for anyone with several accounts.

**New mail on every account.** With many accounts, some could stop getting new
mail without saying so. New mail in any one account made Spool check every
account at once, and mail that arrived during that check could be left
waiting. Now only the account with new mail is checked, and anything that
arrives mid-check is fetched as soon as it ends.

**Accounts that can't connect.** When Spool can't reach an account's server or
log in to it, the account's inbox shows a red dot where the syncing dot usually
appears. Point at it for the reason. Before, the error showed only briefly at
the foot of the sidebar, if at all.

**Fewer failed logins.** If a server refuses the password, Spool now waits an
hour before trying again on its own, instead of retrying every few minutes.
Repeated failed logins are what get a mail server to block your address. Get
New Mail still tries straight away.

## 0.1.12

Two fixes.

**Writing in HTML.** After a while of typing or moving with the arrow keys,
the editor could start showing odd characters where the caret was, then stop
taking keys properly — selecting or moving could delete text. The arrows,
Home and End, Page Up and Down and ⌘-shortcuts now go straight to the message
being written. ⌘⇧⌫ discards the draft from the HTML editor too.

**Attachments.** Photos sent from Gmail showed a paperclip in the list but
appeared nowhere when the message was opened. They are listed as files again,
as is any image too large to show in the message itself. They are also
included again in a forward and in Save Attachments.

## 0.1.11

A release about speed, measured against Apple Mail and MailMate on the same
mailbox.

**Opening mail.** Clicking a message opens it in about a third of the time it
used to — the same speed as the arrow keys. Long conversations open as fast as
short ones: only the messages you can see are laid out.

**Search.** Results keep up with typing: each keystroke cancels the search for
the one before it, and searching no longer waits behind mail being saved by a
sync. Nearly twice as fast on a large mailbox.

**Launch.** The window and your inbox appear sooner, and a large inbox shows
its first screen before the rest has loaded.

**First sync.** A new account syncs at the same steady rate from start to
finish. Before, it slowed down as the mailbox grew: 250,000 messages took more
than ten minutes, and now take under a minute.

**Memory.** About 40% less, and on a large mailbox it no longer grows as you
search. Spool also sits quieter when idle.

## 0.1.0

The first build anyone else can run.

**Reading.** Three panes, driven from the keyboard: `j`/`k` through the list,
and the arrows through the sidebar — `→` opens a folder, `←` folds it,
`←` from the list jumps back to the mailboxes. Multiple accounts, with an
optional unified view. Full-text search across a quarter-million messages,
conversation threading, and HTML mail shown as the sender laid it out, with
remote images blocked until you allow them. Flags, archive, delete, move and
undo; attachments you can preview, save and download on demand; one-click
unsubscribe where a list offers it.

**Writing.** Compose, reply, reply all and forward, with address pills,
attachments, signatures and drafts that survive a restart. Send as text or
HTML — a reply to an HTML message goes back as HTML, with the original shown
and kept exactly as it was formatted.

**Writing tools.** Sign in with your ChatGPT account (Settings → Writing) to
rewrite, proofread, shorten or change the tone of a draft, or to have one
written from a line like "a welcome email for Sam" — with suggested directions
when you reply. This uses the sign-in OpenAI's Codex app uses; OpenAI does not
offer one for other apps, so it is unofficial and may stop working.

**Extensions.** Spool runs extensions in a sandbox, installed by copying one
into `~/.local/share/spool/extensions` — none come with the app yet. They can
badge and filter the list, add a switch per mailbox, and have their own
settings in Settings → Extensions.

**Updates.** Spool checks for new releases and installs them when you say so,
refusing anything not signed and notarized by its developer.

Requires macOS 13 or later. Universal — Apple silicon and Intel.
