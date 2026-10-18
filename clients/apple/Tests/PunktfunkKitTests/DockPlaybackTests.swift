// The dock's player shows the feed's frame, not its placeholder, and keeps composing across a
// seek, which cancels the requests the compositor is holding.

import AVFoundation
import XCTest
@testable import PunktfunkKit

final class DockPlaybackTests: XCTestCase {
    func testThePlayerShowsTheFeedsFrame() async throws {
        guard #available(macOS 26.0, iOS 26.0, tvOS 26.0, visionOS 26.0, *) else {
            throw XCTSkip("the composition configuration API is 26+")
        }
        var frame: CVPixelBuffer?
        CVPixelBufferCreate(
            nil, 1280, 720, kCVPixelFormatType_420YpCbCr8BiPlanarVideoRange,
            [kCVPixelBufferIOSurfacePropertiesKey: [:]] as CFDictionary, &frame)
        let feed = DockFeed()
        feed.put(try XCTUnwrap(frame))
        let player = try await DockPlayback.player(feed: feed, width: 1280, height: 720, hz: 60)
        let item = try XCTUnwrap(player.currentItem)
        let output = AVPlayerItemVideoOutput(pixelBufferAttributes: nil)
        item.add(output)
        player.play()
        defer { player.pause() }

        func nextPicture() async throws -> CVPixelBuffer? {
            for _ in 0..<300 {
                try await Task.sleep(for: .milliseconds(10))
                XCTAssertNotEqual(item.status, .failed, String(describing: item.error))
                let now = item.currentTime()
                if output.hasNewPixelBuffer(forItemTime: now),
                   let picture = output.copyPixelBuffer(forItemTime: now, itemTimeForDisplay: nil) {
                    return picture
                }
            }
            return nil
        }

        let shown = try await nextPicture()
        let first = try XCTUnwrap(shown, "the player composes within three seconds")
        XCTAssertEqual(CVPixelBufferGetWidth(first), 1280)
        XCTAssertEqual(CVPixelBufferGetHeight(first), 720)
        _ = await item.seek(to: CMTime(value: 60, timescale: 1))
        let reshown = try await nextPicture()
        let after = try XCTUnwrap(reshown, "the player composes again after a seek")
        XCTAssertEqual(CVPixelBufferGetWidth(after), 1280)
    }
}
