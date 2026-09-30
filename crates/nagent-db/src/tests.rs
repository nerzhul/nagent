//! Cross-domain integration tests for the [`db`] module.
//!
//! Stub today — fills in as each per-domain repository lands. The
//! sqlite / postgres paths each get an in-memory fixture so the
//! extraction commits can add one query at a time without a full
//! integration suite per commit.

#[cfg(test)]
mod integration_tests {
    // Empty — see per-domain `mod tests` for the extraction-specific
    // coverage. This module exists so the `db::tests` submodule is
    // reachable from `db/mod.rs` (the `#[cfg(test)]` gate keeps it
    // out of production binaries).
}
