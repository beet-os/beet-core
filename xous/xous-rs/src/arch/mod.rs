#[cfg(beetos)]
mod aarch64;
#[cfg(beetos)]
pub use aarch64::*;

#[cfg(all(not(feature = "processes-as-threads"), not(beetos)))]
pub mod hosted;
#[cfg(all(not(feature = "processes-as-threads"), not(beetos)))]
pub use hosted::*;

#[cfg(all(feature = "processes-as-threads", not(beetos)))]
pub mod test;
#[cfg(all(feature = "processes-as-threads", not(beetos)))]
pub use test::*;
