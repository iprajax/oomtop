"""oomtop: see the OOM coming.

The wheel installs the native ``oomtop`` binary into the environment's scripts directory; this module only
locates it (``python -m oomtop`` runs it). Docs: https://github.com/iprajax/oomtop
"""

import os
import sys
import sysconfig


def find_oomtop_bin() -> str:
    """Path of the oomtop binary installed by this wheel."""
    candidates = [sysconfig.get_path("scripts")]
    if sys.version_info >= (3, 10):
        user = sysconfig.get_preferred_scheme("user")
        candidates.append(sysconfig.get_path("scripts", scheme=user))
    for d in candidates:
        p = os.path.join(d, "oomtop")
        if os.path.isfile(p):
            return p
    raise FileNotFoundError("oomtop binary not found in " + ", ".join(candidates))


__all__ = ["find_oomtop_bin"]
