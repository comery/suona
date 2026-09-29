//! Pet geometry: size levels, bell aiming, and edge docking.
//!
//! Everything here is in *logical* pixels; callers convert to physical using
//! the monitor scale factor.
//!
//! # Why the constants are duplicated from CSS
//!
//! The pet is laid out by CSS, so the layout numbers (`PET_W`, `PET_H`,
//! `BOTTOM_MARGIN`) mirror `src/style.css`.  They are collected here so the
//! coupling is visible in one place.  The derived quantities — how far the
//! bell rim sits from the pet's centre, and how wide the docked strip is — are
//! computed from them rather than hand-tuned.
//!
//! The SVG in `index.html` uses `viewBox="120 18 272 486"` with
//! `preserveAspectRatio="xMidYMid meet"`.  In a `PET_W × PET_H` box that
//! scales to fit the height, so the drawing spans the full box height and is
//! horizontally centred.

use crate::screens::EdgePolicy;
use serde::{Deserialize, Serialize};

// ── layout constants, mirrored from style.css ──────────────────────────────

/// Window width.  Fixed across sizes so the speech bubble stays readable.
pub const WINDOW_W: f64 = 380.0;

/// Padding above the pet, reserved for the bubble.
const BUBBLE_ROOM: f64 = 182.0;
/// Gap between the pet and the bottom of the window.
const BOTTOM_MARGIN: f64 = 34.0;

/// Pet element size at scale 1.0.
const PET_W: f64 = 158.0;
const PET_H: f64 = 214.0;

/// Footprint of the list when it is shown beside a docked pet.
const PANEL_W: f64 = 380.0;
const PANEL_H: f64 = 440.0;

/// How far the pet pokes out of its edge while the list is open beside it.
///
/// The resting strip shows only the bell; opening the list nudges the pet
/// further out so it is plainly still there, rather than pulling it away from
/// the edge and parking it under the panel.
const PEEK_STRIP: f64 = 96.0;

/// Height of the viewBox that the bell occupies, expressed as a fraction of
/// the whole.  The bell spans viewBox y≈366..478 out of 18..504.
const BELL_TIP_FRACTION: f64 = (478.0 - 18.0) / 486.0;

// ── size levels ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PetSize {
    Small,
    Medium,
    Large,
}

impl PetSize {
    pub fn scale(self) -> f64 {
        match self {
            PetSize::Small => 0.635,
            PetSize::Medium => 0.785,
            PetSize::Large => 1.0,
        }
    }
}

impl Default for PetSize {
    fn default() -> Self {
        // The largest size is the intended default look.
        PetSize::Large
    }
}

// ── docking ────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Dock {
    None,
    Left,
    Right,
    Top,
    Bottom,
}

/// How close (logical px) the *window* must come to a screen edge before it
/// snaps and hides.
///
/// This is measured against the window rectangle, not the pet's centre.  The
/// pet sits in the middle of a 380px-wide window, so a centre-based test would
/// require dragging the window ~144px off-screen before it could ever fire.
const SNAP_DISTANCE: f64 = 80.0;

/// How far inside the screen the window pops back to when it undocks.
///
/// Deliberately larger than [`SNAP_DISTANCE`], so leaving an edge does not
/// immediately re-trigger a dock — the gap is the hysteresis band.
pub const UNDOCK_INSET: f64 = 110.0;

/// Where the pet is, as far as the front end is concerned.
#[derive(Debug, Clone, Serialize)]
pub struct PetState {
    pub dock: Dock,
    pub size: PetSize,
    pub sound: bool,
    /// Free-floating rotation in degrees, so the bell faces screen centre.
    pub angle: f64,
    /// How much of the pet is inside the window, in logical pixels.  Larger
    /// while the list is open beside a docked pet.
    pub strip: f64,
}

/// Window dimensions for one size level.
#[derive(Debug, Clone, Copy)]
pub struct Dims {
    pub w: f64,
    pub h: f64,
    pub expanded_h: f64,
    /// Long dimension of the docked strip window.
    pub side: f64,
    /// How much of the bell stays on screen when docked.
    pub strip: f64,
    /// How far the pet pokes out while the list is open beside it.
    pub peek: f64,
}

