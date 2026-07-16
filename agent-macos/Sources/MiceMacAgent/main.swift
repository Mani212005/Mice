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

@main
@MainActor
struct MiceMacAgent {
    private static var hoverTask: Task<Void, Never>?
    private static var lastHoverFingerprint = ""
    private static var eventTap: CFMachPort?

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
                    if gestureTrigger.matches(event) {
                        Task {
                            await MiceMacAgent.triggerCapture()
                        }
                        return nil
                    }
                }
                if type == .mouseMoved {
                    let point = event.location
                    Task { @MainActor in
                        if event.flags.contains(.maskControl) {
                            MiceMacAgent.scheduleHover(at: point)
                        } else {
                            MiceMacAgent.cancelHover()
                        }
                    }
                }
                if type == .flagsChanged {
                    Task { @MainActor in
                        if event.flags.contains(.maskControl) {
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
private final class OverlayController {
    private let panel: NSPanel
    private let label: NSTextField
    private let imageView: NSImageView
    private var highlightPanels: [NSPanel] = []

    init() {
        panel = NSPanel(contentRect: NSRect(x: 0, y: 0, width: 420, height: 140), styleMask: [.nonactivatingPanel, .titled], backing: .buffered, defer: false)
        panel.isFloatingPanel = true
        panel.level = .floating
        panel.collectionBehavior = [.canJoinAllSpaces, .fullScreenAuxiliary]
        panel.title = "MICE"
        label = NSTextField(wrappingLabelWithString: "")
        label.font = .systemFont(ofSize: 14)
        label.maximumNumberOfLines = 6
        label.frame = NSRect(x: 16, y: 16, width: 388, height: 100)
        panel.contentView?.addSubview(label)
        imageView = NSImageView(frame: NSRect(x: 16, y: 54, width: 608, height: 500))
        imageView.imageScaling = .scaleProportionallyUpOrDown
        imageView.imageAlignment = .alignCenter
        imageView.isHidden = true
        panel.contentView?.addSubview(imageView)
    }

    func handle(json: String) {
        guard let data = json.data(using: .utf8),
              let frame = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return }
        guard let method = frame["method"] as? String else { return }
        let params = frame["params"] as? [String: Any] ?? [:]
        switch method {
        case "overlay.show":
            imageView.isHidden = true
            panel.setContentSize(NSSize(width: 420, height: 140))
            label.frame = NSRect(x: 16, y: 16, width: 388, height: 100)
            label.maximumNumberOfLines = 6
            label.stringValue = params["text"] as? String ?? "Working…"
            let mouse = NSEvent.mouseLocation
            panel.setFrameOrigin(NSPoint(x: mouse.x + 18, y: mouse.y - 158))
            panel.orderFrontRegardless()
        case "overlay.appendResult":
            label.stringValue += params["chunk"] as? String ?? ""
        case "overlay.finishResult":
            if let text = params["text"] as? String { label.stringValue = text }
        case "overlay.showImage":
            guard let pngBase64 = params["pngBase64"] as? String,
                  let imageData = Data(base64Encoded: pngBase64),
                  let image = NSImage(data: imageData) else { return }
            imageView.image = image
            imageView.isHidden = false
            label.frame = NSRect(x: 16, y: 568, width: 608, height: 36)
            label.maximumNumberOfLines = 2
            panel.setContentSize(NSSize(width: 640, height: 620))
            panel.orderFrontRegardless()
        case "overlay.highlight":
            guard let boxes = params["boxes"] as? [[String: Any]] else { return }
            showHighlights(boxes)
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
