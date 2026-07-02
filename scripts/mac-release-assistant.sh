#!/usr/bin/env bash
set -Eeuo pipefail

ROOT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT_DIR"

CODE_REPO_URL="https://github.com/eduardomg1499/Proyecto-Zenith-Astro-Stacker.git"
CODE_REMOTE="backup-private"
CODE_BRANCH="main"
RELEASE_REPO="eduardomg1499/astro-stacker"
CHECK_ONLY=0
TEMP_DIRS=()

if [[ "${1:-}" == "--check" ]]; then
  CHECK_ONLY=1
elif [[ $# -gt 0 ]]; then
  echo "Uso: $0 [--check]" >&2
  exit 2
fi

cleanup() {
  local dir
  for dir in "${TEMP_DIRS[@]:-}"; do
    [[ -n "$dir" && -d "$dir" ]] && rm -rf "$dir"
  done
}
trap cleanup EXIT

fail() {
  echo "" >&2
  echo "ERROR: $*" >&2
  exit 1
}

title() {
  echo ""
  echo "===================================================================="
  echo "  $*"
  echo "===================================================================="
}

step() {
  echo ""
  echo "-> $*"
}

require_command() {
  command -v "$1" >/dev/null 2>&1 || fail "Falta '$1'. $2"
}

ask_yes_no() {
  local prompt="$1"
  local default="${2:-N}"
  local answer
  while true; do
    if [[ "$default" == "S" ]]; then
      read -r -p "$prompt [S/n]: " answer
      answer="${answer:-S}"
    else
      read -r -p "$prompt [s/N]: " answer
      answer="${answer:-N}"
    fi
    case "$answer" in
      S|s|Y|y) return 0 ;;
      N|n) return 1 ;;
      *) echo "Responde S o N." ;;
    esac
  done
}

ensure_gh_auth() {
  require_command gh "Instala GitHub CLI con: brew install gh"
  if ! gh auth status >/dev/null 2>&1; then
    echo "GitHub CLI necesita autenticacion. Se abrira el inicio de sesion."
    gh auth login
  fi
  gh auth status >/dev/null 2>&1 || fail "GitHub CLI no quedo autenticado."
}

get_version() {
  node -e 'const fs=require("fs"); const c=JSON.parse(fs.readFileSync("src-tauri/tauri.conf.json","utf8")); process.stdout.write(c.version);'
}

assert_versions_match() {
  node <<'NODE'
const fs = require("fs");

const readJson = (path) => JSON.parse(fs.readFileSync(path, "utf8"));
const tauri = readJson("src-tauri/tauri.conf.json").version;
const packageVersion = readJson("package.json").version;
const lock = readJson("package-lock.json");
const cargo = fs.readFileSync("src-tauri/Cargo.toml", "utf8");
const cargoLock = fs.readFileSync("src-tauri/Cargo.lock", "utf8");
const html = fs.readFileSync("index.html", "utf8");

const cargoVersion = cargo.match(/^\[package\][\s\S]*?^version\s*=\s*"([^"]+)"/m)?.[1];
const lockVersion = cargoLock.match(/\[\[package\]\]\r?\nname = "astro-stacker"\r?\nversion = "([^"]+)"/)?.[1];
const splashVersion = html.match(/class="splash-version">v([^ <]+)\s+Beta/)?.[1];
const versions = {
  "src-tauri/tauri.conf.json": tauri,
  "package.json": packageVersion,
  "package-lock.json": lock.version,
  "package-lock.json packages['']": lock.packages?.[""]?.version,
  "src-tauri/Cargo.toml": cargoVersion,
  "src-tauri/Cargo.lock": lockVersion,
  "index.html splash": splashVersion
};

const invalid = Object.entries(versions).filter(([, value]) => value !== tauri);
if (invalid.length) {
  console.error("Las versiones no coinciden:");
  for (const [file, value] of Object.entries(versions)) {
    console.error(`  ${file}: ${value ?? "NO ENCONTRADA"}`);
  }
  process.exit(1);
}
console.log(`Version consistente en todos los archivos: ${tauri}`);
NODE
}

