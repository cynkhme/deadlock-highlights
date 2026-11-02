//! Highlight windows and clips for Deadlock VODs, from the game clock on screen and
//! deadlock-api.com match data.

mod align;
mod api;
mod clock;
mod config;
mod error;
mod events;
mod fetch;
mod gate;
mod history;
mod output;
mod place;
mod profile;
mod read;
mod score;
#[cfg(test)]
mod test_support;
mod track;
mod video;

pub use config::{Config, Progress};
pub use error::Error;
pub use history::AnchorSource;
pub use profile::Profile;
