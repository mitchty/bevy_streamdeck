use std::time::Duration;

use bevy_app::{App, AppExit, Last, Plugin, PreStartup, PreUpdate};
pub use bevy_color::{Color, ColorToComponents, LinearRgba};
use bevy_ecs::{
    message::{Message, MessageReader, MessageWriter},
    resource::Resource,
    system::{Commands, Res, ResMut},
};
#[cfg(feature = "image_compatibility")]
use bevy_image::Image;
use bevy_input::ButtonInput;
use bevy_tasks::IoTaskPool;
use crossbeam_channel::{Receiver, Sender, bounded};
#[cfg(feature = "images")]
use image::{DynamicImage, ImageBuffer, Pixel, Rgba, imageops::FilterType};
use log::debug;
pub use streamdeck::Kind;
use streamdeck::{Colour, Error};

/// Which deck to connect to, by default the first Elgato device found.
///
/// All the filters are optional and combine, a deck has to match every one
/// that is set. Only Elgato (vendor `0x0fd9`) devices are looked at.
#[derive(Default, Clone, Debug)]
pub struct StreamDeckPlugin {
    /// Only connect to this kind of deck, e.g. [`Kind::Xl`].
    pub kind: Option<Kind>,
    /// USB product id, e.g. `0x008f` for a Stream Deck XL v2.
    pub pid: Option<u16>,
    /// USB serial, to pick between several decks of the same kind.
    pub serial: Option<String>,
}

impl StreamDeckPlugin {
    pub fn with_kind(mut self, kind: Kind) -> Self {
        self.kind = Some(kind);
        self
    }

    pub fn with_pid(mut self, pid: u16) -> Self {
        self.pid = Some(pid);
        self
    }

    pub fn with_serial(mut self, serial: impl Into<String>) -> Self {
        self.serial = Some(serial.into());
        self
    }
}

#[derive(Resource, Clone)]
struct Target {
    kind: Option<Kind>,
    pid: Option<u16>,
    serial: Option<String>,
}

impl Target {
    fn wants(&self, kind: Kind) -> bool {
        self.kind.is_none_or(|wanted| wanted == kind)
    }
}