set_project_version() {
  local new_version="$1"
  NEW_VERSION="$new_version" node <<'NODE'
const fs = require("fs");
const next = process.env.NEW_VERSION;

function read(path) {
  return fs.readFileSync(path, "utf8");
}

function write(path, text) {
  fs.writeFileSync(path, text.endsWith("\n") ? text : `${text}\n`);
}

function replaceVersion(path, regex, label) {
  const source = read(path);
  const matches = source.match(regex);
  if (!matches) throw new Error(`No se encontro ${label} en ${path}`);
  const updated = source.replace(regex, (...args) => `${args[1]}${next}${args[2]}`);
  if (updated === source) throw new Error(`No se pudo cambiar ${label} en ${path}`);
  write(path, updated);
}

const pkg = JSON.parse(read("package.json"));
pkg.version = next;
write("package.json", `${JSON.stringify(pkg, null, 2)}\n`);

const packageLock = JSON.parse(read("package-lock.json"));
packageLock.version = next;
if (!packageLock.packages?.[""]) throw new Error("package-lock.json no contiene packages['']");
packageLock.packages[""].version = next;
write("package-lock.json", `${JSON.stringify(packageLock, null, 2)}\n`);

const tauri = JSON.parse(read("src-tauri/tauri.conf.json"));
tauri.version = next;
write("src-tauri/tauri.conf.json", `${JSON.stringify(tauri, null, 2)}\n`);

replaceVersion(
  "src-tauri/Cargo.toml",
  /(^\[package\][\s\S]*?^version\s*=\s*")[^"]+(".*$)/m,
  "la version del paquete"
);
replaceVersion(
  "src-tauri/Cargo.lock",
  /(\[\[package\]\]\r?\nname = "astro-stacker"\r?\nversion = ")[^"]+("\r?$)/m,
  "la version bloqueada de astro-stacker"
);
replaceVersion(
  "index.html",
  /(class="splash-version">v)[^ <]+(\s+Beta)/,
  "la version visible del splash"
);
NODE
}

validate_latest_json() {
  local path="$1"
  local version="$2"
  local tag="$3"
  local require_windows="${4:-0}"
  LATEST_PATH="$path" EXPECTED_VERSION="$version" EXPECTED_TAG="$tag" REQUIRE_WINDOWS="$require_windows" node <<'NODE'
const fs = require("fs");
const manifest = JSON.parse(fs.readFileSync(process.env.LATEST_PATH, "utf8"));
if (manifest.version !== process.env.EXPECTED_VERSION) {
  throw new Error(`latest.json usa ${manifest.version}, se esperaba ${process.env.EXPECTED_VERSION}`);
}
const required = ["darwin-aarch64", "darwin-x86_64"];
if (process.env.REQUIRE_WINDOWS === "1") required.push("windows-x86_64");
for (const platform of required) {
  const entry = manifest.platforms?.[platform];
  if (!entry?.signature || !entry?.url) throw new Error(`Falta ${platform} en latest.json`);
  if (!entry.url.includes(`/releases/download/${process.env.EXPECTED_TAG}/`)) {
    throw new Error(`${platform} no apunta al tag ${process.env.EXPECTED_TAG}`);
  }
}
console.log(`latest.json OK: ${Object.keys(manifest.platforms).join(", ")}`);
NODE
}

release_exists() {
  gh release view "$1" --repo "$RELEASE_REPO" >/dev/null 2>&1
}

preserve_remote_latest() {
  local tag="$1"
  local version="$2"
  local temp

  command -v gh >/dev/null 2>&1 || return 0
  gh auth status >/dev/null 2>&1 || return 0
  release_exists "$tag" || return 0

  temp="$(mktemp -d "${TMPDIR:-/tmp}/zas-existing-latest.XXXXXX")"
  TEMP_DIRS+=("$temp")
  if gh release download "$tag" --repo "$RELEASE_REPO" --pattern latest.json --dir "$temp" >/dev/null 2>&1; then
    validate_latest_json "$temp/latest.json" "$version" "$tag" 0
    cp "$temp/latest.json" latest.json
    echo "Se conservo el latest.json remoto antes de compilar (incluido Windows si ya existia)."
  fi
}

mac_artifact_path() {
  local arch="$1"
  local kind="$2"
  local version="$3"
  local target label
  case "$arch" in
    arm) target="aarch64-apple-darwin"; label="aarch64" ;;
    intel) target="x86_64-apple-darwin"; label="x86_64" ;;
    *) fail "Arquitectura macOS desconocida: $arch" ;;
  esac
  case "$kind" in
    updater) echo "src-tauri/target/$target/release/bundle/macos/Zenith.Astro.Stacker_${version}_${label}.app.tar.gz" ;;
    dmg) echo "src-tauri/target/$target/release/bundle/dmg/Zenith Astro Stacker_${version}_${label}_styled.dmg" ;;
    *) fail "Tipo de artefacto desconocido: $kind" ;;
  esac
}

