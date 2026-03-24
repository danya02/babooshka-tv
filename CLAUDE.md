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
