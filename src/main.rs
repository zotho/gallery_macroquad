use std::{
    env::args,
    fmt, iter,
    mem::{size_of, transmute},
    num::NonZeroUsize,
    ops::Deref,
    path::{Path, PathBuf},
    sync::{atomic::Ordering, Arc, Mutex},
    thread::{spawn, Builder, JoinHandle},
    time::Instant,
};

use flume::{Receiver, Sender, TryRecvError, TrySendError};
use image::{ImageBuffer, ImageError, Rgba};
use lru::LruCache;
use macroquad::prelude::*;
use nu_ansi_term::{Color, Style};
use portable_atomic::AtomicU128;
use tracing::{instrument, trace, warn, Level};
use tracing_subscriber::fmt::{
    format::{FmtSpan, Writer},
    time::FormatTime,
};
use type_freak::{kvlist::KVValueAt, KVListType};

#[cfg(not(target_env = "msvc"))]
use tikv_jemallocator::Jemalloc;

#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static GLOBAL: Jemalloc = Jemalloc;

#[derive(Debug)]
pub enum Command {
    Next,
    Prev,
}

type Img = ImageBuffer<Rgba<u8>, Vec<u8>>;
type Data = (Command, String, Img);

const CACHE_SIZE: usize = 20;

fn main() {
    tracing_subscriber::fmt()
        .with_timer(Difftime::default())
        .with_span_events(FmtSpan::CLOSE)
        .with_max_level(Level::TRACE)
        .init();

    trace!("Starting");

    let (command_sender, command_receiver) = channel();
    let (data_sender, data_receiver) = channel();
    // TODO: use tokio::sync::watch::channel() somehow

    let backend_handle = spawn_named("backend".to_string(), move || {
        tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(backend(data_sender, command_receiver));
    });
    macroquad::Window::from_config(
        Conf {
            window_title: "Gallery".to_string(),
            window_width: 500,
            window_height: 500,
            // fullscreen: true,
            window_resizable: true,
            ..Default::default()
        },
        frontend(data_receiver, command_sender),
    );
    backend_handle.join().unwrap();
}