assert_mac_artifacts() {
  local version="$1"
  local arch updater dmg
  for arch in arm intel; do
    updater="$(mac_artifact_path "$arch" updater "$version")"
    dmg="$(mac_artifact_path "$arch" dmg "$version")"
    [[ -f "$updater" ]] || fail "Falta el actualizador macOS: $updater"
    [[ -f "$updater.sig" ]] || fail "Falta la firma macOS: $updater.sig"
    [[ -f "$dmg" ]] || fail "Falta el DMG macOS: $dmg"
    xcrun stapler validate "$dmg" >/dev/null || fail "El DMG no tiene notarizacion valida: $dmg"
  done
  validate_latest_json latest.json "$version" "$TARGET_TAG" 0
}

stage_mac_assets() {
  local version="$1"
  local stage="$2"
  local arch updater dmg
  for arch in arm intel; do
    updater="$(mac_artifact_path "$arch" updater "$version")"
    dmg="$(mac_artifact_path "$arch" dmg "$version")"
    cp "$updater" "$stage/$(basename "$updater")"
    cp "$updater.sig" "$stage/$(basename "$updater.sig")"
    cp "$dmg" "$stage/$(basename "$dmg" | tr ' ' '.')"
  done
  cp latest.json "$stage/latest.json"
}

merge_remote_windows_entry() {
  local tag="$1"
  local version="$2"
  local temp

  release_exists "$tag" || return 0
  temp="$(mktemp -d "${TMPDIR:-/tmp}/zas-remote-merge.XXXXXX")"
  TEMP_DIRS+=("$temp")
  gh release download "$tag" --repo "$RELEASE_REPO" --pattern latest.json --dir "$temp" >/dev/null
  validate_latest_json "$temp/latest.json" "$version" "$tag" 0

  if REMOTE_LATEST="$temp/latest.json" node -e '
    const fs=require("fs");
    const m=JSON.parse(fs.readFileSync(process.env.REMOTE_LATEST,"utf8"));
    process.exit(m.platforms?.["windows-x86_64"] ? 0 : 1);
  '; then
    cp "$temp/latest.json" latest.json
    GITHUB_RELEASE_TAG="$tag" npm run release:latest-json
    validate_latest_json latest.json "$version" "$tag" 1
    echo "La entrada Windows remota fue conservada."
  fi
}

