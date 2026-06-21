//! virtio-snd (audio) — PCM stream control + playback over the kit.
//!
//! A control queue carries PCM commands (set-params / prepare / start / stop)
//! and a transmit queue carries playback buffers. Each is the kit's
//! submit/reap again; this module serializes the virtio-snd wire structures.
//! Scoped to the PCM playback path (no jacks/chmaps).

use core::ptr;

use crate::dma::DmaRegion;

use super::queue::{QueueError, Segment, Used, VirtQueue};

/// PCM request codes (`virtio_snd_hdr.code`).
pub mod code {
    pub const PCM_SET_PARAMS: u32 = 0x0100;
    pub const PCM_PREPARE: u32 = 0x0102;
    pub const PCM_RELEASE: u32 = 0x0103;
    pub const PCM_START: u32 = 0x0104;
    pub const PCM_STOP: u32 = 0x0105;
}

/// Response status (`virtio_snd_hdr` in a response).
pub mod resp {
    pub const OK: u32 = 0x8000;
    pub const BAD_MSG: u32 = 0x8001;
    pub const NOT_SUPP: u32 = 0x8002;
    pub const IO_ERR: u32 = 0x8003;
}

/// Common PCM formats (`virtio_snd_pcm_set_params.format`).
pub mod format {
    pub const S16: u8 = 5;
    pub const S32: u8 = 8;
}

/// Common PCM rates (`virtio_snd_pcm_set_params.rate`).
pub mod rate {
    pub const R44100: u8 = 8;
    pub const R48000: u8 = 9;
}

/// Length of a bare PCM command header (code + stream_id).
pub const PCM_HDR_LEN: usize = 8;
/// Length of a set-params command.
pub const SET_PARAMS_LEN: usize = 24;
/// Length of a transmit header (`virtio_snd_pcm_xfer`: stream_id).
pub const XFER_LEN: usize = 4;
/// Length of a transmit status (`virtio_snd_pcm_status`).
pub const PCM_STATUS_LEN: usize = 8;

/// A virtio-snd device: a control queue and a transmit (playback) queue.
pub struct VirtioSnd<const CQ: usize, const TQ: usize> {
    /// Control queue.
    pub control: VirtQueue<CQ>,
    /// Transmit (playback) queue.
    pub tx: VirtQueue<TQ>,
}

impl<const CQ: usize, const TQ: usize> VirtioSnd<CQ, TQ> {
    /// Build from the control and transmit queues.
    pub fn new(control: VirtQueue<CQ>, tx: VirtQueue<TQ>) -> Self {
        Self { control, tx }
    }

    /// Configure a PCM stream's parameters.
    #[allow(clippy::too_many_arguments)]
    pub fn set_params<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        stream_id: u32,
        buffer_bytes: u32,
        period_bytes: u32,
        channels: u8,
        fmt: u8,
        sample_rate: u8,
    ) -> Result<u16, QueueError> {
        if cmd.len() < SET_PARAMS_LEN || resp.is_empty() {
            return Err(QueueError::RegionTooSmall);
        }
        // SAFETY: cmd is large enough (checked) and DMA-valid.
        unsafe {
            let p = cmd.cpu_ptr();
            ptr::write_volatile(p as *mut u32, code::PCM_SET_PARAMS.to_le());
            ptr::write_volatile(p.add(4) as *mut u32, stream_id.to_le());
            ptr::write_volatile(p.add(8) as *mut u32, buffer_bytes.to_le());
            ptr::write_volatile(p.add(12) as *mut u32, period_bytes.to_le());
            ptr::write_volatile(p.add(16) as *mut u32, 0u32.to_le()); // features
            ptr::write_volatile(p.add(20), channels);
            ptr::write_volatile(p.add(21), fmt);
            ptr::write_volatile(p.add(22), sample_rate);
            ptr::write_volatile(p.add(23), 0u8); // padding
        }
        self.control_submit(cmd, SET_PARAMS_LEN, resp)
    }

    /// Prepare a stream.
    pub fn prepare<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        stream_id: u32,
    ) -> Result<u16, QueueError> {
        self.pcm_command(cmd, resp, code::PCM_PREPARE, stream_id)
    }
    /// Start a stream.
    pub fn start<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        stream_id: u32,
    ) -> Result<u16, QueueError> {
        self.pcm_command(cmd, resp, code::PCM_START, stream_id)
    }
    /// Stop a stream.
    pub fn stop<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        stream_id: u32,
    ) -> Result<u16, QueueError> {
        self.pcm_command(cmd, resp, code::PCM_STOP, stream_id)
    }

    fn pcm_command<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        resp: &R,
        cmd_code: u32,
        stream_id: u32,
    ) -> Result<u16, QueueError> {
        if cmd.len() < PCM_HDR_LEN || resp.is_empty() {
            return Err(QueueError::RegionTooSmall);
        }
        // SAFETY: cmd is large enough (checked) and DMA-valid.
        unsafe {
            let p = cmd.cpu_ptr();
            ptr::write_volatile(p as *mut u32, cmd_code.to_le());
            ptr::write_volatile(p.add(4) as *mut u32, stream_id.to_le());
        }
        self.control_submit(cmd, PCM_HDR_LEN, resp)
    }

    fn control_submit<C: DmaRegion, R: DmaRegion>(
        &mut self,
        cmd: &C,
        cmd_len: usize,
        resp: &R,
    ) -> Result<u16, QueueError> {
        self.control.add_buf(&[
            Segment {
                addr: cmd.phys_addr(),
                len: cmd_len as u32,
                device_writable: false,
            },
            Segment {
                addr: resp.phys_addr(),
                len: resp.len() as u32,
                device_writable: true,
            },
        ])
    }

    /// Queue PCM playback data: a readable transfer header + the audio buffer,
    /// and a device-writable status. `xfer` holds the `stream_id`.
    pub fn play<X: DmaRegion, D: DmaRegion, S: DmaRegion>(
        &mut self,
        xfer: &X,
        data: &D,
        status: &S,
        stream_id: u32,
    ) -> Result<u16, QueueError> {
        if xfer.len() < XFER_LEN || status.len() < PCM_STATUS_LEN {
            return Err(QueueError::RegionTooSmall);
        }
        // SAFETY: xfer is large enough (checked) and DMA-valid.
        unsafe { ptr::write_volatile(xfer.cpu_ptr() as *mut u32, stream_id.to_le()) };
        self.tx.add_buf(&[
            Segment {
                addr: xfer.phys_addr(),
                len: XFER_LEN as u32,
                device_writable: false,
            },
            Segment {
                addr: data.phys_addr(),
                len: data.len() as u32,
                device_writable: false,
            },
            Segment {
                addr: status.phys_addr(),
                len: PCM_STATUS_LEN as u32,
                device_writable: true,
            },
        ])
    }

    /// Reap a completed control command.
    pub fn poll_control(&mut self) -> Option<Used> {
        self.control.poll_used()
    }

    /// Reap a completed playback transfer.
    pub fn poll_tx(&mut self) -> Option<Used> {
        self.tx.poll_used()
    }

    /// Read the status word from a control response buffer.
    pub fn response_status<R: DmaRegion>(resp: &R) -> u32 {
        // SAFETY: a response region is at least 4 bytes.
        u32::from_le(unsafe { ptr::read_volatile(resp.cpu_ptr() as *const u32) })
    }
}
