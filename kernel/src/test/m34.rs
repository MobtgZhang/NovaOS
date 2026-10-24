//! The M3 and M4 suites: capability, IPC, filesystem, and services.
//!
//! # Why these live in their own file
//!
//! The harness registration list is the milestone's exit gate, and several
//! agents work on it concurrently. Keeping each milestone's cases in its own
//! module means the shared files -- `harness.rs` and `mod.rs` -- carry only a
//! registration line per milestone, so two milestones never have to touch the
//! same region of the same file.
//!
//! # The bar these tests must clear
//!
//! The pure logic behind these mechanisms is host-tested in `nova-cap`,
//! `nova-ipc`, `nova-fs`, and `nova-protocols`. These kernel suites test the
//! *mechanism* the host tests cannot: that the syscall path really consults
//! the capability table, that a denied right really produces `E_DENIED`, that
//! a capability really moves between two task tables atomically.
//!
//! A test that passes without exercising the mechanism is worse than no test
//! at all, because it reports green for a feature that does not work.

use super::harness::TestCase;

/// The M3 suites, in registration order.
pub static M3_SUITES: &[super::harness::TestSuite] = &[];

/// The M4 suites, in registration order.
pub static M4_SUITES: &[super::harness::TestSuite] = &[];

/// The unused-import guard: `TestCase` is referenced by the case functions
/// once they exist.
#[allow(dead_code)]
type UnusedCaseType = TestCase;
