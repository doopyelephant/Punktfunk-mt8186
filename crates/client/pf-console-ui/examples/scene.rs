//! The console shell in an SDL window on a Mac, with a scripted scene: one host, eight pads
//! and a TV remote. For looking at a screen without a device.
//!
//! `cargo run --release -p pf-console-ui --example scene --no-default-features --features metal`
//! draws on the GPU through the window's CAMetalLayer; without `metal` it rasters on the CPU,
//! which runs at a few frames a second. It opens on Controllers. Arrows move, Return is OK,
//! Escape is Back, Y is a hold (the card's menu), X the pad's X; the mouse works too.

#[cfg(not(target_vendor = "apple"))]
fn main() {
    eprintln!("the scene runner links the Homebrew SDL3 on a Mac");
}

#[cfg(target_vendor = "apple")]
fn main() -> anyhow::Result<()> {
    scene::run()
}

/// Skia on the window's Metal layer: a drawable a frame, presented after Skia's submit.
#[cfg(all(target_vendor = "apple", feature = "metal"))]
mod draw {
    use metal::foreign_types::{ForeignType, ForeignTypeRef};
    use metal::{CommandQueue, Device, MTLPixelFormat, MetalLayerRef};
    use sdl3::video::Window;
    use skia_safe::gpu::{self, mtl, DirectContext, SurfaceOrigin};
    use skia_safe::{Canvas, ColorType};

    pub struct Draw {
        window: Window,
        layer: &'static MetalLayerRef,
        queue: CommandQueue,
        context: DirectContext,
    }

    impl Draw {
        pub fn new(window: Window) -> anyhow::Result<Draw> {
            let device =
                Device::system_default().ok_or_else(|| anyhow::anyhow!("no Metal device"))?;
            let queue = device.new_command_queue();
            // SAFETY: a live SDL window; the view and its layer live as long as the window.
            let layer = unsafe {
                let view = sdl3::sys::metal::SDL_Metal_CreateView(window.raw());
                let ptr = sdl3::sys::metal::SDL_Metal_GetLayer(view);
                anyhow::ensure!(!ptr.is_null(), "no CAMetalLayer on the window");
                MetalLayerRef::from_ptr(ptr as *mut _)
            };
            layer.set_device(&device);
            layer.set_pixel_format(MTLPixelFormat::BGRA8Unorm);
            // The trays copy the frame back out of the drawable; a framebuffer-only
            // texture cannot be read, and the copy comes back as noise.
            layer.set_framebuffer_only(false);
            // SAFETY: a live device and a queue on it; Skia retains both.
            let backend =
                unsafe { mtl::BackendContext::new(device.as_ptr() as _, queue.as_ptr() as _) };
            let context = gpu::direct_contexts::make_metal(&backend, None)
                .ok_or_else(|| anyhow::anyhow!("Skia's Metal context did not come up"))?;
            Ok(Draw {
                window,
                layer,
                queue,
                context,
            })
        }

        pub fn window(&self) -> &Window {
            &self.window
        }

        pub fn resized(&mut self) -> anyhow::Result<()> {
            Ok(())
        }

        /// One frame: `paint` draws into the drawable's texture at its pixel size.
        pub fn frame(&mut self, paint: impl FnOnce(&Canvas, u32, u32)) -> anyhow::Result<()> {
            metal::objc::rc::autoreleasepool(|| {
                let Some(drawable) = self.layer.next_drawable() else {
                    return Ok(());
                };
                let tex = drawable.texture();
                let (w, h) = (tex.width() as i32, tex.height() as i32);
                // SAFETY: a live texture from this device; Skia retains it while the target lives.
                let info = unsafe { mtl::TextureInfo::new(tex.as_ptr() as _) };
                let target = gpu::backend_render_targets::make_mtl((w, h), &info);
                let Some(mut surface) = gpu::surfaces::wrap_backend_render_target(
                    &mut self.context,
                    &target,
                    SurfaceOrigin::TopLeft,
                    ColorType::BGRA8888,
                    None,
                    None,
                ) else {
                    anyhow::bail!("Skia could not wrap the {w}×{h} drawable");
                };
                // A drawable comes back holding a frame two or three old; the shell paints
                // over it, not every pixel of it.
                surface.canvas().clear(skia_safe::Color::BLACK);
                paint(surface.canvas(), w as u32, h as u32);
                drop(surface);
                self.context.flush_and_submit();
                let cb = self.queue.new_command_buffer();
                cb.present_drawable(drawable);
                cb.commit();
                Ok(())
            })
        }
    }
}

