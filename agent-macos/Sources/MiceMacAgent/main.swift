import AppKit
import Foundation
import MiceMacSupport
import Vision
@preconcurrency import ScreenCaptureKit
import UniformTypeIdentifiers
import ImageIO

private struct GestureTrigger {
    let requiredFlags: CGEventFlags

    static func fromEnvironment() -> Self {
        switch ProcessInfo.processInfo.environment["MICE_GESTURE_TRIGGER"] {
        case "ctrl+alt+space":
            return Self(requiredFlags: [.maskControl, .maskAlternate])
        case "cmd+shift+space":
            return Self(requiredFlags: [.maskCommand, .maskShift])
        default:
            return Self(requiredFlags: [.maskControl, .maskShift])
        }
    }

    func matches(_ event: CGEvent) -> Bool {
        event.getIntegerValueField(.keyboardEventKeycode) == 49
            && event.flags.contains(requiredFlags)
    }
}

private let gestureTrigger = GestureTrigger.fromEnvironment()

private enum SelectionGesture {
    static let summarize = ProcessInfo.processInfo.environment[
        "MICE_SUMMARIZE_SELECTION_TRIGGER"
    ] ?? "ctrl-double-tap"
    static let infographic = ProcessInfo.processInfo.environment[
        "MICE_INFOGRAPHIC_SELECTION_TRIGGER"
    ] ?? "ctrl+alt+i"

    static func action(for event: CGEvent) -> String? {
        let keyCode = event.getIntegerValueField(.keyboardEventKeycode)
        if event.flags.contains([.maskControl, .maskAlternate]) {
            if infographic == "ctrl+alt+i", keyCode == 34 { return "image" }
            if infographic == "ctrl+alt+m", keyCode == 46 { return "image" }
            if summarize == "ctrl+alt+s", keyCode == 1 { return "summarize" }
        }
        return nil
    }
}

private enum SmartCopyGesture {
    static let trigger = ProcessInfo.processInfo.environment["MICE_SMART_COPY_TRIGGER"] ?? "ctrl+alt+c"

    static func matches(_ event: CGEvent) -> Bool {
        guard event.flags.contains([.maskControl, .maskAlternate]) else { return false }
        let keyCode = event.getIntegerValueField(.keyboardEventKeycode)
        switch trigger {
        case "ctrl+alt+c": return keyCode == 8
        case "ctrl+alt+x": return keyCode == 7
        default: return false
        }
    }
}

private enum GoalGesture {
    static let trigger = ProcessInfo.processInfo.environment["MICE_GOAL_TRIGGER"] ?? "ctrl+alt+space"

    static func matches(_ event: CGEvent) -> Bool {
        trigger == "ctrl+alt+space"
            && event.getIntegerValueField(.keyboardEventKeycode) == 49
            && event.flags.contains([.maskControl, .maskAlternate])
    }
}

private enum AutopilotStopGesture {
    static let enabled = ProcessInfo.processInfo.environment["MICE_AUTOPILOT_ACTIVE"] == "1"

    static func matches(_ event: CGEvent) -> Bool {
        enabled && event.getIntegerValueField(.keyboardEventKeycode) == 53
    }
}

/// One-shot commands only need the overlay surface. In this mode the agent
/// never creates its global event tap, so `mice ask` works without an Input
/// Monitoring grant and observes no input at all.
private let overlayOnlyMode = ProcessInfo.processInfo.environment["MICE_OVERLAY_ONLY"] == "1"

private struct ClipboardSnapshot {
    private let items: [[(type: NSPasteboard.PasteboardType, data: Data)]]

    init(_ pasteboard: NSPasteboard) {
        items = (pasteboard.pasteboardItems ?? []).map { item in
            item.types.compactMap { type in
                item.data(forType: type).map { (type, $0) }
            }
        }
    }

    func restore(to pasteboard: NSPasteboard) {
        pasteboard.clearContents()
        guard !items.isEmpty else { return }
        let restored = items.map { values -> NSPasteboardItem in
            let item = NSPasteboardItem()
            for value in values {
                item.setData(value.data, forType: value.type)
            }
            return item
        }
        pasteboard.writeObjects(restored)
    }
}

