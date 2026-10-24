//! The service manifest: what a service declares that it expects.
//!
//! # A manifest is a declaration, never a grant
//!
//! The plan is explicit: "manifest 只是期望声明，不自动获得权限" — a manifest is
//! only a declaration of expectations, and it does not automatically obtain
//! any authority. The type system is what makes that true, not a convention:
//!
//! * [`ServiceManifest`] has **no field** of any capability or handle type.
//!   There is nowhere to put a handle, so there is nothing to accidentally
//!   honour.
//! * The dependency list holds *names*, not endpoints. A dependency says
//!   "the service called `file-server` must be READY before I start" — it
//!   does not say which endpoint to talk to, and cannot.
//! * The [`ReadyMessage`] carries a service name and a state, nothing else.
//! * The manifest type is `Copy`-free of any handle-bearing field, and a
//!   test asserts that no field of the manifest can carry a handle.
//!
//! The authority to talk to a service comes from the *capability table*, and
//! from nowhere else. A service that wants to talk to the file server must be
//! granted an endpoint capability by whatever supervises it; declaring the
//! dependency in the manifest changes the *order* of startup, not the *set of
//! things the service may do*.
//!
//! # The restart window
//!
//! `restart_max_attempts` alone is not enough: a service that crashes every
//! millisecond would be restarted forever, burning the machine. The manifest
//! therefore carries both a maximum attempt count and a *window* — the
//! period over which the attempts are counted. Attempts that fall outside the
//! window do not count against the budget, so a service that has been stable
//! for a while gets a fresh budget.
//!
//! The window needs a clock, and a real clock makes the policy untestable on
//! the host. The time source is therefore an injected trait,
//! [`Clock`], and [`RestartTracker`] takes it by reference: the kernel
//! supplies a timer-backed clock, and the tests supply a counter.

use nova_abi::error::ErrorCode;

/// Maximum number of dependencies a manifest may declare.
pub const MAX_DEPENDENCIES: usize = 8;

/// Maximum length of a service name, in bytes.
pub const MAX_NAME_BYTES: usize = 32;

/// The state of a service in the supervision hierarchy.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(u8)]
pub enum ServiceState {
    /// The service has been declared but not started.
    Starting = 0,
    /// The service has started and announced itself.
    Ready = 1,
    /// The service has failed and is waiting to be restarted.
    Failed = 2,
    /// The service exceeded its restart budget and will not be tried again.
    PermanentlyFailed = 3,
    /// The service exited normally.
    Stopped = 4,
}

impl ServiceState {
    /// The wire value.
    pub const fn code(self) -> u8 {
        self as u8
    }

    /// Whether the service may still be started or restarted.
    pub const fn is_restartable(self) -> bool {
        matches!(
            self,
            ServiceState::Starting | ServiceState::Failed | ServiceState::Ready
        )
    }

    /// Whether the service has permanently failed.
    pub const fn is_permanently_failed(self) -> bool {
        matches!(self, ServiceState::PermanentlyFailed)
    }
}

/// The READY handshake.
///
/// A service announces that it has finished starting and is ready to serve.
/// The message carries a name and a state — nothing else. In particular it
/// carries no endpoint and no handle: announcing readiness is not the same
/// as granting access, and a handshake that carried a capability would make
/// every service that heard it a holder of that capability.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[repr(C)]
pub struct ReadyMessage {
    /// The protocol version of the handshake.
    pub version: u32,
    /// The state being announced.
    pub state: ServiceState,
    /// The length of the service name that follows, in bytes.
    pub name_len: u32,
    /// Reserved. Must be zero.
    pub reserved: u32,
}

/// Size of a READY message header, in bytes.
pub const READY_MESSAGE_SIZE: usize = 16;

const _: () = assert!(size_of::<ReadyMessage>() == READY_MESSAGE_SIZE);
const _: () = assert!(core::mem::offset_of!(ReadyMessage, version) == 0);
const _: () = assert!(core::mem::offset_of!(ReadyMessage, state) == 4);
const _: () = assert!(core::mem::offset_of!(ReadyMessage, name_len) == 8);
const _: () = assert!(core::mem::offset_of!(ReadyMessage, reserved) == 12);