/// CPU raster blitted through an SDL streaming texture: every machine, few frames a second.
#[cfg(all(target_vendor = "apple", not(feature = "metal")))]
mod draw {
    use sdl3::pixels::PixelFormat;
    use sdl3::render::{Canvas as SdlCanvas, Texture, TextureCreator};
    use sdl3::video::{Window, WindowContext};
    use skia_safe::Canvas;

    pub struct Draw {
        canvas: SdlCanvas<Window>,
        /// Leaked: a texture borrows its creator for life, and this one lives as long as the
        /// runner.
        creator: &'static TextureCreator<WindowContext>,
        texture: Texture<'static>,
        surface: skia_safe::Surface,
        size: (u32, u32),
    }

    fn raster(w: u32, h: u32) -> anyhow::Result<skia_safe::Surface> {
        skia_safe::surfaces::raster_n32_premul((w as i32, h as i32))
            .ok_or_else(|| anyhow::anyhow!("raster surface {w}x{h}"))
    }

    impl Draw {
        pub fn new(window: Window) -> anyhow::Result<Draw> {
            let canvas = window.into_canvas();
            let creator: &'static TextureCreator<WindowContext> =
                Box::leak(Box::new(canvas.texture_creator()));
            let size = canvas.window().size_in_pixels();
            let texture =
                creator.create_texture_streaming(PixelFormat::ARGB8888, size.0, size.1)?;
            Ok(Draw {
                surface: raster(size.0, size.1)?,
                canvas,
                creator,
                texture,
                size,
            })
        }

        pub fn window(&self) -> &Window {
            self.canvas.window()
        }

        pub fn resized(&mut self) -> anyhow::Result<()> {
            self.size = self.canvas.window().size_in_pixels();
            self.surface = raster(self.size.0, self.size.1)?;
            self.texture = self.creator.create_texture_streaming(
                PixelFormat::ARGB8888,
                self.size.0,
                self.size.1,
            )?;
            Ok(())
        }

        pub fn frame(&mut self, paint: impl FnOnce(&Canvas, u32, u32)) -> anyhow::Result<()> {
            let (w, h) = self.size;
            paint(self.surface.canvas(), w, h);
            if let Some(pixmap) = self.surface.peek_pixels() {
                if let Some(bytes) = pixmap.bytes() {
                    self.texture.update(None, bytes, pixmap.row_bytes())?;
                }
            }
            self.canvas.clear();
            self.canvas.copy(&self.texture, None, None)?;
            self.canvas.present();
            Ok(())
        }
    }
}

#[cfg(target_vendor = "apple")]
mod scene {
    use super::draw::Draw;
    use pf_client_core::menu_nav::{PadBattery, PadInfo};
    use pf_client_core::trust::Settings;
    use pf_console_ui::bridge::{self, CreateOptions};
    use pf_console_ui::theme::{set_material, Material};
    use pf_console_ui::{Console, ConsoleHandles, HostRow, Key, OtherDevice, Platform, Viewport};
    use punktfunk_core::config::GamepadPref;
    use sdl3::event::{Event, WindowEvent};
    use sdl3::keyboard::{Keycode, Mod};
    use sdl3::mouse::MouseButton;
    use std::time::{Duration, Instant};

    fn pad(
        name: &str,
        key: &str,
        pref: GamepadPref,
        battery: Option<(u8, bool)>,
        detail: &str,
    ) -> PadInfo {
        PadInfo {
            name: name.into(),
            key: key.into(),
            pref,
            steam_virtual: false,
            battery: battery.map(|(percent, charging)| PadBattery { percent, charging }),
            detail: detail.into(),
            forwarded: true,
            rumble: true,
        }
    }

