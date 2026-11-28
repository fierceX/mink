//! Optional declarative microkernel rim (`Scope`/`Disposer`, `SeatRegistry`/
//! `SeatHub`, `RimProfile`/`RimBundle`) layered over `mink-core`'s reversible
//! seams.

pub mod assemble;
pub mod scope;
pub mod seat;

pub use assemble::{
    BundleActivation, RimBundle, RimInstallContext, RimProfile, RimSession, RuntimeAssembler,
};
pub use scope::{Disposer, Scope, ScopeGuard, ScopeSegmentError, scopes_related};
pub use seat::{SeatError, SeatHub, SeatRegistry};