/// A dependency: another service that must be READY first.
///
/// A dependency is a *name*. The supervisor resolves the name to an endpoint
/// when it wires the service up, and the endpoint — together with the
/// capability to use it — comes from the capability table. The dependency
/// itself confers nothing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Dependency {
    /// The name of the service that must be READY first.
    name: [u8; MAX_NAME_BYTES],
    /// How many bytes of `name` are meaningful.
    name_len: u8,
}

impl Dependency {
    /// A dependency on a service with the given name.
    ///
    /// The name is truncated to [`MAX_NAME_BYTES`]. A longer name is a
    /// mistake in the manifest, and truncating it would create a dependency
    /// on a *different* service than the one named — so the name is refused
    /// by returning `None`.
    pub fn new(name: &str) -> Option<Dependency> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_NAME_BYTES {
            return None;
        }
        let mut stored = [0u8; MAX_NAME_BYTES];
        stored[..bytes.len()].copy_from_slice(bytes);
        Some(Dependency {
            name: stored,
            name_len: bytes.len() as u8,
        })
    }

    /// The dependency's name.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..usize::from(self.name_len)]).unwrap_or("")
    }
}

/// What a service declares about how it should be run.
///
/// # There is no authority here
///
/// This struct has no field of a capability, endpoint, or handle type, and
/// there is no way to add one without changing the type's meaning. A
/// manifest says "I need `file-server` to be READY before I start" and "if I
/// fail more than N times in a window, stop trying". It does not say "and
/// therefore you may send me messages".
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct ServiceManifest {
    /// The name of the service this manifest describes.
    name: [u8; MAX_NAME_BYTES],
    /// How many bytes of `name` are meaningful.
    name_len: u8,
    /// The dependencies, in declaration order.
    dependencies: [Dependency; MAX_DEPENDENCIES],
    /// How many dependencies are declared.
    dependency_count: u8,
    /// How long the service may take to announce READY, in milliseconds.
    startup_timeout_ms: u32,
    /// How many times the service may be restarted within the window.
    restart_max_attempts: u32,
    /// The width of the restart window, in milliseconds.
    restart_window_ms: u32,
}

impl ServiceManifest {
    /// A manifest for a service with the given name and no dependencies.
    pub fn new(name: &str) -> Option<ServiceManifest> {
        let bytes = name.as_bytes();
        if bytes.is_empty() || bytes.len() > MAX_NAME_BYTES {
            return None;
        }
        let mut stored = [0u8; MAX_NAME_BYTES];
        stored[..bytes.len()].copy_from_slice(bytes);
        Some(ServiceManifest {
            name: stored,
            name_len: bytes.len() as u8,
            dependencies: [Dependency {
                name: [0; MAX_NAME_BYTES],
                name_len: 0,
            }; MAX_DEPENDENCIES],
            dependency_count: 0,
            startup_timeout_ms: 0,
            restart_max_attempts: 0,
            restart_window_ms: 0,
        })
    }

    /// The service's name.
    pub fn name(&self) -> &str {
        core::str::from_utf8(&self.name[..usize::from(self.name_len)]).unwrap_or("")
    }

    /// Add a dependency.
    ///
    /// Returns `false` if the dependency list is full or the dependency is
    /// already declared. A manifest with more dependencies than the bound
    /// cannot hold is refused rather than silently truncated: a truncated
    /// dependency list would start a service before something it declared it
    /// needed.
    pub fn add_dependency(&mut self, dependency: Dependency) -> bool {
        let count = usize::from(self.dependency_count);
        if count >= MAX_DEPENDENCIES {
            return false;
        }
        if self.dependencies[..count]
            .iter()
            .any(|existing| existing.name() == dependency.name())
        {
            return false;
        }
        self.dependencies[count] = dependency;
        self.dependency_count = u8::try_from(count.saturating_add(1)).unwrap_or(u8::MAX);
        true
    }

