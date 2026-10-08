//! Motion: values that ease toward a target while the window keeps
//! repainting, and stop costing anything once they arrive.

use std::cell::Cell;
use std::time::Instant;

use windows::Win32::UI::WindowsAndMessaging::{
    SPI_GETCLIENTAREAANIMATION, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS, SystemParametersInfoW,
};
use windows::core::BOOL;

thread_local! {
    static START: Instant = Instant::now();
    /// When the frame being painted began, in seconds.
    static NOW: Cell<f64> = const { Cell::new(0.0) };
    /// Something painted this frame has not arrived yet.
    static BUSY: Cell<bool> = const { Cell::new(false) };
    static OFF: Cell<bool> = const { Cell::new(false) };
}

/// Turns all motion on or off. Off, every [`Anim`] is at its target at once.
pub fn set_enabled(on: bool) {
    OFF.set(!on);
}

/// Whether Windows itself is set to show animations.
pub fn system_enabled() -> bool {
    let mut on = BOOL(1);
    unsafe {
        let _ = SystemParametersInfoW(
            SPI_GETCLIENTAREAANIMATION,
            0,
            Some(&mut on as *mut BOOL as *mut _),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        );
    }
    on.as_bool()
}

pub(crate) fn begin_frame() {
    NOW.set(START.with(|s| s.elapsed().as_secs_f64()));
    BUSY.set(false);
}

/// Whether the frame just painted needs another after it.
pub(crate) fn busy() -> bool {
    BUSY.get()
}

/// Where a value is `t` of the way (0..=1) from `from` to `to`: quick at
/// first, settling at the end.
fn ease(from: f32, to: f32, t: f32) -> f32 {
    if t >= 1.0 {
        return to;
    }
    let left = 1.0 - t.max(0.0);
    from + (to - from) * (1.0 - left * left * left)
}

/// A value that eases toward whatever target it is last asked for. Ask from
/// `paint`; the window repaints until every value asked for has arrived.
#[derive(Default)]
pub struct Anim {
    /// Where it set off from, where it is going, and when it set off.
    state: Cell<Option<(f32, f32, f64)>>,
}

impl Anim {
    /// The value now, on its way to `target` over `secs`. The first call, a
    /// `secs` of zero, and motion being off all give `target` at once.
    pub fn get(&self, target: f32, secs: f32) -> f32 {
        let now = NOW.get();
        let at = |(from, to, start): (f32, f32, f64)| ease(from, to, ((now - start) / secs as f64) as f32);
        let state = match self.state.get() {
            _ if secs <= 0.0 || OFF.get() => (target, target, now),
            Some(s) if s.1 == target => s,
            // Redirected on the way: carry on from wherever it had got to.
            Some(s) => (at(s), target, now),
            None => (target, target, now),
        };
        self.state.set(Some(state));
        let value = at(state);
        if value != target {
            BUSY.set(true);
        }
        value
    }

    /// Puts the value at `v`, so the next `get` sets off from there.
    pub fn jump(&self, v: f32) {
        self.state.set(Some((v, v, NOW.get())));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_value_eases_to_its_target_and_can_be_redirected_or_switched_off() {
        assert_eq!((ease(0.0, 10.0, 0.0), ease(0.0, 10.0, 1.0), ease(0.0, 10.0, 7.0)), (0.0, 10.0, 10.0));
        assert!(ease(0.0, 10.0, 0.5) > 5.0, "fast first");

        let a = Anim::default();
        NOW.set(1.0);
        assert_eq!(a.get(4.0, 1.0), 4.0, "first call is already there");
        assert!(!busy());
        assert_eq!(a.get(8.0, 1.0), 4.0, "sets off from where it was");
        assert!(busy());
        NOW.set(1.5);
        let mid = a.get(8.0, 1.0);
        assert!(mid > 6.0 && mid < 8.0);
        assert_eq!(a.get(0.0, 1.0), mid, "redirected from where it had got to");
        NOW.set(2.5);
        BUSY.set(false);
        assert_eq!(a.get(0.0, 1.0), 0.0);
        assert!(!busy());

        set_enabled(false);
        assert_eq!(a.get(9.0, 1.0), 9.0);
        set_enabled(true);
        a.jump(2.0);
        assert_eq!(a.get(3.0, 0.0), 3.0, "no time, no motion");
    }
}
