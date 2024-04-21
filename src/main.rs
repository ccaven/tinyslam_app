/*
DONE:
 - Camera streaming to winit window
 - Vertex/fragment shader drawing the contents of a storage texture

TODO:
 - Connect ORBFeatureExtractor pipeline
 - Implement proper BRIEF feature descriptors
    - Step 1: Slightly blur image texture
        - Two passes, one for blur x and one for blur y
    - Step 2: Integral image
        - log2(max(width, height)) passes
    - Step 3: Detect FAST corners
        - Corner detection pass + however many merge passes are needed
    - Step 4: BRIEF descriptors
        - One pass, write to pre-allocated texture, no need for atomics
    - Step 5: Feature matching
        - Brute force for now

*/

use std::sync::Arc;

use winit::{
    event::{Event, WindowEvent},
    event_loop::EventLoop,
    window::Window,
};

use nokhwa::{pixel_format::RgbAFormat, utils::RequestedFormat, Camera};

use orbslam_gpu::orb_2::{OrbConfig, OrbParams, OrbProgram};

use tiny_wgpu::{BindGroupItem, Compute, ComputeProgram};

struct VisualizationProgram<'a> {
    pub surface: wgpu::Surface<'a>,
    pub program: tiny_wgpu::ComputeProgram<'a>
}

impl VisualizationProgram<'_> {
    pub fn new(compute: Arc<Compute>, output_image_size: wgpu::Extent3d, window: Arc<Window>) -> Self {
        let surface = compute.instance.create_surface(window).unwrap();

        let mut program = ComputeProgram::new(compute.clone());

        program.add_module("draw_texture", wgpu::include_wgsl!("draw_texture.wgsl"));

        program.add_texture(
            "texture",
            wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            wgpu::TextureFormat::Rgba8Unorm,
            output_image_size
        );

        program.add_bind_group("draw_texture", &[
            BindGroupItem::Texture { label: "texture" }
        ]);

        let swapchain_capabilities = surface.get_capabilities(&compute.adapter);
        let swapchain_format = swapchain_capabilities.formats[0];

        program.add_render_pipelines("draw_texture", &["draw_texture"], &[("draw_texture", ("vs_main", "fs_main"))], &[], &[Some(swapchain_format.into())], &[]);

        Self {
            surface,
            program
        }
    }
}

