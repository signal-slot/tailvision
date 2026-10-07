//! A minimal V4L2 capture layer: just the handful of ioctls needed to grab a
//! frame with memory-mapped buffers. Hand-written so the crate cross-compiles
//! without libclang/bindgen.
//!
//! Struct layouts follow `linux/videodev2.h`. They only have to be right for
//! little-endian 32-bit ARM (EABI) and x86_64, which is what we build for;
//! the size assertions below guard the ioctl numbers, which encode sizeof.

use std::ffi::CStr;
use std::fmt;
use std::fs::{File, OpenOptions};
use std::io;
use std::mem::{size_of, zeroed};
use std::os::unix::io::AsRawFd;
use std::ptr;
use std::time::Duration;

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct FourCC(pub [u8; 4]);

impl FourCC {
    pub const fn code(self) -> u32 {
        u32::from_le_bytes(self.0)
    }
}

impl fmt::Display for FourCC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for &b in &self.0 {
            f.write_str(if b.is_ascii_graphic() {
                std::str::from_utf8(std::slice::from_ref(&b)).unwrap()
            } else {
                "?"
            })?;
        }
        Ok(())
    }
}

impl fmt::Debug for FourCC {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "FourCC({self})")
    }
}

const BUF_TYPE_VIDEO_CAPTURE: u32 = 1;
const MEMORY_MMAP: u32 = 1;
const FRMSIZE_TYPE_DISCRETE: u32 = 1;

