# babooshka-tv

A semi-unattended video appliance running on a Raspberry Pi. When activated via a light
switch wired to a GPIO pin, it turns on a TV (via smart plug and/or HDMI CEC), waits for
it to boot, then plays movies from a folder in sequence until the switch is flipped off.
On deactivation it saves the current playback position and turns the TV off.

## Workspace layout

```
crates/
  babooshka/   — main binary: state machine, config loading, GPIO, TV control, HDMI detection
  player/      — library: MPV IPC client, playlist, watch-later state persistence
  playlistctl/ — workstation TUI: edit the playlist, measure loudness, generate gains
```

### crates/babooshka/src/

| File | Purpose |
|------|---------|
| `main.rs` | CLI (`--config`), TOML config parsing, build TV/HDMI objects, run state machine |
| `gpio.rs` | RPi GPIO via `rppal`: switch input (pull-up interrupt) + LED blink/set |
| `hdmi.rs` | `HdmiReady` trait + `DrmPoll` (sysfs polling) and `FixedTimeout` impls |
| `tv.rs` | `TvPower` trait + `SmartPlug`, `CecTv` (calls `cec-client`), `CombinedTv` impls |
| `smartplug.rs` | Home Assistant REST API calls (`/api/services/switch/turn_{on,off}`) |

### crates/player/src/

| File | Purpose |
|------|---------|
| `api.rs` | `MpvPlayer`: spawns mpv, drives it via JSON-RPC over a Unix socket |
| `playlist.rs` | Loads a JSON playlist file, finds the next item in sequence |
| `watch_later.rs` | Saves/restores `{ path, time }` to a JSON file for resume-on-restart |

## State machine (babooshka/src/main.rs)

```
IDLE ──(switch HIGH)──► BOOTING ──(HDMI ready)──► PLAYING ──(switch LOW)──► SHUTTING DOWN
  ▲                        │                          │                            │
  │                (switch LOW during boot)    (mpv dies / Ctrl-C)                │
  └────────────────────────┴──────────────────────────┴────────────────────────────┘
```

- **IDLE**: LED off, waiting for the GPIO switch to go HIGH (pull-up, switch open)
- **BOOTING**: turns on TV, blinks LED at 500 ms, polls for HDMI ready (or fixed timeout)
- **PLAYING**: LED solid on, mpv running, watch-later saved every second, playlist auto-advances
- **SHUTTING DOWN**: saves state, sends mpv `quit`, turns off TV, LED off → back to IDLE

## Configuration

Config file: `/etc/babooshka/config.toml` (override with `--config`).
See `config.example.toml` at the repo root for all options with comments.

Key settings:
- `play_state` / `playlist` — runtime data paths (default `/srv/`)
- `[gpio]` `switch_pin`, `led_pin` — BCM pin numbers (defaults 17, 27)
- `[tv]` `control` — `"smartplug"` | `"cec"` | `"both"` (both = smartplug first, CEC fallback)
- `[tv]` `ha_token` — Home Assistant long-lived access token (**keep out of version control**)
- `[hdmi]` `detect` — `"drm"` (polls `/sys/class/drm/.../status`) | `"timeout"` (fixed wait)

## playlistctl (workstation tool)

A ratatui TUI for curating the playlist and normalising volume. Runs on a
workstation; it never gets deployed to the Pi. Build with `cargo build -p playlistctl`.

### Hosts

There are two, and the split is forced by permissions, not preference:

| Host | Default | Role |
|------|---------|------|
| media (`--host`) | `danya@10.22.0.60` | qbittorrent VM. Holds the files; `find` and ffmpeg run here. |
| control (`--control-host`) | `danya@10.22.0.51` | The Pi. All JSON reads/writes go through it. |

`/srv` is exported over NFS to `10.22.0.0/24` only, and the export **squashes
NFS clients to the owning uid**. Consequences:

- A workstation on another subnet (e.g. over ZeroTier) cannot mount it, and a
  userspace NFS client does not help — `libnfs` hits the same export ACL.
- `danya` logged into the media host **cannot create files in `/srv`**
  (`drwxr-xr-x qbittorrent root`), so writes must go via a host that mounts it.
- Ergo: measure on the media host, write through the Pi.

ffmpeg and mpv are both built with `--enable-libssh`, so `sftp://` URLs work
without any mount — that is how segment preview plays audio locally.

**Why not run ffmpeg locally?** Measured: 190x realtime on the media host vs
7.5x over SFTP, with the local CPU idle. `-vn` skips video *decoding* but SFTP
still streams every byte across the VPN. The full library is ~16h of audio:
minutes on the VM, hours over the link.

