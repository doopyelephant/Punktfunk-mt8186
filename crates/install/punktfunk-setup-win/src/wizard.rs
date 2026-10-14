//! The D9 step frame over the engine's `WinScreen` (WP2.1): Welcome → Configure → Network →
//! Install → Done, every rule of which lives in `punktfunk_setup::platform::windows::screen`
//! — this module only renders it.
//!
//! State discipline is the client shell's, measured not folklore (`clients/windows/src/app/
//! mod.rs`): everything event- or thread-driven is ROOT `use_async_state`, passed down as
//! values — the WinUI backend wires handlers straight in, and only root async state reliably
//! re-renders. Pages are plain functions (no hooks), so the root's hook order is fixed.
//!
//! The install page is a `Reporter`: the executor runs on a worker thread and every
//! `say`/`plus`/`ok`/`warn` lands as a line of root state — the same dim-command-echo
//! transparency contract as the TUI, phase checklist included.
//!
//! WP2.1b: the stepper draws `run_steps()` — this run's real path, never a ghost Network
//! dot — and each navigation slides the page in directionally via the client shell's manual
//! tween (a worker stepping root state under a generation guard); animations off ⇒ a cut.
//! WP2.2: an installed box opens in manage mode — Welcome re-titled, Reconfigure or
//! Uninstall — and the payload-less `unins000.exe` (D6) is the same page offering only the
//! teardown. Uninstall is run state, chosen there; the executor thread reads it from `Ctx`.

use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::{Arc, Mutex, OnceLock};

use punktfunk_setup::choices::{LAN_BIND, LOOPBACK_BIND};
use punktfunk_setup::platform::windows::choices::NetworkAnswer;
use punktfunk_setup::platform::windows::demo::WinPreset;
use punktfunk_setup::platform::windows::plan::{self, Artifact};
use punktfunk_setup::platform::windows::screen::{Editor, Field, WinScreen, WizStep};
use punktfunk_setup::platform::windows::{choices::WinChoices, report as win_report};
use punktfunk_setup::ui::Reporter;
use windows_reactor::*;

use crate::brand;
use crate::live::{DemoSeams, LiveSeams, RealSeams};
use crate::real::Seams;

mod configure;
mod done;
mod install;
mod network;
mod welcome;

pub use done::PASSWORD_MASK;
use {
    configure::configure_page, done::done_page, install::install_page, network::network_page,
    welcome::welcome_page,
};

/// Long enough that a demo step reads as work happening — the Linux demo's value.
pub const DEMO_LATENCY_MS: u64 = 140;

/// 14 × 16 ms ≈ 220 ms: over before a fast clicker's next click lands (the generation guard
/// cuts a superseded tween regardless).
const SLIDE_FRAMES: u32 = 14;
const SLIDE_TRAVEL: f64 = 36.0;

/// Upcoming dots and the line ahead: a translucent grey that reads on both themes — shapes
/// take a `Color`, not a `ThemeRef`.
const MUTED: Color = Color {
    a: 0x55,
    r: 0x80,
    g: 0x80,
    b: 0x80,
};

/// Stepper dot diameter; the line is 2 px and meets the dot's edge. Each step gets `STEP_W`
/// of the header's right side — narrow, the labels still fit at 11 px.
const DOT: f64 = 10.0;
const STEP_W: f64 = 84.0;

/// The lockup intro: the website's 1.3 s at 16 ms a frame.
const LOGO_FRAMES: u32 = 81;
/// The mark's box and the wordmark's width, in px; both scale from the site's geometry.
const MARK: f64 = 112.0;
const WORD: f64 = 236.0;

/// Command echo and the password: a monospace face that ships with Windows.
const MONO: &str = "Consolas";

/// Where the wizard stands and which way it got there (D9: Continue enters from the right,
/// Back from the left). The install thread's jump to Done is a forward move.
#[derive(Clone, Copy, PartialEq)]
pub struct Nav {
    pub step: WizStep,
    pub forward: bool,
}

/// The page's slide-in: 0 just after a navigation (off to the side, transparent) → 1 settled.
#[derive(Clone, Copy, PartialEq)]
struct Slide {
    progress: f64,
    forward: bool,
}