@main
@MainActor
struct MiceMacAgent {
    private static var hoverTask: Task<Void, Never>?
    private static var lastHoverFingerprint = ""
    private static var eventTap: CFMachPort?
    private static var lastControlDown: TimeInterval?
    /// The app the person was using when MICE opened a result. The panel is
    /// non-activating, but its menu can briefly become the key interaction.
    private static var pasteDestination: NSRunningApplication?

    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.accessory)
        let overlay = OverlayController()
        sendInitialize()
        
        // MICE observes normal mouse input but does not consume it. Native app
        // selection is the only way to retain source table/text/image formats.
        let mask = (CGEventMask(1) << CGEventType.keyDown.rawValue)
            | (CGEventMask(1) << CGEventType.mouseMoved.rawValue)
            | (CGEventMask(1) << CGEventType.flagsChanged.rawValue)
        if overlayOnlyMode {
            // No event tap, no hover, no gestures: display-only lifetime.
        } else if let tap = CGEvent.tapCreate(
            tap: .cgSessionEventTap,
            place: .headInsertEventTap,
            options: .defaultTap,
            eventsOfInterest: CGEventMask(mask),
            callback: { (proxy, type, event, refcon) -> Unmanaged<CGEvent>? in
                if type == .tapDisabledByTimeout || type == .tapDisabledByUserInput {
                    if let eventTap = MiceMacAgent.eventTap {
                        CGEvent.tapEnable(tap: eventTap, enable: true)
                    }
                    return nil
                }
                if type == .keyDown {
                    if event.getIntegerValueField(.keyboardEventKeycode) == 53 {
                        Task { @MainActor in
                            OverlayController.dismissActive()
                        }
                        // Escape keeps its normal pass-through behavior for
                        // the foreground app while dismissing only MICE.
                        return Unmanaged.passUnretained(event)
                    }
                    if AutopilotStopGesture.matches(event) {
                        Task { @MainActor in
                            MiceMacAgent.cancelHover()
                            MiceMacAgent.stopAutopilot()
                        }
                        return nil
                    }
                    if GoalGesture.matches(event) {
                        Task { @MainActor in
                            MiceMacAgent.cancelHover()
                            MiceMacAgent.requestGoal()
                        }
                        return nil
                    }
                    if gestureTrigger.matches(event) {
                        Task {
                            await MiceMacAgent.triggerCapture()
                        }
                        return nil
                    }
                    if SmartCopyGesture.matches(event) {
                        Task { @MainActor in
                            MiceMacAgent.cancelHover()
                            MiceMacAgent.sendClipboardCaptured()
                        }
                        return nil
                    }
                    if let action = SelectionGesture.action(for: event) {
                        Task { @MainActor in
                            MiceMacAgent.cancelHover()
                            MiceMacAgent.sendSelectedText(action: action)
                        }
                        return nil
                    }
                }
                if type == .mouseMoved {
                    let point = event.location
                    Task { @MainActor in
                        if event.flags.contains(.maskControl)
                            && !event.flags.contains(.maskAlternate) {
                            MiceMacAgent.scheduleHover(at: point)
                        } else {
                            MiceMacAgent.cancelHover()
                        }
                    }
                }
                if type == .flagsChanged {
                    Task { @MainActor in
                        if MiceMacAgent.isControlKey(event),
                           event.flags.contains(.maskControl),
                           !event.flags.contains(.maskAlternate),
                           SelectionGesture.summarize == "ctrl-double-tap" {
                            let now = Date.timeIntervalSinceReferenceDate
                            if let last = MiceMacAgent.lastControlDown,
                               now - last <= 0.45 {
                                MiceMacAgent.lastControlDown = nil
                                MiceMacAgent.cancelHover()
                                MiceMacAgent.sendSelectedText(action: "summarize")
                            } else {
                                MiceMacAgent.lastControlDown = now
                                MiceMacAgent.scheduleHover(at: NSEvent.mouseLocation)
                            }
                        } else if event.flags.contains(.maskControl)
                            && !event.flags.contains(.maskAlternate) {
                            MiceMacAgent.scheduleHover(at: NSEvent.mouseLocation)
                        } else {
                            MiceMacAgent.cancelHover()
                        }
                    }
                }
                return Unmanaged.passUnretained(event)
            },
            userInfo: nil
        ) {
            MiceMacAgent.eventTap = tap
            let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .commonModes)
            CGEvent.tapEnable(tap: tap, enable: true)
        }
        
        DispatchQueue.global(qos: .userInitiated).async {
            while let frame = try? readFrameText() {
                DispatchQueue.main.async { overlay.handle(json: frame) }
            }
            DispatchQueue.main.async { NSApp.terminate(nil) }
        }
        app.run()
    }

    static func sendInitialize() {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": [
                "protocolVersion": "1.0",
                "platform": "macos",
                "capabilities": [
                    "screen_capture": MicePermission.screenRecording.granted,
                    "ax_read": MicePermission.accessibility.granted,
                    "inject_text": MicePermission.accessibility.granted,
                    "overlay": true,
                    "local_ocr": MicePermission.screenRecording.granted,
                    "browser_bridge": false,
                    "input_monitoring": MicePermission.inputMonitoring.granted,
                ],
            ],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func triggerCapture() async {
        await captureRegion(centeredAt: NSEvent.mouseLocation, mode: "prompt")
    }

    /// Convert a Cocoa screen point (bottom-left origin on the primary
    /// display) into the CoreGraphics top-left-origin global space that
    /// ScreenCaptureKit display frames use.
    static func cocoaToCG(_ point: CGPoint) -> CGPoint {
        CGPoint(x: point.x, y: CGDisplayBounds(CGMainDisplayID()).height - point.y)
    }

    /// The display actually containing the point, so capture follows the
    /// mouse across a multi-display arrangement instead of always using the
    /// first display.
    static func display(containing point: CGPoint, in content: SCShareableContent) -> SCDisplay? {
        content.displays.first { $0.frame.contains(point) } ?? content.displays.first
    }

    static func captureRegion(centeredAt mouse: CGPoint, mode: String) async {
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            let cgPoint = cocoaToCG(mouse)
            guard let display = display(containing: cgPoint, in: content) else { return }
            let width: CGFloat = 400
            let height: CGFloat = 300
            let frame = display.frame
            // sourceRect is display-relative with a top-left origin, matching
            // the CG space of `frame`; clamp so the region stays on-display.
            let x = min(max(cgPoint.x - frame.origin.x - width / 2, 0), max(frame.width - width, 0))
            let y = min(max(cgPoint.y - frame.origin.y - height / 2, 0), max(frame.height - height, 0))
            try await captureRegion(
                content: content,
                display: display,
                sourceRect: CGRect(x: x, y: y, width: width, height: height),
                mode: mode
            )
        } catch {
            // Capture permission or system failures remain non-fatal to the event tap.
        }
    }

    static func captureRegion(
        content: SCShareableContent,
        display: SCDisplay,
        sourceRect: CGRect,
        mode: String
    ) async throws {
            guard sourceRect.width >= 8, sourceRect.height >= 8 else { return }
            let filter = SCContentFilter(display: display, excludingWindows: [])
            let configuration = SCStreamConfiguration()
            configuration.width = Int(sourceRect.width)
            configuration.height = Int(sourceRect.height)
            configuration.sourceRect = sourceRect
            let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration)
            
            // Perform OCR
            let ocrText = await performOCR(on: image)
            
            // Read AX element
            var role: String? = nil
            var title: String? = nil
            if let element = try? AXSupport.elementAtCursor() {
                let desc = AXSupport.describe(element)
                role = desc.role
                title = desc.title
            }
            
            // Base64 encode PNG
            guard let base64 = imageToBase64(image) else { return }
            
            // Send selection.captured notification
            sendSelectionCaptured(pixels: base64, text: ocrText, role: role, title: title, mode: mode)
    }

    /// Bundle-ID prefixes MICE refuses to capture: a credential manager's
    /// window would put secrets into a model-bound image.
    private static let sensitiveCaptureBundlePrefixes = [
        "com.1password", "com.agilebits", "com.apple.keychainaccess",
        "com.apple.passwords", "com.bitwarden", "com.dashlane", "com.lastpass",
        "com.keepassium", "org.keepassxc",
    ]

    /// One native capture, only ever in response to an explicit
    /// `screen.capture` request from the core. Nothing is persisted; the
    /// captured area is flashed on screen so the person sees exactly what
    /// MICE looked at.
    static func captureForVision(sessionID: String, scope: String) async {
        guard MicePermission.screenRecording.granted else {
            sendScreenCaptured(
                sessionID: sessionID,
                error: "Screen Recording permission is not granted to MICE."
            )
            return
        }
        let frontmost = NSWorkspace.shared.frontmostApplication
        if let bundleID = frontmost?.bundleIdentifier?.lowercased(),
           sensitiveCaptureBundlePrefixes.contains(where: { bundleID.hasPrefix($0) }) {
            sendScreenCaptured(
                sessionID: sessionID,
                error: "MICE does not capture credential or password-manager apps."
            )
            return
        }
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(
                false, onScreenWindowsOnly: true
            )
            let image: CGImage
            var appName = frontmost?.localizedName
            var windowTitle: String?
            let detailScope = scope == "front_window_detail"
            if scope == "front_window" || detailScope {
                guard let window = frontWindow(in: content) else {
                    sendScreenCaptured(
                        sessionID: sessionID,
                        error: "No eligible front window is available. Use `mice see --display ...` to explicitly capture the display."
                    )
                    return
                }
                // The sensitive-app rule must apply to the app actually being
                // captured, which is not the frontmost app when the command
                // runs from a terminal.
                if let ownerBundle = window.owningApplication?.bundleIdentifier.lowercased(),
                   sensitiveCaptureBundlePrefixes.contains(where: { ownerBundle.hasPrefix($0) }) {
                    sendScreenCaptured(
                        sessionID: sessionID,
                        error: "MICE does not capture credential or password-manager apps."
                    )
                    return
                }
                windowTitle = window.title
                appName = window.owningApplication?.applicationName ?? appName
                flashFrame(cgRect: window.frame)
                let configuration = SCStreamConfiguration()
                if detailScope {
                    // Native-resolution capture: dense small text (spreadsheet
                    // cells) survives for the tiled OCR pass below. Only the
                    // on-device OCR ever sees this full-resolution image. One
                    // uniform fit factor preserves the aspect ratio when a
                    // single dimension exceeds the cap.
                    let scale = backingScale(forCGRect: window.frame)
                    let pixelWidth = window.frame.width * scale
                    let pixelHeight = window.frame.height * scale
                    let fit = min(1, 6_000 / max(pixelWidth, pixelHeight, 1))
                    configuration.width = max(Int(pixelWidth * fit), 8)
                    configuration.height = max(Int(pixelHeight * fit), 8)
                } else {
                    let size = boundedCaptureSize(window.frame.size)
                    configuration.width = size.width
                    configuration.height = size.height
                }
                image = try await SCScreenshotManager.captureImage(
                    contentFilter: SCContentFilter(desktopIndependentWindow: window),
                    configuration: configuration
                )
            } else {
                let cgPoint = cocoaToCG(NSEvent.mouseLocation)
                guard let display = display(containing: cgPoint, in: content) else {
                    sendScreenCaptured(sessionID: sessionID, error: "No display is available to capture.")
                    return
                }
                // `--display` captures every visible app under the pointer,
                // not merely the app that happened to be frontmost when the
                // CLI ran. Refuse before flashing or capturing if even one
                // credential-manager window is visible on that display.
                if sensitiveWindow(on: display, in: content) != nil {
                    sendScreenCaptured(
                        sessionID: sessionID,
                        error: "MICE does not capture a display containing a credential or password-manager window."
                    )
                    return
                }
                flashFrame(cgRect: display.frame)
                let configuration = SCStreamConfiguration()
                let size = boundedCaptureSize(display.frame.size)
                configuration.width = size.width
                configuration.height = size.height
                image = try await SCScreenshotManager.captureImage(
                    contentFilter: SCContentFilter(display: display, excludingWindows: []),
                    configuration: configuration
                )
            }
            let ocrText = detailScope
                ? await performTiledOCR(on: image)
                : await performOCR(on: image)
            // The outbound image stays bounded even in detail mode; the
            // full-resolution pixels never leave this machine.
            let outbound = detailScope ? (downscaled(image, maxDimension: 1_600) ?? image) : image
            guard let pngBase64 = imageToBase64(outbound), pngBase64.count <= 12 * 1024 * 1024 else {
                sendScreenCaptured(sessionID: sessionID, error: "The capture is too large to send safely.")
                return
            }
            sendScreenCaptured(
                sessionID: sessionID,
                pngBase64: pngBase64,
                ocrText: ocrText,
                appName: appName,
                windowTitle: windowTitle
            )
        } catch {
            sendScreenCaptured(
                sessionID: sessionID,
                error: "Screen capture failed: \(error.localizedDescription)"
            )
        }
    }

    /// Processes whose windows must never be the "front window": MICE itself
    /// plus the shell/terminal chain that launched the command. Running
    /// `mice see` from a terminal necessarily makes that terminal frontmost,
    /// so capturing the literal frontmost app would always read the terminal
    /// instead of the app the person is asking about.
    private static let excludedLaunchPids: Set<pid_t> = {
        var pids = Set(
            (ProcessInfo.processInfo.environment["MICE_EXCLUDE_PIDS"] ?? "")
                .split(separator: ",")
                .compactMap { pid_t($0) }
        )
        pids.insert(pid_t(ProcessInfo.processInfo.processIdentifier))
        return pids
    }()

    /// Known terminal hosts are excluded even if their shell is no longer an
    /// ancestor of MICE (for example after tmux/SSH detach). The Rust launcher
    /// adds the specific IDE host when TERM_PROGRAM identifies one.
    private static let excludedTerminalBundlePrefixes: [String] = {
        let defaults = [
            "com.apple.terminal", "com.googlecode.iterm2", "dev.warp",
            "net.kovidgoyal.kitty", "org.alacritty", "com.github.wez.wezterm",
            "co.zeit.hyper", "com.mitchellh.ghostty",
        ]
        let inherited = (ProcessInfo.processInfo.environment["MICE_EXCLUDE_BUNDLES"] ?? "")
            .split(separator: ",")
            .map { String($0).lowercased() }
        return Array(Set(defaults + inherited))
    }()

    private static func isSensitiveCaptureWindow(_ window: SCWindow) -> Bool {
        guard let bundleID = window.owningApplication?.bundleIdentifier.lowercased() else {
            return false
        }
        return sensitiveCaptureBundlePrefixes.contains { bundleID.hasPrefix($0) }
    }

    private static func sensitiveWindow(on display: SCDisplay, in content: SCShareableContent) -> SCWindow? {
        content.windows.first { window in
            window.isOnScreen
                && !window.frame.isEmpty
                && window.frame.intersects(display.frame)
                && isSensitiveCaptureWindow(window)
        }
    }

    /// The frontmost eligible normal-layer window that is not owned by MICE
    /// or its launch chain. SCShareableContent orders windows front to back.
    private static func frontWindow(in content: SCShareableContent) -> SCWindow? {
        content.windows.first { window in
            guard let pid = window.owningApplication?.processID else { return false }
            let bundleID = window.owningApplication?.bundleIdentifier.lowercased() ?? ""
            return !excludedLaunchPids.contains(pid)
                && !excludedTerminalBundlePrefixes.contains(where: { bundleID.hasPrefix($0) })
                && window.windowLayer == 0
                && window.isOnScreen
                && window.frame.width >= 120 && window.frame.height >= 90
        }
    }

    /// Cap the larger dimension so uploads stay bounded while text remains
    /// legible for OCR and vision models.
    private static func boundedCaptureSize(_ size: CGSize) -> (width: Int, height: Int) {
        let maxDimension: CGFloat = 1_600
        let scale = min(1, maxDimension / max(size.width, size.height, 1))
        return (max(Int(size.width * scale), 8), max(Int(size.height * scale), 8))
    }

    /// The backing scale of the screen containing a CG-space rectangle, for
    /// native-resolution detail captures.
    private static func backingScale(forCGRect cgRect: CGRect) -> CGFloat {
        let primaryHeight = CGDisplayBounds(CGMainDisplayID()).height
        let cocoaCenter = NSPoint(x: cgRect.midX, y: primaryHeight - cgRect.midY)
        return NSScreen.screens
            .first { $0.frame.contains(cocoaCenter) }?
            .backingScaleFactor ?? 2
    }

    /// OCR dense content viewport by viewport, in reading order. Vision
    /// recognizes small spreadsheet text far better on native-resolution
    /// tiles than on one downscaled full-window image.
    static func performTiledOCR(on image: CGImage) async -> String {
        let tileSize = 2_000
        let columns = max(1, (image.width + tileSize - 1) / tileSize)
        let rows = max(1, (image.height + tileSize - 1) / tileSize)
        if columns == 1 && rows == 1 {
            return await performOCR(on: image)
        }
        var parts: [String] = []
        for row in 0..<rows {
            for column in 0..<columns {
                let rect = CGRect(
                    x: column * tileSize,
                    y: row * tileSize,
                    width: min(tileSize, image.width - column * tileSize),
                    height: min(tileSize, image.height - row * tileSize)
                )
                guard let tile = image.cropping(to: rect) else { continue }
                let text = await performOCR(on: tile)
                if !text.isEmpty {
                    parts.append(text)
                }
            }
        }
        return parts.joined(separator: "\n")
    }

    private static func downscaled(_ image: CGImage, maxDimension: Int) -> CGImage? {
        let scale = min(1.0, Double(maxDimension) / Double(max(image.width, image.height, 1)))
        if scale >= 1 { return image }
        let width = max(Int(Double(image.width) * scale), 8)
        let height = max(Int(Double(image.height) * scale), 8)
        guard let context = CGContext(
            data: nil,
            width: width,
            height: height,
            bitsPerComponent: 8,
            bytesPerRow: 0,
            space: image.colorSpace ?? CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
        ) else { return nil }
        context.interpolationQuality = .high
        context.draw(image, in: CGRect(x: 0, y: 0, width: width, height: height))
        return context.makeImage()
    }

    /// Flash a frame over the captured area for a moment so the person
    /// always sees what MICE just looked at. Shares the guide-highlight
    /// styling for a consistent visual language.
    private static func flashFrame(cgRect: CGRect) {
        let primaryHeight = CGDisplayBounds(CGMainDisplayID()).height
        let cocoaRect = NSRect(
            x: cgRect.origin.x,
            y: primaryHeight - cgRect.maxY,
            width: cgRect.width,
            height: cgRect.height
        )
        let panel = OverlayController.makeHighlightPanel(
            around: cocoaRect,
            label: "MICE is reading this area",
            pulsing: false
        )
        panel.orderFrontRegardless()
        DispatchQueue.main.asyncAfter(deadline: .now() + 1.2) {
            panel.orderOut(nil)
        }
    }

    private static func sendScreenCaptured(
        sessionID: String,
        error: String? = nil,
        pngBase64: String? = nil,
        ocrText: String? = nil,
        appName: String? = nil,
        windowTitle: String? = nil
    ) {
        var params: [String: Any] = ["sessionId": sessionID]
        if let error { params["captureError"] = error }
        if let pngBase64 { params["pngBase64"] = pngBase64 }
        if let ocrText, !ocrText.isEmpty { params["ocrText"] = ocrText }
        if let appName { params["appName"] = appName }
        if let windowTitle, !windowTitle.isEmpty { params["windowTitle"] = windowTitle }
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "screen.captured",
            "params": params,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func scheduleHover(at point: CGPoint) {
        hoverTask?.cancel()
        hoverTask = Task { @MainActor in
            try? await Task.sleep(nanoseconds: 650_000_000)
            guard !Task.isCancelled else { return }
            sendHoverCaptured(at: point)
        }
    }

    static func cancelHover() {
        hoverTask?.cancel()
        hoverTask = nil
        lastHoverFingerprint = ""
    }

    static func isControlKey(_ event: CGEvent) -> Bool {
        let keyCode = event.getIntegerValueField(.keyboardEventKeycode)
        return keyCode == 59 || keyCode == 62
    }

    static func sendSelectedText(action: String) {
        let selection = selectedText()
        var params: [String: Any] = [
            "sessionId": UUID().uuidString,
            "text": selection.text,
            "source": selection.source,
            "action": action,
        ]
        if let html = selection.html { params["html"] = html }
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "selection.text",
            "params": params,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    /// Smart copy reads the pasteboard exactly once, only when its gesture
    /// fires after the user's own Cmd-C. The pasteboard is never observed
    /// continuously and is only rewritten by an explicit clipboard.set.
    static func sendClipboardCaptured() {
        let pasteboard = NSPasteboard.general
        let sessionID = UUID().uuidString
        // `clipboard.set` intentionally owns only these representations. Do
        // not clear a pasteboard that also contains TIFF, file URLs, custom
        // application data, or multiple items: MICE could not restore those
        // losslessly. Leaving it untouched is safer than a partial rewrite.
        let supportedTypes: Set<NSPasteboard.PasteboardType> = [.string, .html, .rtf, .png]
        let items = pasteboard.pasteboardItems ?? []
        let containsUnsupportedRepresentation = items.contains { item in
            item.types.contains { !supportedTypes.contains($0) }
        }
        if items.count > 1 || containsUnsupportedRepresentation {
            sendClipboardCaptureError(
                sessionID: sessionID,
                message: "This copy has additional rich formats MICE cannot preserve yet; the clipboard was left unchanged."
            )
            return
        }
        var params: [String: Any] = ["sessionId": sessionID]
        if let text = pasteboard.string(forType: .string) {
            params["text"] = text
        }
        if let html = pasteboard.data(forType: .html).flatMap({ String(data: $0, encoding: .utf8) }) {
            params["html"] = html
        }
        if let rtf = pasteboard.data(forType: .rtf) {
            params["rtfBase64"] = rtf.base64EncodedString()
        }
        // Preserve an existing PNG independently, but never relabel TIFF
        // bytes as PNG. Smart Copy does not interpret or transform images.
        if let image = pasteboard.data(forType: .png) {
            guard image.count <= 8 * 1024 * 1024 else {
                sendClipboardCaptureError(
                    sessionID: sessionID,
                    message: "This PNG is too large for Smart Copy to preserve safely; the clipboard was left unchanged."
                )
                return
            }
            params["pngBase64"] = image.base64EncodedString()
        }
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "clipboard.captured",
            "params": params,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        // The Rust IPC reader rejects frames larger than 16 MiB. Do not write
        // an oversized frame and tear down the agent/core session just because
        // the user copied a large document; send a tiny typed failure instead.
        guard data.count <= 16 * 1024 * 1024 else {
            sendClipboardCaptureError(
                sessionID: sessionID,
                message: "Copied content is too large for Smart Copy; the clipboard was left unchanged."
            )
            return
        }
        writeFrame(data)
    }

    static func sendClipboardCaptureError(sessionID: String, message: String) {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "clipboard.captured",
            "params": ["sessionId": sessionID, "captureError": message],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    /// Read Finder's existing selection only after `mice file --finder`.
    /// AppleScript may request macOS Automation permission on first use; MICE
    /// never changes the selection or asks Finder to move anything.
    ///
    /// Finder is deliberately *not* required to be frontmost: running the
    /// command from a terminal always makes the terminal frontmost, and the
    /// CLI still shows the exact file and asks for confirmation before any
    /// move. Finder must merely be running with a selection.
    static func captureFinderSelection(sessionID: String) {
        let finderRunning = NSWorkspace.shared.runningApplications
            .contains { $0.bundleIdentifier == "com.apple.finder" }
        guard finderRunning else {
            sendFinderCaptured(sessionID: sessionID, error: "Finder is not running; open it, select one file, then run `mice file --finder` again.")
            return
        }
        let source = "tell application \"Finder\" to get POSIX path of every item of selection"
        var error: NSDictionary?
        guard let result = NSAppleScript(source: source)?.executeAndReturnError(&error) else {
            let detail = (error?[NSAppleScript.errorMessage] as? String) ?? "Finder did not allow selection access."
            sendFinderCaptured(sessionID: sessionID, error: detail)
            return
        }
        // Trailing whitespace and newlines are legal in macOS filenames, so
        // paths are forwarded exactly as Finder reported them; trimming could
        // silently redirect the move to a different existing file.
        var paths: [String] = []
        if result.numberOfItems > 0 {
            paths = (1...result.numberOfItems).compactMap { result.atIndex($0)?.stringValue }
        } else if let single = result.stringValue, !single.isEmpty {
            paths = [single]
        }
        paths = paths.filter { !$0.isEmpty }
        guard paths.count == 1 else {
            sendFinderCaptured(sessionID: sessionID, error: paths.isEmpty ? "Select one file in Finder first." : "Select exactly one file in Finder first.")
            return
        }
        sendFinderCaptured(sessionID: sessionID, paths: paths)
    }

    static func sendFinderCaptured(sessionID: String, paths: [String] = [], error: String? = nil) {
        var params: [String: Any] = ["sessionId": sessionID, "paths": paths]
        if let error { params["captureError"] = error }
        let payload: [String: Any] = ["jsonrpc": "2.0", "method": "finder.captured", "params": params]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func requestGoal() {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "goal.request",
            "params": ["sessionId": UUID().uuidString],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func stopAutopilot() {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "autopilot.stop",
            "params": [:],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func sendPromptSubmitted(sessionID: String, text: String) {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "prompt.submitted",
            "params": ["sessionId": sessionID, "text": text],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func sendPromptCancelled(sessionID: String) {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "prompt.cancelled",
            "params": ["sessionId": sessionID],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func sendGuideControl(sessionID: String, action: String, value: String? = nil) {
        var params: [String: Any] = ["sessionId": sessionID, "action": action]
        if let value { params["value"] = value }
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "guide.control",
            "params": params,
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func sendOverlayAction(sessionID: String, actionID: String) {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "overlay.action",
            "params": ["sessionId": sessionID, "actionId": actionID],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func rememberPasteDestination() {
        guard let application = NSWorkspace.shared.frontmostApplication,
              application.processIdentifier != ProcessInfo.processInfo.processIdentifier else {
            return
        }
        pasteDestination = application
    }

    /// The overlay is a non-activating panel, so the user's document remains
    /// the key destination. The result was already placed on the pasteboard by
    /// the core; synthesize the same Command-V a person would use to preserve
    /// its text/HTML/RTF representations.
    static func pasteClipboard() {
        // A menu click can temporarily take focus even though the panel itself
        // is non-activating. Prefer the app that is frontmost *when Send to…
        // is chosen, so a person can switch from the source page to their
        // document before sending. Fall back to the app remembered when the
        // result opened if the menu temporarily owns frontmost status.
        let currentDestination = NSWorkspace.shared.frontmostApplication
            .flatMap { app in
                app.processIdentifier == ProcessInfo.processInfo.processIdentifier ? nil : app
            }
        let destination = currentDestination ?? pasteDestination
        destination?.activate(options: [])
        DispatchQueue.main.asyncAfter(deadline: .now() + .milliseconds(150)) {
            if let text = NSPasteboard.general.string(forType: .string),
               (try? AXSupport.insertAtSelection(text)) != nil {
                return
            }
            postPasteShortcut()
        }
    }

    private static func postPasteShortcut() {
        guard let source = CGEventSource(stateID: .combinedSessionState),
              let keyDown = CGEvent(keyboardEventSource: source, virtualKey: 9, keyDown: true),
              let keyUp = CGEvent(keyboardEventSource: source, virtualKey: 9, keyDown: false) else {
            return
        }
        keyDown.flags = .maskCommand
        keyUp.flags = .maskCommand
        keyDown.post(tap: .cghidEventTap)
        keyUp.post(tap: .cghidEventTap)
    }

    static func selectedText() -> (text: String, html: String?, source: String) {
        if let text = try? AXSupport.selectedText(), !text.isEmpty {
            return (text, nil, "ax")
        }

        // Some apps do not expose kAXSelectedTextAttribute. Ask their normal
        // copy command for the selection, then restore every clipboard flavor
        // we observed before the request. This is deliberately a fallback:
        // mouse selection, source formatting, and the user's original
        // clipboard remain under the host app's control.
        let pasteboard = NSPasteboard.general
        let previousClipboard = ClipboardSnapshot(pasteboard)
        defer { previousClipboard.restore(to: pasteboard) }
        guard let source = CGEventSource(stateID: .combinedSessionState),
              let keyDown = CGEvent(keyboardEventSource: source, virtualKey: 8, keyDown: true),
              let keyUp = CGEvent(keyboardEventSource: source, virtualKey: 8, keyDown: false) else {
            return ("", nil, "clipboard")
        }
        keyDown.flags = .maskCommand
        keyUp.flags = .maskCommand
        keyDown.post(tap: .cghidEventTap)
        keyUp.post(tap: .cghidEventTap)
        RunLoop.current.run(until: Date().addingTimeInterval(0.08))
        let text = pasteboard.string(forType: .string) ?? ""
        let html = pasteboard.data(forType: .html).flatMap { String(data: $0, encoding: .utf8) }
        return (text, html, "clipboard")
    }

    static func sendHoverCaptured(at point: CGPoint) {
        // Hover must never repeatedly prompt for Accessibility permission while
        // the pointer moves. The normal status/probe flow remains responsible
        // for explaining missing permission.
        guard MicePermission.accessibility.granted,
              let element = try? AXSupport.element(at: point) else { return }
        let description = AXSupport.describe(element)
        let text = AXSupport.semanticText(element) ?? ""
        let fingerprint = [description.role, description.title, description.description, description.value, description.help]
            .compactMap { $0 }
            .joined(separator: "\u{1F}")
        guard !fingerprint.isEmpty, fingerprint != lastHoverFingerprint else { return }
        lastHoverFingerprint = fingerprint
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "hover.captured",
            "params": [
                "sessionId": UUID().uuidString,
                "ax": [
                    "role": description.role ?? "",
                    "title": description.title ?? "",
                    "description": description.description ?? "",
                    "value": description.value ?? "",
                    "help": description.help ?? "",
                    "actions": description.actions,
                ],
                "text": text,
            ],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func performOCR(on image: CGImage) async -> String {
        return await withCheckedContinuation { continuation in
            let request = VNRecognizeTextRequest { request, error in
                guard error == nil, let observations = request.results as? [VNRecognizedTextObservation] else {
                    continuation.resume(returning: "")
                    return
                }
                let recognizedStrings = observations.compactMap { observation in
                    observation.topCandidates(1).first?.string
                }
                continuation.resume(returning: recognizedStrings.joined(separator: "\n"))
            }
            request.recognitionLevel = .accurate
            let handler = VNImageRequestHandler(cgImage: image, options: [:])
            do {
                try handler.perform([request])
            } catch {
                continuation.resume(returning: "")
            }
        }
    }

    static func imageToBase64(_ image: CGImage) -> String? {
        let mutableData = CFDataCreateMutable(nil, 0)!
        guard let destination = CGImageDestinationCreateWithData(mutableData, UTType.png.identifier as CFString, 1, nil) else { return nil }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination) else { return nil }
        let data = mutableData as Data
        return data.base64EncodedString()
    }

    static func sendSelectionCaptured(pixels: String, text: String, role: String?, title: String?, mode: String) {
        let payload: [String: Any] = [
            "jsonrpc": "2.0",
            "method": "selection.captured",
            "params": [
                "sessionId": UUID().uuidString,
                "mode": mode,
                "pixels": pixels,
                "text": text,
                "ax": [
                    "role": role ?? "",
                    "title": title ?? ""
                ]
            ]
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }
}

@MainActor
private final class OverlayController: NSObject {
    private static weak var active: OverlayController?
    private let panel: NSPanel
    private let scrollView: NSScrollView
    private let textView: NSTextView
    private let buttonRow: NSStackView
    private let captionLabel: NSTextField
    private let imageView: NSImageView
    private var highlightPanels: [NSPanel] = []
    private var currentSessionId: String?

    override init() {
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 480, height: 320), styleMask: [.nonactivatingPanel, .titled, .closable], backing: .buffered, defer: false)
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.title = "MICE"

        scrollView = NSScrollView(frame: NSRect(x: 12, y: 52, width: 456, height: 256))
        scrollView.hasVerticalScroller = true
        scrollView.autohidesScrollers = true
        scrollView.drawsBackground = false
        scrollView.borderType = .noBorder

        textView = NSTextView(frame: NSRect(x: 0, y: 0, width: 456, height: 256))
        textView.isEditable = false
        textView.isSelectable = true
        // Fetch Links is an explicit user action; MICE applies link
        // attributes itself (HTTP/HTTPS only) rather than enabling automatic
        // detection, which would also linkify file:, mailto:, and custom
        // schemes. MICE never follows external MCP links automatically.
        textView.isAutomaticLinkDetectionEnabled = false
        textView.linkTextAttributes = [
            .foregroundColor: NSColor.linkColor,
            .underlineStyle: NSUnderlineStyle.single.rawValue,
        ]
        textView.drawsBackground = false
        textView.font = .systemFont(ofSize: 14)
        textView.textColor = .labelColor
        textView.textContainerInset = NSSize(width: 6, height: 6)
        textView.isVerticallyResizable = true
        textView.isHorizontallyResizable = false
        textView.autoresizingMask = [.width]
        textView.textContainer?.widthTracksTextView = true
        textView.textContainer?.containerSize = NSSize(width: 456, height: CGFloat.greatestFiniteMagnitude)
        scrollView.documentView = textView

        buttonRow = NSStackView(frame: NSRect(x: 12, y: 12, width: 456, height: 30))
        buttonRow.orientation = .horizontal
        buttonRow.alignment = .centerY
        buttonRow.spacing = 8
        buttonRow.isHidden = true

        captionLabel = NSTextField(wrappingLabelWithString: "")
        captionLabel.font = .systemFont(ofSize: 13)
        captionLabel.textColor = .labelColor
        captionLabel.maximumNumberOfLines = 2
        captionLabel.isHidden = true

        imageView = NSImageView(frame: NSRect(x: 16, y: 44, width: 608, height: 560))
        imageView.imageScaling = .scaleProportionallyUpOrDown
        imageView.imageAlignment = .alignCenter
        imageView.isHidden = true

        super.init()

        OverlayController.active = self

        panel.contentView?.addSubview(scrollView)
        panel.contentView?.addSubview(buttonRow)
        panel.contentView?.addSubview(captionLabel)
        panel.contentView?.addSubview(imageView)
    }

    func handle(json: String) {
        guard let data = json.data(using: .utf8),
              let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        guard let method = frame["method"] as? String else { return }
        let params = frame["params"] as? [String: Any] ?? [:]
        switch method {
        case "overlay.show":
            MiceMacAgent.rememberPasteDestination()
            // Position near the cursor only when opening fresh; while already
            // visible (streaming a result) keep the panel where the user put it.
            showText(params["text"] as? String ?? "Working…", positionAtMouse: !panel.isVisible)
        case "overlay.update":
            textView.string = params["text"] as? String ?? "Working…"
            panel.orderFrontRegardless()
        case "overlay.appendResult":
            textView.string += params["chunk"] as? String ?? ""
            trimOverlayTextIfNeeded()
            applyHttpLinkAttributes()
            textView.scrollToEndOfDocument(nil)
        case "overlay.finishResult":
            if let text = params["text"] as? String {
                if imageView.isHidden { textView.string = text } else { captionLabel.stringValue = text }
            }
            applyHttpLinkAttributes()
        case "overlay.result":
            guard let sessionID = params["sessionId"] as? String else { return }
            currentSessionId = sessionID
            showActions((params["actions"] as? [[String: Any]]) ?? [])
        case "overlay.showImage":
            guard let pngBase64 = params["pngBase64"] as? String,
                  let imageData = Data(base64Encoded: pngBase64),
                  let image = NSImage(data: imageData) else { return }
            scrollView.isHidden = true
            buttonRow.isHidden = true
            imageView.image = image
            imageView.isHidden = false
            captionLabel.isHidden = false
            panel.setContentSize(NSSize(width: 640, height: 620))
            imageView.frame = NSRect(x: 16, y: 44, width: 608, height: 560)
            captionLabel.frame = NSRect(x: 16, y: 12, width: 608, height: 28)
            panel.orderFrontRegardless()
        case "overlay.highlight":
            guard let boxes = params["boxes"] as? [[String: Any]] else { return }
            showHighlights(boxes)
        case "overlay.promptInput":
            guard let sessionID = params["sessionId"] as? String,
                  let title = params["title"] as? String,
                  let placeholder = params["placeholder"] as? String else { return }
            showPrompt(
                sessionID: sessionID,
                title: title,
                placeholder: placeholder,
                context: params["context"] as? String
            )
        case "overlay.guideStep":
            guard let sessionID = params["sessionId"] as? String,
                  let stepIndex = params["stepIndex"] as? Int,
                  let totalSteps = params["totalSteps"] as? Int,
                  let instruction = params["instruction"] as? String,
                  let appHint = params["appHint"] as? String,
                  let sensitive = params["sensitive"] as? Bool,
                  let browserCapable = params["browserCapable"] as? Bool else { return }
            showGuideStep(
                sessionID: sessionID,
                stepIndex: stepIndex,
                totalSteps: totalSteps,
                instruction: instruction,
                appHint: appHint,
                sensitive: sensitive,
                browserCapable: browserCapable
            )
        case "clipboard.set":
            guard let text = params["text"] as? String else { return }
            let pasteboard = NSPasteboard.general
            pasteboard.clearContents()
            pasteboard.setString(text, forType: .string)
            if let html = params["html"] as? String,
               let data = html.data(using: .utf8) {
                pasteboard.setData(data, forType: .html)
            }
            if let rtf = params["rtf"] as? String,
               let data = rtf.data(using: .utf8) {
                pasteboard.setData(data, forType: .rtf)
            }
            if let pngBase64 = params["pngBase64"] as? String,
               let data = Data(base64Encoded: pngBase64) {
                pasteboard.setData(data, forType: .png)
            }
        case "screen.capture":
            guard let sessionID = params["sessionId"] as? String,
                  let scope = params["scope"] as? String else { return }
            Task { await MiceMacAgent.captureForVision(sessionID: sessionID, scope: scope) }
        case "finder.capture":
            guard let sessionID = params["sessionId"] as? String else { return }
            MiceMacAgent.captureFinderSelection(sessionID: sessionID)
        case "clipboard.paste":
            MiceMacAgent.pasteClipboard()
        case "overlay.dismiss":
            dismiss()
        case "agent.stop":
            NSApp.terminate(nil)
        default:
            break
        }
    }

    /// Very long results make NSTextView relayout quadratic during streaming
    /// and are unreadable in a floating panel anyway. Keep the live tail; the
    /// complete text still reaches the clipboard when the result finishes.
    private static let maximumOverlayCharacters = 40_000
    private static let overlayTrimNotice =
        "… (earlier text trimmed — the complete result is on the clipboard)\n\n"

    private func trimOverlayTextIfNeeded() {
        let text = textView.string
        guard text.count > Self.maximumOverlayCharacters else { return }
        var tail = String(text.suffix(Self.maximumOverlayCharacters * 3 / 4))
        if tail.hasPrefix(Self.overlayTrimNotice) == false {
            tail = Self.overlayTrimNotice + tail
        }
        textView.string = tail
    }

    /// Attribute only HTTP/HTTPS URLs as clickable links. Foundation's
    /// automatic detection would also linkify file:, mailto:, and custom URL
    /// schemes, which the result panel deliberately never offers.
    private func applyHttpLinkAttributes() {
        guard let storage = textView.textStorage, storage.length > 0 else { return }
        let full = NSRange(location: 0, length: storage.length)
        storage.removeAttribute(.link, range: full)
        guard let detector = try? NSDataDetector(
            types: NSTextCheckingResult.CheckingType.link.rawValue
        ) else { return }
        for match in detector.matches(in: storage.string, options: [], range: full) {
            guard let url = match.url,
                  let scheme = url.scheme?.lowercased(),
                  scheme == "http" || scheme == "https" else { continue }
            storage.addAttribute(.link, value: url, range: match.range)
        }
    }

    func dismiss() {
        panel.orderOut(nil)
    }

    static func dismissActive() {
        active?.dismiss()
    }

    /// Show text in the scrolling result panel, resetting from image mode and
    /// clearing any previous action buttons.
    private func showText(_ text: String, positionAtMouse: Bool) {
        imageView.isHidden = true
        captionLabel.isHidden = true
        clearButtons()
        buttonRow.isHidden = true
        panel.setContentSize(NSSize(width: 480, height: 320))
        scrollView.frame = NSRect(x: 12, y: 52, width: 456, height: 256)
        scrollView.isHidden = false
        textView.string = text
        if positionAtMouse {
            let mouse = NSEvent.mouseLocation
            let frame = panel.frame
            var origin = NSPoint(x: mouse.x + 18, y: mouse.y - frame.height - 18)
            if let screen = NSScreen.main?.visibleFrame {
                origin.x = min(max(origin.x, screen.minX + 8), screen.maxX - frame.width - 8)
                origin.y = min(max(origin.y, screen.minY + 8), screen.maxY - frame.height - 8)
            }
            panel.setFrameOrigin(origin)
        }
        panel.orderFrontRegardless()
    }

    private func showActions(_ actions: [[String: Any]]) {
        clearButtons()
        for action in actions {
            guard let id = action["id"] as? String, let title = action["label"] as? String else { continue }
            let button = NSButton(title: title, target: self, action: #selector(actionButtonClicked(_:)))
            button.bezelStyle = .rounded
            button.identifier = NSUserInterfaceItemIdentifier(id)
            buttonRow.addArrangedSubview(button)
        }
        buttonRow.isHidden = buttonRow.arrangedSubviews.isEmpty
        panel.orderFrontRegardless()
    }

    private func clearButtons() {
        for view in buttonRow.arrangedSubviews {
            buttonRow.removeArrangedSubview(view)
            view.removeFromSuperview()
        }
    }

    @objc private func actionButtonClicked(_ sender: NSButton) {
        guard let id = sender.identifier?.rawValue, let session = currentSessionId else { return }
        if id == "send_to" {
            showSendMenu(from: sender)
            return
        }
        MiceMacAgent.sendOverlayAction(sessionID: session, actionID: id)
    }

    private func showSendMenu(from sender: NSButton) {
        let menu = NSMenu()
        let paste = NSMenuItem(
            title: "Paste into frontmost app",
            action: #selector(sendPasteToFrontmostApp(_:)),
            keyEquivalent: ""
        )
        paste.target = self
        menu.addItem(paste)
        menu.popUp(positioning: paste, at: NSPoint(x: 0, y: sender.bounds.height), in: sender)
    }

    @objc private func sendPasteToFrontmostApp(_ sender: NSMenuItem) {
        guard let session = currentSessionId else { return }
        MiceMacAgent.sendOverlayAction(sessionID: session, actionID: "send_paste")
    }

    private func showPrompt(
        sessionID: String,
        title: String,
        placeholder: String,
        context: String?
    ) {
        let alert = NSAlert()
        alert.messageText = title
        alert.informativeText = context ?? ""
        alert.addButton(withTitle: "Continue")
        alert.addButton(withTitle: "Cancel")
        let field = NSTextField(string: "")
        field.placeholderString = placeholder
        field.frame = NSRect(x: 0, y: 0, width: 420, height: 24)
        alert.accessoryView = field
        NSApp.activate(ignoringOtherApps: true)
        let result = alert.runModal()
        if result == .alertFirstButtonReturn {
            MiceMacAgent.sendPromptSubmitted(sessionID: sessionID, text: field.stringValue)
        } else {
            MiceMacAgent.sendPromptCancelled(sessionID: sessionID)
        }
    }

    private func showGuideStep(
        sessionID: String,
        stepIndex: Int,
        totalSteps: Int,
        instruction: String,
        appHint: String,
        sensitive: Bool,
        browserCapable: Bool
    ) {
        if let bounds = try? AXSupport.matchingBounds(for: "\(appHint) \(instruction)") {
            showHighlights([[
                "bounds": [
                    "x": bounds.origin.x,
                    "y": bounds.origin.y,
                    "width": bounds.width,
                    "height": bounds.height,
                ],
                "instructionText": instruction,
            ]])
        } else {
            showHighlights([])
        }
        let alert = NSAlert()
        alert.messageText = "Step \(stepIndex + 1) of \(totalSteps)"
        alert.informativeText = "\(instruction)\n\nApp: \(appHint)"
            + (sensitive ? "\n\nDo this yourself, then choose Next." : "")
        alert.addButton(withTitle: "Next")
        alert.addButton(withTitle: "Back")
        alert.addButton(withTitle: "Quit")
        NSApp.activate(ignoringOtherApps: true)
        switch alert.runModal() {
        case .alertFirstButtonReturn:
            MiceMacAgent.sendGuideControl(sessionID: sessionID, action: "next")
        case .alertSecondButtonReturn:
            MiceMacAgent.sendGuideControl(sessionID: sessionID, action: "back")
        default:
            MiceMacAgent.sendGuideControl(sessionID: sessionID, action: "quit")
        }
    }

    private func showHighlights(_ boxes: [[String: Any]]) {
        highlightPanels.forEach { $0.orderOut(nil) }
        highlightPanels.removeAll()
        for box in boxes {
            guard let bounds = box["bounds"] as? [String: Any],
                  let x = bounds["x"] as? CGFloat,
                  let y = bounds["y"] as? CGFloat,
                  let width = bounds["width"] as? CGFloat,
                  let height = bounds["height"] as? CGFloat else { continue }
            let panel = OverlayController.makeHighlightPanel(
                around: NSRect(x: x, y: y, width: width, height: height),
                label: (box["instructionText"] as? String) ?? "",
                pulsing: true
            )
            panel.orderFrontRegardless()
            highlightPanels.append(panel)
        }
    }

    /// A rounded, softly glowing target frame with an optional pill label
    /// floating above it. Guide highlights pulse to draw the eye; the capture
    /// flash uses the same styling without the pulse so MICE's "look here"
    /// visuals stay consistent.
    static func makeHighlightPanel(around rect: NSRect, label: String, pulsing: Bool) -> NSPanel {
        let margin: CGFloat = 6
        let labelHeight: CGFloat = label.isEmpty ? 0 : 26
        let labelGap: CGFloat = label.isEmpty ? 0 : 6
        let panelRect = NSRect(
            x: rect.origin.x - margin,
            y: rect.origin.y - margin,
            width: rect.width + margin * 2,
            height: rect.height + margin * 2 + labelHeight + labelGap
        )
        let panel = NSPanel(
            contentRect: panelRect,
            styleMask: [.borderless, .nonactivatingPanel],
            backing: .buffered,
            defer: false
        )
        panel.isOpaque = false
        panel.backgroundColor = .clear
        panel.hasShadow = false
        panel.ignoresMouseEvents = true
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]

        let border = NSView(
            frame: NSRect(x: margin, y: margin, width: rect.width, height: rect.height)
        )
        border.wantsLayer = true
        border.layer?.borderWidth = 2.5
        border.layer?.borderColor = NSColor.systemCyan.cgColor
        border.layer?.cornerRadius = 8
        border.layer?.shadowColor = NSColor.systemCyan.cgColor
        border.layer?.shadowOpacity = 0.55
        border.layer?.shadowRadius = 6
        border.layer?.shadowOffset = .zero
        border.layer?.masksToBounds = false
        panel.contentView?.addSubview(border)
        if pulsing, let layer = border.layer {
            let pulse = CABasicAnimation(keyPath: "opacity")
            pulse.fromValue = 1.0
            pulse.toValue = 0.45
            pulse.duration = 0.8
            pulse.autoreverses = true
            pulse.repeatCount = .infinity
            layer.add(pulse, forKey: "mice-pulse")
        }

        if !label.isEmpty {
            let text = NSTextField(labelWithString: label)
            text.textColor = .white
            text.font = .systemFont(ofSize: 12, weight: .semibold)
            text.lineBreakMode = .byTruncatingTail
            text.sizeToFit()
            let pillWidth = min(text.frame.width + 20, max(panelRect.width - 8, 60))
            let pill = NSView(
                frame: NSRect(
                    x: margin,
                    y: rect.height + margin + labelGap,
                    width: pillWidth,
                    height: labelHeight
                )
            )
            pill.wantsLayer = true
            pill.layer?.backgroundColor = NSColor.black.withAlphaComponent(0.82).cgColor
            pill.layer?.cornerRadius = labelHeight / 2
            text.frame = NSRect(
                x: 10,
                y: (labelHeight - text.frame.height) / 2,
                width: pillWidth - 20,
                height: text.frame.height
            )
            pill.addSubview(text)
            panel.contentView?.addSubview(pill)
        }
        return panel
    }
}

private func readFrameText() throws -> String {
    let input = FileHandle.standardInput
    let header = try readExact(input, count: 4)
    let length = header.withUnsafeBytes { $0.loadUnaligned(as: UInt32.self).littleEndian }
    guard length <= 16 * 1024 * 1024 else { throw CocoaError(.fileReadCorruptFile) }
    let body = try readExact(input, count: Int(length))
    return String(decoding: body, as: UTF8.self)
}

private func readExact(_ input: FileHandle, count: Int) throws -> Data {
    var data = Data()
    while data.count < count {
        guard let chunk = try input.read(upToCount: count - data.count), !chunk.isEmpty else {
            throw CocoaError(.fileReadUnknown)
        }
        data.append(chunk)
    }
    return data
}

private func writeFrame(_ data: Data) {
    var length = UInt32(data.count).littleEndian
    let header = Data(bytes: &length, count: 4)
    try? FileHandle.standardOutput.write(contentsOf: header)
    try? FileHandle.standardOutput.write(contentsOf: data)
}
