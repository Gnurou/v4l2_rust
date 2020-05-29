pub mod direction;
pub mod dqbuf;
pub mod qbuf;
pub mod states;

use super::Device;
use crate::ioctl;
use crate::memory::*;
use crate::*;
use direction::*;
use dqbuf::*;
use qbuf::*;
use states::*;
use std::os::unix::io::{AsRawFd, RawFd};
use std::sync::{Arc, Mutex, Weak};

/// Contains the handles (pointers to user memory or DMABUFs) that are kept
/// when a buffer is processed by the kernel and returned to the user upon
/// `dequeue()` or `streamoff()`.
#[allow(type_alias_bounds)]
pub type PlaneHandles<M: Memory> = Vec<M::DQBufType>;

/// Represents the current state of an allocated buffer.
enum BufferState<M: Memory> {
    /// The buffer can be obtained via `get_buffer()` and be queued.
    Free,
    /// The buffer has been requested via `get_buffer()` but is not queued yet.
    PreQueue,
    /// The buffer is queued and waiting to be dequeued.
    Queued(PlaneHandles<M>),
    /// The buffer has been dequeued and the client is still using it. The buffer
    /// will go back to the `Free` state once the reference is dropped.
    Dequeued,
}

/// Base values of a queue, that are always value no matter the state the queue
/// is in. This base object remains alive as long as the queue is borrowed from
/// the `Device`.
pub struct QueueBase {
    /// Reference to the device, so `fd` is kept valid and to let us mark the
    /// queue as free again upon destruction.
    device: Arc<Mutex<Device>>,
    /// Fd of the device, for faster access since it can be shared. This is
    /// guaranteed to remain valid as long as the device is alive, and we are
    /// keeping a refcounted reference to it.
    fd: RawFd,
    type_: QueueType,
    capabilities: ioctl::BufferCapabilities,
}

impl AsRawFd for QueueBase {
    fn as_raw_fd(&self) -> RawFd {
        self.fd
    }
}

impl<'a> Drop for QueueBase {
    /// Make the queue available again.
    fn drop(&mut self) {
        assert_eq!(
            self.device.lock().unwrap().used_queues.remove(&self.type_),
            true
        );
    }
}

/// V4L2 queue object. Specialized according to its configuration state so that
/// only valid methods can be called from a given point.
pub struct Queue<D, S>
where
    D: Direction,
    S: QueueState,
{
    inner: QueueBase,
    _d: std::marker::PhantomData<D>,
    state: S,
}

/// Methods of `Queue` that are available no matter the state.
impl<D, S> Queue<D, S>
where
    D: Direction,
    S: QueueState,
{
    pub fn get_capabilities(&self) -> ioctl::BufferCapabilities {
        self.inner.capabilities
    }

    pub fn get_type(&self) -> QueueType {
        self.inner.type_
    }

    pub fn get_format(&self) -> Result<Format> {
        ioctl::g_fmt(&self.inner, self.inner.type_)
    }

    /// This method can invalidate any current format iterator, hence it requires
    /// the queue to be mutable. This way of doing is not perfect though, as setting
    /// the format on one queue can change the options available on another.
    pub fn set_format(&mut self, format: Format) -> Result<Format> {
        let type_ = self.inner.type_;
        ioctl::s_fmt(&mut self.inner, type_, format)
    }

    /// Performs exactly as `set_format`, but does not actually apply `format`.
    /// Useful to check what modifications need to be done to a format before it
    /// can be used.
    pub fn try_format(&self, format: Format) -> Result<Format> {
        ioctl::try_fmt(&self.inner, self.inner.type_, format)
    }

    /// Returns a `FormatBuilder` which is set to the currently active format
    /// and can be modified and eventually applied. The `FormatBuilder` holds
    /// a mutable reference to this `Queue`.
    pub fn change_format<'a>(&'a mut self) -> Result<FormatBuilder<'a>> {
        FormatBuilder::new(&mut self.inner)
    }

    /// Returns an iterator over all the formats currently supported by this queue.
    pub fn format_iter(&self) -> ioctl::FormatIterator<QueueBase> {
        ioctl::FormatIterator::new(&self.inner, self.inner.type_)
    }
}

