use super::StreamIo;
use ffi::*;

#[derive(Debug)]
pub enum Mode {
    Input,
    Output,
    InputCustomIo(StreamIo),
    OutputCustomIo(StreamIo),
}

pub struct Destructor {
    ptr: *mut AVFormatContext,
    mode: Mode,
}

impl Destructor {
    pub unsafe fn new(ptr: *mut AVFormatContext, mode: Mode) -> Self {
        Destructor { ptr, mode }
    }
}

impl Drop for Destructor {
    fn drop(&mut self) {
        unsafe {
            match self.mode {
                Mode::InputCustomIo(ref _io) => {
                    avformat_close_input(&mut self.ptr);
                    // Custom io will just be dropped here
                }
                Mode::OutputCustomIo(ref _io) => {
                    avformat_free_context(self.ptr);
                    // Custom io will just be dropped here
                }
                Mode::Input => avformat_close_input(&mut self.ptr),

                Mode::Output => {
                    avio_close((*self.ptr).pb);
                    avformat_free_context(self.ptr);
                }
            }
        }
    }
}
