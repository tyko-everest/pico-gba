// use minifb::{Window, WindowOptions};
use pixels::{Pixels, SurfaceTexture};
use winit::dpi::LogicalSize;
use winit::event_loop::EventLoop;
use winit::window::WindowBuilder;

const WIDTH: u32 = 240;
const HEIGHT: u32 = 160;

struct DisplayPixels {
    pixels: Pixels,
}

impl DisplayPixels {
    fn new() -> Self {
        let event_loop = EventLoop::new();
        // let window = WindowBuilder::new().build(&event_loop).unwrap();
        let builder = WindowBuilder::new();
        let builder = builder.with_inner_size(LogicalSize {
            width: WIDTH,
            height: HEIGHT,
        });
        let builder = builder.with_resizable(false);
        let window = builder.build(&event_loop).unwrap();

        let size = window.inner_size();
        let surface_texture = SurfaceTexture::new(size.width, size.height, &window);
        let pixels = Pixels::new(240, 160, surface_texture).unwrap();
        Self { pixels }
    }

    fn clear(&mut self) {
        let frame = self.pixels.get_frame_mut();
        for pixel in frame.chunks_exact_mut(4) {
            pixel[0] = 0x00; // R
            pixel[1] = 0x00; // G
            pixel[2] = 0x00; // B
            pixel[3] = 0xff; // A
        }
        self.pixels.render().unwrap();
    }
}

// struct DisplayMinifb {
//     window: Window,
// }

// impl DisplayMinifb {
//     fn new() -> Self {
//         let mut window = Window::new("Display", WIDTH, HEIGHT, WindowOptions::default()).unwrap();
//         window.limit_update_rate(Some(std::time::Duration::from_micros(16600)));
//         Self { window }
//     }
// }

fn main() {
    let mut display = DisplayPixels::new();
    display.clear();
    // let mut display = DisplayMinifb::new();
    println!("Hello, world!");
}