#[instrument(skip_all, level = "trace")]
async fn backend(data_sender: Sender<Data>, command_receiver: Receiver<Command>) {
    trace!("Starting backend");

    let path = PathBuf::from(args().nth(1).expect("Provide path to image"));
    let mut path = path.as_path();

    let name = file_name(path);

    data_sender
        .send((
            Command::Next,
            name,
            img_from_path(path).expect("Path should be valid image"),
        ))
        .unwrap();

    trace!("Sent first image");

    let files: Vec<_> = path
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .map(|r| r.unwrap().path())
        .filter(|p| p.metadata().unwrap().is_file())
        .collect();

    let mut index = files.iter().position(|p| p == &path).unwrap();

    trace!("Indexed folder");

    // TODO: use async structure to gather cache
    // TODO: clear this cache
    let mut images = Vec::with_capacity(files.len());
    images.extend(iter::from_fn(|| Some(Arc::new(Mutex::new(None)))).take(images.capacity()));

    assert_eq!(files.len(), images.len());
    // for (path, img) in files.iter().cloned().zip(images.iter()) {
    //     let img = img.clone();
    //     spawn(move || {
    //         let mut img_guard = img.lock().unwrap();
    //         let res = img_from_path(&path);
    //         *img_guard = Some(res);
    //     });
    // }

    for i in 0..files.len() {
        let dist = ring_distance(files.len(), i, index);
        let path = files[i].clone();
        let img = images[i].clone();
        if dist < CACHE_SIZE / 2 {
            spawn(move || {
                let mut img_guard = img.lock().unwrap();
                let res = img_from_path(&path);
                *img_guard = Some(res);
            });
        }
    }

    // std::thread::scope(|scope| {
    //     for (path, img) in files.iter().zip(images.iter_mut()) {
    //         scope.spawn(|| {
    //             *img = img_from_path(path);
    //         });
    //     }
    // });
    // trace!("Images done files.len()={} images.len()={}", files.len(), images.len());

    #[cfg_attr(feature = "bench", allow(unused_labels))]
    'commands: for command in command_receiver.iter() {
        trace!("New command");

        let start = Instant::now();

        let starting_index = index;
        let data = loop {
            index = match command {
                Command::Next => index + 1,
                Command::Prev => index + files.len() - 1,
            } % files.len();

            #[cfg(not(feature = "bench"))]
            if command_receiver.len() > 0 {
                trace!("Overlooping backend");
                continue 'commands;
            }

            path = &files[index];

            // let result = img_from_path(path);

            // let result = &images[index];

            let mut img_guard = images[index].lock().unwrap();
            let result = img_guard.get_or_insert_with(|| {
                // FIXME: we should almost never be here
                // panic!();
                img_from_path(path)
            });

            if let Ok(data) = result {
                break data.clone();
            }
            assert_ne!(index, starting_index, "No files to show");
        };

        let mbs = data.as_raw().len() as f64 / 1024.0 / 1024.0;
        let elapsed = start.elapsed().as_secs_f64();
        trace!(
            "{elapsed:7.4} - loading of {mbs:4.1}mb = {:8.4}mb/s",
            mbs / elapsed
        );

        let name = file_name(path);

        match data_sender.try_send((command, name, data)) {
            Err(TrySendError::Disconnected(_)) => break,
            err => err.unwrap(),
        }

        // TODO: improve indexes math
        // TODO: not spawn threads here every time
        for i in 0..files.len() {
            let dist = ring_distance(files.len(), i, index);
            let img = images[i].clone();
            if dist < CACHE_SIZE / 2 {
                let path = files[i].clone();
                spawn(move || {
                    if let Ok(mut img_guard) = img.try_lock() {
                        img_guard.get_or_insert_with(|| img_from_path(&path));
                    }
                });
            } else {
                // TODO: maybe remember error here?
                if let Ok(mut img_guard) = img.try_lock() {
                    img_guard.take();
                }
            }
        }

        trace!("Command processed");
    }
    trace!("End of loop!");
}

#[instrument(skip_all, level = "trace")]
async fn frontend(receiver: Receiver<Data>, command_sender: Sender<Command>) {
    // TODO: clear this cache
    let mut texture_cache = LruCache::new(NonZeroUsize::new(CACHE_SIZE).unwrap());

    let mut texture = Texture2D::empty();

    #[cfg(feature = "bench")]
    let mut k = 0;
    #[cfg(feature = "bench")]
    let bench_start = Instant::now();

    let mut instant = Instant::now();
    let mut loop_trace = 3;

    #[cfg(feature = "bench")]
    for _ in 0..BENCH_STEPS {
        command_sender.send(Command::Next).unwrap();
    }

    loop {
        if loop_trace > 0 {
            trace!("Loop started");
        }

        if is_key_down(KeyCode::Escape) || is_mouse_button_down(MouseButton::Left) {
            break;
        }
        let mouse_y = mouse_wheel().1;
        if is_key_pressed(KeyCode::Right) || mouse_y < 0.0 {
            command_sender.send(Command::Next).unwrap();
        } else if is_key_pressed(KeyCode::Left) || mouse_y > 0.0 {
            command_sender.send(Command::Prev).unwrap();
        }

        let dest_size = Vec2::new(screen_width(), screen_height());

        match receiver.try_recv_last() {
            Ok((_prev_command, name, bytes)) => {
                loop_trace = 3;

                texture = texture_cache
                    .get_or_insert(name, || from_raw_image(&bytes))
                    .deref()
                    .clone();

                // texture = from_raw_image(&bytes);

                // let (dw, dh) = display_size();
                // if dw != 0.0 && dh != 0.0 {
                //     let (_, target_size) = fit_texture(vec2(dw, dh - 0.0), &texture);
                //     request_new_screen_size(target_size.x, target_size.y);
                // }

                let elapsed = instant.elapsed().as_secs_f64();
                instant = Instant::now();
                trace!("{elapsed:7.4} - loop");

                #[cfg(feature = "bench")]
                {
                    k += 1;
                    trace!("Bench {k}/{BENCH_STEPS}");
                    if k == BENCH_STEPS {
                        let bench_elapsed = bench_start.elapsed().as_secs_f64();
                        trace!(
                            "Bench done in {bench_elapsed} s, {} s/image",
                            bench_elapsed / BENCH_STEPS as f64
                        );
                        break;
                    }
                }
            }
            Err(TryRecvError::Disconnected) => {
                trace!("Disconnected");
                break;
            }
            _ => {}
        }

        clear_background(BLACK);
        let (pos, target_size) = fit_texture(dest_size, &texture);
        draw_texture_ex(
            texture,
            pos.x,
            pos.y,
            WHITE,
            DrawTextureParams {
                dest_size: Some(target_size),
                ..Default::default()
            },
        );

        if loop_trace > 0 {
            trace!("Loop ended");
            loop_trace -= 1;
        }
        next_frame().await
    }
}

