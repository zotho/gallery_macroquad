use std::{env::args, path::{PathBuf, Path}, thread::{JoinHandle, Builder}, time::Instant};

use flume::{Receiver, Sender, TrySendError, TryRecvError};
use image::{ImageBuffer, Rgba, DynamicImage, ImageError};
use macroquad::prelude::*;

#[derive(Debug)]
pub enum Command {
    Next,
    Prev,
}

type Img = ImageBuffer<Rgba<u8>, Vec<u8>>;
type Data = (Command, String, Img);

fn main() {

    let (command_sender, command_receiver) = channel();
    let (data_sender, data_receiver) = channel();

    let backend_handle = spawn_named("backend".to_string(), move || backend(data_sender, command_receiver));
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

fn backend(data_sender: Sender<Data>, command_receiver: Receiver<Command>) {
    let path = PathBuf::from(args().nth(1).expect("Provide path to image"));
    let mut path = path.as_path();

    // data_sender
    //     .send((Command::Next, read(path).unwrap()))
    //     .unwrap();

    let name = path.file_name().unwrap_or_default().to_str().unwrap_or_default().to_string();

    data_sender
        .send((Command::Next, name, img_from_path(path).unwrap()))
        .unwrap();

    let files: Vec<_> = path
        .parent()
        .unwrap()
        .read_dir()
        .unwrap()
        .map(|r| r.unwrap().path())
        .filter(|p| p.metadata().unwrap().is_file())
        .collect();

    let mut index = files.iter().position(|p| p == &path).unwrap();

    'commands: for command in command_receiver.iter() {
        let start = Instant::now();

        let data = loop {
            index = match command {
                Command::Next => index + 1,
                Command::Prev => index + files.len() - 1,
            } % files.len();

            #[cfg(not(feature = "bench"))]
            if command_receiver.len() > 0 {
                println!("Overlooping backend");
                continue 'commands;
            }

            path = &files[index];

            // let data = read(path).unwrap();
            if let Ok(data) = img_from_path(path) {
                break data;
            }
        };

        let mbs = data.as_raw().len() as f64 / 1024.0 / 1024.0;
        let elapsed = start.elapsed().as_secs_f64();
        println!("{elapsed:7.4} - loading of {mbs:4.1}mb = {:8.4}mb/s", mbs / elapsed);

        let name = path.file_name().unwrap_or_default().to_str().unwrap_or_default().to_string();

        match data_sender.try_send((command, name, data)) {
            Err(TrySendError::Disconnected(_)) => break,
            err => err.unwrap() 
        }
    }
}

async fn frontend(receiver: Receiver<Data>, command_sender: Sender<Command>) {
    let mut texture = Texture2D::empty();

    // Benchmark
    #[cfg(feature = "bench")]
    let mut k = 0;
    #[cfg(feature = "bench")]
    let bench_start = Instant::now();

    let mut instant = Instant::now();

    #[cfg(feature = "bench")]
    for _ in 0..BENCH_STEPS {
        command_sender.send(Command::Next).unwrap();
    }

    loop {
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
            Ok((_prev_command, _name, bytes)) => {
                texture = from_raw_image(&bytes);

                let (dw, dh) = display_size();
                if dw != 0.0 && dh != 0.0 {
                    let (_, target_size) = fit_texture(vec2(dw, dh - 0.0), &texture);
                    request_new_screen_size(target_size.x, target_size.y);
                }

                let elapsed = instant.elapsed().as_secs_f64();
                instant = Instant::now();
                println!("{elapsed:7.4} - loop\n");

                #[cfg(feature = "bench")]
                {
                    k += 1;
                    println!("Bench {k}/{BENCH_STEPS}");
                    if k == BENCH_STEPS {
                        let bench_elapsed = bench_start.elapsed().as_secs_f64();
                        println!("Bench done in {bench_elapsed} s, {} s/image", bench_elapsed / BENCH_STEPS as f64);
                        break;
                    }
                }

                // println!("{:?}, {}", &prev_command, bytes.len());
                // if let Some(new_texture) = from_file(&bytes[..]) {
                //     texture = new_texture;

                //     // println!("Set texture");
                //     command_sender.send(Command::Next).unwrap();

                //     let elapsed = instant.elapsed().as_secs_f64();
                //     println!("Loop took {elapsed:.7}ms");
                //     instant = Instant::now();

                //     k += 1;
                //     if k == 200 {break;}
                // } else {
                //     command_sender.send(prev_command).unwrap();
                //     // println!("Skip file");
                // }
            },
            Err(TryRecvError::Disconnected) => {
                break;
            },
            _ => {},
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

        next_frame().await
    }
}


pub trait TryRecvLast<T> {
    fn try_recv_last(&self) -> Result<T, TryRecvError>;
}

impl<T> TryRecvLast<T> for Receiver<T> {
    fn try_recv_last(&self) -> Result<T, TryRecvError> {
        let mut res = self.try_recv();
        if matches!(res, Err(_)) {
            return res;
        };
        loop {
            match self.try_recv() {
                ok @ Ok(_) => {
                    println!("Overlooping frontend");
                    res = ok
                },
                Err(TryRecvError::Empty) => break res,
                err => break err,
            }
        }
    }
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
    Builder::new().name(name).spawn(f).expect("failed to spawn thread")
}

#[cfg(feature = "bench")]
use konst::{primitive::parse_usize, result::unwrap_ctx};

#[cfg(feature = "bench")]
const BENCH_STEPS: usize = unwrap_ctx!(parse_usize(env!("BENCH_STEPS")));

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

pub fn img_from_path(path: &Path) -> Result<Img, ImageError> {
    image::open(path).map(DynamicImage::into_rgba8)
}

pub fn from_raw_image(img: &Img) -> Texture2D {
    let start = Instant::now();

    let width = img.width() as u16;
    let height = img.height() as u16;
    let bytes = img.as_raw();
    let res = Texture2D::from_rgba8(width, height, bytes);
    let elapsed = start.elapsed().as_secs_f64();

    println!("{elapsed:7.4} - from raw image");
    res
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

    println!("{elapsed:7.4} - from file ({part1:.7}, {:.7}, {:.7})", part2-part1, part3-part2);
    res
}
