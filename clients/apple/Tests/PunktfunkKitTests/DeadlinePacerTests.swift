import XCTest

#if canImport(Metal)
@testable import PunktfunkKit

/// Deadline pacing: pick inside the window before Apple's deadline less the measured margin,
/// never present past the cutoff, learn from late presents at the cutoff, drain a queue once.
final class DeadlinePacerTests: XCTestCase {
    private let targetNs: Int64 = 1_000_000_000

    private func pair(
        now: Double, pick: Double = 1.007, latePick: Bool = true, drain: Bool = false,
        last: Double = 1.009
    ) -> DeadlinePacer.Pair {
        DeadlinePacer.pair(
            now: now, vendAt: 1.000, last: last, pick: pick, latePick: latePick, drain: drain)
    }

    private func pickAt(_ p: DeadlinePacer.Pair) -> Double {
        if case .present(let at) = p { return at }
        return -1
    }

    func testTheLatencyIntentWaitsForThePickOrPresentsALateArrivalAtOnce() {
        XCTAssertEqual(pickAt(pair(now: 1.001)), 1.007, accuracy: 1e-9)
        XCTAssertEqual(pickAt(pair(now: 1.008)), 1.008, accuracy: 1e-9, "past the pick")
    }

    func testSmoothnessPresentsOnPairing() {
        XCTAssertEqual(pickAt(pair(now: 1.001, latePick: false)), 1.001, accuracy: 1e-9)
    }

    func testPastTheCutoffOrDrainingSkips() {
        XCTAssertEqual(pair(now: 1.0095), .skip, "would land a refresh late")
        XCTAssertEqual(pair(now: 1.001, drain: true), .skip)
    }

    /// A margin past the whole window (a GPU spike) must leave a quarter of it: expiring every
    /// drawable froze the stream, and with nothing rendered the margin never came down.
    func testTheCutoffAlwaysLeavesAQuarterOfTheWindow() {
        XCTAssertEqual(
            DeadlinePacer.cutoff(vendAt: 1.000, deadline: 1.008, margin: 0.004), 1.004,
            accuracy: 1e-9)
        XCTAssertEqual(
            DeadlinePacer.cutoff(vendAt: 1.000, deadline: 1.008, margin: 0.0094), 1.002,
            accuracy: 1e-9)
    }

    func testNoWindowAfterTheVendPresentsAtOnce() {
        XCTAssertEqual(pickAt(pair(now: 1.002, last: 0.999)), 1.002, accuracy: 1e-9)
    }

    func testTheMarginCoversWakeCpuAndGpu() {
        let pacer = DeadlinePacer()
        XCTAssertEqual(pacer.margin, 0.001, accuracy: 1e-9, "floor before any sample")
        pacer.noteRender(seconds: 0.0004)
        pacer.noteGPU(seconds: 0.0029)
        pacer.noteHeld(wakeLate: 0.001, superseded: false)
        XCTAssertEqual(pacer.margin, 0.0004 + 0.0029 + 0.001 + 0.0005, accuracy: 1e-9)
    }

    private func present(
        _ pacer: DeadlinePacer, issuedNs: Int64, lateMs: Double?, cutoffLeadMs: Double = 9
    ) {
        pacer.observe(
            issuedNs: issuedNs, presentedNs: lateMs.map { targetNs + Int64($0 * 1e6) },
            targetNs: targetNs, cutoffLeadNs: Int64(cutoffLeadMs * 1e6))
    }

    /// Issued 9.5 ms before the target: within 1 ms of a 9 ms cutoff lead.
    private let atCutoff = Int64(1_000_000_000 - 9_500_000)

    /// One late event at the cutoff, drained; `issuedNs` moves past each drain.
    private func lateEvent(_ pacer: DeadlinePacer, at issuedNs: Int64) {
        present(pacer, issuedNs: issuedNs, lateMs: 8.3)
        XCTAssertTrue(pacer.takeDrain(nowNs: issuedNs + 1))
    }