/// `UISettings.AnimationsEnabled`, the system "show animations" switch: off ⇒ every slide is
/// a cut (D9). Read once — the setting does not change mid-install.
fn animations_enabled() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        windows::UI::ViewManagement::UISettings::new()
            .and_then(|s| s.AnimationsEnabled())
            .unwrap_or(true)
    })
}

#[derive(Clone, PartialEq)]
pub enum LineKind {
    Phase,
    Cmd,
    Ok,
    Warn,
    Fail,
    Detail,
    Text,
}

#[derive(Clone, PartialEq)]
pub struct LogLine {
    pub kind: LineKind,
    pub text: String,
}

#[derive(Clone, Copy, PartialEq)]
pub enum InstallPhase {
    Idle,
    Running,
    Failed,
    Finished,
}

/// The install page as a `Reporter`: lines accumulate here and mirror into root state.
struct ChannelReporter {
    lines: Mutex<Vec<LogLine>>,
    set: AsyncSetState<Vec<LogLine>>,
}

impl ChannelReporter {
    fn new(set: AsyncSetState<Vec<LogLine>>) -> ChannelReporter {
        ChannelReporter {
            lines: Mutex::new(Vec::new()),
            set,
        }
    }

    fn push(&self, kind: LineKind, text: &str) {
        let mut lines = self.lines.lock().unwrap();
        lines.push(LogLine {
            kind,
            text: text.to_string(),
        });
        self.set.call(lines.clone());
    }
}

impl Reporter for ChannelReporter {
    fn say(&self, msg: &str) {
        self.push(LineKind::Phase, msg);
    }
    fn ok(&self, msg: &str) {
        self.push(LineKind::Ok, msg);
    }
    fn warn(&self, msg: &str) {
        self.push(LineKind::Warn, msg);
    }
    fn die(&self, msg: &str) {
        self.push(LineKind::Fail, msg);
    }
    fn plus(&self, cmd: &str) {
        self.push(LineKind::Cmd, cmd);
    }
    fn detail(&self, msg: &str) {
        self.push(LineKind::Detail, msg);
    }
    fn line(&self, msg: &str) {
        self.push(LineKind::Text, msg);
    }
    fn blank(&self) {
        self.push(LineKind::Text, "");
    }
}

/// Everything a navigation or edit handler needs — one clone per handler.
#[derive(Clone)]
struct Ctx {
    preset: WinPreset,
    screen: WinScreen,
    install: InstallPhase,
    /// This run tears down instead of installing: the uninstaller exe, or Uninstall chosen
    /// on the manage Welcome.
    uninstall: bool,
    seams: Seams,
    slide: Slide,
    /// Custom setup: the Configure step is on this run's path. Off, the defaults install
    /// (Recommended); Reconfigure on an installed box always turns it on.
    custom: bool,
    /// The password row's Show/Hide state.
    reveal: bool,
    /// Done's password card: masked until Show, and whether Copy has fired.
    reveal_done: bool,
    copied: bool,
    /// The install page's progress bar, tweened 0 → 1 across the plan's phases.
    bar: f64,
    /// The Welcome lockup's intro, 0 → 1 once on mount (the website's orbit + slide-in).
    logo: f64,
    scheme: ColorScheme,
    set_custom: AsyncSetState<bool>,
    set_reveal: AsyncSetState<bool>,
    set_reveal_done: AsyncSetState<bool>,
    set_copied: AsyncSetState<bool>,
    set_screen: AsyncSetState<WinScreen>,
    set_step: AsyncSetState<Nav>,
    set_install: AsyncSetState<InstallPhase>,
    set_uninstall: AsyncSetState<bool>,
    set_log: AsyncSetState<Vec<LogLine>>,
}

pub struct WizardRoot {
    preset: WinPreset,
    initial: WinScreen,
    seams: Seams,
}