    pub fn run() -> anyhow::Result<()> {
        let handles = ConsoleHandles::new();
        handles.console.set_hosts(vec![HostRow {
            key: "desk".into(),
            fp_hex: "desk".into(),
            name: "Desk".into(),
            addr: "10.0.0.9".into(),
            port: 9777,
            mgmt_port: 47990,
            paired: true,
            saved: true,
            online: true,
            ..Default::default()
        }]);
        handles.console.set_other_devices(vec![OtherDevice {
            name: "Siri Remote".into(),
            kind: "remote".into(),
            detail: String::new(),
        }]);
        let pads = vec![
            pad(
                "DualSense Wireless Controller",
                "054c:0ce6:DualSense",
                GamepadPref::DualSense,
                Some((72, false)),
                "054C:0CE6 · gamepad",
            ),
            pad(
                "Xbox 360 Controller",
                "045e:028e:Xbox 360",
                GamepadPref::Xbox360,
                None,
                "045E:028E · gamepad",
            ),
            pad(
                "Steam Controller 2",
                "28de:1205:Steam Controller 2",
                GamepadPref::SteamController2,
                Some((100, true)),
                "28DE:1205 · gamepad",
            ),
            pad(
                "Xbox Wireless Controller",
                "045e:0b12:Xbox One",
                GamepadPref::XboxOne,
                Some((45, false)),
                "045E:0B12 · gamepad",
            ),
            pad(
                "8BitDo Pro 2",
                "2dc8:6003:8BitDo Pro 2",
                GamepadPref::EightBitDoPro2,
                Some((88, false)),
                "2DC8:6003 · gamepad",
            ),
            pad(
                "DUALSHOCK 4",
                "054c:09cc:DualShock 4",
                GamepadPref::DualShock4,
                Some((12, false)),
                "054C:09CC · gamepad",
            ),
            pad(
                "Steam Deck",
                "28de:1205:Steam Deck",
                GamepadPref::SteamDeck,
                None,
                "28DE:1205 · gamepad",
            ),
            pad(
                "Pro Controller",
                "057e:2009:Pro Controller",
                GamepadPref::SwitchPro,
                Some((60, true)),
                "057E:2009 · gamepad",
            ),
            // A pad no family claims: Automatic falls back to the Xbox 360 build and drawing.
            PadInfo {
                rumble: false,
                ..pad(
                    "USB Gamepad",
                    "0079:0006:USB Gamepad",
                    GamepadPref::Auto,
                    None,
                    "0079:0006 · gamepad",
                )
            },
        ];
        let create = CreateOptions {
            device_name: "Mac".into(),
            gpu_cache_bytes: 64 << 20,
            fallback_ui: false,
            tv: true,
            tizen: false,
            system_keyboard: false,
            av1_ok: true,
            pyrowave_ok: true,
            profiles: false,
            settings: Settings::default(),
            presets: Vec::new(),
            known_hosts: Default::default(),
            entry: Default::default(),
            screen: None,
            safe_area: None,
        };
        let (opts, entry, _store) = create.into_console(Platform::Apple);
        let mut console = Console::new(opts, entry, &handles)?;

        let sdl = sdl3::init()?;
        let video = sdl.video()?;
        let mut builder = video.window("Punktfunk console", 1920, 1080);
        builder.position_centered().resizable().high_pixel_density();
        #[cfg(feature = "metal")]
        builder.metal_view();
        let mut draw = Draw::new(builder.build()?)?;
        let mut pump = sdl.event_pump()?;
        eprintln!("arrows move · Return OK · Escape Back · Y hold (card menu) · X · mouse works");
        eprintln!(
            "M cycles the panel material: {}",
            Material::ALL.map(Material::name).join(" → ")
        );
        // `PF_SCENE_MATERIAL=<0..6>` opens on that material, for a capture per material.
        let mut material = std::env::var("PF_SCENE_MATERIAL")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .filter(|i| *i < Material::ALL.len())
            .unwrap_or(0);
        set_material(Material::ALL[material]);

        // Opens on Controllers: Tab is the keyboard's R1, two tabs on from Hosts, then down
        // into the row (`PF_SCENE_TABS=1` stops on Games). One key every 45 frames: focus
        // moves on the last painted rects, and a tab slide drops a key that lands mid-flight.
        // Popped from the end.
        let tabs = std::env::var("PF_SCENE_TABS")
            .ok()
            .and_then(|v| v.parse::<usize>().ok())
            .unwrap_or(2);
        let mut script = vec![Key::Down];
        script.extend(std::iter::repeat_n(Key::Tab, tabs));
        let mut frames: u32 = 0;
        let mut last = Instant::now();
        'frames: loop {
            frames += 1;
            // Mouse events arrive in points; the console takes surface pixels.
            let ratio = {
                let (pw, _) = draw.window().size_in_pixels();
                let (lw, _) = draw.window().size();
                pw as f32 / lw.max(1) as f32
            };
            let pointer = |kind: u8, x: f32, y: f32, dy: f32, console: &mut Console| {
                if let Some(p) = bridge::pointer_code(kind, x * ratio, y * ratio, dy) {
                    console.pointer(p);
                }
            };
            for ev in pump.poll_iter() {
                match ev {
                    Event::Quit { .. } => break 'frames,
                    Event::Window {
                        win_event: WindowEvent::PixelSizeChanged(..) | WindowEvent::Resized(..),
                        ..
                    } => draw.resized()?,
                    Event::KeyDown {
                        keycode: Some(Keycode::M),
                        repeat: false,
                        ..
                    } => {
                        material = (material + 1) % Material::ALL.len();
                        set_material(Material::ALL[material]);
                        eprintln!("material: {}", Material::ALL[material].name());
                    }
                    Event::KeyDown {
                        keycode: Some(code),
                        keymod,
                        repeat,
                        ..
                    } => {
                        let key = match code {
                            Keycode::Left => Key::Left,
                            Keycode::Right => Key::Right,
                            Keycode::Up => Key::Up,
                            Keycode::Down => Key::Down,
                            Keycode::Return | Keycode::KpEnter => Key::Return,
                            Keycode::Space => Key::Space,
                            Keycode::Escape => Key::Escape,
                            Keycode::Backspace => Key::Backspace,
                            Keycode::PageUp => Key::PageUp,
                            Keycode::PageDown => Key::PageDown,
                            Keycode::Tab => Key::Tab,
                            Keycode::Y => Key::Y,
                            Keycode::X => Key::X,
                            _ => continue,
                        };
                        let shift = keymod.intersects(Mod::LSHIFTMOD | Mod::RSHIFTMOD);
                        console.key(key, shift, repeat);
                    }
                    Event::MouseMotion { x, y, .. } => pointer(0, x, y, 0.0, &mut console),
                    Event::MouseButtonDown {
                        mouse_btn, x, y, ..
                    } => {
                        let kind = if mouse_btn == MouseButton::Right {
                            3
                        } else {
                            1
                        };
                        pointer(kind, x, y, 0.0, &mut console);
                    }
                    Event::MouseButtonUp {
                        mouse_btn: MouseButton::Left,
                        x,
                        y,
                        ..
                    } => pointer(2, x, y, 0.0, &mut console),
                    Event::MouseWheel {
                        y,
                        mouse_x,
                        mouse_y,
                        ..
                    } => pointer(4, mouse_x, mouse_y, y, &mut console),
                    _ => {}
                }
            }
            if frames % 45 == 0 {
                if let Some(key) = script.pop() {
                    console.key(key, false, false);
                }
            }
            draw.frame(|canvas, w, h| {
                console.frame(
                    canvas,
                    &Viewport::plain(w, h),
                    Some("DualSense Wireless Controller"),
                    Some(GamepadPref::DualSense),
                    &pads,
                );
            })?;
            if frames % 120 == 0 {
                let ms = last.elapsed().as_secs_f64() * 1000.0;
                eprintln!("frame {ms:.1} ms (draw + present, before the cap)");
            }
            let frame = Duration::from_micros(16_667);
            if let Some(rest) = frame.checked_sub(last.elapsed()) {
                std::thread::sleep(rest);
            }
            last = Instant::now();
        }
        Ok(())
    }
}
