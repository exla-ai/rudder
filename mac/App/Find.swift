import AppKit
import SwiftUI
import WebKit

/// Edit > Find for the page in the main window, the way Safari finds.
///
/// WebKit's own find (`WKWebView.find`) selects the match, scrolls it into
/// view and paints its yellow find highlight. It starts at the current match,
/// so typing more of the query keeps the match and Find Next moves on. While
/// the bar is open, text the page made unselectable (the sidebar's rows) is
/// selectable, so WebKit can select a match there and step past it.
///
/// WebKit's public find has no count, so "3 of 12" comes from the page's own
/// `window.find`, the same WebKit search: one script walks every match from
/// the top, notes which one is selected, then puts the selection and every
/// scroll position back before the page draws again.
///
/// Inside the Files editor the page keeps Cmd+F and Cmd+G for CodeMirror's own
/// search, which reaches lines the editor has not drawn.
@MainActor
@Observable
final class PageFind {
    enum Status: Equatable {
        case idle
        case notFound
        /// `index` is 0 when the selection is not one of the counted matches
        /// (the bar reopened after a click elsewhere, or a match in a frame).
        case found(index: Int, total: Int)
        case failed(String)
    }

    /// Counting stops here: more reads as "1,000+".
    static let countLimit = 1000

    private(set) var isVisible = false
    private(set) var query = ""
    private(set) var status: Status = .idle
    /// The field takes the cursor, with the query selected, on its next update.
    private(set) var wantsFocus = false

    @ObservationIgnored weak var webView: WKWebView?
    /// Each search's number: the answer to an older one is dropped.
    @ObservationIgnored private var generation = 0

    private enum Move { case stay, typed, next, previous }

    /// Find… (Cmd+F): opens the bar, or puts the cursor back in it, with the
    /// query selected. A new bar starts from the Mac's shared find text.
    func show() {
        guard webView != nil else { return NSSound.beep() }
        if query.isEmpty, let shared = NSPasteboard(name: .find).string(forType: .string) {
            query = shared
        }
        isVisible = true
        wantsFocus = true
        search(.stay)
    }

    func focusHandled() {
        wantsFocus = false
    }

    /// Done or Esc: the match stays selected and the page has the keyboard again.
    func close() {
        guard isVisible else { return }
        isVisible = false
        wantsFocus = false
        generation += 1
        status = .idle
        if !query.isEmpty { Self.share(query) }
        guard let webView else { return }
        webView.window?.makeFirstResponder(webView)
        // WebKit keeps its find highlight until a search misses and has no
        // public call to hide it, so search once for text no page shows. The
        // miss also drops the selection: the match is put back, as Safari leaves it.
        let current = generation
        Task {
            do {
                _ = try await webView.callAsyncJavaScript(Self.keepScript, contentWorld: .defaultClient)
                _ = try await webView.find(Self.unmatchable)
                // Reopened meanwhile: the new search owns the selection and the page.
                guard current == generation else { return }
                _ = try await webView.callAsyncJavaScript(Self.restoreScript, contentWorld: .defaultClient)
            } catch {
                guard current == generation else { return }
                status = .failed(error.localizedDescription)
            }
        }
    }

    /// Typing in the field: find from the current match, as Safari does.
    func edit(_ text: String) {
        query = text
        search(.typed)
    }

    /// Find Next (Cmd+G, Return). A closed bar opens again, as in Safari;
    /// with nothing to find yet it opens for typing.
    func next() {
        step(.next)
    }

    /// Find Previous (Shift+Cmd+G, Shift+Return).
    func previous() {
        step(.previous)
    }

    private func step(_ move: Move) {
        guard !query.isEmpty else { return show() }
        guard webView != nil else { return NSSound.beep() }
        isVisible = true
        search(move)
    }

    /// Use Selection for Find (Cmd+E): the selected text becomes the query and
    /// the Mac's shared find text, without moving off it.
    func useSelection() {
        guard let webView else { return NSSound.beep() }
        Task {
            do {
                let result = try await webView.callAsyncJavaScript(Self.selectedTextScript, contentWorld: .defaultClient)
                guard let text = result as? String else {
                    preconditionFailure("the selected text script returned \(String(describing: result))")
                }
                guard !text.isEmpty else {
                    NSSound.beep()
                    return
                }
                query = text
                Self.share(text)
                if isVisible { search(.stay) }
            } catch {
                status = .failed(error.localizedDescription)
            }
        }
    }

