/// Where a Vision Pro shows the picture: its window, a curved screen in a volume, or docked in
/// the system environment.
/// `DefaultsKey.screen`, console key `visionos.screen`.
public enum StreamScreen: String, CaseIterable, Sendable {
    case window, curved, environment

    /// Unknown names read as `.window`, so a newer client's value degrades safely.
    public init(name: String?) {
        self = name.flatMap(StreamScreen.init(rawValue:)) ?? .window
    }

    public var label: String {
        switch self {
        case .window: return "Window"
        case .curved: return "Curved"
        case .environment: return "Environment"
        }
    }
}
