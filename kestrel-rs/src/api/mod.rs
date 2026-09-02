//! Reolink HTTP CGI API client.

pub mod discover;
pub mod http;
pub mod client;
pub mod error;
pub mod models;
pub mod settings;
pub mod vendor;

pub use client::ReolinkClient;
// Re-exported as this module's interface. The binary itself does not use
// every name, and in a crate with no external consumers that reads as an
// unused import - but the tests do, and cargo fix will happily delete them
// and break the test build, which is how this comment came to exist.
#[allow(unused_imports)]
pub use error::{redact_rtsp, Error, Result};
#[allow(unused_imports)]
pub use settings::{Block, Setting, SettingValue};
#[allow(unused_imports)]
pub use vendor::{StreamSource, Vendor, VendorInfo, VENDORS};
#[allow(unused_imports)]
pub use models::{
    Channel, DeviceInfo, DeviceKind, EventKind, Lens, Recording, SourceId, StreamType,
};
