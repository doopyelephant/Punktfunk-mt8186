//! Done: the outcome, the fresh host's password card and the next steps.

use super::*;

/// What Done shows in place of the password until Show is pressed.
pub const PASSWORD_MASK: &str = "••••••••••••";

/// A square, icon-only button; `name` is what UI Automation (and the flow tests) call it.
fn icon_button(icon: Icon, name: &str, on_click: impl Fn() + 'static) -> Element {
    Element::from(
        button("")
            .icon(icon)
            .width(32.0)
            .height(32.0)
            .on_click(on_click),
    )
    .padding(Thickness::uniform(0.0))
    .automation_name(name)
}

/// Plain text onto the system clipboard. Best effort: a refused clipboard (another app
/// holding it, no UI thread in a test host) is not worth an error on the finish page.
fn copy_to_clipboard(text: &str) {
    use windows::ApplicationModel::DataTransfer::{Clipboard, DataPackage};
    let _ = (|| -> windows::core::Result<()> {
        let package = DataPackage::new()?;
        package.SetText(&windows::core::HSTRING::from(text))?;
        Clipboard::SetContent(&package)?;
        Clipboard::Flush()
    })();
}

/// A follow-up action on Done: what to do, why, and the link that does it.
fn next_step(title: &str, detail: &str, link: &str, uri: &str) -> Element {
    card(
        vstack((
            text_block(title).semibold(),
            text_block(detail)
                .wrap()
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
            HyperlinkButton::new(link)
                .navigate_uri(uri)
                .margin(edges(-10.0, 4.0, 0.0, 0.0)),
        ))
        .spacing(4.0),
    )
    .into()
}

pub(super) fn done_page(ctx: &Ctx) -> Element {
    let children = if ctx.uninstall {
        done_uninstalled()
    } else {
        done_installed(ctx)
    };
    let content = scroll_view(vstack(children).spacing(12.0).max_width(760.0)).into();
    frame(
        ctx,
        WizStep::Done,
        content,
        vec![button("Finish")
            .accent()
            .min_width(124.0)
            .on_click(|| std::process::exit(0))
            .into()],
    )
}

fn done_uninstalled() -> Vec<Element> {
    vec![
        text_block("punktfunk was removed from this PC.")
            .font_size(18.0)
            .semibold()
            .into(),
        text_block(
            r"Kept on purpose: %ProgramData%\punktfunk (identity, passwords, update cache) — a reinstall picks it up.",
        )
        .wrap()
        .foreground(ThemeRef::SecondaryText)
        .into(),
    ]
}

/// The outcome, the password card on a fresh host, the next steps, and the D11/D12
/// footnotes in the transcript's words as attention cards.
fn done_installed(ctx: &Ctx) -> Vec<Element> {
    use punktfunk_setup::facts::DOCS;
    let fresh = ctx.screen.fresh();
    let mut children: Vec<Element> = vec![text_block(match (ctx.preset.artifact, fresh) {
        (Artifact::Host, true) => "The punktfunk host is installed and streaming.",
        (Artifact::Host, false) => "The punktfunk host is up to date.",
        (Artifact::Client, true) => "The punktfunk client is installed.",
        (Artifact::Client, false) => "The punktfunk client is up to date.",
    })
    .font_size(18.0)
    .semibold()
    .into()];
    // Fresh host: the password card, the one thing the user must leave with (D9) — and
    // the only place a Recommended install shows it.
    if ctx.preset.artifact == Artifact::Host
        && fresh
        && let Some(pw) = &ctx.screen.choices.web_password
    {
        children.push(password_card(ctx, pw));
    }
    children.push(
        text_block("Next")
            .font_size(12.0)
            .semibold()
            .foreground(ThemeRef::SecondaryText)
            .margin(edges(2.0, 6.0, 0.0, -4.0))
            .into(),
    );
    let (first, second) = next_steps(ctx.preset.artifact);
    children.push(
        grid((first.grid_column(0), second.grid_column(1)))
            .columns([GridLength::Star(1.0), GridLength::Star(1.0)])
            .column_spacing(12.0)
            .into(),
    );
    for note in win_report::footnotes(&ctx.screen.facts, &ctx.screen.effective_choices()) {
        children.push(
            card(
                text_block(note)
                    .wrap()
                    .foreground(ThemeRef::SystemAttention),
            )
            .into(),
        );
    }
    children.push(
        HyperlinkButton::new("Stuck? Troubleshooting")
            .navigate_uri(format!("{DOCS}/troubleshooting"))
            .margin(edges(-10.0, 0.0, 0.0, 0.0))
            .into(),
    );
    children
}

/// The fresh host's console password, masked until Show: a finish page can sit on a screen
/// for a while. Copy works either way. Changing it later is a file edit and a service
/// restart (docs: web-console.md), not a console feature.
fn password_card(ctx: &Ctx, pw: &str) -> Element {
    let reveal = ctx.reveal_done;
    let toggle = ctx.clone();
    let copy = ctx.clone();
    let text = pw.to_string();
    card(
        vstack((
            text_block("Your web console password").semibold(),
            hstack((
                text_block(if reveal { pw.to_string() } else { PASSWORD_MASK.into() })
                    .font_size(22.0)
                    .font_family(MONO)
                    .selectable()
                    .min_width(240.0)
                    .vertical_alignment(VerticalAlignment::Center),
                icon_button(
                    if reveal {
                        Icon::font("\u{ED1A}") // Segoe Fluent "Hide"
                    } else {
                        Symbol::View.into()
                    },
                    if reveal { "Hide" } else { "Show" },
                    move || toggle.set_reveal_done.call(!reveal),
                ),
                icon_button(
                    if ctx.copied {
                        Symbol::Accept.into()
                    } else {
                        Symbol::Copy.into()
                    },
                    if ctx.copied { "Copied" } else { "Copy" },
                    move || {
                        copy_to_clipboard(&text);
                        copy.set_copied.call(true);
                    },
                ),
            ))
            .spacing(6.0)
            .vertical_alignment(VerticalAlignment::Center),
            text_block(
                r"Stored in %ProgramData%\punktfunk\web-password. To change it later, edit that file and run `punktfunk-host service restart` from an elevated PowerShell.",
            )
            .wrap()
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(6.0),
    )
    .into()
}

/// The two follow-up cards for what was installed.
fn next_steps(artifact: Artifact) -> (Element, Element) {
    use punktfunk_setup::facts::DOCS;
    match artifact {
        Artifact::Host => (
            next_step(
                "Open the web console",
                "Approve devices, pick what to stream, change settings. The certificate is the host's own — continue past the browser's warning.",
                "https://127.0.0.1:47992",
                "https://127.0.0.1:47992/",
            ),
            next_step(
                "Install a client",
                "On the device you play on — Windows, macOS, iOS, Android, Linux, Steam Deck — then connect and press Approve in the console.",
                "Client downloads",
                &format!("{DOCS}/install-client"),
            ),
        ),
        Artifact::Client => (
            next_step(
                "Open Punktfunk",
                "It is in the Start menu. Pick your host from the list, or add one by address.",
                "Pairing help",
                &format!("{DOCS}/pairing"),
            ),
            next_step(
                "No host yet?",
                "Install the punktfunk host on the PC you stream from.",
                "Host install guide",
                &format!("{DOCS}/windows-host"),
            ),
        ),
    }
}
