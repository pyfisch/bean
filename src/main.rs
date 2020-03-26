#[macro_use(event_enum)]
extern crate wayland_client;

use std::cmp::min;
use std::io::Write;
use std::os::unix::io::AsRawFd;

use pathfinder_canvas::{CanvasFontContext, CanvasRenderingContext2D, Path2D};
use pathfinder_color::ColorF;
use pathfinder_geometry::rect::RectF;
use pathfinder_geometry::vector::{Vector2F, Vector2I};
use pathfinder_gl::{GLDevice, GLVersion};
use pathfinder_renderer::concurrent::rayon::RayonExecutor;
use pathfinder_renderer::concurrent::scene_proxy::SceneProxy;
use pathfinder_renderer::gpu::options::{DestFramebuffer, RendererOptions};
use pathfinder_renderer::gpu::renderer::Renderer;
use pathfinder_renderer::options::BuildOptions;
use pathfinder_resources::embedded::EmbeddedResourceLoader;
use pathfinder_resources::fs::FilesystemResourceLoader;
use khronos_egl::{self as egl, Context as EGLContext, Display as EGLDisplay};
use wayland_client::protocol::{wl_compositor, wl_keyboard, wl_pointer, wl_seat, wl_shell, wl_shm};
use wayland_client::{Display, Filter, GlobalManager};
use wayland_egl::WlEglSurface;

// declare an event enum containing the events we want to receive in the iterator
event_enum!(
    Events |
    Pointer => wl_pointer::WlPointer,
    Keyboard => wl_keyboard::WlKeyboard
);

fn create_context(display: EGLDisplay) -> EGLContext {
    let attributes = [
        egl::RED_SIZE, 8,
        egl::GREEN_SIZE, 8,
        egl::BLUE_SIZE, 8,
        egl::NONE,
    ];

    let config = egl::choose_first_config(display, &attributes)
        .expect("unable to find an appropriate ELG configuration")
        .expect("no config found");

    let context_attributes = [
        egl::CONTEXT_MAJOR_VERSION, 3,
        egl::CONTEXT_MINOR_VERSION, 2,
        // FIXME: If I uncomment this line context creation fails.
        // error: 'unable to create a context: BadAttribute'
        // egl::CONTEXT_OPENGL_PROFILE_MASK, egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
        egl::NONE,
    ];

    egl::create_context(display, config, None, &context_attributes)
        .expect("unable to create a context")
}

