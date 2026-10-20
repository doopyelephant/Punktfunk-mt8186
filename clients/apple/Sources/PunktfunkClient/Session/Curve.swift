// visionOS: the stream on a curved screen in a volume. The session stays in its window's model;
// the volume shows the picture the presenter draws into a CurveTarget.

#if os(visionOS)
import PunktfunkKit
import RealityKit
import SwiftUI

/// The sessions a curved screen can show, by connection, and the volume each one names.
@MainActor @Observable
final class CurveStage {
    static let shared = CurveStage()

    /// A streaming session; `target` is set while its picture is on the curve.
    @MainActor @Observable
    final class Session {
        let id = UUID()
        let connection: PunktfunkConnection
        @ObservationIgnored weak var model: SessionModel?
        @ObservationIgnored let ring: RingState
        var target: CurveTarget?

        init(connection: PunktfunkConnection, model: SessionModel, ring: RingState) {
            self.connection = connection
            self.model = model
            self.ring = ring
        }
    }

    private(set) var sessions: [ObjectIdentifier: Session] = [:]

    /// `connection` streams in a window that can send its picture to a curve.
    func register(_ connection: PunktfunkConnection, model: SessionModel, ring: RingState) {
        let key = ObjectIdentifier(connection)
        guard sessions[key] == nil else { return }
        sessions[key] = Session(connection: connection, model: model, ring: ring)
    }

    func session(_ id: UUID) -> Session? {
        sessions.values.first { $0.id == id }
    }

    func session(for connection: PunktfunkConnection) -> Session? {
        sessions[ObjectIdentifier(connection)]
    }

    func target(for connection: PunktfunkConnection) -> CurveTarget? {
        session(for: connection)?.target
    }

    func enter(_ connection: PunktfunkConnection) {
        guard let session = session(for: connection), session.target == nil else { return }
        session.target = CurveTarget()
    }

    func leave(_ connection: PunktfunkConnection) {
        session(for: connection)?.target = nil
    }

    func ended(_ connection: PunktfunkConnection) {
        sessions[ObjectIdentifier(connection)] = nil
    }
}

/// The volume a curved session shows its picture in. It closes itself once the session leaves
/// the curve, and closing it brings the picture back to the session's window. With the session's
/// window gone too, a Home window opens in its place.
struct CurveScene: View {
    static let sceneID = "curve"
    let id: UUID?
    @Environment(\.openWindow) private var openWindow
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        if let id, let session = CurveStage.shared.session(id), let target = session.target,
           let model = session.model {
            CurveScreen(
                target: target, model: model, ring: session.ring, connection: session.connection)
                .ornament(attachmentAnchor: .scene(.bottom), contentAlignment: .top) {
                    StreamOrnament(
                        connection: session.connection,
                        quickActions: { session.ring.toggleCentred() })
                }
                .onDisappear { CurveStage.shared.leave(session.connection) }
        } else {
            Color.clear.task {
                // The app's last window cannot close itself: the Home must be up before this
                // goes. A window closing right now can still read as on screen for a moment.
                try? await Task.sleep(for: .milliseconds(500))
                if !Self.anotherWindowShows {
                    openWindow(id: PunktfunkClientApp.mainSceneID)
                    for _ in 0..<30 where !Self.anotherWindowShows {
                        try? await Task.sleep(for: .milliseconds(100))
                    }
                }
                dismissWindow()
            }
        }
    }

    /// Another of the app's windows is on screen besides this volume.
    private static var anotherWindowShows: Bool {
        UIApplication.shared.connectedScenes.filter {
            $0 is UIWindowScene
                && ($0.activationState == .foregroundActive
                    || $0.activationState == .foregroundInactive)
        }.count > 1
    }
}

/// What the window draws over the picture, re-homed on the curve as attachments.
fileprivate enum CurveOverlay: String, CaseIterable {
    case hud, badges, ring, resize
}

