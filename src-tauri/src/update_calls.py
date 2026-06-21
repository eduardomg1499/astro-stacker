import os

file_path = r"g:\SOFTWARE by EMG\astro-stacker PROYECTO\astro-stacker\src-tauri\src\commands_v2_v3.rs"

with open(file_path, "r", encoding="utf-8") as f:
    content = f.read()

# 1. Update accumulate_frame_rigid_lanczos calls
content = content.replace(
    "render_dy,\n                                q_weight,",
    "render_dy,\n                                q_weight,\n                                0.75, // Drizzle Drop Size"
)

# 2. Update accumulate_frame_liquid calls
content = content.replace(
    "&custom_points,\n                                sigma_scale,",
    "&custom_points,\n                                sigma_scale,\n                                0.75, // Drizzle Drop Size"
)

with open(file_path, "w", encoding="utf-8") as f:
    f.write(content)

print("Function calls updated in commands_v2_v3.rs")
