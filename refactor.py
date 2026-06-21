import re

with open('src-tauri/src/main.rs', 'r', encoding='utf-8') as f:
    original = f.read()

sections = [
    ('types.rs', r'// 1\. ESTRUCTURAS DE DATOS', r'// 2\. UTILIDADES BASE'),
    ('core_utils.rs', r'// 2\. UTILIDADES BASE', r'// 3\. ALINEACION'),
    ('debayer.rs', r'// 4\. DEBAYER', r'// 4\.5 LIQUID WRAPING & ADVANCED WAVELETS'),
    ('filters.rs', r'// 5\. FILTROS Y WAVELETS', r'// 6\. COMANDOS TAURI \(EXPORTADOS\)'),
]

new_main = original

for filename, start_marker, end_marker in sections:
    start_match = re.search(start_marker, new_main)
    end_match = re.search(end_marker, new_main)
    
    if start_match and end_match:
        header_start = new_main.rfind('// ==========================================', 0, start_match.start())
        if header_start == -1: header_start = start_match.start()
        
        header_end = new_main.rfind('// ==========================================', 0, end_match.start())
        if header_end == -1: header_end = end_match.start()
        
        extracted = new_main[header_start:header_end]
        
        with open('src-tauri/src/' + filename, 'w', encoding='utf-8') as f:
            f.write(extracted)
            
        new_main = new_main[:header_start] + f'\ninclude!(\"{filename}\");\n\n' + new_main[header_end:]
        print(f'Extracted {filename}')

with open('src-tauri/src/main_temp.rs', 'w', encoding='utf-8') as f:
    f.write(new_main)
