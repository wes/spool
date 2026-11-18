# Releasing

Spool is developed and released in this repository: each release here
carries the signed, notarized disk image, and installed copies check this
repository's latest release for updates.

One script cuts the release on a Mac and publishes it. Cutting an official
release needs the signing certificate and the credentials below, none of which
are in this repository; building Spool for yourself needs none of them (see the
README).

## Cutting a release

```sh
scripts/release.sh
```

It asks whether this is a patch, minor or major release, shows the notes and
what it is about to do, and on a yes:

1. sets the version in `Cargo.toml`, `Cargo.lock` and
   `crates/app/bundle/Info.plist`;
2. builds both architectures, `lipo`s them into one binary, signs the bundle
   and the disk image, sends it to Apple for notarization and staples the
   ticket (`scripts/package.sh`, about 10–15 minutes);
3. commits, tags `v<version>` and pushes both;
4. creates the release on `wes/spool` with the disk image and, as
   its notes, the `CHANGELOG.md` section for that version if there is one, or
   else the commit subjects since the last tag — the moment installed copies are offered
   it.

The disk image is uploaded twice: as `Spool-<version>.dmg`, and as
`Spool.dmg`, a name that never changes. GitHub serves the newest release's
assets at a fixed address, so this link is always the latest build:

```
https://github.com/wes/spool/releases/latest/download/Spool.dmg
```

It builds before it commits, so a failed build or notarization leaves the
version where it was and nothing pushed; fix it and run it again. It refuses to
run off `master`, out of step with `origin`, or with anything but the changelog
uncommitted. A version that was tagged but never published — a publish that
failed half-way — is offered again, to build and publish, while the app has not
changed since the tag.

## What the Mac needs, once

- **The Developer ID certificate** — "Developer ID Application: Wes Edling
  (288BJX6YHP)" — in the login keychain. The team is checked: installed copies
  refuse an update signed by any other.