/// Builder for a V4L2 format. This takes a mutable reference on the queue, so
/// it is supposed to be short-lived: get one, adjust the format, and apply.
pub struct FormatBuilder<'a> {
    queue: &'a mut QueueBase,
    format: Format,
}

impl<'a> FormatBuilder<'a> {
    fn new(queue: &'a mut QueueBase) -> Result<Self> {
        let format = ioctl::g_fmt(queue, queue.type_)?;
        Ok(Self { queue, format })
    }

    /// Get a reference to the format built so far. Useful for checking the
    /// currently set format after getting a builder, or the actual settings
    /// that will be applied by the kernel after a `try_apply()`.
    pub fn format(&self) -> &Format {
        &self.format
    }

    pub fn set_size(mut self, width: usize, height: usize) -> Self {
        self.format.width = width as u32;
        self.format.height = height as u32;
        self
    }

    pub fn set_pixelformat(mut self, pixel_format: impl Into<PixelFormat>) -> Self {
        self.format.pixelformat = pixel_format.into();
        self
    }

    /// Apply the format built so far. The kernel will adjust the format to fit
    /// the driver's capabilities if needed, and the format actually applied will
    /// be returned.
    pub fn apply(self) -> Result<Format> {
        ioctl::s_fmt(self.queue, self.queue.type_, self.format)
    }

    /// Try to apply the format built so far. The kernel will adjust the format
    /// to fit the driver's capabilities if needed, so make sure to check important
    /// parameters after this call.
    pub fn try_apply(&mut self) -> Result<()> {
        let new_format = ioctl::try_fmt(self.queue, self.queue.type_, self.format.clone())?;

        self.format = new_format;
        Ok(())
    }
}

impl<D: Direction> Queue<D, QueueInit> {
    /// Create a queue for type `queue_type` on `device`. A queue of a specific type
    /// can be requested only once.
    ///
    /// Not all devices support all kinds of queue. To test whether the queue is supported,
    /// a REQBUFS(0) is issued on the device. If it is not successful, the device is
    /// deemed to not support this kind of queue and this method will fail.
    fn create(device: Arc<Mutex<Device>>, queue_type: QueueType) -> Result<Queue<D, QueueInit>> {
        let mut device_lock = device.lock().unwrap();

        if device_lock.used_queues.contains(&queue_type) {
            return Err(Error::AlreadyBorrowed);
        }

        // Check that the queue is valid for this device by doing a dummy REQBUFS.
        // Obtain its capacities while we are at it.
        let capabilities: ioctl::BufferCapabilities =
            ioctl::reqbufs(&mut *device_lock, queue_type, MemoryType::MMAP, 0)?;

        assert_eq!(device_lock.used_queues.insert(queue_type), true);

        let fd = device_lock.as_raw_fd();

        drop(device_lock);

        Ok(Queue::<D, QueueInit> {
            inner: QueueBase {
                device,
                fd,
                type_: queue_type,
                capabilities,
            },
            _d: std::marker::PhantomData,
            state: QueueInit {},
        })
    }

    /// Allocate `count` buffers for this queue and make it transition to the
    /// `BuffersAllocated` state.
    pub fn request_buffers<M: Memory>(
        mut self,
        count: u32,
    ) -> Result<Queue<D, BuffersAllocated<M>>> {
        let type_ = self.inner.type_;
        let num_buffers: usize =
            ioctl::reqbufs(&mut self.inner, type_, M::HandleType::MEMORY_TYPE, count)?;

        // The buffers have been allocated, now let's get their features.
        let querybuf: ioctl::QueryBuffer = ioctl::querybuf(&self.inner, self.inner.type_, 0)?;

        Ok(Queue {
            inner: self.inner,
            _d: std::marker::PhantomData,
            state: BuffersAllocated {
                num_buffers,
                buffers_state: Arc::new(Mutex::new(std::iter::repeat_with(|| BufferState::Free)
                    .take(num_buffers)
                    .collect())),
                buffer_features: querybuf,
            },
        })
    }
}