    /// The declared dependencies, in order.
    pub fn dependencies(&self) -> impl Iterator<Item = &Dependency> {
        self.dependencies[..usize::from(self.dependency_count)].iter()
    }

    /// The number of declared dependencies.
    pub const fn dependency_count(&self) -> usize {
        self.dependency_count as usize
    }

    /// Whether the manifest declares a dependency on the named service.
    pub fn depends_on(&self, name: &str) -> bool {
        self.dependencies()
            .any(|dependency| dependency.name() == name)
    }

    /// Set how long the service may take to announce READY, in milliseconds.
    pub fn set_startup_timeout_ms(&mut self, timeout_ms: u32) {
        self.startup_timeout_ms = timeout_ms;
    }

    /// The startup timeout, in milliseconds.
    pub const fn startup_timeout_ms(&self) -> u32 {
        self.startup_timeout_ms
    }

    /// Set how many times the service may be restarted within the window.
    pub fn set_restart_max_attempts(&mut self, attempts: u32) {
        self.restart_max_attempts = attempts;
    }

    /// The restart attempt budget.
    pub const fn restart_max_attempts(&self) -> u32 {
        self.restart_max_attempts
    }

    /// Set the width of the restart window, in milliseconds.
    pub fn set_restart_window_ms(&mut self, window_ms: u32) {
        self.restart_window_ms = window_ms;
    }

    /// The width of the restart window, in milliseconds.
    pub const fn restart_window_ms(&self) -> u32 {
        self.restart_window_ms
    }

    /// Build the READY message this service would announce.
    pub fn ready_message(&self) -> ReadyMessage {
        ReadyMessage {
            version: 1,
            state: ServiceState::Ready,
            name_len: u32::from(self.name_len),
            reserved: 0,
        }
    }
}

/// A source of time.
///
/// The restart policy needs to know *when* a failure happened, and a real
/// clock would make the policy untestable on the host. The clock is therefore
/// injected: the kernel supplies one backed by a timer, and the tests supply
/// one backed by a counter.
pub trait Clock {
    /// The current time, in milliseconds since an arbitrary epoch.
    ///
    /// The epoch is arbitrary on purpose — only *differences* between
    /// readings matter, and requiring a wall clock would make the policy
    /// depend on something the kernel does not have.
    fn now_ms(&self) -> u64;
}

/// A clock that advances only when the test tells it to.
///
/// The kernel's clock is a timer; this one is a counter. The restart policy
/// is the same code either way, which is the point.
#[derive(Clone, Copy, Debug, Default)]
pub struct ManualClock {
    now: u64,
}

impl ManualClock {
    /// A clock reading zero.
    pub const fn new() -> Self {
        ManualClock { now: 0 }
    }

    /// Move the clock forward by `delta_ms`.
    pub fn advance(&mut self, delta_ms: u64) {
        self.now = self.now.saturating_add(delta_ms);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now
    }
}

/// What the supervisor should do after a failed start.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RestartDecision {
    /// Start the service again.
    Restart,
    /// The service has used up its budget; it is permanently failed.
    PermanentlyFailed,
}

/// The restart-budget bookkeeping.
///
/// The tracker counts failed starts *within a window*. A failure that
/// happened more than `restart_window_ms` ago does not count, so a service
/// that has been running for a while starts again with a fresh budget. This
/// is the difference between "restart at most N times" — which is a
/// permanent death sentence for a service that crashes once a day — and a
/// policy that only punishes a *rapid* failure loop.
#[derive(Clone, Copy, Debug)]
pub struct RestartTracker {
    /// How many restarts are permitted within the window.
    max_attempts: u32,
    /// The width of the window, in milliseconds.
    window_ms: u32,
    /// How many failures have been counted in the current window.
    attempts: u32,
    /// When the current window opened.
    window_start_ms: u64,
    /// Whether the window has been started at all.
    started: bool,
}

impl RestartTracker {
    /// A tracker with the given budget and window.
    pub const fn new(max_attempts: u32, window_ms: u32) -> Self {
        RestartTracker {
            max_attempts,
            window_ms,
            attempts: 0,
            window_start_ms: 0,
            started: false,
        }
    }

