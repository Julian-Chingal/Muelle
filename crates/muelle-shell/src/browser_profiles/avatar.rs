//! Separate a Chromium-style avatar only when the published bitmap agrees
//! with the browser resource everywhere outside its circular badge. Unknown
//! icon layouts keep their original bitmap rather than cropping another logo.

use crate::icons::IconImage;

pub(crate) fn extract(published: &IconImage, browser: &IconImage) -> Option<IconImage> {
    let side = published.width;
    if !(32..=512).contains(&side)
        || published.height != side
        || browser.width != side
        || browser.height != side
    {
        return None;
    }
    let pixels = (side as usize).checked_mul(side as usize)?.checked_mul(4)?;
    if published.bgra.len() != pixels || browser.bgra.len() != pixels {
        return None;
    }
    if published.bgra == browser.bgra {
        return None;
    }
    // Chromium's documented icon composition uses a badge half the icon's
    // width, at top-right on Windows 11 and bottom-right on older Windows.
    let diameter = side.div_ceil(2);
    let left = side - diameter;
    for top in [0, side - diameter] {
        let radius = diameter as f32 / 2.0;
        let center = (left as f32 + radius, top as f32 + radius);
        let mut outside = 0;
        let mut different_outside = 0;
        let mut different_inside = 0;
        for y in 0..side {
            for x in 0..side {
                let dx = x as f32 + 0.5 - center.0;
                let dy = y as f32 + 0.5 - center.1;
                let distance_squared = dx * dx + dy * dy;
                let at = ((y * side + x) * 4) as usize;
                let different = published.bgra[at..at + 4]
                    .iter()
                    .zip(&browser.bgra[at..at + 4])
                    .any(|(a, b)| a.abs_diff(*b) > 12);
                if distance_squared > (radius + 1.0).powi(2) {
                    outside += 1;
                    different_outside += u32::from(different);
                } else if distance_squared < (radius - 1.0).powi(2) {
                    different_inside += u32::from(different);
                }
            }
        }
        // Permit small resource-resampling differences, not a different app,
        // tint, unknown badge size, or an unbadged browser icon.
        if different_outside * 1000 > outside * 5 || different_inside < diameter * diameter / 12 {
            continue;
        }
        let mut bgra = vec![0; (diameter * diameter * 4) as usize];
        for y in 0..diameter {
            for x in 0..diameter {
                let dx = x as f32 + 0.5 - radius;
                let dy = y as f32 + 0.5 - radius;
                let distance_squared = dx * dx + dy * dy;
                let coverage = if distance_squared <= (radius - 1.0).powi(2) {
                    1.0
                } else if distance_squared >= radius * radius {
                    0.0
                } else {
                    radius - distance_squared.sqrt()
                };
                let source = (((top + y) * side + left + x) * 4) as usize;
                let target = ((y * diameter + x) * 4) as usize;
                for channel in 0..4 {
                    // Both input and output are premultiplied BGRA.
                    bgra[target + channel] =
                        (f32::from(published.bgra[source + channel]) * coverage).round() as u8;
                }
            }
        }
        return Some(IconImage {
            width: diameter,
            height: diameter,
            bgra,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(bottom: bool) -> (IconImage, IconImage) {
        let base = IconImage {
            width: 64,
            height: 64,
            bgra: [30, 60, 90, 255].repeat(64 * 64),
        };
        let mut badged = base.clone();
        let center = (48.0f32, if bottom { 48.0 } else { 16.0 });
        for y in 0..64 {
            for x in 0..64 {
                if (x as f32 + 0.5 - center.0).hypot(y as f32 + 0.5 - center.1) < 16.0 {
                    let offset = (y * 64 + x) * 4;
                    badged.bgra[offset..offset + 4].copy_from_slice(&[160, 120, 80, 255]);
                }
            }
        }
        (base, badged)
    }

    #[test]
    fn extracts_either_windows_corner_with_transparent_antialiased_edges() {
        for bottom in [false, true] {
            let (base, badged) = fixture(bottom);
            let Some(avatar) = extract(&badged, &base) else {
                panic!("expected circular badge")
            };
            assert_eq!((avatar.width, avatar.height), (32, 32));
            assert_eq!(&avatar.bgra[..4], &[0, 0, 0, 0]);
            assert_eq!(
                &avatar.bgra[(16 * 32 + 16) * 4..(16 * 32 + 16) * 4 + 4],
                &[160, 120, 80, 255]
            );
            assert!(
                avatar
                    .bgra
                    .as_chunks::<4>()
                    .0
                    .iter()
                    .any(|pixel| pixel[3] > 0 && pixel[3] < 255)
            );
        }
    }

    #[test]
    fn unknown_tinted_unbadged_or_malformed_icons_are_left_untouched() {
        let (base, mut badged) = fixture(false);
        assert!(extract(&base, &base).is_none());
        badged.bgra[..64 * 4].fill(255);
        assert!(extract(&badged, &base).is_none());
        badged.bgra.truncate(10);
        assert!(extract(&badged, &base).is_none());
    }
}
