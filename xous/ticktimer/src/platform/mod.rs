#[cfg(not(beetos))]
pub mod hosted;
#[cfg(not(beetos))]
pub use hosted::*;

#[cfg(beetos)]
#[macro_use]
pub mod atsama5d2;
#[cfg(beetos)]
pub use atsama5d2::*;