    private func search(_ move: Move) {
        generation += 1
        let current = generation
        let query = self.query
        guard let webView else { return }
        Task {
            do {
                _ = try await webView.callAsyncJavaScript(Self.prepareScript, arguments: ["css": Self.css], contentWorld: .defaultClient)
                guard !query.isEmpty else {
                    if current == generation { status = .idle }
                    return
                }
                if move != .stay {
                    let configuration = WKFindConfiguration()
                    configuration.backwards = move == .previous
                    configuration.wraps = true
                    let result = try await webView.find(query, configuration: configuration)
                    guard current == generation else { return }
                    guard result.matchFound else {
                        status = .notFound
                        return
                    }
                    // Counting walks the whole page: wait for a pause in typing.
                    if move == .typed {
                        try await Task.sleep(for: .milliseconds(150))
                        guard current == generation else { return }
                    }
                }
                let counted = try await webView.callAsyncJavaScript(
                    Self.countScript,
                    arguments: ["q": query, "limit": Self.countLimit],
                    contentWorld: .defaultClient
                )
                guard current == generation else { return }
                guard let pair = counted as? [NSNumber], pair.count == 2 else {
                    preconditionFailure("the count script returned \(String(describing: counted))")
                }
                let total = pair[0].intValue
                // WebKit found a match the page's own search cannot see (a frame): no count.
                status = total == 0 && move == .stay ? .notFound : .found(index: pair[1].intValue, total: total)
            } catch {
                guard current == generation else { return }
                status = .failed(error.localizedDescription)
            }
        }
    }

    /// The bar's "3 of 12".
    var statusText: String? {
        switch status {
        case .idle:
            return nil
        case .notFound:
            return "Not found"
        case .found(_, 0):
            return nil
        case .found(let index, let total):
            let count = total >= Self.countLimit ? "\(Self.countLimit.formatted())+" : total.formatted()
            if index > 0 { return "\(index.formatted()) of \(count)" }
            return total == 1 ? "1 match" : "\(count) matches"
        case .failed:
            return "Find failed"
        }
    }

    /// The find pasteboard: Find in another app starts from this text, and the other way round.
    private static func share(_ text: String) {
        let pasteboard = NSPasteboard(name: .find)
        pasteboard.clearContents()
        pasteboard.setString(text, forType: .string)
    }

    /// Control characters: never rendered text, so a search for them misses.
    private static let unmatchable = "\u{1}\u{2}"

    // MARK: Page scripts (run in their own JavaScript world)

    /// While the bar is open all text is selectable, so WebKit can select a
    /// match anywhere; a match it cannot select would stop Find Next on it.
    private static let css = """
    html[data-rudder-find] * { -webkit-user-select: text !important; user-select: text !important; }
    """

    private static let prepareScript = """
    if (!document.getElementById("rudder-find")) {
      const style = document.createElement("style");
      style.id = "rudder-find";
      style.textContent = css;
      document.head.append(style);
    }
    document.documentElement.setAttribute("data-rudder-find", "");
    return true;
    """

    private static let keepScript = """
    const selection = getSelection();
    globalThis.rudderFindKept = selection.rangeCount ? selection.getRangeAt(0).cloneRange() : null;
    return true;
    """

    private static let restoreScript = """
    const kept = globalThis.rudderFindKept;
    globalThis.rudderFindKept = null;
    if (kept) {
      getSelection().removeAllRanges();
      getSelection().addRange(kept);
    }
    document.documentElement.removeAttribute("data-rudder-find");
    return true;
    """