impl WizardRoot {
    pub fn new(preset: WinPreset, seams: Seams) -> WizardRoot {
        let mut choices = WinChoices::derive(&preset.facts, preset.artifact);
        // The fresh-host password row arrives pre-filled (D9): real RNG, 24 hex chars — the
        // PowerShell RNG hack dies here. It travels via an ACL'd temp file, never argv.
        if preset.artifact == Artifact::Host
            && preset.facts.installed.is_none()
            && !preset.facts.web_password_present
            && let Ok(pw) = punktfunk_setup::platform::windows::sys::random_hex(12)
        {
            choices.web_password = Some(pw);
        }
        let initial = WinScreen::new(preset.facts.clone(), choices, preset.artifact);
        WizardRoot {
            preset,
            initial,
            seams,
        }
    }
}

impl Component for WizardRoot {
    fn render(&self, _props: &(), cx: &mut RenderCx) -> Element {
        let (screen, set_screen) = cx.use_async_state(self.initial.clone());
        let start = Nav {
            step: WizStep::Welcome,
            forward: true,
        };
        let (nav, set_step) = cx.use_async_state(start);
        let (install, set_install) = cx.use_async_state(InstallPhase::Idle);
        let (uninstall, set_uninstall) = cx.use_async_state(self.preset.uninstall);
        let (log, set_log) = cx.use_async_state(Vec::<LogLine>::new());
        let (reveal, set_reveal) = cx.use_async_state(false);
        let (reveal_done, set_reveal_done) = cx.use_async_state(false);
        let (copied, set_copied) = cx.use_async_state(false);
        let (custom, set_custom) = cx.use_async_state(false);
        let scheme = cx.use_color_scheme();

        // The lockup intro plays once, on mount — the same worker-tween, no guard needed.
        let (logo, set_logo) = cx.use_async_state(0.0f64);
        cx.use_effect((), {
            let set_logo = set_logo.clone();
            move || {
                if !animations_enabled() {
                    set_logo.call(1.0);
                    return;
                }
                std::thread::spawn(move || {
                    for i in 0..=LOGO_FRAMES {
                        set_logo.call(f64::from(i) / f64::from(LOGO_FRAMES));
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    }
                });
            }
        });

        // The slide is a manual tween (the client shell's): reactor's one-shot animations run
        // from the visual's CURRENT value, and a freshly mounted page has nothing to fade from.
        // A worker steps 0 → 1 after each navigation; the generation guard stops a superseded
        // one so rapid clicks never fight. A value for another `Nav` reads as 0: no flash.
        let generation = cx.use_ref(Arc::new(AtomicU64::new(0)));
        let (anim, set_anim) = cx.use_async_state((start, 0.0f64));
        cx.use_effect(nav, {
            let (set_anim, generation) = (set_anim.clone(), generation.borrow().clone());
            move || {
                let mine = generation.fetch_add(1, SeqCst) + 1;
                if !animations_enabled() {
                    set_anim.call((nav, 1.0));
                    return;
                }
                std::thread::spawn(move || {
                    for i in 0..=SLIDE_FRAMES {
                        if generation.load(SeqCst) != mine {
                            return;
                        }
                        let p = f64::from(i) / f64::from(SLIDE_FRAMES);
                        set_anim.call((nav, 1.0 - (1.0 - p).powi(3))); // ease-out cubic
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    }
                });
            }
        });
        let progress = if anim.0 == nav { anim.1 } else { 0.0 };

        // The install bar: one phase heading per plan phase reaches the log, so the bar's
        // target is the phase count over the plan's; the same worker-tween eases it there.
        let phases_seen = log.iter().filter(|l| l.kind == LineKind::Phase).count();
        let total = plan::build(
            &screen.facts,
            &screen.effective_choices(),
            self.preset.artifact,
            uninstall,
        )
        .phases
        .len()
        .max(1);
        let target = match install {
            InstallPhase::Idle => 0.0,
            InstallPhase::Finished => 1.0,
            _ => (phases_seen as f64 - 0.5).max(0.0) / total as f64,
        };
        let bar_generation = cx.use_ref(Arc::new(AtomicU64::new(0)));
        let (bar, set_bar) = cx.use_async_state(0.0f64);
        cx.use_effect((phases_seen, install == InstallPhase::Finished, total), {
            let (set_bar, generation, from) =
                (set_bar.clone(), bar_generation.borrow().clone(), bar);
            move || {
                let mine = generation.fetch_add(1, SeqCst) + 1;
                if !animations_enabled() {
                    set_bar.call(target);
                    return;
                }
                std::thread::spawn(move || {
                    for i in 0..=SLIDE_FRAMES {
                        if generation.load(SeqCst) != mine {
                            return;
                        }
                        let p = f64::from(i) / f64::from(SLIDE_FRAMES);
                        let eased = 1.0 - (1.0 - p).powi(3);
                        set_bar.call(from + (target - from) * eased);
                        std::thread::sleep(std::time::Duration::from_millis(16));
                    }
                });
            }
        });

        let ctx = Ctx {
            preset: self.preset.clone(),
            screen: screen.clone(),
            install,
            uninstall,
            seams: self.seams.clone(),
            slide: Slide {
                progress,
                forward: nav.forward,
            },
            custom,
            reveal,
            reveal_done,
            copied,
            bar,
            logo,
            scheme,
            set_custom,
            set_reveal,
            set_reveal_done,
            set_copied,
            set_screen,
            set_step,
            set_install,
            set_uninstall,
            set_log,
        };

        match nav.step {
            WizStep::Welcome => welcome_page(&ctx),
            WizStep::Configure => configure_page(&ctx),
            WizStep::Network => network_page(&ctx),
            WizStep::Install => install_page(&ctx, &log),
            WizStep::Done => done_page(&ctx),
        }
    }
}

