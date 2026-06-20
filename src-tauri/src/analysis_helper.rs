fn process_analysis_frame(
    i: usize,
    raw: &[u8],
    buffers: &mut AnalysisBufferSet,
    roi_x: usize,
    roi_y: usize,
    roi_img_w: usize,
    roi_img_h: usize,
    width: usize,
    height: usize,
    bpp: usize,
    is_surface: bool,
    anchor_pyramid_ref: Option<&Vec<u16>>,
    anchor_mono_ref: &Vec<u16>,
    cog_cx: f32, // NEW
    cog_cy: f32, // NEW
) -> FrameAlignmentData {
    if raw.is_empty() {
        return FrameAlignmentData::empty(i);
    }

    let mut qual_score = 0u64;
    let mut dx = 0.0;
    let mut dy = 0.0;

    if !is_surface {
        // PLANETARY V2: COG
        let stride = 4;
        let mut sum_x = 0.0;
        let mut sum_y = 0.0;
        let mut sum_w = 0.0;

        raw_to_u16_buffer_into(raw, roi_img_w, roi_img_h, bpp, &mut buffers.raw_u16);
        let mono = &buffers.raw_u16;

        for y in (0..roi_img_h).step_by(stride) {
            for x in (0..roi_img_w).step_by(stride) {
                let idx = y * roi_img_w + x;
                let val = mono[idx] as f32;
                if val > 1000.0 {
                    sum_x += x as f32 * val;
                    sum_y += y as f32 * val;
                    sum_w += val;
                }
            }
        }

        if sum_w > 0.0 {
            let cog_x_local = sum_x / sum_w;
            let cog_y_local = sum_y / sum_w;
            let cog_x_global = roi_x as f32 + cog_x_local;
            let cog_y_global = roi_y as f32 + cog_y_local;

            dx = (width as f32 / 2.0) - cog_x_global;
            dy = (height as f32 / 2.0) - cog_y_global;

            qual_score =
                calculate_quality_metric_from_buffer(&buffers.raw_u16, roi_img_w, roi_img_h);
        }
    } else {
        // SURFACE V2
        raw_to_u16_buffer_into(raw, roi_img_w, roi_img_h, bpp, &mut buffers.raw_u16);

        let score_val = enhance_and_score_surface_buffered(
            &buffers.raw_u16,
            roi_img_w,
            roi_img_h,
            &mut buffers.blur_temp,
            &mut buffers.blur_out,
            &mut buffers.lap_out,
        );

        let search_w = roi_img_w / 2;
        let search_h = roi_img_h / 2;
        let search_x = (roi_img_w - search_w) / 2;
        let search_y = (roi_img_h - search_h) / 2;

        let (sdx, sdy) = if let Some(pyr) = anchor_pyramid_ref {
            find_best_match_sad_pyramid(
                anchor_mono_ref,
                &buffers.lap_out,
                pyr,
                roi_img_w,
                roi_img_h,
                roi_img_w / 2,
                roi_img_h / 2,
                search_x,
                search_y,
                search_w,
                search_h,
                128,
                2,
                16,
            )
        } else {
            (0.0, 0.0)
        };

        dx = sdx;
        dy = sdy;
        // In the helper, we just stabilize in place. Advanced planetary centering is in main.rs.
        qual_score = score_val;
    }

    FrameAlignmentData {
        frame_idx: i,
        global_shift: (dx, dy),
        local_shifts: vec![],
        idx: i,
        x_shift: dx,
        y_shift: dy,
        score: qual_score,
        added: false,
    }
}
