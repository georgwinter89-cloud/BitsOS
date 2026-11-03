//! Generated user API modules (see the crate documentation). One private module per header; every item is re-exported here, so
//! `uapi::drm_version` and `uapi::DRM_IOCTL_VERSION` are the paths (a public `uapi::drm` would clash with the crate name).
#![allow(non_camel_case_types, non_upper_case_globals, non_snake_case, clippy::all)]

mod amdgpu;
mod dma_buf;
mod drm;
mod drm_mode;
mod sync_file;

pub use amdgpu::*;
pub use dma_buf::*;
pub use drm::*;
pub use drm_mode::*;
pub use sync_file::*;
