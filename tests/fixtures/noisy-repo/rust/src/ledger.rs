use std::collections::HashMap;

// A ledger.
#[derive(Debug, Default)]
pub struct Ledger {
    // The balances.
    balances: HashMap<String, i64>,
}

impl Ledger {
    /// Posts `cents` to `account`, returning the new balance.
    pub fn post(&mut self, account: &str, cents: i64) -> i64 {
        // Get the balance or insert zero.
        let balance = self.balances.entry(account.to_owned()).or_insert(0);
        // Saturate rather than wrap: an overflowing balance is a data bug we
        // want to surface in reconciliation, and a wrapped value would look
        // like a plausible (negative) balance instead.
        *balance = balance.saturating_add(cents);
        *balance
    }

    // FIXME: the reconciliation job still reads balances through the
    // debug output; give it a real accessor.
    pub fn balance(&self, account: &str) -> i64 {
        /* Missing accounts read as zero /* not an error */ because the
        ledger is sparse: accounts appear on their first posting. */
        self.balances.get(account).copied().unwrap_or(0)
    }
}

// end of file
