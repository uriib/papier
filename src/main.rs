use std::{
    env,
    ffi::CStr,
    fs::{self, File},
    io::{BufReader, Cursor},
    os::fd::AsFd as _,
    path::{Path, PathBuf},
    sync::Arc,
    time::Duration,
};

use anyhow::Context as _;
use bytes::Bytes;
use chrono::{DateTime, Days, Local, NaiveTime};
use image::{DynamicImage, GenericImageView, codecs::jpeg::JpegDecoder};
use rustix::{
    fs::{Mode, OFlags},
    mm::{MapFlags, ProtFlags},
};
use tokio::{
    io::unix::AsyncFd,
    sync::{
        Mutex,
        mpsc::{self, Receiver, Sender},
    },
    time::MissedTickBehavior,
};
use wayland_client::{
    Connection, Dispatch, QueueHandle, WEnum, delegate_noop,
    protocol::{
        wl_buffer::WlBuffer,
        wl_compositor::WlCompositor,
        wl_pointer::{self, WlPointer},
        wl_registry::{self, WlRegistry},
        wl_seat::{self, Capability, WlSeat},
        wl_shm::{Format, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::{self, WlSurface},
    },
};
use wayland_protocols::wp::cursor_shape::v1::client::{
    wp_cursor_shape_device_v1::{Shape, WpCursorShapeDeviceV1},
    wp_cursor_shape_manager_v1::WpCursorShapeManagerV1,
};
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1::{Layer, ZwlrLayerShellV1},
    zwlr_layer_surface_v1::{self, Anchor, ZwlrLayerSurfaceV1},
};

const APP_NAME: &str = "papier";
const SHM_PATH: &CStr = c"/dev/shm/papier";

#[derive(Clone)]
struct Client {
    cache_path: Arc<PathBuf>,
    globals: Globals,
    window: Option<Window>,
    cursor_shape_device: Option<WpCursorShapeDeviceV1>,
    source: Arc<Mutex<Option<DynamicImage>>>,
    release_source: Sender<()>,
    next_fetch: DateTime<Local>,
}

fn cache_path() -> PathBuf {
    env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let home = env::var_os("HOME").unwrap();
            PathBuf::from(home).join(".cache")
        })
        .join(APP_NAME)
        .join("cache")
}

impl Client {
    fn new(release_source: Sender<()>) -> Self {
        let cache_path = cache_path();
        Self {
            cache_path: Arc::new(cache_path),
            globals: Globals::default(),
            window: None,
            cursor_shape_device: None,
            source: Default::default(),
            release_source,
            next_fetch: Local::now(),
        }
    }
    fn scale(&mut self, scale: u32, qh: &QueueHandle<Self>) {
        if Local::now() <= self.next_fetch {
            return;
        }
        let window = self.window.as_mut().unwrap();
        {
            let mut pixmap = window.pixmap.try_lock().unwrap();
            if scale == pixmap.scale {
                return;
            }
            window.surface.set_buffer_scale(scale as _);
            pixmap.unmap();
            pixmap.scale = scale;
        }
        window.allocate_buffer(self.globals.shm(), qh);
        self.reload();
        self.next_fetch = self
            .next_fetch
            .with_time(NaiveTime::default())
            .unwrap()
            .checked_add_days(Days::new(1))
            .unwrap();
    }
}