- **Notarization credentials**, saved in the keychain. Interactive, and asks
  for an app-specific password from
  [appleid.apple.com](https://appleid.apple.com/account/manage):

  ```sh
  xcrun notarytool store-credentials spool \
      --apple-id <your-apple-id> --team-id 288BJX6YHP
  ```

- **The Google OAuth client**, saved in the keychain. The source leaves it
  empty (`crates/net/src/oauth.rs`) and `scripts/package.sh` compiles it in,
  refusing to build a release without it — one that shipped without it could
  not sign in to Gmail. The ID and secret are on the Desktop app client in the
  [Google Cloud console](https://console.cloud.google.com/apis/credentials):

  ```sh
  security add-generic-password -U -s spool-release -a google-client-id -w '<client id>'
  security add-generic-password -U -s spool-release -a google-client-secret -w '<client secret>'
  ```

  `SPOOL_GOOGLE_CLIENT_ID` and `SPOOL_GOOGLE_CLIENT_SECRET` in the
  environment take precedence over the keychain.

- **Xcode**, for `metal`, `codesign`, `notarytool`, `stapler` and `hdiutil`;
  **rustup**, which fetches the toolchain `rust-toolchain.toml` pins; and
  **gh**, logged in as someone who can write to `wes/spool`.

`SPOOL_IDENTITY` and `NOTARY_PROFILE` override the certificate and the profile.

`scripts/package.sh --no-notarize` stops after signing, which is the fast way
to check that a change to the bundle still passes `codesign --verify`.

For everyday work, `scripts/bundle.sh` is unchanged: an ad-hoc signature, the
native architecture only, no Apple involved.

## Releasing from GitHub instead

`.github/workflows/release.yml` does the same build on GitHub's macOS machines,
for when no Mac with the certificate is to hand. It only runs when started by
hand, from the Actions tab: with **publish** off it uploads the disk image as a
workflow artifact; with it on it publishes the release here, using the
workflow's own token. Nothing starts it automatically: a release is meant to be
a deliberate act rather than a side effect of a push.

## Entitlements

`scripts/spool.entitlements` asks the hardened runtime for one thing:
`com.apple.security.cs.allow-unsigned-executable-memory`.

Extensions are why. A component is WebAssembly, and running one means compiling
it to arm64 at load and jumping into the result — which is exactly what the
hardened runtime exists to stop. Without the entitlement the kernel kills the
process the moment an extension loads, and it does not look like a signing
problem when it happens:

```
Exception Type:      EXC_BAD_ACCESS (SIGKILL (Code Signature Invalid))
Termination Reason:  Namespace CODESIGNING, Code 2, Invalid Page
```

It is `allow-unsigned-executable-memory` rather than the narrower `allow-jit`
because `allow-jit` covers only regions mapped with `MAP_JIT`, and wasmtime's
unix backend uses an ordinary `mmap` made executable with `mprotect`. Apple
accepts the entitlement through notarization; it is a documented exception, not
a workaround.

The cost is real and worth naming: it applies to the whole process, not to the
extension host, and it is the one protection the hardened runtime gives up. The
alternative is wasmtime's Pulley interpreter, which runs WebAssembly without
generating native code — no entitlement, much slower execution.

`scripts/bundle.sh` checks that the signature actually carried the entitlement
and fails if it did not, because an app signed without it builds cleanly,
launches, draws a window, and dies later on somebody else's machine.

## The secrets the GitHub workflow needs

Only for releasing from GitHub; `scripts/release.sh` uses the keychain.

Set these under Settings → Secrets and variables → Actions in **this**
repository.

| Secret | What it is |
|---|---|
| `MACOS_CERTIFICATE_P12` | The Developer ID Application certificate and its private key, exported from Keychain Access as a `.p12`, then `base64 -i cert.p12 \| pbcopy`. |
| `MACOS_CERTIFICATE_PASSWORD` | The password set when exporting that `.p12`. |
| `SPOOL_IDENTITY` | `Developer ID Application: Wes Edling (288BJX6YHP)` — the name `codesign` matches against, exactly as `security find-identity -v -p codesigning` prints it. |
| `APPLE_ID` | The Apple ID the Developer ID belongs to. |
| `APPLE_APP_PASSWORD` | An app-specific password for that Apple ID. Not the account password; notarization refuses it. |
| `APPLE_TEAM_ID` | `288BJX6YHP`. |
| `SPOOL_GOOGLE_CLIENT_ID` | The Google OAuth Desktop app client ID, compiled into the build. |
| `SPOOL_GOOGLE_CLIENT_SECRET` | That client's secret. |

Publishing needs no secret of its own: the release is created in this
repository, which the workflow's `GITHUB_TOKEN` can write to.

The certificate expires; Developer ID certificates last five years and the
workflow will start failing on the day it does, with a signing error that does
not say so. Re-export and replace the two `MACOS_CERTIFICATE_*` secrets.

## How an installed copy updates itself

Spool checks this repository for a newer release on launch, and from
**Spool → Check for Updates…** on demand. What it finds appears at the foot of
the sidebar; nothing is downloaded, and nothing is replaced, until it is
pressed.

That means a release is not only a download link — it is what every installed
copy will fetch and run. Three things the release already does are what make
that safe, and an update refuses to install if any of them is missing:

- the bundle's signature is intact (`codesign --verify --deep --strict`),
- its leaf certificate is team `288BJX6YHP` — a valid Developer ID only proves
  that *somebody* signed it,
- and Apple has notarized it (`spctl --assess`).

The version comes from the tag, compared against `CARGO_PKG_VERSION`. This is
the third place the version number has to be right, after `Cargo.toml` and
`Info.plist`: a release tagged lower than the version it contains is a release
nobody is offered.

Drafts and prereleases are ignored, so the manual workflow run described above
stays invisible to installed copies.

A copy that cannot replace itself — a `cargo run` build, or an app installed
somewhere the user cannot write — says so and offers the release page instead.

## The old releases repository

Up to 0.1.15, releases went to
[`wes/spool-releases`](https://github.com/wes/spool-releases), a public repo
that held only disk images while this one was private. Every copy of those
versions still checks there for updates, and nothing can change that now.

So it stays, archived rather than deleted, with **0.1.16 as its last
release**: the same disk image as 0.1.16 here. An old copy finds it there,
updates, and from then on checks this repository instead. Deleting the
repository, or that release, would leave every older copy unable to update
again. Nothing new is ever published there.
