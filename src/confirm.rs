//! The "Keep these display settings?" prompt.
//!
//! After a persistent change, the daemon runs itself again as
//! `regolith-displayd --confirm-prompt <output> <milliseconds>` (see
//! [`PROMPT_ARG`]). That process shows the prompt on `<output>` as a
//! wlr-layer-shell surface on the overlay layer, so that it shows above
//! fullscreen windows, with exclusive keyboard interactivity, so that it gets
//! the keys without a click even while a fullscreen window has focus. It
//! counts the seconds left down to zero.
//!
//! It tells the daemon what happens on its standard output, one word a line:
//! `ready` once the prompt is on screen and has the keyboard, then `keep`
//! (Enter or keypad Enter) or `revert` (Escape). It exits after answering,
//! when its time is up or when its surface is closed. The daemon owns the
//! timeout and the configuration: the prompt only asks.

use ab_glyph::{Font, FontRef, PxScale, ScaleFont};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_compositor, delegate_keyboard, delegate_layer, delegate_output, delegate_registry,
    delegate_seat, delegate_shm,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use std::io::Write;
use std::time::{Duration, Instant};
use wayland_client::{
    globals::registry_queue_init,
    protocol::{wl_callback, wl_keyboard, wl_output, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, QueueHandle,
};

/// The command line argument that makes `regolith-displayd` show the prompt.
pub const PROMPT_ARG: &str = "--confirm-prompt";

const WIDTH: u32 = 560;
const HEIGHT: u32 = 150;
const BORDER: u32 = 3;
const BACKGROUND: u32 = 0xff2b2b2b;
const FRAME: u32 = 0xffe0e0e0;
const FONTS: [&str; 2] = [
    "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
    "/usr/share/fonts/TTF/DejaVuSans.ttf",
];

/// What the user answered.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    Keep,
    Revert,
}

impl Answer {
    /// The answer a line of the prompt's output gives, if any.
    pub fn from_line(line: &str) -> Option<Answer> {
        match line.trim() {
            "keep" => Some(Answer::Keep),
            "revert" => Some(Answer::Revert),
            _ => None,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Answer::Keep => "keep",
            Answer::Revert => "revert",
        }
    }
}

/// The answer that a key gives: Enter keeps, Escape reverts.
pub fn answer_for_key(key: Keysym) -> Option<Answer> {
    match key {
        Keysym::Return | Keysym::KP_Enter => Some(Answer::Keep),
        Keysym::Escape => Some(Answer::Revert),
        _ => None,
    }
}

/// The number of whole seconds to show for `left`: 19.2 s left shows 20.
pub fn seconds_shown(left: Duration) -> u64 {
    left.as_millis().div_ceil(1000) as u64
}

/// How long until the number shown for `left` goes down by one.
fn until_next_second(left: Duration) -> Duration {
    let millis = left.as_millis() as u64;
    match millis % 1000 {
        0 => Duration::from_millis(1000.min(millis.max(1))),
        rest => Duration::from_millis(rest),
    }
}

/// Run the prompt for `output` with `millis` milliseconds left, and return
/// the process exit code.
pub fn run_prompt(output: &str, millis: u64) -> i32 {
    match run(output, Duration::from_millis(millis)) {
        Ok(()) => 0,
        Err(e) => {
            eprintln!("regolith-displayd: cannot show the confirmation prompt: {e}");
            1
        }
    }
}

struct Prompt {
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    shm: Shm,
    pool: SlotPool,
    layer: Option<LayerSurface>,
    keyboard: Option<wl_keyboard::WlKeyboard>,
    configured: bool,
    announced: bool,
    deadline: Instant,
    shown: u64,
    done: bool,
    font: Option<Vec<u8>>,
}