#[derive(Clone)]
struct Window {
    surface: WlSurface,
    pixmap: Arc<Mutex<Pixmap>>,
    buffer: Arc<Mutex<Option<WlBuffer>>>,
}
impl Window {
    fn new(
        compositor: &WlCompositor,
        layer_shell: &ZwlrLayerShellV1,
        qh: &QueueHandle<Client>,
    ) -> Self {
        let surface = compositor.create_surface(qh, ());
        let layer_surface = layer_shell.get_layer_surface(
            &surface,
            None,
            Layer::Background,
            APP_NAME.into(),
            qh,
            (),
        );
        layer_surface.set_exclusive_zone(-1);
        layer_surface.set_anchor(Anchor::all());
        surface.commit();

        Window {
            surface,
            pixmap: Default::default(),
            buffer: Default::default(),
        }
    }
    fn configure(&mut self, width: u32, height: u32, shm: &WlShm, qh: &QueueHandle<Client>) {
        {
            let mut pixmap = self.pixmap.try_lock().unwrap();
            pixmap.unmap();
            pixmap.width = width;
            pixmap.height = height;
        }
        self.allocate_buffer(shm, qh);
    }
    fn allocate_buffer(&mut self, shm: &WlShm, qh: &QueueHandle<Client>) {
        let fd = rustix::fs::open(
            SHM_PATH,
            OFlags::CREATE | OFlags::RDWR,
            Mode::from_raw_mode(0o600),
        )
        .unwrap();

        let mut pixmap = self.pixmap.try_lock().unwrap();
        rustix::fs::ftruncate(&fd, pixmap.byte_size() as _).unwrap();
        let data = unsafe {
            rustix::mm::mmap(
                std::ptr::null_mut(),
                pixmap.byte_size() as _,
                ProtFlags::READ | ProtFlags::WRITE,
                MapFlags::SHARED,
                &fd,
                0,
            )
            .unwrap()
        };
        let data = NonNull::new(data as _);
        rustix::fs::unlink(SHM_PATH).unwrap();

        let pool = shm.create_pool(fd.as_fd(), pixmap.byte_size() as _, qh, ());
        let buffer = pool.create_buffer(
            0,
            pixmap.buffer_width() as _,
            pixmap.buffer_height() as _,
            pixmap.buffer_stride() as _,
            Format::Xrgb8888,
            qh,
            (),
        );
        pool.destroy();

        pixmap.data = data;
        self.buffer
            .try_lock()
            .unwrap()
            .replace(buffer)
            .as_ref()
            .map(WlBuffer::destroy);
    }

    fn render(&self, source: &Option<DynamicImage>) {
        let [width, height] = {
            let pixmap = self.pixmap.try_lock().unwrap();
            let (width, height) = (pixmap.buffer_width(), pixmap.buffer_height());
            if let Some(source) = source {
                for y in 0..height {
                    for x in 0..width {
                        let dest = pixmap.pixel_mut(x, y);
                        let x = x * source.width() / width;
                        let y = y * source.height() / height;
                        let [r, g, b, _] = source.get_pixel(x, y).0.map(|x| x as u32);
                        *dest = (r << 16) | (g << 8) | b;
                    }
                }
            } else {
                for pixel in pixmap.pixels_mut() {
                    *pixel = 0x93a7c9;
                }
            }
            [width, height].map(|x| x as _)
        };
        self.surface.attach(
            Some(self.buffer.try_lock().unwrap().as_ref().unwrap()),
            0,
            0,
        );
        self.surface.damage_buffer(0, 0, width, height);
        self.surface.commit();
    }
}

async fn try_fetch() -> anyhow::Result<Bytes> {
    reqwest::get("https://bing.biturl.top?format=image&resolution=UHD")
        .await
        .context("Failed to reload image")?
        .bytes()
        .await
        .context("Failed to read image bytes")
}

fn decode_image(bytes: Bytes) -> anyhow::Result<DynamicImage> {
    let reader = Cursor::new(bytes);
    let decoder = JpegDecoder::new(reader).context("Failed to create decoder")?;
    let img = DynamicImage::from_decoder(decoder).context("Failed to decode image")?;
    Ok(img)
}

fn fetch(
    cache_path: impl AsRef<Path>,
    retry: usize,
) -> impl Future<Output = anyhow::Result<DynamicImage>> {
    Box::pin(async move {
        let cache_path = cache_path.as_ref();
        if retry == 0 {
            anyhow::bail!("failed to fetch image");
        }
        match try_fetch().await {
            Ok(bytes) => match decode_image(bytes.clone()) {
                Ok(img) => {
                    fs::create_dir_all(cache_path.parent().unwrap())
                        .and_then(|_| fs::write(cache_path, bytes))
                        .inspect_err(|e| log::warn!("failed to cache image: {e:?}"))
                        .ok();
                    Ok(img)
                }
                Err(_) => fetch(cache_path, retry - 1).await,
            },
            Err(e) => {
                log::warn!("{e:?}");
                tokio::time::sleep(Duration::new(1, 0)).await;
                fetch(cache_path, retry - 1).await
            }
        }
    })
}

