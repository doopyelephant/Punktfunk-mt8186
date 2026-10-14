//! Welcome: the lockup, the Recommended / Custom choice, and the manage-mode buttons.

use super::*;

/// Uninstall from Welcome: flips this run to the teardown path, then advances exactly like
/// Continue — the install thread reads the flag through the same `Ctx`.
fn uninstall_button(ctx: &Ctx) -> Element {
    let mut ctx = ctx.clone();
    ctx.uninstall = true;
    button("Uninstall")
        .on_click(move || {
            ctx.set_uninstall.call(true);
            advance(&ctx, WizStep::Welcome);
        })
        .into()
}

/// Pick Custom or Recommended. Recommended installs the defaults, so it drops what an earlier
/// Custom pass edited on Configure; the network answer and the password stay.
fn choose(ctx: &Ctx, custom: bool) {
    if !custom {
        let s = &ctx.screen;
        let mut defaults = WinChoices::derive(&s.facts, ctx.preset.artifact);
        defaults.web_password = s.choices.web_password.clone();
        defaults.network = s.choices.network.clone();
        ctx.set_screen.call(WinScreen::new(
            s.facts.clone(),
            defaults,
            ctx.preset.artifact,
        ));
    }
    ctx.set_custom.call(custom);
}

/// One of the two setup options on Welcome: a radio in a card, the whole card tappable. The
/// chosen one shows an accent ring drawn OVER the card (a stroke-only rectangle, so taps pass
/// through) that fades in and out — the card itself never changes size, so nothing shifts.
fn option_card(ctx: &Ctx, custom: bool, title: &str, detail: &str) -> Element {
    let selected = ctx.custom == custom;
    let (pick, tap) = (ctx.clone(), ctx.clone());
    let card = border(
        vstack((
            RadioButton::new(title)
                .group("setup")
                .checked(selected)
                .on_checked(move || choose(&pick, custom)),
            text_block(detail)
                .wrap()
                .font_size(12.5)
                .foreground(ThemeRef::SecondaryText)
                .margin(edges(30.0, 0.0, 0.0, 0.0)),
        ))
        .spacing(2.0),
    )
    .background(ThemeRef::CardBackground)
    .border_brush(ThemeRef::CardStroke)
    .border_thickness(Thickness::uniform(1.0))
    .corner_radius(10.0)
    .padding(edges(16.0, 12.0, 16.0, 14.0))
    .on_tapped(move || choose(&tap, custom));
    let ring = Shape::rectangle()
        .stroke(brand::VIOLET)
        .stroke_thickness(2.0)
        .corner_radius(10.0)
        .opacity(if selected { 1.0 } else { 0.0 })
        .with_opacity_transition(std::time::Duration::from_millis(180));
    grid((card, ring)).into()
}

