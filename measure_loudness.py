#!/usr/bin/env python3
"""
Measure EBU R128 integrated loudness for all files in a playlist and write
a gain map to a JSON sidecar file.

Usage:
    python3 measure_loudness.py [--playlist /srv/playlist.json] \
                                [--output /srv/gain_db.json] \
                                [--target -18]

The output file maps absolute file paths to a float gain in dB that babooshka
will apply via mpv's `volume-gain` property when loading each file.

Requirements: ffmpeg must be on PATH.
Run on a fast machine (not the Pi) — loudnorm analysis is CPU-intensive.
"""

import argparse
import json
import re
import subprocess
import sys
from pathlib import Path


TARGET_LUFS = -18.0  # EBU R128 target; -23 is broadcast standard, -18 suits films


def measure_lufs(path: str) -> float | None:
    """Return integrated loudness in LUFS, or None on failure."""
    proc = subprocess.Popen(
        [
            "ffmpeg", "-hide_banner",
            "-i", path,
            "-af", "loudnorm=print_format=json",
            "-f", "null", "-",
        ],
        stderr=subprocess.PIPE,
        stdout=subprocess.DEVNULL,
        text=True,
    )
    # Stream stderr line by line so the user sees ffmpeg's progress output.
    # Collect all lines so we can parse the loudnorm JSON block at the end.
    stderr_lines: list[str] = []
    assert proc.stderr is not None
    for line in proc.stderr:
        line_stripped = line.rstrip()
        stderr_lines.append(line_stripped)
        # Print progress lines (time= ...) in-place; skip JSON/blank lines.
        if "time=" in line_stripped:
            print(f"\r  {line_stripped}", end="", flush=True)
    proc.wait()
    if any("time=" in l for l in stderr_lines):
        print()  # newline after the last progress line

    # ffmpeg prints loudnorm JSON to stderr
    output = "\n".join(stderr_lines)
    # Find the JSON block printed by loudnorm
    match = re.search(r'\{[^{}]*"input_i"\s*:', output, re.DOTALL)
    if not match:
        print(f"  WARNING: could not parse loudnorm output for {path}", file=sys.stderr)
        return None
    try:
        data = json.loads(match.group(0) + "}")
        return float(data["input_i"])
    except Exception as e:
        print(f"  WARNING: JSON parse error for {path}: {e}", file=sys.stderr)
        return None


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--playlist", default="/srv/playlist.json",
                        help="Path to playlist.json (default: /srv/playlist.json)")
    parser.add_argument("--output", default=None,
                        help="Output gain_db.json path (default: gain_db.json next to playlist)")
    parser.add_argument("--target", type=float, default=TARGET_LUFS,
                        help=f"Target loudness in LUFS (default: {TARGET_LUFS})")
    parser.add_argument("--skip-existing", action="store_true",
                        help="Skip files already present in the output file")
    args = parser.parse_args()

    playlist_path = Path(args.playlist)
    output_path = Path(args.output) if args.output else playlist_path.parent / "gain_db.json"

    with open(playlist_path) as f:
        playlist = json.load(f)
    items = playlist["items"]

    # Load existing results if skipping
    existing: dict[str, float] = {}
    if args.skip_existing and output_path.exists():
        with open(output_path) as f:
            existing = json.load(f)
        print(f"Loaded {len(existing)} existing entries from {output_path}")

    results: dict[str, float] = dict(existing)
    to_measure = [p for p in items if not args.skip_existing or p not in existing]

    print(f"Measuring {len(to_measure)} files (target {args.target} LUFS)...")

    for i, path in enumerate(to_measure, 1):
        print(f"[{i}/{len(to_measure)}] {Path(path).name}")
        lufs = measure_lufs(path)
        if lufs is None:
            print(f"  Skipping (measurement failed)")
            continue
        gain = args.target - lufs
        print(f"  {lufs:.1f} LUFS  →  gain {gain:+.1f} dB")
        results[path] = round(gain, 2)

        # Write incrementally so progress is not lost if interrupted
        with open(output_path, "w") as f:
            json.dump(results, f, ensure_ascii=False, indent=2)

    print(f"\nDone. Wrote {len(results)} entries to {output_path}")


if __name__ == "__main__":
    main()
