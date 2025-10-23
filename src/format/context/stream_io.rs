use ffi;
use std::ffi::{c_int, c_void};
use std::io::{Read, Seek, SeekFrom, Write};
use Error;

/// Default internal I/O buffer size used by the underlying `AVIOContext`.
const BUFFER_SIZE: usize = 16384;

/// A safe Rust wrapper that creates an FFmpeg [`AVIOContext`] backed by any
/// Rust `Read` / `Write` / `Seek` stream.
///
/// This type allocates and owns an `AVIOContext` whose callbacks bridge to a
/// user-provided Rust stream. The stream is boxed and stored in the `opaque`
/// field of `AVIOContext` and is automatically dropped when `StreamIo` is
/// dropped.
///
/// # What this is for
///
/// FFmpeg allows you to supply custom I/O by passing an `AVIOContext` to
/// demuxers/muxers instead of a filename/URL. `StreamIo` lets you do that
/// with ordinary Rust I/O types like `File`, `Cursor<Vec<u8>>`, network
/// streams, etc.
///
/// # Ownership & lifetime
///
/// - `StreamIo` **owns** both the C `AVIOContext` and the boxed Rust stream.
/// - Dropping `StreamIo` frees the internal buffer, the `AVIOContext`,
///   and the boxed stream in the correct order.
/// - You must ensure the `AVIOContext*` returned by [`StreamIo::as_mut_ptr`]
///   does not outlive the `StreamIo` that created it.
///
/// # Thread-safety
///
/// The underlying Rust stream is not synchronized; callbacks are invoked
/// by FFmpeg on the calling thread. Do not share the same `StreamIo`
/// across threads unless the wrapped stream itself is thread-safe and FFmpeg
/// will not call the callbacks concurrently.
///
/// # EOF
///
/// - A `Read` that returns `Ok(0)` is translated to `AVERROR_EOF`.
///
/// # Safety notes
///
/// - `as_mut_ptr` exposes a raw `*mut AVIOContext` for integration with FFmpeg C APIs.
///   You must make sure this pointer does not outlive the `StreamIo` instance.
///
/// [`AVIOContext`]: https://ffmpeg.org/doxygen/trunk/structAVIOContext.html
pub struct StreamIo {
    ptr: *mut ffi::AVIOContext,
    drop_opaque: fn(*mut c_void),
}
impl StreamIo {
    pub fn from_read<T: Read>(stream: T) -> Result<Self, Error> {
        Self::new_impl(stream, Some(read::<T>), None, None)
    }
    pub fn from_read_seek<T: Read + Seek>(stream: T) -> Result<Self, Error> {
        Self::new_impl(stream, Some(read::<T>), None, Some(seek::<T>))
    }
    pub fn from_read_write_seek<T: Read + Write + Seek>(stream: T) -> Result<Self, Error> {
        Self::new_impl(stream, Some(read::<T>), Some(write::<T>), Some(seek::<T>))
    }
    pub fn from_read_write<T: Read + Write>(stream: T) -> Result<Self, Error> {
        Self::new_impl(stream, Some(read::<T>), Some(write::<T>), None)
    }
    pub fn from_write<T: Write>(stream: T) -> Result<Self, Error> {
        Self::new_impl(stream, None, Some(write::<T>), None)
    }
    pub fn from_write_seek<T: Write + Seek>(stream: T) -> Result<Self, Error> {
        Self::new_impl(stream, None, Some(write::<T>), Some(seek::<T>))
    }

