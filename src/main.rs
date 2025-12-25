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
use rustix::fs::{Mode, OFlags};
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
        wl_shm::WlShm,
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

use crate::pixmap::Pixmap;

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

struct WindowInner {
    pixmap: Pixmap,
    buffer: Option<WlBuffer>,
    size: [u32; 2],
    scale: u32,
}

impl WindowInner {
    fn new() -> Self {
        Self {
            pixmap: Pixmap::default(),
            buffer: None,
            size: [0, 0],
            scale: 1,
        }
    }
}

impl Default for WindowInner {
    fn default() -> Self {
        Self::new()
    }
}

#[derive(Clone)]
struct Window {
    surface: WlSurface,
    inner: Arc<Mutex<WindowInner>>,
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
            inner: Default::default(),
        }
    }
    fn configure(&mut self, size: [u32; 2], shm: &WlShm, qh: &QueueHandle<Client>) {
        {
            let mut inner = self.inner.try_lock().unwrap();
            inner.size = size;
        }
        self.allocate_buffer(shm, qh);
    }
    // panic: if self.pixmap.byte_size() == 0
    fn allocate_buffer(&self, shm: &WlShm, qh: &QueueHandle<Client>) {
        let fd = rustix::fs::open(
            SHM_PATH,
            OFlags::CREATE | OFlags::RDWR,
            Mode::from_raw_mode(0o600),
        )
        .unwrap();

        let mut inner = self.inner.try_lock().unwrap();
        let pixmap = Pixmap::map(inner.size.map(|x| x * inner.scale), &fd);

        rustix::fs::unlink(SHM_PATH).unwrap();

        inner
            .buffer
            .replace(pixmap.shm_buffer(shm, fd.as_fd(), qh))
            .as_ref()
            .map(WlBuffer::destroy);
        inner.pixmap = pixmap;
    }

    fn render(&self, source: &Option<DynamicImage>) {
        let inner = self.inner.try_lock().unwrap();
        let [width, height] = {
            let pixmap = &inner.pixmap;
            let (width, height) = (pixmap.width(), pixmap.height());
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
        self.surface
            .attach(Some(inner.buffer.as_ref().unwrap()), 0, 0);
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
        let window = self.window.as_ref().unwrap();
        {
            let mut inner = window.inner.try_lock().unwrap();
            if inner.scale == scale {
                return;
            }

            inner.scale = scale;
            window.surface.set_buffer_scale(scale as _);

            if inner.pixmap.is_empty() {
                return;
            }
        };

        window.allocate_buffer(self.globals.shm(), qh);
        self.reload();
    }
    fn load_cache(&self) -> anyhow::Result<()> {
        if self.source.try_lock().unwrap().is_some() {
            return Ok(());
        }
        let reader = BufReader::new(File::open(self.cache_path.as_ref())?);
        let decoder = JpegDecoder::new(reader)?;
        let img = DynamicImage::from_decoder(decoder)?;
        *self.source.try_lock().unwrap() = Some(img);
        Ok(())
    }
    fn reload(&self) {
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
        if Local::now() < self.next_fetch {
            return;
        }
        match fetch(self.cache_path.as_ref(), 10).await {
            Ok(image) => {
                *self.source.lock().await = Some(image);
                self.reload();
            }
            Err(err) => log::error!("{err:?}"),
        };
        self.next_fetch = self
            .next_fetch
            .with_time(NaiveTime::from_hms_opt(0, 2, 0).unwrap())
            .unwrap()
            .checked_add_days(Days::new(1))
            .unwrap();
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
                client.window.as_mut().unwrap().configure(
                    [width, height],
                    client.globals.shm(),
                    qh,
                );
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
    ($(@$interface: ident $vis: vis $name: ident: $type: ty),* $(,)?) => {
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
                _: u32,
                qhandle: &wayland_client::QueueHandle<T>,
            ) -> bool
            where
                T: $(Dispatch<$type, ()> + 'static+)*
            {
                match interface {
                    $(stringify!($interface) => {
                        self.$name = Some(registry.bind(name, <$type as wayland_client::Proxy>::interface().version, qhandle, ()));
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
    @wl_compositor
    compositor: WlCompositor,

    @wl_shm
    shm: WlShm,

    @zwlr_layer_shell_v1
    layer_shell_v1: ZwlrLayerShellV1,

    @wp_cursor_shape_manager_v1
    cursor_shaper_manager: WpCursorShapeManagerV1,
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

mod pixmap;
