//! Host-side renderer for the BenchVolt firmware UI.
//!
//! Renders the REAL firmware view (`src/view.rs`, included verbatim via
//! `#[path]`) into an in-memory RGB565 framebuffer sized like the ST7789
//! panel (320x170 landscape, from `Builder::st7789(...).with_display_size(170, 320)
//! .with_orientation(Orientation::Landscape(true))` in `src/main.rs`).
//!
//! Outputs 2x-scaled PNG screenshots and an animated GIF of a menu flow to
//! `Images/firmware/` at the repository root.

#[path = "../../../src/view.rs"]
mod view;

use std::cell::RefCell;
use std::convert::Infallible;
use std::fs::File;
use std::io::BufWriter;
use std::path::{Path, PathBuf};
use std::rc::Rc;

use embedded_graphics::{
    pixelcolor::{raw::RawU16, Rgb565},
    prelude::*,
    primitives::Rectangle,
};

use benchvolt_poc::app::{
    Action, AppReducer, AppState, AwgStatus, AwgWaveform, ControlFocus, Fault, LoadMeasurement,
    Measurement, OutputTransition, RegulationMode, Screen,
};
use benchvolt_poc::input_policy::{clamp_adjustment, encoder_action, ButtonTracker, EncoderAccumulator};
use benchvolt_poc::pd::Contract;
use reducto::{App, View as _};
use view::BenchVoltView;

const WIDTH: usize = 320;
const HEIGHT: usize = 170;
const SCALE: usize = 2;

/// Shared RGB565 framebuffer. The view owns one handle; we keep another to
/// read pixels back out after each render.
#[derive(Clone)]
struct Framebuffer {
    pixels: Rc<RefCell<Vec<u16>>>,
}

impl Framebuffer {
    fn new() -> Self {
        Self {
            pixels: Rc::new(RefCell::new(vec![0u16; WIDTH * HEIGHT])),
        }
    }

    /// Snapshot as RGB888 bytes, scaled `SCALE`x nearest-neighbor.
    fn to_rgb(&self) -> Vec<u8> {
        let pixels = self.pixels.borrow();
        let mut out = Vec::with_capacity(WIDTH * HEIGHT * SCALE * SCALE * 3);
        for y in 0..HEIGHT * SCALE {
            for x in 0..WIDTH * SCALE {
                let raw = pixels[(y / SCALE) * WIDTH + x / SCALE];
                let c = Rgb565::from(RawU16::new(raw));
                // 565 -> 888 with bit replication.
                let r = (c.r() << 3) | (c.r() >> 2);
                let g = (c.g() << 2) | (c.g() >> 4);
                let b = (c.b() << 3) | (c.b() >> 2);
                out.extend_from_slice(&[r, g, b]);
            }
        }
        out
    }
}

impl OriginDimensions for Framebuffer {
    fn size(&self) -> Size {
        Size::new(WIDTH as u32, HEIGHT as u32)
    }
}

