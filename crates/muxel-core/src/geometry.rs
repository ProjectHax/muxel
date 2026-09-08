//! Layout preferences are inputs, never overwritten by a temporary size clamp.

/// Allocate a central split proportionally, freezing children at their minimum
/// as needed. If the container is too small, its children overflow at minimum.
/// Call recursively with each child's allocated extent for nested splits.
pub fn proportional_sizes(extent: f32, preferred: &[f32], minimums: &[f32]) -> Vec<f32> {
    assert_eq!(preferred.len(), minimums.len());
    let weights: Vec<f64> = preferred
        .iter()
        .map(|&v| f64::from(if v.is_finite() && v > 0.0 { v } else { 1.0 }))
        .collect();
    let mut sizes: Vec<f32> = minimums.iter().map(|&v| v.max(0.0)).collect();
    let mut remaining = f64::from(extent.max(sizes.iter().sum()));
    let mut weight: f64 = weights.iter().sum();
    // Highest minimum/weight freezes first. One sorted pass avoids repeated
    // redistribution over every sibling when many small panes hit their limit.
    let mut order: Vec<usize> = (0..weights.len()).collect();
    order.sort_by(|&a, &b| {
        (f64::from(sizes[b]) / weights[b]).total_cmp(&(f64::from(sizes[a]) / weights[a]))
    });
    for ix in order {
        let proposed = remaining * weights[ix] / weight;
        sizes[ix] = sizes[ix].max(proposed as f32);
        remaining = (remaining - f64::from(sizes[ix])).max(0.0);
        weight -= weights[ix];
    }
    sizes
}

/// A sidebar holds logical pixels until its container cannot accommodate both
/// its preference and the other panel's minimum. This does not save the clamp.
pub fn fixed_panel_size(extent: f32, preferred: f32, min: f32, max: f32, other_min: f32) -> f32 {
    preferred
        .max(min)
        .min(max.max(min))
        .min((extent - other_min).max(min))
}

#[cfg(test)]
mod tests {
    use super::{fixed_panel_size, proportional_sizes};

    #[test]
    fn window_resize_preserves_sidebar_pixels_and_center_ratios() {
        for width in [1600.0, 2000.0, 1200.0, 1600.0] {
            let sidebar = fixed_panel_size(width, 232.0, 160.0, 900.0, 100.0);
            assert_eq!(sidebar, 232.0);
            let center = proportional_sizes(width - sidebar, &[3.0, 1.0], &[0.0, 0.0]);
            assert_eq!(center, vec![(width - 232.0) * 0.75, (width - 232.0) * 0.25]);
        }
    }

    #[test]
    fn minimum_round_trip_restores_preferences_recursively() {
        let preferred = [900.0, 300.0];
        assert_eq!(
            proportional_sizes(1200.0, &preferred, &[340.0, 340.0]),
            vec![860.0, 340.0]
        );
        assert_eq!(
            proportional_sizes(680.0, &preferred, &[340.0, 340.0]),
            vec![340.0, 340.0]
        );
        assert_eq!(
            proportional_sizes(1600.0, &preferred, &[340.0, 340.0]),
            vec![1200.0, 400.0]
        );
        let nested = proportional_sizes(900.0, &[1.0, 2.0], &[120.0, 120.0]);
        assert_eq!(nested, vec![300.0, 600.0]);
        assert_eq!(
            proportional_sizes(180.0, &[1.0, 2.0], &[120.0, 120.0]),
            vec![120.0, 120.0]
        );
        assert_eq!(
            proportional_sizes(900.0, &[1.0, 2.0], &[120.0, 120.0]),
            nested
        );
    }

    #[test]
    fn hidden_or_clamped_sidebars_restore_the_saved_width() {
        let preferred = 420.0;
        assert_eq!(
            fixed_panel_size(350.0, preferred, 160.0, 900.0, 100.0),
            250.0
        );
        assert_eq!(
            fixed_panel_size(1200.0, preferred, 160.0, 900.0, 100.0),
            preferred
        );
        // Hiding simply stops allocating that fixed panel; reopening uses the
        // same preference, including beside another independently sized panel.
        let project = fixed_panel_size(1600.0, 232.0, 160.0, 900.0, 100.0);
        let files = fixed_panel_size(1600.0 - project, preferred, 180.0, 800.0, 100.0);
        assert_eq!(files, preferred);
    }

    #[test]
    fn allocation_is_finite_for_legacy_or_invalid_weights() {
        assert_eq!(
            proportional_sizes(900.0, &[0.0, f32::NAN, -1.0], &[100.0; 3]),
            vec![300.0; 3]
        );
        assert_eq!(proportional_sizes(0.0, &[], &[]), Vec::<f32>::new());
        assert_eq!(proportional_sizes(10.0, &[1.0], &[100.0]), vec![100.0]);
    }
}
