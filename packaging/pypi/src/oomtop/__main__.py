"""``python -m oomtop …`` runs the bundled binary (replaces this process; exit code passes through)."""

import os
import sys

from oomtop import find_oomtop_bin


def main() -> None:
    binary = find_oomtop_bin()
    os.execv(binary, [binary, *sys.argv[1:]])


if __name__ == "__main__":
    main()
