file_path = r"g:\SOFTWARE by EMG\astro-stacker PROYECTO\astro-stacker\src-tauri\src\commands_v2_v3.rs"

with open(file_path, "r", encoding="utf-8") as f:
    lines = f.readlines()

stack = []
for i, line in enumerate(lines):
    for j, char in enumerate(line):
        if char == "{":
            stack.append(("{", i + 1, j + 1))
        elif char == "}":
            if not stack:
                print(f"Extra '}}' at line {i+1}, col {j+1}")
            else:
                stack.pop()
        elif char == "(":
            stack.append(("(", i + 1, j + 1))
        elif char == ")":
            if not stack:
                print(f"Extra ')' at line {i+1}, col {j+1}")
            else:
                top, li, co = stack.pop()
                if top != "(":
                    print(f"Mismatch: ')' at line {i+1}, col {j+1} closes '{top}' from line {li}, col {co}")

if stack:
    for char, li, co in stack:
        print(f"Unclosed '{char}' from line {li}, col {co}")
