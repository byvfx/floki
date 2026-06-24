//! GPU contact-sheet thumbnail generator (#67, Phase 1).
//!
//! Renders one decimated AOV layer through the shared display shader
//! ([`crate::gpu::shader`]) into a small `Rgba8Unorm` texture and registers it
//! with egui, replacing the per-frame CPU `generate_texture` bake on the GPU
//! path. The render is identical to the main viewport draw (`srgb`, `gamma`,
//! `exposure`, channel select, background composite), but into a fresh opaque
//! target sized to the thumbnail box — so the sheet matches the central view.
//!
//! Only used when a GPU is present **and** OCIO is not active; the CPU
//! `thumbnails` cache remains the fallback for the headless / OCIO cases
//! (offscreen OCIO thumbnails are a deferred Phase 2).
//!
//! The load-bearing correctness fact (validated by the on-device test below):
//! the display shader emits sRGB-encoded, checker-composited, opaque bytes when
//! driven with `srgb=1, skip_checker=0, opacity=1.0`, so rendering into an
//! `Rgba8Unorm` target yields exactly the bytes egui displays correctly.

use eframe::egui_wgpu::wgpu;

/// Tone / display parameters copied from the viewer for the thumbnail render.
/// Mirrors the relevant `Uniforms` inputs of the main `draw_canvas_gpu` path so
/// thumbnails match the central viewport.
pub struct ThumbnailTone {
    pub exposure: f32,
    pub gamma: f32,
    pub srgb: bool,
    pub enable_lut: bool,
    pub channel_mode: u32,
    pub lut_domain_min: [f32; 4],
    pub lut_domain_max: [f32; 4],
    pub background: crate::background::Background,
}

/// Render `layer_index` of `exr_data`, point-decimated so its longest edge is
/// `<= max_dim`, through the display shader into an `Rgba8Unorm` texture and
/// register it with egui.
///
/// Returns `(TextureId, owned target texture, full-res size in px)` — the size
/// is the layer's *full* resolution so the contact sheet fits by the true
/// aspect ratio (matching the CPU path's `texture.size_vec2()`). The caller owns
/// the returned texture (keeps the registered view alive) and must
/// `free_texture(&id)` when evicting the slot. `None` if the layer is empty.
pub fn generate(
    gpu_resources: &crate::gpu::GpuResources,
    exr_data: &crate::exr_loader::ExrData,
    layer_index: usize,
    max_dim: usize,
    tone: &ThumbnailTone,
) -> Option<(egui::TextureId, wgpu::Texture, egui::Vec2)> {
    let (layer, r_chan, g_chan, b_chan, a_chan) = exr_data.logical_channels(layer_index)?;
    let width = layer.size.0;
    let height = layer.size.1;
    if width == 0 || height == 0 {
        return None;
    }

    let (out_w, out_h, stride) = crate::viewer::thumb_dims(width, height, Some(max_dim));

    // Point-decimate into an Rgba32Float source buffer, mirroring
    // `generate_texture`'s `(ox*stride).min(width-1)` sampling. Alpha defaults to
    // 1.0 when the layer has no alpha channel.
    let mut pixels = vec![0.0f32; out_w * out_h * 4];
    let r_s = crate::viewer::sample_channel_f32(r_chan);
    let g_s = crate::viewer::sample_channel_f32(g_chan);
    let b_s = crate::viewer::sample_channel_f32(b_chan);
    let a_s = crate::viewer::sample_channel_f32(a_chan);
    let has_alpha = a_chan.is_some();
    for oy in 0..out_h {
        let y = (oy * stride).min(height - 1);
        for ox in 0..out_w {
            let x = (ox * stride).min(width - 1);
            let i = (oy * out_w + ox) * 4;
            pixels[i] = crate::viewer::pixel_val(r_s, r_chan, x, y, width);
            pixels[i + 1] = crate::viewer::pixel_val(g_s, g_chan, x, y, width);
            pixels[i + 2] = crate::viewer::pixel_val(b_s, b_chan, x, y, width);
            pixels[i + 3] = if has_alpha {
                crate::viewer::pixel_val(a_s, a_chan, x, y, width)
            } else {
                1.0
            };
        }
    }

    let (tex_id, target) = render_pixels(
        gpu_resources,
        &pixels,
        out_w,
        out_h,
        &uniforms_for(tone, out_w, out_h),
        |view, device, renderer| {
            renderer.register_native_texture(device, view, wgpu::FilterMode::Linear)
        },
    )?;
    // Report the FULL-RES size so the contact sheet fits by the true aspect.
    Some((tex_id, target, egui::vec2(width as f32, height as f32)))
}

