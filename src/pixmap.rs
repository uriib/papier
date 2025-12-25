use std::{
    os::fd::{AsFd, BorrowedFd},
    ptr::NonNull,
};

use rustix::mm::{MapFlags, ProtFlags};
use wayland_client::{
    Dispatch, QueueHandle,
    protocol::{
        wl_buffer::WlBuffer,
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
    },
};

#[derive(Debug)]
pub struct Pixmap {
    size: [u32; 2],
    data: Option<NonNull<u32>>,
}

impl Default for Pixmap {
    fn default() -> Self {
        Self {
            size: [0, 0],
            data: None,
        }
    }
}

impl Pixmap {
    pub fn map(size: [u32; 2], fd: impl AsFd) -> Self {
        let [width, height] = size.map(|x| x as usize);
        let len = width * height * size_of::<u32>();
        rustix::fs::ftruncate(&fd, len as _).unwrap();
        let data = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                len,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
            .unwrap()
        };
        Self {
            size,
            data: NonNull::new(data as _),
        }
    }
    pub fn is_empty(&self) -> bool {
        self.size.iter().any(|&x| x == 0)
    }
    pub fn width(&self) -> u32 {
        self.size[0]
    }
    pub fn height(&self) -> u32 {
        self.size[1]
    }
    pub fn stride(&self) -> u32 {
        self.width() * 4
    }
    pub fn byte_size(&self) -> u32 {
        self.stride() * self.height()
    }
    pub fn pixels_mut(&self) -> &mut [u32] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.data.unwrap().as_ptr() as _,
                (self.width() * self.height()) as _,
            )
        }
    }
    fn unmap(&self) {
        if let Some(ptr) = self.data {
            unsafe { rustix::mm::munmap(ptr.as_ptr() as _, self.byte_size() as _).unwrap() };
        }
    }
    pub fn pixel_mut(&self, x: u32, y: u32) -> &mut u32 {
        &mut self.pixels_mut()[(y * self.width() + x) as usize]
    }
    pub fn shm_buffer<T>(&self, shm: &WlShm, fd: BorrowedFd, qh: &QueueHandle<T>) -> WlBuffer
    where
        T: Dispatch<WlShmPool, ()> + Dispatch<WlBuffer, ()> + 'static,
    {
        let pool = shm.create_pool(fd, self.byte_size() as _, qh, ());
        let buffer = pool.create_buffer(
            0,
            self.width() as _,
            self.height() as _,
            self.stride() as _,
            Format::Xrgb8888,
            qh,
            (),
        );
        pool.destroy();
        buffer
    }
}

impl Drop for Pixmap {
    fn drop(&mut self) {
        self.unmap();
    }
}
