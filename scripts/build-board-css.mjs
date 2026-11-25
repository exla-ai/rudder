import { execFileSync } from "node:child_process";
import { mkdirSync } from "node:fs";
import { build } from "esbuild";

const outfile = "dist/board/board.css";
if (process.env.RUDDER_BUCK2_CACHE_ADDRESS) {
  mkdirSync("dist/board", { recursive: true });
  execFileSync("buck2", ["build", "//:board_css", "--out", outfile], { stdio: "inherit" });
} else {
  await build({ entryPoints: ["src/board/ui/board.css"], bundle: true, minify: true, outfile });
}