### Loudness model

Gains are **dialogue-anchored**, not derived from a whole-file statistic. A
whole-file percentile latches onto whatever is loudest-and-common — dialogue in
one film, an action reel or score in another — so an operator picks a real
talking scene per film and every film is aligned to the same level.

- A segment's loudness is the **p85 of short-term (3s) loudness** inside it.
  Empirically stable within 1 dB across 6s/15s/60s windows, while the median
  swings by 8 dB depending on how much pause the window catches.
- The target is `DEFAULT_TARGET_LUFS = -20.4`, measured — not conventional. It
  is the p85 of a scene in *Иван Васильевич меняет профессию* near t=2652s that
  was calibrated by ear to a comfortable level at mpv 100% / wpctl 110%.
- `loudness.json` stores the full short-term timeline decimated to 1 Hz, so any
  percentile can be recomputed, and a time-varying gain remains possible, with
  no re-measurement.

### Files

| Path | Written by | Description |
|------|-----------|-------------|
| `/srv/loudness.json` | playlistctl | Timelines, integrated/LRA, chosen anchors, target |
| `/srv/gain_db.json` | playlistctl (`g`) | Flat `{path: gain_db}` map that babooshka reads |

### Adding new movies

The torrent client drops files into `/srv/babooshka-tv` on the media host, which
the Pi sees over NFS. From a checkout of this repo:

```sh
just measure       # 1. analyse new files (remote ffmpeg; skips what is done)
just edit          # 2. curate the playlist and pick a dialogue anchor each
just export-gains  # 3. write /srv/gain_db.json
```

`just add-movies` runs steps 1 and 2 back to back.

**Step 2 in detail** — this is the part that cannot be automated:

1. Tab 1: the new file appears in the middle *on disk* pane. `a` inserts it into
   the playlist after the cursor; `J`/`K` position it; `w` writes.
2. Tab 2: select it, `m` if not yet measured, then scrub with `←/→` and `H/L` to
   a stretch of ordinary dialogue — the timeline graph makes talking scenes easy
   to spot as sustained mid-height plateaus, versus the spikes of music or action.
3. `[` and `]` to bracket ~30–60 s of it, `p` to hear it locally through mpv, `s`
   to set it as the anchor. The gain appears immediately in the list.
4. `w` to save, `g` to export.

Then `just gain-on` once you are happy (it restarts babooshka — check with
`just playing` first). `just gain-status` shows whether gains are live.

Nothing is lost if a measurement run is interrupted: each completed file is
flushed to `loudness.json` as soon as it finishes.

