package com.example.docs;

/**
 * A Javadoc comment on a class: doc-protected.
 */
public class DocComments {
    /// A Markdown doc comment (JEP 467): doc-protected.
    /// Its lines form one comment each.
    public int value;

    //// javac reads four slashes as a Markdown doc line too.
    private int hidden;

    /* A plain block comment: prose. */
    // A plain line comment: prose.
    public int get() {
        return value + hidden;
    }
}