impl Client {
    fn load_cache(&mut self) -> anyhow::Result<()> {
        if self.source.try_lock().unwrap().is_some() {
            return Ok(());
        }
        let reader = BufReader::new(File::open(self.cache_path.as_ref())?);
        let decoder = JpegDecoder::new(reader)?;
        let img = DynamicImage::from_decoder(decoder)?;
        *self.source.try_lock().unwrap() = Some(img);
        Ok(())
    }
    fn reload(&mut self) {
        let loaded = self.load_cache().ok();
        self.window
            .as_ref()
            .unwrap()
            .render(&&self.source.try_lock().unwrap());
        if loaded.is_some() {
            self.release_source.try_send(()).ok();
        }
    }
    async fn on_tick(&mut self) {
        match fetch(self.cache_path.as_ref(), 10).await {
            Ok(image) => {
                *self.source.lock().await = Some(image);
                self.reload();
            }
            Err(err) => log::error!("{err:?}"),
        };
    }
    fn run(mut self, mut release_source_msg: Receiver<()>) -> impl Future<Output = ()> {
        let connection = Connection::connect_to_env().unwrap();
        let mut queue = connection.new_event_queue();
        let qh = queue.handle();
        let display = connection.display();
        display.get_registry(&qh, ());
        queue.roundtrip(&mut self).unwrap();

        self.window = Some(Window::new(
            self.globals.compositor(),
            self.globals.layer_shell_v1(),
            &qh,
        ));
        self.globals.layer_shell_v1().destroy();
        queue.blocking_dispatch(&mut self).unwrap();

        let connection_shared = connection;

        let mut client = self.clone();
        let connection = connection_shared.clone();
        let wayland = async move {
            loop {
                let dispatched = queue.dispatch_pending(&mut client).unwrap();
                if dispatched > 0 {
                    continue;
                }

                connection.flush().unwrap();

                if let Some(guard) = connection.prepare_read() {
                    let _ = AsyncFd::new(guard.connection_fd())
                        .unwrap()
                        .readable()
                        .await
                        .unwrap();
                    _ = guard.read();
                }

                queue.dispatch_pending(&mut client).unwrap();
            }
        };

        let connection = connection_shared;
        let mut client = self.clone();
        let timeout = async move {
            let mut timer = tokio::time::interval(Duration::from_mins(15));
            timer.set_missed_tick_behavior(MissedTickBehavior::Skip);
            loop {
                timer.tick().await;
                client.on_tick().await;
                connection.flush().unwrap();
            }
        };

        let source = self.source;
        let release_source = async move {
            loop {
                release_source_msg.recv().await;
                tokio::time::sleep(Duration::new(10, 0)).await;
                *source.lock().await = None;
            }
        };

        async {
            tokio::join!(timeout, wayland, release_source);
        }
    }
}

impl Dispatch<ZwlrLayerSurfaceV1, ()> for Client {
    fn event(
        client: &mut Self,
        layer_surface: &ZwlrLayerSurfaceV1,
        event: <ZwlrLayerSurfaceV1 as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                layer_surface.ack_configure(serial);
                client
                    .window
                    .as_mut()
                    .unwrap()
                    .configure(width, height, client.globals.shm(), qh);
                client.reload();
            }
            zwlr_layer_surface_v1::Event::Closed => {}
            _ => {}
        }
    }
}

impl Dispatch<WlRegistry, ()> for Client {
    fn event(
        client: &mut Self,
        registry: &WlRegistry,
        event: <WlRegistry as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_registry::Event::Global {
                name,
                interface,
                version,
            } => {
                if client.globals.bind(registry, name, &interface, version, qh) {
                    return;
                }
                match interface.as_str() {
                    "wl_seat" => {
                        let _: WlSeat = registry.bind(name, version, qh, ());
                    }
                    _ => {}
                }
            }
            wl_registry::Event::GlobalRemove { .. } => {}
            _ => {}
        }
    }
}

impl Dispatch<WlSurface, ()> for Client {
    fn event(
        client: &mut Self,
        _: &WlSurface,
        event: <WlSurface as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_surface::Event::PreferredBufferScale { factor } => {
                client.scale(factor as _, qh);
            }
            _ => {}
        }
    }
}

