//! Highlight windows and clips for Deadlock VODs, from the game clock on screen and
//! deadlock-api.com match data.

mod config;
mod error;
mod profile;

pub use config::{Config, Progress};
pub use error::Error;
pub use profile::Profile;
