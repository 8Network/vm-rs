//! socket device module

use super::base::Id;

/// common configure of socket device
pub trait VZSocketDeviceConfiguration {
    fn id(&self) -> Id;
}
