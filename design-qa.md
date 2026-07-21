# Verificacion visual: ajustes planetarios

- Fecha: 2026-07-18
- Estado probado: modal Configuracion General, idioma espanol, seccion Computo planetario.
- Viewport: 868 x 738 px.
- Referencia del fallo: `/var/folders/6q/3jzj3gmn19g0x_78vytvp_nh0000gn/T/TemporaryItems/NSIRD_screencaptureui_Z342iE/Captura de pantalla 2026-07-18 a la(s) 4.21.42 p.m..png`
- Captura corregida: `tmp/ui-settings-layout-after.png`
- Comparacion lado a lado: `tmp/ui-settings-layout-comparison.png`

## Revision

- La etiqueta y la ayuda de rigor AP ya no colapsan en una columna de una palabra.
- Los selectores de modo, calidad y decode permanecen dentro del panel sin solaparse.
- El texto conserva jerarquia, espaciado y colores del sistema visual existente.
- La fila pasa a una sola columna cuando el contenedor es estrecho.
- No se observaron recortes ni desbordamientos en el estado probado.

## Resultado final

PASSED
