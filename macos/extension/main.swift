import FSKit
import Foundation

@_silgen_name("xetfs_unary_new")
private func xetfsUnaryNew() -> UnsafeMutableRawPointer

final class XetFSDelegate: FSUnaryFileSystem, FSUnaryFileSystemOperations {
    private let backend: any FSUnaryFileSystemOperations

    override init() {
        let object = Unmanaged<FSUnaryFileSystem>.fromOpaque(xetfsUnaryNew()).takeRetainedValue()
        backend = object as! any FSUnaryFileSystemOperations
        super.init()
    }

    func probeResource(resource: FSResource, replyHandler reply: @escaping (FSProbeResult?, (any Error)?) -> Void) {
        backend.probeResource(resource: resource, replyHandler: reply)
    }

    func loadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping (FSVolume?, (any Error)?) -> Void) {
        backend.loadResource(resource: resource, options: options) { [weak self] volume, error in
            if error == nil { self?.containerStatus = .ready }
            reply(volume, error)
        }
    }

    func unloadResource(resource: FSResource, options: FSTaskOptions, replyHandler reply: @escaping ((any Error)?) -> Void) {
        backend.unloadResource(resource: resource, options: options) { [weak self] error in
            if error == nil { self?.containerStatus = .notReady(status: POSIXError(.ENXIO)) }
            reply(error)
        }
    }
}

@main
struct XetFSExtension: UnaryFileSystemExtension {
    var fileSystem = XetFSDelegate()
}
