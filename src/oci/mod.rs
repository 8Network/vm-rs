//! OCI image management — registry client + content-addressable store.
//!
//! Pull images from any OCI-compliant registry (Docker Hub, GitHub Container Registry,
//! private registries) and store them locally with content-addressable blobs.

pub mod registry;
pub mod store;

pub use registry::pull;
pub use store::{ImageConfig, ImageStore};
