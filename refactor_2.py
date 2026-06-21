import re

with open('src-tauri/src/main.rs', 'r', encoding='utf-8') as f:
    original = f.read()

sections = [
    ('alignment_helpers.rs', r'// 3\. ALINEACION', r'// 4\.5 LIQUID WRAPING & ADVANCED WAVELETS'),
    ('advanced_wavelets.rs', r'// 4\.5 LIQUID WRAPING & ADVANCED WAVELETS', r'// 6\. COMANDOS TAURI \(EXPORTADOS\)'),
    ('commands_core.rs', r'// 6\. COMANDOS TAURI \(EXPORTADOS\)', r'// 7\. ZENITH V2 IMPLEMENTATION \(NEW\)'),
    ('commands_v2_v3.rs', r'// 7\. ZENITH V2 IMPLEMENTATION \(NEW\)', r'// 8\. SMART AP GENERATOR \(Integrated\)'),
    ('smart_ap_generator.rs', r'// 8\. SMART AP GENERATOR \(Integrated\)', r'(?s).*'), # to EOF
]

new_main = original

for filename, start_marker, end_marker in sections:
    start_match = re.search(start_marker, new_main)
    if end_marker == r'(?s).*':
        end_match = type('obj', (object,), {'start': lambda: len(new_main)})()
    else:
        end_match = re.search(end_marker, new_main)
    
    if start_match and end_match:
        header_start = new_main.rfind('// ==========================================', 0, start_match.start())
        if header_start == -1: header_start = start_match.start()
        
        if end_marker == r'(?s).*':
            header_end = len(new_main)
        else:
            header_end = new_main.rfind('// ==========================================', 0, end_match.start())
            if header_end == -1: header_end = end_match.start()
        
        extracted = new_main[header_start:header_end]
        
        with open('src-tauri/src/' + filename, 'w', encoding='utf-8') as f:
            f.write(extracted)
            
        new_main = new_main[:header_start] + f'\ninclude!(\"{filename}\");\n\n' + new_main[header_end:]
        print(f'Extracted {filename}')

with open('src-tauri/src/main_temp2.rs', 'w', encoding='utf-8') as f:
    f.write(new_main)
