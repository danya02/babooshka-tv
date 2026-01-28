#!/usr/bin/env bash
set -euo pipefail

HOST="danya@10.22.0.55"
DEPLOY_PATH="/opt/"

echo "Building..."
cargo build --release

echo "Finding executables..."
BINARIES=$(find target/release -maxdepth 1 -type f -executable -printf '%f\n')

if [ -z "$BINARIES" ]; then
    echo "No executables found!"
    exit 1
fi

echo "Deploying: $BINARIES"
for bin in $BINARIES; do
    rsync -avz --progress "target/release/$bin" "$HOST:$DEPLOY_PATH"
done

echo "Deployment complete!"