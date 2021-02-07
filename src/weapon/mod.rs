//! Weapon related stuff.

use crate::{
    actor::Actor, actor::ActorContainer, message::Message, weapon::projectile::ProjectileKind,
    GameTime,
};
use rg3d::{
    core::{
        algebra::{Matrix3, UnitQuaternion, Vector3},
        arrayvec::ArrayVec,
        color::Color,
        math::{ray::Ray, Matrix4Ext, Vector3Ext},
        pool::{Handle, Pool, PoolIteratorMut},
        visitor::{Visit, VisitResult, Visitor},
    },
    engine::resource_manager::ResourceManager,
    physics::{geometry::InteractionGroups, parry::shape::FeatureId},
    rand::seq::SliceRandom,
    renderer::surface::{SurfaceBuilder, SurfaceSharedData},
    scene::{
        base::BaseBuilder,
        graph::Graph,
        light::{BaseLightBuilder, PointLightBuilder, SpotLightBuilder},
        mesh::{MeshBuilder, RenderPath},
        node::Node,
        physics::{Physics, RayCastOptions},
        sprite::SpriteBuilder,
        ColliderHandle, Scene,
    },
    utils::{
        self,
        log::{Log, MessageKind},
    },
};
use std::{
    hash::{Hash, Hasher},
    ops::{Index, IndexMut},
    path::{Path, PathBuf},
    sync::{mpsc::Sender, Arc, RwLock},
};

pub mod projectile;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
#[repr(u32)]
pub enum WeaponKind {
    M4 = 0,
    Ak47 = 1,
    PlasmaRifle = 2,
}

impl Default for WeaponKind {
    fn default() -> Self {
        Self::M4
    }
}

impl WeaponKind {
    pub fn id(self) -> u32 {
        self as u32
    }

    pub fn new(id: u32) -> Result<Self, String> {
        match id {
            0 => Ok(WeaponKind::M4),
            1 => Ok(WeaponKind::Ak47),
            2 => Ok(WeaponKind::PlasmaRifle),
            _ => Err(format!("unknown weapon kind {}", id)),
        }
    }
}

impl Visit for WeaponKind {
    fn visit(&mut self, name: &str, visitor: &mut Visitor) -> VisitResult {
        let mut id = self.id();
        id.visit(name, visitor)?;
        if visitor.is_reading() {
            *self = Self::new(id)?;
        }
        VisitResult::Ok(())
    }
}

#[derive(Default)]
pub struct LaserSight {
    ray: Handle<Node>,
    tip: Handle<Node>,
}

impl LaserSight {
    pub fn new(scene: &mut Scene, resource_manager: ResourceManager) -> Self {
        let color = Color::from_rgba(0, 162, 232, 200);

        let ray = MeshBuilder::new(BaseBuilder::new().with_visibility(false))
            .with_surfaces(vec![SurfaceBuilder::new(Arc::new(RwLock::new(
                SurfaceSharedData::make_cylinder(
                    6,
                    1.0,
                    1.0,
                    false,
                    UnitQuaternion::from_axis_angle(&Vector3::x_axis(), 90.0f32.to_radians())
                        .to_homogeneous(),
                ),
            )))
            .with_color(color)
            .build()])
            .with_cast_shadows(false)
            .with_render_path(RenderPath::Forward)
            .build(&mut scene.graph);

        let tip = SpriteBuilder::new(BaseBuilder::new().with_visibility(false))
            .with_texture(resource_manager.request_texture("data/particles/star_09.png"))
            .with_color(color)
            .with_size(0.03)
            .build(&mut scene.graph);

        Self { ray, tip }
    }

