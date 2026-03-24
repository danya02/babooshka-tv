#!/usr/bin/env bash
set -euo pipefail

HOST="danya@10.22.0.51"
DEPLOY_PATH="/opt/"

echo "Building..."
cargo build --release --target aarch64-unknown-linux-gnu

echo "Finding executables..."
BINARIES=$(find target/aarch64-unknown-linux-gnu/release -maxdepth 1 -type f -executable -printf '%f\n')

if [ -z "$BINARIES" ]; then
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