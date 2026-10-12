import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

/// Deadline pacing: pick at Apple's deadline less the render margin, never present past it, and
/// drain a present that lands a refresh late anyway.
final class DeadlinePacerTests: XCTestCase {
    private let targetNs: Int64 = 1_000_000_000

    private func pair(now: Double, latePick: Bool = true, drain: Bool = false)
        -> DeadlinePacer.Pair
    {
        DeadlinePacer.pair(now: now, deadline: 1.010, margin: 0.001, latePick: latePick, drain: drain)
    }

    private func pickAt(_ p: DeadlinePacer.Pair) -> Double {
        if case .present(let at) = p { return at }
        return -1
    }

    func testTheLatencyIntentPicksAtTheDeadlineLessTheMargin() {
        XCTAssertEqual(pickAt(pair(now: 1.000)), 1.009, accuracy: 1e-9)
        XCTAssertEqual(pickAt(pair(now: 1.0089)), 1.009, accuracy: 1e-9, "paired just before")
    }

    func testSmoothnessPresentsOnPairing() {
        XCTAssertEqual(pickAt(pair(now: 1.000, latePick: false)), 1.000, accuracy: 1e-9)
    }

    func testADrawablePairedPastThePickIsSkipped() {
        XCTAssertEqual(pair(now: 1.0095), .skip, "would land a refresh late")
        XCTAssertEqual(pair(now: 1.0095, latePick: false), .skip)
    }

    func testAnArmedDrainSkipsEvenAnEarlyPairing() {
        XCTAssertEqual(pair(now: 1.000, drain: true), .skip)
    }

    func testTheMarginFollowsTheSlowestRecentRenderWithinItsBounds() {
        let pacer = DeadlinePacer()
        XCTAssertEqual(pacer.margin, 0.001, accuracy: 1e-9, "floor before any render")
        pacer.noteRender(seconds: 0.0012)
        XCTAssertEqual(pacer.margin, 0.0017, accuracy: 1e-9)
        pacer.noteRender(seconds: 0.010)
        XCTAssertEqual(pacer.margin, 0.004, accuracy: 1e-9, "capped")
        for _ in 0..<32 { pacer.noteRender(seconds: 0.0002) }
        XCTAssertEqual(pacer.margin, 0.001, accuracy: 1e-9, "the slow render aged out")
    }

    private func present(_ pacer: DeadlinePacer, issuedNs: Int64, lateMs: Double?) {
        pacer.observe(
            issuedNs: issuedNs, presentedNs: lateMs.map { targetNs + Int64($0 * 1e6) },
            targetNs: targetNs)
    }

    func testALatePresentArmsOneDrain() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: 100, lateMs: 0)
        XCTAssertFalse(pacer.takeDrain(nowNs: 200), "on time arms nothing")
        present(pacer, issuedNs: 100, lateMs: 8.3)
        XCTAssertTrue(pacer.takeDrain(nowNs: 200))
        XCTAssertFalse(pacer.takeDrain(nowNs: 300), "one skip per arming")
    }

    func testPresentsQueuedBeforeTheDrainArmNothing() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: 100, lateMs: 8.3)
        XCTAssertTrue(pacer.takeDrain(nowNs: 500))
        present(pacer, issuedNs: 400, lateMs: 8.3) // already queued when the drain skipped
        XCTAssertFalse(pacer.takeDrain(nowNs: 600))
        present(pacer, issuedNs: 700, lateMs: 8.3) // a new late present
        XCTAssertTrue(pacer.takeDrain(nowNs: 800))
    }

    func testADroppedPresentArmsNoDrain() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: 100, lateMs: nil)
        XCTAssertFalse(pacer.takeDrain(nowNs: 200), "a drop leaves no queue behind")
    }
}
#endif
