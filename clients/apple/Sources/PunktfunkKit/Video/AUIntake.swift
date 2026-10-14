// The AU intake both VideoToolbox pumps run before their sink: the background keep-alive drain,
// the report to the core's loss recovery, H.265 concealment and the format/keyframe bookkeeping
// of `AUPumpState`. Only the sink differs: stage-1 enqueues to an AVSampleBufferDisplayLayer,
// stage-2 submits to VideoToolbox.

import CoreMedia
import Foundation
import os

private let intakeLog = ClientLog(category: "video")

/// Thread-confined to one pump thread; one per session.
struct AUIntake {
    /// One AU the sink may take. The sink still honours `step.withhold`, after any check of its
    /// own that must see withheld AUs too.
    struct Intake {
        let au: AccessUnit
        let step: AUPumpState.Step
        /// The AU carries parameter sets (an IDR).
        let idr: Bool
    }

    private(set) var pump = AUPumpState()
    private let connection: PunktfunkConnection
    /// The decoder reads the RPS itself, so a lost reference is concealed in the bitstream first.
    private let concealer: HevcConcealer?
    private var wasUnrecoverable = false
    /// When the current wait for parameter sets began, for the resume log.
    private var awaitingSince = Date.distantPast

    init(connection: PunktfunkConnection) {
        self.connection = connection
        concealer = connection.videoCodec == .hevc ? HevcConcealer() : nil
    }

    /// Pull and vet the next AU (100 ms poll); nil when this iteration has nothing for the sink.
    /// `onHdrMeta` set drains the HDR mastering plane every iteration. Throws once the session
    /// closed.
    mutating func next(
        onFrame: ((AccessUnit) -> Void)?,
        onDecodedSize: ((Int, Int) -> Void)?,
        onHdrMeta: ((PunktfunkConnection.HdrMeta) -> Void)? = nil
    ) throws -> Intake? {
        // Background: drain one AU for flow control and host pacing, skip all bookkeeping.
        // Nothing decodes, so the core's freeze is dropped: left up it would ask for a keyframe
        // every half second. exitBackground asks for an IDR and the resumed index gap re-arms.
        if connection.isVideoDropped {
            connection.recoveryReset()
            _ = try connection.nextAU(timeoutMs: 100)
            return nil
        }
        // Re-asked every iteration; the core sends one ask per window.
        if pump.awaitingIDR { connection.requestKeyframe() }
        // Polled regardless of the Welcome's HDR flag: a game can enter HDR mid-session.
        if let onHdrMeta, let meta = try? connection.nextHdrMeta(timeoutMs: 0) {
            onHdrMeta(meta)
        }
        guard var au = try connection.nextAU(timeoutMs: 100) else { return nil }
        // The core asks for a lost range and arms the freeze here. A straggler is dropped before
        // the concealer sees it: the concealer mirrors the decoder's DPB.
        let arrival = connection.recoveryOnFrame(au.frameIndex)
        onFrame?(au)
        if !arrival.admit { return nil }
        let concealed = conceal(&au)
        let idrFormat = connection.videoCodec.formatDescription(fromKeyframe: au.data)
        let step = pump.note(
            frameIndex: au.frameIndex, idrFormat: idrFormat, lossAhead: arrival.gap > 0,
            flags: au.flags, concealed: concealed)
        if step.askKeyframe { connection.requestKeyframe() }
        if let size = step.newSize { onDecodedSize?(size.width, size.height) }
        if step.resumed {
            let ms = Int(Date().timeIntervalSince(awaitingSince) * 1000)
            intakeLog.notice("video: recovery IDR received — resumed after \(ms, privacy: .public) ms")
        }
        if step.startedFormatWait {
            awaitingSince = Date()
            intakeLog.warning(
                "video: received AUs but no decodable format (missing/unparsed parameter sets) — requesting an IDR until one seeds it"
            )
        }
        return Intake(au: au, step: step, idr: idrFormat != nil)
    }

    /// The sink lost its decoder state: wait for the next IDR's parameter sets. Stamps the wait
    /// start for the resume log.
    mutating func requireIDR() {
        pump.requireIDR()
        awaitingSince = Date()
    }

    /// Run the H.265 concealer over `au`, swapping in its rewrite.
    private mutating func conceal(_ au: inout AccessUnit) -> AUPumpState.Concealment {
        guard let concealer else { return .none }
        let concealed: AUPumpState.Concealment
        switch concealer.conceal(au.data) {
        case .intact:
            concealed = .decodable
        case .rewritten(let data):
            concealed = .decodable
            au = au.replacing(data: data)
            intakeLog.notice(
                "video: frame \(au.frameIndex, privacy: .public) names a lost reference — moved to a present picture until the re-anchor"
            )
        case .unrecoverable:
            concealed = .unrecoverable
        }
        if concealed == .unrecoverable, !wasUnrecoverable {
            intakeLog.warning(
                "video: frame \(au.frameIndex, privacy: .public) names a lost reference with nothing to stand in — withholding until an IDR"
            )
        }
        wasUnrecoverable = concealed == .unrecoverable
        return concealed
    }
}
