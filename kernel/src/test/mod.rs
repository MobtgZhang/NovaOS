//! In-kernel test harness for M0 boot verification.
//!
//! Each suite prints a machine-greppable line so `scripts/qemu.sh` can assert on
//! the serial output without parsing free-form prose:
//!
//! ```text
//! [NOVA-TEST] SUITE boot_smoke
//! [NOVA-TEST]   case <name> ... ok|fail
//! [NOVA-TEST] RESULT suites=1 cases=12 passed=12 failed=0
//! ```
//!
//! A failing case is reported but does **not** halt the kernel: the remaining
//! suites still run so one CI log shows every regression at once. Only after
//! all suites finish does the harness exit with [`crate::EXIT_TEST_FAILURE`].

pub mod harness;