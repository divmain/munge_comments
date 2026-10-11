package com.acme.billing;

/// Status codes the ledger service returns in its `X-Ledger-Status` header.
public final class LedgerCodes {
    private LedgerCodes() {}

    // @formatter:off
    // Codes 40-49 are retryable; the ledger documents that range as
    // transient, and RetryPolicy relies on this ordering.
    public static final int ACCEPTED   = 20;
    public static final int DUPLICATE  = 21;
    public static final int BUSY       = 40;
    public static final int REJECTED   = 50;
    // @formatter:on

    // Check if the code is retryable.
    public static boolean isRetryable(int code) {
        return code >= 40 && code < 50;
    }

    private static int legacy = 0; //NOPMD - read reflectively by the old exporter

    // the default
    public static int defaultCode() {
        return ACCEPTED;
    }
}
