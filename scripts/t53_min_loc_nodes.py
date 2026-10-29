#!/usr/bin/env python3
"""T53 helper: read `dfs-admin file info --format json` on stdin and print the
smallest replica count across all of the file's ChunkLocations (0 if the output
can't be parsed or lists no chunks). The ChunkLocations below two nodes go to
stderr, so a failure names them."""
import json
import sys


def main():
    try:
        doc = json.load(sys.stdin)
        locs = doc.get("chunk_locations", [])
        for i, c in enumerate(locs):
            if len(c["nodes"]) < 2:
                print(f"under-replicated ChunkLocation #{i}: {json.dumps(c)[:400]}", file=sys.stderr)
        print(min((len(c["nodes"]) for c in locs), default=0))
    except Exception:
        print(0)


if __name__ == "__main__":
    main()
