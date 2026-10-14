//! Network: the bind and exposure answer.

use super::*;

pub(super) fn network_page(ctx: &Ctx) -> Element {
    let name = match &ctx.screen.choices.network {
        NetworkAnswer::MakePrivate(n) if !n.is_empty() => n.clone(),
        _ => ctx
            .screen
            .facts
            .public_networks()
            .first()
            .map(|n| n.name.clone())
            .unwrap_or_else(|| "this network".into()),
    };
    let selected = match &ctx.screen.choices.network {
        NetworkAnswer::MakePrivate(_) => 0,
        NetworkAnswer::OpenPublicRules => 1,
        NetworkAnswer::Skip => 2,
    };
    let radio = {
        let ctx = ctx.clone();
        let name = name.clone();
        RadioButtons::new([
            format!("This is my home network — make '{name}' Private (recommended)"),
            "Keep it Public, open the firewall for it".to_string(),
            "Skip — I'll fix it later".to_string(),
        ])
        .selected_index(selected)
        .on_selection_changed(move |i: i32| {
            let mut s = ctx.screen.clone();
            s.set_network(match i {
                0 => NetworkAnswer::MakePrivate(name.clone()),
                1 => NetworkAnswer::OpenPublicRules,
                _ => NetworkAnswer::Skip,
            });
            ctx.set_screen.call(s);
        })
    };
    let content = vstack((
        text_block(format!("'{name}' is set to Public"))
            .font_size(16.0)
            .semibold(),
        text_block(
            "Windows scopes the standard firewall rules to private networks, so on this network the host would be silently unreachable. VPN and virtual adapters are routinely Public by design — this changes only the named network, never a global setting.",
        )
        .wrap()
        .foreground(ThemeRef::SecondaryText),
        radio,
    ))
    .spacing(12.0)
    .max_width(640.0)
    .into();
    frame(
        ctx,
        WizStep::Network,
        content,
        vec![
            back_button(ctx, WizStep::Network),
            continue_button(ctx, WizStep::Network, "Install"),
        ],
    )
}
