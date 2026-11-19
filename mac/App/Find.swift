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

/// Under the titlebar while finding: the query with "3 of 12", previous and
/// next, and Done, in the app's own colors.
struct FindBar: View {
    let find: PageFind
    @State private var focused = false

    var body: some View {
        HStack(spacing: 8) {
            Spacer(minLength: 0)
            field
            steps
            Button("Done") { find.close() }
                .buttonStyle(BarButtonStyle())
        }
        .padding(.horizontal, 12)
        .frame(height: 40)
        .background(Theme.canvas)
        .overlay(alignment: .bottom) {
            Rectangle().fill(Theme.line).frame(height: 1)
        }
    }

    private var field: some View {
        HStack(spacing: 6) {
            Image(systemName: "magnifyingglass")
                .font(.system(size: 11.5, weight: .medium))
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
                    .help(failure ?? text)
            }
        }
        .padding(.horizontal, 9)
        .frame(width: 280, height: 26)
        .background {
            let shape = RoundedRectangle(cornerRadius: 8, style: .continuous)
            shape.fill(Theme.surface)
                .overlay(shape.strokeBorder(focused ? Theme.accent.opacity(0.85) : Theme.line, lineWidth: focused ? 1.5 : 1))
                .shadow(color: Theme.accent.opacity(focused ? 0.18 : 0), radius: 4)
        }
        .animation(.easeOut(duration: 0.15), value: focused)
    }

    private var failure: String? {
        if case .failed(let message) = find.status { return message }
        return nil
    }

    private var steps: some View {
        let shape = RoundedRectangle(cornerRadius: 8, style: .continuous)
        return HStack(spacing: 0) {
            Button { find.previous() } label: {
                Image(systemName: "chevron.left")
            }
            .help("Previous Match (Shift-Command-G)")
            .accessibilityLabel("Previous match")
            Rectangle().fill(Theme.line).frame(width: 1, height: 14)
            Button { find.next() } label: {
                Image(systemName: "chevron.right")
            }
            .help("Next Match (Command-G)")
            .accessibilityLabel("Next match")
        }
        .buttonStyle(StepButtonStyle())
        .disabled(find.query.isEmpty || find.status == .notFound)
        .background {
            shape.fill(Theme.surface)
                .overlay(shape.strokeBorder(Theme.line, lineWidth: 1))
        }
        .clipShape(shape)
    }
}

/// Done: a quiet surface with a hairline, like the app's secondary buttons.
private struct BarButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        StyledBody(configuration: configuration)
    }

    private struct StyledBody: View {
        let configuration: Configuration
        @Environment(\.colorScheme) private var scheme
        @State private var hovering = false

        var body: some View {
            let shape = RoundedRectangle(cornerRadius: 8, style: .continuous)
            configuration.label
                .font(.system(size: 12, weight: .medium))
                .foregroundStyle(Color.primary)
                .padding(.horizontal, 11)
                .frame(height: 26)
                .background {
                    shape.fill(Theme.surface)
                        .overlay(shape.fill(Color.primary.opacity(configuration.isPressed ? 0.08 : hovering ? 0.04 : 0)))
                        .overlay(shape.strokeBorder(Theme.line, lineWidth: 1))
                        .shadow(color: .black.opacity(scheme == .dark ? 0 : 0.04), radius: 2, y: 1)
                }
                .contentShape(shape)
                .onHover { hovering = $0 }
                .animation(.easeOut(duration: 0.12), value: hovering)
        }
    }
}

/// One half of the previous/next pair: the pair draws the surface and hairline.
private struct StepButtonStyle: ButtonStyle {
    func makeBody(configuration: Configuration) -> some View {
        StyledBody(configuration: configuration)
    }

    private struct StyledBody: View {
        let configuration: Configuration
        @Environment(\.isEnabled) private var isEnabled
        @State private var hovering = false

        var body: some View {
            configuration.label
                .font(.system(size: 11, weight: .semibold))
                .foregroundStyle(Color.primary)
                .frame(width: 30, height: 26)
                .background(Color.primary.opacity(configuration.isPressed ? 0.08 : hovering ? 0.04 : 0))
                .opacity(isEnabled ? 1 : 0.4)
                .contentShape(Rectangle())
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
