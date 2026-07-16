import Foundation
import MiceMacSupport

@main
struct AXTest {
    static func main() {
        do {
            let description = AXSupport.describe(try AXSupport.elementAtCursor())
            print("role: \(description.role ?? "<none>")")
            print("title: \(description.title ?? "<none>")")
            print("value: \(description.value ?? "<none>")")
            print("help: \(description.help ?? "<none>")")
            print("actions: \(description.actions.joined(separator: ", "))")
        } catch {
            fputs("ax-test: \(error.localizedDescription)\n", stderr)
            exit(1)
        }
    }
}
