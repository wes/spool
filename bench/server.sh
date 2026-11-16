#!/usr/bin/env bash
# The benchmark IMAP server, run natively with Homebrew's Dovecot.
#
#   bench/server.sh start      render the config, start, and warm the index
#   bench/server.sh stop
#   bench/server.sh status
#   bench/server.sh log
#   bench/server.sh reset      stop and throw away the server's indexes
#
# 127.0.0.1:10143, plain IMAP (and an SMTP sink on 10025), user `bench` or `bench@example.com`,
# password `bench`. Serves
# $MAILDIR (default ~/spool-bench/maildir), which generate_corpus.py writes.
#
# A second corpus runs beside the first on a port of its own:
#   MAILDIR=~/spool-bench/maildir-250k PORT=10144 bench/server.sh start
#
# The Docker path in bench/dovecot/ still works where Docker is installed; this
# is the one to prefer on a Mac. See native.conf for why.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BENCH="${SPOOL_BENCH_HOME:-$HOME/spool-bench}"
MAILDIR="${MAILDIR:-$BENCH/maildir}"
PORT="${PORT:-10143}"
# One server state per corpus, so a second corpus can be served beside the
# first (on its own PORT) without either rebuilding the other's indexes.
if [[ "$MAILDIR" == "$BENCH/maildir" ]]; then STATE="$BENCH/server"
else STATE="$BENCH/server-$(basename "$MAILDIR")"; fi
CONF="$STATE/dovecot.conf"

DOVECOT="$(command -v dovecot || echo /opt/homebrew/opt/dovecot/sbin/dovecot)"
DOVEADM="$(command -v doveadm || echo /opt/homebrew/opt/dovecot/bin/doveadm)"

[[ -x "$DOVECOT" ]] || { echo "dovecot not found: brew install dovecot" >&2; exit 1; }

render() {
  [[ -d "$MAILDIR/cur" ]] || {
    echo "no corpus at $MAILDIR" >&2
    echo "  python3 $HERE/generate_corpus.py --count 50000 --out $MAILDIR" >&2
    exit 1
  }
  mkdir -p "$STATE/run" "$STATE/state" "$STATE/index"
  # Both spellings of the one account. Spool, Apple Mail and MailMate all log
  # in with the email address by default; neomutt and doveadm use `bench`.
  # Same mailbox either way — the userdb points every login at one Maildir.
  printf 'bench:{PLAIN}bench::::::\nbench@example.com:{PLAIN}bench::::::\n' >"$STATE/users"
  sed -e "s|@STATE@|$STATE|g" \
      -e "s|@MAILDIR@|$MAILDIR|g" \
      -e "s|@USER@|$(id -un)|g" \
      -e "s|@GROUP@|$(id -gn)|g" \
      -e "s|@UID@|$(id -u)|g" \
      -e "s|@GID@|$(id -g)|g" \
      -e "s|@PORT@|$PORT|g" \
      "$HERE/dovecot/native.conf" >"$CONF"
}

running() { "$DOVEADM" -c "$CONF" process status >/dev/null 2>&1; }

# The outgoing side: a sink on 10025 that accepts and discards, for clients
# that will not add an account until SMTP answers. One for every corpus.
smtp_up() {
  lsof -nP -iTCP:10025 -sTCP:LISTEN >/dev/null 2>&1 && return 0
  nohup python3 "$HERE/smtp_sink.py" 10025 >"$BENCH/smtp.log" 2>&1 &
}
smtp_down() { pkill -f "smtp_sink.py 10025" 2>/dev/null || true; }

case "${1:-}" in
  start)
    render
    if running; then
      echo "already running"
    else
      "$DOVECOT" -c "$CONF"
      sleep 1
    fi
    if ! running; then
      echo "dovecot did not come up; the end of its log:" >&2
      tail -5 "$STATE/dovecot.log" >&2
      exit 1
    fi
    # Dovecot builds its index on first access, and whichever client connects
    # first would otherwise pay for it and report it as its own slowness.
    # `doveadm index` is not the tool for that: without an FTS plugin it is a
    # no-op. Reading every header builds the per-mailbox indexes and pulls the
    # whole corpus into the page cache, so every client starts from the same
    # warm server.
    echo "warming the index and the page cache…"
    SECONDS=0
    "$DOVEADM" -c "$CONF" fetch -u bench 'flags size.physical date.received hdr' \
        mailbox '*' all >/dev/null
    echo "  done in ${SECONDS}s"
    smtp_up
    echo "serving $MAILDIR on 127.0.0.1:$PORT (bench / bench); SMTP sink on 10025"
    ;;
  stop)
    [[ -f "$CONF" ]] && "$DOVEADM" -c "$CONF" stop 2>/dev/null || true
    smtp_down
    ;;
  status)
    if [[ -f "$CONF" ]] && running; then
      echo "running"
      "$DOVEADM" -c "$CONF" mailbox status -u bench 'messages' '*'
    else
      echo "stopped"; exit 1
    fi
    ;;
  log)
    tail -f "$STATE/dovecot.log"
    ;;
  reset)
    "$0" stop
    rm -rf "$STATE/index" "$STATE/state"
    echo "indexes removed; the corpus is untouched"
    ;;
  *)
    echo "usage: $0 {start|stop|status|log|reset}" >&2
    exit 2
    ;;
esac