    pub fn update(
        &self,
        scene: &mut Scene,
        position: Vector3<f32>,
        direction: Vector3<f32>,
        ignore_collider: ColliderHandle,
    ) {
        let mut intersections = ArrayVec::<[_; 64]>::new();

        let ray = &mut scene.graph[self.ray];
        let max_toi = 100.0;

        scene.physics.cast_ray(
            RayCastOptions {
                ray: Ray::new(position, direction.scale(max_toi)),
                max_len: max_toi,
                groups: Default::default(),
                sort_results: true,
            },
            &mut intersections,
        );

        if let Some(result) = intersections
            .into_iter()
            .filter(|i| i.collider != ignore_collider)
            .next()
        {
            ray.local_transform_mut()
                .set_position(position)
                .set_rotation(UnitQuaternion::face_towards(&direction, &Vector3::y()))
                .set_scale(Vector3::new(0.0012, 0.0012, result.toi));

            scene.graph[self.tip]
                .local_transform_mut()
                .set_position(result.position.coords - direction.scale(0.1));
        }
    }
}

impl Visit for LaserSight {
    fn visit(&mut self, name: &str, visitor: &mut Visitor) -> VisitResult {
        visitor.enter_region(name)?;

        self.ray.visit("Ray", visitor)?;
        self.tip.visit("Tip", visitor)?;

        visitor.leave_region()
    }
}

pub struct Weapon {
    kind: WeaponKind,
    model: Handle<Node>,
    shot_point: Handle<Node>,
    muzzle_flash: Handle<Node>,
    shot_light: Handle<Node>,
    offset: Vector3<f32>,
    dest_offset: Vector3<f32>,
    last_shot_time: f64,
    shot_position: Vector3<f32>,
    owner: Handle<Actor>,
    ammo: u32,
    muzzle_flash_timer: f32,
    pub definition: &'static WeaponDefinition,
    pub sender: Option<Sender<Message>>,
    flash_light: Handle<Node>,
    laser_sight: LaserSight,
}

#[derive(Copy, Clone, PartialEq)]
pub struct Hit {
    pub actor: Handle<Actor>, // Can be None if level geometry was hit.
    pub who: Handle<Actor>,
    pub position: Vector3<f32>,
    pub normal: Vector3<f32>,
    pub collider: ColliderHandle,
    pub feature: FeatureId,
}

impl Hash for Hit {
    fn hash<H: Hasher>(&self, state: &mut H) {
        utils::hash_as_bytes(self, state);
    }
}

impl Eq for Hit {}

/// Checks intersection of given ray with actors and environment.
pub fn ray_hit(
    begin: Vector3<f32>,
    end: Vector3<f32>,
    weapon: Handle<Weapon>,
    weapons: &WeaponContainer,
    actors: &ActorContainer,
    physics: &mut Physics,
    ignored_collider: ColliderHandle,
) -> Option<Hit> {
    let ray = Ray::from_two_points(begin, end);

    // TODO: Avoid allocation.
    let mut query_buffer = Vec::default();

    physics.cast_ray(
        RayCastOptions {
            ray,
            max_len: ray.dir.norm(),
            groups: InteractionGroups::all(),
            sort_results: true,
        },
        &mut query_buffer,
    );

    // List of hits sorted by distance from ray origin.
    for hit in query_buffer
        .iter()
        .filter(|i| i.collider != ignored_collider)
    {
        let collider = physics.colliders.get(hit.collider.into()).unwrap();
        let body = collider.parent();

        // Check if there was an intersection with an actor.
        for (actor_handle, actor) in actors.pair_iter() {
            if actor.get_body() == body.into() && weapon.is_some() {
                let weapon = &weapons[weapon];
                // Ignore intersections with owners of weapon.
                if weapon.owner() != actor_handle {
                    return Some(Hit {
                        actor: actor_handle,
                        who: weapon.owner(),
                        position: hit.position.coords,
                        normal: hit.normal,
                        collider: hit.collider,
                        feature: hit.feature,
                    });
                }
            }
        }

        return Some(Hit {
            actor: Handle::NONE,
            who: Handle::NONE,
            position: hit.position.coords,
            normal: hit.normal,
            collider: hit.collider,
            feature: hit.feature,
        });
    }

    None
}

#[derive(Copy, Clone, Debug)]
pub enum WeaponProjectile {
    Projectile(ProjectileKind),
    /// For high-speed "projectiles".
    Ray {
        damage: f32,
    },
}

pub struct WeaponDefinition {
    pub model: &'static str,
    pub shot_sound: &'static str,
    pub ammo: u32,
    pub projectile: WeaponProjectile,
    pub shoot_interval: f64,
}