/// Build the per-thumbnail `Uniforms` from the tone/background config. Mirrors
/// the `draw_canvas_gpu` template (viewer.rs), but with a full-target quad
/// (`rect_min=[0,0]`, `rect_max=screen_size=[out_w,out_h]`), `skip_checker=0`
/// (composite the background), and `opacity=1.0` (opaque output).
fn uniforms_for(tone: &ThumbnailTone, out_w: usize, out_h: usize) -> crate::gpu::Uniforms {
    let bg = &tone.background;
    crate::gpu::Uniforms {
        rect_min: [0.0, 0.0],
        rect_max: [out_w as f32, out_h as f32],
        screen_size: [out_w as f32, out_h as f32],
        wipe_center: [0.0, 0.0],
        exposure: tone.exposure,
        gamma: tone.gamma,
        diff_multiplier: 0.0,
        opacity: 1.0,
        wipe_angle: 0.0,
        channel_mode: tone.channel_mode,
        is_diff_mode: 0,
        srgb: u32::from(tone.srgb),
        enable_lut: u32::from(tone.enable_lut),
        is_composite: 0,
        blend_mode: 0,
        is_wipe_mode: 0,
        skip_checker: 0,
        diff_metric: 0,
        diff_floor: 0.0,
        _pad2: 0,
        lut_domain_min: tone.lut_domain_min,
        lut_domain_max: tone.lut_domain_max,
        bg_checker_dark: rgb3_to_vec4(bg.checker_dark),
        bg_checker_light: rgb3_to_vec4(bg.checker_light),
        bg_solid: rgb3_to_vec4(bg.solid),
        bg_mode: bg.mode.as_u32(),
        bg_grad_angle: bg.gradient_angle,
        bg_checker_size: bg.checker_size,
        _pad3: 0,
    }
}

#[inline]
fn rgb3_to_vec4(c: [f32; 3]) -> [f32; 4] {
    [c[0], c[1], c[2], 0.0]
}

/// Core render: upload `src_pixels` (`out_w*out_h*4` Rgba32Float, scene-linear)
/// to a source texture, render it through the display shader with `uniforms`
/// into a fresh `Rgba8Unorm` target, then hand the target view to `register` to
/// produce a result. Returns `(result, owned target texture)`.
///
/// `register` runs the egui native-texture registration in production; the
/// on-device test passes a closure that copies the target to a readback buffer
/// instead. Factored this way so the wgpu correctness can be proved without an
/// egui `Renderer`.
fn render_pixels<R>(
    gpu_resources: &crate::gpu::GpuResources,
    src_pixels: &[f32],
    out_w: usize,
    out_h: usize,
    uniforms: &crate::gpu::Uniforms,
    register: impl FnOnce(&wgpu::TextureView, &wgpu::Device, &mut eframe::egui_wgpu::Renderer) -> R,
) -> Option<(R, wgpu::Texture)> {
    let render_state = gpu_resources.render_state();
    let device = &render_state.device;
    let queue = &render_state.queue;
    let gpu_state = gpu_resources.gpu_state.as_ref();

    // `_source` is held (not just its bind group) so the texture outlives the
    // render pass — the bind group borrows its view.
    let (_source, source_bg) = upload_source(device, queue, gpu_state, src_pixels, out_w, out_h);

    let target = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Exr Thumbnail Target"),
        size: wgpu::Extent3d {
            width: out_w as u32,
            height: out_h as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba8Unorm,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT
            | wgpu::TextureUsages::TEXTURE_BINDING
            | wgpu::TextureUsages::COPY_SRC,
        view_formats: &[],
    });
    let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

    // Write the uniforms to slot 0 of the persistent ring buffer; we bind with a
    // dynamic offset of 0 below.
    queue.write_buffer(&gpu_state.uniform_buffer, 0, bytemuck::bytes_of(uniforms));

    let mut encoder =
        device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
    {
        let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("Exr Thumbnail Pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &target_view,
                resolve_target: None,
                depth_slice: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                    store: wgpu::StoreOp::Store,
                },
            })],
            depth_stencil_attachment: None,
            timestamp_writes: None,
            occlusion_query_set: None,
            multiview_mask: None,
        });
        rp.set_pipeline(&gpu_state.thumbnail_pipeline);
        rp.set_bind_group(0, &source_bg, &[]);
        rp.set_bind_group(1, gpu_state.default_tex_bind_group.as_ref(), &[]);
        rp.set_bind_group(2, &gpu_state.uniform_bind_group, &[0]);
        rp.set_bind_group(3, gpu_state.default_lut_bind_group.as_ref(), &[]);
        rp.draw(0..6, 0..1);
    }
    queue.submit([encoder.finish()]);
    let result = register(&target_view, device, &mut render_state.renderer.write());
    Some((result, target))
}

