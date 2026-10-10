from typing import List

# Type comments are read by type checkers.
values = []  # type: List[int]


def add(a, b):
    # type: (int, int) -> int
    return a + b


import json  # type: ignore