impl Default for Weapon {
    fn default() -> Self {
        Self {
            kind: WeaponKind::M4,
            model: Handle::NONE,
            offset: Vector3::default(),
            shot_point: Handle::NONE,
            dest_offset: Vector3::default(),
            last_shot_time: 0.0,
            shot_position: Vector3::default(),
            owner: Handle::NONE,
            ammo: 250,
            muzzle_flash_timer: 0.0,
            definition: Self::get_definition(WeaponKind::M4),
            sender: None,
            muzzle_flash: Default::default(),
            shot_light: Default::default(),
            flash_light: Default::default(),
            laser_sight: Default::default(),
        }
    }
}

impl Visit for Weapon {
    fn visit(&mut self, name: &str, visitor: &mut Visitor) -> VisitResult {
        visitor.enter_region(name)?;

        self.kind.visit("KindId", visitor)?;
        self.definition = Self::get_definition(self.kind);
        self.model.visit("Model", visitor)?;
        self.offset.visit("Offset", visitor)?;
        self.dest_offset.visit("DestOffset", visitor)?;
        self.last_shot_time.visit("LastShotTime", visitor)?;
        self.owner.visit("Owner", visitor)?;
        self.ammo.visit("Ammo", visitor)?;
        self.shot_point.visit("ShotPoint", visitor)?;
        self.muzzle_flash.visit("MuzzleFlash", visitor)?;
        self.muzzle_flash_timer.visit("MuzzleFlashTimer", visitor)?;
        self.shot_light.visit("ShotLight", visitor)?;
        self.flash_light.visit("FlashLight", visitor)?;
        self.laser_sight.visit("LaserSight", visitor)?;

        visitor.leave_region()
    }
}

