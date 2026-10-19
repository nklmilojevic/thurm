// Builds the web client into tailnet/internal/web/dist, which thurm-tailnet embeds: `just web`
// runs it after wasm-bindgen has written pkg/. Assets get content-hashed names (served as
// immutable) and .br / .gz copies; index.html names them.
import { brotliCompressSync, constants, gzipSync } from "node:zlib";
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { basename, extname, join } from "node:path";

const root = import.meta.dir;
const out = process.env.THURM_WEB_OUT ?? join(root, "../tailnet/internal/web/dist");
const assets = join(out, "assets");

mkdirSync(out, { recursive: true });
for (const name of readdirSync(out, { withFileTypes: true })) {
  if (name.name !== ".gitignore") rmSync(join(out, name.name), { recursive: true, force: true });
}
mkdirSync(assets, { recursive: true });

const hash = (data: Uint8Array | string) =>
  createHash("sha256").update(data).digest("hex").slice(0, 12);

/** Writes `data` as assets/<stem>-<hash><ext> and returns its URL path. */
function asset(stem: string, ext: string, data: Uint8Array | string): string {
  const name = `${stem}-${hash(data)}${ext}`;
  writeFileSync(join(assets, name), data);
  return `/assets/${name}`;
}

async function bundle(entry: string, define: Record<string, string> = {}): Promise<string> {
  const result = await Bun.build({
    entrypoints: [join(root, entry)],
    target: "browser",
    format: "esm",
    minify: true,
    define,
  });
  if (!result.success) {
    for (const log of result.logs) console.error(log);
    process.exit(1);
  }
  return await result.outputs[0].text();
}

const wasm = asset("thurm_web", ".wasm", readFileSync(join(root, "pkg/thurm_web_bg.wasm")));
const clock = asset("clock", ".js", await bundle("src/clock.ts"));

const font = (style: string) =>
  asset(
    `jetbrains-mono-${style}`,
    ".woff2",
    readFileSync(
      join(
        root,
        `node_modules/@fontsource-variable/jetbrains-mono/files/jetbrains-mono-latin-wght-${style}.woff2`,
      ),
    ),
  );
const css = readFileSync(join(root, "src/style.css"), "utf8")
  .replace("__FONT_NORMAL__", font("normal"))
  .replace("__FONT_ITALIC__", font("italic"));
const style = asset("app", ".css", css);

const app = asset(
  "app",
  ".js",
  await bundle("src/main.ts", {
    __WASM_URL__: JSON.stringify(wasm),
    __CLOCK_URL__: JSON.stringify(clock),
  }),
);

writeFileSync(
  join(out, "index.html"),
  readFileSync(join(root, "index.html"), "utf8")
    .replace("__STYLE__", style)
    .replace("__APP__", app),
);

// Precompressed copies the server picks by Accept-Encoding.
let total = 0;
for (const dir of [out, assets]) {
  for (const name of readdirSync(dir)) {
    if (![".html", ".js", ".css", ".wasm"].includes(extname(name))) continue;
    const data = readFileSync(join(dir, name));
    const br = brotliCompressSync(data, {
      params: { [constants.BROTLI_PARAM_QUALITY]: 11, [constants.BROTLI_PARAM_SIZE_HINT]: data.length },
    });
    writeFileSync(join(dir, `${name}.br`), br);
    writeFileSync(join(dir, `${name}.gz`), gzipSync(data, { level: 9 }));
    total += br.length;
  }
}
for (const name of readdirSync(assets).filter((n) => extname(n) === ".woff2")) {
  total += readFileSync(join(assets, name)).length;
}
console.log(`web client: ${out} (${Math.round(total / 1024)} KiB over the wire with brotli)`);
const budget = Number(process.env.THURM_WEB_BUDGET_KIB ?? 0);
if (budget && total / 1024 > budget) {
  console.error(`over the ${budget} KiB budget (${basename(out)})`);
  process.exit(1);
}