impl DrawTarget for Framebuffer {
    type Color = Rgb565;
    type Error = Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let mut buf = self.pixels.borrow_mut();
        for Pixel(point, color) in pixels {
            if (0..WIDTH as i32).contains(&point.x) && (0..HEIGHT as i32).contains(&point.y) {
                buf[point.y as usize * WIDTH + point.x as usize] =
                    RawU16::from(color).into_inner();
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        let area = area.intersection(&self.bounding_box());
        let raw = RawU16::from(color).into_inner();
        let mut buf = self.pixels.borrow_mut();
        for y in 0..area.size.height as usize {
            let row = (area.top_left.y as usize + y) * WIDTH + area.top_left.x as usize;
            buf[row..row + area.size.width as usize].fill(raw);
        }
        Ok(())
    }
}

fn write_png(path: &Path, rgb: &[u8]) {
    let file = File::create(path).expect("create png");
    let mut encoder = png::Encoder::new(
        BufWriter::new(file),
        (WIDTH * SCALE) as u32,
        (HEIGHT * SCALE) as u32,
    );
    encoder.set_color(png::ColorType::Rgb);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("png header");
    writer.write_image_data(rgb).expect("png data");
}

fn distinct_colors(rgb: &[u8]) -> usize {
    let mut colors: Vec<[u8; 3]> = rgb.chunks_exact(3).map(|c| [c[0], c[1], c[2]]).collect();
    colors.sort_unstable();
    colors.dedup();
    colors.len()
}

fn contract_20v_5a() -> Contract {
    Contract {
        source_position: 4,
        millivolts: 20_000,
        operating_milliamps: 5_000,
        maximum_milliamps: 5_000,
    }
}

fn measurement(millivolts: u16, milliamps: u16) -> Measurement {
    Measurement {
        millivolts,
        milliamps,
        valid: true,
    }
}

/// Common telemetry for the stills: valid 34.5 C temperature and a 20 V / 5 A
/// PD contract on the sink.
fn base_state() -> AppState {
    // temp_sixteenths_c: 34.5 C * 16 = 552.
    let mut state = AppState::new(true, Some(552));
    state.pd_contract = Some(contract_20v_5a());
    state.sink = measurement(19_960, 1_240);
    state
}

fn render_still(name: &str, out_dir: &Path, state: &AppState) -> Vec<u8> {
    let fb = Framebuffer::new();
    let mut view = BenchVoltView::new(fb.clone());
    view.render(state);
    let rgb = fb.to_rgb();
    let colors = distinct_colors(&rgb);
    assert!(
        colors >= 3,
        "{name}: expected non-trivial content, found {colors} distinct colors"
    );
    write_png(&out_dir.join(name), &rgb);
    println!("wrote {name} ({colors} distinct colors)");
    rgb
}

fn stills(out_dir: &Path) {
    // Main menu: exact boot state.
    render_still("main_menu.png", out_dir, &AppState::new(true, Some(552)));

    // Overview: all five channels with realistic measurements.
    let mut state = base_state();
    state.screen = Screen::Overview;
    let on = [
        Some((1_800u16, 420u16)),
        None,
        Some((3_300, 1_200)),
        Some((5_000, 350)),
        Some((12_000, 1_500)),
    ];
    for (channel, config) in on.iter().enumerate() {
        if let Some((mv, ma)) = config {
            state.channels[channel].requested_enabled = true;
            state.channels[channel].physical_enabled = true;
            state.channels[channel].measurement = measurement(*mv, *ma);
        }
    }
    render_still("overview.png", out_dir, &state);

    // CH5 detail: focus Voltage, enabled, CC mode, 12.00 V / 1.50 A.
    let mut state = base_state();
    state.screen = Screen::Channel(4);
    state.focus = ControlFocus::Voltage;
    let ch5 = &mut state.channels[4];
    ch5.requested_enabled = true;
    ch5.physical_enabled = true;
    ch5.regulation_mode = RegulationMode::Cc;
    ch5.current_limit_ma = 1_500;
    ch5.measurement = measurement(12_000, 1_500);
    render_still("channel5_detail.png", out_dir, &state);

    // CH5 detail with a latched OverCurrent fault (output kicked off).
    let mut state = base_state();
    state.screen = Screen::Channel(4);
    let ch5 = &mut state.channels[4];
    ch5.fault = Fault::OverCurrent;
    ch5.requested_enabled = false;
    ch5.physical_enabled = false;
    ch5.transition = OutputTransition::Stable;
    render_still("channel5_fault.png", out_dir, &state);

    // AWG: CH5, sine, 60 Hz, running, with load measurements.
    let mut state = base_state();
    state.screen = Screen::Awg;
    state.awg.channel = 4;
    state.awg.waveform = AwgWaveform::Sine;
    state.awg.frequency_millihz = 60_000;
    state.awg_status = AwgStatus::Running;
    state.channels[4].requested_enabled = true;
    state.channels[4].physical_enabled = true;
    state.awg_load = LoadMeasurement {
        milliamps_rms: 248,
        milliwatts_average: 743,
        valid: true,
    };
    render_still("awg.png", out_dir, &state);

    // USB PD input screen with the active contract.
    let mut state = base_state();
    state.screen = Screen::UsbPdInput;
    render_still("usb_pd.png", out_dir, &state);

    // Help screen (cyan section headings).
    let mut state = base_state();
    state.screen = Screen::Help;
    render_still("help.png", out_dir, &state);
}

/// Drives the reducto `App` (real reducer + real view) exclusively through
/// actions produced by `input_policy`, recording a frame after every event.
struct Recorder {
    app: App<AppReducer, BenchVoltView<Framebuffer>, 8>,
    fb: Framebuffer,
    encoder: EncoderAccumulator,
    button: ButtonTracker,
    tick: u16,
    frames: Vec<Vec<u8>>,
}

impl Recorder {
    fn new(initial: AppState) -> Self {
        let fb = Framebuffer::new();
        let mut app = App::new(BenchVoltView::new(fb.clone()), initial);
        app.render_full();
        let mut recorder = Self {
            app,
            fb,
            encoder: EncoderAccumulator::default(),
            button: ButtonTracker::new(true),
            tick: 1_000,
            frames: Vec::new(),
        };
        recorder.capture(1);
        recorder
    }

