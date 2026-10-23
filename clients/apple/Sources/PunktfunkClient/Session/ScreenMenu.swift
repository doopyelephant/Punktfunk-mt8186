// visionOS: where a session shows its picture — the window or the curved screen — and what its
// window shows while the picture is elsewhere.

#if os(visionOS)
import PunktfunkKit
import SwiftUI

/// Move `connection`'s picture to `screen`; nil brings it back to its window.
@MainActor
func showScreen(_ screen: StreamScreen?, for connection: PunktfunkConnection) {
    if screen == .curved {
        CurveStage.shared.enter(connection)
    } else {
        CurveStage.shared.leave(connection)
    }
}

/// Where `connection`'s picture shows now.
@MainActor
func shownScreen(for connection: PunktfunkConnection) -> StreamScreen {
    CurveStage.shared.target(for: connection) != nil ? .curved : .window
}

/// Where the picture shows. One stored value with the Settings row.
struct ScreenMenu: View {
    let connection: PunktfunkConnection
    @AppStorage(DefaultsKey.screen) private var screen = SettingDefault.screen

    var body: some View {
        Menu {
            Picker("Screen", selection: Binding(
                get: { shownScreen(for: connection) },
                set: { pick in
                    screen = pick.rawValue
                    showScreen(pick, for: connection)
                }
            )) {
                ForEach(StreamScreen.allCases, id: \.self) { Text($0.label).tag($0) }
            }
            .pickerStyle(.inline)
        } label: {
            Label("Screen", systemImage: "rectangle.on.rectangle")
        }
    }
}

/// What the stream's window shows while its picture is elsewhere: where it went, and the way
/// back.
struct ScreenPlaceholder: View {
    let title: String
    let systemImage: String
    let connection: PunktfunkConnection
    let endStream: () -> Void

    var body: some View {
        VStack(spacing: 20) {
            Image(systemName: systemImage)
                .font(.system(size: 44))
            Text(title)
                .font(.title2)
            HStack(spacing: 12) {
                ScreenMenu(connection: connection)
                    .labelStyle(.titleAndIcon)
                Button("End Stream", systemImage: "xmark", role: .destructive, action: endStream)
            }
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
        .background(Color.black)
    }
}
#endif
