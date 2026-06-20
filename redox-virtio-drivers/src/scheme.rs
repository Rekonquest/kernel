//! Scheme servers — the OS-facing glue that exposes a driver to the rest of
//! Redox, wired to the kit's data paths.
//!
//! A network driver provides a `network:` scheme (read = receive a frame,
//! write = send one); a GPU driver provides a framebuffer scheme (write =
//! request a flip). The scheme types here hold the queues that connect client
//! I/O to the driver's data path:
//!
//! - the driver's IRQ handler calls [`NetScheme::deliver`] with each frame the
//!   `RxPool` produced; a client `read` pops one;
//! - a client `write` enqueues a frame, and the driver drains
//!   [`NetScheme::take_tx`] and hands each to `VirtioNet::transmit`.
//!
//! In production the driver runs **one event loop** that multiplexes the scheme
//! socket and the device IRQ via the `event:` scheme, servicing whichever is
//! ready (the standalone [`serve`] loop below shows the scheme half; see
//! `BOOTING.md` for the multiplexed shape).
//!
//! This module is compiled only on Redox; CI cross-compiles it via `redoxer`,
//! so the wiring is type-checked even though it can only *run* on a boot.

#![cfg(target_os = "redox")]

use std::collections::VecDeque;
use std::mem::size_of;

use syscall::{
    data::Packet,
    error::{Error, Result as SysResult, EWOULDBLOCK},
    flag::{O_CLOEXEC, O_CREAT, O_RDWR},
    open, read, write, SchemeMut,
};

/// `network:` scheme front-end, holding the received and to-transmit frame
/// queues that bridge clients and the driver.
#[derive(Default)]
pub struct NetScheme {
    rx: VecDeque<Vec<u8>>,
    tx: VecDeque<Vec<u8>>,
}

impl NetScheme {
    /// Driver → scheme: a frame arrived (from the `RxPool`); make it available
    /// to readers.
    pub fn deliver(&mut self, frame: Vec<u8>) {
        self.rx.push_back(frame);
    }

    /// Scheme → driver: the next frame a client asked to transmit, if any. The
    /// driver hands it to `VirtioNet::transmit`.
    pub fn take_tx(&mut self) -> Option<Vec<u8>> {
        self.tx.pop_front()
    }

    /// Whether any client frames are waiting to be transmitted.
    pub fn has_tx(&self) -> bool {
        !self.tx.is_empty()
    }
}

impl SchemeMut for NetScheme {
    fn open(&mut self, _path: &str, _flags: usize, _uid: u32, _gid: u32) -> SysResult<usize> {
        Ok(0)
    }

    fn read(&mut self, _id: usize, buf: &mut [u8]) -> SysResult<usize> {
        match self.rx.pop_front() {
            Some(frame) => {
                let n = frame.len().min(buf.len());
                buf[..n].copy_from_slice(&frame[..n]);
                Ok(n)
            }
            // No frame pending; under O_NONBLOCK the client retries.
            None => Err(Error::new(EWOULDBLOCK)),
        }
    }

    fn write(&mut self, _id: usize, buf: &[u8]) -> SysResult<usize> {
        self.tx.push_back(buf.to_vec());
        Ok(buf.len())
    }

    fn close(&mut self, _id: usize) -> SysResult<usize> {
        Ok(0)
    }
}

/// Framebuffer scheme front-end. A client write requests a flip; the driver
/// drains [`FbScheme::take_flips`] and calls `VirtioGpu::present`.
#[derive(Default)]
pub struct FbScheme {
    flips_requested: usize,
}

impl FbScheme {
    /// How many flips clients have requested since the last drain.
    pub fn take_flips(&mut self) -> usize {
        core::mem::take(&mut self.flips_requested)
    }
}

impl SchemeMut for FbScheme {
    fn open(&mut self, _path: &str, _flags: usize, _uid: u32, _gid: u32) -> SysResult<usize> {
        Ok(0)
    }

    fn write(&mut self, _id: usize, buf: &[u8]) -> SysResult<usize> {
        // Any write is a flip request. A full driver also supports mmap of the
        // framebuffer so clients draw directly.
        self.flips_requested += 1;
        Ok(buf.len())
    }

    fn close(&mut self, _id: usize) -> SysResult<usize> {
        Ok(0)
    }
}

/// Register the scheme `name` (e.g. `"network"`) and run its packet loop until
/// the socket closes. This drives only the scheme half; a real driver multiplexes
/// this socket with the device IRQ via the `event:` scheme (see `BOOTING.md`).
pub fn serve(name: &str, scheme: &mut impl SchemeMut) -> SysResult<()> {
    let socket = open(format!(":{name}"), O_RDWR | O_CREAT | O_CLOEXEC)?;
    loop {
        let mut packet = Packet::default();
        // A Packet is a fixed C struct; read/write it as raw bytes.
        let got = {
            let bytes = unsafe {
                core::slice::from_raw_parts_mut(
                    &mut packet as *mut Packet as *mut u8,
                    size_of::<Packet>(),
                )
            };
            read(socket, bytes)?
        };
        if got == 0 {
            return Ok(());
        }
        scheme.handle(&mut packet);
        let bytes = unsafe {
            core::slice::from_raw_parts(&packet as *const Packet as *const u8, size_of::<Packet>())
        };
        write(socket, bytes)?;
    }
}
