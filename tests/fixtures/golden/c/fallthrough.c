/* Classifies a character class code. */
int classify(int code) {
    int score = 0;
    switch (code) {
    case 0:
        score += 1;
        /* FALLTHROUGH */
    case 1:
        score += 2;
        // fall through
    case 2:
        score += 3;
        /* Deliberately continues into the next case. */
        // FALLTHRU
    case 3:
        score += 4;
        break;
    default:
        // An ordinary prose comment.
        break;
    }
    return score;
}
