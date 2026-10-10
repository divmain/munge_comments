package com.example.format;

public class Formatter {
    // @formatter:off
    private static final int[][] MATRIX = {
        {1, 0},
        {0, 1},
    };
    // @formatter:on

    // A prose comment after the region.
    public int trace() {
        return MATRIX[0][0] + MATRIX[1][1];
    }
}
