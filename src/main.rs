use anyhow::anyhow;
use std::{
    f32::consts::PI, sync::LazyLock, time::{SystemTime, UNIX_EPOCH}
};
use bevy::{
    prelude::*, post_process::bloom::Bloom, 
    window::{CursorGrabMode, /*PrimaryWindow,*/ CursorOptions},
    input::mouse::{AccumulatedMouseMotion, MouseButton, MouseMotion, MouseWheel}, 
    pbr::{ScreenSpaceAmbientOcclusion, ScreenSpaceAmbientOcclusionQualityLevel, ScreenSpaceReflections}, 
    picking::{backend::ray::RayMap, events::{Pointer, Press}, mesh_picking::MeshPickingPlugin},
    render::{experimental::occlusion_culling::OcclusionCulling}
};//use rayon::prelude::*;
use bevy_slugtext::prelude::*;
use redb::{Database, MultimapTableDefinition, ReadableDatabase, ReadableTable, TableDefinition};

fn init_state(db: &mut Database) -> anyhow::Result<bool> { Ok(db.compact()?) }

macro_rules! database {
    ($i:ident, $n:literal) => {
        static mut $i: LazyLock<Database> = LazyLock::new(|| {
            let mut db = Database::create($n).expect("opening/creating db had issues");
            if init_state(&mut db).expect("state init failed") { println!("memory db compacted"); }
            db
        });
    };
}

macro_rules! multimap {
    ($i:ident, $k:ty, $v:ty) => {
        const $i: MultimapTableDefinition<$k, $v> = MultimapTableDefinition::new(stringify!($i));
    };
}

macro_rules! map {
    ($i:ident, $k:ty, $v:ty) => {
        const $i: TableDefinition<$k, $v> = TableDefinition::new(stringify!($i));
    };
}

database!(STATES, "./states");

multimap!(TEXTS, &str, &[u8]);
multimap!(NON_BILLBOARD_TEXTS, &str, &[u8]);
map!(LOOKING_PLACES, u64, &[u8]);

fn main() -> anyhow::Result<()> {
    if let Ok(rx) = unsafe { #[allow(static_mut_refs)] STATES.begin_read() } {
        let mut texts = vec![];
        let txts = rx.open_multimap_table(TEXTS)?;
        let mut mmv = txts.range::<&str>(..)?;
        while let Some(r) = mmv.next() {
            let (k_ag, _v_ag) = r?;
            texts.push(k_ag.value().to_string());
        }
        std::fs::write("./texts2", texts.join("\n").as_bytes())?;
    } // warm up db in before the game starts
    println!("moo, db is warmed up");
    App::new()
        .insert_resource(GlobalAmbientLight {brightness: 1000., ..default()})
        .init_resource::<InteractionState>()
        .add_plugins((DefaultPlugins, SlugTextPlugin, MeshPickingPlugin))
        .add_systems(Startup, setup)
        .add_systems(Update, (camera_controller, update_interaction_transforms, bouncing_raycast, global_input_handler))
        .add_systems(PostUpdate, (editables, interactables))
        .insert_resource(ClearColor(Color::BLACK))
        .run();

    Ok(())
}

fn unix_now(dur: u64) -> anyhow::Result<u64> {
    Ok(SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs() + dur)
}

#[derive(Default, compactly::v2::Encode, Clone)]
struct LookingPlace{
    #[compactly(Decimal)] x: f32, // translation
    #[compactly(Decimal)] y: f32,
    #[compactly(Decimal)] z: f32,
    #[compactly(Decimal)] a: f32, // rotation
    #[compactly(Decimal)] b: f32,
    #[compactly(Decimal)] c: f32
}