    func testAnIsolatedLatePresentDrainsOnceWithoutLearning() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        present(pacer, issuedNs: atCutoff, lateMs: 8.3)
        present(pacer, issuedNs: atCutoff, lateMs: 8.3) // same event, still undrained
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9, "the system's own odd late present")
        XCTAssertTrue(pacer.takeDrain(nowNs: 2_000_000_000))
        XCTAssertFalse(pacer.takeDrain(nowNs: 2_000_000_001), "one skip per event")
    }

    func testThreeLateEventsAtTheCutoffWithinASecondAddAMillisecond() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        lateEvent(pacer, at: atCutoff)
        lateEvent(pacer, at: atCutoff + 10)
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9)
        lateEvent(pacer, at: atCutoff + 20)
        XCTAssertEqual(pacer.margin, base + 0.001, accuracy: 1e-9)
    }

    func testLateEventsFarApartNeverLearn() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        for i in 0..<5 {
            lateEvent(pacer, at: atCutoff + Int64(i * 1000))
            for _ in 0..<130 { present(pacer, issuedNs: atCutoff + Int64(i * 1000), lateMs: 0) }
        }
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9)
    }

    func testALatePresentWellBeforeTheCutoffNeverLearns() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        for i in 0..<3 { lateEvent(pacer, at: targetNs - 16_000_000 + Int64(i * 10)) }
        XCTAssertEqual(pacer.margin, base, accuracy: 1e-9, "a queue, not a short margin")
    }

    func testPresentsQueuedBeforeTheDrainCountForNothing() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: atCutoff, lateMs: 8.3)
        XCTAssertTrue(pacer.takeDrain(nowNs: atCutoff + 5_000_000))
        present(pacer, issuedNs: atCutoff + 1_000_000, lateMs: 8.3) // queued before the skip
        XCTAssertFalse(pacer.takeDrain(nowNs: atCutoff + 9_000_000))
    }

    func testOnTimePresentsTakeTheExtraBack() {
        let pacer = DeadlinePacer()
        let base = pacer.margin
        for i in 0..<3 { lateEvent(pacer, at: atCutoff + Int64(i * 10)) }
        for _ in 0..<599 { present(pacer, issuedNs: atCutoff, lateMs: 0) }
        XCTAssertEqual(pacer.margin, base + 0.001, accuracy: 1e-9)
        present(pacer, issuedNs: atCutoff, lateMs: 0)
        XCTAssertEqual(pacer.margin, base + 0.0005, accuracy: 1e-9)
    }

    func testADroppedPresentArmsNothing() {
        let pacer = DeadlinePacer()
        present(pacer, issuedNs: atCutoff, lateMs: nil)
        XCTAssertFalse(pacer.takeDrain(nowNs: 2_000_000_000))
    }
}

/// Where frames land in the refresh, and the pick that keeps two of them out of one refresh.
final class ArrivalPhaseTests: XCTestCase {
    private let p = 8.33

    func testTheEstimateWrapsAroundTheVend() {
        var phase = ArrivalPhase()
        XCTAssertNil(phase.estimate())
        for i in 0..<60 { phase.note(fraction: i % 2 == 0 ? 0.98 : 1.02) }
        let est = phase.estimate()
        XCTAssertNotNil(est)
        XCTAssertEqual(min(est!.mean, 1 - est!.mean), 0, accuracy: 0.01, "0.98 and 0.02 meet at 0")
        XCTAssertEqual(est!.spread, 0.02, accuracy: 0.005)
    }

    func testAnEarlyArrivalIsPickedMidWayAfterIt() {
        XCTAssertEqual(
            ArrivalPhase.pickOffset(arrival: 1, period: p, window: 6.5, need: 2), 5.165,
            accuracy: 1e-9)
    }

    func testTheWindowEndServesWhenItStillClears() {
        XCTAssertEqual(
            ArrivalPhase.pickOffset(arrival: 3, period: p, window: 6.5, need: 2), 6.5,
            accuracy: 1e-9)
    }

    func testAnArrivalTooCloseToTheWindowEndWaitsARefresh() {
        XCTAssertEqual(
            ArrivalPhase.pickOffset(arrival: 5.5, period: p, window: 6.5, need: 2), 1.335,
            accuracy: 1e-9, "mid-way after the previous frame")
        XCTAssertEqual(
            ArrivalPhase.pickOffset(arrival: 7.5, period: p, window: 6.5, need: 2), 3.335,
            accuracy: 1e-9, "past the window altogether")
    }
}
#endif
