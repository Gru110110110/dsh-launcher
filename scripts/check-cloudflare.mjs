import { access, readFile } from "node:fs/promises";
import { constants } from "node:fs";
import { resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import vm from "node:vm";
import {
  RELEASE_PLATFORMS,
  releaseAssetName,
  releaseDownloadUrl,
} from "./release-assets.mjs";

const root = resolve(fileURLToPath(new URL("..", import.meta.url)));
const publicRoot = resolve(root, "public");
const read = (path) => readFile(resolve(root, path), "utf8");

const wrangler = JSON.parse(await read("public/wrangler.jsonc"));
if (wrangler.assets?.directory !== ".") {
  throw new Error(
    "public/wrangler.jsonc must publish the current public directory",
  );
}
if (
  wrangler.main !== "worker.js" ||
  wrangler.assets?.binding !== "ASSETS" ||
  !wrangler.assets?.run_worker_first?.includes("/latest.json")
) {
  throw new Error(
    "Cloudflare must route /latest.json through worker.js with the ASSETS binding",
  );
}
if (!wrangler.name || !wrangler.compatibility_date) {
  throw new Error(
    "Cloudflare configuration is missing its name or compatibility date",
  );
}

const ignored = (await read("public/.assetsignore"))
  .split(/\r?\n/u)
  .map((line) => line.trim())
  .filter(Boolean);
if (!ignored.includes("wrangler.jsonc") || !ignored.includes("worker.js")) {
  throw new Error(
    "public/.assetsignore must exclude Wrangler and Worker source files",
  );
}

const workerSource = await read("public/worker.js");
const workerModule = await import(
  `data:text/javascript;base64,${Buffer.from(workerSource).toString("base64")}`
);
const worker = workerModule.default;
let assetRequests = 0;
const assets = {
  fetch: async () => {
    assetRequests += 1;
    return new Response("asset");
  },
};
const assetResponse = await worker.fetch(
  new Request("https://dsdesktop.com/index.html"),
  { ASSETS: assets },
);
if ((await assetResponse.text()) !== "asset" || assetRequests !== 1) {
  throw new Error("Cloudflare Worker must delegate website assets unchanged");
}

const originalFetch = globalThis.fetch;
try {
  let upstreamRequests = 0;
  globalThis.fetch = async (input, options) => {
    upstreamRequests += 1;
    if (
      String(input) !==
        "https://github.com/Gru110110110/deepseek-harness-desktop-launcher/releases/latest/download/latest.json" ||
      options?.redirect !== "follow"
    ) {
      throw new Error("Unexpected updater manifest request");
    }
    return new Response('{"version":"0.2.1"}', {
      headers: { "Content-Type": "application/octet-stream" },
    });
  };
  const manifestResponse = await worker.fetch(
    new Request("https://dsdesktop.com/latest.json"),
    { ASSETS: assets },
  );
  if (
    manifestResponse.status !== 200 ||
    manifestResponse.headers.get("Content-Type") !==
      "application/json; charset=utf-8" ||
    (await manifestResponse.json()).version !== "0.2.1" ||
    upstreamRequests !== 1
  ) {
    throw new Error("Cloudflare updater manifest proxy contract failed");
  }
  const rejectedMethod = await worker.fetch(
    new Request("https://dsdesktop.com/latest.json", { method: "POST" }),
    { ASSETS: assets },
  );
  if (rejectedMethod.status !== 405 || upstreamRequests !== 1) {
    throw new Error("Cloudflare updater proxy must reject non-read methods");
  }
  globalThis.fetch = async () =>
    new Response("upstream error", { status: 503 });
  const unavailableManifest = await worker.fetch(
    new Request("https://dsdesktop.com/latest.json"),
    { ASSETS: assets },
  );
  if (
    unavailableManifest.status !== 502 ||
    unavailableManifest.headers.get("Cache-Control") !== "no-store"
  ) {
    throw new Error("Cloudflare updater proxy failures must remain retryable");
  }
} finally {
  globalThis.fetch = originalFetch;
}

const tauriConfig = JSON.parse(await read("src-tauri/tauri.conf.json"));
const updaterEndpoints = tauriConfig.plugins?.updater?.endpoints;
if (
  JSON.stringify(updaterEndpoints) !==
  JSON.stringify([
    "https://dsdesktop.com/latest.json",
    "https://github.com/Gru110110110/deepseek-harness-desktop-launcher/releases/latest/download/latest.json",
  ])
) {
  throw new Error(
    "Desktop updater must prefer the website manifest and retain GitHub fallback",
  );
}

const marketplaceSource = await read("crates/dsh-core/src/marketplace.rs");
if (
  !marketplaceSource.includes(
    'const MARKET_PUBLIC_BASE: &str = "https://market.dsdesktop.com/v1";',
  ) ||
  !marketplaceSource.includes(
    "const MARKET_PUBLIC_REPOSITORY: &str = MARKET_REPOSITORY;",
  )
) {
  throw new Error(
    "Desktop marketplace must use the dedicated Cloudflare R2 custom domain",
  );
}

const marketplaceWorkflow = await read(".github/workflows/marketplace.yml");
for (const required of [
  'cron: "0 23 * * *"',
  "MARKETPLACE_BUCKET: dsh-launcher-marketplace",
  "MARKETPLACE_ORIGIN: https://market.dsdesktop.com/v1",
  "R2_ACCESS_KEY_ID",
  "R2_SECRET_ACCESS_KEY",
  "aws s3api put-object",
  "v1/catalog-$slot.json",
  "v1/latest.json",
]) {
  if (!marketplaceWorkflow.includes(required)) {
    throw new Error(`Marketplace publication contract is missing: ${required}`);
  }
}

const vite = await read("vite.config.ts");
if (!/publicDir\s*:\s*false/u.test(vite)) {
  throw new Error(
    "Vite must keep the standalone public website out of the desktop bundle",
  );
}

const html = await read("public/index.html");
const css = await read("public/style.css");
const main = await read("public/main.js");

if (
  (html.match(/<figure\b[^>]*\bdata-carousel-slide\b/gu) ?? []).length !== 4 ||
  (html.match(/<button\b[^>]*\bdata-carousel-to=/gu) ?? []).length !== 4
) {
  throw new Error("Website feature carousel must contain exactly four slides");
}
for (const screenshot of [
  "ScreenShot",
  "ScreenShot_plugin",
  "ScreenShot_remote",
  "ScreenShot_settings",
]) {
  for (const language of ["zh", "en"]) {
    if (!html.includes(`screenshots/${screenshot}_${language}.png`)) {
      throw new Error(
        `Website feature carousel is missing ${screenshot}_${language}.png`,
      );
    }
  }
}

const references = [
  ...html.matchAll(/\b(?:href|src)="([^"]+)"/gu),
  ...html.matchAll(/\bdata-screenshot-(?:zh|en)="([^"]+)"/gu),
  ...css.matchAll(/url\(\s*["']?([^"')]+)["']?\s*\)/gu),
].map((match) => match[1]);

