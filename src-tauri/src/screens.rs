//! Which screen edges are actually free to hide against.
//!
//! Assuming the sides are free is wrong: the Dock can be moved to either side,
//! which is then exactly where the pet would tuck itself — behind the Dock,
//! invisible and unclickable.  macOS answers this properly.  `NSScreen.frame`
//! is the whole display; `visibleFrame` is what is left after the menu bar and
//! the Dock, so the difference measures how much of each edge is taken.
//!
//! Reading NSScreen needs the main thread, so the answer is sampled there
//! (at startup and whenever the pet is focused) and cached for the threads
//! that actually place the window.

#[cfg(target_os = "macos")]
mod platform {
    use objc2_app_kit::NSScreen;
    use objc2_foundation::MainThreadMarker;

    /// How much of each edge is occupied, in points.
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct Insets {
        pub top: f64,
        pub bottom: f64,
        pub left: f64,
        pub right: f64,
    }

    impl Insets {
        /// A few points of slack: window shadows and rounding show up as
        /// sub-point differences that do not mean the edge is taken.
        const SLACK: f64 = 2.0;

        pub fn left_free(&self) -> bool {
            self.left <= Self::SLACK
        }

        pub fn right_free(&self) -> bool {
            self.right <= Self::SLACK
        }

        pub fn bottom_free(&self) -> bool {
            self.bottom <= Self::SLACK
        }

        /// Turn raw measurements into a docking policy.
        ///
        /// The menu bar sits along the top and *always* will, so refusing the
        /// top edge would be over-strict — the pet only has to keep clear of
        /// it, which an offset achieves.  Anything occupying any other edge is
        /// the Dock, and no offset saves you from the Dock: the pet would sit
        /// behind it, invisible and unclickable.  So those edges are refused.
        pub fn policy(&self) -> crate::screens::EdgePolicy {
            crate::screens::EdgePolicy {
                top: true,
                top_inset: self.top,
                bottom: self.bottom_free(),
                left: self.left_free(),
                right: self.right_free(),
            }
        }
    }

    /// Edge occupation across every attached display.
    ///
    /// An edge counts as taken if *any* display has something along it.  That
    /// is the conservative reading: docking there could hide the pet on the
    /// display that has the bar, even if another one is clear.
    ///
    /// Returns `None` off the main thread, where AppKit must not be touched.
    pub fn insets() -> Option<Insets> {
        let mtm = MainThreadMarker::new()?;
        let screens = NSScreen::screens(mtm);

        let mut worst = Insets::default();
        let mut saw_any = false;

        for screen in screens.iter() {
            let frame = screen.frame();
            let visible = screen.visibleFrame();

            let frame_top = frame.origin.y + frame.size.height;
            let frame_right = frame.origin.x + frame.size.width;
            let visible_top = visible.origin.y + visible.size.height;
            let visible_right = visible.origin.x + visible.size.width;

            worst.top = worst.top.max(frame_top - visible_top);
            worst.bottom = worst.bottom.max(visible.origin.y - frame.origin.y);
            worst.left = worst.left.max(visible.origin.x - frame.origin.x);
            worst.right = worst.right.max(frame_right - visible_right);
            saw_any = true;
        }

        saw_any.then_some(worst)
    }
}

#[cfg(not(target_os = "macos"))]
mod platform {
    #[derive(Debug, Clone, Copy, PartialEq, Default)]
    pub struct Insets {
        pub top: f64,
        pub bottom: f64,
        pub left: f64,
        pub right: f64,
    }

    impl Insets {
        pub fn left_free(&self) -> bool {
            true
        }
        pub fn right_free(&self) -> bool {
            true
        }
        pub fn bottom_free(&self) -> bool {
            true
        }
        pub fn policy(&self) -> crate::screens::EdgePolicy {
            crate::screens::EdgePolicy {
                top: true,
                top_inset: self.top,
                bottom: true,
                left: true,
                right: true,
            }
        }
    }

    pub fn insets() -> Option<Insets> {
        Some(Insets::default())
    }
}

pub use platform::{insets, Insets};

/// Which edges the pet may hide against, and how far to stay off the top.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct EdgePolicy {
    pub top: bool,
    pub bottom: bool,
    pub left: bool,
    pub right: bool,
    /// Clearance below the top edge, in points — the menu bar's height.
    pub top_inset: f64,
}

impl EdgePolicy {
    /// What to assume when the system cannot be asked.
    ///
    /// The stock macOS arrangement: menu bar on top, Dock at the bottom.  That
    /// leaves the two sides, which is strictly better than refusing to dock.
    pub const FALLBACK: Self = Self {
        top: true,
        top_inset: 24.0,
        bottom: false,
        left: true,
        right: true,
    };

    /// Only used to express "nothing may dock", mostly for tests.
    pub const NONE: Self = Self {
        top: false,
        bottom: false,
        left: false,
        right: false,
        top_inset: 0.0,
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reported setup: the Dock was moved to the right.  Everything except
    /// that side must stay available — refusing the others is what left the
    /// pet with nowhere to dock at all.
    #[test]
    fn only_the_dock_side_is_refused() {
        let dock_right = Insets {
            top: 34.0,
            bottom: 0.0,
            left: 0.0,
            right: 47.0,
        };
        let policy = dock_right.policy();
        assert!(policy.left, "left is clear");
        assert!(policy.bottom, "the Dock is not there");
        assert!(policy.top, "the menu bar is avoided with an offset, not refused");
        assert_eq!(policy.top_inset, 34.0, "and it uses the measured height");
        assert!(!policy.right, "the Dock lives there");
    }

    #[test]
    fn a_bottom_dock_only_refuses_the_bottom() {
        let dock_bottom = Insets {
            top: 34.0,
            bottom: 82.0,
            left: 0.0,
            right: 0.0,
        };
        let policy = dock_bottom.policy();
        assert!(!policy.bottom);
        assert!(policy.left && policy.right && policy.top);
    }

    #[test]
    fn a_left_dock_only_refuses_the_left() {
        let dock_left = Insets {
            top: 34.0,
            bottom: 0.0,
            left: 78.0,
            right: 0.0,
        };
        let policy = dock_left.policy();
        assert!(!policy.left);
        assert!(policy.right && policy.bottom && policy.top);
    }

    #[test]
    fn sub_point_noise_is_not_an_occupied_edge() {
        let noise = Insets {
            top: 34.0,
            bottom: 0.5,
            left: 0.0,
            right: 1.0,
        };
        let policy = noise.policy();
        assert!(policy.left && policy.right && policy.bottom);
    }

    #[test]
    fn a_hidden_menu_bar_needs_no_clearance() {
        let clean = Insets::default();
        assert_eq!(clean.policy().top_inset, 0.0);
    }
}
