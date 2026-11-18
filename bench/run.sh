#!/usr/bin/env bash
# The comparative benchmark, end to end.
#
#   bench/run.sh doctor              what this machine has and is missing
#   bench/run.sh permissions         ask for Screen Recording + Accessibility, open Settings
#   bench/run.sh mail-account        add the benchmark account to Apple Mail
#   bench/run.sh calibrate           the instrument's own overhead, on this machine
#   bench/run.sh spool-install       download the released Spool.dmg into /Applications
#   bench/run.sh spool-up            launch Spool against the benchmark profile
#   bench/run.sh spool-build <tag>   build the working tree as ~/spool-bench/apps/Spool-<tag>.app
#   bench/run.sh sync-time           Spool's first sync, from an empty profile (CORPUS=250k for the big one)
#   bench/run.sh snapshot <client>   PNG of the window with every region and click marked
#   bench/run.sh <client>...         measure: spool applemail mailmate thunderbird neomutt
#   bench/run.sh all                 every client that is installed and running
#   bench/run.sh launch <client>...  launch and quit only
#   bench/run.sh report              one table, every client, this machine
#
# TAG=x keeps a build's results apart, CORPUS=250k measures the second mailbox,
# ACTIONS="open next" runs a subset of: open next scroll search static launch.
#
# Results land in ~/spool-bench/results/<machine>/<client>.jsonl: a `session`
# line saying who, what and on which machine, then one JSON object per run and
# a summary per action. Different machines get different directories, because
# their numbers are not comparable and should never meet in one table.
#
# BEFORE MEASURING A CLIENT
#   1. bench/server.sh start — the corpus is being served and the server is warm.
#   2. The client is open, its account fully synced, showing the Inbox sorted
#      newest first, with nothing selected.
#   3. Nothing else is animating on screen. A progress bar inside the watched
#      rectangle is a repaint the instrument cannot tell from the one measured.
#   4. `bench/run.sh snapshot <client>` and look at the picture: the red box
#      must be the pane that changes, the blue crosses must be on message rows.
#      A click two pixels off a row is a measurement of nothing.

set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/.." && pwd)"
BENCH="${SPOOL_BENCH_HOME:-$HOME/spool-bench}"
MEASURE="$HERE/measure"
CALIBRATE="$HERE/calibrate"
RUNS="${RUNS:-20}"
SEARCH_TERM="${SEARCH_TERM:-postmortem}"
SPOOL_DMG_URL="${SPOOL_DMG_URL:-https://github.com/wes/spool/releases/latest/download/Spool.dmg}"

# The released app, installed the way anyone installs it, is what gets
# measured — the same footing as every other client. A build of this checkout
# is the fallback, for measuring a change before it ships.
if [[ -z "${SPOOL_APP:-}" ]]; then
  if [[ -d /Applications/Spool.app ]]; then SPOOL_APP=/Applications/Spool.app
  else SPOOL_APP="$ROOT/target/Spool.app"; fi
fi

# One size and place for everyone. A taller window shows more rows, and rows are
# the only thing that makes a frame cost more — so an unequal window is an
# unequal test. 1400x900 at (100, 60) fits a 1920x1080 display with the menu bar.
WIN_X=100
WIN_Y=60
WIN_W=1400
WIN_H=900

# The same slug `cargo perf` files its history under, so both harnesses agree
# on what "this machine" is.
MACHINE="$(sysctl -n machdep.cpu.brand_string | tr '[:upper:]' '[:lower:]' \
           | sed -E 's/[^a-z0-9]+/-/g; s/^-//; s/-$//')"
# CORPUS picks a second mailbox: `CORPUS=250k` means ~/spool-bench/maildir-250k,
# served on port 10144 by `MAILDIR=… PORT=10144 bench/server.sh start`, with
# its own Spool profile and its own results, so the two sizes never mix.
CORPUS="${CORPUS:-}"
if [[ -n "$CORPUS" ]]; then PORT="${PORT:-10144}"; else PORT="${PORT:-10143}"; fi
PROFILE="$BENCH/profile${CORPUS:+-$CORPUS}"
RESULTS="$BENCH/results/$MACHINE${CORPUS:+-$CORPUS}"
mkdir -p "$RESULTS"

# --------------------------------------------------------------- client table
#
# Coordinates are window-relative points for a WIN_W x WIN_H window. Check them
# with `snapshot` before trusting them: they are per client layout, and a
# client update that moves a pane moves them.
#
#   PROC     process name, for memory and for resizing
#   BUNDLE   bundle id the instrument finds the window by (or APPNAME)
#   TITLE    window title substring, when the client has several windows
#   ROW      a message row in the list, x,y
#   ROW_DY   the distance to another row; clicks alternate between the two
#   READER   the rectangle the message body paints into — not the whole
#            window, which catches the row highlight instead
#   LIST     the rectangle of the message list
#   SEARCH   the search field, x,y; empty for no search
#   STORE    where the client keeps the benchmark account, for disk size

