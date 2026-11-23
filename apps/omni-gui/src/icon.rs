//! Icono de la aplicación: estilos y colores que elige el usuario en
//! Ajustes › Apariencia.
//!
//! Cada estilo es una plantilla SVG con una paleta (fondo, tres manchas de
//! color y dos tonos de neón). Se dibuja con `resvg` a cualquier tamaño, así
//! que la ventana, la bandeja y las miniaturas de Ajustes salen del mismo
//! dibujo y no hace falta guardar una imagen por cada combinación.
//!
//! El icono del `.exe`, el del instalador y el del acceso directo los fija el
//! sistema al compilar (`ui/icon.png` y `ui/icon.ico`): ahí va el estilo y el
//! color por defecto.

use resvg::{tiny_skia, usvg};

/// Colores de un icono.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Palette {
    pub base: u32,
    pub blobs: [u32; 3],
    pub n1: u32,
    pub n2: u32,
}

/// Nombre de cada estilo (el orden es el de `Settings::icon_style`).
pub const STYLES: &[&str] = &[
    "Hexágono con rayo",
    "Cristal con rayo",
    "Hexágono de cristal",
    "Rayo con ondas",
    "Doble hexágono",
    "Cristal facetado",
    "Clásico",
    "Emblema",
    "Escudo",
];

const fn p(base: u32, b: [u32; 3], n1: u32, n2: u32) -> Palette {
    Palette {
        base,
        blobs: b,
        n1,
        n2,
    }
}

/// Paletas con nombre; la 0 no está aquí: es «Según el acento».
pub const NAMED: &[(&str, Palette)] = &[
    (
        "Rojo y blanco",
        p(0x1a0509, [0xef2b3d, 0x7a0b1c, 0xef2b3d], 0xffffff, 0xff3b4a),
    ),
    (
        "Violeta y rosa",
        p(0x1b0f3d, [0xf472b6, 0x38bdf8, 0xfb923c], 0x22d3ee, 0xe879f9),
    ),
    (
        "Azul hielo",
        p(0x06183a, [0x38bdf8, 0x6366f1, 0x22d3ee], 0xa5f3fc, 0x3b82f6),
    ),
    (
        "Rojo fuego",
        p(0x2a0710, [0xef4444, 0xf59e0b, 0xec4899], 0xfde047, 0xf43f5e),
    ),
    (
        "Verde tóxico",
        p(0x04180f, [0x22c55e, 0x14b8a6, 0xa3e635], 0xd9f99d, 0x10b981),
    ),
    (
        "Oro",
        p(0x14110a, [0xf59e0b, 0xb45309, 0xfbbf24], 0xfde68a, 0xf59e0b),
    ),
    (
        "Magenta",
        p(0x1f0524, [0xd946ef, 0x7e22ce, 0xec4899], 0xf0abfc, 0xd946ef),
    ),
    (
        "Naranja",
        p(0x1c0d03, [0xf97316, 0xb45309, 0xef4444], 0xfdba74, 0xf97316),
    ),
    (
        "Turquesa",
        p(0x021a1a, [0x06b6d4, 0x14b8a6, 0x0e7490], 0x5eead4, 0x06b6d4),
    ),
    (
        "Lima",
        p(0x0f1a02, [0x65a30d, 0xa3e635, 0x16a34a], 0xbef264, 0x65a30d),
    ),
    (
        "Rosa chicle",
        p(0x230818, [0xec4899, 0xf472b6, 0xa21caf], 0xfbcfe8, 0xec4899),
    ),
    (
        "Marino y plata",
        p(0x050b1c, [0x1e40af, 0x334155, 0x0ea5e9], 0xe2e8f0, 0x94a3b8),
    ),
    (
        "Violeta",
        p(0x140a30, [0x7c3aed, 0x4f46e5, 0xa855f7], 0xc4b5fd, 0x7c3aed),
    ),
    (
        "Rojo sangre",
        p(0x1c0306, [0xdc2626, 0x7f1d1d, 0xef4444], 0xfca5a5, 0xdc2626),
    ),
    (
        "Esmeralda",
        p(0x021a10, [0x059669, 0x10b981, 0x047857], 0x6ee7b7, 0x059669),
    ),
    (
        "Azul eléctrico",
        p(0x02081f, [0x1d4ed8, 0x2563eb, 0x0ea5e9], 0x38bdf8, 0x1d4ed8),
    ),
    (
        "Amarillo y cian",
        p(0x071524, [0x22d3ee, 0x0ea5e9, 0xfacc15], 0xfde047, 0x22d3ee),
    ),
    (
        "Atardecer",
        p(0x1a0a24, [0xf43f5e, 0xfb923c, 0x7c3aed], 0xfde047, 0xf43f5e),
    ),
    (
        "Blanco",
        p(0xf1f3f8, [0xfda4af, 0x93c5fd, 0xc4b5fd], 0xef2b3d, 0x7c3aed),
    ),
    (
        "Grafito",
        p(0x0b0b0d, [0x374151, 0x1f2937, 0x4b5563], 0xffffff, 0x9ca3af),
    ),
];