impl<D: Direction, M: Memory> Queue<D, BuffersAllocated<M>> {
    pub fn free_buffers(mut self) -> Result<Queue<D, QueueInit>> {
        let type_ = self.inner.type_;
        ioctl::reqbufs(&mut self.inner, type_, M::HandleType::MEMORY_TYPE, 0)?;

        Ok(Queue {
            inner: self.inner,
            _d: std::marker::PhantomData,
            state: QueueInit {},
        })
    }
}

impl Queue<Output, QueueInit> {
    /// Acquires the OUTPUT queue from `device`.
    ///
    /// This method will fail if the queue has already been obtained and has not
    /// yet been released.
    pub fn get_output_queue(device: Arc<Mutex<Device>>) -> Result<Queue<Output, QueueInit>> {
        Queue::<Output, QueueInit>::create(device, QueueType::VideoOutput)
    }

    /// Acquires the OUTPUT_MPLANE queue from `device`.
    ///
    /// This method will fail if the queue has already been obtained and has not
    /// yet been released.
    pub fn get_output_mplane_queue(device: Arc<Mutex<Device>>) -> Result<Queue<Output, QueueInit>> {
        Queue::<Output, QueueInit>::create(device, QueueType::VideoOutputMplane)
    }
}

impl Queue<Capture, QueueInit> {
    /// Acquires the CAPTURE queue from `device`.
    ///
    /// This method will fail if the queue has already been obtained and has not
    /// yet been released.
    pub fn get_capture_queue(device: Arc<Mutex<Device>>) -> Result<Queue<Capture, QueueInit>> {
        Queue::<Capture, QueueInit>::create(device, QueueType::VideoCapture)
    }

    /// Acquires the CAPTURE_MPLANE queue from `device`.
    ///
    /// This method will fail if the queue has already been obtained and has not
    /// yet been released.
    pub fn get_capture_mplane_queue(
        device: Arc<Mutex<Device>>,
    ) -> Result<Queue<Capture, QueueInit>> {
        Queue::<Capture, QueueInit>::create(device, QueueType::VideoCaptureMplane)
    }
}

/// Represents a queued buffer which has not been processed due to `streamoff` being
/// called on the queue.
pub struct CanceledBuffer<M: Memory> {
    /// Index of the buffer,
    pub index: u32,
    /// Plane handles that were passed when the buffer has been queued.
    pub plane_handles: PlaneHandles<M>,
}

impl<D: Direction, M: Memory> Queue<D, BuffersAllocated<M>> {
    pub fn num_buffers(&self) -> usize {
        self.state.num_buffers
    }

    pub fn streamon(&mut self) -> Result<()> {
        let type_ = self.inner.type_;
        ioctl::streamon(&mut self.inner, type_)
    }

    /// Stop streaming on this queue.
    ///
    /// If successful, then all the buffers that are queued but have not been
    /// dequeued yet return to the `Free` state. Buffer references obtained via
    /// `dequeue()` remain valid.
    pub fn streamoff(&mut self) -> Result<Vec<CanceledBuffer<M>>> {
        let type_ = self.inner.type_;
        ioctl::streamoff(&mut self.inner, type_)?;

        let mut buffers_state = self.state.buffers_state.lock().unwrap();

        let canceled_buffers = buffers_state
            .iter_mut()
            .enumerate()
            .filter_map(|(i, state)| {
                // Filter entries not in queued state.
                match *state {
                    BufferState::Queued(_) => (),
                    _ => return None,
                };

                // Set entry to Free state and steal its handles.
                let old_state = std::mem::replace(state, BufferState::Free);
                Some(CanceledBuffer::<M> {
                    index: i as u32,
                    plane_handles: match old_state {
                        // We have already tested for this state above, so this
                        // branch is guaranteed.
                        BufferState::Queued(plane_handles) => plane_handles,
                        _ => unreachable!("Inconsistent buffer state!"),
                    },
                })
            })
            .collect();

        Ok(canceled_buffers)
    }