    /// `[total, index]` for `q`. A match in a text box is the box's own
    /// selection: the page's range only says which box.
    private static let countScript = """
    const selection = getSelection();
    const point = () => {
      const range = selection.rangeCount ? selection.getRangeAt(0).cloneRange() : null;
      const box = document.activeElement;
      if (range && box && (box.tagName === "TEXTAREA" || box.tagName === "INPUT") && range.collapsed
          && range.startContainer.childNodes[range.startOffset] === box) {
        return { range, box, start: box.selectionStart, end: box.selectionEnd };
      }
      return { range, box: null, start: 0, end: 0 };
    };
    const same = (a, b) => a.range !== null && b.range !== null && a.box === b.box && a.start === b.start && a.end === b.end
      && a.range.compareBoundaryPoints(Range.START_TO_START, b.range) === 0
      && a.range.compareBoundaryPoints(Range.END_TO_END, b.range) === 0;
    const current = point();
    const scrolled = [];
    for (const el of document.querySelectorAll("*")) {
      if (el.scrollHeight > el.clientHeight || el.scrollWidth > el.clientWidth) scrolled.push([el, el.scrollTop, el.scrollLeft]);
    }
    let total = 0;
    let index = 0;
    selection.collapse(document.body, 0);
    while (total < limit && window.find(q, false, false, false, false, false, false)) {
      total += 1;
      if (index === 0 && same(point(), current)) index = total;
    }
    if (current.box) {
      current.box.focus({ preventScroll: true });
      current.box.setSelectionRange(current.start, current.end);
    } else {
      selection.removeAllRanges();
      if (current.range) selection.addRange(current.range);
    }
    for (const [el, top, left] of scrolled) {
      el.scrollTop = top;
      el.scrollLeft = left;
    }
    return [total, index];
    """

    private static let selectedTextScript = """
    const box = document.activeElement;
    if (box && (box.tagName === "TEXTAREA" || (box.tagName === "INPUT" && box.type !== "password"))) {
      return box.value.slice(box.selectionStart ?? 0, box.selectionEnd ?? 0);
    }
    return getSelection().toString();
    """
}

// MARK: Bar

/// Over the page while finding: the bar floats at the top of the chat's side,
/// takes no layout space, so the page never moves or resizes, and slides down
/// from under the titlebar strip. With Reduce Motion it fades in place.
///
/// The slide is AppKit's: SwiftUI does not carry an AppKit field (the query)
/// through an animated offset or opacity, so the bar has a hosting view of
/// its own that Core Animation moves or fades whole, inside a corner of the
/// page that clips it at the strip.
struct FindOverlay: NSViewRepresentable {
    let find: PageFind

    func makeNSView(context: Context) -> Host {
        Host(find: find)
    }

    func updateNSView(_ host: Host, context: Context) {
        host.show(find.isVisible, reduceMotion: context.environment.accessibilityReduceMotion)
    }

    func sizeThatFits(_ proposal: ProposedViewSize, nsView: Host, context: Context) -> CGSize? {
        Host.size
    }

    final class Host: NSView {
        /// Room around the bar for its shadow.
        private static let room: CGFloat = 24
        private static let top: CGFloat = 10
        private static let trailing: CGFloat = 16
        static let size = CGSize(width: room + FindBar.width + trailing, height: top + FindBar.height + room)

        private let bar: NSView
        private var showing = false
        /// Each show or close: only the latest one's end hides the bar.
        private var generation = 0

        init(find: PageFind) {
            let bar = NSHostingView(rootView: FindBar(find: find).padding(Self.room))
            bar.sizingOptions = []
            bar.frame = Self.barFrame(shown: false)
            bar.isHidden = true
            self.bar = bar
            super.init(frame: CGRect(origin: .zero, size: Self.size))
            wantsLayer = true
            clipsToBounds = true
            addSubview(bar)
        }

        required init?(coder: NSCoder) {
            fatalError("FindOverlay.Host is made in code")
        }

        override var isFlipped: Bool { true }

        /// Shown, just under the strip; closed, parked above it with its shadow.
        private static func barFrame(shown: Bool) -> CGRect {
            let size = CGSize(width: FindBar.width + 2 * room, height: FindBar.height + 2 * room)
            return CGRect(origin: CGPoint(x: 0, y: shown ? top - room : -size.height), size: size)
        }

