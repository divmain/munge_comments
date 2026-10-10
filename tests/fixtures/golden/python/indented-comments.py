def outer():
    # Indented comment-only line, first in the block.
    value = 1
    if value:
        # Deeper indentation.
        # A two-line group at the same indent.
        value += 1
            # Over-indented comment lines are still comments.
    # Dedented back to the function body.
    return value