/// The picture on a cylinder segment as wide as the volume, bowed away from the viewer. A pinch
/// on it reaches the host through the session's stream view, or closes the ring while it is
/// open; a two-hand twist turns the ring, and the window's overlays lie along it, just in front.
private struct CurveScreen: View {
    let target: CurveTarget
    @ObservedObject var model: SessionModel
    @ObservedObject var ring: RingState
    let connection: PunktfunkConnection
    @AppStorage(DefaultsKey.hudPlacement) private var hudPlacement = HUDPlacement.topTrailing.rawValue
    @AppStorage(DefaultsKey.statsScalePct) private var statsScalePct = 100
    @Environment(\.physicalMetrics) private var metrics
    /// The shape the screen's mesh was last built for, and the pinch in progress.
    @State private var state = CurveState()
    /// Each overlay's laid-out size in points, for placing it on the curve.
    @State private var sizes: [CurveOverlay: CGSize] = [:]

    final class CurveState {
        var shape: CurveShape?
        var pinching = false
        /// This pinch closed the ring and goes no further.
        var swallowing = false
    }

    /// The ring's frame on the curve: its scrim and the wheel inside it.
    private static let ringSide: CGFloat = 720
    /// The window picture width, in points, whose overlay proportions the curve keeps.
    private static let referenceWidth: CGFloat = 1280
    /// The twist that commits the ring, as on a touchscreen.
    private static let commitDegrees: Double = 30

    var body: some View {
        GeometryReader3D { proxy in
            let box = metrics.convert(proxy.size, to: .meters)
            let texture = target.texture, aspect = target.aspect
            let placement = HUDPlacement(rawValue: hudPlacement) ?? .topTrailing
            let extents = sizes.mapValues { metrics.convert($0, to: .meters) }
            let pointWidth = Float(metrics.convert(Self.referenceWidth, to: .meters))
            RealityView { content, attachments in
                let screen = ModelEntity()
                screen.components.set(InputTargetComponent())
                content.add(screen)
                for overlay in CurveOverlay.allCases {
                    if let entity = attachments.entity(for: overlay) { screen.addChild(entity) }
                }
            } update: { content, attachments in
                guard let screen = content.entities.first as? ModelEntity, let texture, let aspect
                else { return }
                let shape = CurveShape(
                    box: SIMD3(Float(box.width), Float(box.height), Float(box.depth)),
                    aspect: Float(aspect))
                if shape != state.shape, let mesh = try? shape.mesh() {
                    var material = UnlitMaterial(applyPostProcessToneMap: false)
                    material.color = .init(tint: .white, texture: .init(texture))
                    screen.model = ModelComponent(mesh: mesh, materials: [material])
                    screen.position = [0, 0, shape.front]
                    state.shape = shape
                    Task { @MainActor in
                        guard let collision = try? await ShapeResource.generateStaticMesh(from: mesh),
                              state.shape == shape
                        else { return }
                        screen.components.set(CollisionComponent(shapes: [collision]))
                    }
                }
                // The overlays keep the size they have over a window picture this wide.
                let scale = 2 * shape.halfChord / pointWidth
                for overlay in CurveOverlay.allCases {
                    guard let entity = attachments.entity(for: overlay) else { continue }
                    let size = extents[overlay] ?? .zero
                    let pose = shape.pose(
                        of: overlay, size: SIMD2(Float(size.width), Float(size.height)) * scale,
                        placement: placement)
                    entity.scale = SIMD3(repeating: scale)
                    entity.position = pose.position
                    entity.orientation = pose.orientation
                }
            } attachments: {
                Attachment(id: CurveOverlay.hud) {
                    if model.phase == .streaming, model.statsVerbosity != .off {
                        StreamHUDView(
                            model: model, connection: connection, placement: placement,
                            verbosity: model.statsVerbosity,
                            scale: Double(min(max(statsScalePct, 75), 200)) / 100)
                            .measured { sizes[.hud] = $0 }
                    }
                }
                Attachment(id: CurveOverlay.badges) {
                    StreamBadgeStack(
                        model: model, captureEnabled: true, statsVerbosity: model.statsVerbosity)
                        .measured { sizes[.badges] = $0 }
                }
                Attachment(id: CurveOverlay.ring) {
                    if ring.visible {
                        RingOverlay(
                            state: ring,
                            cfg: OverlayConfig.parse(model.settings.overlayActions),
                            actions: model.ringActions(connection, ring: ring))
                            .frame(width: Self.ringSide, height: Self.ringSide)
                            .measured { sizes[.ring] = $0 }
                    }
                }
                Attachment(id: CurveOverlay.resize) {
                    if model.resizing {
                        ResizeIndicatorView(active: true)
                            .measured { sizes[.resize] = $0 }
                    }
                }
            }
            .environment(\.inAttachment, true)
            .gesture(pinch)
            .simultaneousGesture(twist)
        }
        .background(WindowProbe { target.window = $0 })
        .volumeBaseplateVisibility(.hidden)
        .supportedVolumeViewpoints(.front)
    }