#[cfg(feature = "bench")]
use konst::{primitive::parse_usize, result::unwrap_ctx};

#[cfg(feature = "bench")]
const BENCH_STEPS: usize = unwrap_ctx!(parse_usize(env!("BENCH_STEPS")));

pub trait TryRecvLast<T> {
    fn try_recv_last(&self) -> Result<T, TryRecvError>;
}

impl<T> TryRecvLast<T> for Receiver<T> {
    #[cfg(not(feature = "bench"))]
    fn try_recv_last(&self) -> Result<T, TryRecvError> {
        let mut res = self.try_recv();
        if matches!(res, Err(_)) {
            return res;
        };
        loop {
            match self.try_recv() {
                ok @ Ok(_) => {
                    trace!("Overlooping frontend");
                    res = ok
                }
                Err(TryRecvError::Empty) => break res,
                err => break err,
            }
        }
    }

    #[cfg(feature = "bench")]
    fn try_recv_last(&self) -> Result<T, TryRecvError> {
        self.try_recv()
    }
}

pub fn file_name(path: &Path) -> String {
    path.file_name()
        .unwrap_or_default()
        .to_str()
        .unwrap_or_default()
        .to_string()
}

pub fn ring_distance(ring_size: usize, a: usize, b: usize) -> usize {
    let (min, max) = (a.min(b), a.max(b));
    let first = max - min;
    let second = min + ring_size - max;
    first.min(second)
}

pub fn channel<T>() -> (Sender<T>, Receiver<T>) {
    flume::unbounded()
}

pub fn spawn_named<F, T>(name: String, f: F) -> JoinHandle<T>
where
    F: FnOnce() -> T,
    F: Send + 'static,
    T: Send + 'static,
{
    Builder::new()
        .name(name)
        .spawn(f)
        .expect("failed to spawn thread")
}

pub fn fit_texture(dest_size: Vec2, texture: &Texture2D) -> (Vec2, Vec2) {
    // See: https://stackoverflow.com/questions/6565703/math-algorithm-fit-image-to-screen-retain-aspect-ratio

    let wi = texture.width();
    let hi = texture.height();

    let ws = dest_size[0];
    let hs = dest_size[1];

    let ri = wi / hi;
    let rs = ws / hs;

    let (wt, ht) = if rs > ri {
        (wi * hs / hi, hs)
    } else {
        (ws, hi * ws / wi)
    };

    let xt = (ws - wt) / 2.0;
    let yt = (hs - ht) / 2.0;

    (vec2(xt, yt), vec2(wt, ht))
}