#[repr(C)]
struct FmtDesc {
    index: u32,
    typ: u32,
    flags: u32,
    description: [u8; 32],
    pixelformat: u32,
    mbus_code: u32,
    reserved: [u32; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrmSizeDiscrete {
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct FrmSizeStepwise {
    min_width: u32,
    max_width: u32,
    step_width: u32,
    min_height: u32,
    max_height: u32,
    step_height: u32,
}

#[repr(C)]
union FrmSizeUnion {
    discrete: FrmSizeDiscrete,
    stepwise: FrmSizeStepwise,
}

#[repr(C)]
struct FrmSizeEnum {
    index: u32,
    pixel_format: u32,
    typ: u32,
    size: FrmSizeUnion,
    reserved: [u32; 2],
}

#[repr(C)]
#[derive(Clone, Copy, Default)]
pub struct PixFormat {
    pub width: u32,
    pub height: u32,
    pub pixelformat: u32,
    pub field: u32,
    pub bytesperline: u32,
    pub sizeimage: u32,
    pub colorspace: u32,
    pub private: u32,
    pub flags: u32,
    pub ycbcr_enc: u32,
    pub quantization: u32,
    pub xfer_func: u32,
}

#[repr(C)]
union FormatUnion {
    pix: PixFormat,
    raw_data: [u8; 200],
    /// `v4l2_window` holds pointers, so the union is pointer-aligned.
    _align: usize,
}

#[repr(C)]
struct Format {
    typ: u32,
    fmt: FormatUnion,
}

#[repr(C)]
struct RequestBuffers {
    count: u32,
    typ: u32,
    memory: u32,
    capabilities: u32,
    flags: u8,
    reserved: [u8; 3],
}

#[repr(C)]
#[derive(Clone, Copy)]
struct Timecode {
    typ: u32,
    flags: u32,
    frames: u8,
    seconds: u8,
    minutes: u8,
    hours: u8,
    userbits: [u8; 4],
}

#[repr(C)]
union BufferM {
    offset: u32,
    userptr: usize,
    fd: i32,
}

#[repr(C)]
struct Buffer {
    index: u32,
    typ: u32,
    bytesused: u32,
    flags: u32,
    field: u32,
    /// `struct timeval` with 64-bit time_t: 16 bytes, 8-aligned on every
    /// target we build for. We never read it.
    timestamp: [u64; 2],
    timecode: Timecode,
    sequence: u32,
    memory: u32,
    m: BufferM,
    length: u32,
    reserved2: u32,
    request_fd: i32,
}

const _: () = {
    assert!(size_of::<FmtDesc>() == 64);
    assert!(size_of::<FrmSizeEnum>() == 44);
    assert!(size_of::<PixFormat>() == 48);
    assert!(size_of::<Format>() == 4 + size_of::<usize>().saturating_sub(4) + 200);
    assert!(size_of::<RequestBuffers>() == 20);
    assert!(size_of::<Buffer>() == if size_of::<usize>() == 8 { 88 } else { 80 });
};

const fn ioc(dir: u32, nr: u32, size: usize) -> u32 {
    (dir << 30) | ((size as u32) << 16) | (b'V' as u32) << 8 | nr
}
const IOC_WRITE: u32 = 1;
const IOC_READ: u32 = 2;
const fn iow(nr: u32, size: usize) -> u32 {
    ioc(IOC_WRITE, nr, size)
}
const fn iowr(nr: u32, size: usize) -> u32 {
    ioc(IOC_READ | IOC_WRITE, nr, size)
}

const VIDIOC_ENUM_FMT: u32 = iowr(2, size_of::<FmtDesc>());
const VIDIOC_S_FMT: u32 = iowr(5, size_of::<Format>());
const VIDIOC_REQBUFS: u32 = iowr(8, size_of::<RequestBuffers>());
const VIDIOC_QUERYBUF: u32 = iowr(9, size_of::<Buffer>());
const VIDIOC_QBUF: u32 = iowr(15, size_of::<Buffer>());
const VIDIOC_DQBUF: u32 = iowr(17, size_of::<Buffer>());
const VIDIOC_STREAMON: u32 = iow(18, size_of::<i32>());
const VIDIOC_STREAMOFF: u32 = iow(19, size_of::<i32>());
const VIDIOC_ENUM_FRAMESIZES: u32 = iowr(74, size_of::<FrmSizeEnum>());

/// Issues an ioctl, retrying on EINTR.
unsafe fn xioctl<T>(fd: i32, req: u32, arg: *mut T) -> io::Result<()> {
    loop {
        // SAFETY: the caller passes a pointer to a properly sized struct for `req`.
        let r = unsafe { libc::ioctl(fd, req as _, arg) };
        if r >= 0 {
            return Ok(());
        }
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(libc::EINTR) {
            return Err(err);
        }
    }
}

pub struct FormatDescription {
    pub fourcc: FourCC,
    pub description: String,
}

pub struct Device {
    file: File,
}

impl Device {
    pub fn open(path: &str) -> io::Result<Self> {
        let file = OpenOptions::new().read(true).write(true).open(path)?;
        Ok(Self { file })
    }

    fn fd(&self) -> i32 {
        self.file.as_raw_fd()
    }

    pub fn enum_formats(&self) -> io::Result<Vec<FormatDescription>> {
        let mut out = Vec::new();
        for index in 0.. {
            // SAFETY: all-zero is a valid FmtDesc.
            let mut desc: FmtDesc = unsafe { zeroed() };
            desc.index = index;
            desc.typ = BUF_TYPE_VIDEO_CAPTURE;
            // SAFETY: desc matches the ioctl's argument type.
            match unsafe { xioctl(self.fd(), VIDIOC_ENUM_FMT, &mut desc) } {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
            let description = CStr::from_bytes_until_nul(&desc.description)
                .map(|s| s.to_string_lossy().into_owned())
                .unwrap_or_default();
            out.push(FormatDescription {
                fourcc: FourCC(desc.pixelformat.to_le_bytes()),
                description,
            });
        }
        Ok(out)
    }

    /// Discrete frame sizes for a pixel format. Stepwise/continuous ranges are
    /// reported by their maximum.
    pub fn enum_framesizes(&self, fourcc: FourCC) -> io::Result<Vec<(u32, u32)>> {
        let mut out = Vec::new();
        for index in 0.. {
            // SAFETY: all-zero is a valid FrmSizeEnum.
            let mut fs: FrmSizeEnum = unsafe { zeroed() };
            fs.index = index;
            fs.pixel_format = fourcc.code();
            // SAFETY: fs matches the ioctl's argument type.
            match unsafe { xioctl(self.fd(), VIDIOC_ENUM_FRAMESIZES, &mut fs) } {
                Ok(()) => {}
                Err(e) if e.raw_os_error() == Some(libc::EINVAL) => break,
                Err(e) => return Err(e),
            }
            // SAFETY: the driver filled whichever union member `typ` names.
            unsafe {
                if fs.typ == FRMSIZE_TYPE_DISCRETE {
                    out.push((fs.size.discrete.width, fs.size.discrete.height));
                } else {
                    out.push((fs.size.stepwise.max_width, fs.size.stepwise.max_height));
                    break;
                }
            }
        }
        Ok(out)
    }

    /// Requests a capture format and returns what the driver actually chose.
    pub fn set_format(&self, width: u32, height: u32, fourcc: FourCC) -> io::Result<PixFormat> {
        // SAFETY: all-zero is a valid Format.
        let mut f: Format = unsafe { zeroed() };
        f.typ = BUF_TYPE_VIDEO_CAPTURE;
        f.fmt.pix = PixFormat {
            width,
            height,
            pixelformat: fourcc.code(),
            ..Default::default()
        };
        // SAFETY: f matches the ioctl's argument type.
        unsafe { xioctl(self.fd(), VIDIOC_S_FMT, &mut f)? };
        // SAFETY: for a capture type the driver fills `pix`.
        Ok(unsafe { f.fmt.pix })
    }

    pub fn start_stream(&self, buffer_count: u32) -> io::Result<Stream<'_>> {
        let mut req = RequestBuffers {
            count: buffer_count,
            typ: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        // SAFETY: req matches the ioctl's argument type.
        unsafe { xioctl(self.fd(), VIDIOC_REQBUFS, &mut req)? };
        if req.count == 0 {
            return Err(io::Error::other("driver granted no buffers"));
        }

        let mut stream = Stream {
            dev: self,
            maps: Vec::new(),
            streaming: false,
        };
        for index in 0..req.count {
            let mut buf = new_buffer(index);
            // SAFETY: buf matches the ioctl's argument type.
            unsafe { xioctl(self.fd(), VIDIOC_QUERYBUF, &mut buf)? };
            let len = buf.length as usize;
            // SAFETY: the driver reports a valid mmap offset for this buffer.
            let p = unsafe {
                libc::mmap(
                    ptr::null_mut(),
                    len,
                    libc::PROT_READ | libc::PROT_WRITE,
                    libc::MAP_SHARED,
                    self.fd(),
                    buf.m.offset as libc::off_t,
                )
            };
            if p == libc::MAP_FAILED {
                return Err(io::Error::last_os_error());
            }
            stream.maps.push((p as *mut u8, len));
            let mut buf = new_buffer(index);
            // SAFETY: buf matches the ioctl's argument type.
            unsafe { xioctl(self.fd(), VIDIOC_QBUF, &mut buf)? };
        }

        let mut typ = BUF_TYPE_VIDEO_CAPTURE as i32;
        // SAFETY: STREAMON takes a pointer to the buffer type.
        unsafe { xioctl(self.fd(), VIDIOC_STREAMON, &mut typ)? };
        stream.streaming = true;
        Ok(stream)
    }
}

fn new_buffer(index: u32) -> Buffer {
    // SAFETY: all-zero is a valid Buffer.
    let mut buf: Buffer = unsafe { zeroed() };
    buf.index = index;
    buf.typ = BUF_TYPE_VIDEO_CAPTURE;
    buf.memory = MEMORY_MMAP;
    buf
}

pub struct Stream<'a> {
    dev: &'a Device,
    maps: Vec<(*mut u8, usize)>,
    streaming: bool,
}

impl Stream<'_> {
    /// Waits for the next frame and hands its bytes to `f`, then re-queues the buffer.
    pub fn next_frame<R>(
        &mut self,
        timeout: Duration,
        f: impl FnOnce(&[u8]) -> R,
    ) -> io::Result<R> {
        let mut pfd = libc::pollfd {
            fd: self.dev.fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        let ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
        loop {
            // SAFETY: pfd is a valid pollfd array of length 1.
            let r = unsafe { libc::poll(&mut pfd, 1, ms) };
            if r > 0 {
                break;
            }
            if r == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "timed out waiting for a frame",
                ));
            }
            let err = io::Error::last_os_error();
            if err.raw_os_error() != Some(libc::EINTR) {
                return Err(err);
            }
        }

        let mut buf = new_buffer(0);
        // SAFETY: buf matches the ioctl's argument type.
        unsafe { xioctl(self.dev.fd(), VIDIOC_DQBUF, &mut buf)? };
        let (p, len) = self.maps[buf.index as usize];
        let used = (buf.bytesused as usize).min(len);
        // SAFETY: the mapping is `len` bytes long and stays valid until munmap in Drop.
        let data = unsafe { std::slice::from_raw_parts(p, used) };
        let result = f(data);
        // SAFETY: buf still describes the buffer we just dequeued.
        unsafe { xioctl(self.dev.fd(), VIDIOC_QBUF, &mut buf)? };
        Ok(result)
    }
}