    /// One pinch, held or tapped, on the screen: the session's touch model takes it from there.
    private var pinch: some Gesture {
        DragGesture(minimumDistance: 0)
            .targetedToAnyEntity()
            .onChanged { value in
                send(value, phase: state.pinching ? .moved : .began)
                state.pinching = true
            }
            .onEnded { value in
                send(value, phase: .ended)
                state.pinching = false
            }
    }

    /// A pinch on the picture goes to the host, unless the ring is open: then it closes the ring,
    /// as the window's scrim does, and none of it reaches the host.
    private func send(_ value: EntityTargetValue<DragGesture.Value>, phase: CurvePointerPhase) {
        guard let shape = state.shape, value.entity.components.has(ModelComponent.self) else { return }
        if phase == .began, ring.visible {
            if ring.sheet { ring.sheet = false } else { ring.close() }
            state.swallowing = true
        }
        if state.swallowing {
            if phase == .ended { state.swallowing = false }
            return
        }
        let local = value.convert(value.location3D, from: .local, to: value.entity)
        let uv = shape.uv(at: local)
        target.pointer?.curvePointer(u: Double(uv.x), v: Double(uv.y), phase: phase)
    }

    /// Two hands turning the screen: the ring's dial, opened at its own frame's centre.
    private var twist: some Gesture {
        RotateGesture3D(constrainedToAxis: .z)
            .targetedToAnyEntity()
            .onChanged { value in
                let degrees = value.rotation.angle.degrees
                ring.handle(.turn(
                    progress: min(abs(degrees) / Self.commitDegrees, 1),
                    clockwise: degrees < 0,
                    at: CGPoint(x: Self.ringSide / 2, y: Self.ringSide / 2)))
            }
            .onEnded { value in
                let committed = abs(value.rotation.angle.degrees) >= Self.commitDegrees
                ring.handle(committed ? .commit : .cancel)
            }
    }
}

private extension View {
    /// Report this view's laid-out size.
    func measured(_ report: @escaping (CGSize) -> Void) -> some View {
        onGeometryChange(for: CGSize.self, of: { $0.size }, action: report)
    }
}

/// Hands back the UIWindow the volume's content lands in.
private struct WindowProbe: UIViewRepresentable {
    let found: (UIWindow?) -> Void

    func makeUIView(context: Context) -> ProbeView {
        let view = ProbeView()
        view.found = found
        return view
    }

    func updateUIView(_ view: ProbeView, context: Context) {}

    final class ProbeView: UIView {
        var found: ((UIWindow?) -> Void)?

        override func didMoveToWindow() {
            super.didMoveToWindow()
            found?(window)
        }
    }
}

/// A cylinder segment that fits a volume: as wide as the box allows at the picture's aspect,
/// the chord on the box's front face and the arc bowed back inside it. 1.8 m is the radius of
/// a curved desk monitor's arc; a box too shallow for that sag flattens it instead.
struct CurveShape: Equatable {
    static let radius: Float = 1.8
    static let columns = 64
    /// Room left at the box's faces, metres.
    static let margin: Float = 0.01

    let radius: Float
    /// Half the arc's angle, radians.
    let halfAngle: Float
    let height: Float
    /// The chord's z inside the box: its front face.
    let front: Float

    init(box: SIMD3<Float>, aspect: Float) {
        let sag = max(box.z - 2 * Self.margin, 0.01)
        var chord = max(box.x - 2 * Self.margin, 0.1)
        var shape = Self.arc(chord: chord, sag: sag)
        // Too tall at full width: narrow the chord until the picture fits the box's height.
        let fit = (box.y - 2 * Self.margin) / (shape.length / aspect)
        if fit < 1 {
            chord *= fit
            shape = Self.arc(chord: chord, sag: sag)
        }
        radius = shape.radius
        halfAngle = shape.halfAngle
        height = shape.length / aspect
        front = box.z / 2 - Self.margin
    }

