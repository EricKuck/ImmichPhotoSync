# immich-photo-sync

Headless macOS daemon, written in Rust, that keeps an [Immich](https://immich.app) server in sync with the
Apple Photos library (including iCloud Photos). It is a command line reimplementation of the parts of
[ImmiBridge](https://github.com/emerysilb/immibridge) needed for unattended use:

- source: the Photos library, both **originals** and **edited renditions**, images and videos, Live Photos
  (paired video is linked so Immich shows them as Live Photos)
- destination: Immich only (no folder export)
- **albums**: every Photos album becomes an Immich album with the same name; renames follow, duplicates are disambiguated
- **favorites** are pushed to Immich and kept in sync
- **smart incremental** only: a local SQLite state database knows what has been uploaded, so a pass over an
  already-synced library costs a scan and nothing else. Nothing is ever deleted from Immich.
- runs **live**: PhotoKit change notifications trigger an incremental pass seconds after a photo arrives,
  and a full reconciliation pass runs every hour as a safety net (both configurable)
- no UI, no menu bar, no notifications; every setting is a command line flag or environment variable,
  so it can run as a launchd agent

## Build

Requires Rust 1.85+ and Xcode (or the Command Line Tools) for the PhotoKit framework headers at link time.

```sh
cargo build --release
```

The binary embeds an `Info.plist` section (bundle id `com.immich-photo-sync.cli`, Photos usage
description) so macOS can show the Photos permission prompt for a bare executable.

## Run

```sh
# one full pass, then exit
immich-photo-sync --server https://photos.example.com --api-key-file ~/.config/immich-photo-sync/api-key once

# daemon: full pass, then react to library changes + hourly full pass
immich-photo-sync --server https://photos.example.com --api-key-file ~/.config/immich-photo-sync/api-key run

# see what a pass would do without touching anything
immich-photo-sync --server ... --api-key ... --dry-run once

# counts and recent failures from the state database
immich-photo-sync status
```

The server URL and API key can also come from `IMMICH_SERVER`, `IMMICH_API_KEY` or `IMMICH_API_KEY_FILE`.
Generate the key in Immich under User Settings → API Keys. `--help` lists every flag; the important ones:

| Flag | Default | Meaning |
|------|---------|---------|
| `--interval 1h` | 1h | full reconciliation pass cadence (`0` disables) |
| `--no-watch` | watch on | ignore live change notifications, rely on the interval |
| `--debounce 30s` | 30s | quiet period after a change before the incremental pass |
| `--media all\|images\|videos` | all | media filter |
| `--no-originals`, `--no-edited` | both on | which renditions to upload |
| `--no-albums`, `--no-favorites` | both on | skip album mirroring / favorite sync |
| `--include-hidden` | off | include hidden photos |
| `--include-shared-albums` | off | mirror iCloud Shared Albums too (their contents get uploaded) |
| `--replace-stale-edits` | off | when an edit changes, trash the previously uploaded edited rendition |
| `--upload-concurrency 3` | 3 | parallel uploads |
| `--export-idle-timeout 2m` | 2m | give up on an iCloud download when no progress is reported for this long |
| `--http-timeout 30m` | 30m | per request timeout; raise it for huge videos on slow uplinks |
| `--limit N`, `--dry-run`, `--oldest-first` | | testing aids |
| `--state-dir`, `--temp-dir`, `--device-id` | `~/Library/Application Support/immich-photo-sync` | storage locations and the device id reported to Immich |

## Install as a launchd agent

```sh
./install.sh --server https://photos.example.com --api-key-file ~/.config/immich-photo-sync/api-key
```

This builds the release binary, ad-hoc signs it (so the privacy permission survives rebuilds), installs it to
`~/.local/bin`, writes `~/Library/LaunchAgents/com.immich-photo-sync.agent.plist` and bootstraps it.
Logs go to `~/Library/Logs/immich-photo-sync/`. The plist can also be produced by hand with
`immich-photo-sync <flags> launchd-plist`.

It must be a **LaunchAgent** (user session), not a LaunchDaemon: PhotoKit needs the user's Photos library
and privacy context.

## Nix

The repository is a flake exposing `packages.<system>.default` and `overlays.default`. With nix-darwin,
add it as an input and define the agent with `launchd.user.agents`:

```nix
inputs.immich-photo-sync = {
  url = "github:EricKuck/ImmichPhotoSync";
  inputs.nixpkgs.follows = "nixpkgs";
};

# in the darwin configuration
nixpkgs.overlays = [ inputs.immich-photo-sync.overlays.default ];
launchd.user.agents.immich-photo-sync.serviceConfig = {
  ProgramArguments = [
    (lib.getExe pkgs.immich-photo-sync)
    "--server" "https://photos.example.com"
    "--api-key-file" config.sops.secrets.immich-api-key.path
    "run"
  ];
  RunAtLoad = true;
  KeepAlive = true;
};
```

Nix ad-hoc signs the binary, and the signature changes with every rebuild of the package. macOS ties the
Photos permission to that signature, so expect a new Photos prompt after each update of the package.

### Photos permission

macOS asks for Photos access the first time a process actually touches the library, and it attributes the
request to the *responsible process*. Started from a terminal, that is the terminal app, not this binary.
Started by launchd, it is the binary itself. So the first launch of the agent is the one that produces the
prompt; if it is dismissed, grant access later in **System Settings → Privacy & Security → Photos**.
`immich-photo-sync auth` reports the current status and requests access for the current context.

## How a pass works

1. Enumerate the library (newest first). For each asset, compare modification date, edit state and
   favorite flag with the state database. Unchanged assets cost one SQLite lookup and nothing else.
2. For new or changed assets, inspect the PhotoKit resources and plan the files to upload:
   - `original`: the original photo or video resource (`IMG_1234.HEIC`)
   - `pairedVideo`: the Live Photo movie, uploaded first and linked to the still via `livePhotoVideoId`
   - `edited`: the full size rendered edit (`IMG_1234_edited.jpg`), falling back to a PhotoKit render if
     Photos has no rendered resource; keyed on the asset modification date so re-edits are picked up
   - `editedPairedVideo`: the edited Live Photo movie, when Photos has one
3. Export each file to the temp directory (PhotoKit downloads from iCloud as needed), SHA-1 it, ask Immich
   whether that checksum already exists (`bulk-upload-check`), and upload only if it does not. Assets that the
   Immich mobile app already uploaded from the phone are therefore adopted, not duplicated.
4. Record the Immich asset id per file. Push the favorite flag for files that were not uploaded by this pass.
5. Mirror albums: for every Photos album, map it to an Immich album (existing one with the same name is
   adopted, otherwise created), and add every uploaded rendition of its assets that is not yet a member.

Failures are recorded per asset and retried with exponential backoff (10 min doubling up to 24 h).

## Live updates

The daemon registers a `PHPhotoLibraryChangeObserver`. Inserted and changed assets are collected, debounced,
and processed in a small incremental pass; album changes trigger an album pass. Anything missed is caught by
the periodic full pass.

## What it does not do

- delete or archive anything in Immich (also not when a photo leaves an album or is deleted in Photos)
- sync keywords, descriptions, locations beyond what the files carry in EXIF
- Immich v2 specific endpoints (`/assets/exist`, device id search); duplicate detection is checksum based
  and works on both v2 and v3 servers
- run two instances against the same state directory at the same time

## Development

```sh
cargo test       # plan logic, state store, Immich client against a mock server
cargo clippy
```

The PhotoKit layer (`src/photos.rs`) can only be exercised with a real library and Photos permission;
`--dry-run` and `--limit` exist to make the first real runs cheap.
