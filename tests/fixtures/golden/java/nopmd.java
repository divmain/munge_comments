package com.example.lint;

public class Nopmd {
    private int unused = 0; //NOPMD - kept for serialization
    private int other = 1; // NOPMD

    // CHECKSTYLE:OFF
    public int Bad_Name() {
        return unused + other;
    }
    // CHECKSTYLE:ON

    //noinspection unused
    private void ignored() {}
}