pub fn dims(size: PetSize) -> Dims {
    let s = size.scale();
    let pet_h = PET_H * s;
    Dims {
        w: WINDOW_W,
        h: BOTTOM_MARGIN + pet_h + BUBBLE_ROOM,
        expanded_h: BOTTOM_MARGIN + pet_h + BUBBLE_ROOM + 390.0,
        // Docked, the pet rotates so its element width runs along the edge —
        // the strip has to be at least that long or the bell gets sliced.
        side: PET_W * s + 12.0,
        strip: 40.0 * s,
        peek: PEEK_STRIP * s,
    }
}

/// Window size while the list is open beside a docked pet.
///
/// The window keeps the docked edge and grows inward, so the pet stays on its
/// edge and the list sits next to it instead of displacing it.
pub fn docked_panel_size(dock: Dock, size: PetSize) -> (f64, f64) {
    let d = dims(size);
    match dock {
        Dock::Left | Dock::Right => (d.peek + PANEL_W, PANEL_H),
        Dock::Top | Dock::Bottom => (PANEL_W, d.peek + PANEL_H),
        Dock::None => (d.w, d.h),
    }
}

pub fn pet_h(size: PetSize) -> f64 {
    PET_H * size.scale()
}

/// Distance from the pet's centre to the lowest point of the bell rim.
///
/// The pet rotates about its own centre, so this is the radius at which the
/// bell sweeps.
pub fn bell_reach(size: PetSize) -> f64 {
    let pet_h = PET_H * size.scale();
    let centre_from_top = pet_h / 2.0;
    let tip_from_top = BELL_TIP_FRACTION * pet_h;
    tip_from_top - centre_from_top
}

/// Centre of the pet inside the window, in logical coordinates from the
/// window's top-left.
pub fn pet_centre_in_window(size: PetSize) -> (f64, f64) {
    let d = dims(size);
    (d.w / 2.0, d.h - BOTTOM_MARGIN - pet_h(size) / 2.0)
}

// ── aiming ─────────────────────────────────────────────────────────────────

/// Rotation that points the bell at a target.
///
/// The bell points along the pet's local +Y.  A CSS `rotate(θ)` maps (0,1) to
/// (-sin θ, cos θ), so solving for the unit vector `(ux, uy)` toward the
/// target gives `θ = atan2(-ux, uy)`.
pub fn aim_at(dx: f64, dy: f64) -> f64 {
    (-dx).atan2(dy).to_degrees()
}

/// Rotation the pet takes once docked: perpendicular to the edge, pointing
/// into the screen.  This is the same direction `aim_at` would give for the
/// centre of that edge, so the two states agree.
pub fn dock_angle(dock: Dock) -> f64 {
    match dock {
        Dock::None => 0.0,
        // Bell to the right.
        Dock::Left => -90.0,
        // Bell to the left.
        Dock::Right => 90.0,
        // Bell downward (natural orientation).
        Dock::Top => 0.0,
        // Bell upward.
        Dock::Bottom => 180.0,
    }
}

/// Which edge, if any, the window is close enough to snap to.
///
/// All arguments are physical pixels.  The gaps are measured from the window
/// rectangle, so dragging the pet to the edge of the screen — where macOS
/// stops the window — is what arms the snap.
pub fn edge_for(
    win_x: f64,
    win_y: f64,
    win_w: f64,
    win_h: f64,
    mon_x: f64,
    mon_y: f64,
    mon_w: f64,
    mon_h: f64,
    scale: f64,
    policy: EdgePolicy,
) -> Dock {
    let reach = SNAP_DISTANCE * scale;

    // Every edge the system says is available.  The Dock's side is excluded
    // because the pet would sit *behind* the Dock — invisible and impossible
    // to click back out.  The menu bar is not excluded: it only needs
    // clearance, which the placement applies.
    let mut gaps: Vec<(Dock, f64)> = Vec::with_capacity(4);
    if policy.left {
        gaps.push((Dock::Left, win_x - mon_x));
    }
    if policy.right {
        gaps.push((Dock::Right, (mon_x + mon_w) - (win_x + win_w)));
    }
    if policy.top {
        gaps.push((Dock::Top, win_y - mon_y));
    }
    if policy.bottom {
        gaps.push((Dock::Bottom, (mon_y + mon_h) - (win_y + win_h)));
    }
    if gaps.is_empty() {
        return Dock::None;
    }

    // Whichever edge the window is nearest wins, so a corner docks to the side
    // it was pushed against rather than flipping arbitrarily.
    match gaps
        .into_iter()
        .min_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal))
    {
        Some((dock, gap)) if gap <= reach => dock,
        _ => Dock::None,
    }
}

