#!/usr/bin/env python3
"""seed_wiped.py <stick image> <seed>: fail if any 64-byte run of <seed> is
still anywhere in <stick image>.

seeded-pair.sh runs it on node 1's RBSEED stick once the node is up. A
create seed carries the cluster's master key, so prepare zeroes it before
removing it. FAT's delete only marks the directory entry and frees the
clusters, so a seed that was unlinked but not zeroed is still readable in
the image; scanning the raw bytes catches that, and a name check wouldn't.
"""

import sys
from pathlib import Path

WINDOW = 64


def main(image_path, seed_path):
    image = Path(image_path).read_bytes()
    seed = Path(seed_path).read_bytes()
    # Non-overlapping windows: a cluster left behind holds several whole ones.
    left = [offset for offset in range(0, len(seed) - WINDOW + 1, WINDOW)
            if seed[offset:offset + WINDOW] in image]
    if left:
        print(f"{image_path} still holds {len(left)} of {seed_path}'s "
              f"{len(seed) // WINDOW} {WINDOW}-byte runs")
        return 1
    print(f"{image_path} holds nothing of {seed_path}")
    return 0


if __name__ == "__main__":
    if len(sys.argv) != 3:
        sys.exit(__doc__.splitlines()[0])
    sys.exit(main(sys.argv[1], sys.argv[2]))
