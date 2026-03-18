#[cfg(not(beetos))]
mod hosted;

#[cfg(not(beetos))]
pub use hosted::*;

#[cfg(beetos)]
mod apple_t8103;

#[cfg(beetos)]
pub use apple_t8103::*;