client_spec() {
  APPNAME=""; TITLE=""; SEARCH=""; SEARCH_SUBMIT=""; SEARCH_CLEAR="cmd+a,backspace"
  # What clear_search does last, once the field is empty: Escape, to leave it.
  SEARCH_DONE=(--key escape)
  # A client whose search needs its own sequence sets these instead:
  # SEARCH_PREPARE replaces the click-and-clear before each search, and
  # SEARCH_RESET is the whole of clear_search.
  SEARCH_PREPARE=""; SEARCH_RESET=()
  case "$1" in
    spool)
      # Verified by snapshot, Spool 0.1.10: 27pt conversation rows, reader
      # to the right of the list.
      PROC="spool"; BUNDLE="com.joedesigns.spool"
      ROW="420,301"; ROW_DY=54
      READER="790,90,580,700"
      LIST="225,70,540,800"
      SEARCH="1250,18"
      STORE="$PROFILE/data/spool"
      APP_PATH="$SPOOL_APP"
      ;;
    applemail)
      # Verified by snapshot, macOS 15.4: 80pt two-line rows, list between
      # x 200 and 600, reader to the right, search in the toolbar.
      PROC="Mail"; BUNDLE="com.apple.mail"; TITLE="Inbox"
      ROW="420,340"; ROW_DY=80
      READER="780,120,580,700"
      LIST="210,60,385,800"
      SEARCH="1290,27"
      STORE="$(applemail_store || true)"
      APP_PATH="/System/Applications/Mail.app"
      ;;
    mailmate)
      # Verified by snapshot, MailMate 1.13.2 default layout: the list is a
      # band across the top (18pt rows) and the reader is *below* it.
      # TITLE: a stray Return opens a message in a window of its own, and the
      # main window's title always carries the mailbox name.
      PROC="MailMate"; BUNDLE="com.freron.MailMate"; TITLE="Inbox"
      ROW="450,149"; ROW_DY=36
      READER="210,330,1180,560"
      LIST="210,88,1180,220"
      SEARCH="1230,41"
      # MailMate searches on Return, not as you type. Without it the query sits
      # in the field and the list never changes.
      SEARCH_SUBMIT=$'\n'
      # And clearing the field only brings the full list back on Return too.
      # Escape, not Return: Return on an empty field falls through to the list
      # and opens the selected message in a window of its own.
      SEARCH_CLEAR="cmd+a,backspace,escape"
      STORE="$HOME/Library/Application Support/MailMate"
      APP_PATH="/Applications/MailMate.app"
      ;;
    canary)
      # Verified by snapshot, Canary Mail 5.26: Copilot panel closed (the AI
      # button), Discover cards dismissed. 91pt conversation rows; the reader
      # to the right shows the whole conversation.
      #
      # Search is the toolbar field: as-you-type, across every folder. Focused
      # by clicking where the magnifier sits, which is inside the field when
      # it is open and the button that opens it when it is not, so the click
      # lands on search either way. Cleared with the field's own ⓧ and left
      # with "‹ Back" — no select-all, backspace or Escape, because Canary
      # has single-key shortcuts in the list (e archives).
      PROC="Canary Mail"; BUNDLE="io.canarymail.mac"; TITLE="Inbox"
      ROW="480,175"; ROW_DY=91
      READER="690,60,680,800"
      LIST="325,105,330,790"
      SEARCH="1101,27"
      # The magnifier toggles the field, so every run starts from a known
      # state: "‹ Back" (the All tab, when no search is open) closes it and
      # returns to the inbox, and the magnifier then opens it empty.
      SEARCH_PREPARE="click@350:72,wait@800,click@1101:27,wait@500"
      SEARCH_RESET=(--click 350,72)
      STORE=""
      APP_PATH="/Applications/Canary Mail.app"
      ;;
    airmail)
      # Placeholder coordinates until checked by snapshot.
      PROC="Airmail"; BUNDLE="it.bloop.airmail2"; TITLE="Airmail"
      ROW="520,150"; ROW_DY=95
      READER="820,140,560,700"
      LIST="350,60,340,800"
      SEARCH=""
      STORE=""
      APP_PATH="/Applications/Airmail.app"
      ;;
    thunderbird)
      # Verified by snapshot, Thunderbird 144 default "cards" layout, with the
      # Quick Filter bar left open — it stays open across launches, and the
      # list sits below it. 66pt cards; the reader is to the right.
      #
      # Search is the Quick Filter, Thunderbird's search-as-you-type over the
      # folder (sender, recipients and subject by default; not the body).
      # Cleared with the field's own × and never with select-all and
      # backspace, and never closed with Escape: Backspace in Thunderbird's
      # message list deletes, and a closed bar would put a row where the
      # field was. A missed click here selects a message and nothing else.
      PROC="thunderbird"; BUNDLE="org.mozilla.thunderbird"; TITLE="Inbox"
      ROW="520,150"; ROW_DY=66
      READER="820,60,560,800"
      LIST="250,160,550,710"
      SEARCH="400,101"
      SEARCH_CLEAR="click@484:101,click@400:101"
      SEARCH_DONE=(--click 400,101)
      STORE="$(ls -d "$HOME"/Library/Thunderbird/Profiles/*.default-release/ImapMail 2>/dev/null | head -1)"
      APP_PATH="/Applications/Thunderbird.app"
      ;;
    neomutt)
      # A terminal client has no bundle of its own; it is whatever emulator is
      # hosting it, and the window title is set by neomutt.
      PROC="neomutt"; BUNDLE=""; APPNAME="Ghostty"; TITLE="neomutt"
      ROW="400,200"; ROW_DY=18
      READER="20,60,1360,800"
      LIST="20,60,1360,800"
      STORE="$BENCH/neomutt"
      APP_PATH="$(command -v neomutt || true)"
      ;;
    *) echo "unknown client: $1" >&2; exit 2 ;;
  esac
}

