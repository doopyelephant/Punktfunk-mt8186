// visionOS: the controls hung below a stream window.

#if os(visionOS)
import SwiftUI

/// The stream window's controls, below it.
struct StreamOrnament: View {
    let quickActions: () -> Void

    var body: some View {
        HStack(spacing: 12) {
            Button(action: quickActions) {
                Label("Quick Actions", systemImage: "ellipsis.circle")
            }
            NewWindowButton()
        }
        .labelStyle(.iconOnly)
        .padding(12)
        .glassBackgroundEffect()
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