**Superseded:** `~/add_new_movies.py` on the Pi (via that host's `just
add-new-movies`) appends new files to the playlist automatically. It uses a
non-recursive `os.listdir`, so it silently misses files in subdirectories such as
`12 стульев_1971-DVDRip-AVC/`, and it knows nothing about loudness. Prefer
`playlistctl`.

### Usage

```sh
cargo run -p playlistctl                  # the TUI
cargo run -p playlistctl -- --measure-all # headless: analyse everything, save, exit
cargo run -p playlistctl -- --export-only # regenerate gain_db.json and exit
```

Tab 1 (playlist): `h/l` pane, `j/k` move, `J/K` reorder, `a` add, `i` ignore,
`I` ignore every unplayable file, `U` un-ignore all, `d` remove (`D` overrides
the resume-point guard), `x` purge missing, `r` rescan, `w` write.

### The scan shows everything

The middle pane lists **every file** under the root, recursively, by path
relative to it — not just recognised video. Playable files are green, the rest
red. This is deliberate: filtering the scan by extension is what previously let
a DVD rip (`VOLSHEBNAYA_SILA/VIDEO_TS/*.VOB`) sit on disk completely invisibly.

Noise is removed by dismissing it, not by hiding it: `i` on an entry, or `I` for
every unplayable file at once. Dismissals persist in `playlist.json` under an
`ignored` key and never appear again (`U` clears them).

```json
{ "items": ["/srv/babooshka-tv/...mkv"], "ignored": ["/srv/babooshka-tv/...jpg"] }
```

`player::Playlist` must therefore **never** gain `#[serde(deny_unknown_fields)]`
— babooshka has to keep loading a playlist carrying that key. There is a test in
`crates/player/src/playlist.rs` guarding exactly this.

`.vob` is intentionally absent from `VIDEO_EXTS`: a DVD rip splits one feature
across several `VTS_*` fragments, so listing them individually would put chunks
of a film into the rotation rather than the film. Remux such rips to a single
container (or re-download them) rather than teaching the tools about `dvd://`.

Tab 2 (loudness): `m`/`M` measure, `←/→` ±5s, `H/L` ±60s, `A` propose an anchor,
`[` `]` mark a segment, `p` play/pause the preview, `s` set it as the anchor,
`t` adopt the selected file's anchor as the global target, `g` export gains,
`w` save.

**The preview window.** `p` starts one mpv on first use and keeps it alive; it
opens the file over `sftp://` at the cursor. Its playback position and the TUI
cursor are the same value from two ends — `←/→` and `H/L` seek the window, and
scrubbing in the window walks the cursor along the loudness timeline. Marking a
segment with `[` and `]` mirrors it into mpv's A-B loop, so `p` repeats exactly
the stretch under consideration. mpv runs at `--volume=100`, matching the
conditions the target was calibrated under, so what you hear is what the Pi
plays at the same system volume.

Note that mpv's own waveform/spectrum overlays (`--lavfi-complex` with
`showwaves`) were considered and rejected: they show instantaneous amplitude
over a rolling few seconds, whereas anchoring needs whole-film short-term LUFS
against the target line. The TUI keeps the chart; mpv is only the video surface.

**`A` — propose an anchor.** Searches the stored timeline for the 45 s window
that best resembles the calibrated reference: p85 at the 92nd percentile of the
film, low internal spread, no room tone hanging off the edge of a scene, and
not in the first or last 4% (opening titles and end credits are sustained music
and score well otherwise). It only *marks* the segment — the timeline cannot
tell speech from a held note, so confirm by ear with `p` before `s`.

Once a segment is marked, the readout shows its **percentile rank within its own
film** against the reference's 92%. An anchor much below that is on a quiet
scene and will make the film play too loud; this is the check that was missing
when the first batch of anchors came out 1.5–4.5 dB hot.

Removing or reordering the entry `play-state.json` points at is guarded, because
`Playlist::next_file` silently falls back to `items[0]` when the saved path is no
longer in the list — which restarts the rotation.

## Hardware

- **Target**: Raspberry Pi (aarch64)
- **Switch**: light switch wired between GPIO pin and GND; RPi pull-up enabled
  - Pin HIGH (switch open) = active/playing
  - Pin LOW (switch to ground) = inactive/off
- **LED**: wired to a GPIO output pin (with appropriate resistor)
- **TV power**: smart plug via Home Assistant REST API, and/or HDMI CEC via `cec-client`
- **HDMI**: TV presence detected via `/sys/class/drm/card*-HDMI-A-1/status`
  - Find the right path with: `cat /sys/class/drm/*/status`

## MPV IPC

mpv is spawned with `--input-ipc-server=/tmp/run/mpv-ipc.sock`. The `MpvPlayer` type in
`player/src/api.rs` communicates with it using mpv's JSON-RPC protocol (newline-delimited
JSON over a Unix socket). Commands are tracked by `request_id`; events are dispatched to
registered one-shot predicate subscribers.

## Justfiles

There are two, and they are not the same file:

- **`./Justfile`** (this repo, runs on your workstation) — building, deploying,
  and the add-movies workflow above. `just --list` to see them.
- **`~/Justfile` on the Pi** — runtime control: `switch on|off`, `skip`,
  `rewind`, `volume-*`, `logs`, `config-app`.

## Build & deploy

### One-time setup (Arch Linux)

```sh
# Cross-compiler toolchain
sudo pacman -S aarch64-linux-gnu-gcc aarch64-linux-gnu-glibc

# Rust target (if not already installed)
rustup target add aarch64-unknown-linux-gnu
```

The linker is pre-configured in `.cargo/config.toml` — no extra env vars needed.

### Building

```sh
cargo build --release --target aarch64-unknown-linux-gnu
```

Or use the deploy script to build and rsync to the Pi in one step:

```sh
./deploy.sh   # builds release, rsyncs binaries to danya@10.0.0.95:/opt/
```

### Why these packages

`ring` (the TLS crypto backend used by `reqwest`) has a small C/asm component that
requires the C cross-compiler. `aws-lc-rs` (the other common backend) was explicitly
disabled via `default-features = false` on both `reqwest` and `rustls` to avoid its
much heavier build requirements.

## Runtime files

| Path | Description |
|------|-------------|
| `/etc/babooshka/config.toml` | Main config |
| `/srv/playlist.json` | Ordered list of video file paths: `{"items": ["/path/to/file.mp4", ...]}` |
| `/srv/play-state.json` | Current playback position (written every second while playing) |
| `/tmp/run/mpv-ipc.sock` | mpv IPC socket (created by mpv on startup) |
