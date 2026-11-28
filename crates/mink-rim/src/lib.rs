//! Optional declarative microkernel rim (`Scope`/`Disposer`, `SeatRegistry`,
//! `RimProfile`/`RimBundle`) layered over `mink-core`'s reversible seams.

pub mod assemble;
pub mod scope;
pub mod seat;

pub use assemble::{
    BundleActivation, RimBundle, RimInstallContext, RimProfile, RimSession, RuntimeAssembler,
};
pub use scope::{Disposer, Scope, ScopeGuard, scopes_related};
pub use seat::{SeatError, SeatRegistry};
