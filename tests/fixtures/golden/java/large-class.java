package com.example.big;

/**
 * A large class body: many members, each with its own comments.
 */
public class LargeClass {
    // The running total, updated by every operation.
    private int total;

    /** Adds the two operands. */
    public int add(int a, int b) {
        // Record the result of add.
        total = a + b;
        return total;
    }

    /** Subtracts the second operand from the first. */
    public int subtract(int a, int b) {
        // Record the result of subtract.
        total = a - b;
        return total;
    }

    /** Multiplies the operands. */
    public int multiply(int a, int b) {
        // Record the result of multiply.
        total = a * b;
        return total;
    }

    /** Divides, returning zero for a zero divisor. */
    public int divide(int a, int b) {
        // Record the result of divide.
        total = b == 0 ? 0 : a / b;
        return total;
    }

    /** Takes the remainder, zero for a zero divisor. */
    public int remainder(int a, int b) {
        // Record the result of remainder.
        total = b == 0 ? 0 : a % b;
        return total;
    }

    /** Returns the larger operand. */
    public int max(int a, int b) {
        // Record the result of max.
        total = Math.max(a, b);
        return total;
    }

    /** Returns the smaller operand. */
    public int min(int a, int b) {
        // Record the result of min.
        total = Math.min(a, b);
        return total;
    }

    /** Averages the operands, truncating. */
    public int average(int a, int b) {
        // Record the result of average.
        total = (a + b) / 2;
        return total;
    }

    /** Raises the first operand to the second. */
    public int power(int a, int b) {
        // Record the result of power.
        total = (int) Math.pow(a, b);
        return total;
    }

    /** Returns the absolute difference. */
    public int distance(int a, int b) {
        // Record the result of distance.
        total = Math.abs(a - b);
        return total;
    }

    /** Clamps the first operand into [0, b]. */
    public int clamp(int a, int b) {
        // Record the result of clamp.
        total = Math.max(0, Math.min(a, b));
        return total;
    }

    /** Compares the operands by sign. */
    public int sign(int a, int b) {
        // Record the result of sign.
        total = Integer.signum(a - b);
        return total;
    }

    // Reset support for tests.
    public void reset() {
        total = 0; // back to the initial state
    }
}
