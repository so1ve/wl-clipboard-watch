//! Watch Wayland clipboard selection changes through the data-control
//! protocols.
//!
//! [`Watcher`] prefers `ext-data-control-v1` and falls back to
//! `wlr-data-control-v1`.
//!
//! ```no_run
//! use anyhow::Result;
//! use wl_clipboard_watch::{Event, Transfer, Watcher};
//!
//! # fn main() -> Result<()> {
//! let mut watcher = Watcher::connect()?;
//!
//! loop {
//!     let Event::Selection(selection) = watcher.next_event()? else {
//!         continue;
//!     };
//!     let mime_type = "text/plain;charset=utf-8";
//!     if !selection.offers(mime_type) {
//!         continue;
//!     }
//!
//!     if let Transfer::Complete(bytes) = watcher.receive(&selection, mime_type)? {
//!         println!("{}", String::from_utf8_lossy(&bytes));
//!     }
//! }
//! # }
//! ```

#![deny(unsafe_code)]

mod watcher;

use std::time::Duration;

use anyhow::{Result, ensure};
pub use watcher::Watcher;

const DEFAULT_MAX_MIME_BYTES: usize = 256 * 1024 * 1024;
const DEFAULT_TRANSFER_TIMEOUT: Duration = Duration::from_secs(5);

/// Limits applied while receiving clipboard data.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Config {
    max_mime_bytes: usize,
    transfer_timeout: Duration,
}

impl Config {
    /// Creates a configuration.
    ///
    /// The size limit and `transfer_timeout` must be greater than zero.
    pub fn new(max_mime_bytes: usize, transfer_timeout: Duration) -> Result<Self> {
        ensure!(
            max_mime_bytes > 0,
            "maximum MIME size must be greater than zero"
        );
        ensure!(
            !transfer_timeout.is_zero(),
            "transfer timeout must be greater than zero"
        );

        Ok(Self {
            max_mime_bytes,
            transfer_timeout,
        })
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            max_mime_bytes: DEFAULT_MAX_MIME_BYTES,
            transfer_timeout: DEFAULT_TRANSFER_TIMEOUT,
        }
    }
}

/// The data-control protocol selected for a watcher.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Protocol {
    /// `ext-data-control-v1`.
    ExtDataControlV1,
    /// `wlr-data-control-v1`.
    WlrDataControlV1,
}

impl Protocol {
    /// Returns the protocol's Wayland interface name.
    #[must_use]
    pub const fn interface_name(self) -> &'static str {
        match self {
            Self::ExtDataControlV1 => "ext_data_control_manager_v1",
            Self::WlrDataControlV1 => "zwlr_data_control_manager_v1",
        }
    }
}

/// A clipboard selection notification.
#[derive(Clone, Debug)]
pub enum Event {
    /// A new selection and its advertised MIME types.
    Selection(Selection),
    /// The clipboard selection was cleared.
    Cleared,
}

/// An advertised clipboard selection.
///
/// This value contains the advertised MIME types, but not their data. Pass it
/// to [`Watcher::receive`] on the same watcher to receive one MIME type.
///
/// Receiving returns [`Transfer::Stale`] after the selection changes.
#[derive(Clone, Debug)]
pub struct Selection {
    generation: u64,
    mime_types: Vec<String>,
}

impl Selection {
    /// Returns the MIME types advertised by the clipboard source.
    #[must_use]
    pub fn mime_types(&self) -> &[String] {
        &self.mime_types
    }

    /// Returns whether the source advertised `mime_type`.
    #[must_use]
    pub fn offers(&self, mime_type: &str) -> bool {
        self.mime_types.iter().any(|offered| offered == mime_type)
    }
}

/// The result of receiving one MIME type from a selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum Transfer {
    /// The requested data was received completely.
    Complete(Vec<u8>),
    /// A newer clipboard selection superseded the requested selection.
    Stale,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_zero_limit_and_timeout() {
        assert!(Config::new(0, Duration::from_secs(1)).is_err());
        assert!(Config::new(1, Duration::ZERO).is_err());
    }

    #[test]
    fn watcher_can_move_to_a_worker_thread() {
        const fn assert_send<T: Send>() {}

        assert_send::<Watcher>();
    }
}