/// This run's step list. An uninstall run has nothing to configure — Welcome states what is
/// about to happen and Install is the teardown. Recommended (the default) skips Configure;
/// the Network step still materializes on its own trigger, whichever was chosen.
fn run_steps(ctx: &Ctx) -> Vec<WizStep> {
    if ctx.uninstall {
        vec![WizStep::Welcome, WizStep::Install, WizStep::Done]
    } else {
        ctx.screen
            .steps()
            .into_iter()
            .filter(|s| ctx.custom || *s != WizStep::Configure)
            .collect()
    }
}

/// What Continue will do from `cur`: install now, or one more step first.
fn go_label(ctx: &Ctx, cur: WizStep) -> &'static str {
    let next = run_steps(ctx).into_iter().skip_while(|s| *s != cur).nth(1);
    if next == Some(WizStep::Install) {
        "Install"
    } else {
        "Continue"
    }
}

/// The Install step is the teardown on an uninstall run, and its dot says so.
fn step_title(step: WizStep, uninstall: bool) -> &'static str {
    match step {
        WizStep::Install if uninstall => "Uninstall",
        _ => step.title(),
    }
}

/// Continue: move to the step after `cur` on this run's real path.
fn advance(ctx: &Ctx, cur: WizStep) {
    let steps = run_steps(ctx);
    let next = steps
        .iter()
        .skip_while(|s| **s != cur)
        .nth(1)
        .copied()
        .unwrap_or(WizStep::Done);
    if next == WizStep::Network && ctx.screen.choices.network == NetworkAnswer::Skip {
        // The step opens on its recommended answer (D12: recommended first); the radio
        // edits it from there.
        let mut s = ctx.screen.clone();
        let name = s
            .facts
            .public_networks()
            .first()
            .map(|n| n.name.clone())
            .unwrap_or_default();
        s.set_network(NetworkAnswer::MakePrivate(name));
        ctx.set_screen.call(s);
    }
    if next == WizStep::Install {
        if ctx.install != InstallPhase::Idle {
            return;
        }
        start_install(ctx);
    }
    ctx.set_step.call(Nav {
        step: next,
        forward: true,
    });
}

fn back(ctx: &Ctx, cur: WizStep) {
    let steps = run_steps(ctx);
    if let Some(i) = steps.iter().position(|s| *s == cur)
        && i > 0
    {
        ctx.set_step.call(Nav {
            step: steps[i - 1],
            forward: false,
        });
    }
}

