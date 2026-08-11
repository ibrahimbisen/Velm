//! A GPU buffer that grows and never shrinks.
//!
//! Every per-frame array in this crate goes through here. The rule is the one
//! `docs/01-architecture.md` sets for the whole renderer: a steady-state frame must
//! allocate nothing. A board that once had 100k quads on screen will very likely
//! have them again, and reallocating a several-megabyte GPU buffer mid-pan is
//! exactly the hitch the project exists to avoid — so capacity ratchets upwards and
//! stays there.
//!
//! Growth is to the next power of two, which bounds the number of reallocations over
//! a session to about thirty regardless of how the board is used.

/// Whether a write reused the existing allocation or replaced it.
///
/// A caller holding a bind group over the buffer has to rebuild it when the buffer
/// is replaced, and forgetting to is a validation error at draw time rather than at
/// write time. Returning it makes the obligation visible.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Growth {
    Reused,
    Reallocated,
}

pub(crate) struct GrowableBuffer {
    buffer: wgpu::Buffer,
    label: &'static str,
    usage: wgpu::BufferUsages,
    /// Bytes allocated, always a multiple of [`wgpu::COPY_BUFFER_ALIGNMENT`].
    capacity: wgpu::BufferAddress,
    /// Bytes written by the last [`Self::write`].
    len: wgpu::BufferAddress,
}

impl GrowableBuffer {
    pub(crate) fn new(
        device: &wgpu::Device,
        label: &'static str,
        usage: wgpu::BufferUsages,
        capacity: wgpu::BufferAddress,
    ) -> Self {
        let capacity = round_up(capacity.max(wgpu::COPY_BUFFER_ALIGNMENT));
        Self {
            buffer: create(device, label, usage, capacity),
            label,
            usage,
            capacity,
            len: 0,
        }
    }

    pub(crate) fn buffer(&self) -> &wgpu::Buffer {
        &self.buffer
    }

    /// Uploads `bytes`, growing first if they do not fit.
    ///
    /// Writing zero bytes is legal and leaves the contents alone: an empty instance
    /// array is a frame with nothing of that kind on screen, not an error.
    pub(crate) fn write(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: &[u8],
    ) -> Growth {
        self.len = bytes.len() as wgpu::BufferAddress;
        let mut growth = Growth::Reused;
        if self.len > self.capacity {
            self.capacity = round_up((self.len as usize).next_power_of_two() as wgpu::BufferAddress);
            log::debug!("growing {} to {} bytes", self.label, self.capacity);
            self.buffer = create(device, self.label, self.usage, self.capacity);
            growth = Growth::Reallocated;
        }
        if !bytes.is_empty() {
            queue.write_buffer(&self.buffer, 0, bytes);
        }
        growth
    }
}

/// `write_buffer` requires a size that is a multiple of 4, and so does the buffer
/// itself; rounding here means no caller has to think about it.
fn round_up(bytes: wgpu::BufferAddress) -> wgpu::BufferAddress {
    bytes.div_ceil(wgpu::COPY_BUFFER_ALIGNMENT) * wgpu::COPY_BUFFER_ALIGNMENT
}

fn create(
    device: &wgpu::Device,
    label: &'static str,
    usage: wgpu::BufferUsages,
    size: wgpu::BufferAddress,
) -> wgpu::Buffer {
    device.create_buffer(&wgpu::BufferDescriptor {
        label: Some(label),
        size,
        usage,
        mapped_at_creation: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_round_up_to_the_copy_alignment() {
        assert_eq!(round_up(0), 0);
        assert_eq!(round_up(1), 4);
        assert_eq!(round_up(4), 4);
        assert_eq!(round_up(5), 8);
        assert_eq!(round_up(255), 256);
    }
}