        func show(_ visible: Bool, reduceMotion: Bool) {
            guard visible != showing else { return }
            showing = visible
            generation += 1
            let current = generation
            bar.isHidden = false
            let finish: @Sendable () -> Void = { [weak self] in
                MainActor.assumeIsolated {
                    // Closed and done sliding: out of the key view loop and VoiceOver.
                    guard let self, current == self.generation, !self.showing else { return }
                    self.bar.isHidden = true
                }
            }
            if reduceMotion {
                bar.frame = Self.barFrame(shown: true)
                if visible { bar.alphaValue = 0 }
                NSAnimationContext.runAnimationGroup({ context in
                    context.duration = 0.15
                    bar.animator().alphaValue = visible ? 1 : 0
                }, completionHandler: finish)
            } else {
                bar.alphaValue = 1
                NSAnimationContext.runAnimationGroup({ context in
                    // Down quick and settling, up quicker: the curves of a macOS sheet.
                    context.duration = visible ? 0.32 : 0.2
                    context.timingFunction = visible
                        ? CAMediaTimingFunction(controlPoints: 0.2, 0.9, 0.25, 1)
                        : CAMediaTimingFunction(controlPoints: 0.4, 0, 0.9, 0.6)
                    bar.animator().frame = Self.barFrame(shown: visible)
                }, completionHandler: finish)
            }
        }

        /// Only the bar takes clicks; the rest of the corner is the page's.
        override func hitTest(_ point: NSPoint) -> NSView? {
            guard showing else { return nil }
            let local = convert(point, from: superview)
            guard bar.frame.insetBy(dx: Self.room, dy: Self.room).contains(local) else { return nil }
            return super.hitTest(point)
        }
    }
}

/// A compact panel on the window's material: the query with "3 of 12",
/// previous and next, and close.
struct FindBar: View {
    static let width: CGFloat = 372
    static let height: CGFloat = 40

    let find: PageFind
    @State private var focused = false
    @Environment(\.colorScheme) private var scheme
    @Environment(\.displayScale) private var displayScale

    var body: some View {
        let shape = RoundedRectangle(cornerRadius: 12, style: .continuous)
        HStack(spacing: 2) {
            field
                .padding(.trailing, 6)
            Button { find.previous() } label: {
                Image(systemName: "chevron.up")
            }
            .help("Previous Match (Shift-Command-G)")
            .accessibilityLabel("Previous match")
            .disabled(cannotStep)
            Button { find.next() } label: {
                Image(systemName: "chevron.down")
            }
            .help("Next Match (Command-G)")
            .accessibilityLabel("Next match")
            .disabled(cannotStep)
            Button { find.close() } label: {
                Image(systemName: "xmark")
                    .font(.system(size: 10.5, weight: .bold))
            }
            .help("Close (Esc)")
            .accessibilityLabel("Close find")
        }
        .buttonStyle(PanelButtonStyle())
        .padding(.horizontal, 6)
        .frame(width: Self.width, height: Self.height)
        .background(.regularMaterial, in: shape)
        .overlay(shape.strokeBorder(Color.primary.opacity(scheme == .dark ? 0.18 : 0.12), lineWidth: 1 / displayScale))
        .shadow(color: .black.opacity(scheme == .dark ? 0.4 : 0.14), radius: 16, y: 6)
        .shadow(color: .black.opacity(scheme == .dark ? 0.3 : 0.08), radius: 1.5, y: 0.5)
        .accessibilityElement(children: .contain)
        .accessibilityLabel("Find on page")
    }

    private var cannotStep: Bool {
        find.query.isEmpty || find.status == .notFound
    }

    private var field: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(.secondary)
            FindField(
                text: find.query,
                wantsFocus: find.wantsFocus,
                focused: $focused,
                onFocusHandled: { find.focusHandled() },
                onEdit: { find.edit($0) },
                onSubmit: { backwards in backwards ? find.previous() : find.next() },
                onCancel: { find.close() }
            )
            .frame(maxWidth: .infinity)
            if let text = find.statusText {
                Text(text)
                    .font(.system(size: 11.5).monospacedDigit())
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                    .fixedSize()
                    .contentTransition(.numericText())
                    .animation(.snappy(duration: 0.2), value: text)
                    .help(failure ?? text)
            }
        }
        .padding(.horizontal, 8)
        .frame(height: 28)
        .background {
            let shape = RoundedRectangle(cornerRadius: 7, style: .continuous)
            shape.fill(Color.primary.opacity(scheme == .dark ? 0.09 : 0.055))
                // The macOS focus ring: the accent, just outside the field.
                .overlay(
                    RoundedRectangle(cornerRadius: 10, style: .continuous)
                        .strokeBorder(Theme.accent.opacity(focused ? 0.5 : 0), lineWidth: 3)
                        .padding(-3)
                )
        }
        .animation(.easeOut(duration: 0.15), value: focused)
    }

    private var failure: String? {
        if case .failed(let message) = find.status { return message }
        return nil
    }
}