fn run(output: &str, left: Duration) -> Result<(), Box<dyn std::error::Error>> {
    let conn = Connection::connect_to_env()?;
    let (globals, mut event_queue) = registry_queue_init(&conn)?;
    let qh: QueueHandle<Prompt> = event_queue.handle();
    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;
    let pool = SlotPool::new((WIDTH * HEIGHT * 4) as usize, &shm)?;
    let deadline = Instant::now() + left;
    let mut prompt = Prompt {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        shm,
        pool,
        layer: None,
        keyboard: None,
        configured: false,
        announced: false,
        deadline,
        shown: seconds_shown(left),
        done: false,
        font: FONTS.iter().find_map(|path| std::fs::read(path).ok()),
    };
    // The outputs' names, and the seat's keyboard.
    event_queue.roundtrip(&mut prompt)?;
    event_queue.roundtrip(&mut prompt)?;
    let target = prompt.output_state.outputs().find(|o| {
        prompt
            .output_state
            .info(o)
            .and_then(|info| info.name)
            .is_some_and(|name| name == output)
    });
    let surface = compositor.create_surface(&qh);
    // The overlay layer shows above fullscreen windows; the top layer doesn't.
    let layer = layer_shell.create_layer_surface(
        &qh,
        surface,
        Layer::Overlay,
        Some("regolith-display-confirm"),
        target.as_ref(),
    );
    // Centred on the output.
    layer.set_anchor(Anchor::empty());
    // Exclusive: the keys come here at once, even from a fullscreen window.
    layer.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
    layer.set_size(WIDTH, HEIGHT);
    layer.commit();
    prompt.layer = Some(layer);

    let mut event_loop: EventLoop<Prompt> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|e| e.to_string())?;
    event_loop
        .handle()
        .insert_source(Timer::from_duration(until_next_second(left)), |_, _, p| {
            let left = p.deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                p.done = true;
                return TimeoutAction::Drop;
            }
            let shown = seconds_shown(left);
            if shown != p.shown {
                p.shown = shown;
                p.draw();
            }
            TimeoutAction::ToDuration(until_next_second(left))
        })
        .map_err(|e| e.to_string())?;
    while !prompt.done {
        event_loop.dispatch(Duration::from_millis(500), &mut prompt)?;
    }
    Ok(())
}

impl Prompt {
    /// Tell the daemon `word`. A daemon that is gone can't use the prompt.
    fn tell(&mut self, word: &str) {
        let mut out = std::io::stdout().lock();
        if writeln!(out, "{word}").and_then(|_| out.flush()).is_err() {
            self.done = true;
        }
    }

    fn answer(&mut self, answer: Answer) {
        if !self.done {
            self.tell(answer.word());
            self.done = true;
        }
    }

    fn draw(&mut self) {
        if !self.configured {
            return;
        }
        let Some(layer) = self.layer.as_ref() else {
            return;
        };
        let stride = WIDTH as i32 * 4;
        let Ok((buffer, canvas)) =
            self.pool
                .create_buffer(WIDTH as i32, HEIGHT as i32, stride, wl_shm::Format::Argb8888)
        else {
            return;
        };
        for (i, px) in canvas.chunks_exact_mut(4).enumerate() {
            let x = i as u32 % WIDTH;
            let y = i as u32 / WIDTH;
            let frame = x < BORDER || y < BORDER || x >= WIDTH - BORDER || y >= HEIGHT - BORDER;
            px.copy_from_slice(&(if frame { FRAME } else { BACKGROUND }).to_le_bytes());
        }
        let countdown = format!(
            "Settings changes will revert in {} second{}",
            self.shown,
            if self.shown == 1 { "" } else { "s" }
        );
        let lines = [
            ("Keep these display settings?", 26.0, 20.0),
            (countdown.as_str(), 20.0, 66.0),
            ("Enter: Keep Changes      Esc: Revert Settings", 18.0, 106.0),
        ];
        if let Some(font) = self.font.as_deref().and_then(|f| FontRef::try_from_slice(f).ok()) {
            for (text, size, top) in lines {
                draw_text(canvas, &font, text, size, 20.0, top);
            }
        }
        let surface = layer.wl_surface();
        surface.damage_buffer(0, 0, WIDTH as i32, HEIGHT as i32);
        if buffer.attach_to(surface).is_ok() {
            layer.commit();
        }
    }
}

