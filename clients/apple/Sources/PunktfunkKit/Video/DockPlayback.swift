// The picture a Vision Pro docks into its environment. Only an AVPlayer docks, so the dock plays
// a still placeholder clip through a compositor that answers every refresh with the session's
// newest decoded frame (DockFeed).

import AVFoundation
import CoreVideo

private let dockLog = ClientLog(category: "dock")

/// The newest decoded frame, for the dock's player to pull at its own cadence. While a session
/// holds one it presents on the decoded path and hands every frame here too.
public final class DockFeed: @unchecked Sendable {
    private let lock = NSLock()
    private var frame: CVPixelBuffer?

    public init() {}

    /// The newest frame, again until a newer one lands; nil before the first.
    public func newest() -> CVPixelBuffer? {
        lock.lock()
        defer { lock.unlock() }
        return frame
    }

    func put(_ pixelBuffer: CVPixelBuffer) {
        lock.lock()
        frame = pixelBuffer
        lock.unlock()
    }

    /// Whether `connection`'s frames can reach the feed: the decoded path takes VideoToolbox's
    /// biplanar 4:2:0 output, so PyroWave and 4:4:4 stay on Metal.
    public static func supports(_ connection: PunktfunkConnection) -> Bool {
        connection.videoCodec != .pyrowave && !connection.isChroma444
    }
}

/// An AVPlayer whose picture is the stream: a still placeholder clip, recomposed at every
/// refresh from the feed's newest frame.
public enum DockPlayback {
    /// How long a dock can stay up before the placeholder ends.
    private static let length = CMTime(value: 12 * 3600, timescale: 1)

    /// A player for `feed` at the stream's size and rate, not yet playing.
    @available(macOS 26.0, iOS 26.0, tvOS 26.0, visionOS 26.0, *)
    public static func player(
        feed: DockFeed, width: Int, height: Int, hz: Int
    ) async throws -> AVPlayer {
        let asset = AVURLAsset(url: try await placeholder())
        guard let track = try await asset.loadTracks(withMediaType: .video).first else {
            throw CocoaError(.fileReadCorruptFile)
        }
        let instruction = DockInstruction(
            feed: feed, track: track.trackID,
            timeRange: CMTimeRange(start: .zero, duration: try await asset.load(.duration)))
        let item = AVPlayerItem(asset: asset)
        item.videoComposition = AVVideoComposition(configuration: .init(
            customVideoCompositorClass: DockCompositor.self,
            frameDuration: CMTime(value: 1, timescale: CMTimeScale(max(hz, 30))),
            instructions: [instruction],
            renderSize: width > 0 ? CGSize(width: width, height: height)
                : CGSize(width: 1920, height: 1080)))
        let player = AVPlayer(playerItem: item)
        instruction.clock.timebase = item.timebase
        return player
    }

    /// Black for `length`, written to the temporary directory once: a frame at each end, since a
    /// session's end time does not stretch its last sample.
    private static func placeholder() async throws -> URL {
        let files = FileManager.default
        let url = files.temporaryDirectory.appendingPathComponent("dock-placeholder.mov")
        if files.fileExists(atPath: url.path) { return url }
        let partial = files.temporaryDirectory.appendingPathComponent(
            "dock-\(UUID().uuidString).mov")
        let writer = try AVAssetWriter(outputURL: partial, fileType: .mov)
        let input = AVAssetWriterInput(mediaType: .video, outputSettings: [
            AVVideoCodecKey: AVVideoCodecType.h264, AVVideoWidthKey: 160, AVVideoHeightKey: 90,
        ])
        let adaptor = AVAssetWriterInputPixelBufferAdaptor(
            assetWriterInput: input, sourcePixelBufferAttributes: nil)
        writer.add(input)
        var black: CVPixelBuffer?
        CVPixelBufferCreate(nil, 160, 90, kCVPixelFormatType_32BGRA, nil, &black)
        guard let black, writer.startWriting() else {
            throw writer.error ?? CocoaError(.fileWriteUnknown)
        }
        CVPixelBufferLockBaseAddress(black, [])
        memset(CVPixelBufferGetBaseAddress(black), 0, CVPixelBufferGetDataSize(black))
        CVPixelBufferUnlockBaseAddress(black, [])
        writer.startSession(atSourceTime: .zero)
        adaptor.append(black, withPresentationTime: .zero)
        adaptor.append(black, withPresentationTime: length - CMTime(value: 1, timescale: 1))
        input.markAsFinished()
        writer.endSession(atSourceTime: length)
        await writer.finishWriting()
        guard writer.status == .completed else {
            throw writer.error ?? CocoaError(.fileWriteUnknown)
        }
        if !files.fileExists(atPath: url.path) { try files.moveItem(at: partial, to: url) }
        return url
    }
}

