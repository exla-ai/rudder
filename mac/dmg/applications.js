// osascript -l JavaScript applications.js <out>
//
// Writes <out>: a Finder alias to /Applications that carries the Applications
// folder's icon as its own custom icon. make-dmg.sh puts it in Rudder.dmg as
// "Applications". A symlink or a plain alias would do the same job, but
// Finder on macOS 26 (seen on 26.3) draws every symlink and alias as an empty
// dashed square instead of its target's icon; a custom icon it draws as is.
// Dropping Rudder onto the alias still copies it into /Applications.
ObjC.import("AppKit")

function run(argv) {
  if (argv.length !== 1) throw new Error("usage: osascript -l JavaScript applications.js <out>")
  const out = argv[0]
  const options = $.NSURLBookmarkCreationSuitableForBookmarkFile
  const error = $()
  const bookmark = $.NSURL.fileURLWithPath("/Applications")
    .bookmarkDataWithOptionsIncludingResourceValuesForKeysRelativeToURLError(options, $(), $(), error)
  if (bookmark.isNil()) throw new Error(`no bookmark for /Applications: ${error.localizedDescription.js}`)
  if (!$.NSURL.writeBookmarkDataToURLOptionsError(bookmark, $.NSURL.fileURLWithPath(out), options, error)) {
    throw new Error(`could not write ${out}: ${error.localizedDescription.js}`)
  }
  const workspace = $.NSWorkspace.sharedWorkspace
  if (!workspace.setIconForFileOptions(workspace.iconForFile("/Applications"), out, 0)) {
    throw new Error(`could not give ${out} the Applications icon`)
  }
}