fn start_install(ctx: &Ctx) {
    let preset = ctx.preset.clone();
    let screen = ctx.screen.clone();
    let uninstall = ctx.uninstall;
    let seams = ctx.seams.clone();
    let set_install = ctx.set_install.clone();
    let set_step = ctx.set_step.clone();
    let set_log = ctx.set_log.clone();
    set_install.call(InstallPhase::Running);
    std::thread::spawn(move || {
        let choices = screen.effective_choices();
        let built = plan::build(&screen.facts, &choices, preset.artifact, uninstall);
        let ui = ChannelReporter::new(set_log);
        let live = match &seams {
            Seams::Demo { latency_ms } => LiveSeams::Demo(DemoSeams::new(&preset, *latency_ms)),
            Seams::Real { root, version } => {
                LiveSeams::Real(RealSeams::new(root.as_deref(), version))
            }
        };
        let exec = live.executor(&ui, false, false, choices.web_password.clone());
        match exec.execute(&built) {
            Ok(()) => {
                set_install.call(InstallPhase::Finished);
                set_step.call(Nav {
                    step: WizStep::Done,
                    forward: true,
                });
            }
            Err(failed) => {
                ui.die(&failed.0);
                set_install.call(InstallPhase::Failed);
            }
        }
    });
}

// --- rendering ---------------------------------------------------------------------------

fn edges(left: f64, top: f64, right: f64, bottom: f64) -> Thickness {
    Thickness {
        left,
        top,
        right,
        bottom,
    }
}

/// A rounded, bordered surface in the theme's card colours — the client shell's card.
fn card(child: impl Into<Element>) -> Border {
    border(child.into())
        .background(ThemeRef::CardBackground)
        .border_brush(ThemeRef::CardStroke)
        .border_thickness(Thickness::uniform(1.0))
        .corner_radius(10.0)
        .padding(edges(18.0, 12.0, 18.0, 12.0))
}

/// The D9 stepper: dots joined by a line, filled through the current step in brand violet,
/// hollow ahead. Each step owns one equal column: a half-segment either side of its dot,
/// stopping at the dot's edge, so the line meets every dot and the first/last halves are
/// simply invisible. `steps` is this run's real path — no ghost Network dot.
fn stepper(steps: &[WizStep], pos: usize, uninstall: bool) -> Element {
    let n = steps.len();
    let half = |lit: bool, hidden: bool| {
        Shape::rectangle()
            .fill(if lit { brand::VIOLET } else { MUTED })
            .height(2.0)
            .opacity(if hidden { 0.0 } else { 1.0 })
            .vertical_alignment(VerticalAlignment::Center)
    };
    let cells: Vec<Element> = steps
        .iter()
        .enumerate()
        .map(|(i, step)| {
            let dot = if i <= pos {
                Shape::ellipse().fill(brand::VIOLET)
            } else {
                Shape::ellipse().stroke(MUTED).stroke_thickness(1.5)
            };
            let line = grid((
                half(i > 0 && i <= pos, i == 0).grid_column(0),
                dot.width(DOT).height(DOT).grid_column(1),
                half(i < pos, i + 1 == n).grid_column(2),
            ))
            .columns([
                GridLength::Star(1.0),
                GridLength::Auto,
                GridLength::Star(1.0),
            ]);
            let label = text_block(step_title(*step, uninstall)).font_size(11.0);
            let label = if i == pos {
                label.semibold()
            } else {
                label.foreground(ThemeRef::SecondaryText)
            };
            vstack((
                line,
                label.horizontal_alignment(HorizontalAlignment::Center),
            ))
            .spacing(4.0)
            .grid_column(i as i32)
            .into()
        })
        .collect();
    grid(cells)
        .columns(std::iter::repeat_n(GridLength::Star(1.0), n))
        .into()
}

/// The incoming page's offset at `progress`: forward starts to the right and travels left,
/// back the mirror. Opposite margins keep the width constant, so nothing reflows mid-tween.
pub fn slide_margin(forward: bool, progress: f64) -> Thickness {
    let off = (1.0 - progress) * SLIDE_TRAVEL;
    let (left, right) = if forward { (off, -off) } else { (-off, off) };
    edges(left, 0.0, right, 0.0)
}

