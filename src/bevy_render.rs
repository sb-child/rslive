use bevy::app::ScheduleRunnerPlugin;
use bevy::asset::RenderAssetUsages;
use bevy::camera::{ImageRenderTarget, RenderTarget};
use bevy::log::LogPlugin;
use bevy::prelude::*;
use bevy::render::render_resource::{Extent3d, TextureDimension, TextureFormat, TextureUsages};
use bevy::window::ExitCondition;
use bevy::winit::WinitPlugin;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

const WIDTH: u32 = 1920;
const HEIGHT: u32 = 1080;
const FPS: u32 = 60 * 1;

#[derive(Resource)]
struct FrameSender(mpsc::Sender<Vec<u8>>);

pub fn bevy_app() {
    let (sender, receiver) = mpsc::channel::<Vec<u8>>();

    let encoder_handle = thread::spawn(move || {
        let mut frame_count = 0;
        while let Ok(rgba_data) = receiver.recv() {
            frame_count += 1;
            println!(
                "Frame {}: Received RGBA8 data: {}x{}x4 = {:.2} MB",
                frame_count,
                WIDTH,
                HEIGHT,
                (rgba_data.len() as f64) / 1024.0 / 1024.0
            );
            // rgba_data
        }
    });

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
    .add_plugins(ScheduleRunnerPlugin::run_loop(Duration::from_secs_f64(
        1.0 / FPS as f64,
    )))
    .insert_resource(Time::<Fixed>::from_hz(FPS as f64))
    .insert_resource(FrameSender(sender))
    .add_systems(Startup, setup)
    .add_systems(Update, rotate_cube)
    .add_systems(Last, capture_frame_system);
    app.run();
    let _ = encoder_handle.join();
}

#[derive(Component)]
struct RotatingCube;

#[derive(Resource)]
struct RenderTextureHandle(Handle<Image>);

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

    commands.spawn((
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
    ));

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

    commands.insert_resource(RenderTextureHandle(image_handle));

    println!("setup done");
}

fn rotate_cube(mut query: Query<&mut Transform, With<RotatingCube>>, time: Res<Time>) {
    for mut transform in query.iter_mut() {
        transform.rotation *= Quat::from_rotation_y(time.delta_secs() * std::f32::consts::PI);
        transform.rotation *= Quat::from_rotation_x(time.delta_secs() * std::f32::consts::PI * 0.5);
    }
}

fn capture_frame_system(
    images: Res<Assets<Image>>,
    render_tex: Res<RenderTextureHandle>,
    sender: Res<FrameSender>,
) {
    if let Some(image) = images.get(&render_tex.0) {
        if let Some(data) = &image.data {
            let _ = sender.0.send(data.clone());
        }
    }
}
