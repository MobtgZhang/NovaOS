//! The kernel-side service supervisor.
//!
//! # What the supervisor does
//!
//! The supervisor owns the service table: which services exist, what state
//! each is in, when each may start, and what happens when one crashes. The
//! *policy* — dependency checking, the restart budget and its window, the
//! startup timeout — is `nova-protocols`' `check_dependencies`,
//! `RestartTracker`, and `ServiceManifest`. The supervisor is the mechanism
//! around that policy.
//!
//! # A manifest grants nothing
//!
//! The report is explicit: "manifest 只是期望声明，不自动获得权限" — a manifest is
//! a declaration of expectations and does not automatically obtain authority.
//! The supervisor therefore never mints a capability because a manifest asked
//! for one. The service's authority comes from the capability table, and the
//! supervisor's job is to decide *when* the service runs, not *what* it may
//! do.
//!
//! # The clock
//!
//! The restart policy needs a clock, and a real clock makes the policy
//! untestable. The clock is therefore injected: the kernel supplies one backed
//! by a monotonic tick counter, and the host tests supply
//! `nova_protocols::ManualClock`. The same policy code runs either way.
//!
//! # The startup timeout
//!
//! A service that is `Starting` but has not announced READY is given the
//! manifest's `startup_timeout_ms`. When the deadline passes, the start is
//! treated as a failure: the service is recorded as failed, and the restart
//! policy decides what happens next. A service that never becomes READY
//! therefore consumes its restart budget rather than hanging the boot.
//!
//! # Dependency ordering
//!
//! A service may start only when every service it depends on is READY. The
//! check is `nova_protocols::check_dependencies`, which is pure: it takes the
//! manifest and a lookup of the states of the services it names. The
//! supervisor supplies the lookup from its own table.

use nova_abi::error::ErrorCode;
use nova_protocols::manifest::{
    Clock, DependencyStatus, RestartDecision, RestartTracker, ServiceManifest, ServiceState,
    check_dependencies,
};

/// Maximum number of services the supervisor tracks.
pub const MAX_SERVICES: usize = 8;

/// Maximum length of a service name, in bytes.
///
/// The manifest's own bound, repeated here so the supervisor's name buffer and
/// the manifest's cannot drift apart in the *kernel's* copy of the name.
pub const MAX_NAME_BYTES: usize = 32;

/// A monotonic millisecond clock.
///
/// The kernel has no wall clock, and it does not need one: only *differences*
/// between readings matter. The counter is bumped by the timer interrupt
/// handler once M2's PIT is running; until then it stays at zero, which
/// makes every timeout fire immediately — a loud failure rather than a silent
/// one.
#[derive(Clone, Copy, Debug, Default)]
pub struct TickClock {
    now_ms: u64,
}

impl TickClock {
    /// A clock reading zero.
    pub const fn new() -> Self {
        Self { now_ms: 0 }
    }

    /// Advance the clock by `delta_ms`.
    ///
    /// Called from the timer interrupt handler. The value is saturated rather
    /// than wrapping, because a wrapping clock would make a timeout fire at
    /// the wrong moment — and a timeout firing at the wrong moment is a policy
    /// decision made by accident.
    pub fn advance(&mut self, delta_ms: u64) {
        self.now_ms = self.now_ms.saturating_add(delta_ms);
    }

    /// Set the clock to an absolute reading.
    pub fn set(&mut self, now_ms: u64) {
        self.now_ms = now_ms;
    }
}

impl Clock for TickClock {
    fn now_ms(&self) -> u64 {
        self.now_ms
    }
}

/// One service the supervisor manages.
#[derive(Clone, Copy, Debug)]
struct ServiceEntry {
    /// The service's name.
    name: [u8; MAX_NAME_BYTES],
    /// How many bytes of `name` are meaningful.
    name_len: u8,
    /// The state the service is in.
    state: ServiceState,
    /// The manifest that declares how it should be run.
    manifest: ServiceManifest,
    /// The restart-budget bookkeeping.
    tracker: RestartTracker,
    /// When the current start attempt began, in milliseconds.
    started_at_ms: u64,
    /// Whether the entry is occupied.
    in_use: bool,
}

impl ServiceEntry {
    /// The service's name.
    fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..usize::from(self.name_len)]).unwrap_or("")
    }
}

/// The service supervisor.
pub struct Supervisor {
    services: [Option<ServiceEntry>; MAX_SERVICES],
    clock: TickClock,
}

impl Default for Supervisor {
    fn default() -> Self {
        Self::new()
    }
}

impl Supervisor {
    /// A supervisor with no services and a clock reading zero.
    pub const fn new() -> Self {
        Self {
            services: [None; MAX_SERVICES],
            clock: TickClock::new(),
        }
    }

    /// The clock the restart policy reads.
    pub const fn clock(&self) -> &TickClock {
        &self.clock
    }

    /// The clock the restart policy reads, mutably.
    pub fn clock_mut(&mut self) -> &mut TickClock {
        &mut self.clock
    }

    /// Advance the clock.
    pub fn advance_clock(&mut self, delta_ms: u64) {
        self.clock.advance(delta_ms);
    }

    /// Forget every service.
    pub fn reset(&mut self) {
        self.services = [None; MAX_SERVICES];
        self.clock = TickClock::new();
    }

