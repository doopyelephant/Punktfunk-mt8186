// visionOS: the controls hung above a stream's picture. They show for a session's first seconds,
// then fade out until the player looks at them.

#if os(visionOS)
import PunktfunkKit
import SwiftUI

/// The stream's controls, above the picture. A look at any of its buttons is the reveal: a pinch
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
            ScreenMenu()
            NewWindowButton()
        }
        .labelStyle(.iconOnly)
        .padding(12)
        .glassBackgroundEffect()
        .contentShape(.hoverEffect, .rect(cornerRadius: 24))
        .hoverEffect { effect, active, _ in
            effect.animation(.smooth(duration: 0.25)) { $0.opacity(active || !resting ? 1 : 0) }
        }
        // A look at a button is a look at the pill: the buttons' own hover effects join this one.
        .hoverEffectGroup()
        .task {
            try? await Task.sleep(for: .seconds(6))
            resting = true
        }
    }
}

/// Flat or curved: the stored Screen value, one with the Settings row.
private struct ScreenMenu: View {
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen

    var body: some View {
        Menu {
            Picker("Screen", selection: $screen) {
                ForEach(StreamScreen.allCases, id: \.self) { Text($0.label).tag($0.rawValue) }
            }
            .pickerStyle(.inline)
        } label: {
            Label("Screen", systemImage: "rectangle.on.rectangle")
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
