import { defineConfig } from "vite";

const isDebugBuild = !!(process.env.TAURI_ENV_DEBUG || process.env.TAURI_DEBUG);

export default defineConfig({
  // Evita que Vite oculte errores de Rust
  clearScreen: false,
  server: {
    port: 5173,      // Forzamos el puerto 5173 siempre
    strictPort: true, // Si el 5173 est� ocupado, falla en vez de cambiarlo
    host: true,
  },
  envPrefix: ["VITE_", "TAURI_"], // Permite leer variables de entorno de Tauri
  build: {
    target: "esnext", // Optimizacion para navegadores modernos (Webview)
    // Tauri v2 exporta TAURI_ENV_DEBUG; TAURI_DEBUG era el nombre de la v1.
    // Mirar solo el nombre viejo hacia que `tauri build --debug` saliera
    // igualmente minificado y sin sourcemaps, y con eso una pila de error de
    // produccion es ilegible (nombres tipo 'Br'). Aceptamos los dos.
    minify: isDebugBuild ? false : "esbuild",
    sourcemap: isDebugBuild,
  },
});