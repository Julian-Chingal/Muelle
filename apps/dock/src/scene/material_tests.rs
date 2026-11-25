use super::*;

#[test]
fn frosted_keeps_the_backdrop_visible_and_its_text_panels_denser() {
    for light in [false, true] {
        let look = Look {
            material: Material::Frosted,
            light,
            glass: true,
        };
        let dock = Theme::new(look);
        let bar = Theme::for_bar(look);
        // The icon surface retains at least 40% of its backdrop. The text
        // strip can be denser, but must still retain at least a quarter.
        assert!(255 - u16::from(dock.tint.A) >= 102);
        assert!(255 - u16::from(bar.tint.A) >= 64);
        assert!(dock.panel_tint.A > dock.tint.A);
        assert_eq!(dock.light, light);
    }
    let light = Theme::new(Look {
        material: Material::Frosted,
        light: true,
        glass: true,
    });
    let dark = Theme::new(Look {
        material: Material::Frosted,
        light: false,
        glass: true,
    });
    assert!(light.tint.R > dark.tint.R && light.tint.G > dark.tint.G && light.tint.B > dark.tint.B);
}

#[test]
fn disabling_transparency_keeps_every_palette_opaque_including_clear() {
    for material in Material::ALL {
        for light in [false, true] {
            let look = Look {
                material,
                light,
                glass: false,
            };
            let dock = Theme::new(look);
            let bar = Theme::for_bar(look);
            assert!(!dock.glass && !bar.glass);
            assert_eq!(dock.tint.A, 255);
            assert_eq!(dock.panel_tint.A, 255);
            assert_eq!(bar.tint.A, 255);
        }
    }
}

#[test]
#[ignore = "Native composition regression; no windows, wallpaper or system preference changes"]
fn frosted_and_clear_build_real_backdrop_layers_only_when_windows_allows_them() -> Result<()> {
    use windows::{
        UI::Composition::{CompositionBackdropBrush, CompositionColorBrush},
        Win32::System::WinRT::{
            CreateDispatcherQueueController, DQTAT_COM_STA, DQTYPE_THREAD_CURRENT,
            DispatcherQueueOptions,
        },
    };
    // SAFETY: Only this thread's private dispatcher/compositor is created.
    let _queue = unsafe {
        CreateDispatcherQueueController(DispatcherQueueOptions {
            dwSize: size_of::<DispatcherQueueOptions>() as u32,
            threadType: DQTYPE_THREAD_CURRENT,
            apartmentType: DQTAT_COM_STA,
        })?
    };
    let gfx = Gfx::new()?;
    for material in [Material::Frosted, Material::Clear] {
        for light in [false, true] {
            for glass in [false, true] {
                let look = Look {
                    material,
                    light,
                    glass,
                };
                for theme in [Theme::new(look), Theme::for_bar(look)] {
                    let fills = theme.fills(&gfx)?;
                    let backdrops = fills
                        .iter()
                        .filter(|brush| brush.cast::<CompositionBackdropBrush>().is_ok())
                        .count();
                    assert_eq!(backdrops, usize::from(glass));
                    let tint = fills[usize::from(glass)]
                        .cast::<CompositionColorBrush>()?
                        .Color()?;
                    assert_eq!(tint, theme.tint);
                    assert_eq!(tint.A == 255, !glass);
                }
            }
        }
    }
    Ok(())
}
