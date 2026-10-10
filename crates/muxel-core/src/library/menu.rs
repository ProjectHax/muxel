//! Height of the toolbar drop-down lists.

use super::{MENU_LIST_MAX_FRACTION, MENU_WINDOW_MARGIN};

/// Largest height, in logical px, of a toolbar drop-down's scrollable list,
/// for window content height `h` and the drop-down's fixed part `f` (title,
/// footer, padding, border): at most a fraction of `h`, and always leaving
/// `MENU_WINDOW_MARGIN` to the window edges.
pub fn menu_list_max_height(h: f32, f: f32) -> f32 {
    let by_fraction = (MENU_LIST_MAX_FRACTION * h).floor();
    let by_window = h - 2.0 * MENU_WINDOW_MARGIN - f;
    by_fraction.min(by_window).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::menu_list_max_height;

    const F: f32 = 60.0;

    #[test]
    fn list_height_for_reference_heights() {
        // Expected values are worked out by hand, not from the formula.
        let cases: [(f32, f32); 6] = [
            (1000.0, 600.0),
            (1001.0, 600.0),
            (300.0, 180.0),
            (150.0, 74.0),
            (76.0, 0.0),
            (50.0, 0.0),
        ];
        for (h, expected) in cases {
            assert_eq!(
                menu_list_max_height(h, F),
                expected,
                "menu_list_max_height({h}, {F})"
            );
        }
    }

    #[test]
    fn list_and_fixed_part_fit_with_margins() {
        // For every H >= F + 16 the drop-down keeps 8 px from both edges.
        for h in [1000.0_f32, 1001.0, 300.0, 150.0, 76.0] {
            let list = menu_list_max_height(h, F);
            assert!(list >= 0.0, "non-negative for H = {h}");
            assert!(list + F + 16.0 <= h, "fits for H = {h}: {list}");
        }
        for f in [0.0_f32, 40.0, 60.0, 123.0] {
            for h in (f as u32 + 16)..=2000 {
                let h = h as f32;
                let list = menu_list_max_height(h, f);
                assert!(list >= 0.0, "non-negative for H = {h}, F = {f}");
                assert!(list + f + 16.0 <= h, "fits for H = {h}, F = {f}: {list}");
            }
        }
    }

    #[test]
    fn fraction_cap_applies_when_room_is_plenty() {
        // The 60 % cap binds (the margin alone would allow 984).
        assert_eq!(menu_list_max_height(1000.0, 0.0), 600.0);
        // The margin binds (60 % would allow 60).
        assert_eq!(menu_list_max_height(100.0, 30.0), 54.0);
    }
}