    fn new_impl<T>(
        stream: T,
        r: Option<unsafe extern "C" fn(*mut c_void, *mut u8, c_int) -> c_int>,
        w: Option<unsafe extern "C" fn(*mut c_void, *const u8, c_int) -> c_int>,
        s: Option<unsafe extern "C" fn(*mut c_void, i64, c_int) -> i64>,
    ) -> Result<Self, Error> {
        let buffer = unsafe { ffi::av_malloc(BUFFER_SIZE) };
        if buffer.is_null() {
            return Err(Error::Other { errno: ffi::ENOMEM });
        }
        let stream_box_ptr = Box::into_raw(Box::new(stream)) as *mut c_void;
        let ptr = unsafe {
            ffi::avio_alloc_context(
                buffer as *mut _,
                BUFFER_SIZE as _,
                w.is_some() as _,
                stream_box_ptr,
                r,
                w,
                s,
            )
        };
        if ptr.is_null() {
            unsafe {
                drop(Box::from_raw(stream_box_ptr as *mut T));
            }
            return Err(Error::Other { errno: ffi::ENOMEM });
        }

        fn drop_box<T>(p: *mut c_void) {
            drop(unsafe { Box::from_raw(p as *mut T) });
        }
        Ok(Self {
            ptr,
            drop_opaque: drop_box::<T>,
        })
    }

    /// Returns a mutable raw pointer to the underlying `AVIOContext`.
    ///
    /// # Safety
    /// The returned pointer is owned by `self`. Do **not** free it or mutate its
    /// `buffer`/`opaque` fields directly. It must not outlive `self`.
    pub fn as_mut_ptr(&mut self) -> *mut ffi::AVIOContext {
        self.ptr
    }
}

impl Drop for StreamIo {
    fn drop(&mut self) {
        if !self.ptr.is_null() {
            unsafe {
                let opaque = (*self.ptr).opaque;
                ffi::av_freep(&raw mut (*self.ptr).buffer as *mut c_void);
                ffi::avio_context_free(&mut self.ptr);
                (self.drop_opaque)(opaque);
            }
        }
    }
}

impl std::fmt::Debug for StreamIo {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamIo").field("ptr", &self.ptr).finish()
    }
}

unsafe extern "C" fn read<T: Read>(opaque: *mut c_void, buf: *mut u8, buf_size: c_int) -> c_int {
    let buf = unsafe { std::slice::from_raw_parts_mut(buf, buf_size as usize) };
    let stream = unsafe { &mut *(opaque as *mut T) };
    match stream.read(buf) {
        Ok(0) => ffi::AVERROR_EOF,
        Ok(n) => n as c_int,
        Err(e) => map_io_error(e),
    }
}
unsafe extern "C" fn write<T: Write>(
    opaque: *mut c_void,
    buf: *const u8,
    buf_size: c_int,
) -> c_int {
    let buf = unsafe { std::slice::from_raw_parts(buf, buf_size as usize) };
    let stream = unsafe { &mut *(opaque as *mut T) };
    match stream.write(buf) {
        Ok(n) => n as c_int,
        Err(e) => map_io_error(e),
    }
}
unsafe extern "C" fn seek<T: Seek>(opaque: *mut c_void, offset: i64, whence: c_int) -> i64 {
    let stream = unsafe { &mut *(opaque as *mut T) };

    if whence == ffi::AVSEEK_SIZE {
        // Return stream size
        match stream.stream_position().and_then(|cur| {
            let end = stream.seek(SeekFrom::End(0))?;
            if cur != end {
                stream.seek(SeekFrom::Start(cur))?;
            }
            Ok(end)
        }) {
            Ok(sz) => return sz as i64,
            Err(_) => return ffi::AVERROR(ffi::ENOSYS) as i64,
        }
    }

    let pos = match whence {
        0 => SeekFrom::Start(offset as u64),
        1 => SeekFrom::Current(offset),
        2 => SeekFrom::End(offset),
        _ => return ffi::AVERROR(ffi::EINVAL) as i64,
    };
    match stream.seek(pos) {
        Ok(pos) => pos as i64,
        Err(_) => ffi::AVERROR(ffi::EIO) as i64,
    }
}

fn map_io_error(e: std::io::Error) -> i32 {
    use std::io::ErrorKind::*;
    match e.kind() {
        UnexpectedEof => ffi::AVERROR_EOF,
        Interrupted => ffi::AVERROR(ffi::EINTR),
        WouldBlock | TimedOut => ffi::AVERROR(ffi::EAGAIN),
        _ => ffi::AVERROR(ffi::EIO),
    }
}
