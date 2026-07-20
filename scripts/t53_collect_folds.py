#!/usr/bin/env python3
"""T53 helper: report each chunk slot's SURVIVING folded generation.

Reads every node's server log and emits one line per chunk_idx:

    <chunk_idx> <new_chunk_id> <number_of_nodes_that_folded_it>

Only the latest fold per slot is reported. Earlier generations are folded
during the storm (by the client's own ForceFold, which reaches both replicas
and is healthy) and are then superseded and deleted by later patches, so
checking whether their bytes are still on disk would fail for reasons that
have nothing to do with replication.

A node count of 1 on the surviving generation is the bug's signature: that
node minted the identity alone, so it is the only holder of those bytes, and
the ChunkLocation it broadcasts names only itself.
"""
import glob
import os
import re
import sys

USAGE = "usage: t53_collect_folds.py <file_id> <log_dir>"

TS = re.compile(r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d+)Z")


def main():
    if len(sys.argv) != 3:
        sys.exit(USAGE)
    fid, logdir = sys.argv[1], sys.argv[2]
    fold = re.compile(
        r"Single fold: file %s chunk_idx (\d+) consolidated "
        r"\([0-9a-f]+ \+ delta -> ([0-9a-f]{64})" % re.escape(fid)
    )

    # chunk_idx -> new_chunk_id -> (latest_timestamp, {nodes})
    by_idx = {}
    for path in glob.glob(os.path.join(logdir, "server*.log")):
        node = os.path.basename(path)
        with open(path, errors="replace") as f:
            for line in f:
                m = fold.search(line)
                if not m:
                    continue
                ts_m = TS.search(line)
                if not ts_m:
                    continue
                idx, new, ts = int(m.group(1)), m.group(2), ts_m.group(1)
                gens = by_idx.setdefault(idx, {})
                prev_ts, nodes = gens.get(new, ("", set()))
                nodes.add(node)
                gens[new] = (max(prev_ts, ts), nodes)

    for idx in sorted(by_idx):
        # The surviving generation is the one folded last.
        new, (_, nodes) = max(by_idx[idx].items(), key=lambda kv: kv[1][0])
        print(f"{idx} {new} {len(nodes)}")


if __name__ == "__main__":
    main()
