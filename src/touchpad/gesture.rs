//! Touchpad gestures: from frames with contacts — cursor motion, buttons and the wheel.
//!
//! - One finger moves the cursor, with acceleration: the faster the finger, the farther the cursor.
//! - Two fingers scroll: the content follows the fingers.
//! - A short tap with one finger is a click, with two — a right click.
//! - Pressing the pad is the left button; in the bottom right corner or with two fingers — the right one.
//!
//! The module knows nothing about hardware: it can be tested with frames recorded in advance.

use alloc::vec::Vec;

/// How many cursor pixels per millimetre of finger travel for slow motion, and how many
/// times more for fast motion.
const BASE_GAIN: f32 = 5.5;
const MAX_ACCEL: f32 = 2.8;
/// Finger speeds (mm/s) between which the acceleration grows from 1 to `MAX_ACCEL`.
const ACCEL_FROM: f32 = 25.0;
const ACCEL_TO: f32 = 160.0;
/// How many millimetres of two-finger scrolling per wheel click.
const SCROLL_STEP: f32 = 2.6;
/// A contact shorter than this and with travel below `TAP_TRAVEL` is a click.
const TAP_MS: f32 = 200.0;
const TAP_TRAVEL: f32 = 1.6;
/// After pressing the pad the finger twitches: the cursor stays still for this many milliseconds.
const PRESS_SETTLE_MS: f32 = 90.0;
/// The bottom right corner for the right button: fractions of the width and height from the top left corner.
const RIGHT_ZONE_X: f32 = 0.55;
const RIGHT_ZONE_Y: f32 = 0.72;
/// A jump larger than this in one frame is not motion but a new finger with the same id.
const MAX_STEP_MM: f32 = 15.0;

pub const LEFT: u8 = 1;
pub const RIGHT: u8 = 2;

/// A contact in millimetres from the top left corner.
#[derive(Clone, Copy, Debug)]
pub struct Contact {
    pub id: u32,
    pub x: f32,
    pub y: f32,
}

#[derive(Clone, Debug, Default)]
pub struct Frame {
    /// Only fingers on the pad (not a palm).
    pub contacts: Vec<Contact>,
    /// The pad is pressed.
    pub button: bool,
    /// The frame time in milliseconds (from any origin).
    pub time: f32,
}

/// What to pass to the system: the cursor delta, held buttons, wheel clicks.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Step {
    pub dx: i32,
    pub dy: i32,
    pub buttons: u8,
    pub wheel: i32,
}

pub struct Gestures {
    width: f32,
    height: f32,
    last: Vec<Contact>,
    last_time: f32,
    /// The contact started then, there were up to this many fingers, the travel of the farthest.
    touch_start: Option<f32>,
    max_fingers: usize,
    travel: f32,
    starts: Vec<Contact>,
    /// The pad was pressed during this contact: then it is not a tap click.
    pressed_in_touch: bool,
    /// The held pad button and when it was pressed.
    held: u8,
    pressed_at: f32,
    /// Fractional remainders of motion and scrolling.
    rest_x: f32,
    rest_y: f32,
    scroll: f32,
    /// Which buttons the system sees as held.
    sent_buttons: u8,
}

impl Gestures {
    pub fn new(width_mm: f32, height_mm: f32) -> Self {
        Self {
            width: width_mm.max(10.0),
            height: height_mm.max(10.0),
            last: Vec::new(),
            last_time: 0.0,
            touch_start: None,
            max_fingers: 0,
            travel: 0.0,
            starts: Vec::new(),
            pressed_in_touch: false,
            held: 0,
            pressed_at: 0.0,
            rest_x: 0.0,
            rest_y: 0.0,
            scroll: 0.0,
            sent_buttons: 0,
        }
    }

