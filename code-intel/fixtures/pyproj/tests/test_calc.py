"""pytest test module — gives test functions + third-party (pytest) + intra-project imports."""

import pytest

from pyproj.calc import make_default, total_area
from pyproj.shapes import Circle


def test_total_area() -> None:
    assert total_area([Circle(1.0)]) == pytest.approx(3.14159)


def test_make_default() -> None:
    assert len(make_default()) == 2
