//! Configure: the settings sections and their editors.

use super::*;

/// One titled section: its rows in a single card, divided, never a card per row.
fn section(ctx: &Ctx, title: &str, fields: &[Field], lead: Option<Element>) -> Element {
    let mut rows: Vec<Element> = Vec::new();
    if let Some(lead) = lead {
        rows.push(lead);
    }
    for field in fields {
        let row = config_row(ctx, *field);
        rows.push(if rows.is_empty() {
            row
        } else {
            border(row)
                .border_brush(ThemeRef::DividerStroke)
                .border_thickness(edges(0.0, 1.0, 0.0, 0.0))
                .padding(edges(0.0, 10.0, 0.0, 0.0))
                .margin(edges(0.0, 10.0, 0.0, 0.0))
                .into()
        });
    }
    vstack((
        text_block(title)
            .font_size(12.0)
            .semibold()
            .foreground(ThemeRef::SecondaryText)
            .margin(edges(2.0, 0.0, 0.0, 6.0)),
        card(vstack(rows)),
    ))
    .into()
}

pub(super) fn configure_page(ctx: &Ctx) -> Element {
    // The D11 coexistence note leads the Network section: a visible row, never a dialog.
    let coexistence = ctx.screen.coexistence_note().map(|note| {
        vstack((
            text_block("Runs next to your other streaming host")
                .semibold()
                .foreground(ThemeRef::SystemAttention),
            text_block(note)
                .wrap()
                .font_size(12.0)
                .foreground(ThemeRef::SecondaryText),
        ))
        .spacing(2.0)
        .into()
    });
    let mut columns: [Vec<Element>; 2] = [Vec::new(), Vec::new()];
    for (i, (title, fields)) in ctx.screen.sections().into_iter().enumerate() {
        let lead = if title == "Network" {
            coexistence.clone()
        } else {
            None
        };
        columns[i % 2].push(section(ctx, title, &fields, lead));
    }
    let [left, right] = columns;
    let body: Element = if right.is_empty() {
        vstack(left).spacing(18.0).max_width(560.0).into()
    } else {
        grid((
            vstack(left).spacing(18.0).grid_column(0),
            vstack(right).spacing(18.0).grid_column(1),
        ))
        .columns([GridLength::Star(1.0), GridLength::Star(1.0)])
        .column_spacing(18.0)
        .into()
    };
    let content = scroll_view(body).into();
    frame(
        ctx,
        WizStep::Configure,
        content,
        vec![
            back_button(ctx, WizStep::Configure),
            continue_button(ctx, WizStep::Configure, go_label(ctx, WizStep::Configure)),
        ],
    )
}

fn config_row(ctx: &Ctx, field: Field) -> Element {
    let editor: Element = match ctx.screen.editor(field) {
        Editor::Toggle(on) => {
            let ctx = ctx.clone();
            ToggleSwitch::new(on)
                .on_toggled(move |v: bool| {
                    let mut s = ctx.screen.clone();
                    s.set_bool(field, v);
                    ctx.set_screen.call(s);
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
        Editor::TriState(value) => {
            let ctx = ctx.clone();
            ComboBox::new(["Keep the box's setting", "On", "Off"])
                .selected_index(match value {
                    None => 0,
                    Some(true) => 1,
                    Some(false) => 2,
                })
                .on_selection_changed(move |i: i32| {
                    let mut s = ctx.screen.clone();
                    match i {
                        0 => s.keep_box_setting(field),
                        1 => s.set_bool(field, true),
                        _ => s.set_bool(field, false),
                    }
                    ctx.set_screen.call(s);
                })
                .vertical_alignment(VerticalAlignment::Center)
                .into()
        }
        Editor::Bind(value) => {
            // Four answers, one control pair. The address box stays beside the list rather than
            // appearing on the fourth choice: a row that changes height as you open a dropdown
            // reflows the whole column under the pointer. It is enabled only when it means
            // something, so an address typed there is never quietly ignored.
            const KEEP: usize = 0;
            let selected = match value.as_deref() {
                None => KEEP,
                Some(LOOPBACK_BIND) => 1,
                Some(LAN_BIND) => 2,
                Some(_) => 3,
            };
            let typed = match (&value, selected) {
                (Some(addr), 3) => addr.clone(),
                _ => String::new(),
            };
            let pick = ctx.clone();
            let edit = ctx.clone();
            hstack((
                ComboBox::new([
                    "Leave host.env alone",
                    "This PC only",
                    "This local network",
                    "A specific address",
                ])
                .selected_index(selected as i32)
                .on_selection_changed(move |i: i32| {
                    let mut s = pick.screen.clone();
                    s.set_bind(match i {
                        0 => None,
                        1 => Some(LOOPBACK_BIND.to_string()),
                        2 => Some(LAN_BIND.to_string()),
                        _ => Some(String::new()),
                    });
                    pick.set_screen.call(s);
                })
                .vertical_alignment(VerticalAlignment::Center),
                TextBox::new(typed)
                    .placeholder_text("e.g. 100.64.0.3")
                    .enabled(selected == 3)
                    .min_width(160.0)
                    .on_text_changed(move |addr: String| {
                        let mut s = edit.screen.clone();
                        s.set_bind(Some(addr));
                        edit.set_screen.call(s);
                    }),
            ))
            .spacing(6.0)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
        }
        Editor::Password(value) => {
            // WinUI's own eye only appears while the user types, so a pre-generated password
            // would never be revealable — this toggle drives the reveal mode instead.
            let reveal = ctx.reveal;
            let edit = ctx.clone();
            let toggle = ctx.clone();
            let regen = ctx.clone();
            hstack((
                PasswordBox::new()
                    .value(value.unwrap_or_default())
                    .password_reveal_mode(if reveal {
                        PasswordRevealMode::Visible
                    } else {
                        PasswordRevealMode::Hidden
                    })
                    .reveal_button_enabled(false)
                    .font_family(MONO)
                    .min_width(240.0)
                    .on_password_changed(move |pw: String| {
                        let mut s = edit.screen.clone();
                        s.set_password(pw);
                        edit.set_screen.call(s);
                    }),
                button(if reveal { "Hide" } else { "Show" })
                    .min_width(72.0)
                    .on_click(move || toggle.set_reveal.call(!reveal)),
                button("Regenerate").subtle().on_click(move || {
                    if let Ok(pw) = punktfunk_setup::platform::windows::sys::random_hex(12) {
                        let mut s = regen.screen.clone();
                        s.set_password(pw);
                        regen.set_screen.call(s);
                    }
                }),
            ))
            .spacing(6.0)
            .vertical_alignment(VerticalAlignment::Center)
            .into()
        }
    };
    let words = vstack((
        text_block(WinScreen::label(field)).semibold(),
        text_block(ctx.screen.why(field))
            .wrap()
            .font_size(12.0)
            .foreground(ThemeRef::SecondaryText),
    ))
    .spacing(1.0);
    // The password and bind editors are wider than a section column's Auto slot leaves room
    // for: side by side, the label column collapses to nothing and the wrapped hint explodes
    // in height. Those rows stack instead.
    if field == Field::Password || field == Field::WebBind {
        return vstack((words, editor.margin(edges(0.0, 8.0, 0.0, 0.0))))
            .spacing(2.0)
            .into();
    }
    grid((
        words
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(0),
        editor.grid_column(1),
    ))
    .columns([GridLength::Star(1.0), GridLength::Auto])
    .column_spacing(16.0)
    .into()
}
