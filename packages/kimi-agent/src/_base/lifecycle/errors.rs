use std::fmt;

#[derive(Clone, Debug)]
pub struct LedgerDisposedError {
    pub ledger_label: String,
    pub operation: String,
    pub ledger_state: String,
}

impl LedgerDisposedError {
    pub fn new(
        ledger_label: impl Into<String>,
        operation: impl Into<String>,
        ledger_state: impl Into<String>,
    ) -> Self {
        Self {
            ledger_label: ledger_label.into(),
            operation: operation.into(),
            ledger_state: ledger_state.into(),
        }
    }
}

impl fmt::Display for LedgerDisposedError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "Ledger '{}' is {}: cannot {} on a {} ledger",
            self.ledger_label, self.ledger_state, self.operation, self.ledger_state
        )
    }
}

impl std::error::Error for LedgerDisposedError {}
