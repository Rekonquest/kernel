//! Scheme servers — the OS-facing glue that exposes a driver to the rest of
//! Redox.
//!
//! A network driver provides a `network:` scheme (read = receive a frame,
//! write = send one); a GPU driver provides a framebuffer scheme (mmap the
//! framebuffer, write = request a flip). This module sketches those servers and
//! the packet loop that backs them.
//!
//! These are **skeletons**, and they are the one part of this project that is
//! not verified in CI: they are compiled only on Redox, and the exact
//! `SchemeMut` / `Packet` surface can shift between `redox_syscall` versions, so
//! expect to adjust against the version you build with. The production loop also
//! multiplexes the scheme socket with the device IRQ through the `event:`
//! scheme, servicing whichever is ready; here the loop is shown standalone for
//! clarity.

#![cfg(target_os = "redox")]

use core::mem::size_of;
use syscall::{
    data::Packet,
    error::{Error, Result as SysResult, EWOULDBLOCK},
    flag::{O_CLOEXEC, O_CREAT, O_RDWR},
    open, read, write, SchemeMut,
};

/// `network:` scheme front-end. Reads pull the next received frame (fed from the
/// driver's `RxPool`); writes transmit a frame via the TX queue.
#[derive(Default)]
pub struct NetScheme {
    // TODO: wire to the driver — a queue of received frames and a TX submit path.
}

impl SchemeMut for NetScheme {
    fn open(&mut self, _path: &str, _flags: usize, _uid: u32, _gid: u32) -> SysResult<usize> {
        Ok(0)
    }
    fn read(&mut self, _id: usize, _buf: &mut [u8]) -> SysResult<usize> {
        // TODO: copy the next received frame into `buf`; block (EWOULDBLOCK
        // under O_NONBLOCK) when none are pending.
        Err(Error::new(EWOULDBLOCK))
    }
    fn write(&mut self, _id: usize, buf: &[u8]) -> SysResult<usize> {
        // TODO: transmit `buf` via VirtioNet::transmit; for now accept it.
        Ok(buf.len())
    }
    fn close(&mut self, _id: usize) -> SysResult<usize> {
        Ok(0)
    }
}

/// A framebuffer scheme front-end. Clients mmap the framebuffer and request a
/// flip by writing; the daemon calls `VirtioGpu::present` in response.
#[derive(Default)]
pub struct FbScheme {
    // TODO: wire to the driver — the framebuffer region and a present trigger.
}

impl SchemeMut for FbScheme {
    fn open(&mut self, _path: &str, _flags: usize, _uid: u32, _gid: u32) -> SysResult<usize> {
        Ok(0)
    }
    fn write(&mut self, _id: usize, buf: &[u8]) -> SysResult<usize> {
        // TODO: interpret as a flip request and present the framebuffer.
        Ok(buf.len())
    }
    fn close(&mut self, _id: usize) -> SysResult<usize> {
        Ok(0)
    }
}

/// Register the scheme `name` (e.g. `"network"`) and run its packet loop until
/// the socket closes. Production drivers multiplex this with the device IRQ via
/// the `event:` scheme instead of blocking here.
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
