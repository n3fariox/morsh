#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::{fork_and_detach, redirect_stdio};

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::{fork_and_detach, redirect_stdio};
