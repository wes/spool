#!/bin/bash
# Cuts a release from this Mac: the version, the notes, a signed and notarized
# disk image, the tag, and the release on this repository, wes/spool.
#
#   scripts/release.sh
#
# It asks which kind of release this is, shows what it is about to do, and
# does nothing outward until you say yes. It builds before it commits: a build
# or a notarization that fails leaves nothing pushed and the version where it
# was, so it can simply be run again.
#
# Needs, once per machine (see RELEASING.md):
#   - the Developer ID certificate in the keychain,
#   - notarization credentials saved as a keychain profile:
#       xcrun notarytool store-credentials spool \
#           --apple-id <your-apple-id> --team-id 288BJX6YHP
#   - the Google OAuth client, in the keychain (see scripts/package.sh),
#   - gh, logged in as someone who can write to wes/spool.
#
# SPOOL_IDENTITY and NOTARY_PROFILE override the identity and profile.
#
# The version lives in three places and they have to agree — Cargo.toml (the
# DMG's name and what the updater compares), Info.plist (what the Finder
# shows) and the tag (what installed copies are offered) — so this is the only
# thing that should change them.

set -euo pipefail

cd "$(dirname "$0")/.."

PLIST=crates/app/bundle/Info.plist
BRANCH=master
RELEASES_REPO=wes/spool
TEAM=288BJX6YHP

bold() { printf '\033[1m%s\033[0m\n' "$*"; }
fail() { printf '\033[31m%s\033[0m\n' "$*" >&2; exit 1; }

# --- what this Mac needs ----------------------------------------------------

[ "$(uname)" = "Darwin" ] || fail "Releases are built on a Mac."

# rustup's cargo, not whichever is first on PATH. A Homebrew Rust ahead of it
# builds for this Mac's architecture only and ignores rust-toolchain.toml, so
# the universal build fails half-way; rustup's follows the pinned toolchain
# and both targets it lists.
[ -x "$HOME/.cargo/bin/rustup" ] ||
    fail "rustup is not installed. Install it from https://rustup.rs — Homebrew's Rust cannot build both architectures."
export PATH="$HOME/.cargo/bin:$PATH"

# The team is pinned, not just "a Developer ID": installed copies refuse an
# update signed by anyone else, so a release signed with the wrong one of
# several certificates is a release nobody can install.
IDENTITY=${SPOOL_IDENTITY:-$(security find-identity -v -p codesigning \
    | grep -o "\"Developer ID Application: [^\"]*($TEAM)\"" | head -1 | tr -d '"' || true)}
[ -n "$IDENTITY" ] || fail "No \"Developer ID Application: … ($TEAM)\" certificate in the keychain."

PROFILE=${NOTARY_PROFILE:-spool}
xcrun notarytool history --keychain-profile "$PROFILE" >/dev/null 2>&1 ||
    fail "No working notarization profile called \"$PROFILE\". Save one with:
  xcrun notarytool store-credentials $PROFILE --apple-id <your-apple-id> --team-id $TEAM"

# Checked here as well as in package.sh, so a missing client is found before
# the questions rather than after them.
if [ -z "${SPOOL_GOOGLE_CLIENT_ID:-}" ] &&
    ! security find-generic-password -s spool-release -a google-client-id >/dev/null 2>&1; then
    fail "No Google OAuth client for the build. Save it in the keychain — see the top of scripts/package.sh."
fi

gh auth status >/dev/null 2>&1 || fail "gh is not logged in. Run: gh auth login"

# --- where things stand -----------------------------------------------------

[ "$(git rev-parse --abbrev-ref HEAD)" = "$BRANCH" ] ||
    fail "Releases are cut from $BRANCH; this is $(git rev-parse --abbrev-ref HEAD)."

# Only the changelog may have uncommitted changes: it is the one file a
# release is expected to be written alongside. Anything else would ship in the
# release without anyone having meant it to.
stray=$(git status --porcelain | grep -v ' CHANGELOG.md$' || true)
[ -z "$stray" ] || fail "Commit or stash these first — only CHANGELOG.md may be uncommitted:
$stray"

git fetch --quiet origin "$BRANCH" --tags
[ "$(git rev-parse HEAD)" = "$(git rev-parse "origin/$BRANCH")" ] ||
    fail "$BRANCH is not the same as origin/$BRANCH. Pull or push first."

