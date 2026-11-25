load("//scripts:board-build.bzl", "board_platform", "board_stylesheet")

board_platform(name = "board_platform")

board_stylesheet(
    name = "board_css",
    css = "src/board/ui/board.css",
    esbuild = "node_modules/@esbuild/linux-x64/bin/esbuild",
)
