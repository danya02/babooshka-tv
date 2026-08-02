#!/usr/bin/env bash
set -euo pipefail

HOST="danya@10.22.0.51"
DEPLOY_PATH="/opt/"

echo "Building..."
# Only babooshka ships to the Pi. playlistctl is a workstation tool that talks
# to the media host over SSH, so cross-compiling it here would be wasted work.
cargo build --release --target aarch64-unknown-linux-gnu -p babooshka

echo "Finding executables..."
# Named explicitly rather than globbed, so a stale cross-built playlistctl left
# in the target directory can never be picked up and shipped.
BINARIES=babooshka

if [ ! -f "target/aarch64-unknown-linux-gnu/release/$BINARIES" ]; then
    echo "No executables found!"
    exit 1
fi

echo "Deploying: $BINARIES"
for bin in $BINARIES; do
    rsync -avz --progress "target/aarch64-unknown-linux-gnu/release/$bin" "$HOST:$DEPLOY_PATH"
done

echo "Deploying service unit..."
rsync -avz --progress babooshka.service "$HOST:.config/systemd/user/babooshka.service"

echo "Restarting services..."
ssh "$HOST" "systemctl --user daemon-reload && systemctl --user restart babooshka.service"
echo "Deployment complete, logs follow"

ssh "$HOST" "sudo journalctl -f"