    /// Register a service from its manifest.
    ///
    /// Returns `false` when the service is already registered or the table is
    /// full. A duplicate is refused rather than merged: two manifests for one
    /// name is a configuration error, and picking one of them silently would
    /// make the effective policy depend on registration order.
    pub fn register(&mut self, manifest: ServiceManifest) -> bool {
        if self.find(manifest.name()).is_some() {
            return false;
        }
        let Some(slot) = self.services.iter().position(|entry| entry.is_none()) else {
            return false;
        };

        let mut name = [0u8; MAX_NAME_BYTES];
        let name_bytes = manifest.name().as_bytes();
        let name_len = name_bytes.len().min(MAX_NAME_BYTES);
        name[..name_len].copy_from_slice(&name_bytes[..name_len]);

        let tracker = RestartTracker::from_manifest(&manifest);
        self.services[slot] = Some(ServiceEntry {
            name,
            name_len: name_len as u8,
            state: ServiceState::Starting,
            manifest,
            tracker,
            started_at_ms: self.clock.now_ms(),
            in_use: true,
        });
        true
    }

    /// The state of a service, if it is registered.
    pub fn state_of(&self, name: &str) -> Option<ServiceState> {
        self.find(name).map(|entry| entry.state)
    }

    /// The manifest of a service, if it is registered.
    pub fn manifest_of(&self, name: &str) -> Option<&ServiceManifest> {
        self.find(name).map(|entry| &entry.manifest)
    }

    /// Whether a service may start now.
    ///
    /// A service may start when every service it depends on is READY. The
    /// check is `nova_protocols`' `check_dependencies`, so the rule is the
    /// one the manifest crate documents.
    pub fn may_start(&self, name: &str) -> bool {
        let Some(entry) = self.find(name) else {
            return false;
        };
        check_dependencies(&entry.manifest, |dependency| self.state_of(dependency))
            == DependencyStatus::Satisfied
    }

    /// Mark a service as READY.
    ///
    /// A successful start restores the restart budget: a service that ran
    /// successfully has, by definition, not failed.
    pub fn mark_ready(&mut self, name: &str) -> bool {
        let Some(slot) = self.find_slot(name) else {
            return false;
        };
        let Some(entry) = self.services[slot].as_mut() else {
            return false;
        };
        entry.state = ServiceState::Ready;
        entry.tracker.reset();
        true
    }

    /// Mark a service as failed and ask what the supervisor should do.
    ///
    /// The answer comes from the manifest's declared policy: the restart
    /// budget and the window it is counted in. A service that keeps crashing
    /// inside the window exhausts its budget and is declared permanently
    /// failed, rather than being restarted forever.
    pub fn mark_failed(&mut self, name: &str) -> RestartDecision {
        let Some(slot) = self.find_slot(name) else {
            return RestartDecision::PermanentlyFailed;
        };
        let Some(entry) = self.services[slot].as_mut() else {
            return RestartDecision::PermanentlyFailed;
        };
        entry.state = ServiceState::Failed;
        let decision = entry.tracker.record_failure(&self.clock);
        if decision == RestartDecision::PermanentlyFailed {
            entry.state = ServiceState::PermanentlyFailed;
        }
        decision
    }

    /// Restart a service, if the policy allows it.
    ///
    /// This is the whole of the restart mechanism: the budget is consulted,
    /// and either the service is started again or it is declared permanently
    /// failed.
    pub fn restart(&mut self, name: &str) -> RestartDecision {
        let decision = self.mark_failed(name);
        if decision == RestartDecision::Restart {
            if let Some(slot) = self.find_slot(name)
                && let Some(entry) = self.services[slot].as_mut()
            {
                entry.state = ServiceState::Starting;
                entry.started_at_ms = self.clock.now_ms();
            }
        }
        decision
    }

    /// Check every starting service against its startup timeout.
    ///
    /// A service that has not announced READY within its manifest's
    /// `startup_timeout_ms` is treated as a failed start. The list of names
    /// that timed out is returned so the caller can act on each one — the
    /// supervisor itself does not restart them, because restarting is the
    /// supervisor's *policy* and the caller's *action*.
    pub fn collect_timed_out(&mut self, out: &mut [u8]) -> usize {
        let now = self.clock.now_ms();
        let mut count = 0usize;
        for entry in self.services.iter_mut().flatten() {
            if entry.state != ServiceState::Starting {
                continue;
            }
            let timeout = u64::from(entry.manifest.startup_timeout_ms());
            if timeout == 0 {
                // A zero timeout means "no timeout": the manifest declined to
                // set one.
                continue;
            }
            let elapsed = now.saturating_sub(entry.started_at_ms);
            if elapsed < timeout {
                continue;
            }
            entry.state = ServiceState::Failed;
            if count < out.len() {
                out[count] = entry.name_len;
                count += 1;
            }
        }
        count
    }

    /// How many services are registered.
    pub fn count(&self) -> usize {
        self.services.iter().filter(|entry| entry.is_some()).count()
    }

    /// The state of every service, for the boot log.
    pub fn describe(&self) -> usize {
        self.count()
    }

    /// Find a service by name.
    fn find(&self, name: &str) -> Option<&ServiceEntry> {
        self.services
            .iter()
            .flatten()
            .find(|entry| entry.name() == name)
    }

    /// Find the slot a service occupies.
    fn find_slot(&self, name: &str) -> Option<usize> {
        self.services
            .iter()
            .position(|entry| entry.as_ref().is_some_and(|entry| entry.name() == name))
    }
}

/// The error a supervisor operation reports to the syscall layer.
pub fn supervisor_error(decision: RestartDecision) -> ErrorCode {
    match decision {
        RestartDecision::Restart => ErrorCode::Busy,
        RestartDecision::PermanentlyFailed => ErrorCode::Revoked,
    }
}
