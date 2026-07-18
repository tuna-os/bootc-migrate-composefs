//! Transition routing: which (source backend → target backend) re-bases are
//! supported, and by what strategy.
//!
//! This is the capability table from issue #30 / #45. Rows are added as
//! engine support lands; `route()` is the single source of truth the CLI
//! consults before touching the system.

use std::fmt;

/// A bootc root-storage backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    /// Classic OSTree deployment (hardlink checkout of an ostree commit).
    Ostree,
    /// ComposeFS-sealed EROFS deployment.
    Composefs,
}

impl fmt::Display for Backend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Backend::Ostree => write!(f, "ostree"),
            Backend::Composefs => write!(f, "composefs"),
        }
    }
}

/// How a supported transition is carried out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Strategy {
    /// The proven OSTree → ComposeFS pipeline from bootc-migrate-core
    /// (phases 0–5 with /etc merge, /var carry-over, bootloader switch).
    CoreMigration,
    /// Swap the composefs image in place — no backend conversion needed.
    /// Planned; not yet implemented (issue #30, scenario A analog).
    ImageSwap,
    /// Deploy the target as a plain OSTree deployment, skipping the
    /// composefs phases. Planned; not yet implemented (issue #30, M1).
    OstreeDeploy,
}

/// A supported (or planned) transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Route {
    pub from: Backend,
    pub to: Backend,
    pub strategy: Strategy,
    /// Whether the engine can execute this today.
    pub implemented: bool,
}

/// The transition table. Ordered; first match wins.
const ROUTES: &[Route] = &[
    Route {
        from: Backend::Ostree,
        to: Backend::Composefs,
        strategy: Strategy::CoreMigration,
        implemented: true,
    },
    Route {
        from: Backend::Composefs,
        to: Backend::Composefs,
        strategy: Strategy::ImageSwap,
        implemented: false,
    },
    Route {
        from: Backend::Ostree,
        to: Backend::Ostree,
        strategy: Strategy::OstreeDeploy,
        implemented: false,
    },
    Route {
        from: Backend::Composefs,
        to: Backend::Ostree,
        strategy: Strategy::OstreeDeploy,
        implemented: false,
    },
];

/// Look up the route for a backend pair. `None` means the transition is not
/// even planned.
pub fn route(from: Backend, to: Backend) -> Option<Route> {
    ROUTES
        .iter()
        .copied()
        .find(|r| r.from == from && r.to == to)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ostree_to_composefs_is_implemented() {
        let r = route(Backend::Ostree, Backend::Composefs).unwrap();
        assert!(r.implemented);
        assert_eq!(r.strategy, Strategy::CoreMigration);
    }

    #[test]
    fn every_backend_pair_has_a_planned_route() {
        for from in [Backend::Ostree, Backend::Composefs] {
            for to in [Backend::Ostree, Backend::Composefs] {
                assert!(route(from, to).is_some(), "no route for {from} -> {to}");
            }
        }
    }

    #[test]
    fn unimplemented_routes_are_marked() {
        assert!(
            !route(Backend::Composefs, Backend::Ostree)
                .unwrap()
                .implemented
        );
    }
}