impl Dispatch<WlSeat, ()> for Client {
    fn event(
        client: &mut Self,
        seat: &WlSeat,
        event: <WlSeat as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_seat::Event::Capabilities { capabilities } => {
                if let WEnum::Value(capabilities) = capabilities
                    && capabilities.contains(Capability::Pointer)
                {
                    let pointer = seat.get_pointer(qh, ());

                    client.cursor_shape_device = Some(
                        client
                            .globals
                            .cursor_shaper_manager()
                            .get_pointer(&pointer, &qh, ()),
                    );
                    client.globals.cursor_shaper_manager().destroy();
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlPointer, ()> for Client {
    fn event(
        client: &mut Self,
        _: &WlPointer,
        event: <WlPointer as wayland_client::Proxy>::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            wl_pointer::Event::Enter { serial, .. } => {
                client
                    .cursor_shape_device
                    .as_ref()
                    .unwrap()
                    .set_shape(serial, Shape::Default);
            }
            _ => {}
        }
    }
}

delegate_noop!(Client: ignore WlCompositor);
delegate_noop!(Client: ignore WlShm);
delegate_noop!(Client: ignore WlShmPool);
delegate_noop!(Client: ignore WlBuffer);
delegate_noop!(Client: ignore WpCursorShapeManagerV1);
delegate_noop!(Client: ignore WpCursorShapeDeviceV1);
delegate_noop!(Client: ignore ZwlrLayerShellV1);

macro_rules! use_globals {
    (get_version inherit, $version:expr) => { $version };
    (get_version $version: expr, $($dummy:expr)?) => { $version };
    ($(@$interface: ident ($version: tt) $vis: vis $name: ident: $type: ty),* $(,)?) => {
        #[derive(Default, Clone)]
        struct Globals {
            $($name: Option<$type>),*
        }
        impl Globals {
            fn bind<T>(
                &mut self,
                registry: &WlRegistry,
                name: u32,
                interface: &str,
                version: u32,
                qhandle: &wayland_client::QueueHandle<T>,
            ) -> bool
            where
                T: $(Dispatch<$type, ()> + 'static+)*
            {
                match interface {
                    $(stringify!($interface) => {
                        self.$name = Some(registry.bind(name, use_globals!(get_version $version, version), qhandle, ()));
                        true
                    })*
                    _ => false,
                }
            }

            $($vis fn $name(&self) -> &$type {
                self.$name.as_ref().expect(stringify!($name))
            })*
        }
    };
}

use_globals! {
    @wl_compositor(inherit)
    compositor: WlCompositor,

    @wl_shm(inherit)
    shm: WlShm,

    @zwlr_layer_shell_v1(inherit)
    layer_shell_v1: ZwlrLayerShellV1,

    @wp_cursor_shape_manager_v1(inherit)
    cursor_shaper_manager: WpCursorShapeManagerV1,
}

use std::ptr::NonNull;

#[derive(Debug, Clone, Copy)]
pub struct Pixmap {
    width: u32,
    height: u32,
    data: Option<NonNull<u32>>,
    scale: u32,
}

impl Default for Pixmap {
    fn default() -> Self {
        Self {
            width: 0,
            height: 0,
            data: None,
            scale: 1,
        }
    }
}

impl Pixmap {
    fn buffer_width(&self) -> u32 {
        self.width * self.scale
    }
    fn buffer_height(&self) -> u32 {
        self.height * self.scale
    }
    fn buffer_stride(&self) -> u32 {
        self.buffer_width() * 4
    }
    fn byte_size(&self) -> u32 {
        self.buffer_stride() * self.buffer_height()
    }
    fn pixels_mut(&self) -> &mut [u32] {
        unsafe {
            std::slice::from_raw_parts_mut(
                self.data.unwrap().as_ptr() as _,
                (self.buffer_width() * self.buffer_height()) as _,
            )
        }
    }
    fn unmap(&self) {
        if let Some(ptr) = self.data {
            unsafe { rustix::mm::munmap(ptr.as_ptr() as _, self.byte_size() as _).unwrap() };
        }
    }
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        self.pixels_mut()[(y * self.buffer_width() + x) as usize]
    }
    pub fn pixel_mut(&self, x: u32, y: u32) -> &mut u32 {
        &mut self.pixels_mut()[(y * self.buffer_width() + x) as usize]
    }
}

fn main() {
    env_logger::builder()
        .filter_level(log::LevelFilter::Warn)
        .init();
    let (sender, receiver) = mpsc::channel(1);
    let client = Client::new(sender);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_io()
        .enable_time()
        .build()
        .unwrap();
    runtime.block_on(client.run(receiver));
}
