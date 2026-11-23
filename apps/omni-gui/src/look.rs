//! Colores, transparencias, formas e icono que el usuario elige encima del
//! tema (Ajustes › Apariencia). Aquí se pasan entre [`Look`] (lo que se
//! guarda) y el `Theme` de Slint, y se preparan las miniaturas del icono.

use slint::{Color, ComponentHandle, Image, Rgba8Pixel, SharedPixelBuffer, SharedString};

use crate::icon;
use crate::settings::{Look, parse_hex, to_hex};
use crate::{AppWindow, IconPal, IconStyle, Theme};

/// Transparencia con la que se aplican «Filas alternas» y «Fila seleccionada»
/// (el usuario elige solo el color).
const ALT_ALPHA: f32 = 0.18;
const SEL_ALPHA: f32 = 0.25;

pub fn color(rgb: u32) -> Color {
    Color::from_rgb_u8((rgb >> 16) as u8, (rgb >> 8) as u8, rgb as u8)
}

pub fn rgb(c: Color) -> u32 {
    (u32::from(c.red()) << 16) | (u32::from(c.green()) << 8) | u32::from(c.blue())
}

/// «#rrggbb» de un color de Slint.
pub fn hex_of(c: Color) -> String {
    to_hex(rgb(c))
}

/// Lee del `Theme` los colores, transparencias y formas personalizados.
pub fn read_theme(theme: &Theme) -> Look {
    let mut look = Look::default();
    macro_rules! slot {
        ($i:expr, $on:ident, $c:ident) => {
            if theme.$on() {
                look.colors[$i] = hex_of(theme.$c());
            }
        };
    }
    slot!(0, get_c_accent_on, get_c_accent);
    slot!(1, get_c_bg_on, get_c_bg);
    slot!(2, get_c_panel_on, get_c_panel);
    slot!(3, get_c_text_on, get_c_text);
    slot!(4, get_c_bar_on, get_c_bar);
    slot!(5, get_c_ok_on, get_c_ok);
    slot!(6, get_c_warn_on, get_c_warn);
    slot!(7, get_c_err_on, get_c_err);
    slot!(8, get_c_alt_on, get_c_alt);
    slot!(9, get_c_sel_on, get_c_sel);
    look.panel_opacity = theme.get_panel_opacity();
    look.glow = theme.get_glow();
    look.radius = theme.get_radius_level();
    look.border = theme.get_border_level();
    look
}

/// Pone en el `Theme` lo personalizado (los colores vacíos vuelven al tema).
pub fn apply_theme(theme: &Theme, look: &Look) {
    macro_rules! slot {
        ($i:expr, $on:ident, $c:ident, $alpha:expr) => {
            match parse_hex(&look.colors[$i]) {
                Some(v) => {
                    theme.$c(color(v).with_alpha($alpha));
                    theme.$on(true);
                }
                None => theme.$on(false),
            }
        };
    }
    slot!(0, set_c_accent_on, set_c_accent, 1.0);
    slot!(1, set_c_bg_on, set_c_bg, 1.0);
    slot!(2, set_c_panel_on, set_c_panel, 1.0);
    slot!(3, set_c_text_on, set_c_text, 1.0);
    slot!(4, set_c_bar_on, set_c_bar, 1.0);
    slot!(5, set_c_ok_on, set_c_ok, 1.0);
    slot!(6, set_c_warn_on, set_c_warn, 1.0);
    slot!(7, set_c_err_on, set_c_err, 1.0);
    slot!(8, set_c_alt_on, set_c_alt, ALT_ALPHA);
    slot!(9, set_c_sel_on, set_c_sel, SEL_ALPHA);
    theme.set_panel_opacity(look.panel_opacity);
    theme.set_glow(look.glow);
    theme.set_radius_level(look.radius);
    theme.set_border_level(look.border);
}

/// Todo lo personalizado, con el icono elegido en la ventana.
pub fn read(ui: &AppWindow) -> Look {
    let mut look = read_theme(&ui.global::<Theme>());
    look.icon_style = ui.get_icon_style();
    look.icon_palette = ui.get_icon_palette();
    look.fixed()
}

/// Lo pone todo en la ventana (el icono se dibuja aparte, ver [`icon_for`]).
pub fn apply(ui: &AppWindow, look: &Look) {
    let look = look.clone().fixed();
    apply_theme(&ui.global::<Theme>(), &look);
    ui.set_icon_style(look.icon_style);
    ui.set_icon_palette(look.icon_palette);
}

fn image(rgba: &[u8], size: u32) -> Image {
    Image::from_rgba8(SharedPixelBuffer::<Rgba8Pixel>::clone_from_slice(
        rgba, size, size,
    ))
}

/// El acento que se ve ahora en la ventana (0xRRGGBB).
pub fn current_accent(ui: &AppWindow) -> u32 {
    rgb(ui.global::<Theme>().get_accent())
}

/// El icono elegido, dibujado a 32 y 256 píxeles, y su imagen para Slint.
pub struct DrawnIcon {
    pub set: Vec<(u32, u32, Vec<u8>)>,
    pub image: Image,
}

pub fn icon_for(look: &Look, accent: u32) -> Option<DrawnIcon> {
    let pal = icon::palette(look.icon_palette, accent);
    let set = icon::rgba_set(look.icon_style, &pal)?;
    let image = image(&set[1].2, 256);
    Some(DrawnIcon { set, image })
}

fn style_name(i: usize) -> SharedString {
    icon::tr_name(icon::STYLES[i]).into()
}

fn palette_name(i: usize) -> String {
    if i == 0 {
        icon::tr_name("Según el acento")
    } else {
        icon::tr_name(icon::NAMED[i - 1].0)
    }
}

/// Miniaturas de los estilos con los colores elegidos.
pub fn style_models(look: &Look, accent: u32) -> Vec<IconStyle> {
    let pal = icon::palette(look.icon_palette, accent);
    (0..icon::STYLES.len())
        .map(|i| IconStyle {
            name: style_name(i),
            image: icon::render(i as i32, &pal, 120)
                .map(|px| image(&px, 120))
                .unwrap_or_default(),
        })
        .collect()
}

/// Los colores del icono para elegir, en dos filas.
pub fn palette_models(accent: u32) -> (Vec<IconPal>, Vec<IconPal>) {
    let from = icon::from_accent(accent);
    let mut all = vec![IconPal {
        idx: 0,
        name: palette_name(0).into(),
        c1: color(from.n1),
        c2: color(from.n2),
    }];
    all.extend(icon::NAMED.iter().enumerate().map(|(k, (_, p))| IconPal {
        idx: k as i32 + 1,
        name: palette_name(k + 1).into(),
        c1: color(p.n1),
        c2: color(p.n2),
    }));
    let second = all.split_off(10);
    (all, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn color_roundtrip() {
        assert_eq!(rgb(color(0x3b82f6)), 0x3b82f6);
        assert_eq!(hex_of(color(0x0a5fd0)), "#0a5fd0");
    }

    #[test]
    fn palettes_split_in_two_rows() {
        let (a, b) = palette_models(0x3b82f6);
        assert_eq!(a.len(), 10);
        assert_eq!(a.len() + b.len(), icon::NAMED.len() + 1);
        assert_eq!(b[0].idx, 10);
    }

    #[test]
    fn default_icon_draws() {
        let d = icon_for(&Look::default(), 0x3b82f6).unwrap();
        assert_eq!(d.set.len(), 2);
    }
}
