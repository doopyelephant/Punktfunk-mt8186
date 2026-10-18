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
/// the curve, and closing it brings the picture back to the session's window.
struct CurveScene: View {
    static let sceneID = "curve"
    let id: UUID?
    @Environment(\.dismissWindow) private var dismissWindow

    var body: some View {
        if let id, let session = CurveStage.shared.session(id), let target = session.target {
            CurveScreen(target: target)
                .ornament(attachmentAnchor: .scene(.bottom), contentAlignment: .top) {
                    StreamOrnament(
                        connection: session.connection,
                        quickActions: { session.ring.toggleCentred() })
                }
                .onDisappear { CurveStage.shared.leave(session.connection) }
        } else {
            Color.clear.onAppear { dismissWindow() }
        }
    }
}

/// The picture on a cylinder segment as wide as the volume, bowed away from the viewer.
private struct CurveScreen: View {
    let target: CurveTarget
    @Environment(\.physicalMetrics) private var metrics
    /// The shape the screen's mesh was last built for.
    @State private var built = Built()

    final class Built {
        var shape: CurveShape?
    }

    var body: some View {
        GeometryReader3D { proxy in
            let box = metrics.convert(proxy.size, to: .meters)
            let texture = target.texture, aspect = target.aspect
            RealityView { content in
                content.add(ModelEntity())
            } update: { content in
                guard let screen = content.entities.first as? ModelEntity,
                      let texture, let aspect
                else { return }
                let shape = CurveShape(
                    box: SIMD3(Float(box.width), Float(box.height), Float(box.depth)),
                    aspect: Float(aspect))
                guard shape != built.shape, let mesh = try? shape.mesh() else { return }
                var material = UnlitMaterial(applyPostProcessToneMap: false)
                material.color = .init(tint: .white, texture: .init(texture))
                screen.model = ModelComponent(mesh: mesh, materials: [material])
                screen.position = [0, 0, shape.front]
                built.shape = shape
            }
        }
        .volumeBaseplateVisibility(.hidden)
        .supportedVolumeViewpoints(.front)
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