/// The placeholder's one instruction, carrying the feed and the player's clock to the compositor.
final class DockInstruction: NSObject, AVVideoCompositionInstructionProtocol {
    let feed: DockFeed
    let track: CMPersistentTrackID
    let clock = DockClock()
    let timeRange: CMTimeRange
    let enablePostProcessing = false
    /// Every refresh can differ while the placeholder never does; without this the engine
    /// reuses one composed frame.
    let containsTweening = true
    let passthroughTrackID = kCMPersistentTrackID_Invalid
    var requiredSourceTrackIDs: [NSValue]? { [NSNumber(value: track)] }

    init(feed: DockFeed, track: CMPersistentTrackID, timeRange: CMTimeRange) {
        self.feed = feed
        self.track = track
        self.timeRange = timeRange
    }
}

/// Answers each refresh with the feed's newest frame, as is; the placeholder's black frame
/// stands in until the first one lands. The player asks 75–150 ms ahead, so each request waits
/// until `margin` before its display time and takes the frame newest then.
final class DockCompositor: NSObject, AVVideoCompositing {
    /// Answer this long before display. 15 ms ≈ 1.4 refreshes at 90 Hz.
    private static let margin = 0.015
    private static let formats: [String: any Sendable] = [
        kCVPixelBufferPixelFormatTypeKey as String: [
            kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr8BiPlanarFullRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarVideoRange,
            kCVPixelFormatType_420YpCbCr10BiPlanarFullRange,
        ],
    ]
    let sourcePixelBufferAttributes: [String: any Sendable]? = formats
    let requiredPixelBufferAttributesForRenderContext: [String: any Sendable] = formats
    private let queue = DispatchQueue(label: "io.unom.punktfunk.dock", qos: .userInteractive)
    /// Requests waiting for their answer time. A cancel finishes them all at once.
    private let lock = NSLock()
    private var held: [Int: AVAsynchronousVideoCompositionRequest] = [:]
    private var nextID = 0

    func renderContextChanged(_ newRenderContext: AVVideoCompositionRenderContext) {}

    func startRequest(_ request: AVAsynchronousVideoCompositionRequest) {
        guard let instruction = request.videoCompositionInstruction as? DockInstruction else {
            request.finishCancelledRequest()
            return
        }
        lock.lock()
        nextID += 1
        let id = nextID
        held[id] = request
        lock.unlock()
        let wait = instruction.clock.secondsUntil(request.compositionTime) - Self.margin
        queue.asyncAfter(deadline: .now() + max(wait, 0)) { [self] in
            lock.lock()
            let request = held.removeValue(forKey: id)
            lock.unlock()
            guard let request else { return } // cancelled meanwhile
            guard let frame = instruction.feed.newest()
                ?? request.sourceFrame(byTrackID: instruction.track)
            else { return request.finishCancelledRequest() }
            instruction.clock.noteAnswered(request.compositionTime)
            request.finish(withComposedVideoFrame: frame)
        }
    }

    func cancelAllPendingVideoCompositionRequests() {
        lock.lock()
        let pending = held.values
        held.removeAll()
        lock.unlock()
        pending.forEach { $0.finishCancelledRequest() }
    }
}

/// The player item's clock, read from the compositor's queue. Logs how far ahead of display
/// requests are answered: the latency the dock adds to the stream.
final class DockClock: @unchecked Sendable {
    /// Answers per log line: 10 s at 90 Hz.
    private static let window = 900
    var timebase: CMTimebase?
    private var leads: [Double] = []

    func secondsUntil(_ time: CMTime) -> Double {
        guard let timebase else { return 0 }
        return (time - CMTimebaseGetTime(timebase)).seconds
    }

    /// Compositor queue only.
    func noteAnswered(_ time: CMTime) {
        leads.append(secondsUntil(time) * 1000)
        guard leads.count >= Self.window else { return }
        let sorted = leads.sorted()
        leads.removeAll(keepingCapacity: true)
        dockLog.info(
            "dock answer lead ms min \(sorted[0], format: .fixed(precision: 1)) p50 \(sorted[Self.window / 2], format: .fixed(precision: 1)) max \(sorted[Self.window - 1], format: .fixed(precision: 1))")
    }
}