impl Weapon {
    pub fn get_definition(kind: WeaponKind) -> &'static WeaponDefinition {
        match kind {
            WeaponKind::M4 => {
                static DEFINITION: WeaponDefinition = WeaponDefinition {
                    model: "data/models/m4.FBX",
                    shot_sound: "data/sounds/m4_shot.ogg",
                    ammo: 200,
                    projectile: WeaponProjectile::Ray { damage: 15.0 },
                    shoot_interval: 0.15,
                };
                &DEFINITION
            }
            WeaponKind::Ak47 => {
                static DEFINITION: WeaponDefinition = WeaponDefinition {
                    model: "data/models/ak47.FBX",
                    shot_sound: "data/sounds/ak47.ogg",
                    ammo: 200,
                    projectile: WeaponProjectile::Ray { damage: 17.0 },
                    shoot_interval: 0.15,
                };
                &DEFINITION
            }
            WeaponKind::PlasmaRifle => {
                static DEFINITION: WeaponDefinition = WeaponDefinition {
                    model: "data/models/plasma_rifle.fbx",
                    shot_sound: "data/sounds/plasma_shot.ogg",
                    ammo: 100,
                    projectile: WeaponProjectile::Projectile(ProjectileKind::Plasma),
                    shoot_interval: 0.25,
                };
                &DEFINITION
            }
        }
    }

    pub async fn new(
        kind: WeaponKind,
        resource_manager: ResourceManager,
        scene: &mut Scene,
        sender: Sender<Message>,
    ) -> Weapon {
        let definition = Self::get_definition(kind);

        let model = resource_manager
            .request_model(Path::new(definition.model))
            .await
            .unwrap()
            .instantiate_geometry(scene);

        let shot_point = scene.graph.find_by_name(model, "Weapon:ShotPoint");

        if shot_point.is_none() {
            Log::writeln(
                MessageKind::Warning,
                format!("Shot point not found for {:?} weapon!", kind),
            );
        }

        let muzzle_flash = scene.graph.find_by_name(model, "MuzzleFlash");

        let shot_light = if muzzle_flash.is_none() {
            Log::writeln(
                MessageKind::Warning,
                format!("Muzzle flash not found for {:?} weapon!", kind),
            );
            Default::default()
        } else {
            let light = PointLightBuilder::new(
                BaseLightBuilder::new(BaseBuilder::new().with_visibility(false))
                    .with_scatter_enabled(false)
                    .with_color(Color::opaque(255, 255, 255)),
            )
            .with_radius(2.0)
            .build(&mut scene.graph);

            scene.graph.link_nodes(light, muzzle_flash);

            // Explicitly define render path to be able to render transparent muzzle flash.
            scene.graph[muzzle_flash]
                .as_mesh_mut()
                .set_render_path(RenderPath::Forward);

            light
        };

        let flash_light_point = scene.graph.find_by_name(model, "FlashLightPoint");

        let flash_light = if flash_light_point.is_some() {
            let flash_light = SpotLightBuilder::new(BaseLightBuilder::new(BaseBuilder::new()))
                .with_distance(10.0)
                .with_cookie_texture(
                    resource_manager.request_texture("data/particles/light_01.png"),
                )
                .with_hotspot_cone_angle(30.0f32.to_radians())
                .build(&mut scene.graph);

            scene.graph.link_nodes(flash_light, flash_light_point);

            flash_light
        } else {
            Handle::NONE
        };

        Weapon {
            kind,
            model,
            shot_point,
            definition,
            muzzle_flash,
            shot_light,
            ammo: definition.ammo,
            sender: Some(sender),
            flash_light,
            laser_sight: LaserSight::new(scene, resource_manager),
            ..Default::default()
        }
    }

    pub fn set_visibility(&self, visibility: bool, graph: &mut Graph) {
        graph[self.model].set_visibility(visibility);
        graph[self.laser_sight.tip].set_visibility(visibility);
        graph[self.laser_sight.ray].set_visibility(visibility);
    }

    pub fn get_model(&self) -> Handle<Node> {
        self.model
    }

    pub fn update(&mut self, scene: &mut Scene, actors: &ActorContainer, dt: f32) {
        self.offset.follow(&self.dest_offset, 0.2);

        let node = &mut scene.graph[self.model];
        node.local_transform_mut().set_position(self.offset);
        self.shot_position = node.global_position();

        self.muzzle_flash_timer -= dt;
        if self.muzzle_flash_timer <= 0.0 && self.muzzle_flash.is_some() {
            scene.graph[self.muzzle_flash].set_visibility(false);
            scene.graph[self.shot_light].set_visibility(false);
        }

        let ignored_collider = if actors.contains(self.owner) {
            ColliderHandle::from(
                scene
                    .physics
                    .bodies
                    .get(actors.get(self.owner).get_body().into())
                    .unwrap()
                    .colliders()[0],
            )
        } else {
            Default::default()
        };
        let dir = self.get_shot_direction(&scene.graph);
        let pos = self.get_shot_position(&scene.graph);
        self.laser_sight.update(scene, pos, dir, ignored_collider)
    }

    pub fn get_shot_position(&self, graph: &Graph) -> Vector3<f32> {
        if self.shot_point.is_some() {
            graph[self.shot_point].global_position()
        } else {
            // Fallback
            graph[self.model].global_position()
        }
    }

    pub fn get_shot_direction(&self, graph: &Graph) -> Vector3<f32> {
        graph[self.model].look_vector()
    }

    pub fn get_kind(&self) -> WeaponKind {
        self.kind
    }

    pub fn world_basis(&self, graph: &Graph) -> Matrix3<f32> {
        graph[self.model].global_transform().basis()
    }

    pub fn add_ammo(&mut self, amount: u32) {
        self.ammo += amount;
    }

    pub fn ammo(&self) -> u32 {
        self.ammo
    }

    pub fn owner(&self) -> Handle<Actor> {
        self.owner
    }

    pub fn set_owner(&mut self, owner: Handle<Actor>) {
        self.owner = owner;
    }

    pub fn switch_flash_light(&self, graph: &mut Graph) {
        if self.flash_light.is_some() {
            let flash_light = &mut graph[self.flash_light];
            let enabled = flash_light.visibility();
            flash_light.set_visibility(!enabled);
        }
    }

    pub fn try_shoot(
        &mut self,
        self_handle: Handle<Weapon>,
        scene: &mut Scene,
        time: GameTime,
        resource_manager: ResourceManager,
        direction: Option<Vector3<f32>>,
    ) {
        if self.ammo != 0 && time.elapsed - self.last_shot_time >= self.definition.shoot_interval {
            self.ammo -= 1;

            self.offset = Vector3::new(0.0, 0.0, -0.05);
            self.last_shot_time = time.elapsed;

            let position = self.get_shot_position(&scene.graph);

            self.sender
                .as_ref()
                .unwrap()
                .send(Message::PlaySound {
                    path: PathBuf::from(self.definition.shot_sound),
                    position,
                    gain: 1.0,
                    rolloff_factor: 5.0,
                    radius: 3.0,
                })
                .unwrap();

            if self.muzzle_flash.is_some() {
                let muzzle_flash = &mut scene.graph[self.muzzle_flash];
                muzzle_flash.set_visibility(true);
                for surface in muzzle_flash.as_mesh_mut().surfaces_mut() {
                    let textures = [
                        "data/particles/muzzle_01.png",
                        "data/particles/muzzle_02.png",
                        "data/particles/muzzle_03.png",
                        "data/particles/muzzle_04.png",
                        "data/particles/muzzle_05.png",
                    ];
                    surface.set_diffuse_texture(Some(
                        resource_manager.request_texture(
                            textures.choose(&mut rg3d::rand::thread_rng()).unwrap(),
                        ),
                    ))
                }
                scene.graph[self.shot_light].set_visibility(true);
                self.muzzle_flash_timer = 0.075;
            }

            let position = self.get_shot_position(&scene.graph);
            let direction = direction
                .unwrap_or_else(|| self.get_shot_direction(&scene.graph))
                .try_normalize(std::f32::EPSILON)
                .unwrap_or_else(|| Vector3::z());

            match self.definition.projectile {
                WeaponProjectile::Projectile(projectile) => self
                    .sender
                    .as_ref()
                    .unwrap()
                    .send(Message::CreateProjectile {
                        kind: projectile,
                        position,
                        direction,
                        owner: self_handle,
                        initial_velocity: Default::default(),
                    })
                    .unwrap(),
                WeaponProjectile::Ray { damage } => {
                    self.sender
                        .as_ref()
                        .unwrap()
                        .send(Message::ShootRay {
                            weapon: self_handle,
                            begin: position,
                            end: position + direction.scale(1000.0),
                            damage,
                        })
                        .unwrap();
                }
            }
        }
    }

    pub fn clean_up(&mut self, scene: &mut Scene) {
        scene.graph.remove_node(self.model);
        scene.graph.remove_node(self.laser_sight.ray);
        scene.graph.remove_node(self.laser_sight.tip);
    }
}

