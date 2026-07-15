use rayon::prelude::*;

#[derive(Clone)]
pub struct IntegralImage {
    pub width: usize,
    pub height: usize,
    pub sum: Vec<u64>,
    pub sq_sum: Vec<u64>,
}

impl IntegralImage {
    /// Compute Integral Image (Summed Area Table) from 8-bit or 16-bit raw data.
    /// Supports u8 (bpp=8) or u16 (bpp=16, packed in u8 slice).
    /// If bpp=16, width is in pixels, but data len is width*height*2.
    pub fn new(data: &[u8], width: usize, height: usize, bpp: usize) -> Self {
        // We compute two tables:
        // 1. Sum (for Mean)
        // 2. Square Sum (for Variance)

        // Basic scalar implementation first for correctness.
        // Parallelized by rows?
        // Integral Image is sequential by definition (Prefix Sum).
        // BUT, vertical pass can be independent of horizontal pass.
        // Pass 1: Horizontal Prefix Sum (Row by Row) -> Parallelizable.
        // Pass 2: Vertical Prefix Sum (Col by Col) -> Parallelizable (if transposed) or just sequential column-wise.

        let num_pixels = width * height;
        let mut sum = vec![0u64; num_pixels];
        let mut sq_sum = vec![0u64; num_pixels];

        // PASS 1: HORIZONTAL SCAN (Parallel by Rows)
        sum.par_chunks_mut(width)
            .zip(sq_sum.par_chunks_mut(width))
            .enumerate()
            .for_each(|(y, (row_sum, row_sq))| {
                let row_offset = y * width * (if bpp == 16 { 2 } else { 1 });

                let mut s: u64 = 0;
                let mut ss: u64 = 0;

                for x in 0..width {
                    // READ PIXEL
                    let val = if bpp == 16 {
                        // LE or BE? Assume LE (standard for Intel/Rust)
                        // data is &[u8].
                        let off = row_offset + x * 2;
                        let low = data[off] as u16;
                        let high = data[off + 1] as u16;
                        (high as u64) << 8 | (low as u64)
                    } else {
                        data[row_offset + x] as u64
                    };

                    s += val;
                    ss += val * val;

                    row_sum[x] = s;
                    row_sq[x] = ss;
                }
            });

        // PASS 2: VERTICAL SCAN (Sequential Column-Wise or Transposed?)
        // Standard SAT definition: I(x,y) = Val(x,y) + I(x-1,y) + I(x,y-1) - I(x-1,y-1).
        // Our Pass 1 did: I'(x,y) = Sum(Row 0..x).
        // Now we need: I(x,y) = I'(x,y) + I(x, y-1).

        // This dependency (y depends on y-1) makes full parallelization hard without "scan" pattern.
        // But width is usually small (e.g. 1920).
        // We can parallelize by COLUMNS independently!
        // Each column is valid to process independently after Pass 1.

        // BUT, Vec is Row-Major. Iterating columns is cache-unfriendly.
        // However, we just need to add `sum[x + (y-1)*width]` to `sum[x + y*width]`.
        // If we process strictly Row-by-Row, we read y-1 (cache hot?) and write y.

        // F3: PARALELO por FRANJAS DE COLUMNAS — cada columna solo depende de
        // sí misma tras la pasada 1, y las franjas anchas conservan la
        // localidad de caché fila a fila dentro de cada hilo. Índices
        // disjuntos por franja → los punteros crudos son sonoros.
        let n_stripes = rayon::current_num_threads().clamp(1, width.max(1));
        let stripe_w = width.div_ceil(n_stripes);
        let sum_addr = sum.as_mut_ptr() as usize;
        let sq_addr = sq_sum.as_mut_ptr() as usize;
        (0..n_stripes).into_par_iter().for_each(|s| {
            let x0 = s * stripe_w;
            let x1 = ((s + 1) * stripe_w).min(width);
            if x0 >= x1 {
                return;
            }
            let p_sum = sum_addr as *mut u64;
            let p_sq = sq_addr as *mut u64;
            for y in 1..height {
                let prev = (y - 1) * width;
                let cur = y * width;
                for x in x0..x1 {
                    unsafe {
                        *p_sum.add(cur + x) += *p_sum.add(prev + x);
                        *p_sq.add(cur + x) += *p_sq.add(prev + x);
                    }
                }
            }
        });

        Self {
            width,
            height,
            sum,
            sq_sum,
        }
    }

    /// Calculate sum of region in O(1).
    /// Region is inclusive-inclusive? Standard Rect usually is x, y, w, h.
    pub fn get_sum(&self, x: usize, y: usize, w: usize, h: usize) -> u64 {
        if w == 0 || h == 0 {
            return 0;
        }

        // Bounds clamping
        let x0 = x; // Left
        let y0 = y; // Top
        let x1 = (x + w - 1).min(self.width - 1); // Right
        let y1 = (y + h - 1).min(self.height - 1); // Bottom

        // SAT Formula: D - B - C + A
        // A = (x0-1, y0-1)
        // B = (x1, y0-1)
        // C = (x0-1, y1)
        // D = (x1, y1)

        let d = self.sum[y1 * self.width + x1];

        let a = if x0 > 0 && y0 > 0 {
            self.sum[(y0 - 1) * self.width + (x0 - 1)]
        } else {
            0
        };
        let b = if y0 > 0 {
            self.sum[(y0 - 1) * self.width + x1]
        } else {
            0
        };
        let c = if x0 > 0 {
            self.sum[y1 * self.width + (x0 - 1)]
        } else {
            0
        };

        // d + a - b - c (watch for underflow if using signed, but unsigned is fine if order corrects)
        // Order: (D + A) - (B + C)
        (d + a).saturating_sub(b + c)
    }

    pub fn get_sq_sum(&self, x: usize, y: usize, w: usize, h: usize) -> u64 {
        if w == 0 || h == 0 {
            return 0;
        }

        let x0 = x;
        let y0 = y;
        let x1 = (x + w - 1).min(self.width - 1);
        let y1 = (y + h - 1).min(self.height - 1);

        let d = self.sq_sum[y1 * self.width + x1];
        let a = if x0 > 0 && y0 > 0 {
            self.sq_sum[(y0 - 1) * self.width + (x0 - 1)]
        } else {
            0
        };
        let b = if y0 > 0 {
            self.sq_sum[(y0 - 1) * self.width + x1]
        } else {
            0
        };
        let c = if x0 > 0 {
            self.sq_sum[y1 * self.width + (x0 - 1)]
        } else {
            0
        };

        (d + a).saturating_sub(b + c)
    }

    /// Returns Variance and Mean of a region in O(1).
    pub fn get_stats(&self, x: usize, y: usize, w: usize, h: usize) -> (f64, f64) {
        let count = (w * h) as f64;
        if count == 0.0 {
            return (0.0, 0.0);
        }

        let s = self.get_sum(x, y, w, h) as f64;
        let ss = self.get_sq_sum(x, y, w, h) as f64;

        let mean = s / count;
        let variance = (ss / count) - (mean * mean);

        (variance.max(0.0), mean)
    }
}
