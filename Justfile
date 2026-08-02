# Workstation recipes for babooshka-tv.
#
# This Justfile runs on your machine, not the Pi. The Pi has its own ~/Justfile
# for runtime control (switch, volume, logs, skip/rewind).
#
# Hosts:
#   media   10.22.0.60  qbittorrent VM — holds the files, runs ffmpeg
#   control 10.22.0.51  the Pi — the only host that can write /srv (see CLAUDE.md)

media := "danya@10.22.0.60"
control := "danya@10.22.0.51"

default:
    just --list

# ---------------------------------------------------------------------------
# Adding new movies — the usual workflow
# ---------------------------------------------------------------------------

# 1. Analyse any newly downloaded file. Safe to re-run: skips what is measured.
measure:
    cargo run --release -p playlistctl -- --measure-all

# 2. Curate the playlist and pick a dialogue anchor per new film.
edit:
    cargo run --release -p playlistctl

# 3. Regenerate /srv/gain_db.json from the anchors chosen in the TUI.
export-gains:
    cargo run --release -p playlistctl -- --export-only

# Everything a new download needs, up to the part only you can do by ear.
add-movies: measure edit

# ---------------------------------------------------------------------------
# Volume normalisation switch
# ---------------------------------------------------------------------------

# Show whether gains are live, and what babooshka last decided to apply.
gain-status:
    @ssh {{control}} 'grep -E "^gain_(db|enabled)" /srv/babooshka-config.toml'
    @ssh {{control}} 'journalctl --user -u babooshka.service -n 40 --no-pager -o cat | grep -i "normalisation" || echo "(no normalisation lines in the last 40)"'

# Turn gains on/off. Both restart babooshka — check nothing is playing first.
gain-on: (_set-gain "true")
gain-off: (_set-gain "false")

_set-gain value:
    ssh {{control}} 'sed -i "s/^gain_enabled = .*/gain_enabled = {{value}}/" /srv/babooshka-config.toml && grep "^gain_enabled" /srv/babooshka-config.toml'
    just restart

# ---------------------------------------------------------------------------
# Build & deploy
# ---------------------------------------------------------------------------

build:
    cargo build --release --target aarch64-unknown-linux-gnu -p babooshka

test:
    cargo test --workspace

check:
    cargo clippy --workspace --all-targets

# Cross-compile babooshka, rsync to the Pi, restart, then tail the log.
deploy:
    ./deploy.sh

restart:
    ssh {{control}} 'systemctl --user restart babooshka.service && sleep 2 && systemctl --user is-active babooshka.service'

logs:
    ssh {{control}} 'journalctl --user -u babooshka.service -f --no-pager'

# Refuse to restart while a film is on screen.
playing:
    @ssh {{control}} 'pgrep -a mpv >/dev/null && echo "PLAYING — do not restart" || echo "idle — safe to restart"'
