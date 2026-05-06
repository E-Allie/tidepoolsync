//! Library surface of `tidepoolsync`.

pub mod config;
pub mod convert;
pub(crate) mod convert_data;
pub mod sync;
pub mod watermark;

/// Nightscout `app` value used by all tidepoolsync documents.
pub use convert_data::APP_NAME;