impl LookingPlace {
    fn from_transform(tf: &Transform) -> anyhow::Result<(Self, u64)> {
        let (a, b, c) = tf.rotation.to_euler(EulerRot::XYZ);
        let lp = Self {a, b, c, x: tf.translation.x, y: tf.translation.y, z: tf.translation.z};
        let ts = unix_now(0)?;
        let packed = compactly::encode(&lp);
        let wx = unsafe {  #[allow(static_mut_refs)] STATES.begin_write() }?;
        if !{
            wx.open_table(LOOKING_PLACES)?.insert(ts, packed.as_slice())?.is_some_and(|ag| ag.value().eq(&packed))
        } {
            wx.commit()?;
        }
        Ok((lp, ts))
    }

    #[allow(dead_code)]
    fn from_timestamp(ts: u64) -> anyhow::Result<Self> {
        let rx = unsafe {  #[allow(static_mut_refs)] STATES.begin_read() }?;
        let lps = rx.open_table(LOOKING_PLACES)?;
        if let Some(ag) = lps.get(ts)? {
            let mut raw = ag.value();
            if let Some(lp) = compactly::decode(&mut raw) {
                return Ok(lp);
            }
        }
        Err(anyhow!("didn't find that looking place from that time"))
    }

    fn last() -> anyhow::Result<(Self, u64)> {
        let rx = unsafe {  #[allow(static_mut_refs)] STATES.begin_read() }?;
        let lps = rx.open_table(LOOKING_PLACES)?;
        if let Some((ts_ag, ag)) = lps.last()? {
            let mut raw = ag.value();
            if let Some(lp) = compactly::decode(&mut raw) {
                return Ok((lp, ts_ag.value()));
            }
        }
        Err(anyhow!("didn't find any looking places"))
    }

    fn nth(n: usize) -> anyhow::Result<Self> {
        let rx = unsafe {  #[allow(static_mut_refs)] STATES.begin_read() }?;
        let lps = rx.open_table(LOOKING_PLACES)?;
        if let Some(r) = lps.iter()?.nth(n) {
            let (_, ag) = r?;
            let mut raw = ag.value();
            if let Some(lp) = compactly::decode(&mut raw) {
                return Ok(lp);
            }
        }
        Err(anyhow!("didn't find any looking places"))
    }

    fn to_transform(&self) -> Transform {
        Transform::from_xyz(self.x, self.y, self.z).with_rotation(Quat::from_euler(EulerRot::XYZ, self.a, self.b, self.c))
    }

    // fn remove(&self, ts: u64) -> anyhow::Result<()> { Ok(()) } todo
}

#[derive(Default, compactly::v2::Encode, Clone)]
struct TextInstance{
    #[compactly(Small)] colors: u64, // (Color, Color) text, and then background as an rgba pair of eight bytes as a u64
    #[compactly(Decimal)] size: f32,
    #[compactly(Decimal)] x: f32, #[compactly(Decimal)] y: f32, #[compactly(Decimal)] z: f32,
}

impl TextInstance {
    fn new(c: Color, bg_c: Color, size: f32, tf: &Transform) -> Self {
        TextInstance{
            colors: color_pair_to_u64(c, bg_c), size,
            x: tf.translation.x, y: tf.translation.y, z: tf.translation.z
        }
    }

    fn attach(&self, text: &str, rm_old: Option<&Transform>) -> anyhow::Result<()> {
        let wx = unsafe { #[allow(static_mut_refs)] STATES.begin_write() }?;
        if {
            let mut texts = wx.open_multimap_table(TEXTS)?;
            if let Some(old_tf) = rm_old {
                let ti2 = Self{colors: self.colors, size: self.size, x: old_tf.translation.x, y: old_tf.translation.y, z: old_tf.translation.z};
                texts.remove(text, compactly::encode(&ti2).as_slice())?;
            }
            texts.insert(text, compactly::encode(self).as_slice())?
        } {
            wx.abort()?;
        } else {
            wx.commit()?;
        }
        Ok(())
    }

    fn instantiate() -> anyhow::Result<Vec<(String, Self)>> {
        let mut them = vec![];
        {
            let rx = unsafe {  #[allow(static_mut_refs)] STATES.begin_read() }?;
            let texts = rx.open_multimap_table(TEXTS)?;
            let mut mmr = texts.range::<&str>(..)?;
            while let Some(r) = mmr.next() {
                if let Ok((k_ag, mut mmv)) = r {
                    while let Some(r) = mmv.next() {
                        if let Ok(ag) = r {
                            if let Some(ti) = compactly::decode(ag.value()) {
                                them.push((k_ag.value().to_string(), ti));
                            }
                        }
                    }
                }
            }
        }
        return Ok(them);
    }

    fn remove(&self, text: &str) -> anyhow::Result<()> {
        let wx = unsafe { #[allow(static_mut_refs)] STATES.begin_write() }?;
        {
            let mut texts = wx.open_multimap_table(TEXTS)?;
            texts.remove(text, compactly::encode(self).as_slice())?;
        }
        wx.commit()?;
        Ok(())
    }
}


#[derive(Default, compactly::v2::Encode, Clone)]
struct TextInstanceNonBillboard{
    #[compactly(Small)] colors: u64, // (Color, Color) text, and then background as an rgba pair of eight bytes as a u64
    #[compactly(Decimal)] size: f32,
    #[compactly(Decimal)] x: f32, // translation
    #[compactly(Decimal)] y: f32,
    #[compactly(Decimal)] z: f32,
    #[compactly(Decimal)] a: f32, // rotation
    #[compactly(Decimal)] b: f32,
    #[compactly(Decimal)] c: f32,
}

impl TextInstanceNonBillboard {
    fn new(color: Color, bg_color: Color, size: f32, tf: &Transform) -> Self {
        let (a, b, c) = tf.rotation.to_euler(EulerRot::XYZ);
        Self{
            colors: color_pair_to_u64(color, bg_color), size,
            x: tf.translation.x, y: tf.translation.y, z: tf.translation.z,
            a, b, c
        }
    }

    fn attach(&self, text: &str, rm_old: Option<&Transform>) -> anyhow::Result<()> {
        let wx = unsafe { #[allow(static_mut_refs)] STATES.begin_write() }?;
        {
            let mut texts = wx.open_multimap_table(NON_BILLBOARD_TEXTS)?;
            if let Some(old_tf) = rm_old {
                let (a, b, c) = old_tf.rotation.to_euler(EulerRot::XYZ);
                let ti2 = Self{colors: self.colors, size: self.size,
                    x: old_tf.translation.x, y: old_tf.translation.y, z: old_tf.translation.z,
                    a, b, c
                };
                texts.remove(text, compactly::encode(&ti2).as_slice())?;
            }
            texts.insert(text, compactly::encode(self).as_slice())?;
        }
        wx.commit()?;
        Ok(())
    }

    fn instantiate() -> anyhow::Result<Vec<(String, Self)>> {
        let mut them = vec![];
        {
            let rx = unsafe { #[allow(static_mut_refs)] STATES.begin_read() }?;
            let texts = rx.open_multimap_table(NON_BILLBOARD_TEXTS)?;
            let mut mmr = texts.range::<&str>(..)?;
            while let Some(r) = mmr.next() {
                if let Ok((k_ag, mut mmv)) = r {
                    while let Some(r) = mmv.next() {
                        if let Ok(ag) = r {
                            if let Some(ti) = compactly::decode(ag.value()) {
                                them.push((k_ag.value().to_string(), ti));
                            }
                        }
                    }
                }
            }
        }
        return Ok(them);
    }

    fn remove(&self, text: &str) -> anyhow::Result<()> {
        let wx = unsafe { #[allow(static_mut_refs)] STATES.begin_write() }?;
        {
            let mut texts = wx.open_multimap_table(NON_BILLBOARD_TEXTS)?;
            texts.remove(text, compactly::encode(self).as_slice())?;
        }
        wx.commit()?;
        Ok(())
    }
}

fn u64_to_2_rgba_colors(n: u64) -> (Color, Color) {
    let c = n.to_be_bytes();
    (
        Color::Srgba(Srgba::rgba_u8(c[0], c[1], c[2], c[3])),
        Color::Srgba(Srgba::rgba_u8(c[4], c[5], c[6], c[7]))
    )
}

fn color_pair_to_u64(ca: Color, cb: Color) -> u64 {
    let a = ca.to_srgba().to_u8_array();
    let b = cb.to_srgba().to_u8_array();
    u64::from_be_bytes([a[0], a[1], a[2], a[3], b[0], b[1], b[2], b[3]])
}

const MAX_BOUNCES: usize = 64;
const LASER_SPEED: f32 = 0.03;

fn bouncing_raycast(
    keys: Res<ButtonInput<KeyCode>>,
    mut ray_cast: MeshRayCast,
    mut gizmos: Gizmos,
    time: Res<Time>,
    ray_map: Res<RayMap>,
) {
    let t = ops::cos((time.elapsed_secs() - 4.0).max(0.0) * LASER_SPEED) * PI;
    let ray_pos = Vec3::new(ops::sin(t), ops::cos(3.0 * t) * 0.5, ops::cos(t)) * 0.5;
    let ray_dir = Dir3::new(-ray_pos).unwrap();
    let ray = Ray3d::new(ray_pos, ray_dir);
    gizmos.sphere(ray_pos, 0.1, Color::srgb_u8(233, 100, 100));
    bounce_ray(ray, &mut ray_cast, &mut gizmos, Color::srgb_u8(240, 60, 60));
    if keys.pressed(KeyCode::NumpadMultiply) {
        for (_, ray) in ray_map.iter() {
            bounce_ray(*ray, &mut ray_cast, &mut gizmos, Color::srgb_u8(173, 160, 160));
        }
    }    
}

fn bounce_ray(mut ray: Ray3d, ray_cast: &mut MeshRayCast, gizmos: &mut Gizmos, color: Color) {
    let mut intersections = Vec::with_capacity(MAX_BOUNCES + 1);
    intersections.push((ray.origin, Color::srgb(30.0, 0.0, 0.0)));

    for i in 0..MAX_BOUNCES { // Cast the ray and get the first hit
        let Some((_, hit)) = ray_cast
            .cast_ray(ray, &MeshRayCastSettings::default())
            .first()
        else { break; };

        // Draw the point of intersection and add it to the list
        let brightness = 1.0 + 10.0 * (1.0 - i as f32 / MAX_BOUNCES as f32);
        intersections.push((hit.point, Color::BLACK.mix(&color, brightness)));
        //gizmos.sphere(hit.point, 0.005, Color::BLACK.mix(&color, brightness * 2.0));
        // Reflect the ray off of the surface
        ray.direction = Dir3::new(ray.direction.reflect(hit.normal)).unwrap();
        ray.origin = hit.point + ray.direction * 1e-6;
    }
    gizmos.linestrip_gradient(intersections);
}

multimap!(COLORED_PLANES, [u8; 4], &[u8]);

#[derive(Default, compactly::v2::Encode, Clone)]
struct ColoredPlaneInstance{
    #[compactly(Decimal)] x: f32,
    #[compactly(Decimal)] y: f32,
    #[compactly(Decimal)] z: f32,
    #[compactly(Decimal)] a: f32,
    #[compactly(Decimal)] b: f32,
    #[compactly(Decimal)] c: f32,
    #[compactly(Decimal)] r: f32,
    #[compactly(Decimal)] t: f32,
    #[compactly(Decimal)] s: f32,
}

impl ColoredPlaneInstance {
    fn new(tf: Transform) -> Self {
        Self{
            x: tf.translation.x, y: tf.translation.y, z: tf.translation.z,
            a: tf.rotation.x, b: tf.rotation.y, c: tf.rotation.z,
            r: tf.scale.x, t: tf.scale.y, s: tf.scale.z
        }
    }

    fn attach(&self, color: Color, old: Option<(&Color, &Self)>) -> anyhow::Result<()> {
        let wx = unsafe {  #[allow(static_mut_refs)] STATES.begin_write() }?;
        {
            let mut colored_planes = wx.open_multimap_table(COLORED_PLANES)?;
            colored_planes.insert(color.to_srgba().to_u8_array(), compactly::encode(self).as_slice())?;
            if let Some((c, cpi)) = old {
                colored_planes.remove(c.to_srgba().to_u8_array(), compactly::encode(cpi).as_slice())?;
            }
        }
        wx.commit()?;
        Ok(())
    }

    fn instantiate() -> anyhow::Result<Vec<([u8; 4], Self)>> {
        let mut them = vec![];
        {
            let rx = unsafe { #[allow(static_mut_refs)] STATES.begin_read() }?;
            let colored_planes = rx.open_multimap_table(COLORED_PLANES)?;
            let mut mmr = colored_planes.range::<[u8; 4]>(..)?;
            while let Some(r) = mmr.next() {
                if let Ok((k_ag, mut mmv)) = r {
                    while let Some(r) = mmv.next() {
                        if let Ok(ag) = r {
                            if let Some(ti) = compactly::decode(ag.value()) {
                                them.push((k_ag.value(), ti));
                            }
                        }
                    }
                }
            }
        }
        return Ok(them);
    }
}

multimap!(COLORED_PLANE_BILLBOARDS, [u8; 4], &[u8]);

#[derive(Default, compactly::v2::Encode, Clone)]
struct ColoredPlaneInstanceBillboard{
    #[compactly(Decimal)] x: f32,
    #[compactly(Decimal)] y: f32,
    #[compactly(Decimal)] z: f32,
    #[compactly(Decimal)] r: f32,
    #[compactly(Decimal)] t: f32,
    #[compactly(Decimal)] s: f32,
}

impl ColoredPlaneInstanceBillboard {
    fn new(tf: Transform) -> Self {
        Self{
            x: tf.translation.x, y: tf.translation.y, z: tf.translation.z,
            r: tf.scale.x, t: tf.scale.y, s: tf.scale.z
        }
    }

    fn attach(&self, color: Color, old: Option<(&Color, &Self)>) -> anyhow::Result<()> {
        let wx = unsafe { #[allow(static_mut_refs)] STATES.begin_write() }?;
        {
            let mut colored_planes = wx.open_multimap_table(COLORED_PLANE_BILLBOARDS)?;
            colored_planes.insert(color.to_srgba().to_u8_array(), compactly::encode(self).as_slice())?;
            if let Some((c, cpi)) = old {
                colored_planes.remove(c.to_srgba().to_u8_array(), compactly::encode(cpi).as_slice())?;
            }
        }
        wx.commit()?;
        Ok(())
    }

    fn instantiate() -> anyhow::Result<Vec<([u8; 4], Self)>> {
        let mut them = vec![];
        {
            let rx = unsafe { #[allow(static_mut_refs)] STATES.begin_read() }?;
            let colored_planes = rx.open_multimap_table(COLORED_PLANE_BILLBOARDS)?;
            let mut mmr = colored_planes.range::<[u8; 4]>(..)?;
            while let Some(r) = mmr.next() {
                if let Ok((k_ag, mut mmv)) = r {
                    while let Some(r) = mmv.next() {
                        if let Ok(ag) = r {
                            if let Some(ti) = compactly::decode(ag.value()) {
                                them.push((k_ag.value(), ti));
                            }
                        }
                    }
                }
            }
        }
        return Ok(them);
    }
}

fn swap_db(
    commands: &mut Commands,
    mut meshes: &mut ResMut<Assets<Mesh>>,
    mut materials: &mut ResMut<Assets<StandardMaterial>>,
    path: &str
) -> anyhow::Result<()> {
    unsafe {
        #[allow(static_mut_refs)]
        if let Some (db) = LazyLock::get_mut(&mut STATES) {
            let mut another_world = Database::create(path)?;
            another_world.compact()?;
            *db = another_world;
        }
    }
    let mut ids = vec![];
    for (id, _m) in meshes.iter() { ids.push(id); }
    for id in ids { meshes.remove(id); }
    let mut ids = vec![];
    for (id, _m) in materials.iter() { ids.push(id); }
    for id in ids { materials.remove(id); }
    init_remembered_items(commands, &mut meshes, &mut materials)?;
    Ok(())
}

fn spawn_colored_plane(
    commands: &mut Commands,
    meshes: &mut ResMut<Assets<Mesh>>,
    materials: &mut ResMut<Assets<StandardMaterial>>,
    tf: Transform,
    color: Color,
    billboard: bool,
    save: bool
) -> anyhow::Result<()> {
    let plane_mesh = meshes.add(Plane3d::default());
    let plane_material = materials.add(color);
    if billboard {
        if save { ColoredPlaneInstanceBillboard::new(tf.clone()).attach(color.clone(), None)?; }
        commands.spawn((tf, Mesh3d(plane_mesh), MeshMaterial3d(plane_material), Interactable::new(), Billboard));
    } else {
        if save { ColoredPlaneInstance::new(tf.clone()).attach(color.clone(), None)?; }
        commands.spawn((tf, Mesh3d(plane_mesh), MeshMaterial3d(plane_material), Interactable::new()));
    }
    Ok(())
}

fn setup(
    mut commands: Commands,
    mut state: ResMut<InteractionState>,
    mut meshes: ResMut<Assets<Mesh>>, //asset_server: Res<AssetServer>,
    mut materials: ResMut<Assets<StandardMaterial>>,
) {
    commands.spawn((
        Camera3d::default(), Camera {order: 0, ..default()},
        ScreenSpaceAmbientOcclusion { quality_level: ScreenSpaceAmbientOcclusionQualityLevel::Medium, ..default() },
        Bloom::default(), Msaa::Off,
        Transform::from_xyz(-2.0, 2.0, -2.0).looking_at(Vec3::ZERO, Vec3::Y),
        OcclusionCulling,
        ScreenSpaceReflections::default(),
        MainCamera
    ));

    state.txt_color = Color::WHITE;

    commands.spawn((Text::new(""), Node {
        position_type: PositionType::Absolute,
        bottom: px(12),
        left: px(12),
        ..default()
    }, TypingBufferText));

    let material = materials.add(StandardMaterial {
        base_color: Color::srgb(0.85, 0.5, 0.5),
        perceptual_roughness: 1.0, reflectance: 0.2, ..default()
    });
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::default())), MeshMaterial3d(material.clone()),
        Transform::from_xyz(0.0, 0.0, 10.0), Interactable::new()
    ));
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::default())), MeshMaterial3d(material.clone()),
        Transform::from_xyz(0.0, -10.0, 0.0),
        Interactable::new()
    ));
    commands.spawn((
        Mesh3d(meshes.add(Cuboid::default())), MeshMaterial3d(material),
        Transform::from_xyz(10.0, 0.0, 0.0), Interactable::new()
    ));
    commands.spawn((
        Mesh3d(meshes.add(Sphere::new(0.4).mesh().uv(72, 36))),
        Transform::from_xyz(2.0, -10.0, 40.0),
        MeshMaterial3d(materials.add(StandardMaterial {
            base_color: Color::srgb(0.4, 0.4, 0.4),
            perceptual_roughness: 1.0, reflectance: 0.0, ..default()
        })), SphereMarker
    ));
    commands.spawn((
        DirectionalLight {shadows_enabled: true, ..default()},
        Transform::from_rotation(Quat::from_euler(EulerRot::ZYX, 0.0, PI * -0.15, PI * -0.15)),
    ));

    if let Err(e) = init_remembered_items(&mut commands, &mut meshes, &mut materials) { println!("{e}"); }
}

fn init_remembered_items(
    mut commands: &mut Commands,
    mut meshes: &mut ResMut<Assets<Mesh>>,
    mut materials: &mut ResMut<Assets<StandardMaterial>>,
) -> anyhow::Result<()> {
    for (text, ti) in TextInstance::instantiate()? {
        let (c, bg_c) = u64_to_2_rgba_colors(ti.colors);
        floating_txt(commands, true, text, ti.size, c, Some(bg_c), Transform::from_xyz(ti.x, ti.y, ti.z), false, false);
    }
    for (text, ti) in TextInstanceNonBillboard::instantiate()? {
        let (c, bg_c) = u64_to_2_rgba_colors(ti.colors);
        floating_txt(commands, false, text, ti.size, c, Some(bg_c), Transform::from_xyz(ti.x, ti.y, ti.z).with_rotation(Quat::from_euler(EulerRot::XYZ, ti.a, ti.b, ti.c)), false, false);
    }
    for (rgba, cpi) in ColoredPlaneInstance::instantiate()? {
        spawn_colored_plane(
            &mut commands, &mut meshes, &mut materials, 
            Transform::from_xyz(cpi.x, cpi.y, cpi.z)
            .with_rotation(Quat::from_euler(EulerRot::XYZ, cpi.a, cpi.b, cpi.c))
            .with_scale(Vec3{x: cpi.r, y: cpi.t, z: cpi.s}),
            Color::Srgba(Srgba::from_u8_array(rgba)), false, false
        )?;
    }
    for (rgba, cpi) in ColoredPlaneInstanceBillboard::instantiate()? {
        spawn_colored_plane(&mut commands, &mut meshes, &mut materials,
            Transform::from_xyz(cpi.x, cpi.y, cpi.z).with_scale(Vec3{x: cpi.r, y: cpi.t, z: cpi.s}),
            Color::Srgba(Srgba::from_u8_array(rgba)), true, false
        )?;
    }
    Ok(())
}

fn floating_txt(commands: &mut Commands, billboard: bool, text: String, size: f32, color: Color, bg_color: Option<Color>, tf: Transform, save: bool, editing: bool) {
    if text == "" { return; }
    let idx = text.len() - 1;
    if billboard {
        let mut ec = commands.spawn((
            TextMesh {text: text.clone(), color, size, bg_color: bg_color.unwrap_or_else(|| Color::BLACK.with_alpha(0.0)), ..Default::default()},
            tf.clone(), Pickable::default(), Billboard
        ));
        ec.observe(|ev: On<Pointer<Press>>, mut commands: Commands, q: Query<(&Transform, &TextMesh)>, mut state: ResMut<InteractionState>, keys: Res<ButtonInput<KeyCode>>| {
            if ev.button != PointerButton::Primary && ev.button != PointerButton::Middle { return; }
            state.is_focused = true;
            let entity = ev.entity;
            if let Ok((tf, tm)) = q.get(entity) {
                if keys.pressed(KeyCode::Delete) {
                    if let Err(e) = TextInstance::new(tm.color, tm.bg_color, tm.size, tf).remove(&tm.text) { println!("{e}"); }
                    commands.entity(entity).despawn();
                    state.is_focused = false;
                } else if keys.pressed(KeyCode::KeyC) {
                    state.accumulated_input = tm.text.clone();
                } else {
                    let hit_pos = ev.hit.position.unwrap_or(tf.translation);
                    let offset = tf.translation - hit_pos;
                    match ev.button {
                        PointerButton::Primary => {
                            if keys.pressed(KeyCode::ControlLeft) {
                                let idx = tm.text.len() - 1;
                                commands.entity(entity).insert(Editing{idx});
                            } else {
                                commands.entity(entity).insert(Dragging {distance: ev.hit.depth, offset});
                            }
                        }
                        PointerButton::Middle => {
                            commands.entity(entity).insert(Rotating { /*distance: ev.hit.depth,*/ offset });
                        }
                        _ => {}
                    }
                }
            }
        }).observe(|ev: On<Pointer<DragEnd>>, mut commands: Commands, q: Query<(&Transform, &TextMesh)>, mut state: ResMut<InteractionState>| {
            state.is_focused = false;
            let entity = ev.entity;
            let mut ec = commands.entity(entity);
            ec.remove::<Dragging>();
            ec.remove::<Rotating>();
            if let Ok((tf, tm)) = q.get(entity) {
                if let Err(e) = TextInstance::new(tm.color, tm.bg_color, tm.size, tf).attach(tm.text.as_str(), Some(tf)) { println!("{e}"); }
            }
        });
        if save { if let Err(e) = TextInstance::new(color, bg_color.unwrap_or_else(|| Color::srgba_u8(0, 0, 0, 0)), size, &tf).attach(text.as_str(), Some(&tf)) { println!("{e}"); } }
        if editing { ec.insert(Editing{idx}); }
    } else {
        let mut ec = commands.spawn((
            TextMesh {text: text.clone(), color, size, bg_color: bg_color.unwrap_or_else(|| Color::BLACK.with_alpha(0.0)), ..Default::default()},
            tf, Pickable::default()
        ));
        ec.observe(|ev: On<Pointer<Press>>, mut commands: Commands, q: Query<(&Transform, &TextMesh)>, mut state: ResMut<InteractionState>, keys: Res<ButtonInput<KeyCode>>| {
            if ev.button != PointerButton::Primary && ev.button != PointerButton::Middle { return; }
            state.is_focused = true;
            let entity = ev.entity;
            if let Ok((tf, tm)) = q.get(entity) {
                if keys.pressed(KeyCode::Delete) {
                    if let Err(e) = TextInstanceNonBillboard::new(tm.color, tm.bg_color, tm.size, tf).remove(&tm.text) { println!("{e}"); }
                    commands.entity(entity).despawn();
                } else if keys.pressed(KeyCode::KeyC) {
                    state.accumulated_input = tm.text.clone();
                } else {
                    let hit_pos = ev.hit.position.unwrap_or(tf.translation);
                    let offset = tf.translation - hit_pos;
                    match ev.button {
                        PointerButton::Primary => {
                            if keys.pressed(KeyCode::ControlLeft) {
                                commands.entity(entity).insert(Editing{idx: tm.text.len() - 1});
                            } else {
                                commands.entity(entity).insert(Dragging {distance: ev.hit.depth, offset});
                            }
                        }
                        PointerButton::Middle => {
                            commands.entity(entity).insert(Rotating { /*distance: ev.hit.depth,*/ offset });
                        }
                        _ => {}
                    }
                }
            }
        }).observe(|ev: On<Pointer<DragEnd>>, mut commands: Commands, q: Query<(&Transform, &TextMesh)>, mut state: ResMut<InteractionState>| {
            commands.entity(ev.entity).remove::<Dragging>();
            commands.entity(ev.entity).remove::<Rotating>();
            if let Ok((tf, tm)) = q.get(ev.entity) {
                if let Err(e) = TextInstanceNonBillboard::new(tm.color, tm.bg_color, tm.size, tf).attach(&tm.text, Some(tf)) {
                    println!("{e}");
                }
            }
            state.is_focused = false;
        });
        if save { 
            if let Err(e) = TextInstanceNonBillboard::new(color, bg_color.unwrap_or_else(|| Color::BLACK.with_alpha(0.0)), size, &tf).attach(text.as_str(), None) { 
                println!("{e}");
            } 
        }
        if editing { ec.insert(Editing{idx}); }
    }
}

#[derive(Resource, Default)]
struct InteractionState { is_focused: bool, accumulated_input: String, caps: bool, despawn_all: bool, placed_already: bool, txt_color: Color }
#[derive(Component)] struct Dragging {distance: f32, offset: Vec3}
#[derive(Component)] struct Rotating {/*distance: f32,*/offset: Vec3}
#[derive(Component)] struct Billboard;
#[derive(Component)] struct MainCamera;
#[derive(Component)] struct SphereMarker;
#[derive(Component)] struct TypingBufferText;
#[derive(Component)] #[component(storage = "SparseSet")] struct Interactable{observing: bool, interacting: bool}
#[derive(Component)] #[component(storage = "SparseSet")] struct Editing{idx: usize}

impl Interactable{
    fn new() -> Self { Self{observing: false, interacting: false} }
}

fn editables(
    mut state: ResMut<InteractionState>,
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut edit_query: Query<(Entity, &mut TextMesh, &Transform, &mut Editing)>
) {
    for (e, mut tm, tf, mut editing) in edit_query.iter_mut() {
        if keys.just_pressed(KeyCode::Backspace) {
            tm.text.pop();
        }
        if keys.just_pressed(KeyCode::ArrowLeft) && editing.idx > 0 {
            editing.idx -= 1;
        }
        if keys.just_pressed(KeyCode::ArrowRight) && editing.idx < tm.text.len() {
            editing.idx += 1;
        }
        
        if editing.idx == tm.text.len() - 1 {
            tm.text = format!("{}{}", tm.text, state.accumulated_input.drain(..).collect::<String>());
        } else {
            tm.text.insert_str(editing.idx, state.accumulated_input.drain(..).collect::<String>().as_str());
            editing.idx = tm.text.len() - 1;
        }

        if keys.just_pressed(KeyCode::Tab) {
            commands.entity(e).remove::<Editing>();
            if let Err(e) = TextInstance::new(tm.color, tm.bg_color, tm.size, tf).attach(tm.text.as_str(), None) { println!("{e}"); }
            state.is_focused = false;
        }
    }
}

fn interactables(
    mut state: ResMut<InteractionState>,
    mut commands: Commands,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<StandardMaterial>>,
    mut set: ParamSet<(Query<(Entity, &mut Interactable)>, Query<Entity, With<TextMesh>>)>
) {
    for (e, mut i) in set.p0().iter_mut() {
        if !i.observing {
            i.observing = true;
            let mut ec = commands.entity(e);
            ec.observe(|ev: On<Pointer<Press>>, mut commands: Commands, mut q: Query<(&Transform, &mut Interactable)>, mut state: ResMut<InteractionState>| {
                if ev.button != PointerButton::Primary && ev.button != PointerButton::Middle { return; }
                state.is_focused = true;
                let entity = ev.entity;
                if let Ok((tf, mut interactable)) = q.get_mut(entity) {
                    let hit_pos = ev.hit.position.unwrap_or(tf.translation);
                    let offset = tf.translation - hit_pos;
                    interactable.interacting = true;
                    match ev.button {
                        PointerButton::Primary => {
                            commands.entity(entity).insert(Dragging { distance: ev.hit.depth, offset});
                        }
                        PointerButton::Middle => {
                            commands.entity(entity).insert(Rotating { /*distance: ev.hit.depth,*/ offset });
                        }
                        _ => { interactable.interacting = false; }
                    }
                }
            }).observe(|ev: On<Pointer<DragEnd>>, mut commands: Commands, mut q: Query<(&Transform, &mut Interactable)>, mut state: ResMut<InteractionState>| {
                let mut ec = commands.entity(ev.entity);
                ec.remove::<Dragging>();
                ec.remove::<Rotating>();
                if let Ok((_tf, mut interactable)) = q.get_mut(ev.entity) { interactable.interacting = false; }
                state.is_focused = false;
            });
        }
    }
    if state.despawn_all {
        for e in set.p1() {
            match commands.get_entity(e) {
                Ok(mut ec) => { ec.despawn(); }
                Err(e) => { println!("huh {e}"); }
            }
        }
    }
    if state.despawn_all {
        if let Err(e) = swap_db(
            &mut commands, &mut meshes, &mut materials,
            state.accumulated_input.drain(..).collect::<String>().as_str()
        ) { println!("{e}"); }
        state.despawn_all = false;
    }
}

fn camera_controller(
    mut commands: Commands,
    mut state: ResMut<InteractionState>,
    time: Res<Time>,
    keys: Res<ButtonInput<KeyCode>>,
    mouse_buttons: Res<ButtonInput<MouseButton>>,
    accumulated_mouse_motion: Res<AccumulatedMouseMotion>, // mut window_query: Query<&mut Window, With<PrimaryWindow>>,
    mut cursor_options: Single<&mut CursorOptions>,
    mut buff_txt: Single<(&mut Text, Entity), With<TypingBufferText>>,
    mut set: ParamSet<(
        Query<&mut Transform, With<MainCamera>>,
        Query<(&mut Transform, &mut TextMesh), With<Billboard>>
    )>,
) { // let Ok(mut window) = window_query.single_mut() else { return };
    let tds = time.delta_secs();
    let campos = {
        let mut cam = set.p0();
        let Ok(mut transform) = cam.single_mut() else { return };
        if mouse_buttons.just_pressed(MouseButton::Right) {
            cursor_options.grab_mode = CursorGrabMode::Locked;
            cursor_options.visible = false;
        }
        if keys.just_pressed(KeyCode::Escape) || mouse_buttons.just_released(MouseButton::Right) {
            cursor_options.grab_mode = CursorGrabMode::None;
            cursor_options.visible = true;
            state.is_focused = false;
        }
        if cursor_options.grab_mode != CursorGrabMode::Locked && !mouse_buttons.any_pressed([MouseButton::Left, MouseButton::Middle, MouseButton::Right]) {
            for k in keys.get_just_pressed() {
                match k {
                    KeyCode::CapsLock => { state.caps = !state.caps; }
                    KeyCode::KeyA => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "A" } else { "a" }); }
                    KeyCode::KeyB => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "B" } else { "b" }); }
                    KeyCode::KeyC => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "C" } else { "c" }); }
                    KeyCode::KeyD => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "D" } else { "d" }); }
                    KeyCode::KeyE => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "E" } else { "e" }); }
                    KeyCode::KeyF => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "F" } else { "f" }); }
                    KeyCode::KeyG => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "G" } else { "g" }); }
                    KeyCode::KeyH => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "H" } else { "h" }); }
                    KeyCode::KeyI => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "I" } else { "i" }); }
                    KeyCode::KeyJ => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "J" } else { "j" }); }
                    KeyCode::KeyK => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "K" } else { "k" }); }
                    KeyCode::KeyL => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "L" } else { "l" }); }
                    KeyCode::KeyM => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "M" } else { "m" }); }
                    KeyCode::KeyN => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "N" } else { "n" }); }
                    KeyCode::KeyO => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "O" } else { "o" }); }
                    KeyCode::KeyP => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "P" } else { "p" }); }
                    KeyCode::KeyQ => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "Q" } else { "q" }); }
                    KeyCode::KeyR => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "R" } else { "r" }); }
                    KeyCode::KeyS => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "S" } else { "s" }); }
                    KeyCode::KeyT => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "T" } else { "t" }); }
                    KeyCode::KeyU => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "U" } else { "u" }); }
                    KeyCode::KeyV => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "V" } else { "v" });}
                    KeyCode::KeyW => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "W" } else { "w" }); }
                    KeyCode::KeyX => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "X" } else { "x" }); }
                    KeyCode::KeyY => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "Y" } else { "y" }); }
                    KeyCode::KeyZ => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "Z" } else { "z" }); }
                    KeyCode::Space => { state.accumulated_input = format!("{}{}", state.accumulated_input, " "); }
                    KeyCode::Period => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { ">" } else { "." }); }
                    KeyCode::Comma => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "<" } else { "," }); }
                    KeyCode::Quote => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "\"" } else { "'" }); }
                    KeyCode::Digit1 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "!" } else { "1" }); }
                    KeyCode::Digit2 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "@" } else { "2" }); }
                    KeyCode::Digit3 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "#" } else { "3" }); }
                    KeyCode::Digit4 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "$" } else { "4" }); }
                    KeyCode::Digit5 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "^" } else { "5" }); }
                    KeyCode::Digit6 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "&" } else { "6" }); }
                    KeyCode::Digit7 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "&" } else { "7" }); }
                    KeyCode::Digit8 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "*" } else { "8" }); }
                    KeyCode::Digit9 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "(" } else { "9" }); }
                    KeyCode::Digit0 => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { ")" } else { "0" }); }
                    KeyCode::Semicolon => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { ":" } else { ";" }); }
                    KeyCode::BracketLeft => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "[" } else { "{" }); }
                    KeyCode::BracketRight => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "]" } else { "}" }); }
                    KeyCode::Slash => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "?" } else { "/" }); }
                    KeyCode::Backslash => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "|" } else { "\\" }); }
                    KeyCode::Enter => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "`" } else { "~" }); }
                    KeyCode::Minus => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "_" } else { "-" }); }
                    KeyCode::Equal => { state.accumulated_input = format!("{}{}", state.accumulated_input, if state.caps | keys.pressed(KeyCode::ShiftLeft) { "+" } else { "=" }); }
                    KeyCode::Escape if keys.pressed(KeyCode::ShiftLeft) => {
                        transform.translation.x = 0.0;
                        transform.translation.y = 0.0;
                        transform.translation.z = 0.0;
                        state.despawn_all = true;
                    }
                    KeyCode::Backquote => {
                        let spawn_distance = 5.5;
                        if &state.accumulated_input[0..4] == "rgb:" {
                            let rgb: Vec<u8> = state.accumulated_input.trim_start_matches("rgb:").trim().replace("  ", " ").replace("  ", " ").split(" ").filter_map(|c| match u8::from_str_radix(c, 10) { Ok(c) => Some(c), Err(_) => None}).collect();
                            if rgb.len() >= 3 {
                                state.txt_color = Color::srgb_u8(rgb[0], rgb[1], rgb[2]);
                                commands.entity(buff_txt.1).remove::<TextColor>();
                                commands.entity(buff_txt.1).insert(TextColor(state.txt_color));
                                state.accumulated_input = "rgb:".to_string();
                            } else {
                                let (a,b,c) = transform.rotation.to_euler(EulerRot::XYZ);
                                floating_txt(&mut commands, false, "basically shite, can't interpret that, oof sawi".to_string(), 1.0, Color::srgb_u8(250, 10, 10), None, Transform::from_translation(transform.translation + (transform.forward() * spawn_distance)).with_rotation(Quat::from_euler(EulerRot::XYZ, a, b, c)), false, false);    
                            }
                        } else {
                            if keys.pressed(KeyCode::ShiftLeft) {
                                let (a,b,c) = transform.rotation.to_euler(EulerRot::XYZ);
                                floating_txt(&mut commands, false, state.accumulated_input.drain(..).collect::<String>(), 1.0, state.txt_color, None, Transform::from_translation(transform.translation + (transform.forward() * spawn_distance)).with_rotation(Quat::from_euler(EulerRot::XYZ, a, b, c)), true, false);
                            } else {
                                let mut tf = Transform::from_translation(transform.translation + (transform.forward() * spawn_distance) - (2.0 * transform.left()));
                                tf.look_at(transform.translation, transform.up());
                                tf.rotate_local_y(std::f32::consts::PI);
                                floating_txt(&mut commands, true, state.accumulated_input.drain(..).collect::<String>(), 0.5, state.txt_color, None, tf, true, false);
                            }
                            state.is_focused = false;
                            state.placed_already = true;
                        }
                    }
                    KeyCode::Backspace => {
                        state.accumulated_input.pop();
                    }
                    KeyCode::Delete => {
                        state.accumulated_input = "".to_string();
                    },
                    _ => {}
                }
                buff_txt.0.0 = state.accumulated_input.clone();
            }
            return;
        }

        if state.is_focused {  return; }
    
        let delta = accumulated_mouse_motion.delta;
        if delta != Vec2::ZERO {
            let sensitivity = 0.003;
            let yaw = Quat::from_rotation_y(-delta.x * sensitivity);
            let pitch = Quat::from_rotation_x(-delta.y * sensitivity);
            transform.rotation = transform.rotation * yaw * pitch;
            transform.rotation = transform.rotation.normalize();
        }

        let mut roll_speed: f32 = 0.0;

        if keys.pressed(KeyCode::KeyQ) { roll_speed += 1.5; }
        if keys.pressed(KeyCode::KeyE) { roll_speed -= 1.5; }

        let delta_roll = roll_speed * tds;
        if delta_roll != 0.0 { transform.rotate_local_z(delta_roll); }
            
        let mut velocity = Vec3::ZERO;
        let forward = transform.forward();
        let right = transform.right();
        let up: Dir3 = transform.up();
    
        let mut speed: f32 = 33.333;
        if mouse_buttons.pressed(MouseButton::Left) && mouse_buttons.pressed(MouseButton::Right) {
            velocity += *forward;
            speed += 27.0;
            if mouse_buttons.pressed(MouseButton::Middle) {
                speed += 24.20;
            }
            if keys.pressed(KeyCode::KeyW) {
                speed += 40.484;
            }
        } else if keys.pressed(KeyCode::KeyW) || mouse_buttons.pressed(MouseButton::Middle) { 
            velocity += *forward; 
        }
        if keys.pressed(KeyCode::KeyS) { velocity -= *forward; }
        if keys.pressed(KeyCode::KeyA) { velocity -= *right; }
        if keys.pressed(KeyCode::KeyD) { velocity += *right; }
        if keys.pressed(KeyCode::Space) { velocity += *up; }
        if keys.pressed(KeyCode::ControlLeft) { velocity -= *up; }

        if keys.pressed(KeyCode::ShiftLeft) { speed += 25.0; }

        transform.translation += velocity.normalize_or_zero() * speed * tds;

        if keys.pressed(KeyCode::ShiftLeft) && keys.just_pressed(KeyCode::KeyT) {
            if state.placed_already { return; }
            match LookingPlace::from_transform(&transform) {
                Ok(_lp) => {},
                Err(e) => { println!("{e}"); }
            };
            state.placed_already = true;
        } else if keys.pressed(KeyCode::ShiftLeft) && keys.just_pressed(KeyCode::KeyR) {
            match LookingPlace::last() {
                Ok((lp, ts)) => {
                    let tf = lp.to_transform();
                    transform.translation = tf.translation;
                    transform.rotation = tf.rotation;
                    floating_txt(&mut commands, true, ts.to_string(), 1.0, Color::Srgba(Srgba::rgba_u8(255, 0, 30, 255)), None, transform.clone(), false, false);
                    state.placed_already = false;
                },
                Err(e) => { println!("{e}"); }
            };
        }

        if keys.pressed(KeyCode::Backspace) {
            if let Ok(n) = usize::from_str_radix(state.accumulated_input.drain(..).collect::<String>().as_str(), 10) {
                match LookingPlace::nth(n) {
                    Ok(lp) => {
                        let tf = lp.to_transform();
                        transform.translation = tf.translation;
                        transform.rotation = tf.rotation;
                    },
                    Err(e) => { println!("{e}"); }
                };
            }
        }
        if state.placed_already { state.placed_already = false; }

        transform.clone()
    };

    for (mut tt, mut tm) in set.p1().iter_mut() {
        let distance = tt.translation.distance(campos.translation);
        if distance < 420.0 {
            tt.look_at(campos.translation, campos.up());
            tt.rotate_local_y(std::f32::consts::PI);
            if distance < 40.0 {
                tm.size = distance * 0.026;
            } else if tm.size != 1.0 {
                if tm.size < 1.0 { 
                    tm.size += 0.1;
                } else if tm.size > 1.0 {
                    tm.size -= 0.1; 
                }
                tm.size = tm.size.clamp(0.0, 1.0);
            }
        }
    }
}