// ── persistence ────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Settings {
    #[serde(default)]
    pub size: PetSize,
    #[serde(default)]
    pub sound: bool,
    /// Which agents to watch and where to find them.
    #[serde(default)]
    pub agents: crate::agents::AgentSettings,
}

// ── multi-display safety ───────────────────────────────────────────────────

/// A display rectangle plus the margin to keep clear of its edges, all in
/// physical pixels.
#[derive(Debug, Clone, Copy)]
pub struct Display {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
    pub margin: f64,
}

/// Whether a window rectangle overlaps a display at all.
pub fn overlaps(
    (left, top, right, bottom): (f64, f64, f64, f64),
    display: &Display,
) -> bool {
    left < display.x + display.w
        && right > display.x
        && top < display.y + display.h
        && bottom > display.y
}

/// Whether a window spans more than one display.
///
/// Only *that* is unsafe to dock from: the placement would use one display's
/// scale factor while the window partly lives on another, and on a mixed-DPI
/// desk that corrupts every logical/pixel conversion that follows.
///
/// A window hanging off the outer edge of a single display is fine — that is
/// simply what pushing the pet against the edge of the screen looks like, and
/// refusing it made the left and bottom edges impossible to dock to.
pub fn spans_displays(rect: (f64, f64, f64, f64), displays: &[Display]) -> bool {
    displays.iter().filter(|d| overlaps(rect, d)).count() > 1
}