/// Draw `text` in light grey on the background, with its top at `top`.
fn draw_text(canvas: &mut [u8], font: &FontRef, text: &str, size: f32, left: f32, top: f32) {
    let scaled = font.as_scaled(PxScale::from(size));
    let mut x = left;
    let baseline = top + scaled.ascent();
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        let glyph = id.with_scale_and_position(size, ab_glyph::point(x, baseline));
        x += scaled.h_advance(id);
        let Some(outline) = font.outline_glyph(glyph) else {
            continue;
        };
        let bounds = outline.px_bounds();
        outline.draw(|gx, gy, coverage| {
            let px = bounds.min.x as i32 + gx as i32;
            let py = bounds.min.y as i32 + gy as i32;
            if px < 0 || py < 0 || px >= WIDTH as i32 || py >= HEIGHT as i32 {
                return;
            }
            let i = ((py as u32 * WIDTH + px as u32) * 4) as usize;
            let v = (0x2b as f32 + (0xf0 - 0x2b) as f32 * coverage.min(1.0)) as u8;
            canvas[i..i + 4].copy_from_slice(&[v, v, v, 0xff]);
        });
    }
}

impl CompositorHandler for Prompt {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Prompt {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Prompt {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &LayerSurface) {
        // The output went away: the daemon's timeout still applies.
        self.done = true;
    }

    fn configure(&mut self, conn: &Connection, qh: &QueueHandle<Self>, _: &LayerSurface, _: LayerSurfaceConfigure, _: u32) {
        let first = !self.configured;
        self.configured = true;
        self.draw();
        if first {
            // Once the compositor has handled the first frame, the prompt is
            // mapped and has the keyboard: the sync's reply says so.
            conn.display().sync(qh, ());
        }
    }
}

impl Dispatch<wl_callback::WlCallback, ()> for Prompt {
    fn event(state: &mut Self, _: &wl_callback::WlCallback, event: wl_callback::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_callback::Event::Done { .. } = event {
            if !state.announced {
                state.announced = true;
                state.tell("ready");
            }
        }
    }
}

impl SeatHandler for Prompt {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard && self.keyboard.is_none() {
            self.keyboard = self.seat_state.get_keyboard(qh, &seat, None).ok();
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Keyboard {
            if let Some(keyboard) = self.keyboard.take() {
                keyboard.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for Prompt {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32, _: &[u32], _: &[Keysym]) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: &wl_surface::WlSurface, _: u32) {}
    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        if let Some(answer) = answer_for_key(event.keysym) {
            self.answer(answer);
        }
    }
    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: KeyEvent) {}
    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, _: Modifiers, _: u32) {}
}

impl ShmHandler for Prompt {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

impl ProvidesRegistryState for Prompt {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
    registry_handlers![OutputState, SeatState];
}

delegate_compositor!(Prompt);
delegate_output!(Prompt);
delegate_shm!(Prompt);
delegate_seat!(Prompt);
delegate_keyboard!(Prompt);
delegate_layer!(Prompt);
delegate_registry!(Prompt);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enter_keeps_and_escape_reverts() {
        assert_eq!(answer_for_key(Keysym::Return), Some(Answer::Keep));
        assert_eq!(answer_for_key(Keysym::KP_Enter), Some(Answer::Keep));
        assert_eq!(answer_for_key(Keysym::Escape), Some(Answer::Revert));
        assert_eq!(answer_for_key(Keysym::space), None);
    }

    #[test]
    fn answers_read_back() {
        for answer in [Answer::Keep, Answer::Revert] {
            assert_eq!(Answer::from_line(&format!("{}\n", answer.word())), Some(answer));
        }
        assert_eq!(Answer::from_line("ready"), None);
    }

    #[test]
    fn countdown_rounds_up() {
        assert_eq!(seconds_shown(Duration::from_millis(20_000)), 20);
        assert_eq!(seconds_shown(Duration::from_millis(19_001)), 20);
        assert_eq!(seconds_shown(Duration::from_millis(19_000)), 19);
        assert_eq!(seconds_shown(Duration::from_millis(1)), 1);
        assert_eq!(seconds_shown(Duration::ZERO), 0);
        assert_eq!(until_next_second(Duration::from_millis(19_250)), Duration::from_millis(250));
        assert_eq!(until_next_second(Duration::from_millis(19_000)), Duration::from_millis(1000));
    }
}