current=$(sed -n 's/^version = "\(.*\)"$/\1/p' Cargo.toml | head -1)
[[ "$current" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "Could not read the version from Cargo.toml."
IFS=. read -r major minor patch <<<"$current"

tagged() { git rev-parse -q --verify "refs/tags/v$1" >/dev/null; }
published() { gh release view "v$1" --repo "$RELEASES_REPO" >/dev/null 2>&1; }

# --- which version ----------------------------------------------------------

bold "Spool is at $current."
options=()
if tagged "$current" && ! published "$current"; then
    # Tagged, and the build or the publish after it never finished. Built
    # from here rather than from the tag, which is only right while the app
    # itself has not changed since — so that is checked.
    if git diff --quiet "v$current" HEAD -- crates extensions Cargo.toml Cargo.lock rust-toolchain.toml; then
        options+=("$current   — tagged but never published: build and publish it")
    else
        bold "v$current is tagged but was never published, and the app has changed since."
        echo "  Release a new version instead; $current will simply be skipped."
    fi
elif ! tagged "$current"; then
    # Never tagged, so never shipped: it can go out as it is — which is how
    # the first release goes.
    options+=("$current   — release the current version, never tagged")
fi
options+=(
    "$major.$minor.$((patch + 1))   — patch: fixes"
    "$major.$((minor + 1)).0   — minor: new things"
    "$((major + 1)).0.0   — major: breaking changes"
)

PS3="Which release? "
select choice in "${options[@]}"; do
    [ -n "${choice:-}" ] && break
done
next=${choice%% *}

if [ "$next" != "$current" ] && tagged "$next"; then
    fail "v$next is already tagged."
fi

# --- the notes --------------------------------------------------------------

# A "## $next" section in CHANGELOG.md, if one was written, is the notes.
# Otherwise they are the commit subjects since the last tag — nothing to write
# first.
notes=$(awk -v v="## $next" '$0 == v { on = 1; next } on && /^## / { exit } on' CHANGELOG.md)
if [ -z "$(printf '%s' "$notes" | tr -d '[:space:]')" ]; then
    last=$(git describe --tags --abbrev=0 --match 'v*' 2>/dev/null || true)
    notes=$(git log --no-merges --format='- %s' ${last:+"$last..HEAD"} | grep -v '^- Release [0-9]' || true)
    [ -n "$notes" ] || notes="Spool $next."
fi

# --- what is about to happen ------------------------------------------------

echo
bold "Release notes for $next:"
printf '%s\n' "$notes" | sed 's/^/  /'
echo
bold "About to:"
[ "$next" = "$current" ] || echo "  • set the version to $next in Cargo.toml, Cargo.lock and Info.plist"
echo "  • build both architectures, sign as $IDENTITY, and notarize (about 10–15 minutes)"
tagged "$next" || echo "  • commit \"Release $next\" (if anything changed), tag v$next, push both"
echo "  • publish Spool-$next.dmg (and a copy as Spool.dmg) to $RELEASES_REPO — installed copies are offered it"
echo
read -r -p "Go ahead? [y/N] " answer
[[ "$answer" =~ ^[Yy]$ ]] || { echo "Nothing done."; exit 1; }

# --- the build --------------------------------------------------------------

# Until the commit, a failure puts the version back, so the tree is as it was
# and this can be run again. The notes stay: they were worth writing.
committed=0
restore() {
    if [ "$committed" = 0 ] && [ "$next" != "$current" ]; then
        git checkout -- Cargo.toml Cargo.lock "$PLIST" 2>/dev/null || true
        echo "The version is back at $current; nothing was committed or pushed." >&2
    fi
}
trap restore EXIT

if [ "$next" != "$current" ]; then
    sed -i '' "s/^version = \"$current\"$/version = \"$next\"/" Cargo.toml
    /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $next" "$PLIST"
    /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $next" "$PLIST"
    # Brings the workspace's own entries in Cargo.lock up to the new version
    # without upgrading a single dependency.
    cargo metadata --format-version 1 >/dev/null
fi

bold "Building and notarizing Spool ${next}…"
SPOOL_IDENTITY="$IDENTITY" NOTARY_PROFILE="$PROFILE" scripts/package.sh
dmg=target/Spool-$next.dmg
[ -f "$dmg" ] || fail "The build finished without $dmg."

# What an installed copy checks before it will update to this, checked here
# first: a release that fails these is one nobody can install.
spctl --assess --type open --context context:primary-signature "$dmg" ||
    fail "Gatekeeper does not accept $dmg."

# --- the tag ----------------------------------------------------------------

if ! tagged "$next"; then
    git add Cargo.toml Cargo.lock "$PLIST" CHANGELOG.md
    git diff --cached --quiet || git commit -q -m "Release $next"
    committed=1
    git tag -a "v$next" -m "Spool $next"
    git push -q origin "$BRANCH"
    git push -q origin "v$next"
    bold "Pushed v$next."
else
    committed=1
fi

# --- the release ------------------------------------------------------------

notes_file=$(mktemp)
printf '%s\n' "$notes" >"$notes_file"
# The same disk image a second time under a name that never changes, so
# https://github.com/$RELEASES_REPO/releases/latest/download/Spool.dmg is always
# the newest build. The versioned one goes first: it is the one the updater
# finds.
latest=target/Spool.dmg
cp "$dmg" "$latest"
gh release create "v$next" "$dmg" "$latest" \
    --repo "$RELEASES_REPO" \
    --title "Spool $next" \
    --notes-file "$notes_file" \
    --latest
rm -f "$notes_file"

bold "Spool $next is out: https://github.com/$RELEASES_REPO/releases/tag/v$next"
echo "Latest download: https://github.com/$RELEASES_REPO/releases/latest/download/Spool.dmg"
