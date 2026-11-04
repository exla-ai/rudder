import PhotosUI
import UIKit
import UniformTypeIdentifiers

/// The same cap the control plane puts on uploads from the app.
private let maxPickBytes = 25 * 1024 * 1024

/// One file the user picked, read into memory for the web app.
struct PickedFile: Sendable {
    let name: String
    let type: String
    let data: Data
}

/// What a pick produced: the files, plus a line for each one left out (too
/// big, unreadable) so the app can say why.
struct PickResult: Sendable {
    var files: [PickedFile] = []
    var skipped: [String] = []
}

/// The native pickers behind the composer's attach menu: Photos, Camera, Files.
///
/// WKWebView's own `<input type=file>` cannot open the photo library directly:
/// anything that accepts images first shows WebKit's menu of Photo Library,
/// Take Photo and Choose Files, a second menu after the app's own. So the web
/// app sends `{type: "pick", kind, id}` on the `rudder` handler and gets the
/// files back as base64 through `window.rudderPicked`. The photo library
/// picker runs out of process and needs no permission; the camera uses
/// `NSCameraUsageDescription`.
@MainActor
final class AttachmentPicker: NSObject {
    private var completion: ((PickResult) -> Void)?

    func present(kind: String, from presenter: UIViewController, completion: @escaping (PickResult) -> Void) {
        // A picker that never reported back answers empty before the next one.
        finish(PickResult())
        self.completion = completion
        switch kind {
        case "photos":
            var configuration = PHPickerConfiguration()
            configuration.filter = .images
            configuration.selection = .ordered
            configuration.selectionLimit = 10
            // HEIC arrives as JPEG, which the agents can read.
            configuration.preferredAssetRepresentationMode = .compatible
            let picker = PHPickerViewController(configuration: configuration)
            picker.delegate = self
            presenter.present(picker, animated: true)
        case "camera":
            guard UIImagePickerController.isSourceTypeAvailable(.camera) else {
                finish(PickResult(skipped: ["This device has no camera"]))
                return
            }
            let picker = UIImagePickerController()
            picker.sourceType = .camera
            picker.delegate = self
            presenter.present(picker, animated: true)
        default:
            let picker = UIDocumentPickerViewController(forOpeningContentTypes: [.item], asCopy: true)
            picker.allowsMultipleSelection = true
            picker.delegate = self
            presenter.present(picker, animated: true)
        }
    }

    private func finish(_ result: PickResult) {
        guard let completion else { return }
        self.completion = nil
        completion(result)
    }
}

extension AttachmentPicker: @preconcurrency PHPickerViewControllerDelegate {
    func picker(_ picker: PHPickerViewController, didFinishPicking results: [PHPickerResult]) {
        picker.dismiss(animated: true)
        let providers = results.map(\.itemProvider)
        if providers.isEmpty {
            finish(PickResult())
            return
        }
        PickLoader.photos(providers) { [weak self] result in
            self?.finish(result)
        }
    }
}

extension AttachmentPicker: @preconcurrency UIDocumentPickerDelegate {
    func documentPicker(_ controller: UIDocumentPickerViewController, didPickDocumentsAt urls: [URL]) {
        Task.detached(priority: .userInitiated) { [weak self] in
            let result = PickLoader.files(urls)
            await self?.finish(result)
        }
    }

    func documentPickerWasCancelled(_ controller: UIDocumentPickerViewController) {
        finish(PickResult())
    }
}

extension AttachmentPicker: @preconcurrency UIImagePickerControllerDelegate, @preconcurrency UINavigationControllerDelegate {
    func imagePickerController(
        _ picker: UIImagePickerController,
        didFinishPickingMediaWithInfo info: [UIImagePickerController.InfoKey: Any]
    ) {
        picker.dismiss(animated: true)
        guard let image = info[.originalImage] as? UIImage,
              let data = image.jpegData(compressionQuality: 0.85) else {
            finish(PickResult(skipped: ["Could not read the photo"]))
            return
        }
        if data.count > maxPickBytes {
            finish(PickResult(skipped: ["The photo is over 25 MB"]))
            return
        }
        finish(PickResult(files: [PickedFile(name: "photo.jpg", type: "image/jpeg", data: data)]))
    }