fn main() {
    assert!(wayland_egl::is_available());

    let display = Display::connect_to_env().unwrap();
    let mut event_queue = display.create_event_queue();
    let attached_display = (*display).clone().attach(event_queue.token());
    let globals = GlobalManager::new(&attached_display);

    // roundtrip to retrieve the globals list
    event_queue
        .sync_roundtrip(&mut (), |_, _, _| unreachable!())
        .unwrap();

    gl::load_with(|name| egl::get_proc_address(name).unwrap() as *const std::ffi::c_void);

    /*
     * Create a buffer with window contents
     */

    // buffer (and window) width and height
    let buf_x: u32 = 320;
    let buf_y: u32 = 240;

    /*
     * Init wayland objects
     */

    // The compositor allows us to creates surfaces
    let compositor = globals
        .instantiate_exact::<wl_compositor::WlCompositor>(1)
        .unwrap();
    let surface = compositor.create_surface();

    // The shell allows us to define our surface as a "toplevel", meaning the
    // server will treat it as a window
    //
    // NOTE: the wl_shell interface is actually deprecated in favour of the xdg_shell
    // protocol, available in wayland-protocols. But this will do for this example.
    let shell = globals
        .instantiate_exact::<wl_shell::WlShell>(1)
        .expect("Compositor does not support wl_shell");
    let shell_surface = shell.get_shell_surface(&surface);
    shell_surface.quick_assign(|shell_surface, event, _| {
        use wayland_client::protocol::wl_shell_surface::Event;
        // This ping/pong mechanism is used by the wayland server to detect
        // unresponsive applications
        if let Event::Ping { serial } = event {
            shell_surface.pong(serial);
        }
    });

    // Initialize OpenGL
    let egl_display = egl::get_display(display.get_display_ptr() as *mut std::ffi::c_void).unwrap();
    let egl_version = egl::initialize(egl_display).unwrap();
    let egl_context = create_context(egl_display);
    let egl_surface = WlEglSurface::new(&surface, buf_x as i32, buf_y as i32);
    let egl_pointer = egl_surface.ptr();
    egl::make_current(
        egl_display,
        Some(egl_pointer as *mut std::ffi::c_void),
        None,
        Some(egl_context),
    );

    draw_house();
    surface.commit();

    // Set our surface as toplevel and define its contents
    shell_surface.set_toplevel();

    // initialize a seat to retrieve pointer & keyboard events
    //
    // example of using a common filter to handle both pointer & keyboard events
    let common_filter = Filter::new(move |event, _, _| match event {
        Events::Pointer { event, .. } => match event {
            wl_pointer::Event::Enter {
                surface_x,
                surface_y,
                ..
            } => {
                println!("Pointer entered at ({}, {}).", surface_x, surface_y);
            }
            wl_pointer::Event::Leave { .. } => {
                println!("Pointer left.");
            }
            wl_pointer::Event::Motion {
                surface_x,
                surface_y,
                ..
            } => {
                println!("Pointer moved to ({}, {}).", surface_x, surface_y);
            }
            wl_pointer::Event::Button { button, state, .. } => {
                println!("Button {} was {:?}.", button, state);
            }
            _ => {}
        },
        Events::Keyboard { event, .. } => match event {
            wl_keyboard::Event::Enter { .. } => {
                println!("Gained keyboard focus.");
            }
            wl_keyboard::Event::Leave { .. } => {
                println!("Lost keyboard focus.");
            }
            wl_keyboard::Event::Key { key, state, .. } => {
                println!("Key with id {} was {:?}.", key, state);
            }
            _ => (),
        },
    });
    // to be handled properly this should be more dynamic, as more
    // than one seat can exist (and they can be created and destroyed
    // dynamically), however most "traditional" setups have a single
    // seat, so we'll keep it simple here
    let mut pointer_created = false;
    let mut keyboard_created = false;
    globals
        .instantiate_exact::<wl_seat::WlSeat>(1)
        .unwrap()
        .quick_assign(move |seat, event, _| {
            // The capabilities of a seat are known at runtime and we retrieve
            // them via an events. 3 capabilities exists: pointer, keyboard, and touch
            // we are only interested in pointer & keyboard here
            use wayland_client::protocol::wl_seat::{Capability, Event as SeatEvent};

            if let SeatEvent::Capabilities { capabilities } = event {
                if !pointer_created && capabilities.contains(Capability::Pointer) {
                    // create the pointer only once
                    pointer_created = true;
                    seat.get_pointer().assign(common_filter.clone());
                }
                if !keyboard_created && capabilities.contains(Capability::Keyboard) {
                    // create the keyboard only once
                    keyboard_created = true;
                    seat.get_keyboard().assign(common_filter.clone());
                }
            }
        });

    event_queue
        .sync_roundtrip(&mut (), |_, _, _| { /* we ignore unfiltered messages */ })
        .unwrap();

    loop {
        event_queue
            .dispatch(&mut (), |_, _, _| { /* we ignore unfiltered messages */ })
            .unwrap();
    }
}

fn draw_house() {
    let window_size = Vector2I::new(320, 240);
    // FIXME: panic
    // thread 'main' panicked at 'Vertex shader 'blit' compilation failed'
    let mut renderer = Renderer::new(
        GLDevice::new(GLVersion::GL3, 0),
        &EmbeddedResourceLoader::new(),
        DestFramebuffer::full_window(window_size),
        RendererOptions {
            background_color: Some(ColorF::white()),
        },
    );

    // Make a canvas. We're going to draw a house.
    let mut canvas = CanvasRenderingContext2D::new(
        CanvasFontContext::from_system_source(),
        window_size.to_f32(),
    );

    // Set line width.
    canvas.set_line_width(10.0);

    // Draw walls.
    canvas.stroke_rect(RectF::new(
        Vector2F::new(75.0, 140.0),
        Vector2F::new(150.0, 110.0),
    ));

    // Draw door.
    canvas.fill_rect(RectF::new(
        Vector2F::new(130.0, 190.0),
        Vector2F::new(40.0, 60.0),
    ));

    // Draw roof.
    let mut path = Path2D::new();
    path.move_to(Vector2F::new(50.0, 140.0));
    path.line_to(Vector2F::new(150.0, 60.0));
    path.line_to(Vector2F::new(250.0, 140.0));
    path.close_path();
    canvas.stroke_path(path);
}