import { defineConfig } from "vite";

export default defineConfig({
  // Evita que Vite oculte errores de Rust
  clearScreen: false,
  server: {
    port: 5173,      // Forzamos el puerto 5173 siempre
    strictPort: true, // Si el 5173 está ocupado, falla en vez de cambiarlo
    host: true,
  },
  envPrefix: ["VITE_", "TAURI_"], // Permite leer variables de entorno de Tauri
  build: {
    target: "esnext", // Optimización para navegadores modernos (Webview)
    minify: !process.env.TAURI_DEBUG ? "esbuild" : false,
    sourcemap: !!process.env.TAURI_DEBUG,
  },
});