/// The brand lockup — the mark above the "funk" wordmark — with the website's one-shot
/// intro (pfweb `BrandMark.tsx` / `Wordmark.tsx`) at progress `t` (0 → 1 over 1.3 s):
/// the two circles orbit once on an axis into the screen, scaling in antiphase with a
/// small diagonal sway that vanishes at rest, the lens highlight fades in over the tail,
/// and the letters slide in from the right, staggered. Everything lands on the static mark.
fn lockup(t: f64, scheme: ColorScheme) -> Element {
    use std::f64::consts::{FRAC_1_SQRT_2, TAU};
    // The site's geometry: circles r=194.41 in a 1000 box, cropped to the mark's own box.
    const R: f64 = 194.41;
    const BOX: f64 = 583.776;
    const ORIGIN: (f64, f64) = (208.627, 208.443);
    let k = MARK / BOX;
    let angle = TAU * (1.0 - (1.0 - t.clamp(0.0, 1.0)).powi(4));
    let circle = |cx: f64, cy: f64, side: f64, color: Color| -> (f64, Element) {
        let z = side * angle.sin() * 0.34;
        let p = 1.05 / (1.05 - z);
        let mag = side * 0.06 * (angle.cos() - 1.0);
        let (tx, ty) = (
            mag * -FRAC_1_SQRT_2 * 1000.0 * p,
            mag * FRAC_1_SQRT_2 * 1000.0 * p,
        );
        let d = 2.0 * R * k * p;
        let x = (cx - ORIGIN.0 + tx) * k - d / 2.0;
        let y = (cy - ORIGIN.1 + ty) * k - d / 2.0;
        (
            p,
            Shape::ellipse()
                .fill(color)
                .width(d)
                .height(d)
                .horizontal_alignment(HorizontalAlignment::Left)
                .vertical_alignment(VerticalAlignment::Top)
                .margin(edges(x, y, 0.0, 0.0))
                .into(),
        )
    };
    let light = circle(403.037, 597.262, 1.0, brand::LIGHT_CIRCLE);
    let deep = circle(597.808, 402.853, -1.0, brand::DEEP_CIRCLE);
    // The nearer circle paints on top; the crisp overlap only reads once they settle.
    let mut layers: Vec<Element> = if light.0 <= deep.0 {
        vec![light.1, deep.1]
    } else {
        vec![deep.1, light.1]
    };
    let h = ((t - 0.6) / 0.346).clamp(0.0, 1.0);
    let h = 1.0 - (1.0 - h).powi(3);
    if let Some(uri) = brand::uri("lens-highlight.png") {
        let size = MARK * (0.6 + 0.4 * h);
        layers.push(
            Image::new_with_uri(uri)
                .stretch(Stretch::Uniform)
                .width(size)
                .height(size)
                .opacity(h)
                .horizontal_alignment(HorizontalAlignment::Center)
                .vertical_alignment(VerticalAlignment::Center)
                .into(),
        );
    }
    let mark = grid(layers).width(MARK).height(MARK);

    // Letters: each slides in from its own width, 0.55 s from 0.15 s + 90 ms per letter.
    const LETTER_W: [f64; 4] = [108.54, 133.33, 142.87, 141.79];
    let wk = WORD / 579.0;
    let wh = 136.0 * wk;
    let letters: Vec<Element> = (0..4)
        .filter_map(|i| {
            let uri = brand::letter(scheme, i)?;
            let start = (0.15 + 0.09 * i as f64) / 1.3;
            let e = ((t - start) / (0.55 / 1.3)).clamp(0.0, 1.0);
            let e = 1.0 - (1.0 - e).powi(5);
            let off = (1.0 - e) * LETTER_W[i] * wk;
            Some(
                Image::new_with_uri(uri)
                    .stretch(Stretch::Uniform)
                    .width(WORD)
                    .height(wh)
                    .opacity(e)
                    .margin(edges(off, 0.0, -off, 0.0))
                    .into(),
            )
        })
        .collect();
    let word = grid(letters).width(WORD).height(wh);
    vstack((mark, word))
        .spacing(10.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .into()
}

pub(super) fn welcome_page(ctx: &Ctx) -> Element {
    let what = match ctx.preset.artifact {
        Artifact::Host => "host",
        Artifact::Client => "client",
    };
    // Manage mode (D9): an installed box re-titles Welcome. The uninstaller exe (D6) offers
    // the teardown only; the installer offers Reconfigure — the upgrade path — or Uninstall.
    let installed = ctx.screen.facts.installed_for(ctx.preset.artifact);
    let subtitle = installed.map(|inst| match &inst.version {
        Some(v) => format!("{v} · {what} installed"),
        None => format!("{what} installed"),
    });
    let fresh_install = !ctx.preset.uninstall && installed.is_none();
    let (sentence, buttons): (&str, Vec<Element>) = if ctx.preset.uninstall {
        (
            "This removes punktfunk from this PC. Identity, pairings and passwords stay — a reinstall picks them up.",
            vec![uninstall_button(ctx)],
        )
    } else if !fresh_install {
        let mut reconfigure = ctx.clone();
        reconfigure.custom = true;
        (
            "Reconfigure keeps what is on this PC and applies your changes. Uninstall removes it — identity, pairings and passwords stay.",
            vec![uninstall_button(ctx), {
                let go = reconfigure.clone();
                button("Reconfigure")
                    .accent()
                    .min_width(124.0)
                    .on_click(move || {
                        go.set_custom.call(true);
                        advance(&go, WizStep::Welcome);
                    })
                    .into()
            }],
        )
    } else {
        (
            match ctx.preset.artifact {
                Artifact::Host => {
                    "This PC becomes the one you stream from: the punktfunk host sends its screen, audio and games to the devices you play on."
                }
                Artifact::Client => {
                    "This PC becomes a device you play on: the punktfunk client shows the stream from the PC you stream from."
                }
            },
            vec![continue_button(
                ctx,
                WizStep::Welcome,
                go_label(ctx, WizStep::Welcome),
            )],
        )
    };
    let mut children: Vec<Element> = vec![lockup(ctx.logo, ctx.scheme)];
    if let Some(subtitle) = subtitle {
        children.push(
            text_block(subtitle)
                .font_size(16.0)
                .semibold()
                .foreground(ThemeRef::SecondaryText)
                .horizontal_alignment(HorizontalAlignment::Center)
                .into(),
        );
    }
    children.push(
        text_block(sentence)
            .wrap()
            .font_size(15.0)
            .foreground(ThemeRef::SecondaryText)
            .horizontal_alignment(HorizontalAlignment::Center)
            .max_width(520.0)
            .into(),
    );
    if fresh_install {
        let (recommended, custom) = match ctx.preset.artifact {
            Artifact::Host => (
                "The defaults: virtual display and gamepad drivers, the HDR layer, a tray icon, and the service starts right away. A web console password is generated for you.",
                "Pick the drivers, Moonlight compatibility, firewall scope, autostart and the console password yourself.",
            ),
            Artifact::Client => (
                "Install into your profile with Start-menu shortcuts.",
                "Choose whether a desktop shortcut is added.",
            ),
        };
        children.push(
            grid((
                option_card(ctx, false, "Recommended", recommended).grid_column(0),
                option_card(ctx, true, "Custom", custom).grid_column(1),
            ))
            .columns([GridLength::Star(1.0), GridLength::Star(1.0)])
            .column_spacing(12.0)
            .max_width(720.0)
            .margin(edges(0.0, 10.0, 0.0, 0.0))
            .into(),
        );
    }
    let content = vstack(children)
        .spacing(16.0)
        .horizontal_alignment(HorizontalAlignment::Center)
        .vertical_alignment(VerticalAlignment::Center)
        .into();
    frame(ctx, WizStep::Welcome, content, buttons)
}
