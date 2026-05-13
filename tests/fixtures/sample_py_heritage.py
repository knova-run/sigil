"""Fixture for Python heritage extraction tests."""

from abc import ABC, abstractmethod


class Animal:
    pass


class Mixin:
    pass


class Dog(Animal, Mixin):
    pass


class Shape(ABC):
    @abstractmethod
    def area(self):
        pass
