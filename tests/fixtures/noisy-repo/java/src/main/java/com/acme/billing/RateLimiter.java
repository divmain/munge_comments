package com.acme.billing;

import java.util.ArrayDeque;
import java.util.Deque;

/**
 * Sliding-window limiter for calls to the ledger service.
 *
 * <p>Instances are not thread-safe; give each worker its own.
 */
public class RateLimiter {
    // The maximum number of calls.
    private final int maxCalls;
    // The window in milliseconds.
    private final long windowMillis;
    private final Deque<Long> calls = new ArrayDeque<>();

    // Constructor.
    public RateLimiter(int maxCalls, long windowMillis) {
        this.maxCalls = maxCalls;
        this.windowMillis = windowMillis;
    }

    /** Returns whether a call may be made at {@code nowMillis}, recording it if so. */
    public boolean tryAcquire(long nowMillis) {
        // Evict before counting: the ledger's own limiter uses a sliding
        // window too, and counting stale calls here makes us back off when
        // the ledger would have accepted the request.
        while (!calls.isEmpty() && nowMillis - calls.peekFirst() >= windowMillis) {
            calls.pollFirst();
        }
        // check if were at the limit, if so say no
        if (calls.size() >= maxCalls) {
            return false;
        }
        // Add the call.
        calls.addLast(nowMillis);
        return true;
    }
}
