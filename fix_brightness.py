import re

files = ['src-tauri/src/commands_v2_v3.rs', 'src-tauri/src/commands_core.rs']

for f in files:
    with open(f, 'r', encoding='utf-8') as file:
        content = file.read()
    
    # We will replace the block:
    # if !is_surface && stack_std > 10.0 {
    #     let gain = (ref_std / stack_std).clamp(1.0, 1.25);
    #     for v in &mut ...
    
    # We'll use regex to find the linear match section.
    pattern = re.compile(r'if !is_surface && stack_std > 10\.0 \{(.*?)\}', re.DOTALL)
    
    def replacer(match):
        inner = match.group(1)
        # We rewrite the logic completely.
        new_logic = '''if stack_std > 10.0 {
            // Para alinear el brillo con el original y no oscurecer las sombras:
            // Aplicamos ganancia completa para planetas, y casi unitaria para superficies (para evitar contraste destructivo).
            let gain = if is_surface {
                (ref_std / stack_std).clamp(1.0, 1.02)
            } else {
                (ref_std / stack_std).clamp(1.0, 1.25)
            };
            
            for v in &mut final_u16 {
                let val = *v as f32;
                // Esto levanta el brillo general (stack_mean -> ref_mean) 
                // asegurando que las sombras no se hundan a negro.
                let new_val = (val - stack_mean) * gain + ref_mean;
                *v = new_val.clamp(0.0, 65535.0) as u16;
            }
        }'''
        if 'pre_processed_base' in inner:
            new_logic = new_logic.replace('final_u16', 'pre_processed_base')
        return new_logic
        
    modified = pattern.sub(replacer, content)
    
    with open(f, 'w', encoding='utf-8') as file:
        file.write(modified)
    
    print(f"Patched {f}")