/// Previous, next and close: a plain symbol that gets a soft rounded fill
/// under the pointer, like the controls in a macOS toolbar.
private struct PanelButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        StyledBody(configuration: configuration)
    }

    private struct StyledBody: View {
        let configuration: Configuration
        @Environment(\.isEnabled) private var isEnabled
        @State private var hovering = false

        var body: some View {
            let shape = RoundedRectangle(cornerRadius: 6, style: .continuous)
            configuration.label
                .font(.system(size: 12, weight: .semibold))
                .foregroundStyle(.secondary)
                .frame(width: 26, height: 26)
                .background(shape.fill(Color.primary.opacity(configuration.isPressed ? 0.12 : hovering ? 0.07 : 0)))
                .opacity(isEnabled ? 1 : 0.4)
                .contentShape(shape)
                .onHover { hovering = $0 && isEnabled }
                .animation(.easeOut(duration: 0.12), value: hovering)
        }
    }
}

/// The query: an AppKit field, for Return and Shift+Return, Esc, and putting
/// the cursor in it with the text selected.
private struct FindField: NSViewRepresentable {
    let text: String
    let wantsFocus: Bool
    @Binding var focused: Bool
    let onFocusHandled: () -> Void
    let onEdit: (String) -> Void
    let onSubmit: (_ backwards: Bool) -> Void
    let onCancel: () -> Void

    func makeCoordinator() -> Coordinator {
        Coordinator(self)
    }

    func makeNSView(context: Context) -> Field {
        let field = Field()
        field.isBordered = false
        field.drawsBackground = false
        field.focusRingType = .none
        field.font = .systemFont(ofSize: 13)
        field.placeholderString = "Find on page"
        field.usesSingleLineMode = true
        field.cell?.isScrollable = true
        field.cell?.wraps = false
        field.delegate = context.coordinator
        // Fills the space beside the count instead of sizing to its text.
        field.setContentHuggingPriority(.defaultLow, for: .horizontal)
        field.setContentCompressionResistancePriority(.defaultLow, for: .horizontal)
        field.setAccessibilityLabel("Find on page")
        return field
    }

    func updateNSView(_ field: Field, context: Context) {
        context.coordinator.parent = self
        field.onFocus = { [weak coordinator = context.coordinator] in coordinator?.parent.focused = true }
        if field.stringValue != text { field.stringValue = text }
        guard wantsFocus else { return }
        DispatchQueue.main.async {
            guard let window = field.window else { return }
            window.makeFirstResponder(field)
            field.currentEditor()?.selectAll(nil)
            onFocusHandled()
        }
    }

    final class Field: NSTextField {
        var onFocus: (() -> Void)?

        override func becomeFirstResponder() -> Bool {
            let became = super.becomeFirstResponder()
            if became { onFocus?() }
            return became
        }
    }

    @MainActor
    final class Coordinator: NSObject, NSTextFieldDelegate {
        var parent: FindField

        init(_ parent: FindField) {
            self.parent = parent
        }

        func controlTextDidChange(_ notification: Notification) {
            guard let field = notification.object as? NSTextField else { return }
            parent.onEdit(field.stringValue)
        }

        func controlTextDidEndEditing(_ notification: Notification) {
            parent.focused = false
        }

        func control(_ control: NSControl, textView: NSTextView, doCommandBy selector: Selector) -> Bool {
            switch selector {
            case #selector(NSResponder.insertNewline(_:)), #selector(NSResponder.insertLineBreak(_:)):
                parent.onSubmit(NSApp.currentEvent?.modifierFlags.contains(.shift) == true)
                return true
            case #selector(NSResponder.cancelOperation(_:)):
                parent.onCancel()
                return true
            default:
                return false
            }
        }
    }
}