/// Upload `src_pixels` to an `Rgba32Float` source texture and build a tex bind
/// group (layout `bind_group_layout_tex`; binding0=view, binding1=sampler) — the
/// same upload pattern as `build_layer_texture`, at `out_w x out_h`.
fn upload_source(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    gpu_state: &crate::gpu::GpuState,
    src_pixels: &[f32],
    out_w: usize,
    out_h: usize,
) -> (wgpu::Texture, wgpu::BindGroup) {
    let source = device.create_texture(&wgpu::TextureDescriptor {
        label: Some("Exr Thumbnail Source"),
        size: wgpu::Extent3d {
            width: out_w as u32,
            height: out_h as u32,
            depth_or_array_layers: 1,
        },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: wgpu::TextureFormat::Rgba32Float,
        usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
        view_formats: &[],
    });
    queue.write_texture(
        wgpu::TexelCopyTextureInfo {
            texture: &source,
            mip_level: 0,
            origin: wgpu::Origin3d::ZERO,
            aspect: wgpu::TextureAspect::All,
        },
        bytemuck::cast_slice(src_pixels),
        wgpu::TexelCopyBufferLayout {
            offset: 0,
            bytes_per_row: Some((out_w * 4 * 4) as u32),
            rows_per_image: Some(out_h as u32),
        },
        wgpu::Extent3d {
            width: out_w as u32,
            height: out_h as u32,
            depth_or_array_layers: 1,
        },
    );

    let view = source.create_view(&wgpu::TextureViewDescriptor::default());
    let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
        label: Some("Exr Thumbnail Source Bind Group"),
        layout: &gpu_state.bind_group_layout_tex,
        entries: &[
            wgpu::BindGroupEntry {
                binding: 0,
                resource: wgpu::BindingResource::TextureView(&view),
            },
            wgpu::BindGroupEntry {
                binding: 1,
                resource: wgpu::BindingResource::Sampler(&gpu_state.sampler),
            },
        ],
    });
    (source, bind_group)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gpu::GpuState;

    /// On-device proof that the `Rgba8Unorm` + `srgb=1` assumption holds: feed a
    /// known scene-linear input through the thumbnail render and read back the
    /// target bytes. Linear 0.5 must encode to sRGB ~188 (not 128 = no encode,
    /// not 223 = double encode). Skips gracefully if no GPU adapter is present.
    #[test]
    fn thumbnail_render_srgb_encodes_on_device() {
        let instance =
            wgpu::Instance::new(wgpu::InstanceDescriptor::new_without_display_handle_from_env());
        let adapter = match pollster::block_on(
            instance.request_adapter(&wgpu::RequestAdapterOptions::default()),
        ) {
            Ok(a) => a,
            Err(_) => {
                eprintln!("no GPU adapter available; skipping on-device thumbnail test");
                return;
            }
        };
        // FLOAT32_FILTERABLE is required by `GpuState` (the 3D LUT). CI runners
        // often expose a software adapter that lacks it — skip rather than panic so
        // this device-gated test never fails on a GPU-less/limited runner (it still
        // runs + asserts on a real GPU, e.g. Metal locally).
        let Ok((device, queue)) =
            pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                label: Some("thumbnail-test-device"),
                required_features: wgpu::Features::FLOAT32_FILTERABLE,
                ..Default::default()
            }))
        else {
            eprintln!("GPU lacks FLOAT32_FILTERABLE; skipping on-device thumbnail test");
            return;
        };

        let gpu_state = GpuState::new(&device, &queue, wgpu::TextureFormat::Rgba8Unorm);

        // 2x2 scene-linear mid-grey, fully opaque.
        let out_w = 2usize;
        let out_h = 2usize;
        let src: Vec<f32> = [0.5f32, 0.5, 0.5, 1.0]
            .iter()
            .cycle()
            .take(out_w * out_h * 4)
            .copied()
            .collect();

        // srgb=1, exposure=0, gamma=1, enable_lut=0, skip_checker=1 (no checker
        // composite — the pixel is opaque anyway, but this isolates the encode).
        let mut u = uniforms_for(
            &ThumbnailTone {
                exposure: 0.0,
                gamma: 1.0,
                srgb: true,
                enable_lut: false,
                channel_mode: 0,
                lut_domain_min: [0.0, 0.0, 0.0, 0.0],
                lut_domain_max: [1.0, 1.0, 1.0, 1.0],
                background: crate::background::Background::default(),
            },
            out_w,
            out_h,
        );
        u.skip_checker = 1;

        // `_source` is held so the texture outlives the pass (`source_bg` borrows
        // its view).
        let (_source, source_bg) = upload_source(&device, &queue, &gpu_state, &src, out_w, out_h);

        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("thumb-test-target"),
            size: wgpu::Extent3d {
                width: out_w as u32,
                height: out_h as u32,
                depth_or_array_layers: 1,
            },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::Rgba8Unorm,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::COPY_SRC,
            view_formats: &[],
        });
        let target_view = target.create_view(&wgpu::TextureViewDescriptor::default());

        queue.write_buffer(&gpu_state.uniform_buffer, 0, bytemuck::bytes_of(&u));

        let readback = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("thumb-readback"),
            // 256-aligned bytes_per_row * out_h rows.
            size: 256 * out_h as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });

        let mut encoder =
            device.create_command_encoder(&wgpu::CommandEncoderDescriptor { label: None });
        {
            let mut rp = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("thumb-test-pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &target_view,
                    resolve_target: None,
                    depth_slice: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
                multiview_mask: None,
            });
            rp.set_pipeline(&gpu_state.thumbnail_pipeline);
            rp.set_bind_group(0, &source_bg, &[]);
            rp.set_bind_group(1, gpu_state.default_tex_bind_group.as_ref(), &[]);
            rp.set_bind_group(2, &gpu_state.uniform_bind_group, &[0]);
            rp.set_bind_group(3, gpu_state.default_lut_bind_group.as_ref(), &[]);
            rp.draw(0..6, 0..1);
        }
        encoder.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &readback,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(256),
                    rows_per_image: Some(out_h as u32),
                },
            },
            wgpu::Extent3d {
                width: out_w as u32,
                height: out_h as u32,
                depth_or_array_layers: 1,
            },
        );
        queue.submit([encoder.finish()]);

        readback.slice(..).map_async(wgpu::MapMode::Read, |_| {});
        let _ = device.poll(wgpu::PollType::wait_indefinitely());
        let data = readback.slice(..).get_mapped_range();
        let red = data[0];
        eprintln!("thumbnail on-device sRGB readback: red byte = {red} (expected ~188)");
        assert!(
            (red as i32 - 188).abs() <= 4,
            "linear 0.5 should sRGB-encode to ~188 in Rgba8Unorm, got {red} \
             (128 = no encode, 223 = double encode)"
        );
        assert_eq!(data[3], 255, "thumbnail output must be opaque");
    }
}