fn update_interaction_transforms(
    camera_q: Query<(&Camera, &GlobalTransform), With<MainCamera>>,
    window_q: Query<&Window>,
    mut motion_ev: MessageReader<MouseMotion>,
    mut scroll_ev: MessageReader<MouseWheel>,
    mut dragged_q: Query<(&mut Transform, &mut Dragging)>,
    mut rotating_q: Query<(&mut Transform, &Rotating), Without<Dragging>>,
) {
    let Ok((camera, camera_tf)) = camera_q.single() else { return };
    let window = window_q.single(); 
    let scroll_delta: f32 = scroll_ev.read().map(|e| e.y).sum();
    let mouse_delta: Vec2 = motion_ev.read().map(|e| e.delta).sum();
    if let Ok(w) = window {
        if let Some(cursor) = w.cursor_position() {
            if let Ok(ray) = camera.viewport_to_world(camera_tf, cursor) {
                for (mut tf, mut drag) in dragged_q.iter_mut() {
                    drag.distance = (drag.distance + scroll_delta).max(1.0);
                    let ray_point = ray.origin + *ray.direction * drag.distance;
                    tf.translation = ray_point + drag.offset;
                }
            }
        }
    }
    if mouse_delta != Vec2::ZERO {
        let sensitivity = 0.01;
        for (mut tf, rot_data) in rotating_q.iter_mut() {
            let local_up = tf.rotation.inverse() * *camera_tf.up();
            let local_right = tf.rotation.inverse() * *camera_tf.right();
            let local_rot_inc = Quat::from_axis_angle(local_up, -mouse_delta.x * sensitivity) * Quat::from_axis_angle(local_right, -mouse_delta.y * sensitivity);
            let world_pivot = tf.translation + (tf.rotation * rot_data.offset);
            tf.rotation = tf.rotation * local_rot_inc;
            tf.translation = world_pivot - (tf.rotation * rot_data.offset);
        }
    }
}