    func imagePickerControllerDidCancel(_ picker: UIImagePickerController) {
        picker.dismiss(animated: true)
        finish(PickResult())
    }
}

/// Reading picked files, off the main thread.
enum PickLoader {
    /// Loads each photo's file representation (in the order the user tapped
    /// them) and calls `done` on the main actor once all of them are read.
    static func photos(_ providers: [NSItemProvider], done: @escaping @MainActor @Sendable (PickResult) -> Void) {
        let collected = Collected(count: providers.count)
        let group = DispatchGroup()
        for (index, provider) in providers.enumerated() {
            let base = provider.suggestedName ?? "photo-\(index + 1)"
            let type = provider.registeredTypeIdentifiers.lazy
                .compactMap { UTType($0) }
                .first { $0.conforms(to: .image) }
            guard let type else {
                collected.add(index, .skipped("Could not read \(base)"))
                continue
            }
            let identifier = type.identifier
            let fallbackExtension = type.preferredFilenameExtension ?? "jpg"
            let mime = type.preferredMIMEType ?? "image/jpeg"
            group.enter()
            _ = provider.loadFileRepresentation(forTypeIdentifier: identifier) { url, _ in
                // The temporary copy is deleted when this handler returns.
                let ext = (url?.pathExtension).flatMap { $0.isEmpty ? nil : $0 } ?? fallbackExtension
                collected.add(index, PickLoader.read(url, name: PickLoader.fileName(base, ext: ext), mime: mime))
                group.leave()
            }
        }
        group.notify(queue: .main) {
            let result = collected.result
            MainActor.assumeIsolated { done(result) }
        }
    }

    /// Reads documents the picker copied into the app's temporary folder, then
    /// removes the copies.
    static func files(_ urls: [URL]) -> PickResult {
        var result = PickResult()
        for url in urls {
            let mime = UTType(filenameExtension: url.pathExtension)?.preferredMIMEType ?? "application/octet-stream"
            switch read(url, name: url.lastPathComponent, mime: mime) {
            case .file(let file): result.files.append(file)
            case .skipped(let why): result.skipped.append(why)
            }
            try? FileManager.default.removeItem(at: url)
        }
        return result
    }

    enum Outcome: Sendable {
        case file(PickedFile)
        case skipped(String)
    }

    static func read(_ url: URL?, name: String, mime: String) -> Outcome {
        guard let url else { return .skipped("Could not read \(name)") }
        let size = (try? url.resourceValues(forKeys: [.fileSizeKey]))?.fileSize ?? 0
        if size > maxPickBytes { return .skipped("\(name) is over 25 MB") }
        guard let data = try? Data(contentsOf: url) else { return .skipped("Could not read \(name)") }
        if data.count > maxPickBytes { return .skipped("\(name) is over 25 MB") }
        return .file(PickedFile(name: name, type: mime, data: data))
    }

    static func fileName(_ base: String, ext: String) -> String {
        if base.lowercased().hasSuffix("." + ext.lowercased()) { return base }
        return "\(base).\(ext)"
    }

    /// Photos load on background queues in any order; slots keep the order
    /// the user picked them in.
    final class Collected: @unchecked Sendable {
        private let lock = NSLock()
        private var slots: [PickedFile?]
        private var skipped: [String] = []

        init(count: Int) {
            slots = Array(repeating: nil, count: count)
        }

        func add(_ index: Int, _ outcome: Outcome) {
            lock.withLock {
                switch outcome {
                case .file(let file): slots[index] = file
                case .skipped(let why): skipped.append(why)
                }
            }
        }

        var result: PickResult {
            lock.withLock { PickResult(files: slots.compactMap { $0 }, skipped: skipped) }
        }
    }
}
