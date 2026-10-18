// Synthetic input for UI smoke tests (needs Accessibility permission for
// the calling terminal). Coordinates are global screen points.
//
//   ui-driver window <pid>                      -> x y w h of the pid's first window
//   ui-driver drag x1 y1 x2 y2 [shift]
//   ui-driver click x y [count] [shift]
//   ui-driver rclick x y
//   ui-driver scroll x y lines                  (positive = up)
//   ui-driver move x y
import CoreGraphics
import Foundation

let args = CommandLine.arguments
func num(_ i: Int) -> Double { Double(args[i])! }
let source = CGEventSource(stateID: .hidSystemState)

func post(_ type: CGEventType, _ p: CGPoint, _ button: CGMouseButton = .left, flags: CGEventFlags = [], clicks: Int64 = 1) {
    let e = CGEvent(mouseEventSource: source, mouseType: type, mouseCursorPosition: p, mouseButton: button)!
    e.flags = flags
    e.setIntegerValueField(.mouseEventClickState, value: clicks)
    e.post(tap: .cghidEventTap)
}

switch args[1] {
case "window":
    let pid = Int32(args[2])!
    let list = CGWindowListCopyWindowInfo([.optionAll], kCGNullWindowID) as! [[String: Any]]
    // The largest on-screen normal window of the process.
    var best: [String: Double]? = nil
    for w in list where (w[kCGWindowOwnerPID as String] as? Int32) == pid && (w[kCGWindowLayer as String] as? Int) == 0
        && (w[kCGWindowIsOnscreen as String] as? Bool) == true {
        let b = w[kCGWindowBounds as String] as! [String: Double]
        if best == nil || b["Width"]! * b["Height"]! > best!["Width"]! * best!["Height"]! { best = b }
    }
    guard let b = best else { exit(1) }
    print(Int(b["X"]!), Int(b["Y"]!), Int(b["Width"]!), Int(b["Height"]!))
    exit(0)
case "move":
    post(.mouseMoved, CGPoint(x: num(2), y: num(3)))
case "drag":
    let flags: CGEventFlags = args.count > 6 ? .maskShift : []
    let a = CGPoint(x: num(2), y: num(3)), b = CGPoint(x: num(4), y: num(5))
    post(.mouseMoved, a)
    usleep(50_000)
    post(.leftMouseDown, a, flags: flags)
    for i in 1...20 {
        let t = Double(i) / 20
        post(.leftMouseDragged, CGPoint(x: a.x + (b.x - a.x) * t, y: a.y + (b.y - a.y) * t), flags: flags)
        usleep(15_000)
    }
    post(.leftMouseUp, b, flags: flags)
case "click":
    let p = CGPoint(x: num(2), y: num(3))
    let count = args.count > 4 ? Int64(args[4])! : 1
    let flags: CGEventFlags = args.count > 5 ? .maskShift : []
    post(.mouseMoved, p)
    usleep(30_000)
    for n in 1...count {
        post(.leftMouseDown, p, flags: flags, clicks: n)
        usleep(20_000)
        post(.leftMouseUp, p, flags: flags, clicks: n)
        usleep(60_000)
    }
case "rclick":
    let p = CGPoint(x: num(2), y: num(3))
    post(.mouseMoved, p)
    usleep(30_000)
    post(.rightMouseDown, p, .right)
    usleep(20_000)
    post(.rightMouseUp, p, .right)
case "scroll":
    post(.mouseMoved, CGPoint(x: num(2), y: num(3)))
    usleep(30_000)
    let lines = Int32(args[4])!
    for _ in 0..<abs(lines) {
        let e = CGEvent(scrollWheelEvent2Source: source, units: .line, wheelCount: 1, wheel1: lines > 0 ? 1 : -1, wheel2: 0, wheel3: 0)!
        e.post(tap: .cghidEventTap)
        usleep(10_000)
    }
default:
    fputs("unknown command\n", stderr)
    exit(2)
}
usleep(50_000)