fn global_input_handler(
    keys: Res<ButtonInput<KeyCode>>,
    mut commands: Commands,
    mut state: ResMut<InteractionState>,
    mut cc: ResMut<ClearColor>,
    active_q: Query<Entity, Or<(With<Dragging>, With<Rotating>)>>,
) {
    if keys.just_pressed(KeyCode::Escape) {
        for entity in active_q.iter() {
            commands.entity(entity).remove::<Dragging>();
            commands.entity(entity).remove::<Rotating>();
        }
        state.is_focused = false;
    }
    if keys.pressed(KeyCode::PageUp) {
        cc.0 = cc.0.darker(0.001);
    } else if keys.pressed(KeyCode::PageDown) {
        cc.0 = cc.0.lighter(0.001);
    }
}

/*map!(GLOBAL_MEMORY, &[u8], &[u8]);
fn recall(input: &[u8]) -> anyhow::Result<Option<Vec<u8>>> {
    Ok(STATES.begin_read()?.open_table(GLOBAL_MEMORY)?.get(input)?.map(|ag| ag.value().to_vec()))
}

fn recall_chain(input: &[u8]) -> anyhow::Result<Vec<u8>> {
    {
        let rx = STATES.begin_read()?;
        let gm = rx.open_table(GLOBAL_MEMORY)?;
        if let Some(mut ag) = gm.get(input)? {
            while let Some(next) = gm.get(ag.value())? {
                ag = next;
            }
            return Ok(ag.value().to_vec());
        }
    }
    Err(anyhow!("ain't got shit for that"))
}

fn remember(input: &[u8], output: &[u8], overwrite: bool) -> anyhow::Result<Option<Vec<u8>>> {
    let wx = STATES.begin_write()?;
    let mut cancel = false;
    let got = {
        if let Some(ag) = wx.open_table(GLOBAL_MEMORY)?.insert(input, output)? {
            if !overwrite {
                cancel = true;
            }
            Some(ag.value().to_vec())
        } else {
            None
        }
    };
    if cancel {
        wx.abort()?;
    } else {
        wx.commit()?;
    }
    Ok(got) //Err(anyhow!("ain't remembering that shit"))
}
*/
