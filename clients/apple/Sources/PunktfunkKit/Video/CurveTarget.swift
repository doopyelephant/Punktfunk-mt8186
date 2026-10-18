// The curved screen's picture: a RealityKit texture the Metal presenter draws into.

#if os(visionOS)
import CoreGraphics
import Metal
import Observation
import RealityKit
import UIKit

/// The curved screen's picture. The Metal presenter draws each frame into this queue's next
/// drawable in place of its layer's, and RealityKit shows the newest at the panel's rate. The
/// queue follows the frame's size: a new size rebuilds it on the main actor, and frames wait.
///
/// The drawables are sRGB textures, drawn through a plain view: the shader writes encoded
/// BT.709, and RealityKit decodes it on sampling.
@Observable
public final class CurveTarget: @unchecked Sendable {
    /// Main actor: what the screen's material shows; nil until the first frame's size lands.
    public private(set) var texture: TextureResource?
    /// Main actor: the picture's width over its height, once a frame has landed.
    public private(set) var aspect: Double?

    /// Main actor: the session's stream view; a pinch on the curve goes there.
    @ObservationIgnored public weak var pointer: (any CurvePointerSink)?
    /// Main actor: the volume's window. Keyed, it keeps this session's keyboard and mouse.
    @ObservationIgnored public weak var window: UIWindow?

    @ObservationIgnored private let lock = NSLock()
    @ObservationIgnored private var queue: TextureResource.DrawableQueue?
    @ObservationIgnored private var building: CGSize?

    public init() {}

    struct Frame {
        let drawable: TextureResource.Drawable
        /// The drawable's texture as the presenter's pipelines write it.
        let view: MTLTexture
    }

    /// Render thread: the next drawable at `size`, or nil while the queue is rebuilt to it.
    func next(size: CGSize) -> Frame? {
        let width = Int(size.width), height = Int(size.height)
        guard width > 0, height > 0 else { return nil }
        lock.lock()
        let queue = self.queue
        let fits = queue?.width == width && queue?.height == height
        let build = !fits && building != size
        if build { building = size }
        lock.unlock()
        if build { Task { @MainActor in self.build(width: width, height: height) } }
        guard fits, let queue, let drawable = try? queue.nextDrawable(),
              let view = drawable.texture.makeTextureView(pixelFormat: .bgra8Unorm)
        else { return nil }
        return Frame(drawable: drawable, view: view)
    }

    @MainActor private func build(width: Int, height: Int) {
        let descriptor = TextureResource.DrawableQueue.Descriptor(
            pixelFormat: .bgra8Unorm_srgb, width: width, height: height,
            usage: [.renderTarget, .shaderRead, .pixelFormatView], mipmapsMode: .none)
        guard let queue = try? TextureResource.DrawableQueue(descriptor) else { return }
        queue.allowsNextDrawableTimeout = true
        if texture == nil { texture = Self.blank() }
        texture?.replace(withDrawables: queue)
        lock.lock()
        self.queue = queue
        building = nil
        lock.unlock()
        aspect = Double(width) / Double(height)
    }

    /// One black pixel: a texture to hang the first queue on.
    @MainActor private static func blank() -> TextureResource? {
        guard let context = CGContext(
            data: nil, width: 1, height: 1, bitsPerComponent: 8, bytesPerRow: 4,
            space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.noneSkipLast.rawValue),
            let image = context.makeImage()
        else { return nil }
        return try? TextureResource(image: image, options: .init(semantic: .color))
    }
}

/// What a pinch on the curved screen is doing.
public enum CurvePointerPhase: Sendable {
    case began, moved, ended
}

/// The session's stream view, taking the curve's pinches as its own.
@MainActor
public protocol CurvePointerSink: AnyObject {
    /// A pinch at picture coordinates `u`, `v`: 0…1 from the top left.
    func curvePointer(u: Double, v: Double, phase: CurvePointerPhase)
}
#endif
