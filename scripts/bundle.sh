#!/bin/sh
# Assembles Spool.app.
#
# The bundle exists for one reason above all the cosmetic ones: macOS refuses
# several APIs to a process with no bundle identifier, and refuses them by
# raising an Objective-C exception — which in this app is an abort, not an
# error. Notifications, login items and registering as the handler for mailto:
# all need the identifier in Info.plist to exist.
#
# A shell script rather than cargo-bundle: the layout is four files, and a
# build dependency that has to be installed before the app can be built is a
# worse trade than twenty lines that are obvious.
#
#   scripts/bundle.sh          builds release and assembles target/Spool.app
#   scripts/bundle.sh --open   and launches it
#
# Two environment variables change what comes out, and `scripts/package.sh`
# sets both. Everything else is the same build a developer gets:
#
#   SPOOL_IDENTITY   the codesigning identity. Defaults to `-`, ad-hoc, which
#                    is enough for a local build; a release needs the real
#                    Developer ID, because an ad-hoc signature does not
#                    survive being downloaded.
#   SPOOL_UNIVERSAL  set to 1 to build both architectures and `lipo` them
#                    together. Off locally — it doubles the build for a slice
#                    the machine doing the building cannot run.
set -eu

root=$(cd "$(dirname "$0")/.." && pwd)
app="$root/target/Spool.app"
contents="$app/Contents"
identity=${SPOOL_IDENTITY:--}

if [ "${SPOOL_UNIVERSAL:-0}" = "1" ]; then
	# Checked up front, because the error for getting this wrong is
	# misleading. A Homebrew-installed cargo ships only the host target and
	# comes earlier on PATH than rustup's shim, so `rustup target list` says
	# x86_64 is installed while the cargo actually running cannot see it —
	# and rustc's advice is to add the target that is already added.
	for target in aarch64-apple-darwin x86_64-apple-darwin; do
		if [ ! -d "$(rustc --print sysroot)/lib/rustlib/$target" ]; then
			echo "bundle.sh: $target is missing from $(command -v cargo)." >&2
			echo "A universal build needs a rustup toolchain with both:" >&2
			echo "  rustup target add $target" >&2
			echo "  PATH=\"\$HOME/.cargo/bin:\$PATH\" scripts/bundle.sh" >&2
			exit 1
		fi
	done

	for target in aarch64-apple-darwin x86_64-apple-darwin; do
		cargo build --release --target "$target" --manifest-path "$root/Cargo.toml"
	done
	binary=$(mktemp -d)/spool
	lipo -create -output "$binary" \
		"$root/target/aarch64-apple-darwin/release/spool" \
		"$root/target/x86_64-apple-darwin/release/spool"
else
	cargo build --release --manifest-path "$root/Cargo.toml"
	binary="$root/target/release/spool"
fi

rm -rf "$app"
mkdir -p "$contents/MacOS" "$contents/Resources"

cp "$binary" "$contents/MacOS/spool"
cp "$root/crates/app/bundle/Info.plist" "$contents/Info.plist"

# `iconutil` wants a directory of named sizes, so the .icns is built here
# rather than committed: it is derived from the script that draws it, and a
# binary in the repo that nobody can edit is a binary that never changes.
iconset=$(mktemp -d)/Spool.iconset
mkdir -p "$iconset"
python3 "$root/scripts/make_icon.py" "$iconset/../icon.png"

for size in 16 32 128 256 512; do
	sips -z $size $size "$iconset/../icon.png" \
		--out "$iconset/icon_${size}x${size}.png" >/dev/null
	double=$((size * 2))
	sips -z $double $double "$iconset/../icon.png" \
		--out "$iconset/icon_${size}x${size}@2x.png" >/dev/null
done

iconutil -c icns "$iconset" -o "$contents/Resources/Spool.icns"
rm -rf "$(dirname "$iconset")"

# Ad-hoc by default, so the app has a stable identity to the system without
# needing a developer certificate. It is what stops the keychain asking again
# after every rebuild, and what makes a notification prompt say "Spool" rather
# than naming a path in target/.
#
# `--options runtime` is on even for the ad-hoc signature. The hardened runtime
# is required for notarization, and turning it on only in release would mean
# discovering what it forbids on the tag rather than on the desk.
#
# The entitlements are what let extensions run at all: a WebAssembly component
# is compiled to arm64 at load, and the hardened runtime kills a process that
# jumps into code nobody signed. See scripts/spool.entitlements.
entitlements="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/spool.entitlements"

codesign --force --timestamp --options runtime \
	--entitlements "$entitlements" \
	--sign "$identity" --identifier com.joedesigns.spool "$app"

# Checked rather than assumed, because the failure it prevents does not look
# like a signing failure. An app signed without this builds, launches, shows a
# window, and is killed by the kernel the moment somebody's extension loads —
# which is a crash report about WebAssembly rather than about codesign, and a
# long way from here.
if ! codesign -d --entitlements - --xml "$app" 2>/dev/null \
	| grep -q "allow-unsigned-executable-memory"; then
	echo "the signature did not take the entitlements; extensions would kill the app" >&2
	exit 1
fi

echo "built $app"

if [ "${1:-}" = "--open" ]; then
	open "$app"
fi
