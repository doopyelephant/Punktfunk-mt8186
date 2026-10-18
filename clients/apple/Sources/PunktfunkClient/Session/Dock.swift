// visionOS: the stream docked into the system environment, and what its window shows meanwhile.
// Only an AVPlayer docks and gets the environment's light; its picture is the session's own
// decoded frames (DockPlayback), never a second decode.

#if os(visionOS)
import AVKit
import PunktfunkKit
import SwiftUI

private let dockLog = ClientLog(category: "dock")

/// The app's one dock (AVKit expands one player at a time) and the stream in it.
@MainActor @Observable
final class DockStage {
    static let shared = DockStage()

    /// The frames the dock's player shows while it is up.
    private(set) var feed: DockFeed?
    /// The connection presenting into it.
    private(set) var owner: ObjectIdentifier?
    /// Sessions the environment refused: their picture stays in the window until they end.
    private(set) var refused: Set<ObjectIdentifier> = []

    func canDock(_ connection: PunktfunkConnection) -> Bool {
        DockFeed.supports(connection) && !refused.contains(ObjectIdentifier(connection))
    }

    func feed(for connection: PunktfunkConnection) -> DockFeed? {
        owner == ObjectIdentifier(connection) ? feed : nil
    }

    /// Dock `connection`'s picture. A stream that held the dock goes back to its window.
    func enter(_ connection: PunktfunkConnection) {
        let id = ObjectIdentifier(connection)
        guard owner != id, canDock(connection) else { return }
        owner = id
        feed = DockFeed()
    }

    /// Bring `connection`'s picture back to its window, if it is docked.
    func leave(_ connection: PunktfunkConnection) {
        guard owner == ObjectIdentifier(connection) else { return }
        owner = nil
        feed = nil
    }

    /// The player behind `feed` is gone; `refused` keeps its session in the window.
    func release(_ feed: DockFeed, refused: Bool) {
        guard feed === self.feed, let owner else { return }
        if refused { self.refused.insert(owner) }
        self.owner = nil
        self.feed = nil
    }

    /// `connection` ended: its dock goes, and its refusal with it.
    func ended(_ connection: PunktfunkConnection) {
        leave(connection)
        refused.remove(ObjectIdentifier(connection))
    }
}

/// The dock's player, laid over the stream window it expands out of. Expanded, the system docks
/// it into the open environment; back in the window, the dock is over.
struct DockPlayer: UIViewControllerRepresentable {
    let feed: DockFeed
    let mode: (width: UInt32, height: UInt32, refreshHz: UInt32)
    /// Main actor: the environment refused the picture.
    let onRefused: () -> Void

    func makeUIViewController(context: Context) -> DockPlayerController {
        DockPlayerController(feed: feed, mode: mode, onRefused: onRefused)
    }

    func updateUIViewController(_ controller: DockPlayerController, context: Context) {}

    static func dismantleUIViewController(_ controller: DockPlayerController, coordinator: ()) {
        controller.close()
    }
}

/// Holds the AVPlayerViewController, which is not meant to be subclassed, and expands it once it
/// is on screen and playing.
final class DockPlayerController: UIViewController, AVExperienceController.Delegate {
    private let playerController = AVPlayerViewController()
    private let feed: DockFeed
    private let mode: (width: UInt32, height: UInt32, refreshHz: UInt32)
    private let onRefused: () -> Void
    private var opening: Task<Void, Never>?
    private var status: NSKeyValueObservation?

    init(
        feed: DockFeed, mode: (width: UInt32, height: UInt32, refreshHz: UInt32),
        onRefused: @escaping () -> Void
    ) {
        self.feed = feed
        self.mode = mode
        self.onRefused = onRefused
        super.init(nibName: nil, bundle: nil)
    }

    required init?(coder: NSCoder) { nil }

    override func viewDidLoad() {
        super.viewDidLoad()
        addChild(playerController)
        playerController.view.frame = view.bounds
        playerController.view.autoresizingMask = [.flexibleWidth, .flexibleHeight]
        view.addSubview(playerController.view)
        playerController.didMove(toParent: self)
        // A live picture: nothing to scrub, skip or hand to Now Playing.
        playerController.showsPlaybackControls = false
        playerController.requiresLinearPlayback = true
        playerController.updatesNowPlayingInfoCenter = false
        playerController.allowsPictureInPicturePlayback = false
        playerController.experienceController.delegate = self
    }

    override func viewDidAppear(_ animated: Bool) {
        super.viewDidAppear(animated)
        guard opening == nil else { return }
        opening = Task { [weak self] in await self?.open() }
    }

    private func open() async {
        do {
            let player = try await DockPlayback.player(
                feed: feed, width: Int(mode.width), height: Int(mode.height),
                hz: Int(mode.refreshHz))
            guard !Task.isCancelled else { return }
            status = player.currentItem?.observe(\.status) { [weak self] item, _ in
                guard item.status == .failed else { return }
                let error = item.error as NSError?
                let cause = error?.userInfo[NSUnderlyingErrorKey] as? NSError
                dockLog.error(
                    "dock player item failed code=\(error?.code ?? 0) cause=\(cause?.code ?? 0)")
                Task { @MainActor in self?.refuse() }
            }
            playerController.player = player
            player.play()
        } catch {
            dockLog.error("dock player did not start: \(error.localizedDescription)")
            refuse()
            return
        }
        let result = await playerController.experienceController.transition(to: .expanded)
        if case .reversed(let reason) = result {
            dockLog.warning("dock did not expand reason=\(String(describing: reason))")
            refuse()
        }
    }

    /// The environment did not take the picture: say so once, and keep it in the window.
    private func refuse() {
        guard DockStage.shared.feed === feed else { return }
        DockStage.shared.release(feed, refused: true)
        onRefused()
    }

    /// Bring the player back from the dock before the overlay goes.
    func close() {
        opening?.cancel()
        status = nil
        playerController.player?.pause()
        guard playerController.experienceController.experience != .embedded else { return }
        Task { await playerController.experienceController.transition(to: .embedded) }
    }

    func experienceController(
        _ controller: AVExperienceController,
        didChangeTransitionContext context: AVExperienceController.TransitionContext
    ) {
        guard context.status == .finished(result: .completed) else { return }
        if context.toExperience == .expanded {
            dockLog.info("dock entered experience=expanded")
        } else if context.toExperience == .embedded {
            // The player's own control brought it back: the dock is over.
            dockLog.info("dock left experience=embedded")
            DockStage.shared.release(feed, refused: false)
        }
    }

    func experienceController(
        _ controller: AVExperienceController,
        prepareForTransitionUsing context: AVExperienceController.TransitionContext
    ) async {}

    func experienceController(
        _ controller: AVExperienceController,
        didChangeAvailableExperiences availableExperiences: AVExperienceController.Experiences
    ) {
        dockLog.info("dock experiences available=\(String(describing: availableExperiences))")
    }
}
#endif