/// Nombre en el idioma de la interfaz (español u inglés).
pub fn tr_name(name: &str) -> String {
    const EN: &[(&str, &str)] = &[
        ("Hexágono con rayo", "Lightning hexagon"),
        ("Cristal con rayo", "Glass lightning"),
        ("Hexágono de cristal", "Glass hexagon"),
        ("Rayo con ondas", "Lightning waves"),
        ("Doble hexágono", "Double hexagon"),
        ("Cristal facetado", "Faceted glass"),
        ("Clásico", "Classic"),
        ("Emblema", "Emblem"),
        ("Escudo", "Shield"),
        ("Según el acento", "Follow accent"),
        ("Rojo y blanco", "Red and white"),
        ("Violeta y rosa", "Violet and pink"),
        ("Azul hielo", "Ice blue"),
        ("Rojo fuego", "Fire red"),
        ("Verde tóxico", "Toxic green"),
        ("Oro", "Gold"),
        ("Magenta", "Magenta"),
        ("Naranja", "Orange"),
        ("Turquesa", "Turquoise"),
        ("Lima", "Lime"),
        ("Rosa chicle", "Bubblegum"),
        ("Marino y plata", "Navy and silver"),
        ("Violeta", "Violet"),
        ("Rojo sangre", "Blood red"),
        ("Esmeralda", "Emerald"),
        ("Azul eléctrico", "Electric blue"),
        ("Amarillo y cian", "Yellow and cyan"),
        ("Atardecer", "Sunset"),
        ("Blanco", "White"),
        ("Grafito", "Graphite"),
    ];
    if crate::i18n::is_english()
        && let Some((_, en)) = EN.iter().find(|(es, _)| *es == name)
    {
        return (*en).to_owned();
    }
    name.to_owned()
}

fn mix(a: u32, b: u32, t: f32) -> u32 {
    let ch = |s: u32| {
        let (x, y) = (((a >> s) & 255) as f32, ((b >> s) & 255) as f32);
        (x + (y - x) * t).round().clamp(0.0, 255.0) as u32
    };
    (ch(16) << 16) | (ch(8) << 8) | ch(0)
}

/// Paleta a partir de un color de acento (0xRRGGBB): fondo muy oscuro del
/// mismo tono, manchas del acento y neón del acento aclarado.
pub fn from_accent(accent: u32) -> Palette {
    let base = mix(accent, 0x000000, 0.9);
    Palette {
        base,
        blobs: [
            accent,
            mix(accent, 0x000000, 0.45),
            mix(accent, 0xffffff, 0.2),
        ],
        n1: mix(accent, 0xffffff, 0.55),
        n2: accent,
    }
}

/// Acento del tema por defecto: de él sale el icono fijo del programa.
#[cfg(test)]
pub const DEFAULT_ACCENT: u32 = 0x0a5fd0;

/// Paleta del número `i` de la lista de Ajustes: 0 «Según el acento», el
/// resto las de [`NAMED`].
pub fn palette(i: i32, accent: u32) -> Palette {
    match usize::try_from(i - 1).ok().and_then(|k| NAMED.get(k)) {
        Some((_, pal)) => pal.clone(),
        None => from_accent(accent),
    }
}