impl Drop for Stream<'_> {
    fn drop(&mut self) {
        if self.streaming {
            let mut typ = BUF_TYPE_VIDEO_CAPTURE as i32;
            // SAFETY: STREAMOFF takes a pointer to the buffer type.
            let _ = unsafe { xioctl(self.dev.fd(), VIDIOC_STREAMOFF, &mut typ) };
        }
        for &(p, len) in &self.maps {
            // SAFETY: each mapping came from mmap with exactly this length.
            unsafe { libc::munmap(p as *mut libc::c_void, len) };
        }
        // Release the driver's buffers so the next open can request its own.
        let mut req = RequestBuffers {
            count: 0,
            typ: BUF_TYPE_VIDEO_CAPTURE,
            memory: MEMORY_MMAP,
            capabilities: 0,
            flags: 0,
            reserved: [0; 3],
        };
        // SAFETY: req matches the ioctl's argument type.
        let _ = unsafe { xioctl(self.dev.fd(), VIDIOC_REQBUFS, &mut req) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_numbers_match_the_kernel_headers() {
        // Values from <linux/videodev2.h> on x86_64 / 32-bit ARM.
        if size_of::<usize>() == 8 {
            assert_eq!(VIDIOC_S_FMT, 0xc0d0_5605);
            assert_eq!(VIDIOC_DQBUF, 0xc058_5611);
        } else {
            assert_eq!(VIDIOC_S_FMT, 0xc0cc_5605);
            assert_eq!(VIDIOC_DQBUF, 0xc050_5611);
        }
        assert_eq!(VIDIOC_ENUM_FMT, 0xc040_5602);
        assert_eq!(VIDIOC_REQBUFS, 0xc014_5608);
        assert_eq!(VIDIOC_STREAMON, 0x4004_5612);
        assert_eq!(VIDIOC_ENUM_FRAMESIZES, 0xc02c_564a);
    }

    #[test]
    fn fourcc_roundtrip() {
        let f = FourCC(*b"MJPG");
        assert_eq!(f.to_string(), "MJPG");
        assert_eq!(FourCC(f.code().to_le_bytes()), f);
    }
}
