#[cfg(not(beetos))]
mod hosted;

#[cfg(not(beetos))]
pub use hosted::*;

#[cfg(beetos)]
mod atsama5d2;

#[cfg(beetos)]
pub use atsama5d2::*;
