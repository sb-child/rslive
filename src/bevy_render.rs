use bevy::app::ScheduleRunnerPlugin;
use bevy::asset::RenderAssetUsages;
use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::render::gpu_readback::{Readback, ReadbackComplete};
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::window::ExitCondition;
use bevy::winit::WinitPlugin;
use bytes::Bytes;
use crossfire::{MRx, MTx, mpmc};
use std::thread;
use std::time::Duration;

use crate::Frame;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 60 * 1;

#[derive(Resource, Clone)]
struct FrameSender(MTx<mpmc::Array<Frame>>);

#[derive(Resource)]
struct FrameTimer(std::time::Instant);

#[derive(Resource)]
struct LastFrameTime(std::time::Instant);

pub fn bevy_app(rawstream_tx: MTx<mpmc::Array<Frame>>) {
    // let (tx, rx) = mpmc::bounded_async::<Bytes>(10);

    // let bevy_tx: MTx<mpmc::Array<Bytes>> = tx.into_blocking();
    // let thread_rx: MRx<mpmc::Array<Bytes>> = rx.into_blocking();

    // let encoder_handle = thread::spawn(move || {
    //     let mut total_frames = 0;
    //     let mut fps_counter = 0;
    //     let mut last_time = std::time::Instant::now();

    //     while let Ok(bytes_data) = thread_rx.recv() {
    //         total_frames += 1;
    //         fps_counter += 1;

    //         let elapsed = last_time.elapsed();
    //         if elapsed.as_secs_f32() >= 1.0 {
    //             let actual_fps = fps_counter as f32 / elapsed.as_secs_f32();

    //             tracing::info!(
    //                 "bevy stats: Total Frames: {} | Size: {}x{} ({:.2} MB) | Actual FPS: {:.2}",
    //                 total_frames,
    //                 WIDTH,
    //                 HEIGHT,
    //                 (bytes_data.len() as f64) / 1024.0 / 1024.0,
    //                 actual_fps
    //             );

    //             fps_counter = 0;
    //             last_time = std::time::Instant::now();
    //         }

    //         let frame = Frame {
    //             data: bytes_data,
    //             dur: Duration::from_secs_f64(1.0 / FPS as f64),
    //             ts: chrono::Utc::now(),
    //         };

    //         if let Err(e) = rawstream_tx.try_send(frame) {
    //             match e {
    //                 crossfire::TrySendError::Full(_) => {
    //                     tracing::warn!("bevy: rawstream_tx buffer is full");
    //                 }
    //                 crossfire::TrySendError::Disconnected(_) => {
    //                     tracing::warn!("bevy: rawstream_tx is closed");
    //                     return;
    //                 }
    //             }
    //         }
    //     }
    // });

    let mut app = App::new();

    app.add_plugins(
        DefaultPlugins
            .set(WindowPlugin {
                primary_window: None,
                exit_condition: ExitCondition::DontExit,
                ..default()
            })
            .disable::<LogPlugin>()
            .disable::<WinitPlugin>(),
    )
    // .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
    //     1.0 / FPS as f64,
    //     // 0.0,
    // )))
    .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::ZERO))
    // or Time<Real>?
    .insert_resource(Time::<Fixed>::from_hz(FPS as f64))
    .insert_resource(FrameSender(rawstream_tx))
    .insert_resource(FrameTimer(std::time::Instant::now()))
    .insert_resource(LastFrameTime(std::time::Instant::now()))
    .add_systems(Startup, setup)
    .add_systems(Update, rotate_cube)
    .add_systems(Update, update_time_text)
    .add_systems(Last, frame_limiter);

    app.run();
    // let _ = encoder_handle.join();
}

#[derive(Component)]
struct RotatingCube;

#[derive(Component)]
struct TimeText;