#[instrument(skip_all, level = "trace")]
pub fn img_from_path(path: &Path) -> Result<Img, ImageError> {
    // trace!("Getting image from path {:?}", path);
    let img = image::open(path)?;
    let img = img.into_rgba8();
    // trace!("Ready image from path {:?} with len {}", path, img.len());
    Ok(img)
}

// Should be called only from macroquad thread
#[instrument(skip_all, level = "trace")]
pub fn from_raw_image(img: &Img) -> SafeTexture2D {
    let width = img.width() as u16;
    let height = img.height() as u16;
    let bytes = img.as_raw();
    SafeTexture2D(Texture2D::from_rgba8(width, height, bytes))
}

#[derive(Clone)]
pub struct SafeTexture2D(pub Texture2D);

impl Deref for SafeTexture2D {
    type Target = Texture2D;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl Drop for SafeTexture2D {
    fn drop(&mut self) {
        self.delete();
    }
}

pub fn from_file(bytes: &[u8]) -> Option<Texture2D> {
    let start = Instant::now();
    let img = image::load_from_memory(bytes).ok()?;
    let part1 = start.elapsed().as_secs_f64();

    let img = img.to_rgba8();
    let part2 = start.elapsed().as_secs_f64();

    let width = img.width() as u16;
    let height = img.height() as u16;
    let bytes = img.into_raw();
    let part3 = start.elapsed().as_secs_f64();

    let res = Some(Texture2D::from_rgba8(width, height, &bytes));
    let elapsed = start.elapsed().as_secs_f64();

    trace!(
        "{elapsed:7.4} - from file ({part1:.7}, {:.7}, {:.7})",
        part2 - part1,
        part3 - part2
    );
    res
}

type SizeToRawTMapping = KVListType![([(); 8], u64), ([(); 16], u128)];
type SizeOfInstant = [(); size_of::<Instant>()];
type RawTIdx<Idx> = KVValueAt<SizeToRawTMapping, SizeOfInstant, Idx>;

#[derive(Debug)]
pub struct Difftime {
    epoch: AtomicU128,
}

impl Default for Difftime {
    fn default() -> Self {
        let epoch: RawTIdx<_> = unsafe { transmute(Instant::now()) };
        Self {
            epoch: AtomicU128::new(epoch as _),
        }
    }
}

impl From<Instant> for Difftime {
    fn from(epoch: Instant) -> Self {
        let epoch: RawTIdx<_> = unsafe { transmute(epoch) };
        Self {
            epoch: AtomicU128::new(epoch as _),
        }
    }
}

impl FormatTime for Difftime {
    fn format_time(&self, w: &mut Writer<'_>) -> fmt::Result {
        let epoch: RawTIdx<_> = unsafe { transmute(Instant::now()) };
        let prev_epoch = self.epoch.swap(epoch as _, Ordering::SeqCst);
        let prev_epoch: Instant = unsafe { transmute(prev_epoch as RawTIdx<_>) };
        let e = prev_epoch.elapsed();

        let color = match e.as_secs_f64() {
            x if x < 0.001 => Color::DarkGray.normal(),
            x if x < 0.01 => Color::Green.normal(),
            x if x < 0.1 => Color::LightBlue.normal(),
            x if x < 1.0 => Color::Yellow.normal(),
            x if x >= 1.0 => Color::Red.bold(),
            _ => Color::Red.bold(),
        };

        let convert = |s: &mut String| {
            for byte in unsafe { s.as_bytes_mut() } {
                match *byte {
                    b' ' => (),
                    b'0' => *byte = b'_',
                    _ => break,
                }
            }
        };

        let mut as_secs = format!("{:2}", e.as_secs());
        convert(&mut as_secs);

        let mut subsec_nanos = format!("{:09}", e.subsec_nanos());
        convert(&mut subsec_nanos);

        write!(
            w,
            "{}{}{:2}.{:09}s{}{}",
            Style::new().dimmed().suffix(),
            color.prefix(),
            as_secs,
            subsec_nanos,
            color.suffix(),
            Style::new().dimmed().prefix(),
        )
    }
}