CLIENTS=(spool applemail mailmate thunderbird neomutt)

# ------------------------------------------------------------------- helpers

say() { printf '%s\n' "$*" >&2; }

ensure_tools() {
  local src bin
  for bin in measure calibrate; do
    src="$HERE/$bin.swift"
    # Built here rather than committed: a binary built on another machine's
    # macOS is the instrument from a different experiment.
    if [[ ! -x "$HERE/$bin" || "$src" -nt "$HERE/$bin" ]]; then
      say "building ${bin}…"
      swiftc -O "$src" -o "$HERE/$bin"
    fi
  done
}

target_args() {
  TARGET=()
  if [[ -n "$BUNDLE" ]]; then TARGET+=(--app "$BUNDLE"); else TARGET+=(--app-name "$APPNAME"); fi
  [[ -n "$TITLE" ]] && TARGET+=(--window-title "$TITLE")
  return 0
}

pid_of() { pgrep -x "$1" | head -1; }

app_version() {
  local plist="$1/Contents/Info.plist"
  [[ -f "$plist" ]] && /usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$plist" 2>/dev/null || echo ""
}

resize() {
  osascript >/dev/null 2>&1 <<OSA || say "  (could not resize $PROC — set it to ${WIN_W}x${WIN_H} at ${WIN_X},${WIN_Y} by hand)"
tell application "System Events"
  tell process "$PROC"
    set frontmost to true
    -- The main window by title where there is one: "front window" is
    -- whatever was last opened, and a stray compose or spelling window
    -- would be the one resized and measured.
    if "$TITLE" is "" then
      set target to front window
    else
      set target to first window whose name contains "$TITLE"
    end if
    perform action "AXRaise" of target
    set position of target to {$WIN_X, $WIN_Y}
    set size of target to {$WIN_W, $WIN_H}
  end tell
end tell
OSA
  sleep 1
}

