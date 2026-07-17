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
        if let tap = CGEvent.tapCreate(
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
                ],
            ],
        ]
        guard let data = try? JSONSerialization.data(withJSONObject: payload) else { return }
        writeFrame(data)
    }

    static func triggerCapture() async {
        await captureRegion(centeredAt: NSEvent.mouseLocation, mode: "prompt")
    }

    static func captureRegion(centeredAt mouse: CGPoint, mode: String) async {
        do {
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            guard let display = content.displays.first else { return }
            let width: CGFloat = 400
            let height: CGFloat = 300
            let frame = display.frame
            let x = min(max(mouse.x - frame.origin.x - width / 2, 0), frame.width - width)
            let y = min(max(frame.maxY - mouse.y - height / 2, 0), frame.height - height)
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
            // Position near the cursor only when opening fresh; while already
            // visible (streaming a result) keep the panel where the user put it.
            showText(params["text"] as? String ?? "Working…", positionAtMouse: !panel.isVisible)
        case "overlay.update":
            textView.string = params["text"] as? String ?? "Working…"
            panel.orderFrontRegardless()
        case "overlay.appendResult":
            textView.string += params["chunk"] as? String ?? ""
            textView.scrollToEndOfDocument(nil)
        case "overlay.finishResult":
            if let text = params["text"] as? String {
                if imageView.isHidden { textView.string = text } else { captionLabel.stringValue = text }
            }
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
        case "overlay.dismiss":
            panel.orderOut(nil)
        case "agent.stop":
            NSApp.terminate(nil)
        default:
            break
        }
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
        MiceMacAgent.sendOverlayAction(sessionID: session, actionID: id)
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
        if browserCapable { alert.addButton(withTitle: "Do it") }
        alert.addButton(withTitle: "Quit")
        NSApp.activate(ignoringOtherApps: true)
        switch alert.runModal() {
        case .alertFirstButtonReturn:
            MiceMacAgent.sendGuideControl(sessionID: sessionID, action: "next")
        case .alertSecondButtonReturn:
            MiceMacAgent.sendGuideControl(sessionID: sessionID, action: "back")
        case .alertThirdButtonReturn where browserCapable:
            let preview = NSAlert()
            let needsText = ["type", "enter", "write", "fill"].contains { instruction.lowercased().contains($0) }
            preview.messageText = "Confirm browser action"
            preview.informativeText = needsText ? "MICE will type only text you provide. This performs one action only." : "MICE will click the highlighted browser control. This performs one action only."
            preview.addButton(withTitle: "Confirm")
            preview.addButton(withTitle: "Cancel")
            let field = NSTextField(string: "")
            if needsText { field.placeholderString = "Text to type (never stored)"; field.frame = NSRect(x: 0, y: 0, width: 320, height: 24); preview.accessoryView = field }
            if preview.runModal() == .alertFirstButtonReturn {
                MiceMacAgent.sendGuideControl(sessionID: sessionID, action: needsText ? "do-it-fill" : "do-it", value: needsText ? field.stringValue : nil)
            } else {
                MiceMacAgent.sendGuideControl(sessionID: sessionID, action: "stay")
            }
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
            let highlight = NSPanel(
                contentRect: NSRect(x: x, y: y, width: width, height: height),
                styleMask: [.borderless, .nonactivatingPanel],
                backing: .buffered,
                defer: false
            )
            highlight.isOpaque = false
            highlight.backgroundColor = .clear
            highlight.hasShadow = false
            highlight.ignoresMouseEvents = true
            highlight.level = .floating
            highlight.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
            let border = NSView(frame: highlight.contentView?.bounds ?? .zero)
            border.wantsLayer = true
            border.layer?.borderWidth = 3
            border.layer?.borderColor = NSColor.systemCyan.cgColor
            border.layer?.cornerRadius = 6
            border.autoresizingMask = [.width, .height]
            highlight.contentView?.addSubview(border)
            if let instruction = box["instructionText"] as? String, !instruction.isEmpty {
                let text = NSTextField(labelWithString: instruction)
                text.textColor = .black
                text.backgroundColor = .systemCyan
                text.font = .systemFont(ofSize: 12, weight: .semibold)
                text.sizeToFit()
                text.frame.origin = NSPoint(x: 4, y: max(height - text.frame.height - 4, 0))
                highlight.contentView?.addSubview(text)
            }
            highlight.orderFrontRegardless()
            highlightPanels.append(highlight)
        }
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