    fn capture(&mut self, count: usize) {
        let rgb = self.fb.to_rgb();
        for _ in 0..count {
            self.frames.push(rgb.clone());
        }
    }

    fn dispatch(&mut self, action: Action) {
        self.app.dispatch(action);
        self.capture(1);
    }

    /// One slow encoder detent (spacing > the 80 ms acceleration window, so
    /// the multiplier stays at 1x = fine 10 mV steps).
    fn detent(&mut self, direction: i8) {
        self.tick = self.tick.wrapping_add(200);
        let (raw, accelerated) = self.encoder.step(direction, self.tick);
        let (raw, accelerated) = clamp_adjustment(raw.into(), accelerated.into());
        if let Some(action) = encoder_action(self.app.state(), raw, accelerated) {
            self.dispatch(action);
        } else {
            self.capture(1);
        }
    }

    /// A short button press: press, release 100 ms later -> `NextControl`
    /// (which the reducer turns into ActivateMenu on menu screens).
    fn click(&mut self) {
        self.tick = self.tick.wrapping_add(400);
        assert!(self.button.sample(self.tick, false).is_none());
        self.tick = self.tick.wrapping_add(100);
        let action = self.button.sample(self.tick, true).expect("click action");
        self.dispatch(action);
    }

    fn state(&self) -> &AppState {
        self.app.state()
    }
}

fn menu_flow(out_dir: &Path) {
    let mut rec = Recorder::new(AppState::new(true, Some(552)));
    rec.capture(4); // hold on the boot main menu

    // Browse the menu a little, ending back on "DC Power".
    rec.detent(1); // -> AWG highlighted
    rec.detent(1); // -> Settings highlighted
    rec.detent(-1);
    rec.detent(-1); // back to DC Power
    rec.capture(2);

    // Select DC Power -> Overview.
    rec.click();
    assert!(rec.state().screen == Screen::Overview);

    // Feed telemetry the way the firmware monitoring loop would.
    rec.dispatch(Action::Measurements([
        Measurement::INVALID_LIKE(),
        Measurement::INVALID_LIKE(),
        Measurement::INVALID_LIKE(),
        Measurement::INVALID_LIKE(),
        Measurement::INVALID_LIKE(),
    ]));
    rec.dispatch(Action::PdNegotiated(contract_20v_5a()));
    rec.dispatch(Action::SinkMeasurement(measurement(19_960, 380)));
    rec.capture(4);

    // With no focus, encoder detents page through the channel screens:
    // Overview -> CH1 -> CH2 -> CH3 -> CH4 -> CH5.
    for expected in 0u8..5 {
        rec.detent(1);
        assert!(rec.state().screen == Screen::Channel(expected));
        rec.capture(1);
    }
    rec.capture(4);

    // Click: focus Output, click again: focus Voltage.
    rec.click();
    assert!(rec.state().focus == ControlFocus::Output);
    rec.capture(2);
    rec.click();
    assert!(rec.state().focus == ControlFocus::Voltage);
    rec.capture(2);

    // Spin the voltage up: 8 fine detents of +10 mV -> 12.08 V.
    for _ in 0..8 {
        rec.detent(1);
    }
    assert_eq!(rec.state().channels[4].setpoint_mv, 12_080);
    rec.capture(4);

    // Walk focus back around to Output: Voltage -> RegulationMode ->
    // CurrentLimit -> None -> Output.
    rec.click();
    assert!(rec.state().focus == ControlFocus::RegulationMode);
    rec.click();
    assert!(rec.state().focus == ControlFocus::CurrentLimit);
    rec.click();
    assert!(rec.state().focus == ControlFocus::None);
    rec.click();
    assert!(rec.state().focus == ControlFocus::Output);
    rec.capture(2);

    // Turn while Output is focused -> ToggleOutputRequested (real path).
    rec.detent(1);
    let operation = rec.state().channels[4].operation;
    assert!(rec.state().channels[4].transition == OutputTransition::Enabling(operation));

    // Simulate the PowerExecutor completing the enable: the plan for an
    // Enabling transition ends with OutputApplied{enabled: true} carrying the
    // same operation token (see src/power.rs).
    rec.dispatch(Action::OutputApplied {
        channel: 4,
        operation,
        enabled: true,
    });
    assert!(rec.state().channels[4].physical_enabled);

    // Fresh measurements show the channel live at the adjusted voltage.
    let mut samples = [Measurement::INVALID_LIKE(); 5];
    samples[4] = measurement(12_080, 1_500);
    rec.dispatch(Action::Measurements(samples));
    rec.capture(8); // hold the finale

    // Sanity: consecutive key frames must differ.
    let unique: std::collections::BTreeSet<&Vec<u8>> = rec.frames.iter().collect();
    assert!(
        unique.len() >= 15,
        "expected a varied animation, got {} unique frames",
        unique.len()
    );

    let file = File::create(out_dir.join("menu_flow.gif")).expect("create gif");
    let mut encoder = gif::Encoder::new(
        BufWriter::new(file),
        (WIDTH * SCALE) as u16,
        (HEIGHT * SCALE) as u16,
        &[],
    )
    .expect("gif encoder");
    encoder.set_repeat(gif::Repeat::Infinite).expect("gif repeat");
    for rgb in &rec.frames {
        let mut data = rgb.clone();
        let mut frame =
            gif::Frame::from_rgb_speed((WIDTH * SCALE) as u16, (HEIGHT * SCALE) as u16, &mut data, 10);
        frame.delay = 10; // 100 ms -> ~10 fps
        encoder.write_frame(&frame).expect("gif frame");
    }
    println!("wrote menu_flow.gif ({} frames)", rec.frames.len());
}

/// Helper because `Measurement::INVALID` is private to the app module.
trait InvalidLike {
    #[allow(non_snake_case)]
    fn INVALID_LIKE() -> Measurement;
}

impl InvalidLike for Measurement {
    fn INVALID_LIKE() -> Measurement {
        Measurement {
            millivolts: 0,
            milliamps: 0,
            valid: false,
        }
    }
}

fn main() {
    // tools/render/src -> tools -> benchvolt-poc -> Rust -> Firmware -> repo root.
    let out_dir: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../../../Images/firmware")
        .canonicalize()
        .unwrap_or_else(|_| {
            let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../../../Images/firmware");
            std::fs::create_dir_all(&dir).expect("create output dir");
            dir.canonicalize().expect("canonicalize output dir")
        });
    std::fs::create_dir_all(&out_dir).expect("create output dir");

    stills(&out_dir);
    menu_flow(&out_dir);
    println!("output: {}", out_dir.display());
}