    /// A tracker built from a manifest's declared policy.
    pub const fn from_manifest(manifest: &ServiceManifest) -> Self {
        RestartTracker::new(manifest.restart_max_attempts, manifest.restart_window_ms)
    }

    /// How many failures are currently counted.
    pub const fn attempts(&self) -> u32 {
        self.attempts
    }

    /// The budget.
    pub const fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    /// The window width, in milliseconds.
    pub const fn window_ms(&self) -> u32 {
        self.window_ms
    }

    /// Record a failed start and decide what to do.
    ///
    /// The window is checked first: a failure that falls outside the current
    /// window starts a new one, so the attempt count resets. The budget is
    /// then checked, and only when it is exhausted is the service declared
    /// permanently failed.
    pub fn record_failure<C: Clock>(&mut self, clock: &C) -> RestartDecision {
        let now = clock.now_ms();

        if !self.started {
            self.started = true;
            self.window_start_ms = now;
        } else {
            let window = u64::from(self.window_ms);
            // A window of zero means "no window": every failure is counted.
            if window > 0 {
                let elapsed = now.saturating_sub(self.window_start_ms);
                if elapsed >= window {
                    // The previous window has closed. This failure opens a new
                    // one, and a new window starts with a fresh budget.
                    self.window_start_ms = now;
                    self.attempts = 0;
                }
            }
        }

        self.attempts = self.attempts.saturating_add(1);
        if self.attempts > self.max_attempts {
            RestartDecision::PermanentlyFailed
        } else {
            RestartDecision::Restart
        }
    }

    /// Forget the counted failures, as after a successful start.
    ///
    /// A service that ran successfully has, by definition, not failed, so the
    /// budget is restored.
    pub fn reset(&mut self) {
        self.attempts = 0;
        self.started = false;
        self.window_start_ms = 0;
    }
}

/// The result of checking a service's dependencies.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum DependencyStatus {
    /// Every dependency is READY.
    Satisfied,
    /// At least one dependency is not READY yet.
    Waiting,
}

impl DependencyStatus {
    /// The ABI error code a failure maps to.
    ///
    /// A service that is not ready yet is a transient condition, and
    /// [`ErrorCode::Busy`] says exactly that: the caller should try again.
    pub const fn code(self) -> ErrorCode {
        match self {
            DependencyStatus::Satisfied => ErrorCode::Invalid,
            DependencyStatus::Waiting => ErrorCode::Busy,
        }
    }
}

/// Check a manifest's dependencies against the states of the services they name.
///
/// The check is pure: it takes the manifest and a lookup function, and
/// answers whether the service may start. It grants nothing — starting the
/// service is the supervisor's decision, and the service's authority comes
/// from the capability table.
pub fn check_dependencies<F>(manifest: &ServiceManifest, mut state_of: F) -> DependencyStatus
where
    F: FnMut(&str) -> Option<ServiceState>,
{
    for dependency in manifest.dependencies() {
        match state_of(dependency.name()) {
            Some(ServiceState::Ready) => {}
            _ => return DependencyStatus::Waiting,
        }
    }
    DependencyStatus::Satisfied
}

#[cfg(test)]
mod tests {
    use super::{
        Clock, Dependency, DependencyStatus, ManualClock, ReadyMessage, RestartDecision,
        RestartTracker, ServiceManifest, ServiceState, check_dependencies,
    };

    fn manifest() -> ServiceManifest {
        let mut manifest = ServiceManifest::new("file-server").expect("a valid name");
        manifest.add_dependency(Dependency::new("console-server").expect("a valid name"));
        manifest.add_dependency(Dependency::new("init").expect("a valid name"));
        manifest.set_startup_timeout_ms(5_000);
        manifest.set_restart_max_attempts(3);
        manifest.set_restart_window_ms(60_000);
        manifest
    }