async fn run(
    event_loop: EventLoop<()>,
    window: Arc<Window>,
) -> Result<(), winit::error::EventLoopError> {

    let mut camera = {
        let mut camera = Camera::new(
            nokhwa::utils::CameraIndex::Index(0),
            RequestedFormat::new::<nokhwa::pixel_format::RgbAFormat>(
                //nokhwa::utils::RequestedFormatType::HighestFrameRate(30)
                // nokhwa::utils::RequestedFormatType::Exact(
                //     CameraFormat::new(
                //         //Resolution::new(320, 240),
                //         Resolution::new(800, 600),
                //         FrameFormat::MJPEG,
                //         30
                //     )
                // )
                nokhwa::utils::RequestedFormatType::AbsoluteHighestResolution
            ),
        )
        .unwrap();

        println!("Frame rate: {}", camera.refresh_camera_format().unwrap().frame_rate());

        let mut formats = camera.compatible_camera_formats().unwrap();

        formats.sort_by(|a, b| {
            if a.frame_rate() > b.frame_rate() {
                std::cmp::Ordering::Greater
            } else if a.frame_rate() < b.frame_rate() {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Equal
            }
        });

        for format in formats {
            println!("Available: {:?}", format);
        }

        camera.open_stream().expect("Could not open stream.");

        camera
    };

    let (frame_width, frame_height) = {
        let frame = camera.frame().unwrap();
        let resolution = frame.resolution();

        (resolution.width(), resolution.height())
    };

    // Create Vec<u8> and fill with zeros
    // This will hold the decoded image
    let mut frame_buffer = vec![0u8; (frame_width * frame_height * 4) as usize];

    let mut window_size = window.inner_size();

    window_size.width = frame_width;
    window_size.height = frame_height;

    let _ = window.request_inner_size(window_size);

    let compute = Compute::init().await;

    let output_image_size = wgpu::Extent3d {
        width: window_size.width,
        height: window_size.height,
        depth_or_array_layers: 1
    };

    let vis = VisualizationProgram::new(
        compute.clone(),
        output_image_size, 
        window.clone()
    );

    let orb_program = OrbProgram::init(OrbConfig {
        max_features: 1 << 14,
        max_matches: 1 << 14,
        image_size: output_image_size
    }, compute.clone());

    let mut config = vis.surface
        .get_default_config(&compute.adapter, window_size.width, window_size.height)
        .unwrap();
    vis.surface.configure(&compute.device, &config);

    let window = &window;

    let mut frame_count: u32 = 0;

    event_loop.run(move |event, target| {
        let _ = (&compute, &vis);

        if let Event::WindowEvent {
            window_id: _,
            event,
        } = event
        {
            match event {
                WindowEvent::Resized(new_size) => {
                    config.width = new_size.width.max(1);
                    config.height = new_size.height.max(1);
                    vis.surface.configure(&compute.device, &config);
                    window.request_redraw();
                }
                WindowEvent::RedrawRequested => {
                    let frame = vis.surface.get_current_texture().unwrap();
                    let view = frame
                        .texture
                        .create_view(&wgpu::TextureViewDescriptor::default());
                    let mut encoder = compute.device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
                        label: Some("Draw loop"),
                    });

                    // Decode the image on the CPU and write the decoded buffer to the GPU
                    // TODO: Try to use VulkanVideo to stream directly to GPU
                    // Or gstreamer with Vulkan integration
                    
                    let new_camera_frame = camera.frame().unwrap();
                    new_camera_frame.decode_image_to_buffer::<RgbAFormat>(&mut frame_buffer).unwrap();

                    compute.queue.write_texture(
                        wgpu::ImageCopyTexture {
                            texture: &orb_program.program.textures["input_image"],
                            mip_level: 0,
                            origin: wgpu::Origin3d::ZERO,
                            aspect: wgpu::TextureAspect::All
                        },
                        &frame_buffer,
                        wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: (4 * frame_width).into(),
                            rows_per_image: None
                        },
                        output_image_size
                    );

                    orb_program.run(OrbParams {
                        record_keyframe: frame_count == 100
                    });

                    if frame_count == 100 {
                        println!("Recorded keyframe.");
                    }

                    encoder.copy_texture_to_texture(
                        wgpu::ImageCopyTextureBase { 
                            texture: &orb_program.program.textures["visualization"], 
                            mip_level: 0, 
                            origin: wgpu::Origin3d::ZERO, 
                            aspect: wgpu::TextureAspect::All
                        },
                        wgpu::ImageCopyTextureBase { 
                            texture: &vis.program.textures["texture"], 
                            mip_level: 0, 
                            origin: wgpu::Origin3d::ZERO, 
                            aspect: wgpu::TextureAspect::All
                        },
                        orb_program.config.image_size.clone()
                    );

                    {
                        let mut rpass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                            label: None,
                            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                                view: &view,
                                resolve_target: None,
                                ops: wgpu::Operations {
                                    load: wgpu::LoadOp::Clear(wgpu::Color::GREEN),
                                    store: wgpu::StoreOp::Store,
                                },
                            })],
                            depth_stencil_attachment: None,
                            timestamp_writes: None,
                            occlusion_query_set: None,
                        });
                        rpass.set_pipeline(&vis.program.render_pipelines["draw_texture"]);
                        rpass.set_bind_group(0, &vis.program.bind_groups["draw_texture"], &[]);
                        rpass.draw(0..3, 0..1);
                    }

                    compute.queue.submit(Some(encoder.finish()));

                    frame.present();

                    window.request_redraw();

                    frame_count += 1;
                },
                WindowEvent::CloseRequested => {
                    target.exit();
                },
                _ => {}
            }
        }
    })
}

fn main() -> Result<(), winit::error::EventLoopError> {
    let event_loop = EventLoop::new().unwrap();
    let window = Window::new(&event_loop).unwrap();
    pollster::block_on(run(event_loop, Arc::new(window)))
}
