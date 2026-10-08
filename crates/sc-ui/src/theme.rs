use crate::Color;

/// Colour tokens. Flask has one look: warm dark with cream and milk-white.
#[derive(Clone, Copy, PartialEq)]
pub struct Theme {
    /// Window background.
    pub ground: Color,
    pub titlebar: Color,
    /// Recessed areas: the tab switcher well, graph backgrounds.
    pub well: Color,
    /// Rounded content cards.
    pub card: Color,
    /// Chips and unselected controls sitting on a card.
    pub raised: Color,
    /// Selected row; also hairlines.
    pub selected: Color,
    pub line: Color,
    /// Secondary button fill.
    pub cocoa: Color,
    pub text: Color,
    pub text_dim: Color,
    /// Primary button fill (bottom of its gradient) and usage bars.
    pub milk: Color,
    pub milk_hi: Color,
    /// Text on milk.
    pub ink: Color,
    /// "Worth checking" state.
    pub butter: Color,
    /// Destructive actions and failed checks.
    pub peach: Color,
    pub peach_ink: Color,
}

impl Theme {
    pub const COCOA: Theme = Theme {
        ground: Color(0x1d1b19),
        titlebar: Color(0x211f1c),
        well: Color(0x181614),
        card: Color(0x262320),
        raised: Color(0x34302b),
        selected: Color(0x3a3530),
        line: Color(0x3a3530),
        cocoa: Color(0x4a443d),
        text: Color(0xf3ece0),
        text_dim: Color(0xa89f92),
        milk: Color(0xf1e9da),
        milk_hi: Color(0xfffdf8),
        ink: Color(0x2a2622),
        butter: Color(0xf2cf7a),
        peach: Color(0xf2a497),
        peach_ink: Color(0x3a1712),
    };
}
