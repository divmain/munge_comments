#!/usr/bin/env python3
# Inventory helpers, one small function per concern.
import json

PAGE_SIZE = 50  # tuned against the slowest client


# A stock item as stored on disk.
class Item:
    # Construct from raw fields.
    def __init__(self, ident, quantity):
        self.ident = ident
        self.quantity = quantity  # never negative

    # Format for logs.
    def describe(self):
        return f"{self.quantity} x {self.ident}"


# Totals the quantities of all items.
def total(items):
    # The empty sum is zero.
    return sum(item.quantity for item in items)


def load(path):  # reads a JSON list
    with open(path, encoding="utf8") as handle:
        return [Item(**raw) for raw in json.load(handle)]


# Remembers results per argument.
def memoize(function):
    cache = {}

    def wrapper(argument):
        # Compute once per argument.
        if argument not in cache:
            cache[argument] = function(argument)
        return cache[argument]

    return wrapper


@memoize
def square(value):
    # Squares a number, slowly.
    return value * value


DEFAULT_ITEMS = [Item("a", 1), Item("b", 2)]  # sample data

# End of the module.
# Keep this note last.
