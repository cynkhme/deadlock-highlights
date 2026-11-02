//! The deadlock-api client dlhl uses for every request.

use deadlock_api::blocking::{Client, Transport};
use deadlock_api::{FileCache, RetryPolicy};

use crate::config::Config;
use crate::error::{Error, Result};

/// A client caching under the output directory: the crate stores match metadata at
/// `matches/{id}/metadata.json` there, so a rerun requests only what it lacks.
///
/// `transport` replaces the network in tests.
pub(crate) fn client(cfg: &Config, transport: Option<Box<dyn Transport>>) -> Result<Client> {
    let mut b = Client::builder()
        .base_url(cfg.api_base_url.clone())
        .api_key(cfg.deadlock_api_key.clone())
        .cache(FileCache::new(cfg.out.clone()))
        .retry(RetryPolicy::new(3));
    if let Some(t) = transport {
        b = b.transport(t);
    }
    // The base URL is fixed, so a build error means a key that is not a header value.
    b.build()
        .map_err(|e| Error::Usage(format!("deadlock-api client: {e}")))
}
