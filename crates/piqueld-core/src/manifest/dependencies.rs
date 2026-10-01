//! Startup ordering between the services of one application.

use std::collections::BTreeSet;

/// A service that names the services it waits for during rollout.
pub(crate) trait Dependent {
    /// Logical service name.
    fn name(&self) -> &str;
    /// Logical names of the services that must be healthy first.
    fn dependencies(&self) -> impl Iterator<Item = &str>;
}

impl Dependent for super::Service {
    fn name(&self) -> &str {
        &self.name
    }

    fn dependencies(&self) -> impl Iterator<Item = &str> {
        self.depends_on.iter().map(String::as_str)
    }
}

impl Dependent for crate::resource::DesiredService {
    fn name(&self) -> &str {
        self.logical_name.as_str()
    }

    fn dependencies(&self) -> impl Iterator<Item = &str> {
        self.depends_on.iter().map(crate::ServiceName::as_str)
    }
}

/// Orders services so each one follows every service it depends on.
pub(crate) trait StartupOrder<T> {
    /// Returns services in startup order, preserving declaration order where
    /// dependencies allow, followed by the services in or behind a cycle.
    /// Unknown dependency names do not constrain the order.
    fn startup_order(&self) -> (Vec<&T>, Vec<&T>);
}

impl<T: Dependent> StartupOrder<T> for [T] {
    fn startup_order(&self) -> (Vec<&T>, Vec<&T>) {
        let known = self.iter().map(Dependent::name).collect::<BTreeSet<_>>();
        let mut started = BTreeSet::new();
        let mut ordered = Vec::with_capacity(self.len());
        let mut pending = self.iter().collect::<Vec<_>>();
        // Each pass starts every service whose dependencies already started.
        // Services count is bounded by validation, so repeated passes are cheap.
        loop {
            let remaining = pending.len();
            pending.retain(|&service| {
                let ready = service
                    .dependencies()
                    .all(|dependency| started.contains(dependency) || !known.contains(dependency));
                if ready {
                    started.insert(service.name());
                    ordered.push(service);
                }
                !ready
            });
            if pending.len() == remaining {
                return (ordered, pending);
            }
        }
    }
}
