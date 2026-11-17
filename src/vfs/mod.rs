pub mod fs;
#[cfg(feature = "fuse")]
mod fuse;

pub use self::fs::Filesystem;
#[cfg(feature = "fuse")]
pub use self::fuse::Fuse;

#[cfg(test)]
pub mod tests;
