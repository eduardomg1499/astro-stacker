#!/usr/bin/env node
import { existsSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import path from "node:path";

const rootDir = process.cwd();
const repo = process.env.GITHUB_REPO || "eduardomg1499/astro-stacker";
const tauriConfigPath = path.join(rootDir, "src-tauri", "tauri.conf.json");
const tauriConfig = JSON.parse(readFileSync(tauriConfigPath, "utf8"));
const version = tauriConfig.version;
const tag = process.env.GITHUB_RELEASE_TAG || `v${version}`;
const outputPath = process.env.LATEST_JSON_PATH || path.join(rootDir, "latest.json");
const releaseDownloadPath = `/releases/download/${tag}/`;

function readJsonFile(filePath) {
  return JSON.parse(readFileSync(filePath, "utf8").replace(/^\uFEFF/, ""));
}

let existingManifest = null;
if (existsSync(outputPath)) {
  try {
    existingManifest = readJsonFile(outputPath);
  } catch {
    existingManifest = null;
  }
}

function githubAssetName(fileName) {
  return fileName.replaceAll(" ", ".");
}

function isReusablePlatformEntry(platformKey, platform) {
  if (!platform?.signature || !platform?.url) return false;

  const decodedUrl = decodeURIComponent(platform.url);
  if (!decodedUrl.includes(releaseDownloadPath)) return false;
  const assetName = decodedUrl.split("/").pop() || "";

  if (platformKey.startsWith("windows-")) {
    return assetName.includes(version);
  }

  if (platformKey === "darwin-aarch64") {
    return assetName.includes(`_${version}_aarch64.app.tar.gz`);
  }

  if (platformKey === "darwin-x86_64") {
    return assetName.includes(`_${version}_x86_64.app.tar.gz`);
  }

  return true;
}

function collectFiles(dir, predicate) {
  if (!existsSync(dir)) return [];
  return readdirSync(dir)
    .map((file) => path.join(dir, file))
    .filter((file) => statSync(file).isFile() && predicate(file))
    .sort((a, b) => statSync(b).mtimeMs - statSync(a).mtimeMs);
}

function addPlatform(platforms, platformKey, artifactPath) {
  if (!artifactPath) return;

  const signaturePath = `${artifactPath}.sig`;
  if (!existsSync(signaturePath)) {
    console.warn(`Skipping ${platformKey}: missing signature ${signaturePath}`);
    return;
  }

  const fileName = githubAssetName(path.basename(artifactPath));
  platforms[platformKey] = {
    signature: readFileSync(signaturePath, "utf8").trim(),
    url: `https://github.com/${repo}/releases/download/${tag}/${encodeURIComponent(fileName)}`
  };
}

const platforms = {};
if (existingManifest?.version === version && existingManifest?.platforms) {
  for (const [platformKey, platform] of Object.entries(existingManifest.platforms)) {
    if (isReusablePlatformEntry(platformKey, platform)) {
      platforms[platformKey] = platform;
    }
  }
}

const windowsInstaller = collectFiles(
  path.join(rootDir, "src-tauri", "target", "release", "bundle", "nsis"),
  (file) => path.basename(file).endsWith("-setup.exe") && path.basename(file).includes(version)
)[0];
addPlatform(platforms, "windows-x86_64", windowsInstaller);

const macAppleSiliconUpdater = collectFiles(
  path.join(rootDir, "src-tauri", "target", "aarch64-apple-darwin", "release", "bundle", "macos"),
  (file) => path.basename(file).endsWith(`_${version}_aarch64.app.tar.gz`)
)[0];
addPlatform(platforms, "darwin-aarch64", macAppleSiliconUpdater);

const macIntelUpdater = collectFiles(
  path.join(rootDir, "src-tauri", "target", "x86_64-apple-darwin", "release", "bundle", "macos"),
  (file) => path.basename(file).endsWith(`_${version}_x86_64.app.tar.gz`)
)[0];
addPlatform(platforms, "darwin-x86_64", macIntelUpdater);

if (Object.keys(platforms).length === 0) {
  throw new Error("No updater artifacts with .sig files were found.");
}

for (const expectedPlatform of ["windows-x86_64", "darwin-aarch64", "darwin-x86_64"]) {
  if (!platforms[expectedPlatform]) {
    console.warn(`Warning: ${expectedPlatform} is missing from latest.json.`);
  }
}

const manifest = {
  version,
  notes: process.env.RELEASE_NOTES || `Actualizacion Zenith Astro Stacker v${version}`,
  pub_date:
    process.env.RELEASE_PUB_DATE ||
    (existingManifest?.version === version ? existingManifest?.pub_date : null) ||
    new Date().toISOString(),
  platforms
};

writeFileSync(outputPath, `${JSON.stringify(manifest, null, 2)}\n`);
console.log(`Wrote ${outputPath}`);
console.log(`Platforms: ${Object.keys(platforms).join(", ")}`);
