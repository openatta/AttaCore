//! A sandbox that admits it is not one.

use crate::interface::exec::{
    Confined, Enforcement, ProcessSpec, Sandbox, SandboxMode, SandboxPolicy,
};

/// Constrains nothing, and says so.
///
/// The honest backend for a provider that does not run real processes: there
/// is nothing to confine. Reporting `None` rather than `Full` is what keeps
/// `require_enforcement` meaningful — a deployment that demands an absolute
/// boundary must not get one from a provider that has no way to impose it.
pub struct NoSandbox;

impl Sandbox for NoSandbox {
    fn confine(&self, spec: ProcessSpec, _policy: &SandboxPolicy) -> Confined {
        Confined {
            spec,
            mode: SandboxMode::Unavailable,
            enforcement: Enforcement::None,
            unmet: vec!["this provider does not run processes it could confine".into()],
        }
    }
}