    var halfChord: Float { radius * sin(halfAngle) }

    /// The surface's z at `x` in the screen's own space: 0 at the edges, minus the sag at the
    /// centre.
    func surfaceZ(atX x: Float) -> Float {
        radius * cos(halfAngle) - sqrt(max(radius * radius - x * x, 0))
    }

    /// Picture coordinates, 0…1 from the top left, of a point on the screen in its own space.
    func uv(at point: SIMD3<Float>) -> SIMD2<Float> {
        let angle = atan2(point.x, radius * cos(halfAngle) - point.z)
        let u = (angle / halfAngle + 1) / 2
        let v = 0.5 - point.y / height
        return [min(max(u, 0), 1), min(max(v, 0), 1)]
    }

    /// Where an overlay `size` metres across floats, and how it turns: the HUD in its corner, the
    /// rest on the centre line. Each lies tangent to the surface at its centre, out by its own
    /// width's sag plus a lift, so no edge dips behind the picture; the ring floats further out.
    fileprivate func pose(
        of overlay: CurveOverlay, size: SIMD2<Float>, placement: HUDPlacement
    ) -> (position: SIMD3<Float>, orientation: simd_quatf) {
        let inset: Float = 0.02
        let lift: Float = overlay == .ring ? 0.02 : 0.005
        var x: Float = 0, y: Float = 0
        switch overlay {
        case .hud:
            let trailing = placement == .topTrailing || placement == .bottomTrailing
            let top = placement == .topLeading || placement == .topTrailing
            x = (trailing ? 1 : -1) * max(halfChord - size.x / 2 - inset, 0)
            y = (top ? 1 : -1) * max(height / 2 - size.y / 2 - inset, 0)
        case .badges:
            y = -max(height / 2 - size.y / 2 - inset, 0)
        case .ring, .resize:
            break
        }
        // The mesh's normal at angle a is (-sin a, 0, cos a); a turn of -a about y faces it.
        let angle = asin(min(max(x / radius, -1), 1))
        let normal = SIMD3<Float>(-sin(angle), 0, cos(angle))
        let out = size.x * size.x / (8 * radius) + lift
        return (
            SIMD3(x, y, surfaceZ(atX: x)) + normal * out,
            simd_quatf(angle: -angle, axis: [0, 1, 0]))
    }

    private static func arc(chord: Float, sag: Float) -> (radius: Float, halfAngle: Float, length: Float) {
        // The radius whose sagitta over this chord just fits the box, if 1.8 m does not.
        let radius = max(Self.radius, (chord * chord / 4 + sag * sag) / (2 * sag))
        let halfAngle = asin(min(chord / (2 * radius), 1))
        return (radius, halfAngle, 2 * halfAngle * radius)
    }

    /// Two rows of `columns + 1` vertices, u linear in angle and v in height (1 at the top).
    func mesh() throws -> MeshResource {
        var positions: [SIMD3<Float>] = []
        var normals: [SIMD3<Float>] = []
        var uvs: [SIMD2<Float>] = []
        let centreZ = radius * cos(halfAngle)
        for row in 0...1 {
            let y = (Float(row) - 0.5) * height
            for column in 0...Self.columns {
                let u = Float(column) / Float(Self.columns)
                let angle = (u * 2 - 1) * halfAngle
                positions.append([radius * sin(angle), y, centreZ - radius * cos(angle)])
                normals.append([-sin(angle), 0, cos(angle)])
                uvs.append([u, Float(row)])
            }
        }
        var indices: [UInt32] = []
        let stride = UInt32(Self.columns + 1)
        for column in 0..<UInt32(Self.columns) {
            let bottomLeft = column, bottomRight = column + 1
            let topLeft = column + stride, topRight = column + 1 + stride
            indices += [bottomLeft, bottomRight, topRight, bottomLeft, topRight, topLeft]
        }
        var descriptor = MeshDescriptor(name: "curve")
        descriptor.positions = MeshBuffers.Positions(positions)
        descriptor.normals = MeshBuffers.Normals(normals)
        descriptor.textureCoordinates = MeshBuffers.TextureCoordinates(uvs)
        descriptor.primitives = .triangles(indices)
        return try MeshResource.generate(from: [descriptor])
    }
}
#endif
