//! The wizard's brand pieces: the lens highlight and the four "funk" letters in each colour
//! scheme, rendered by `scripts/gen-setup-brand.sh` from the website's exact path data —
//! regenerate and commit when the brand changes. The mark's two circles are drawn live as
//! ellipses (`wizard::welcome::lockup`) so they can orbit. `punktfunk.ico` is staged to disk:
//! the window icon takes a file path.

use std::path::PathBuf;
use std::sync::OnceLock;

use windows_reactor::{Color, ColorScheme, EncodedImage};

/// pf-console-ui's dark-appearance brand value; the terminal theme carries the same one.
pub const VIOLET: Color = Color::rgb(0x86, 0x78, 0xf5);

/// The mark's palette (`web/public/favicon.svg`), the same in both schemes.
pub const LIGHT_CIRCLE: Color = Color::rgb(0xa7, 0x9f, 0xf8);
pub const DEEP_CIRCLE: Color = Color::rgb(0x6c, 0x5b, 0xf3);

const LENS: &[u8] = include_bytes!("../assets/lens-highlight.png");
const LETTERS_DARK: [&[u8]; 4] = [
    include_bytes!("../assets/wordmark-dark-0.png"),
    include_bytes!("../assets/wordmark-dark-1.png"),
    include_bytes!("../assets/wordmark-dark-2.png"),
    include_bytes!("../assets/wordmark-dark-3.png"),
];
const LETTERS_LIGHT: [&[u8]; 4] = [
    include_bytes!("../assets/wordmark-light-0.png"),
    include_bytes!("../assets/wordmark-light-1.png"),
    include_bytes!("../assets/wordmark-light-2.png"),
    include_bytes!("../assets/wordmark-light-3.png"),
];
const ICON: &[u8] = include_bytes!("../../../../packaging/windows/branding/punktfunk.ico");

pub fn lens() -> EncodedImage {
    EncodedImage::from_static(LENS)
}

/// The i-th "funk" letter in the scheme's colour (the site's highlight on dark, brand on light).
pub fn letter(scheme: ColorScheme, i: usize) -> EncodedImage {
    EncodedImage::from_static(match scheme {
        ColorScheme::Dark => LETTERS_DARK[i],
        ColorScheme::Light => LETTERS_LIGHT[i],
    })
}

/// `punktfunk.ico` under `%LOCALAPPDATA%\punktfunk\setup-brand`, written once per run; a
/// size mismatch rewrites so a re-baked icon lands. `None` (no icon) when staging failed.
pub fn icon_path() -> Option<PathBuf> {
    static PATH: OnceLock<Option<PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        let dir = PathBuf::from(std::env::var_os("LOCALAPPDATA")?)
            .join("punktfunk")
            .join("setup-brand");
        std::fs::create_dir_all(&dir).ok()?;
        let path = dir.join("punktfunk.ico");
        let stale = std::fs::metadata(&path).map_or(true, |m| m.len() != ICON.len() as u64);
        if stale {
            std::fs::write(&path, ICON).ok()?;
        }
        Some(path)
    })
    .clone()
}
