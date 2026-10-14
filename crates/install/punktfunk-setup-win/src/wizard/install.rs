//! Install: the phase checklist and the command transcript.

use super::*;

pub(super) fn install_page(ctx: &Ctx, log: &[LogLine]) -> Element {
    let mut items: Vec<Element> = Vec::new();
    for line in log {
        items.push(match line.kind {
            LineKind::Phase => text_block(line.text.as_str())
                .semibold()
                .font_size(13.5)
                .margin(edges(0.0, 10.0, 0.0, 2.0))
                .into(),
            LineKind::Cmd => text_block(format!("  + {}", line.text))
                .font_size(12.0)
                .font_family(MONO)
                .wrap()
                .foreground(ThemeRef::TertiaryText)
                .into(),
            LineKind::Ok => text_block(format!("✓ {}", line.text))
                .wrap()
                .foreground(ThemeRef::SecondaryText)
                .into(),
            LineKind::Warn => text_block(format!("⚠ {}", line.text))
                .wrap()
                .foreground(ThemeRef::SystemCaution)
                .into(),
            LineKind::Fail => text_block(format!("✗ {}", line.text))
                .wrap()
                .foreground(ThemeRef::SystemCritical)
                .into(),
            LineKind::Detail => text_block(format!("  {}", line.text))
                .font_size(12.0)
                .wrap()
                .foreground(ThemeRef::TertiaryText)
                .into(),
            LineKind::Text => text_block(line.text.as_str()).wrap().into(),
        });
    }
    let mut children: Vec<Element> = Vec::new();
    let status = match (ctx.install, ctx.uninstall) {
        (InstallPhase::Failed, _) => "Something went wrong — the log below has the step.",
        (_, true) => "Removing punktfunk…",
        (_, false) => "Installing punktfunk…",
    };
    let mut head: Vec<Element> = Vec::new();
    if ctx.install == InstallPhase::Running {
        head.push(
            ProgressRing::indeterminate()
                .width(16.0)
                .height(16.0)
                .into(),
        );
    }
    head.push(
        text_block(status)
            .foreground(if ctx.install == InstallPhase::Failed {
                ThemeRef::SystemCritical
            } else {
                ThemeRef::SecondaryText
            })
            .into(),
    );
    children.push(hstack(head).spacing(8.0).into());
    // Range 0–100 is the widget's default; the value tweens in root state.
    children.push(ProgressBar::new(ctx.bar * 100.0).into());
    children.push(
        card(scroll_view(vstack(items).spacing(2.0)))
            .padding(edges(16.0, 10.0, 16.0, 12.0))
            .into(),
    );
    let content = vstack(children).spacing(12.0).into();
    let buttons = if ctx.install == InstallPhase::Failed {
        vec![button("Close").on_click(|| std::process::exit(1)).into()]
    } else {
        vec![] // no cancel mid-plan: the executor's steps are not interruptible-safe
    };
    frame(ctx, WizStep::Install, content, buttons)
}