#[derive(Default)]
pub struct WeaponContainer {
    pool: Pool<Weapon>,
}

impl WeaponContainer {
    pub fn new() -> Self {
        Self { pool: Pool::new() }
    }

    pub fn add(&mut self, weapon: Weapon) -> Handle<Weapon> {
        self.pool.spawn(weapon)
    }

    pub fn contains(&self, weapon: Handle<Weapon>) -> bool {
        self.pool.is_valid_handle(weapon)
    }

    pub fn free(&mut self, weapon: Handle<Weapon>) {
        self.pool.free(weapon);
    }

    pub fn iter_mut(&mut self) -> PoolIteratorMut<Weapon> {
        self.pool.iter_mut()
    }

    pub fn update(&mut self, scene: &mut Scene, actors: &ActorContainer, dt: f32) {
        for weapon in self.pool.iter_mut() {
            weapon.update(scene, actors, dt)
        }
    }
}

impl Index<Handle<Weapon>> for WeaponContainer {
    type Output = Weapon;

    fn index(&self, index: Handle<Weapon>) -> &Self::Output {
        &self.pool[index]
    }
}

impl IndexMut<Handle<Weapon>> for WeaponContainer {
    fn index_mut(&mut self, index: Handle<Weapon>) -> &mut Self::Output {
        &mut self.pool[index]
    }
}

impl Visit for WeaponContainer {
    fn visit(&mut self, name: &str, visitor: &mut Visitor) -> VisitResult {
        visitor.enter_region(name)?;

        self.pool.visit("Pool", visitor)?;

        visitor.leave_region()
    }
}