//! Reolink HTTP CGI API client.

pub mod http;
pub mod client;
pub mod error;
pub mod models;
pub mod settings;
pub mod vendor;

pub use client::ReolinkClient;
pub use error::{redact_rtsp, Error, Result};
pub use settings::{Block, Setting, SettingValue};
pub use vendor::{StreamSource, Vendor, VendorInfo, VENDORS};
pub use models::{
    Channel, DeviceInfo, DeviceKind, EventKind, Lens, Recording, SourceId, StreamType,
};
