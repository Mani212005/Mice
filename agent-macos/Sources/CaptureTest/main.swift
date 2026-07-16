import AppKit
import ImageIO
import MiceMacSupport
@preconcurrency import ScreenCaptureKit
import UniformTypeIdentifiers

@main
@MainActor
struct CaptureTest {
    static func main() async {
        do {
            try MicePermission.screenRecording.require()
            let started = ContinuousClock.now
            let content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
            guard let display = content.displays.first else { throw CaptureError.noDisplay }
            let mouse = NSEvent.mouseLocation
            let width: CGFloat = 400
            let height: CGFloat = 300
            let frame = display.frame
            let x = min(max(mouse.x - frame.origin.x - width / 2, 0), frame.width - width)
            let y = min(max(frame.maxY - mouse.y - height / 2, 0), frame.height - height)
            let filter = SCContentFilter(display: display, excludingWindows: [])
            let configuration = SCStreamConfiguration()
            configuration.width = Int(width)
            configuration.height = Int(height)
            configuration.sourceRect = CGRect(x: x, y: y, width: width, height: height)
            let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: configuration)
            try writePNG(image, to: URL(fileURLWithPath: "/tmp/mice-capture.png"))
            let elapsed = started.duration(to: .now)
            print("Captured /tmp/mice-capture.png in \(elapsed.components.seconds * 1_000) ms")
        } catch {
            fputs("capture-test: \(error.localizedDescription)\n", stderr)
            exit(1)
        }
    }

    static func writePNG(_ image: CGImage, to url: URL) throws {
        guard let destination = CGImageDestinationCreateWithURL(url as CFURL, UTType.png.identifier as CFString, 1, nil) else {
            throw CaptureError.destination
        }
        CGImageDestinationAddImage(destination, image, nil)
        guard CGImageDestinationFinalize(destination) else { throw CaptureError.destination }
    }
}

enum CaptureError: LocalizedError { case noDisplay, destination
    var errorDescription: String? { self == .noDisplay ? "No display available for capture." : "Could not write PNG." }
}