impl Plugin for StreamDeckPlugin {
    fn build(&self, app: &mut App) {
        app.insert_resource(Target {
            kind: self.kind,
            pid: self.pid,
            serial: self.serial.clone(),
        })
        .add_message::<StreamDeckInput>()
        .init_resource::<ButtonInput<StreamDeckKey>>()
        .add_systems(PreStartup, listener)
        .add_systems(PreUpdate, receiver)
        .add_systems(Last, exit_on_exit);
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
pub struct StreamDeckKey(pub u8);

#[derive(Message, Debug)]
pub enum StreamDeckInput {
    Press(u8),
    Release(u8),
    Disconnected,
    Connected(Kind),
}

#[derive(Debug)]
enum StreamDeckEvent {
    LostConnection,
    Connected(streamdeck::Kind),
    KeyPressed(Vec<u8>),
}

enum StreamDeckOrder {
    Reset,
    Color(u8, Color),
    #[cfg(feature = "images")]
    Image(u8, DynamicImage),
    Exit,
}

const ELGATO_VID: u16 = 0x0fd9;

fn connect(target: &Target) -> Option<streamdeck::StreamDeck> {
    let open = |pid| {
        streamdeck::StreamDeck::connect(ELGATO_VID, pid, target.serial.clone())
            .ok()
            .filter(|deck| target.wants(deck.kind()))
    };
    match target.pid {
        Some(pid) => open(pid),
        // Probing first means a kind filter never opens decks it doesn't want.
        None => streamdeck::StreamDeck::probe()
            .ok()?
            .into_iter()
            .flatten()
            .filter(|(kind, _)| target.wants(*kind))
            .find_map(|(_, pid)| open(pid)),
    }
}

fn listener(mut commands: Commands, target: Res<Target>) {
    let target = Target::clone(&target);
    let (event_tx, event_rx) = bounded::<StreamDeckEvent>(10);
    let (order_tx, order_rx) = bounded::<StreamDeckOrder>(100);

    let taskpool = IoTaskPool::get();
    let task = taskpool.spawn(async move {
        let mut streamdeck: Option<streamdeck::StreamDeck> = None;
        loop {
            let mut lost_connection = false;
            if let Some(streamdeck) = streamdeck.as_mut() {
                let mut act = || {
                    let read = streamdeck.read_buttons(Some(Duration::from_millis(1)));
                    match read {
                        Ok(read) => {
                            let _ = event_tx.send(StreamDeckEvent::KeyPressed(read));
                        }
                        Err(Error::NoData) => {}
                        Err(err) => {
                            return Err(err);
                        }
                    }

                    for order in order_rx.try_iter() {
                        match match order {
                            StreamDeckOrder::Exit => return Ok(false),
                            StreamDeckOrder::Reset => streamdeck.reset(),
                            StreamDeckOrder::Color(k, color) => {
                                // The device wants gamma encoded values, linear
                                // makes everything look dark and muddy.
                                let [r, g, b, _] = color.to_srgba().to_f32_array();
                                streamdeck.set_button_rgb(
                                    k,
                                    &Colour {
                                        r: (r * 255.0) as u8,
                                        g: (g * 255.0) as u8,
                                        b: (b * 255.0) as u8,
                                    },
                                )
                            }
                            #[cfg(feature = "images")]
                            StreamDeckOrder::Image(k, image) => {
                                streamdeck.set_button_image(k, image)
                            }
                        } {
                            Ok(_) => (),
                            Err(Error::Hid(error)) => {
                                debug!("HidError {:?}", error)
                            }
                            Err(err) => {
                                return Err(err);
                            }
                        }
                    }
                    Ok(true)
                };
                match act() {
                    Ok(true) => (),
                    Ok(false) => break,
                    Err(error) => {
                        debug!("Error communicating with StreamDeck: {:?}", error);
                        let _ = event_tx.send(StreamDeckEvent::LostConnection);
                        lost_connection = true;
                    }
                }
            }
            if lost_connection {
                streamdeck = None;
            }
            if streamdeck.is_none() {
                if let Some(new_streamdeck) = connect(&target) {
                    let _ = event_tx.send(StreamDeckEvent::Connected(new_streamdeck.kind()));
                    streamdeck = Some(new_streamdeck);
                } else {
                    // Probing opens hidapi each time, don't spin on it while
                    // no deck is plugged in.
                    std::thread::sleep(Duration::from_millis(500));
                }
            }
        }
    });
    task.detach();
    commands.insert_resource(StreamDeckInternal { events: event_rx });
    commands.insert_resource(StreamDeck {
        orders: order_tx,
        kind: None,
    });
}

fn receiver(
    mut streamdeck: ResMut<StreamDeck>,
    internal: Res<StreamDeckInternal>,
    mut inputs: ResMut<ButtonInput<StreamDeckKey>>,
    mut input_events: MessageWriter<StreamDeckInput>,
) {
    inputs.clear();
    for from_stream in internal.events.try_iter() {
        match from_stream {
            StreamDeckEvent::LostConnection => {
                streamdeck.kind = None;
                input_events.write(StreamDeckInput::Disconnected);
            }
            StreamDeckEvent::Connected(kind) => {
                streamdeck.kind = Some(kind);
                input_events.write(StreamDeckInput::Connected(kind));
            }
            StreamDeckEvent::KeyPressed(keys) => {
                for (k, s) in keys.iter().enumerate() {
                    if *s == 1 && !inputs.pressed(StreamDeckKey(k as u8)) {
                        inputs.press(StreamDeckKey(k as u8));
                        input_events.write(StreamDeckInput::Press(k as u8));
                    }

                    if *s == 0 && inputs.pressed(StreamDeckKey(k as u8)) {
                        inputs.release(StreamDeckKey(k as u8));
                        input_events.write(StreamDeckInput::Release(k as u8));
                    }
                }
            }
        }
    }
}

#[derive(Resource)]
struct StreamDeckInternal {
    events: Receiver<StreamDeckEvent>,
}

#[derive(Resource)]
pub struct StreamDeck {
    orders: Sender<StreamDeckOrder>,
    kind: Option<Kind>,
}

/// Keys everywhere in this api are zero indexed, left to right then top to
/// bottom, matching [`StreamDeckKey`] and the `Press`/`Release` events.
impl StreamDeck {
    /// The connected deck, `None` until one is found.
    pub fn kind(&self) -> Option<Kind> {
        self.kind
    }

    pub fn set_key_color(&self, key: u8, color: Color) {
        // try_send: a full queue must never block the main thread, a dropped
        // color update is harmless.
        let _ = self.orders.try_send(StreamDeckOrder::Color(key, color));
    }

    /// Returns `true` if the update was queued, `false` if there is no deck
    /// yet or the queue was full and the update was dropped. Callers that keep
    /// their own copy of what is on the deck need this to stay in sync.
    #[cfg(feature = "images")]
    pub fn set_key_image(&self, key: u8, image: &Image) -> bool {
        self.set_key_image_with_mode(key, image, ImageMode::default())
    }

    /// See [`StreamDeck::set_key_image`] for the return value.
    #[cfg(feature = "images")]
    pub fn set_key_image_with_mode(&self, key: u8, image: &Image, image_mode: ImageMode) -> bool {
        if let Some(kind) = self.kind {
            // Convert the texture to an image
            let mut dynamic_image = match image.texture_descriptor.format {
                bevy_render::render_resource::TextureFormat::Rgba8UnormSrgb => {
                    ImageBuffer::from_raw(
                        image.texture_descriptor.size.width,
                        image.texture_descriptor.size.height,
                        image.data.as_ref().unwrap().clone(),
                    )
                    .map(DynamicImage::ImageRgba8)
                }
                _ => unimplemented!(),
            }
            .unwrap();

            // Resize the image to the size supported by the Stream Deck
            let (x, y) = kind.image_size();
            dynamic_image = match image_mode.resize {
                ImageResize::Exact => {
                    dynamic_image.resize_exact(x as u32, y as u32, FilterType::Gaussian)
                }
                ImageResize::Aspect => {
                    dynamic_image.resize(x as u32, y as u32, FilterType::Gaussian)
                }
                ImageResize::AspectFill => {
                    dynamic_image.resize_to_fill(x as u32, y as u32, FilterType::Gaussian)
                }
            };

            // Apply a background
            if let Some(background) = image_mode.background {
                let bevy_color::LinearRgba {
                    red, green, blue, ..
                } = background.to_linear();

                for pixel in dynamic_image.as_mut_rgba8().unwrap().pixels_mut() {
                    pixel.blend(&Rgba([
                        (red * 255.0) as u8,
                        (green * 255.0) as u8,
                        (blue * 255.0) as u8,
                        255 - pixel.0[3],
                    ]));
                }
            }

            // Invert
            if image_mode.invert {
                dynamic_image.invert();
            }

            self.orders
                .try_send(StreamDeckOrder::Image(key, dynamic_image))
                .is_ok()
        } else {
            false
        }
    }

    pub fn reset_key(&self, key: u8) {
        let _ = self
            .orders
            .try_send(StreamDeckOrder::Color(key, Color::BLACK));
    }

    pub fn reset(&self) {
        let _ = self.orders.send(StreamDeckOrder::Reset);
    }
}

fn exit_on_exit(streamdeck: Res<StreamDeck>, mut exit_events: MessageReader<AppExit>) {
    if exit_events.read().next().is_some() {
        let _ = streamdeck.orders.send(StreamDeckOrder::Reset);
        let _ = streamdeck.orders.send(StreamDeckOrder::Exit);
    }
}

#[cfg(feature = "images")]
#[derive(Default)]
pub enum ImageResize {
    /// Does not preserve aspect ratio.
    #[default]
    Exact,
    /// The image's aspect ratio is preserved.
    Aspect,
    /// The image's aspect ratio is preserved.
    /// The image is scaled to the maximum possible size that fits within the
    /// larger (relative to aspect ratio) of the bounds, then cropped to fit
    /// within the other bound.
    AspectFill,
}

#[cfg(feature = "images")]
#[derive(Default)]
pub struct ImageMode {
    pub resize: ImageResize,
    pub invert: bool,
    pub background: Option<Color>,
}
