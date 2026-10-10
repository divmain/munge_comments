// Copyright 2026 Example Authors.
// SPDX-License-Identifier: MIT
#include <cstdlib>

// Parses a decimal integer, ignoring trailing garbage.
// The caller guarantees the text is non-null.
int parse(const char *text) {
    // NOLINTNEXTLINE(cert-err34-c)
    return std::atoi(text); /* atoi never throws */
}

/// Doubles its argument.
int twice(int value) {
    return value * 2; // NOLINT(readability-magic-numbers)
}
