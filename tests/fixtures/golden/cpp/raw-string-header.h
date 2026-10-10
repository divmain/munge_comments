#pragma once

// A real comment before the raw string.
inline const char *pattern() {
    return R"(/* not a comment */ // nor this)"; // a real trailing comment
}

/* A real block comment after it. */
inline const char *delimited() {
    return R"xyz(text with )" inside /* still text */)xyz";
}