/// Stepper · (title · content · button bar). The stepper stays put; the rest is the page
/// that slides in.
fn frame(ctx: &Ctx, step: WizStep, content: Element, buttons: Vec<Element>) -> Element {
    let steps = run_steps(ctx);
    let pos = steps.iter().position(|s| *s == step).unwrap_or(0);
    // One header row: the step title left, the stepper right and narrow.
    let head = grid((
        text_block(step_title(step, ctx.uninstall))
            .font_size(26.0)
            .semibold()
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(0),
        stepper(&steps, pos, ctx.uninstall)
            .width(steps.len() as f64 * STEP_W)
            .horizontal_alignment(HorizontalAlignment::Right)
            .vertical_alignment(VerticalAlignment::Center)
            .grid_column(1),
    ))
    .columns([GridLength::Star(1.0), GridLength::Auto])
    .margin(edges(0.0, 0.0, 0.0, 20.0));
    let bar = border(
        hstack(buttons)
            .spacing(8.0)
            .horizontal_alignment(HorizontalAlignment::Right),
    )
    .border_brush(ThemeRef::DividerStroke)
    .border_thickness(edges(0.0, 1.0, 0.0, 0.0))
    .padding(edges(0.0, 14.0, 0.0, 0.0))
    .margin(edges(0.0, 16.0, 0.0, 0.0));
    let page = grid((content.grid_row(0), bar.grid_row(1)))
        .rows([GridLength::Star(1.0), GridLength::Auto]);
    let slide = ctx.slide;
    let page = border(page)
        .opacity(slide.progress)
        .margin(slide_margin(slide.forward, slide.progress));
    grid((head.grid_row(0), page.grid_row(1)))
        .rows([GridLength::Auto, GridLength::Star(1.0)])
        .margin(edges(36.0, 26.0, 36.0, 24.0))
        .into()
}

fn continue_button(ctx: &Ctx, cur: WizStep, label: &str) -> Element {
    let ctx = ctx.clone();
    button(label)
        .accent()
        .min_width(124.0)
        .on_click(move || advance(&ctx, cur))
        .into()
}

fn back_button(ctx: &Ctx, cur: WizStep) -> Element {
    let ctx = ctx.clone();
    button("Back")
        .min_width(92.0)
        .on_click(move || back(&ctx, cur))
        .into()
}

const WINDOW_W: f64 = 980.0;
const WINDOW_H: f64 = 700.0;

/// The real window over a preset (canned or probed) and the seams its install runs on.
pub fn run(preset: WinPreset, seams: Seams) -> windows_reactor::Result<()> {
    brand::install();
    let root = WizardRoot::new(preset, seams);
    // Self-contained: the runtime DLLs sit beside the exe (build.rs), so there is
    // deliberately NO windows_reactor::bootstrap() call — that is the framework path (S1).
    // One size: every page is laid out for 980×700 and the presenter exposes no
    // non-resizable switch, so min = max pins the frame (the maximize box is then inert).
    let mut app = App::new()
        .title("Punktfunk Setup")
        .inner_size(WINDOW_W, WINDOW_H)
        .inner_constraints(InnerConstraints {
            min_width: Some(WINDOW_W),
            min_height: Some(WINDOW_H),
            max_width: Some(WINDOW_W),
            max_height: Some(WINDOW_H),
        })
        .backdrop(Backdrop::Mica);
    // WinUI ignores the exe's embedded icon for the window; the title bar and taskbar
    // take it from a file.
    if let Some(icon) = brand::path("punktfunk.ico") {
        app = app.icon(icon.display().to_string());
    }
    app.render(move |cx| root.render(&(), cx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn continue_enters_from_the_right_and_back_from_the_left() {
        let fwd = slide_margin(true, 0.0);
        assert!(fwd.left > 0.0 && fwd.right == -fwd.left);
        let back = slide_margin(false, 0.0);
        assert!(back.left < 0.0 && back.right == -back.left);
        let settled = slide_margin(true, 1.0);
        assert_eq!((settled.left, settled.right), (0.0, 0.0));
    }
}
