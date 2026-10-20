// visionOS: the controls hung below a stream window. They show for a session's first seconds,
// then rest as a faint pill until the player looks at it.

#if os(visionOS)
import PunktfunkKit
import SwiftUI

/// The stream window's controls, below it. A look at any of its buttons is the reveal: a pinch
/// on the picture is the host's click. The system draws the hover state, so the rest state costs
/// no input.
struct StreamOrnament: View {
    /// The session the Screen menu moves; nil draws the ornament without it.
    var connection: PunktfunkConnection?
    let quickActions: () -> Void
    @State private var resting = false

    var body: some View {
        HStack(spacing: 12) {
            Button(action: quickActions) {
                Label("Quick Actions", systemImage: "ellipsis.circle")
            }
            if let connection { ScreenMenu(connection: connection) }
            NewWindowButton()
        }
        .labelStyle(.iconOnly)
        .padding(12)
        .glassBackgroundEffect()
        .contentShape(.hoverEffect, .rect(cornerRadius: 24))
        .hoverEffect { effect, active, _ in
            effect.animation(.smooth(duration: 0.25)) { $0.opacity(active || !resting ? 1 : 0.2) }
        }
        // A look at a button is a look at the pill: the buttons' own hover effects join this one.
        .hoverEffectGroup()
        .task {
            try? await Task.sleep(for: .seconds(6))
            resting = true
        }
    }
}

/// Opens another main window: its own host list and, once connected, its own stream.
struct NewWindowButton: View {
    @Environment(\.openWindow) private var openWindow

    var body: some View {
        Button { openWindow(id: PunktfunkClientApp.mainSceneID) } label: {
            Label("New Window", systemImage: "macwindow.badge.plus")
        }
    }
}
#endif
