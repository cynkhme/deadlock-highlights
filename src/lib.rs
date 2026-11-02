//! Highlight windows and clips for Deadlock VODs, from the game clock on screen and
//! deadlock-api.com match data.

mod align;
mod clock;
mod config;
mod error;
mod events;
mod place;
mod profile;
#[cfg(test)]
mod test_support;
mod track;

pub use config::{Config, Progress};
pub use error::Error;
pub use profile::Profile;
