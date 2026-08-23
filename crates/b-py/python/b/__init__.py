"""Package wrapper around the `b` extension module.

Symmetrical with `a`'s wrapper. Module b publishes no capsule of its own —
it is purely a consumer of `a._C_API` — so there is nothing extra to
forward, but the explicit form documents that and keeps the two packages
shaped the same.
"""

from .b import *  # noqa: F401,F403
from . import b as _ext

__doc__ = _ext.__doc__
__all__ = list(getattr(_ext, "__all__", []))