validate_remote_assets() {
  local tag="$1"
  local stage="$2"
  local release_json file name local_sha remote_sha
  release_json="$(gh release view "$tag" --repo "$RELEASE_REPO" --json assets,url)"

  for file in "$stage"/*; do
    name="$(basename "$file")"
    local_sha="$(shasum -a 256 "$file" | awk '{print $1}')"
    remote_sha="$(RELEASE_JSON="$release_json" ASSET_NAME="$name" node -e '
      const data=JSON.parse(process.env.RELEASE_JSON);
      const asset=data.assets.find((item)=>item.name===process.env.ASSET_NAME);
      if (!asset) process.exit(2);
      process.stdout.write((asset.digest || "").replace(/^sha256:/, ""));
    ')" || fail "GitHub no muestra el asset $name"
    [[ -n "$remote_sha" ]] || fail "GitHub no entrego SHA-256 para $name"
    [[ "$local_sha" == "$remote_sha" ]] || fail "SHA-256 remoto incorrecto para $name"
    echo "SHA OK: $name"
  done
}

publish_release() {
  local version="$1"
  local tag="$2"
  local stage notes_dir notes verify_dir release_url

  ensure_gh_auth
  merge_remote_windows_entry "$tag" "$version"
  assert_mac_artifacts "$version"

  stage="$(mktemp -d "${TMPDIR:-/tmp}/zas-mac-release.XXXXXX")"
  TEMP_DIRS+=("$stage")
  stage_mac_assets "$version" "$stage"

  title "PUBLICACION DEL RELEASE $tag"
  echo "Repositorio: $RELEASE_REPO"
  echo "Se subiran seis archivos macOS y latest.json."
  echo "Los instaladores Windows existentes no se eliminaran."
  ls -lh "$stage"
  ask_yes_no "Confirmar publicacion" N || {
    echo "Publicacion omitida."
    return 0
  }

  if release_exists "$tag"; then
    gh release upload "$tag" "$stage"/* --repo "$RELEASE_REPO" --clobber
    gh release edit "$tag" --repo "$RELEASE_REPO" --latest
  else
    notes_dir="$(mktemp -d "${TMPDIR:-/tmp}/zas-release-notes.XXXXXX")"
    TEMP_DIRS+=("$notes_dir")
    notes="$notes_dir/release-notes.md"
    printf 'Zenith Astro Stacker v%s\n\nPaquetes macOS Apple Silicon e Intel firmados y notarizados. Windows se agrega posteriormente con scripts/windows-release-assistant.ps1.\n' "$version" > "$notes"
    gh release create "$tag" "$stage"/* \
      --repo "$RELEASE_REPO" \
      --target main \
      --title "Zenith Astro Stacker v$version Beta" \
      --notes-file "$notes" \
      --latest
  fi

  validate_remote_assets "$tag" "$stage"
  verify_dir="$(mktemp -d "${TMPDIR:-/tmp}/zas-release-verify.XXXXXX")"
  TEMP_DIRS+=("$verify_dir")
  gh release download "$tag" --repo "$RELEASE_REPO" --pattern latest.json --dir "$verify_dir"
  cmp latest.json "$verify_dir/latest.json" >/dev/null || fail "El latest.json remoto no coincide con el local."
  release_url="$(gh release view "$tag" --repo "$RELEASE_REPO" --json url --jq .url)"
  echo "Release publicado y validado: $release_url"
}

push_project() {
  local version="$1"
  local status message local_sha remote_sha item
  local -a untracked_files=()

  title "SUBIDA DEL PROYECTO PRIVADO"
  status="$(git status --short)"
  if [[ -n "$status" ]]; then
    echo "$status"
  else
    echo "No hay cambios locales pendientes."
  fi

  ask_yes_no "Preparar los cambios rastreados para el commit" S || {
    echo "Subida del proyecto omitida."
    return 0
  }
  git add -u

  while IFS= read -r -d '' item; do
    untracked_files+=("$item")
  done < <(git ls-files --others --exclude-standard -z)
  if [[ ${#untracked_files[@]} -gt 0 ]]; then
    echo ""
    echo "Archivos no rastreados (no se incluyen automaticamente):"
    printf '  %s\n' "${untracked_files[@]}"
    if ask_yes_no "Incluir tambien TODOS estos archivos no rastreados" N; then
      git add -- "${untracked_files[@]}"
    fi
  fi

  if git diff --cached --quiet; then
    echo "No hay cambios preparados; se comprobara igualmente el push."
  else
    git diff --cached --stat
    ask_yes_no "Confirmar este commit para el repositorio privado" N || {
      echo "Commit cancelado; los archivos preparados permanecen en el indice para revision."
      return 0
    }
    read -r -p "Mensaje del commit [Prepare v$version]: " message
    message="${message:-Prepare v$version}"
    git commit -m "$message"
  fi

  if git remote get-url "$CODE_REMOTE" >/dev/null 2>&1; then
    git remote set-url "$CODE_REMOTE" "$CODE_REPO_URL"
  else
    git remote add "$CODE_REMOTE" "$CODE_REPO_URL"
  fi
  git push "$CODE_REMOTE" "HEAD:$CODE_BRANCH"
  local_sha="$(git rev-parse HEAD)"
  remote_sha="$(git ls-remote "$CODE_REMOTE" "refs/heads/$CODE_BRANCH" | awk '{print $1}')"
  [[ "$local_sha" == "$remote_sha" ]] || fail "El commit remoto no coincide con el local."
  echo "Proyecto validado en $CODE_REPO_URL"
  echo "Commit: $local_sha"
}

title "ZENITH ASTRO STACKER - ASISTENTE DE RELEASE MAC"
require_command node "Instala Node.js 20 o superior."
require_command npm "Instala Node.js 20 o superior."
require_command cargo "Instala Rust con rustup."
require_command git "Instala las herramientas de Xcode o Git."

for required_file in \
  scripts/release/build-macos-release.sh \
  scripts/windows-release-assistant.ps1 \
  package.json \
  src-tauri/tauri.conf.json \
  src-tauri/Cargo.toml; do
  [[ -e "$required_file" ]] || fail "Falta $required_file"
done

assert_versions_match
CURRENT_VERSION="$(get_version)"

if [[ "$CHECK_ONLY" == "1" ]]; then
  echo "Modo --check: estructura y versiones correctas; no se cambio ni subio nada."
  exit 0
fi

echo "Version actual: $CURRENT_VERSION"
read -r -p "Nueva version (Enter para conservar $CURRENT_VERSION): " REQUESTED_VERSION
REQUESTED_VERSION="${REQUESTED_VERSION#v}"
VERSION="${REQUESTED_VERSION:-$CURRENT_VERSION}"
[[ "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || fail "Usa una version como 0.2.8, sin la letra v."

if [[ "$VERSION" != "$CURRENT_VERSION" ]]; then
  step "Actualizando $CURRENT_VERSION -> $VERSION"
  set_project_version "$VERSION"
  assert_versions_match
else
  echo "Se conserva la version $VERSION."
fi

DEFAULT_TAG="v${VERSION}-Beta"
read -r -p "Tag para actualizadores y Release [$DEFAULT_TAG]: " TARGET_TAG
TARGET_TAG="${TARGET_TAG:-$DEFAULT_TAG}"
[[ "$TARGET_TAG" != *" "* ]] || fail "El tag no puede contener espacios."

preserve_remote_latest "$TARGET_TAG" "$VERSION"

echo ""
echo "Compilacion macOS:"
echo "  1) Apple Silicon + Intel (recomendado)"
echo "  2) Solo Apple Silicon"
echo "  3) Solo Intel"
echo "  4) Omitir compilacion"
while true; do
  read -r -p "Opcion [1]: " BUILD_MODE
  BUILD_MODE="${BUILD_MODE:-1}"
  [[ "$BUILD_MODE" =~ ^[1-4]$ ]] && break
  echo "Elige 1, 2, 3 o 4."
done

case "$BUILD_MODE" in
  1)
    GITHUB_RELEASE_TAG="$TARGET_TAG" npm run release:mac
    GITHUB_RELEASE_TAG="$TARGET_TAG" npm run release:mac:intel
    ;;
  2) GITHUB_RELEASE_TAG="$TARGET_TAG" npm run release:mac ;;
  3) GITHUB_RELEASE_TAG="$TARGET_TAG" npm run release:mac:intel ;;
  4) echo "Compilacion omitida por el usuario." ;;
esac

if ask_yes_no "Deseas publicar/actualizar ahora el Release $TARGET_TAG" N; then
  publish_release "$VERSION" "$TARGET_TAG"
else
  echo "No se publico ningun Release."
fi

if ask_yes_no "Deseas subir el proyecto al repositorio privado" N; then
  push_project "$VERSION"
else
  echo "No se subio el proyecto."
fi

title "SIGUIENTE PASO EN WINDOWS"
echo "macOS no genera el instalador NSIS nativo de Windows."
echo "En Windows actualiza el proyecto y ejecuta:"
echo '  powershell -ExecutionPolicy Bypass -File .\scripts\windows-release-assistant.ps1'
echo "Ese asistente compila Windows y, si lo autorizas, agrega sus tres archivos"
echo "al mismo Release sin reemplazar los paquetes macOS."
echo ""
echo "Proceso de Mac terminado."