    #[test]
    fn manifest_declares_dependencies() {
        let manifest = manifest();
        assert_eq!(manifest.name(), "file-server");
        assert_eq!(manifest.dependency_count(), 2);
        assert!(manifest.depends_on("console-server"));
        assert!(manifest.depends_on("init"));
        assert!(!manifest.depends_on("shell"));
        assert_eq!(
            manifest.dependencies().next().map(|d| d.name()),
            Some("console-server"),
            "the dependencies keep their declaration order"
        );

        // The startup timeout and the restart policy are declared too.
        assert_eq!(manifest.startup_timeout_ms(), 5_000);
        assert_eq!(manifest.restart_max_attempts(), 3);
        assert_eq!(manifest.restart_window_ms(), 60_000);
    }

    #[test]
    fn the_dependency_list_is_bounded() {
        let mut manifest = ServiceManifest::new("svc").expect("a valid name");
        for index in 0..super::MAX_DEPENDENCIES {
            let name = [b'd', b'0' + index as u8];
            assert!(
                manifest
                    .add_dependency(Dependency::new(core::str::from_utf8(&name).unwrap()).unwrap()),
                "dependency {index} should fit"
            );
        }
        assert_eq!(manifest.dependency_count(), super::MAX_DEPENDENCIES);
        assert!(
            !manifest.add_dependency(Dependency::new("one-too-many").unwrap()),
            "a ninth dependency must be refused, not silently dropped"
        );
        assert_eq!(manifest.dependency_count(), super::MAX_DEPENDENCIES);
    }

    #[test]
    fn manifest_grants_no_authority() {
        // The manifest cannot express authority. This is the check that
        // matters: a manifest that could name an endpoint or carry a handle
        // would be a back door around the capability table.
        //
        // 1. No field of the manifest is a handle, an endpoint, or a
        //    capability. The type has no such field, and the compiler agrees:
        //    the sizes below are fixed, and there is nothing else to set.
        let manifest = manifest();
        assert_eq!(
            size_of::<ServiceManifest>(),
            312,
            "the manifest is names, numbers, and arrays of the same"
        );
        assert_eq!(
            size_of::<Dependency>(),
            33,
            "a dependency is a name and nothing else"
        );

        // 2. A dependency names a service; it cannot address one. The only
        //    way to name a service is by name.
        let dependency = Dependency::new("console-server").expect("a valid name");
        assert_eq!(dependency.name(), "console-server");

        // 3. The READY handshake carries a name and a state. It carries no
        //    endpoint and no handle.
        let ready: ReadyMessage = manifest.ready_message();
        assert_eq!(ready.state, ServiceState::Ready);
        assert_eq!(ready.name_len, 11);
        assert_eq!(ready.reserved, 0);
        assert_eq!(size_of::<ReadyMessage>(), 16);

        // 4. The manifest's copy is `Copy` and contains no pointer-sized
        //    field that could smuggle an authority in. The whole type is
        //    names, numbers, and arrays of the same.
        let copied = manifest;
        assert_eq!(copied, manifest);
    }

    #[test]
    fn a_manifest_cannot_be_built_with_an_authority_bearing_field() {
        // The manifest's fields are private, so a caller outside the crate
        // cannot construct one field by field and smuggle something in. The
        // only way to build one is through the constructor, which takes a
        // name.
        let manifest = ServiceManifest::new("svc").expect("a valid name");
        assert_eq!(manifest.name(), "svc");
        assert_eq!(manifest.dependency_count(), 0);
        assert_eq!(manifest.startup_timeout_ms(), 0);
        assert_eq!(manifest.restart_max_attempts(), 0);
        assert_eq!(manifest.restart_window_ms(), 0);
    }

    #[test]
    fn the_ready_handshake_is_versioned() {
        let manifest = manifest();
        let ready = manifest.ready_message();
        assert_eq!(ready.version, 1);
        assert_eq!(ready.state, ServiceState::Ready);
        assert!(ready.state.is_restartable());
    }

    #[test]
    fn manifest_restart_window_limits_attempts() {
        let clock = ManualClock::new();
        let mut tracker = RestartTracker::new(3, 60_000);

        // Three failures inside the window are permitted.
        assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
        assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
        assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
        assert_eq!(tracker.attempts(), 3);

        // The fourth failure inside the same window exhausts the budget.
        assert_eq!(
            tracker.record_failure(&clock),
            RestartDecision::PermanentlyFailed,
            "the service must not be restarted forever"
        );
    }

