//! Operations specific to DMABuf-type buffers.
use log::warn;

use super::*;
use crate::{bindings, ioctl};
use std::os::unix::io::AsRawFd;

pub struct DMABuf;

pub type DMABufferHandles<T> = Vec<DMABufHandle<T>>;

impl Memory for DMABuf {
    const MEMORY_TYPE: MemoryType = MemoryType::DMABuf;
}

impl Imported for DMABuf {}

pub trait DMABufSource: AsRawFd + Debug + Send {
    fn len(&self) -> u64;

    /// Make Clippy happy.
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl DMABufSource for std::fs::File {
    fn len(&self) -> u64 {
        match self.metadata() {
            Err(_) => {
                warn!("Failed to compute File size for use as DMABuf, using 0...");
                0
            }
            Ok(m) => m.len(),
        }
    }
}

/// Handle for a DMABUF plane. Any type that can provide a file descriptor is
/// valid.
#[derive(Debug)]
pub struct DMABufHandle<T: DMABufSource>(pub T);

impl<T: DMABufSource> From<T> for DMABufHandle<T> {
    fn from(dmabuf: T) -> Self {
        DMABufHandle(dmabuf)
    }
}

impl<T: DMABufSource + 'static> PlaneHandle for DMABufHandle<T> {
    type Memory = DMABuf;

    fn fill_v4l2_plane(&self, plane: &mut bindings::v4l2_plane) {
        plane.m.fd = self.0.as_raw_fd();
        plane.length = self.0.len() as u32;
    }
}

impl<T: DMABufSource> DMABufHandle<T> {
    pub fn map(&self) -> Result<PlaneMapping, ioctl::MMAPError> {
        let len = self.0.len();

        ioctl::mmap(&self.0, 0, len as u32)
    }
}
