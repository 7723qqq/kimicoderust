//! The value source the injection providers read from.
//!
//! An injection provider needs exactly one thing from the rest of the
//! engine: the durable value of a domain (`"goal"`, `"plan"`). That is a
//! narrow, read-only contract, and several very different things satisfy
//! it — the local state store the REPL and stdio entries run on, a
//! host-backed snapshot on the napi path, a fixture in a test.
//!
//! It lives here, next to the registry that consumes it, rather than
//! being borrowed from `crate::storage`. Naming it after what it provides
//! rather than after one implementation keeps the two apart: there is also
//! a `storage::StateStore` with a concrete store behind it (lifecycle and
//! migration) rather than this contract.

use crate::primitives::read::ReadOutcome;
use serde_json::Value;

/// A read-only source of durable domain values.
pub trait DomainValueSource {
    /// Read the durable value of a domain.
    ///
    /// Three outcomes, not two: `Absent` means the domain has no state, while
    /// `Unreadable` carries the cause of a failed read. A provider that renders
    /// `Unreadable` as "no state" tells the model the user has nothing, which
    /// is how a corrupt `todo.json` became "no todos" in the prompt.
    fn read_domain_state(&self, domain: &str) -> ReadOutcome<Value>;
}
