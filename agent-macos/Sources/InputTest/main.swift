import ApplicationServices
import CoreFoundation
import MiceMacSupport

@main
struct InputTest {
    static func main() {
        do {
            try MicePermission.inputMonitoring.require()
            let mask = (1 << CGEventType.mouseMoved.rawValue) | (1 << CGEventType.leftMouseDown.rawValue) | (1 << CGEventType.rightMouseDown.rawValue) | (1 << CGEventType.keyDown.rawValue)
            guard let tap = CGEvent.tapCreate(tap: .cgSessionEventTap, place: .headInsertEventTap, options: .listenOnly, eventsOfInterest: CGEventMask(mask), callback: callback, userInfo: nil) else {
                throw InputError.tap
            }
            let source = CFMachPortCreateRunLoopSource(kCFAllocatorDefault, tap, 0)
            CFRunLoopAddSource(CFRunLoopGetCurrent(), source, .commonModes)
            CGEvent.tapEnable(tap: tap, enable: true)
            print("Listening for global mouse and keyboard events. Press Ctrl-C to stop.")
            CFRunLoopRun()
        } catch {
            fputs("input-test: \(error.localizedDescription)\n", stderr)
            exit(1)
        }
    }
}

private func callback(_ proxy: CGEventTapProxy, _ type: CGEventType, _ event: CGEvent, _ userInfo: UnsafeMutableRawPointer?) -> Unmanaged<CGEvent>? {
    print("event type=\(type.rawValue) location=\(event.location)")
    return Unmanaged.passUnretained(event)
}

enum InputError: LocalizedError { case tap
    var errorDescription: String? { "Could not create an Input Monitoring event tap." }
}
