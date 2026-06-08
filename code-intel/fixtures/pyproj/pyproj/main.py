"""Entry point — calls across modules (calc -> shapes)."""

from .calc import make_default, total_area


def run() -> float:
    return total_area(make_default())