    /// Handles a frame. There may be several steps: a tap click is a press
    /// and a release.
    pub fn frame(&mut self, f: &Frame) -> Vec<Step> {
        let mut out = Vec::new();
        let dt = (f.time - self.last_time).max(1.0);
        let fingers = f.contacts.len();

        // The start and continuation of a contact.
        if fingers > 0 && self.touch_start.is_none() {
            self.touch_start = Some(f.time);
            self.max_fingers = 0;
            self.travel = 0.0;
            self.starts.clear();
            self.pressed_in_touch = false;
            self.scroll = 0.0;
        }
        self.max_fingers = self.max_fingers.max(fingers);
        for c in &f.contacts {
            match self.starts.iter().find(|s| s.id == c.id) {
                Some(s) => self.travel = self.travel.max(distance(s, c)),
                None => self.starts.push(*c),
            }
        }

        // The button under the pad.
        let mut buttons = self.held;
        if f.button && self.held == 0 {
            let right_corner = f.contacts.iter().any(|c| c.x > self.width * RIGHT_ZONE_X && c.y > self.height * RIGHT_ZONE_Y)
                && !f.contacts.iter().any(|c| c.y < self.height * RIGHT_ZONE_Y);
            self.held = if fingers >= 2 || right_corner { RIGHT } else { LEFT };
            self.pressed_at = f.time;
            self.pressed_in_touch = true;
            buttons = self.held;
        } else if !f.button && self.held != 0 {
            self.held = 0;
            buttons = 0;
        }

        // Finger motion since the previous frame (only those that were there then too).
        let moves: Vec<(f32, f32)> = f
            .contacts
            .iter()
            .filter_map(|c| self.last.iter().find(|p| p.id == c.id).map(|p| (c.x - p.x, c.y - p.y)))
            .filter(|(dx, dy)| dx.abs() < MAX_STEP_MM && dy.abs() < MAX_STEP_MM)
            .collect();

        let mut step = Step { buttons, ..Step::default() };
        let settling = self.held != 0 && f.time - self.pressed_at < PRESS_SETTLE_MS;
        if fingers == 2 && self.held == 0 && moves.len() == 2 {
            // Scrolling: the content follows the fingers, so a finger moving down is the wheel up.
            self.scroll += (moves[0].1 + moves[1].1) / 2.0;
            let clicks = (self.scroll / SCROLL_STEP) as i32;
            if clicks != 0 {
                self.scroll -= clicks as f32 * SCROLL_STEP;
                step.wheel = -clicks;
            }
        } else if !settling && !moves.is_empty() && (fingers == 1 || self.held != 0) {
            // The cursor follows the finger that moved most (with the pad pressed
            // the second finger usually just rests on the button).
            let (mx, my) = moves.iter().copied().fold((0.0f32, 0.0f32), |best, m| if len(m) > len(best) { m } else { best });
            let speed = len((mx, my)) / dt * 1000.0;
            let gain = BASE_GAIN * accel(speed);
            self.rest_x += mx * gain;
            self.rest_y += my * gain;
            step.dx = self.rest_x as i32;
            step.dy = self.rest_y as i32;
            self.rest_x -= step.dx as f32;
            self.rest_y -= step.dy as f32;
        }
        if step.dx != 0 || step.dy != 0 || step.wheel != 0 || buttons != self.sent_buttons {
            out.push(step);
        }

        // The end of a contact: a short contact without a press is a click.
        if fingers == 0
            && let Some(start) = self.touch_start.take()
        {
            let quick = f.time - start <= TAP_MS && self.travel < TAP_TRAVEL;
            if quick && !self.pressed_in_touch && self.held == 0 {
                let button = if self.max_fingers >= 2 { RIGHT } else { LEFT };
                out.push(Step { buttons: button, ..Step::default() });
                out.push(Step::default());
            }
            self.rest_x = 0.0;
            self.rest_y = 0.0;
        }

        self.sent_buttons = out.last().map_or(self.sent_buttons, |s| s.buttons);
        self.last = f.contacts.clone();
        self.last_time = f.time;
        out
    }
}

fn len((x, y): (f32, f32)) -> f32 {
    libm::sqrtf(x * x + y * y)
}

fn distance(a: &Contact, b: &Contact) -> f32 {
    len((a.x - b.x, a.y - b.y))
}

/// Acceleration: 1 for a slow finger, smoothly up to `MAX_ACCEL` for a fast one.
fn accel(speed: f32) -> f32 {
    let t = ((speed - ACCEL_FROM) / (ACCEL_TO - ACCEL_FROM)).clamp(0.0, 1.0);
    // A smooth ramp: slowly at first, then faster.
    1.0 + (MAX_ACCEL - 1.0) * t * t * (3.0 - 2.0 * t)
}
