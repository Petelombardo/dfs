#!/usr/bin/env python3
"""T53 helper: read `dfs-admin file info --format json` on stdin and print the
smallest replica count across all of the file's ChunkLocations (0 if the output
can't be parsed or lists no chunks)."""
import json
import sys


def main():
    try:
        doc = json.load(sys.stdin)
        locs = doc.get("chunk_locations", [])
        print(min((len(c["nodes"]) for c in locs), default=0))
    except Exception:
        print(0)


if __name__ == "__main__":
    main()