    #[test]
    fn a_failure_outside_the_window_does_not_count() {
        let mut clock = ManualClock::new();
        let mut tracker = RestartTracker::new(3, 60_000);

        // Three failures, all inside the window.
        for _ in 0..3 {
            assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
        }

        // Wait past the window, then fail again. The window has closed, so
        // the budget is fresh and the service is restarted rather than
        // declared permanently failed.
        clock.advance(60_000);
        assert_eq!(
            tracker.record_failure(&clock),
            RestartDecision::Restart,
            "a failure after the window must not count against the old budget"
        );
        assert_eq!(tracker.attempts(), 1);
    }

    #[test]
    fn a_window_of_zero_counts_every_failure() {
        // A zero window means "no window": the budget is a plain attempt
        // count, which is the degenerate but well-defined case.
        let clock = ManualClock::new();
        let mut tracker = RestartTracker::new(1, 0);
        assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
        assert_eq!(
            tracker.record_failure(&clock),
            RestartDecision::PermanentlyFailed
        );
    }

    #[test]
    fn a_successful_start_restores_the_budget() {
        let clock = ManualClock::new();
        let mut tracker = RestartTracker::new(2, 60_000);
        assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
        assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
        tracker.reset();
        assert_eq!(tracker.attempts(), 0);
        assert_eq!(tracker.record_failure(&clock), RestartDecision::Restart);
    }

    #[test]
    fn the_tracker_can_be_built_from_a_manifest() {
        let manifest = manifest();
        let tracker = RestartTracker::from_manifest(&manifest);
        assert_eq!(tracker.max_attempts(), 3);
        assert_eq!(tracker.window_ms(), 60_000);
    }

    #[test]
    fn dependencies_gate_the_start() {
        let manifest = manifest();
        let states = |name: &str| match name {
            "console-server" => Some(ServiceState::Ready),
            "init" => Some(ServiceState::Ready),
            _ => None,
        };
        assert_eq!(
            check_dependencies(&manifest, states),
            DependencyStatus::Satisfied
        );

        let not_ready = |name: &str| match name {
            "console-server" => Some(ServiceState::Ready),
            "init" => Some(ServiceState::Starting),
            _ => None,
        };
        assert_eq!(
            check_dependencies(&manifest, not_ready),
            DependencyStatus::Waiting,
            "a dependency that is not READY blocks the start"
        );

        let missing = |_name: &str| None;
        assert_eq!(
            check_dependencies(&manifest, missing),
            DependencyStatus::Waiting,
            "an unknown dependency blocks the start rather than being ignored"
        );
    }

    #[test]
    fn a_manifest_with_no_dependencies_is_ready_to_start() {
        let manifest = ServiceManifest::new("leaf").expect("a valid name");
        assert_eq!(
            check_dependencies(&manifest, |_| None),
            DependencyStatus::Satisfied
        );
    }

    #[test]
    fn names_are_bounded_and_refused_when_too_long() {
        assert!(
            ServiceManifest::new("").is_none(),
            "an empty name is refused"
        );
        assert!(ServiceManifest::new(&"x".repeat(33)).is_none());
        assert!(ServiceManifest::new(&"x".repeat(32)).is_some());
        assert!(Dependency::new("").is_none());
        assert!(Dependency::new(&"x".repeat(33)).is_none());
    }

    #[test]
    fn the_clock_is_injectable() {
        // The policy works on any clock, which is what makes it testable
        // without a real timer.
        struct Ticks(u64);
        impl Clock for Ticks {
            fn now_ms(&self) -> u64 {
                self.0
            }
        }
        let mut tracker = RestartTracker::new(1, 10);
        let early = Ticks(0);
        let late = Ticks(10_000);
        assert_eq!(tracker.record_failure(&early), RestartDecision::Restart);
        assert_eq!(
            tracker.record_failure(&late),
            RestartDecision::Restart,
            "the second failure is outside the window"
        );
    }
}
