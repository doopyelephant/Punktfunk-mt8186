// visionOS: the controls hung above a stream's picture. They show for a session's first seconds,
// then fade out until the player looks at them.

#if os(visionOS)
import PunktfunkKit
import SwiftUI

/// The stream's controls, above the picture. A look at any of its controls is the reveal: a pinch
/// on the picture is the host's click. The system draws the hover state, so the rest state costs
/// no input.
struct StreamOrnament: View {
    let quickActions: () -> Void
    @State private var resting = false

    var body: some View {
        HStack(spacing: 12) {
            Button(action: quickActions) {
                Label("Quick Actions", systemImage: "ellipsis.circle")
            }
            CurveSlider()
            NewWindowButton()
        }
        .labelStyle(.iconOnly)
        .padding(12)
        .glassBackgroundEffect()
        .contentShape(.hoverEffect, .rect(cornerRadius: 24))
        .hoverEffect { effect, active, _ in
            effect.animation(.smooth(duration: 0.25)) { $0.opacity(active || !resting ? 1 : 0) }
        }
        // A look at a control is a look at the pill: their own hover effects join this one.
        .hoverEffectGroup()
        .task {
            try? await Task.sleep(for: .seconds(6))
            resting = true
        }
    }
}

/// Flat to 800R, stop by stop: the stored Screen value, one with the Settings row. The picture
/// bends as the slider moves.
private struct CurveSlider: View {
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen

    var body: some View {
        let stops = ScreenCurve.allCases
        let curve = ScreenCurve(name: screen)
        HStack(spacing: 8) {
            Slider(
                value: Binding(
                    get: { Double(stops.firstIndex(of: curve) ?? 0) },
                    set: { screen = stops[Int($0.rounded())].rawValue }),
                in: 0...Double(stops.count - 1), step: 1
            ) {
                Text("Screen")
            }
            .labelsHidden()
            .accessibilityValue(curve.label)
            .frame(width: 160)
            Text(curve.label)
                .monospacedDigit()
                .frame(minWidth: 52, alignment: .leading)
        }
    }
}

/// Opens another main volume: its own host list and, once connected, its own stream.
struct NewWindowButton: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button { openWindow(id: PunktfunkClientApp.mainSceneID) } label: {
            Label("New Window", systemImage: "macwindow.badge.plus")
        }
    }
}
#endif