for (const reference of references) {
  if (
    reference.startsWith("#") ||
    /^(?:https?:|mailto:|data:|javascript:)/u.test(reference)
  ) {
    continue;
  }
  const clean = reference.split(/[?#]/u, 1)[0];
  const target = resolve(publicRoot, clean.replace(/^\//u, ""));
  if (target !== publicRoot && !target.startsWith(`${publicRoot}${sep}`)) {
    throw new Error(`Website asset escapes public/: ${reference}`);
  }
  try {
    await access(target, constants.R_OK);
  } catch {
    throw new Error(`Website asset is missing: ${reference}`);
  }
}

const dictionaryStart = main.indexOf("var I18N = {");
const dictionaryEnd = main.indexOf("\n\n  var STORAGE_KEY", dictionaryStart);
if (dictionaryStart < 0 || dictionaryEnd < 0) {
  throw new Error("Unable to locate the website translation dictionary");
}
const objectSource = main
  .slice(dictionaryStart + "var I18N = ".length, dictionaryEnd)
  .replace(/;\s*$/u, "");
const dictionary = vm.runInNewContext(
  `(${objectSource})`,
  Object.create(null),
  {
    timeout: 100,
  },
);
const zhKeys = Object.keys(dictionary.zh).sort();
const enKeys = Object.keys(dictionary.en).sort();
if (JSON.stringify(zhKeys) !== JSON.stringify(enKeys)) {
  throw new Error(
    "Website Chinese and English dictionaries have different keys",
  );
}
for (const match of html.matchAll(/data-i18n(?:-html|-alt)?="([^"]+)"/gu)) {
  if (!(match[1] in dictionary.zh) || !(match[1] in dictionary.en)) {
    throw new Error(`Website translation is missing: ${match[1]}`);
  }
}

const packageJson = JSON.parse(await read("package.json"));
const releaseTag = `desktop-v${packageJson.version}`;
const downloadCards = new Map();
for (const match of html.matchAll(
  /<a\b[^>]*\bdata-platform="([^"]+)"[^>]*>/gu,
)) {
  const href = match[0].match(/\bhref="([^"]+)"/u)?.[1];
  if (!href || downloadCards.has(match[1])) {
    throw new Error(`Website download card is invalid: ${match[1]}`);
  }
  downloadCards.set(match[1], href);
}
for (const platform of RELEASE_PLATFORMS) {
  const filename = releaseAssetName({
    productName: tauriConfig.productName,
    version: packageJson.version,
    platform,
    ext: platform.installerExt,
  });
  const expectedUrl = releaseDownloadUrl(
    "Gru110110110/deepseek-harness-desktop-launcher",
    releaseTag,
    filename,
  );
  if (downloadCards.get(platform.websitePlatform) !== expectedUrl) {
    throw new Error(
      `Website download URL mismatch for ${platform.websitePlatform}: expected ${expectedUrl}`,
    );
  }
}
if (downloadCards.size !== RELEASE_PLATFORMS.length) {
  throw new Error("Website must expose exactly one download card per platform");
}
const advertisedVersions = [...main.matchAll(/\bv(\d+\.\d+\.\d+)\b/gu)].map(
  (match) => match[1],
);
if (
  advertisedVersions.length === 0 ||
  advertisedVersions.some((version) => version !== packageJson.version)
) {
  throw new Error(`Website version must match package ${packageJson.version}`);
}

console.log(
  `Cloudflare website contract passed (${wrangler.name}, ${references.length} asset references)`,
);
