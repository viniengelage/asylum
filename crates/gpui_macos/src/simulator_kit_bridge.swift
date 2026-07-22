import AppKit
import Foundation

extension NSObject {
    @_silgen_name("$s12SimulatorKit14SimDisplayViewC7connect6screen6inputsyAA0C12DeviceScreenC_AC0j5InputI0VtKFTj")
    fileprivate func connectSimulatorKit(
        screen: AnyObject,
        inputs: UnsafePointer<UInt>
    ) throws

    @_silgen_name("$s12SimulatorKit14SimDisplayViewC11beginResizeyyFTj")
    fileprivate func beginResizeSimulatorKit()

    @_silgen_name("$s12SimulatorKit14SimDisplayViewC8resizeTo4sizeySo6CGSizeV_tFTj")
    fileprivate func resizeSimulatorKit(size: CGSize)



    @_silgen_name("$s12SimulatorKit14SimDisplayViewC9endResizeyyFTj")
    fileprivate func endResizeSimulatorKit()
}

@_cdecl("zed_simulator_kit_connect")
public func zedSimulatorKitConnect(
    _ displayViewPointer: UnsafeMutableRawPointer?,
    _ deviceScreenPointer: UnsafeMutableRawPointer?
) -> Int32 {
    guard let displayViewPointer, let deviceScreenPointer else {
        return 1
    }

    let displayView = Unmanaged<NSObject>
        .fromOpaque(displayViewPointer)
        .takeUnretainedValue()
    let deviceScreen = Unmanaged<AnyObject>
        .fromOpaque(deviceScreenPointer)
        .takeUnretainedValue()

    var enabled_inputs: UInt = 0b111

    do {
        try withUnsafePointer(to: &enabled_inputs) { inputs in
            try displayView.connectSimulatorKit(screen: deviceScreen, inputs: inputs)
        }
        return 0
    } catch {
        return 2
    }
}



/// Resize result codes shared with `simulator_kit.rs`.
///
/// 0: resize applied; 1: invalid display view; 2: display view has no
/// intrinsic size yet (no frame received from the device); 3: SimulatorKit
/// accepted the resize but the view did not adopt the requested size, so the
/// simulator may still be rendering at its previous resolution.
@_cdecl("zed_simulator_kit_resize")
public func zedSimulatorKitResize(
    _ displayViewPointer: UnsafeMutableRawPointer?,
    _ width: Double,
    _ height: Double
) -> Int32 {
    guard let displayViewPointer else {
        return 1
    }

    let displayView = Unmanaged<NSObject>
        .fromOpaque(displayViewPointer)
        .takeUnretainedValue()
    guard let view = displayView as? NSView else {
        return 1
    }

    let intrinsicSize = view.intrinsicContentSize
    guard intrinsicSize.width > 0, intrinsicSize.height > 0 else {
        return 2
    }

    let scale = min(width / intrinsicSize.width, height / intrinsicSize.height)
    let requestedSize = CGSize(
        width: intrinsicSize.width * scale,
        height: intrinsicSize.height * scale
    )
    displayView.beginResizeSimulatorKit()
    displayView.resizeSimulatorKit(size: requestedSize)
    displayView.endResizeSimulatorKit()
    view.layoutSubtreeIfNeeded()

    let resultingIntrinsicSize = view.intrinsicContentSize
    let resultingFrameSize = view.frame.size
    let adopted = { (size: CGSize) in
        abs(size.width - requestedSize.width) <= 1 && abs(size.height - requestedSize.height) <= 1
    }
    guard adopted(resultingIntrinsicSize) || adopted(resultingFrameSize) else {
        return 3
    }
    return 0
}
