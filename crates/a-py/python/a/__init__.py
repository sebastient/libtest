"""Package wrapper around the `a` extension module.

Written out rather than left to maturin's generated wrapper for one
specific reason: the generated form is `from .a import *`, and `import *`
does not export names beginning with an underscore unless the module
declares `__all__`. PyO3 happens to emit an `__all__` that includes
`_C_API`, so the generated wrapper works today — but the capsule is the
cross-module contract this whole architecture rests on, and resting it on
a codegen detail of the binding generator is exactly the kind of implicit
coupling the rest of the design refuses. So re-export it deliberately.
"""

from .a import *  # noqa: F401,F403  -- the public surface (A, Frame)
from .a import _C_API  # the PyCapsule: module b resolves this by name
from . import a as _ext

__doc__ = _ext.__doc__
__all__ = [*getattr(_ext, "__all__", []), "_C_API"]
