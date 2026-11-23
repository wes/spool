#!/bin/sh
# Builds Spool and installs it into /Applications.
#
# For using the app rather than working on it: after this, Spool is in
# Launchpad and Spotlight, opens from the Dock, and survives `cargo clean`.
#
#   scripts/install.sh            build, install, and leave it closed
#   scripts/install.sh --open     and launch it
#   scripts/install.sh --no-build install whatever is already in target/
#
# Your mail is not touched. The store lives in ~/.local/share/spool and the
# accounts in ~/.config/spool, neither of which is inside the bundle — so the
# installed copy opens on exactly the mail the one in target/ was showing.
#
# Signing is the part worth knowing about. macOS ties keychain access to a
# program's signature, and an ad-hoc signature is different every build: run
# from target/, every rebuild is a new program as far as the keychain is
# concerned, and every rebuild asks again for the passwords it already had.
# So this signs with a Developer ID when the machine has one, which is stable
# across builds, and falls back to ad-hoc when it does not.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
app="$root/target/Spool.app"
installed="/Applications/Spool.app"

build=1
launch=0
for arg in "$@"; do
	case "$arg" in
		--no-build) build=0 ;;
		--open) launch=1 ;;
		*) echo "install.sh: unknown option $arg" >&2; exit 1 ;;
	esac
done

# SPOOL_IDENTITY if set; otherwise the Developer ID from the team official
# builds are signed by (TEAM_ID in update.rs), so this copy reaches the
# keychain items the released app already has; otherwise the first Developer
# ID on the machine. Its name is what `codesign` matches, so it is taken whole
# from the line rather than parsed.
team=$(sed -n 's/^const TEAM_ID: &str = "\(.*\)";/\1/p' "$root/crates/app/src/update.rs")
identities=$(
	security find-identity -v -p codesigning 2>/dev/null \
		| sed -n 's/.*"\(Developer ID Application: .*\)"/\1/p'
)
preferred=$(printf '%s\n' "$identities" | grep -F "($team)" | head -1)
identity=${SPOOL_IDENTITY:-${preferred:-$(printf '%s\n' "$identities" | head -1)}}
if [ -n "$identity" ]; then
	echo "signing as: $identity"
else
	identity="-"
	echo "signing ad-hoc: no Developer ID on this machine."
	echo "  The keychain will ask for your mail passwords again after a rebuild."
fi

# The Google OAuth client, from the environment or from where package.sh keeps
# it. Without one this build cannot refresh a Gmail sign-in made by an
# official build, since Google ties a refresh token to the client it was
# issued to.
keychain_value() {
	security find-generic-password -s spool-release -a "$1" -w 2>/dev/null || true
}
SPOOL_GOOGLE_CLIENT_ID=${SPOOL_GOOGLE_CLIENT_ID:-$(keychain_value google-client-id)}
SPOOL_GOOGLE_CLIENT_SECRET=${SPOOL_GOOGLE_CLIENT_SECRET:-$(keychain_value google-client-secret)}
if [ -z "$SPOOL_GOOGLE_CLIENT_ID" ] || [ -z "$SPOOL_GOOGLE_CLIENT_SECRET" ]; then
	echo "no Google OAuth client: Gmail accounts will not be able to sign in."
	echo "  See \"The Google OAuth client\" in README.md."
fi
export SPOOL_GOOGLE_CLIENT_ID SPOOL_GOOGLE_CLIENT_SECRET

if [ "$build" = "1" ]; then
	SPOOL_IDENTITY="$identity" sh "$root/scripts/bundle.sh"
fi

[ -d "$app" ] || { echo "install.sh: no $app to install. Drop --no-build." >&2; exit 1; }

# Quit a running copy first. Replacing the bundle under a running process
# leaves it holding files that no longer exist, and the next thing it does
# with them fails in a way that is nobody's idea of a good error message.
if pgrep -qf "Spool.app/Contents/MacOS/spool"; then
	echo "quitting the running Spool…"
	osascript -e 'quit app "Spool"' >/dev/null 2>&1 || true
	# Give it a moment to close its database cleanly.
	for _ in 1 2 3 4 5 6 7 8 9 10; do
		pgrep -qf "Spool.app/Contents/MacOS/spool" || break
		/bin/sleep 0.5
	done
	if pgrep -qf "Spool.app/Contents/MacOS/spool"; then
		echo "install.sh: Spool is still running. Quit it and try again." >&2
		exit 1
	fi
fi

# Only ever removes something that is Spool. `rm -rf` on a path assembled
# from a variable deserves to be sure what it is pointing at.
if [ -e "$installed" ]; then
	id=$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" \
		"$installed/Contents/Info.plist" 2>/dev/null || echo "")
	if [ "$id" != "com.joedesigns.spool" ]; then
		echo "install.sh: $installed is not Spool ($id). Leaving it alone." >&2
		exit 1
	fi
	rm -rf "$installed"
fi

# `ditto` rather than `cp -R`: it copies the extended attributes a signed
# bundle carries, and a signature that arrives without them is a signature
# that no longer verifies.
ditto "$app" "$installed"

codesign --verify --deep --strict "$installed"
echo "installed $installed"

# Anything else still running is on the same database. Not fatal — SQLite
# copes — but two copies of a mail client syncing one store is a way to be
# confused about which one just did something.
others=$(pgrep -f "target/release/spool" 2>/dev/null || true)
if [ -n "$others" ]; then
	echo
	echo "note: a Spool built into target/release is still running (pid $others)."
	echo "  It shares ~/.local/share/spool with the copy just installed."
fi

if [ "$launch" = "1" ]; then
	open "$installed"
fi
