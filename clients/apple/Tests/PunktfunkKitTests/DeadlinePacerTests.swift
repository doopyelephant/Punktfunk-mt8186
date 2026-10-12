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
final class PhaseTrackerTests: XCTestCase {
    private let p = 0.00833
    private let w = 0.0065

    private func settled(at f: Double) -> PhaseTracker {
        var phase = PhaseTracker()
        for i in 0..<PhaseTracker.capacity { phase.note(fraction: f + (i % 2 == 0 ? 0.005 : -0.005)) }
        return phase
    }

    func testTheEstimateWrapsAroundTheVend() {
        var phase = PhaseTracker()
        XCTAssertNil(phase.estimate())
        for i in 0..<PhaseTracker.capacity { phase.note(fraction: i % 2 == 0 ? 0.98 : 1.02) }
        let est = phase.estimate()
        XCTAssertNotNil(est)
        XCTAssertEqual(min(est!.mean, 1 - est!.mean), 0, accuracy: 0.01, "0.98 and 0.02 meet at 0")
        XCTAssertEqual(est!.sigma, 0.02 / 1.645, accuracy: 0.003)
    }

    func testAnOutlierIsNotSampledButAMovedPhaseRestarts() {
        var phase = settled(at: 0.3)
        phase.note(fraction: 0.8)
        XCTAssertEqual(phase.estimate()!.mean, 0.3, accuracy: 0.005, "one loss burst")
        XCTAssertEqual(phase.drainCounters().rejected, 1)
        for _ in 0..<PhaseTracker.capacity / 4 { phase.note(fraction: 0.8) }
        XCTAssertNil(phase.estimate(), "a half second of nothing else: the phase moved")
    }

    func testAnEarlyArrivalIsPickedMidWayAfterIt() {
        var phase = PhaseTracker()
        let pick = phase.pick(arrival: 0.001, sigma: 0.0002, period: p, window: w, now: 0)
        XCTAssertEqual(pick.offset, 0.001 + p / 2, accuracy: 1e-9)
        XCTAssertFalse(pick.switched)
        XCTAssertEqual(phase.mapping, .same)
    }

    func testAnArrivalTooCloseToTheWindowEndWaitsARefresh() {
        var phase = PhaseTracker()
        let pick = phase.pick(arrival: 0.0055, sigma: 0.0005, period: p, window: w, now: 0)
        XCTAssertEqual(pick.offset, 0.0055 - p / 2, accuracy: 1e-9, "mid-way after the previous")
        XCTAssertTrue(pick.switched)
        XCTAssertEqual(phase.mapping, .next)
    }

    func testAClearedSamePickNeverTradesARefreshForMoreClearance() {
        var phase = PhaseTracker()
        // Same clears by 2.5 ms (≥ 3σ); next would clear by 4 ms but costs a refresh.
        let pick = phase.pick(arrival: 0.004, sigma: 0.0005, period: p, window: w, now: 0)
        XCTAssertEqual(pick.offset, w, accuracy: 1e-9)
        XCTAssertEqual(phase.mapping, .same)
    }

    func testTheWayBackWaitsOutTheDwell() {
        var phase = PhaseTracker()
        _ = phase.pick(arrival: 0.0055, sigma: 0.0005, period: p, window: w, now: 0)
        XCTAssertEqual(phase.mapping, .next)
        _ = phase.pick(arrival: 0.004, sigma: 0.0005, period: p, window: w, now: 1)
        XCTAssertEqual(phase.mapping, .next, "a switch costs a glitch; 1 s is too soon")
        let back = phase.pick(arrival: 0.004, sigma: 0.0005, period: p, window: w, now: 2.5)
        XCTAssertEqual(phase.mapping, .same)
        XCTAssertTrue(back.switched)
    }

    func testAPickInsideOneSigmaSwitchesAtOnce() {
        var phase = PhaseTracker()
        _ = phase.pick(arrival: 0.0055, sigma: 0.0005, period: p, window: w, now: 0)
        let pick = phase.pick(arrival: 0.0002, sigma: 0.0005, period: p, window: w, now: 0.5)
        XCTAssertTrue(pick.switched, "frames are flipping now")
        XCTAssertEqual(phase.mapping, .same)
    }
}

/// The bounded one-frame hold.
final class CarryTests: XCTestCase {
    func testABurstShowsTheOlderAndKeepsTheNewer() {
        var carry = Carry()
        XCTAssertTrue(carry.choose([1], allowed: true) == (1, nil))
        XCTAssertTrue(carry.choose([1, 2], allowed: true) == (1, 2))
        XCTAssertTrue(carry.choose([2, 3], allowed: true) == (2, 3), "one behind while it lasts")
        XCTAssertTrue(carry.choose([3], allowed: true) == (3, nil), "a gap ends the hold")
        XCTAssertEqual(carry.run, 0)
        XCTAssertEqual(carry.drainCounters().carried, 2)
    }

    func testTwoNewBehindAKeptFrameDropTheKeptOne() {
        var carry = Carry()
        XCTAssertTrue(carry.choose([1, 2, 3], allowed: true) == (2, 3))
    }

    func testAPhaseSwitchNeverCarries() {
        var carry = Carry()
        XCTAssertTrue(carry.choose([1, 2], allowed: false) == (2, nil))
    }

    func testAStandingQueueEndsAfterItsBound() {
        var carry = Carry()
        for i in 0..<Carry.maxRun { _ = carry.choose([i, i + 1], allowed: true) }
        XCTAssertTrue(carry.choose([50, 51], allowed: true) == (51, nil))
        XCTAssertEqual(carry.drainCounters().forcedDrains, 1)
    }

    /// A host slower than the panel ends its own holds within a beat: the bound covers one.
    func testASlowHostHoldsForABeat() {
        let p = 1.0 / 120
        XCTAssertEqual(Carry.run(forSlowBy: 0, period: p), Carry.maxRun, "matched or faster")
        XCTAssertEqual(Carry.run(forSlowBy: 2.3, period: p), Int(1.2 / 2.3 / p))
        XCTAssertEqual(Carry.run(forSlowBy: 0.4, period: p), Int(2.0 / p), "capped at 2 s")
        XCTAssertEqual(Carry.run(forSlowBy: 60, period: p), Carry.maxRun, "never under the base")
        var carry = Carry()
        for i in 0..<Carry.maxRun { _ = carry.choose([i, i + 1], allowed: true, maxRun: 62) }
        XCTAssertTrue(carry.choose([50, 51], allowed: true, maxRun: 62) == (50, 51), "still held")
    }

    func testTheStoreKeepsTheNewestTwo() {
        let store = FrameStore<Int>(policy: .newestTwo)
        store.submit(1); store.submit(2); store.submit(3)
        XCTAssertEqual(store.take(), 2, "the oldest kept")
        store.submit(4)
        store.putBack(2)
        XCTAssertEqual(store.takeAll(), [3, 4], "two newer win over a returned frame")
    }

    func testGlitchesCountStepsAgainstTheSource() {
        let r = 8.333
        let g = PresentDebugStats.glitches(
            steps: [(8.33, 8.33), (16.67, 8.33), (8.33, 16.67), (16.67, 16.67)], refreshMs: r)
        XCTAssertEqual(g.repeats, 1)
        XCTAssertEqual(g.skips, 1)
    }
}
#endif
