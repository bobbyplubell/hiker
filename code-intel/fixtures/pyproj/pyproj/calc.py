"""Cross-module imports + a dynamic-dispatch call (`s.area()` on Shape) — the call edges."""

from .shapes import Circle, Shape, Square


def total_area(shapes: list[Shape]) -> float:
    return sum(s.area() for s in shapes)


def make_default() -> list[Shape]:
    return [Circle(1.0), Square(2.0)]