    pub fn query_buffer(&self, id: usize) -> Result<ioctl::QueryBuffer> {
        ioctl::querybuf(&self.inner, self.inner.type_, id)
    }

    // Take buffer `id` in order to prepare it for queueing, provided it is available.
    // When we get a WRBuffer, can't we have it pre-filled with the right number of planes,
    // etc from QUERY_BUF?
    pub fn get_buffer<'a>(&'a mut self, id: usize) -> Result<QBuffer<'a, D, M>> {
        let mut buffers_state = self.state.buffers_state.lock().unwrap();
        let buffer_state = &mut buffers_state[id];

        match buffer_state {
            BufferState::Free => (),
            _ => return Err(Error::AlreadyBorrowed),
        };

        let num_planes = self.state.buffer_features.planes.len();

        // The buffer remains will remain in PreQueue state until it is queued
        // or the reference to it is lost.
        *buffer_state = BufferState::PreQueue;
        let fuse = BufferStateFuse::new(Arc::downgrade(&self.state.buffers_state), id);

        Ok(QBuffer::new(self, id, num_planes, fuse))
    }

    /// Dequeue the next processed buffer and return it.
    ///
    /// The V4L2 buffer can not be reused until the returned `DQBuffer` is
    /// dropped, so make sure to keep it around for as long as you need it. It can
    /// be moved into a `Rc` or `Arc` if you need to pass it to several clients.
    ///
    /// The data in the `DQBuffer` is read-only.
    pub fn dequeue(&self) -> Result<DQBuffer<M>> {
        let dqbuf: ioctl::DQBuffer = ioctl::dqbuf(&self.inner, self.inner.type_)?;
        let id = dqbuf.index as usize;

        let mut buffers_state = self.state.buffers_state.lock().unwrap();
        let buffer_state = &mut buffers_state[id];

        // The buffer will remain Dequeued until our reference to it is destroyed.
        let state = std::mem::replace(buffer_state, BufferState::Dequeued);
        let plane_handles = match state {
            BufferState::Queued(plane_handles) => plane_handles,
            _ => unreachable!("Inconsistent buffer state"),
        };
        let fuse = BufferStateFuse::new(Arc::downgrade(&self.state.buffers_state), id);

        Ok(DQBuffer::new(plane_handles, dqbuf, fuse))
    }
}

/// A fuse that will return the buffer to the Free state when destroyed, unless
/// it has been disarmed.
// TODO Use Arc::Weak<Mutex<BufferState>> here to make DQBuffer passable across threads?
struct BufferStateFuse<M: Memory> {
    buffers_state: Weak<Mutex<Vec<BufferState<M>>>>,
    index: usize,
}

impl<M: Memory> BufferStateFuse<M> {
    /// Create a new fuse that will set `state` to `BufferState::Free` if
    /// destroyed before `disarm()` has been called.
    fn new(buffers_state: Weak<Mutex<Vec<BufferState<M>>>>, index: usize) -> Self {
        BufferStateFuse {
            buffers_state,
            index,
        }
    }

    /// Disarm this fuse, e.g. the monitored state will be left untouched when
    /// the fuse is destroyed.
    fn disarm(&mut self) {
        // Drop our weak reference.
        self.buffers_state = Weak::new();
    }
}

impl<M: Memory> Drop for BufferStateFuse<M> {
    fn drop(&mut self) {
        match self.buffers_state.upgrade() {
            None => (),
            Some(buffers_state) => {
                let mut buffers_state = buffers_state.lock().unwrap();
                buffers_state[self.index] = BufferState::Free;
            }
        };
    }
}