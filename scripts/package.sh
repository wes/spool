#!/bin/sh
# Builds a signed, notarized Spool.dmg ready to hand to a stranger.
#
# `bundle.sh` makes an app that works on the machine that built it. This makes
# one that survives being downloaded, which is a different problem: a signature
# Apple recognises, a hardened runtime, a notarization ticket stapled to the
# disk image so Gatekeeper does not have to be online to check it, and both
# architectures in one binary because half the Macs in the world are not the
# one on this desk.
#
#   scripts/package.sh            builds target/Spool-<version>.dmg
#   scripts/package.sh --no-notarize   signs and packages, skips Apple
#
# What it needs, in the environment:
#
#   SPOOL_IDENTITY   the Developer ID, e.g.
#                    "Developer ID Application: JOE DESIGNS LLC (ULMVR3KWCR)"
#
#   SPOOL_GOOGLE_CLIENT_ID  SPOOL_GOOGLE_CLIENT_SECRET
#                    the Google OAuth client, compiled in. The source leaves
#                    it empty (see crates/net/src/oauth.rs), so a release
#                    built without these could not sign in to Gmail. Unset,
#                    they are read from the login keychain, saved there once
#                    with:
#
#       security add-generic-password -U -s spool-release -a google-client-id -w '<id>'
#       security add-generic-password -U -s spool-release -a google-client-secret -w '<secret>'
#
# and, to notarize, either a keychain profile made once by hand —
#
#   NOTARY_PROFILE   the name given to `xcrun notarytool store-credentials`
#
# — or the three values a runner has no keychain to hold:
#
#   APPLE_ID  APPLE_APP_PASSWORD  APPLE_TEAM_ID
#
# The profile is the local path and the three variables are the CI one; the
# script takes whichever it is given so the same code runs in both places, and
# a release is never the first time the packaging is exercised.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
notarize=1
[ "${1:-}" = "--no-notarize" ] && notarize=0

if [ -z "${SPOOL_IDENTITY:-}" ]; then
	echo "package.sh: SPOOL_IDENTITY is not set." >&2
	echo "Pick one from: security find-identity -v -p codesigning" >&2
	exit 1
fi

keychain_value() {
	security find-generic-password -s spool-release -a "$1" -w 2>/dev/null || true
}
SPOOL_GOOGLE_CLIENT_ID=${SPOOL_GOOGLE_CLIENT_ID:-$(keychain_value google-client-id)}
SPOOL_GOOGLE_CLIENT_SECRET=${SPOOL_GOOGLE_CLIENT_SECRET:-$(keychain_value google-client-secret)}
if [ -z "$SPOOL_GOOGLE_CLIENT_ID" ] || [ -z "$SPOOL_GOOGLE_CLIENT_SECRET" ]; then
	echo "package.sh: no Google OAuth client, so this build could not sign in to Gmail." >&2
	echo "Set SPOOL_GOOGLE_CLIENT_ID and SPOOL_GOOGLE_CLIENT_SECRET, or save them in the" >&2
	echo "keychain as described at the top of this script." >&2
	exit 1
fi
export SPOOL_GOOGLE_CLIENT_ID SPOOL_GOOGLE_CLIENT_SECRET

version=$(sed -n 's/^version = "\(.*\)"/\1/p' "$root/Cargo.toml" | head -1)
[ -n "$version" ] || { echo "package.sh: no version in Cargo.toml" >&2; exit 1; }

app="$root/target/Spool.app"
dmg="$root/target/Spool-$version.dmg"

SPOOL_UNIVERSAL=1 sh "$root/scripts/bundle.sh"

# Verify before packaging rather than after uploading. `codesign --verify`
# catches a bundle signed with the wrong identity; `spctl` is the closer
# stand-in for what the downloader's machine will actually do, and it is
# expected to fail here on the notarization check alone — that ticket does not
# exist yet — so its verdict is printed rather than acted on.
codesign --verify --deep --strict --verbose=2 "$app"
spctl --assess --type execute --verbose=4 "$app" 2>&1 || true

# A folder staged into the image rather than the .app directly: the symlink to
# /Applications is what makes the window a drag-and-drop install instead of an
# icon the user is expected to know what to do with.
staging=$(mktemp -d)
cp -R "$app" "$staging/Spool.app"
ln -s /Applications "$staging/Applications"

rm -f "$dmg"
hdiutil create -volname "Spool $version" -srcfolder "$staging" \
	-ov -format ULFO "$dmg"
rm -rf "$staging"

# The image is signed too. Notarization staples the ticket to the thing that
# was submitted, and an unsigned container cannot carry one.
codesign --force --timestamp --sign "$SPOOL_IDENTITY" "$dmg"

if [ "$notarize" = "0" ]; then
	echo "packaged $dmg (not notarized)"
	exit 0
fi

if [ -n "${NOTARY_PROFILE:-}" ]; then
	set -- --keychain-profile "$NOTARY_PROFILE"
elif [ -n "${APPLE_ID:-}" ] && [ -n "${APPLE_APP_PASSWORD:-}" ] && [ -n "${APPLE_TEAM_ID:-}" ]; then
	set -- --apple-id "$APPLE_ID" --password "$APPLE_APP_PASSWORD" \
		--team-id "$APPLE_TEAM_ID"
else
	echo "package.sh: no notarization credentials." >&2
	echo "Set NOTARY_PROFILE, or APPLE_ID + APPLE_APP_PASSWORD + APPLE_TEAM_ID," >&2
	echo "or pass --no-notarize." >&2
	exit 1
fi

# `--wait` blocks until Apple has an answer, which is usually a minute or two
# and occasionally much longer. Without it the staple below would run against
# a ticket that does not exist yet.
#
# Watched, and tried twice. A submission can stall before it has even
# uploaded — "initiating connection to the Apple notary service" and then
# nothing — and notarytool's own `--timeout` only covers the wait that comes
# after. Left alone that stall runs until the CI job is killed, which on a
# hosted macOS runner is ninety minutes billed for nothing. So a submission
# that has not finished in 25 minutes is abandoned and made once more.
NOTARY_LIMIT=1500
notarize() {
	xcrun notarytool submit "$dmg" "$@" --wait --timeout 20m &
	submit=$!
	waited=0
	while kill -0 "$submit" 2>/dev/null; do
		if [ "$waited" -ge "$NOTARY_LIMIT" ]; then
			echo "package.sh: no answer from the notary service in $((NOTARY_LIMIT / 60)) minutes" >&2
			kill "$submit" 2>/dev/null || true
			break
		fi
		sleep 10
		waited=$((waited + 10))
	done
	wait "$submit"
}

if ! notarize "$@"; then
	echo "package.sh: notarization did not finish; submitting once more" >&2
	notarize "$@" || { echo "package.sh: notarization failed twice" >&2; exit 1; }
fi

# Stapling is what lets the app open on a machine that is offline or behind a
# firewall: the ticket travels inside the image instead of being fetched.
xcrun stapler staple "$dmg"
xcrun stapler validate "$dmg"

echo "packaged $dmg"