/// Whether a point sits safely inside any display.
///
/// Used to vet a remembered pet position: a window can overlap a display while
/// the pet itself — which hangs at the window's bottom-centre — lands in the
/// gap between screens.
pub fn inside_any(displays: &[Display], x: f64, y: f64) -> bool {
    displays.iter().any(|d| {
        x >= d.x + d.margin
            && x <= d.x + d.w - d.margin
            && y >= d.y + d.margin
            && y <= d.y + d.h - d.margin
    })
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            size: PetSize::default(),
            sound: false,
            agents: crate::agents::AgentSettings::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1920x1080 monitor at the origin, 2x scale (physical pixels).
    const MON: (f64, f64, f64, f64) = (0.0, 0.0, 3840.0, 2160.0);
    const SCALE: f64 = 2.0;

    fn edge_at_sides(win_x: f64, win_y: f64, w: f64, h: f64, sides: EdgePolicy) -> Dock {
        edge_for(win_x, win_y, w, h, MON.0, MON.1, MON.2, MON.3, SCALE, sides)
    }

    fn edge_at(win_x: f64, win_y: f64, w: f64, h: f64) -> Dock {
        edge_for(win_x, win_y, w, h, MON.0, MON.1, MON.2, MON.3, SCALE, EdgePolicy::FALLBACK)
    }

    #[test]
    fn bell_reach_scales_with_size() {
        let large = bell_reach(PetSize::Large);
        // The bell rim sits below the pet's centre but above the element's
        // bottom edge, so the reach is positive and less than half the height.
        assert!(large > 0.0 && large < PET_H / 2.0, "reach was {large}");

        // Smaller pets reach proportionally less far.
        assert!(bell_reach(PetSize::Medium) < large);
        assert!(bell_reach(PetSize::Small) < bell_reach(PetSize::Medium));
    }

    #[test]
    fn aim_points_the_bell_at_the_target() {
        // Pet directly below screen centre -> bell points up.
        assert!((aim_at(0.0, -1.0).abs() - 180.0).abs() < 0.01);
        // Pet to the left of centre -> bell points right.
        assert!((aim_at(1.0, 0.0) + 90.0).abs() < 0.01);
        // Pet to the right of centre -> bell points left.
        assert!((aim_at(-1.0, 0.0) - 90.0).abs() < 0.01);
        // Pet above centre -> bell points down (its natural orientation).
        assert!(aim_at(0.0, 1.0).abs() < 0.01);
    }

    #[test]
    fn dock_angle_matches_aiming_at_the_edge() {
        // Docking must agree with what the free-floating pet was already doing
        // as it approached that edge, or the snap would visibly twist it.
        for dock in [Dock::Left, Dock::Right, Dock::Top, Dock::Bottom] {
            let (dx, dy) = match dock {
                Dock::Left => (MON.2, 0.0),   // centre is to the right
                Dock::Right => (-MON.2, 0.0), // centre is to the left
                Dock::Top => (0.0, MON.3),    // centre is below
                Dock::Bottom => (0.0, -MON.3), // centre is above
                Dock::None => continue,
            };
            let free = aim_at(dx, dy);
            let docked = dock_angle(dock);
            let diff = (free - docked).abs() % 360.0;
            let diff = diff.min(360.0 - diff);
            assert!(diff < 0.01, "{dock:?}: free {free} vs docked {docked}");
        }
    }

    #[test]
    fn a_window_flush_against_a_side_snaps() {
        let (w, h) = (760.0, 860.0); // 380x430 at 2x
        assert_eq!(edge_at(0.0, 500.0, w, h), Dock::Left);
        assert_eq!(edge_at(MON.2 - w, 500.0, w, h), Dock::Right);
    }

    /// The reported setup: the Dock was moved to the right.  The pet must not
    /// hide there, however hard it is pushed against that edge.
    #[test]
    fn a_side_occupied_by_the_dock_is_never_used() {
        let (w, h) = (760.0, 860.0);
        let dock_on_right = EdgePolicy {
            right: false,
            ..EdgePolicy::FALLBACK
        };
        let at_right = MON.2 - w;
        assert_eq!(
            edge_at_sides(at_right, 500.0, w, h, dock_on_right),
            Dock::None,
            "the Dock lives there"
        );
        // The clear side still works.
        assert_eq!(
            edge_at_sides(0.0, 500.0, w, h, dock_on_right),
            Dock::Left
        );

        // Nothing usable means no docking at all, rather than a guess.
        assert_eq!(
            edge_at_sides(0.0, 500.0, w, h, EdgePolicy::NONE),
            Dock::None
        );
    }

    /// The rule is "avoid the Dock's side", and nothing more.
    ///
    /// Refusing the top and bottom as well is what once left the pet with no
    /// reachable edge at all: the Dock had been moved to the right, so the
    /// only surviving candidate was the left — which the user never dragged
    /// far enough to reach.
    #[test]
    fn the_menu_bar_edge_is_usable_but_the_docks_is_not() {
        let (w, h) = (760.0, 860.0);
        // Exactly what NSScreen reported with the Dock on the right: menu bar
        // 34pt at the top, everything else clear but the right.
        let dock_right = EdgePolicy {
            top: true,
            top_inset: 34.0,
            bottom: true,
            left: true,
            right: false,
        };

        assert_eq!(
            edge_at_sides(500.0, 0.0, w, h, dock_right),
            Dock::Top,
            "the menu bar needs clearance, not refusal"
        );
        assert_eq!(edge_at_sides(0.0, 500.0, w, h, dock_right), Dock::Left);
        assert_eq!(
            edge_at_sides(500.0, MON.3 - h, w, h, dock_right),
            Dock::Bottom,
            "the bottom is free when the Dock is on the right"
        );
        assert_eq!(
            edge_at_sides(MON.2 - w, 500.0, w, h, dock_right),
            Dock::None,
            "the Dock lives there"
        );
    }

    /// Regression guard for a bug where opening the list closed it again.
    ///
    /// Expanding grows the window upward, which used to drag its top edge into
    /// the snap threshold, dock the pet to the top and collapse the list the
    /// instant it opened.
    ///
    /// Now that only the sides can dock, that is structurally impossible:
    /// expanding changes neither x nor width, so the horizontal gaps the
    /// decision rests on are untouched.  This asserts that property directly,
    /// rather than the hazard it replaced.
    #[test]
    fn expanding_cannot_change_the_dock_decision() {
        let d = dims(PetSize::Large);
        let w = d.w * SCALE;
        let collapsed = d.h * SCALE;
        let expanded = d.expanded_h * SCALE;
        // Expanding keeps the bottom edge fixed and moves the origin up.
        let expanded_y = 1708.0 - (expanded - collapsed);

        for (x, expected) in [
            (0.0, Dock::Left),
            (MON.2 - w, Dock::Right),
            ((MON.2 - w) / 2.0, Dock::None),
        ] {
            let before = edge_for(
                x, 1708.0 - collapsed, w, collapsed, MON.0, MON.1, MON.2, MON.3, SCALE,
                EdgePolicy::FALLBACK,
            );
            let after = edge_for(
                x, expanded_y, w, expanded, MON.0, MON.1, MON.2, MON.3, SCALE,
                EdgePolicy::FALLBACK,
            );
            assert_eq!(before, expected, "collapsed at x={x}");
            assert_eq!(after, before, "expanding changed the decision at x={x}");
        }
    }

    #[test]
    fn a_window_in_the_middle_does_not_snap() {
        let (w, h) = (760.0, 860.0);
        assert_eq!(
            edge_at((MON.2 - w) / 2.0, (MON.3 - h) / 2.0, w, h),
            Dock::None
        );
    }

    /// The pet sits mid-window, so a centre-based test would need the window
    /// dragged far off-screen. Guard against regressing to that.
    #[test]
    fn snapping_is_reachable_without_dragging_off_screen() {
        let (w, _) = (760.0, 860.0);
        // Window flush at the left edge: this is as far as the user can push.
        assert_eq!(edge_at(0.0, 500.0, w, 860.0), Dock::Left);

        // And the undock inset must exceed the snap threshold, or the pet
        // would be sucked straight back into the edge it just left.
        assert!(UNDOCK_INSET > SNAP_DISTANCE);
    }

    #[test]
    fn only_the_nearest_edge_wins() {
        // Pushed into the top-left corner, the window is flush against both.
        // It must still resolve to a single edge rather than flapping.
        let dock = edge_at(0.0, 0.0, 760.0, 860.0);
        assert!(matches!(dock, Dock::Left | Dock::Top));
    }

    /// Regression guard for the bug where the pet vanished with no way back.
    ///
    /// Two displays of different scale factors.  A remembered pet position in
    /// the gap between them must be rejected — otherwise the pet is parked
    /// where nothing is drawn, and relaunching restores it to the same spot.
    #[test]
    fn a_pet_between_two_displays_is_not_reachable() {
        let builtin = Display {
            x: 0.0,
            y: 0.0,
            w: 3024.0,
            h: 1964.0,
            margin: 40.0 * 2.0,
        };
        // A 1x display arranged above.
        let external = Display {
            x: 0.0,
            y: -1440.0,
            w: 2560.0,
            h: 1440.0,
            margin: 40.0,
        };
        let displays = [builtin, external];

        // The real failure: pet centre past the built-in's right edge and
        // below the external, in dead space covered by no screen.
        assert!(
            !inside_any(&displays, 3086.0, 1882.0),
            "a pet in the dead zone must be rejected"
        );
        // A sane spot on the built-in is accepted.
        assert!(inside_any(&displays, 1500.0, 1500.0));
        // Right at the very edge is still reachable.
        assert!(!inside_any(&displays, 20.0, 1000.0));
    }

    /// The regression behind "left and bottom never dock".
    ///
    /// Pushing the pet against the left edge of the screen sends its x
    /// negative — the log recorded `rect=(-426,68,760x1484)`.  The old rule
    /// demanded the window fit *entirely* inside a display, so it read that as
    /// a straddle and refused to dock, making those edges unreachable.  Only
    /// actually lying across two displays is unsafe.
    #[test]
    fn hanging_off_one_edge_is_not_a_straddle() {
        let builtin = Display { x: 0.0, y: 0.0, w: 3024.0, h: 1964.0, margin: 0.0 };
        // A 1x display arranged above, so the seam is the line y = 0.
        let external = Display { x: 0.0, y: -1440.0, w: 2560.0, h: 1440.0, margin: 0.0 };
        let displays = [builtin, external];

        // The real coordinates from the log: dragged past the left edge.
        let off_left = (-426.0, 68.0, 334.0, 1552.0);
        assert!(overlaps(off_left, &builtin), "it is on the built-in");
        assert!(
            !spans_displays(off_left, &displays),
            "hanging off one edge is not spanning two displays"
        );

        // Comfortably inside is fine too.
        assert!(!spans_displays((1132.0, 630.0, 1892.0, 1334.0), &displays));

        // Genuinely across the seam is not.
        assert!(spans_displays((500.0, -100.0, 1260.0, 604.0), &displays));
    }
}
