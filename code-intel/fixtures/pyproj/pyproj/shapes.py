"""Abstract base + concrete shapes — exercises inheritance / abstractmethod override
(Python's 'interface impl' — the DI/fan-out case to compare against rust-analyzer)."""

from abc import ABC, abstractmethod


class Shape(ABC):
    @abstractmethod
    def area(self) -> float:
        ...


class Circle(Shape):
    def __init__(self, r: float) -> None:
        self.r = r

    def area(self) -> float:
        return 3.14159 * self.r * self.r


class Square(Shape):
    def __init__(self, s: float) -> None:
        self.s = s

    def area(self) -> float:
        return self.s * self.s