fn setup(
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut images: ResMut<Assets<Image>>,
    mut clear_color: ResMut<ClearColor>,
) {
    *clear_color = ClearColor(Color::srgb(0.0, 0.0, 0.0));

    let mut render_image = Image::new(
        Extent3d {
            width: WIDTH,
            height: HEIGHT,
            depth_or_array_layers: 1,
        },
        TextureDimension::D2,
        vec![0u8; (WIDTH * HEIGHT * 4) as usize],
        TextureFormat::Rgba8UnormSrgb,
        RenderAssetUsages::default(),
    );

    render_image.texture_descriptor.usage =
        TextureUsages::TEXTURE_BINDING | TextureUsages::RENDER_ATTACHMENT | TextureUsages::COPY_SRC;

    let image_handle = images.add(render_image);

    let camera_entity = commands
        .spawn((
            Camera3d::default(),
            RenderTarget::Image(ImageRenderTarget {
                handle: image_handle.clone(),
                scale_factor: 1.0,
            }),
            Transform::from_xyz(3.0, 3.0, 3.0).looking_at(Vec3::ZERO, Vec3::Y),
            Projection::Perspective(PerspectiveProjection {
                fov: std::f32::consts::PI / 4.0,
                aspect_ratio: WIDTH as f32 / HEIGHT as f32,
                near: 0.1,
                far: 1000.0,
                ..default()
            }),
        ))
        .id();

    commands.spawn((
        Text::new(""),
        TextFont {
            font_size: 60.0,
            ..default()
        },
        TextColor(Color::WHITE),
        Node {
            position_type: PositionType::Absolute,
            top: Val::Px(20.0),
            left: Val::Px(20.0),
            ..default()
        },
        UiTargetCamera(camera_entity),
        TimeText,
    ));

    commands
        .spawn(Readback::texture(image_handle.clone()))
        .observe(
            |trigger: On<ReadbackComplete>,
             sender: Res<FrameSender>,
             mut last_time: ResMut<LastFrameTime>,
             mut exit: MessageWriter<AppExit>| {
                let now = std::time::Instant::now();
                let actual_duration = now.duration_since(last_time.0);
                last_time.0 = now;
                let data = Bytes::copy_from_slice(&trigger.data);
                let frame = Frame {
                    data,
                    dur: actual_duration,
                    ts: chrono::Utc::now(),
                };

                if let Err(e) = sender.0.try_send(frame) {
                    match e {
                        crossfire::TrySendError::Full(_) => {
                            tracing::warn!("bevy: gpu_readback buffer is full");
                        }
                        crossfire::TrySendError::Disconnected(_) => {
                            tracing::warn!("bevy: gpu_readback channel is closed");
                            exit.write(AppExit::Success);
                        }
                    }
                }
            },
        );

    let cube_mesh = meshes.add(Cuboid::new(2.0, 2.0, 2.0));
    let cube_material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.2, 0.8, 0.2),
        ..default()
    });

    commands.spawn((
        Mesh3d(cube_mesh),
        MeshMaterial3d(cube_material),
        Transform::default(),
        RotatingCube,
    ));

    commands.spawn((
        DirectionalLight {
            illuminance: 15000.0,
            ..default()
        },
        Transform::from_rotation(Quat::from_euler(
            EulerRot::XYZEx,
            -std::f32::consts::PI / 4.0,
            std::f32::consts::PI / 6.0,
            0.0,
        )),
    ));

    tracing::info!("bevy: setup completed");
}

fn rotate_cube(mut query: Query<&mut Transform, With<RotatingCube>>, time: Res<Time>) {
    for mut transform in query.iter_mut() {
        transform.rotation *= Quat::from_rotation_y(time.delta_secs() * std::f32::consts::PI);
        transform.rotation *= Quat::from_rotation_x(time.delta_secs() * std::f32::consts::PI * 0.5);
    }
}

fn update_time_text(mut query: Query<&mut Text, With<TimeText>>, time: Res<Time>) {
    for mut text in query.iter_mut() {
        text.0 = format!("Time: {:.2} s", time.elapsed_secs());
    }
}

fn frame_limiter(mut timer: ResMut<FrameTimer>) {
    let target_frame_time = Duration::from_secs_f64(1.0 / FPS as f64);
    loop {
        let elapsed = timer.0.elapsed();
        if elapsed >= target_frame_time {
            break;
        }
        let remaining = target_frame_time - elapsed;
        if remaining > Duration::from_millis(2) {
            std::thread::sleep(Duration::from_millis(1));
        } else {
            std::hint::spin_loop();
        }
    }
    timer.0 = std::time::Instant::now();
}
