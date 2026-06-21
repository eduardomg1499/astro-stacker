import os

file_path = r"g:\SOFTWARE by EMG\astro-stacker PROYECTO\astro-stacker\src-tauri\src\liquid_warping.rs"

with open(file_path, "r", encoding="utf-8") as f:
    content = f.read()

# 1. Update LanczosLUT::get
content = content.replace(
    "fn get(&self, x: f32) -> f32 {",
    "fn get(&self, x: f32, drop_size: f32) -> f32 {"
)
content = content.replace(
    "let ax = x.abs();",
    "let ax = (x / drop_size).abs();"
)

# 2. Update functions signatures and calls
content = content.replace(
    "sigma_scale: f32, // NEW: 1.0=normal, >1 = more permissive (for surface/lunar)\n)",
    "sigma_scale: f32, // NEW: 1.0=normal, >1 = more permissive (for surface/lunar)\n    drop_size: f32,\n)"
)
content = content.replace(
    "sigma_scale: f32, // NEW: 1.0=normal, >1.0=more permissive (for surface/lunar)\n)",
    "sigma_scale: f32, // NEW: 1.0=normal, >1.0=more permissive (for surface/lunar)\n    drop_size: f32,\n)"
)
content = content.replace(
    "quality_weight: f32,\n)",
    "quality_weight: f32,\n    drop_size: f32,\n)"
)

# 3. Update lut.get calls
content = content.replace(
    "lut.get(fx - kx as f32)",
    "lut.get(fx - kx as f32, drop_size)"
)
content = content.replace(
    "lut.get(fy - ky as f32)",
    "lut.get(fy - ky as f32, drop_size)"
)

with open(file_path, "w", encoding="utf-8") as f:
    f.write(content)

print("Patch applied successfully to liquid_warping.rs")