# Where Apple Mail keeps the *benchmark* account, and nothing else.
#
# `~/Library/Mail` is every account on the machine. Measuring the lot and
# calling it Apple Mail's footprint for the corpus would be wrong, in Spool's
# favour. Mail keeps one directory per account under `V*/<uuid>/`, named by
# mailbox; `Lists.mbox` comes from `Lists/rust` in the generated corpus and
# marks the benchmark account.
#
# `MailData/` (the Envelope Index) is shared across accounts and cannot be
# attributed, so the figure excludes it. Say so next to it.
applemail_store() {
  local dir
  for dir in "$HOME"/Library/Mail/V*/*/; do
    [[ -d "$dir/Lists.mbox" ]] && { printf '%s' "$dir"; return 0; }
  done
  return 1
}

session_line() {
  local client="$1" version="$2"
  local display
  display="$(system_profiler SPDisplaysDataType 2>/dev/null \
            | awk -F': ' '/UI Looks like/ {print $2; exit}')"
  printf '{"event":"session","client":"%s","version":"%s","machine":"%s","model":"%s","os":"%s","display":"%s","window":"%dx%d","runs":%d,"search_term":"%s","spool_commit":"%s","corpus":"%s","at":%d}\n' \
    "$client" "$version" "$MACHINE" "$(sysctl -n hw.model)" "$(sw_vers -productVersion)" \
    "$display" "$WIN_W" "$WIN_H" "$RUNS" "$SEARCH_TERM" \
    "$(git -C "$ROOT" rev-parse --short HEAD 2>/dev/null)" "${CORPUS:-50k}" "$(date +%s)"
}

conditions_warn() {
  local load
  load="$(sysctl -n vm.loadavg | awk '{print $2}')"
  if awk -v l="$load" 'BEGIN {exit !(l > 2.0)}'; then
    say "  warning: load average $load — something else is busy, and it lands in the tail of every number"
  fi
}

# ------------------------------------------------------------------- measure

measure_client() {
  local client="$1"
  client_spec "$client"
  # TAG keeps an A/B apart: `TAG=fix SPOOL_APP=… run.sh spool` writes
  # spool@fix.jsonl, and the report shows it as its own column.
  local name="$client${TAG:+@$TAG}"
  local out="$RESULTS/$name.jsonl"

  if [[ -z "$(pid_of "$PROC")" ]]; then
    say "== $client: not running, skipped"
    return 1
  fi
  say "== $client =="
  ensure_unlocked
  conditions_warn
  [[ -n "$BUNDLE" ]] && resize
  target_args

  local version
  case "$client" in
    spool) version="$(app_version "$APP_PATH")"
           [[ "$APP_PATH" == "$ROOT"/* ]] && version="$version+$(git -C "$ROOT" rev-parse --short HEAD)" ;;
    neomutt) version="$(neomutt -v 2>/dev/null | head -1)" ;;
    *) version="$(app_version "$APP_PATH")" ;;
  esac
  session_line "$name" "$version" >>"$out"

  # 0. Start from the whole mailbox. A search left in the field by an earlier
  #    pass filters the list to a page of results, and arrowing down a short
  #    list runs off the end and measures nothing.
  clear_search

  # 1. Open a message: click a row, watch the reader pane. Alternates between
  #    two rows, so every click lands on one that is not already selected.
  want open && { say "  open a message"
  "$MEASURE" "${TARGET[@]}" --region "$READER" --click "$ROW" --click-alternate "$ROW_DY" \
      --runs "$RUNS" --label "$name/open-message" >>"$out"; }

  # 2. Next message with the keyboard: the same work without the mouse and
  #    without the row-highlight confound.
  want next && { say "  next message (down arrow)"
  "$MEASURE" "${TARGET[@]}" --region "$READER" --key down \
      --runs "$RUNS" --label "$name/next-message" >>"$out"; }

  # 3. Scroll the list. `settle` is the interesting one: how long after the
  #    wheel stops does the list stop changing. Before search, so the list is
  #    the whole mailbox rather than a page of results.
  want scroll && { say "  scroll"
  "$MEASURE" "${TARGET[@]}" --region "$LIST" --scroll "-8,30" \
      --runs 10 --label "$name/scroll" >>"$out"; }

  # 4. Search. Watch the list, because the results are what the query changes.
  #    Each run empties the field first and lets the full list come back, so
  #    every run types the same query into the same empty field.
  if [[ -n "$SEARCH" ]] && want search; then
    say "  search for '$SEARCH_TERM'"
    [[ -z "$SEARCH_PREPARE" ]] && "$MEASURE" "${TARGET[@]}" --region "$LIST" --click "$SEARCH" \
        --runs 1 --timeout-ms 2000 --label "$name/focus-search" >/dev/null
    "$MEASURE" "${TARGET[@]}" --region "$LIST" --type "$SEARCH_TERM$SEARCH_SUBMIT" \
        --prepare "${SEARCH_PREPARE:-click@${SEARCH/,/:},$SEARCH_CLEAR}" --quiet-ms 1500 \
        --runs "$RUNS" --label "$name/search" >>"$out"
    clear_search
  fi

  want static && static "$name" >>"$out"

  # 6. Launch and quit, last because it restarts the client.
  [[ -n "$BUNDLE" ]] && want launch && launch_cycle "$client" "$name" >>"$out"
  say "  -> $out"
}

# Quit the client, then launch and quit it LAUNCH_RUNS times (run 1 is the
# cold-ish one and is left out of the medians), then start it again for
# whatever comes next. Window position is whatever the client restores, which
# after a measured run is the benchmark size.
launch_cycle() {
  local client="$1" name="$2" pid
  local env=()
  if [[ "$client" == spool ]]; then
    env=(--env "XDG_CONFIG_HOME=$PROFILE/config" --env "XDG_DATA_HOME=$PROFILE/data")
  fi
  say "  launch and quit (${LAUNCH_RUNS:-6} cycles)"
  # Not timed, so a signal: an app started from its binary rather than by
  # Launch Services does not always answer an AppleScript quit.
  osascript -e "tell application id \"$BUNDLE\" to quit" >/dev/null 2>&1 || true
  for _ in $(seq 1 50); do [[ -z "$(pid_of "$PROC")" ]] && break; sleep 0.1; done
  pkill -x "$PROC" 2>/dev/null || true
  for _ in $(seq 1 100); do [[ -z "$(pid_of "$PROC")" ]] && break; sleep 0.1; done
  sleep 2
  "$MEASURE" "${TARGET[@]}" --launch "$APP_PATH" ${env[@]+"${env[@]}"} --region "$LIST" \
      --runs "${LAUNCH_RUNS:-6}" --timeout-ms 60000 --label "$name"
  if [[ "$client" == spool ]]; then
    spool_up >&2
  else
    open -a "$APP_PATH"
  fi
  sleep 5
}

# MailMate opens a message in its own window now and then (a Return that
# reaches the list). Close any window that is not the main one, so none sits
# over the rows being clicked.
close_strays() {
  [[ "$PROC" == "MailMate" ]] || return 0
  osascript -e 'tell application "System Events" to tell process "MailMate" to click (button 1 of (every window whose name does not contain "Inbox"))' >/dev/null 2>&1 || true
}

# Stops before a run rather than recording twenty timeouts: a locked screen
# captures black and takes no input, and `measure` says so and exits 3.
ensure_unlocked() {
  "$MEASURE" --list >/dev/null || { say "stopping: $MEASURE cannot measure (see above)"; exit 3; }
}

# ACTIONS picks which steps run, e.g. ACTIONS="open next"; all of them by default.
want() { [[ " ${ACTIONS:-open next scroll search static launch} " == *" $1 "* ]]; }

clear_search() {
  close_strays
  [[ -n "$SEARCH" ]] || return 0
  if (( ${#SEARCH_RESET[@]} )); then
    "$MEASURE" "${TARGET[@]}" --region "$LIST" "${SEARCH_RESET[@]}" \
        --runs 1 --timeout-ms 3000 --quiet-ms 500 --label setup >/dev/null
    return 0
  fi
  "$MEASURE" "${TARGET[@]}" --region "$LIST" --click "$SEARCH" \
      --runs 1 --timeout-ms 1000 --quiet-ms 100 --label setup >/dev/null
  "$MEASURE" "${TARGET[@]}" --region "$LIST" --prepare "click@${SEARCH/,/:},$SEARCH_CLEAR" "${SEARCH_DONE[@]}" \
      --runs 1 --timeout-ms 3000 --quiet-ms 500 --label setup >/dev/null
  close_strays
}

# CPU time a process has used, in seconds. `ps` prints [[dd-]hh:]mm:ss.cc.
cpu_seconds() {
  ps -o time= -p "$1" | awk -F'[:-]' '{ s = 0; for (i = 1; i <= NF; i++) s = s * 60 + $i; print s }'
}

# Memory and disk, after the measurements, with the client idle.
static() {
  local client="$1" pid rss=0 disk=0 memory='{}'
  pid="$(pid_of "$PROC")"
  [[ -n "$pid" ]] && rss="$(ps -o rss= -p "$pid" | tr -d ' ')"
  # What Activity Monitor shows: physical footprint, with the WebKit helpers
  # each client renders mail in. See footprint.py for why not RSS.
  [[ -n "$pid" ]] && memory="$(python3 "$HERE/footprint.py" "$pid" 2>/dev/null || echo '{}')"
  # Apparent size, not allocated blocks: 50,000 one-kilobyte files occupy four
  # times their content in 4K blocks, and that is the filesystem's number.
  [[ -n "$STORE" && -e "$STORE" ]] && disk="$(du -sk -A "$STORE" 2>/dev/null | cut -f1)"
  # Idle CPU: what the client costs sitting there, which is most of its life.
  # Ten seconds of the main process doing nothing it was asked to.
  local idle="null"
  if [[ -n "$pid" ]]; then
    local before after
    before="$(cpu_seconds "$pid")"; sleep 10; after="$(cpu_seconds "$pid")"
    idle="$(python3 -c "print(round(($after - $before) / 10 * 100, 2))")"
  fi
  say "  memory $(python3 -c 'import json,sys; m=json.loads(sys.argv[1]); print(m.get("total_kb",0)//1024, "MB,", m.get("main_kb",0)//1024, "MB main")' "$memory")   store $((disk / 1024)) MB   idle cpu ${idle}%"
  # Main process only. Apple Mail renders messages in WebKit's WebContent
  # processes and keeps more in helpers; this column undercounts it, and the
  # report says so rather than guessing which helpers to add.
  printf '{"event":"static","client":"%s","rss_kb":%s,"memory":%s,"store_kb":%s,"idle_cpu_pct":%s,"store":"%s"}\n' \
    "$client" "${rss:-0}" "$memory" "${disk:-0}" "$idle" "$STORE"
}

snapshot() {
  local client="$1"
  client_spec "$client"
  [[ -n "$(pid_of "$PROC")" ]] || { say "$client is not running"; exit 1; }
  [[ -n "$BUNDLE" ]] && resize
  target_args
  local dir="$RESULTS/snapshots"; mkdir -p "$dir"
  "$MEASURE" "${TARGET[@]}" --region "$READER" --click "$ROW" --click-alternate "$ROW_DY" \
      --snapshot "$dir/$client-reader.png" >/dev/null
  "$MEASURE" "${TARGET[@]}" --region "$LIST" ${SEARCH:+--click "$SEARCH"} \
      --snapshot "$dir/$client-list.png" >/dev/null
  say "red: watched region   blue: where clicks land"
  say "  $dir/$client-reader.png   (open a message)"
  say "  $dir/$client-list.png     (scroll and search)"
  [[ -z "${SPOOL_BENCH_DESKTOP:-}" ]] && open "$dir/$client-reader.png" "$dir/$client-list.png"; true
}

# ----------------------------------------------------------------- calibrate

calibrate() {
  local out="$RESULTS/calibration.jsonl" delay log
  ensure_unlocked
  session_line calibrate "" >>"$out"
  for delay in 50 200 800; do
    log="$(mktemp)"
    "$CALIBRATE" --delay "$delay" 2>"$log" &
    local cpid=$!
    sleep 2
    say "calibrating at ${delay}ms…"
    "$MEASURE" --app-name calibrate --window-title Calibrate --click 450,350 \
        --runs 10 --label "calibrate/$delay" | tee -a "$out" | grep summary >&2 || true
    kill "$cpid" 2>/dev/null; wait "$cpid" 2>/dev/null || true
    # What the target actually did, so overhead = reported - fired.
    grep '"fired"' "$log" | sed "s/^{/{\"label\":\"calibrate\/$delay\",/" >>"$out"
    rm -f "$log"
  done
  python3 "$HERE/report.py" --calibration "$out"
}

# --------------------------------------------------------------------- spool

# Spool against the benchmark profile, never the real one: its own config and
# data directories, so nothing here can touch real mail.
spool_up() {
  local profile="$PROFILE"
  local app="$SPOOL_APP"
  mkdir -p "$profile/config/spool" "$profile/data"
  if [[ ! -f "$profile/config/spool/accounts.toml" ]]; then
    # `initial_limit` is larger than the corpus on purpose: the default would
    # have Spool sync a fraction of what the other clients sync, which is the
    # easiest way to accidentally win a benchmark.
    cat >"$profile/config/spool/accounts.toml" <<TOML
[[accounts]]
name = "Bench"
email = "bench@example.com"
auth = "password"
imap_host = "127.0.0.1"
imap_port = $PORT
imap_security = "plaintext"
smtp_host = "127.0.0.1"
smtp_port = 10025
smtp_security = "plaintext"
password = "bench"
initial_limit = 1000000
TOML
  fi
  if [[ ! -x "$app/Contents/MacOS/spool" ]]; then
    # gpui compiles its Metal shaders at build time, and the Command Line
    # Tools do not include the Metal compiler; only a full Xcode does.
    if ! xcrun -f metal >/dev/null 2>&1; then
      say "can't build Spool here: the Metal compiler is missing (it comes with Xcode, not the Command Line Tools)."
      say "  either install Xcode and run: sudo xcode-select -s /Applications/Xcode.app"
      say "  or use the release:          bench/run.sh spool-install"
      exit 1
    fi
    say "building Spool.app…"; sh "$ROOT/scripts/bundle.sh"
  fi
  # Through Launch Services, as the other clients are and as a person would:
  # macOS then counts the WebKit processes that render its mail as Spool's,
  # where a binary started from a shell has them charged to the terminal.
  # `--env` carries the benchmark profile across, which `open` once could not.
  open -a "$app" --env "XDG_CONFIG_HOME=$profile/config" --env "XDG_DATA_HOME=$profile/data" \
    --stdout "$BENCH/spool.log" --stderr "$BENCH/spool.log"
  say "Spool started on the benchmark profile (log: $BENCH/spool.log)"
  say "store: $profile/data/spool"
}

# Builds the working tree into ~/spool-bench/apps/Spool-<tag>.app, for an A/B
# against another tag or the release. `runtime_shaders` has gpui compile its
# Metal shaders when the app starts instead of at build time, so this needs no
# Xcode — and costs the app some milliseconds at launch, so compare launch
# times only between builds made this way, never against the release.
spool_build() {
  local tag="$1" app="$BENCH/apps/Spool-$1.app"
  [[ -d /Applications/Spool.app ]] || { say "needs the release in /Applications for its bundle: bench/run.sh spool-install"; exit 1; }
  (cd "$ROOT" && cargo build --release -p spool --features gpui/runtime_shaders) >&2
  mkdir -p "$BENCH/apps"; rm -rf "$app"
  ditto /Applications/Spool.app "$app"
  cp "$ROOT/target/release/spool" "$app/Contents/MacOS/spool"
  codesign --force --deep -s - "$app" 2>/dev/null
  say "built $app ($(git -C "$ROOT" rev-parse --short HEAD)$(git -C "$ROOT" diff --quiet || echo +dirty))"
  say "  TAG=$tag SPOOL_APP=$app bench/run.sh spool"
}

# How long a first sync takes: Spool on an empty profile, timed until its store
# holds every message the server has. The profile's data is moved aside, never
# deleted, so a store that took an hour to build is not lost to a typo.
sync_time() {
  local data="$PROFILE/data" total count start now
  total="$(MAILDIR="$( [[ -n "$CORPUS" ]] && echo "$BENCH/maildir-$CORPUS" || echo "$BENCH/maildir")" PORT="$PORT" \
           "$HERE/server.sh" status | awk -F'messages=' '/messages=/ {n += $2} END {print n+0}')"
  [[ "$total" -gt 0 ]] || { say "the server on port $PORT is not up"; exit 1; }
  pkill -x spool 2>/dev/null || true
  for _ in $(seq 1 50); do [[ -z "$(pid_of spool)" ]] && break; sleep 0.1; done
  if [[ -d "$data" ]]; then mv "$data" "$data.$(date +%Y%m%d-%H%M%S)"; fi
  say "first sync of $total messages (port $PORT)…"
  start="$(date +%s)"
  spool_up >/dev/null
  local db="$data/spool/mail.db" last=-1 stalled=0
  while :; do
    sleep 2
    count="$(sqlite3 "file:$db?mode=ro" 'select count(*) from messages' 2>/dev/null || echo 0)"
    now="$(date +%s)"
    printf '\r  %ss  %s / %s ' "$((now - start))" "$count" "$total" >&2
    [[ "$count" -ge "$total" ]] && break
    if [[ "$count" == "$last" ]]; then stalled=$((stalled + 1)); else stalled=0; fi
    last="$count"
    [[ "$stalled" -ge 60 ]] && { echo >&2; say "stalled at $count for two minutes"; break; }
  done
  echo >&2
  local secs=$((now - start))
  say "spool: $count messages in ${secs}s ($(( count / (secs > 0 ? secs : 1) )) msg/s)"
  printf '{"event":"sync","client":"spool%s","messages":%s,"seconds":%s,"corpus":"%s","version":"%s"}\n' \
    "${TAG:+@$TAG}" "$count" "$secs" "${CORPUS:-50k}" "$(app_version "$SPOOL_APP")" >>"$RESULTS/sync.jsonl"
}

permissions() {
  if "$MEASURE" --permissions; then
    say "both granted"
    return 0
  fi
  say "turn on the switch for Terminal in both panes that just opened, then run this again"
  open "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
  sleep 1
  open "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
  return 1
}

# Apple Mail's own scripting interface can create an IMAP account, which saves
# the hand-filled sheet. The first run asks once for permission to control Mail.
mail_account() {
  osascript <<'OSA'
tell application "Mail"
  if (count of (every account whose name is "Bench")) > 0 then return "Bench account already exists"
  set acct to make new imap account with properties {name:"Bench", user name:"bench@example.com", password:"bench", server name:"127.0.0.1", port:10143, uses ssl:false, email addresses:{"bench@example.com"}, full name:"Bench"}
  set enabled of acct to true
  return "Bench account created; Mail is syncing it now"
end tell
OSA
}

# The release, from the same URL a person downloads it from, into
# /Applications the way they would install it.
spool_install() {
  local tmp mnt
  tmp="$(mktemp -d)"; mnt="$tmp/mnt"
  say "downloading $SPOOL_DMG_URL"
  curl -fL --progress-bar -o "$tmp/Spool.dmg" "$SPOOL_DMG_URL"
  hdiutil attach -nobrowse -readonly -quiet -mountpoint "$mnt" "$tmp/Spool.dmg"
  if pgrep -x spool >/dev/null; then
    say "quitting the running Spool first"
    osascript -e 'quit app id "com.joedesigns.spool"' 2>/dev/null || pkill -x spool || true
    sleep 2
  fi
  rm -rf /Applications/Spool.app
  ditto "$mnt/Spool.app" /Applications/Spool.app
  hdiutil detach -quiet "$mnt"
  rm -rf "$tmp"
  say "installed Spool $(app_version /Applications/Spool.app) at /Applications/Spool.app"
}

# --------------------------------------------------------------------- doctor

doctor() {
  local ok="  ok" no="  --"
  say "machine   $(sysctl -n machdep.cpu.brand_string) ($(sysctl -n hw.model)), macOS $(sw_vers -productVersion)"
  say "display   $(system_profiler SPDisplaysDataType 2>/dev/null | awk -F': ' '/UI Looks like/ {print $2; exit}')"
  say "results   $RESULTS"
  say ""
  [[ -d "$BENCH/maildir/cur" ]] && say "$ok corpus at $BENCH/maildir" \
    || say "$no corpus: python3 bench/generate_corpus.py --count 50000 --out $BENCH/maildir"
  command -v dovecot >/dev/null && say "$ok dovecot $(dovecot --version | cut -d' ' -f1)" \
    || say "$no dovecot: brew install dovecot"
  "$HERE/server.sh" status >/dev/null 2>&1 && say "$ok server running on 127.0.0.1:10143" \
    || say "$no server: bench/server.sh start"
  command -v swiftc >/dev/null && say "$ok swiftc" || say "$no swiftc: xcode-select --install"
  [[ -s "$RESULTS/calibration.jsonl" ]] && say "$ok calibrated on this machine" \
    || say "$no not calibrated here: bench/run.sh calibrate"
  if [[ -x "$MEASURE" ]] && "$MEASURE" --list >/dev/null 2>&1; then
    say "$ok screen recording permission"
  else
    say "$no screen recording: System Settings ▸ Privacy & Security ▸ Screen Recording ▸ add your terminal"
  fi
  say "     (accessibility, for posting clicks and keys, is not checkable from here — grant it to your terminal too)"
  say ""
  local c
  for c in "${CLIENTS[@]}"; do
    client_spec "$c"
    if [[ -z "$APP_PATH" || ! -e "$APP_PATH" ]]; then
      case "$c" in
        spool) say "$no spool: bench/run.sh spool-install" ;;
        mailmate) say "$no mailmate: brew install --cask mailmate" ;;
        thunderbird) say "$no thunderbird: brew install --cask thunderbird" ;;
        neomutt) say "$no neomutt: brew install neomutt" ;;
      esac
    elif [[ -n "$(pid_of "$PROC")" ]]; then
      say "$ok $c running ($APP_PATH)"
    else
      say "  ..  $c installed, not running"
    fi
  done
}

# ------------------------------------------------------------- over ssh
#
# Nothing started over SSH can capture the screen or post input: it is outside
# the desktop's login session, and macOS attributes its permission requests to
# sshd rather than to anything a person can grant them to. So from an SSH
# session, the commands that touch the screen re-run themselves in Terminal.app
# on the Mac's own display — which is where Screen Recording and Accessibility
# get granted — and the output streams back here.

in_desktop() {
  local dir; dir="$(mktemp -d)"
  {
    echo '#!/bin/bash'
    # Terminal does not pass a locale to a .command script, and without UTF-8
    # bash reads the bytes of a `…` after `$var` as part of the variable name.
    echo 'export LANG=en_US.UTF-8 LC_ALL=en_US.UTF-8'
    printf 'cd %q\n' "$PWD"
    printf 'export SPOOL_BENCH_DESKTOP=1 RUNS=%q SEARCH_TERM=%q SPOOL_APP=%q TAG=%q CORPUS=%q ACTIONS=%q
' "$RUNS" "$SEARCH_TERM" "$SPOOL_APP" "${TAG:-}" "$CORPUS" "${ACTIONS:-}"
    # Holds the display awake for the run: a display that sleeps mid-run locks
    # the screen, and a locked screen measures nothing.
    printf 'caffeinate -di '; printf '%q ' "$0" "$@"; printf '> %q 2>&1
' "$dir/out"
    printf 'echo $? > %q
' "$dir/status"
    # Close this window once done; the output already went to ssh.
    echo 'osascript -e "tell application \"Terminal\" to close (every window whose name contains \"run.command\")" >/dev/null 2>&1 &'
  } >"$dir/run.command"
  chmod +x "$dir/run.command"
  : >"$dir/out"
  say "(over ssh: running in Terminal on this Mac's display)"
  open -a Terminal "$dir/run.command"
  tail -n +1 -f "$dir/out" &
  local tailer=$!
  while [[ ! -f "$dir/status" ]]; do sleep 1; done
  sleep 1; kill "$tailer" 2>/dev/null; wait "$tailer" 2>/dev/null || true
  local status; status="$(cat "$dir/status")"
  rm -rf "$dir"
  return "$status"
}

if [[ -n "${SSH_CONNECTION:-}" && -z "${SPOOL_BENCH_DESKTOP:-}" ]]; then
  case "${1:-}" in
    calibrate|snapshot|launch|all|spool|applemail|mailmate|thunderbird|airmail|canary|neomutt|permissions|mail-account)
      in_desktop "$@"; exit $? ;;
  esac
fi

# --------------------------------------------------------------------- main

ensure_tools

case "${1:-}" in
  doctor) doctor ;;
  permissions) permissions ;;
  mail-account) mail_account ;;
  calibrate) calibrate ;;
  spool-install) spool_install ;;
  spool-up) spool_up ;;
  spool-build) spool_build "${2:?a tag for the build, e.g. fix}" ;;
  sync-time) sync_time ;;
  snapshot) snapshot "${2:?which client}" ;;
  launch)
    # Launch and quit only, into the client's results as a session of its own.
    shift
    for c in "$@"; do
      client_spec "$c"; target_args
      name="$c${TAG:+@$TAG}"
      ensure_unlocked
      session_line "$name" "$(app_version "$APP_PATH")" >>"$RESULTS/$name.jsonl"
      launch_cycle "$c" "$name" | tee -a "$RESULTS/$name.jsonl" | grep summary >&2 || true
    done ;;
  report) shift; python3 "$HERE/report.py" "$RESULTS" "$@" ;;
  all)
    for c in "${CLIENTS[@]}"; do measure_client "$c" || true; done
    python3 "$HERE/report.py" "$RESULTS" ;;
  spool|applemail|mailmate|thunderbird|airmail|canary|neomutt)
    for c in "$@"; do measure_client "$c" || true; done
    python3 "$HERE/report.py" "$RESULTS" ;;
  *)
    sed -n '2,20p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2 ;;
esac
