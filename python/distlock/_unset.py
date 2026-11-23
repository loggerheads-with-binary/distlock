"""The "argument not given" marker, shared by locks, events and the defaults.

``None`` cannot play this part: for a wait it already means "as long as the server allows".
"""

from __future__ import annotations


class _Unset:
    __slots__ = ()

    def __repr__(self) -> str:
        return "UNSET"

    def __bool__(self) -> bool:
        return False

    def __copy__(self) -> "_Unset":
        return self

    def __deepcopy__(self, memo: object) -> "_Unset":
        return self


#: Marks "not given", so ``wait=None`` (wait as long as the server allows) stays meaningful.
_UNSET = _Unset()
