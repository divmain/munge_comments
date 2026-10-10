package com.example.docs;

public class Deprecated {
    /**
     * Use {@link #replacement()} instead.
     *
     * @deprecated javac reads this tag.
     */
    @java.lang.Deprecated
    public void old() {}

    /** Not deprecated: doc-protected. */
    public void replacement() {}
}