fn hex(c: u32) -> String {
    format!("#{c:06x}")
}

const BOLT: &str = "M152 26L100 84H128L112 112H74L128 180 182 112H146L162 68H134z";
const HEX: &str = "M128 26L216 77V179L128 230 40 179V77z";
const AR: &str = "M112 40h32v72h36L128 176 76 112h36z";
const SHIELD: &str = "M128 20L222 56V128C222 184 182 222 128 240C74 222 34 184 34 128V56z";

/// Dibujo SVG (256×256) de un estilo con una paleta.
pub fn svg(style: i32, pal: &Palette) -> String {
    let (base, n1, n2) = (hex(pal.base), hex(pal.n1), hex(pal.n2));
    let bl = pal.blobs.map(hex);
    let defs = format!(
        r##"<filter id="bl" x="-50%" y="-50%" width="200%" height="200%"><feGaussianBlur stdDeviation="28"/></filter>
<filter id="gl" x="-30%" y="-30%" width="160%" height="160%"><feGaussianBlur stdDeviation="6" result="b"/><feMerge><feMergeNode in="b"/><feMergeNode in="SourceGraphic"/></feMerge></filter>
<linearGradient id="n" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="{n1}"/><stop offset="1" stop-color="{n2}"/></linearGradient>
<linearGradient id="v" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#fff"/><stop offset="1" stop-color="{n1}"/></linearGradient>
<linearGradient id="b" x1="0" y1="0" x2="1" y2="1"><stop offset="0" stop-color="{n2}"/><stop offset="1" stop-color="{base}"/></linearGradient>
<linearGradient id="f" x1="0" y1="0" x2="0" y2="1"><stop offset="0" stop-color="#fff"/><stop offset="1" stop-color="{fe}"/></linearGradient>
<clipPath id="cp"><rect width="256" height="256" rx="58"/></clipPath>
<clipPath id="lh"><rect width="128" height="256"/></clipPath>
<clipPath id="rh"><rect x="128" width="128" height="256"/></clipPath>
<filter id="sh" x="-20%" y="-20%" width="140%" height="150%"><feDropShadow dx="0" dy="6" stdDeviation="6" flood-color="#000" flood-opacity=".35"/></filter>"##,
        fe = hex(mix(pal.n1, 0xffffff, 0.6)),
    );
    let sq = format!(r#"<rect width="256" height="256" rx="58" fill="{base}"/>"#);
    let blobs = format!(
        r#"<g clip-path="url(#cp)"><g filter="url(#bl)"><circle cx="60" cy="50" r="80" fill="{}"/><circle cx="212" cy="90" r="72" fill="{}"/><circle cx="120" cy="232" r="92" fill="{}"/></g></g>"#,
        bl[0], bl[1], bl[2]
    );
    let faint = format!(
        r#"<g clip-path="url(#cp)" opacity=".28"><g filter="url(#bl)"><circle cx="200" cy="40" r="70" fill="{}"/><circle cx="50" cy="230" r="80" fill="{}"/></g></g>"#,
        bl[0], bl[1]
    );
    let bar =
        |extra: &str| format!(r#"<rect x="70" y="198" width="116" height="18" rx="9" {extra}/>"#);
    let gs = r#"transform="translate(128 126) scale(.6) translate(-128 -126)""#;
    let glass = r##"fill="#fff" fill-opacity=".26" stroke="#fff" stroke-opacity=".9" stroke-width="4" stroke-linejoin="round""##;
    let dark = hex(pal.base);
    let emb = |cx: f32, cy: f32, s: f32| {
        // Flecha con relieve: sombra clara debajo y flecha del color del fondo.
        let g =
            format!(r#"<path d="{AR}"/><rect x="64" y="192" width="128" height="20" rx="10"/>"#);
        format!(
            r##"<g transform="translate({cx} {cy}) scale({s}) translate(-128 -126)"><g fill="#fff" fill-opacity=".85" transform="translate(0 4)">{g}</g><g fill="{dark}">{g}</g></g>"##
        )
    };
    let body = match style {
        0 => format!(
            r#"{sq}{faint}<g filter="url(#gl)"><path d="{HEX}" fill="none" stroke="url(#n)" stroke-width="8" stroke-linejoin="round"/><g {gs} fill="url(#n)"><path d="{BOLT}"/>{}</g></g>"#,
            bar("")
        ),
        1 => format!(
            r##"{sq}{blobs}<g filter="url(#sh)"><path d="{BOLT}" {glass}/>{}</g><path d="M150 40L112 82H122z" fill="#fff" fill-opacity=".5"/>"##,
            bar(glass)
        ),
        2 => format!(
            r##"{sq}{blobs}<g filter="url(#sh)"><path d="{HEX}" fill="#fff" fill-opacity=".16" stroke="#fff" stroke-opacity=".85" stroke-width="5" stroke-linejoin="round"/></g><g transform="translate(128 128) scale(.78) translate(-128 -128)"><path d="{HEX}" fill="none" stroke="#fff" stroke-opacity=".35" stroke-width="3" stroke-linejoin="round"/></g><g {gs} filter="url(#sh)"><path d="{BOLT}" {glass}/>{}</g>"##,
            bar(glass)
        ),
        3 => {
            let rings: String = [(58, ".7"), (84, ".4"), (110, ".2")]
                .iter()
                .map(|(r, o)| format!(r#"<circle cx="128" cy="128" r="{r}" fill="none" stroke="{n1}" stroke-opacity="{o}" stroke-width="3"/>"#))
                .collect();
            let sparks = format!(
                r#"<g stroke="{n1}" stroke-width="5" stroke-linecap="round"><line x1="46" y1="64" x2="66" y2="78"/><line x1="210" y1="58" x2="190" y2="74"/><line x1="40" y1="164" x2="62" y2="158"/><line x1="216" y1="168" x2="194" y2="160"/></g>"#
            );
            format!(
                r##"{sq}{faint}{rings}{sparks}<g filter="url(#gl)"><path d="{BOLT}" fill="url(#v)" stroke="#fff" stroke-width="3" stroke-linejoin="round"/><rect x="70" y="198" width="116" height="18" rx="9" fill="url(#n)"/></g>"##
            )
        }
        4 => {
            let lines: String = [
                "M40 77H12",
                "M40 179H12",
                "M216 77H244",
                "M216 179H244",
                "M128 26V6",
                "M128 230V250",
            ]
            .iter()
            .map(|d| format!(r#"<path d="{d}" stroke="url(#n)" stroke-width="3" fill="none"/>"#))
            .collect();
            format!(
                r##"{sq}{faint}<g filter="url(#gl)">{lines}<path d="{HEX}" fill="none" stroke="url(#n)" stroke-width="8" stroke-linejoin="round"/><g transform="translate(128 128) scale(.8) translate(-128 -128)"><path d="{HEX}" fill="none" stroke="{n2}" stroke-opacity=".7" stroke-width="5" stroke-linejoin="round"/></g><g transform="translate(128 126) scale(.52) translate(-128 -126)" fill="url(#v)"><path d="{BOLT}"/>{}</g></g>"##,
                bar("")
            )
        }
        5 => format!(
            r##"{sq}{blobs}<g filter="url(#gl)"><path d="{BOLT}" fill="none" stroke="{n1}" stroke-width="7" stroke-linejoin="round"/></g><g filter="url(#sh)"><path d="{BOLT}" fill="#fff" fill-opacity=".42" clip-path="url(#lh)"/><path d="{BOLT}" fill="#fff" fill-opacity=".14" clip-path="url(#rh)"/><path d="{BOLT}" fill="none" stroke="#fff" stroke-opacity=".9" stroke-width="3" stroke-linejoin="round"/>{}</g>"##,
            bar(
                r##"fill="#fff" fill-opacity=".3" stroke="#fff" stroke-width="3" stroke-opacity=".9""##
            )
        ),
        6 => format!(
            r##"<rect width="256" height="256" rx="58" fill="url(#b)"/><g filter="url(#sh)"><g transform="translate(128 128) scale(.86) translate(-128 -128)"><path d="{AR}" fill="#fff"/><rect x="64" y="192" width="128" height="20" rx="10" fill="#fff"/></g></g>"##
        ),
        7 => format!(
            r##"<rect width="256" height="256" rx="58" fill="url(#b)"/><circle cx="128" cy="128" r="112" fill="none" stroke="{n1}" stroke-width="8"/><g filter="url(#sh)"><circle cx="128" cy="128" r="92" fill="url(#f)"/></g>{}"##,
            emb(128.0, 128.0, 0.6)
        ),
        _ => format!(
            r##"<rect width="256" height="256" rx="58" fill="url(#b)"/><g filter="url(#sh)"><path d="{SHIELD}" fill="url(#f)" stroke="{n1}" stroke-width="8" stroke-linejoin="round"/></g>{}"##,
            emb(128.0, 132.0, 0.5)
        ),
    };
    format!(
        r#"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 256 256"><defs>{defs}</defs>{body}</svg>"#
    )
}

/// Dibuja el icono a `size`×`size` píxeles y lo devuelve en RGBA sin
/// premultiplicar.
pub fn render(style: i32, pal: &Palette, size: u32) -> Option<Vec<u8>> {
    let tree = usvg::Tree::from_str(&svg(style, pal), &usvg::Options::default()).ok()?;
    let mut pix = tiny_skia::Pixmap::new(size, size)?;
    let k = size as f32 / 256.0;
    resvg::render(
        &tree,
        tiny_skia::Transform::from_scale(k, k),
        &mut pix.as_mut(),
    );
    let mut out = Vec::with_capacity((size * size * 4) as usize);
    for px in pix.pixels() {
        let c = px.demultiply();
        out.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
    }
    Some(out)
}

/// Los tamaños que usan la ventana y las bandejas: 32 y 256 píxeles.
pub fn rgba_set(style: i32, pal: &Palette) -> Option<Vec<(u32, u32, Vec<u8>)>> {
    Some(vec![
        (32, 32, render(style, pal, 32)?),
        (256, 256, render(style, pal, 256)?),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_style_and_palette_renders() {
        for style in 0..STYLES.len() as i32 {
            for i in 0..=NAMED.len() as i32 {
                let img = render(style, &palette(i, 0x3b82f6), 64)
                    .unwrap_or_else(|| panic!("estilo {style} paleta {i}"));
                assert_eq!(img.len(), 64 * 64 * 4);
                // El centro no está vacío.
                assert!(img.as_chunks::<4>().0.iter().any(|p| p[3] == 255));
            }
        }
    }

    #[test]
    fn corners_are_transparent_in_squircle_styles() {
        let img = render(0, &palette(1, 0), 64).unwrap();
        assert_eq!(img[3], 0);
    }

    /// Genera los iconos fijos (`ui/icon.png` y los tamaños del .ico y del
    /// .icns) con el estilo y el color por defecto:
    /// `cargo test -p omni-gui write_default_icons -- --ignored`
    /// con `OMNILOAD_ICON_OUT` apuntando a una carpeta.
    #[test]
    #[ignore]
    fn write_default_icons() {
        let dir = std::path::PathBuf::from(std::env::var("OMNILOAD_ICON_OUT").unwrap());
        std::fs::create_dir_all(&dir).unwrap();
        let pal = from_accent(DEFAULT_ACCENT);
        for size in [16u32, 24, 32, 48, 64, 128, 256, 512, 1024] {
            let px = render(0, &pal, size).unwrap();
            let file = std::fs::File::create(dir.join(format!("icon-{size}.png"))).unwrap();
            let mut enc = png::Encoder::new(file, size, size);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.write_header().unwrap().write_image_data(&px).unwrap();
        }
    }

    #[test]
    fn accent_palette_follows_the_accent() {
        let a = from_accent(0xff0000);
        assert_eq!(a.n2, 0xff0000);
        assert!(a.base < 0x300000);
    }
}
