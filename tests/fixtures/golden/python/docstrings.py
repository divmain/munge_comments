"""Module docstring: a string expression, never a comment."""


class Greeter:
    """Class docstring with a # hash that is not a comment."""

    def greet(self, name):
        """Method docstring."""
        # The only real comment in this file.
        return f"hello {name}  # not a comment either"
