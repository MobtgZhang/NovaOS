//! The M2 suites: paging, user mode, and scheduling.
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
//! A test that passes without exercising the mechanism is worse than no test
//! at all, because it reports green for a feature that does not work. Every
//! case here names the specific failure it is designed to catch, and the
//! comment above each case says what that failure looks like from the outside.

use super::harness::TestSuite;

/// The M2 suites, in registration order.
///
/// The names are the milestone's exit gate and must not be renamed: the shell
/// harness asserts on exactly these strings.
pub static M2_SUITES: &[TestSuite] = &[];
