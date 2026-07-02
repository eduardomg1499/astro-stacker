# Zenith Astro Stacker

Aplicacion de escritorio Tauri + Vite + Rust para apilado y procesamiento de video astronomico.

## Desarrollo en macOS

Requisitos recomendados:

- Node.js 20 o superior
- Rust estable con Cargo
- FFmpeg y FFprobe disponibles en `PATH`

Setup rapido:

```bash
brew install node rust ffmpeg
npm ci
npm run tauri:dev
```

Para verificar sin abrir la app:

```bash
npm run check
```

### Asistente de version, compilacion y publicacion para macOS

El asistente `scripts/mac-release-assistant.sh` permite conservar o cambiar la
version, compilar Apple Silicon e Intel con los scripts existentes, publicar u
omitir el Release y subir por separado el proyecto al repositorio privado.
Tambien protege una entrada Windows ya existente en `latest.json`.

```bash
./scripts/mac-release-assistant.sh
```

Para comprobar el script y la coherencia de versiones sin modificar nada:

```bash
./scripts/mac-release-assistant.sh --check
```

El instalador Windows se genera nativamente con
`scripts/windows-release-assistant.ps1`; el asistente de Mac muestra su comando
al finalizar.

## Desarrollo en Windows

Requisitos recomendados:

- Node.js 20 o superior
- Rust estable con toolchain MSVC
- WebView2 Runtime
- FFmpeg y FFprobe en `PATH`, o en `src-tauri/bin`

Comandos:

```powershell
npm ci
npm run tauri:dev
```

### Asistente de release para Windows

El script `scripts/windows-release-assistant.ps1` automatiza la configuracion o
actualizacion del repositorio, FFmpeg, el build NSIS firmado, la conservacion de
las entradas macOS de `latest.json`, la subida al Release y su validacion final.

En un repositorio ya descargado, colocalo en `scripts\` y ejecuta desde la raiz:

```powershell
powershell -ExecutionPolicy Bypass -File .\scripts\windows-release-assistant.ps1
```

Para una configuracion desde cero, ejecuta una copia del script desde fuera de
la carpeta destino y elige la opcion 1. La ruta predeterminada es:

```text
C:\Users\picis\Desktop\Proyecto-Zenith-Astro-Stacker
```

## FFmpeg por plataforma

Durante desarrollo, la app usa `ffmpeg` y `ffprobe` desde el `PATH`.

Para builds empaquetados puedes colocar binarios locales en `src-tauri/bin`:

- Windows: `ffmpeg.exe` y `ffprobe.exe`
- macOS/Linux: `ffmpeg` y `ffprobe`

Los binarios de `src-tauri/bin` estan ignorados por Git porque son pesados y especificos de sistema operativo. Tambien puedes forzar rutas concretas con:

```bash
export ZENITH_FFMPEG_PATH=/ruta/a/ffmpeg
export ZENITH_FFPROBE_PATH=/ruta/a/ffprobe
```

## Git

Este repo debe subir codigo fuente, configuracion, `package-lock.json`, `Cargo.lock`, iconos y assets propios. No debe subir `node_modules`, `.vs`, `dist`, `src-tauri/target` ni binarios FFmpeg locales.

Si esos artefactos ya quedaron agregados al indice, limpialos sin borrarlos del disco:

```bash
git rm -r --cached node_modules .vs dist src-tauri/target
git rm --cached src-tauri/bin/ffmpeg.exe src-tauri/bin/ffprobe.exe
git add .gitignore .gitattributes .editorconfig README.md package.json package-lock.json src-tauri/Cargo.toml src-tauri/Cargo.lock src-tauri/src src-tauri/tauri.conf.json src-tauri/capabilities src-tauri/icons src-tauri/vendor src src-tauri/bin/README.md .github
git status
```
