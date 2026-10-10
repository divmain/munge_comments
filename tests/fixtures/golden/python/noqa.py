import os  # noqa: F401
import sys  # noqa

# A long line follows; the trailing noqa silences the linter.
URL = "https://example.com/a/very/long/path/that/exceeds/the/configured/line/length/limit"  # noqa: E501
PATH = sys.path  # nosec B105
