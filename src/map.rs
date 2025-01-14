use crate::{loader::TiledMapLoader, TileMapChunk, TILE_MAP_PIPELINE_HANDLE};
use anyhow::Result;
use bevy::{
    ecs::system::EntityCommands,
    prelude::*,
    reflect::TypeUuid,
    render::mesh::Indices,
    render::{
        draw::Visible, mesh::VertexAttributeValues, pipeline::PrimitiveTopology,
        pipeline::RenderPipeline, render_graph::base::MainPass,
    },
    utils::{HashMap, HashSet},
};
use std::{
    io::BufReader,
    path::{Path, PathBuf},
};

// objects include these by default for now
pub use tiled::ObjectShape;
pub use tiled::Properties;
pub use tiled::PropertyValue;
pub use tiled::LayerData;
pub use tiled;

#[derive(Debug)]
pub struct Tile {
    pub tile_id: u32,
    pub pos: Vec2,
    pub vertex: Vec4,
    pub uv: Vec4,
    pub flip_d: bool,
    pub flip_h: bool,
    pub flip_v: bool,
}

#[derive(Debug)]
pub struct Chunk {
    pub position: Vec2,
    pub tiles: Vec<Vec<Tile>>,
}

#[derive(Debug)]
pub struct TilesetLayer {
    pub tile_size: Vec2,
    pub chunks: Vec<Vec<Chunk>>,
    pub tileset_guid: u32,
}

#[derive(Debug)]
pub struct Layer {
    pub tileset_layers: Vec<TilesetLayer>,
}

// An asset for maps
#[derive(Debug, TypeUuid)]
#[uuid = "5f6fbac8-3f52-424e-a928-561667fea074"]
pub struct Map {
    pub map: tiled::Map,
    pub meshes: Vec<(u32, u32, Mesh)>,
    pub layers: Vec<Layer>,
    pub groups: Vec<ObjectGroup>,
    pub tile_size: Vec2,
    pub image_folder: std::path::PathBuf,
    pub asset_dependencies: Vec<PathBuf>,
}

impl Map {
    pub fn project_ortho(pos: Vec2, tile_width: f32, tile_height: f32) -> Vec2 {
        let x = tile_width * pos.x;
        let y = tile_height * pos.y;
        Vec2::new(x, -y)
    }
    pub fn unproject_ortho(pos: Vec2, tile_width: f32, tile_height: f32) -> Vec2 {
        let x = pos.x / tile_width;
        let y = -(pos.y) / tile_height;
        Vec2::new(x, y)
    }
    pub fn project_iso(pos: Vec2, tile_width: f32, tile_height: f32) -> Vec2 {
        let x = (pos.x - pos.y) * tile_width / 2.0;
        let y = (pos.x + pos.y) * tile_height / 2.0;
        Vec2::new(x, -y)
    }
    pub fn unproject_iso(pos: Vec2, tile_width: f32, tile_height: f32) -> Vec2 {
        let half_width = tile_width / 2.0;
        let half_height = tile_height / 2.0;
        let x = ((pos.x / half_width) + (-(pos.y) / half_height)) / 2.0;
        let y = ((-(pos.y) / half_height) - (pos.x / half_width)) / 2.0;
        Vec2::new(x.round(), y.round())
    }
    pub fn center(&self, origin: Transform) -> Transform {
        let tile_size = Vec2::new(self.map.tile_width as f32, self.map.tile_height as f32);
        let map_center = Vec2::new(self.map.width as f32 / 2.0, self.map.height as f32 / 2.0);
        match self.map.orientation {
            tiled::Orientation::Orthogonal => {
                let center = Map::project_ortho(map_center, tile_size.x, tile_size.y);
                Transform::from_matrix(
                    origin.compute_matrix() * Mat4::from_translation(-center.extend(0.0)),
                )
            }
            tiled::Orientation::Isometric => {
                let center = Map::project_iso(map_center, tile_size.x, tile_size.y);
                Transform::from_matrix(
                    origin.compute_matrix() * Mat4::from_translation(-center.extend(0.0)),
                )
            }
            _ => panic!("Unsupported orientation {:?}", self.map.orientation),
        }
    }

    pub fn try_from_bytes(asset_path: &Path, bytes: Vec<u8>) -> Result<Map> {
        let map = tiled::parse_with_path(BufReader::new(bytes.as_slice()), asset_path).unwrap();

        let mut layers = Vec::new();
        let mut groups = Vec::new();

        // this only works if gids are uniques across all maps used - todo move into ObjectGroup?
        let mut tile_gids: HashMap<u32, u32> = Default::default();

        for tileset in &map.tilesets {
            for i in tileset.first_gid..(tileset.first_gid + tileset.tilecount.unwrap_or(1)) {
                tile_gids.insert(i, tileset.first_gid);
            }
        }

        let mut object_gids: HashSet<u32> = Default::default();
        for object_group in map.object_groups.iter() {
            // recursively creates objects in the groups:
            let tiled_o_g = ObjectGroup::new_with_tile_ids(object_group, &tile_gids);
            // keep track of which objects will need to have tiles loaded
            tiled_o_g.objects.iter().for_each(|o| {
                tile_gids.get(&o.gid).map(|first_gid| {
                    object_gids.insert(*first_gid);
                });
            });
            groups.push(tiled_o_g);
        }

        let target_chunk_x = 32;
        let target_chunk_y = 32;

        let chunk_size_x = (map.width as f32 / target_chunk_x as f32).ceil().max(1.0) as usize;
        let chunk_size_y = (map.height as f32 / target_chunk_y as f32).ceil().max(1.0) as usize;
        let tile_size = Vec2::new(map.tile_width as f32, map.tile_height as f32);
        let image_folder: PathBuf = asset_path.parent().unwrap().into();
        let mut asset_dependencies = Vec::new();

        for layer in map.layers.iter() {
            if !layer.visible {
                continue;
            }
            let mut tileset_layers = Vec::new();

            for tileset in map.tilesets.iter() {
                let tile_width = tileset.tile_width as f32;
                let tile_height = tileset.tile_height as f32;
                let tile_space = tileset.spacing as f32;
                let image = tileset.images.first().unwrap();
                let texture_width = image.width as f32;
                let texture_height = image.height as f32;
                let columns = ((texture_width + tile_space) / (tile_width + tile_space)).floor(); // account for no end tile

                let tile_path = image_folder.join(tileset.images.first().unwrap().source.as_str());
                asset_dependencies.push(tile_path);

                let mut chunks = Vec::new();
                // 32 x 32 tile chunk sizes
                for chunk_x in 0..chunk_size_x {
                    let mut chunks_y = Vec::new();
                    for chunk_y in 0..chunk_size_y {
                        let mut tiles = Vec::new();

                        for tile_x in 0..target_chunk_x {
                            let mut tiles_y = Vec::new();
                            for tile_y in 0..target_chunk_y {
                                let lookup_x = (chunk_x * target_chunk_x) + tile_x;
                                let lookup_y = (chunk_y * target_chunk_y) + tile_y;

                                // Get chunk tile.
                                let chunk_tile = if lookup_x < map.width as usize
                                    && lookup_y < map.height as usize
                                {
                                    // New Tiled crate code:
                                    let map_tile = match &layer.tiles {
                                        tiled::LayerData::Finite(tiles) => {
                                            &tiles[lookup_y][lookup_x]
                                        }
                                        _ => panic!("Infinte maps not supported"),
                                    };

                                    let tile = map_tile.gid;
                                    if tile < tileset.first_gid
                                        || tile >= tileset.first_gid + tileset.tilecount.unwrap()
                                    {
                                        continue;
                                    }

                                    let tile = (TiledMapLoader::remove_tile_flags(tile) as f32)
                                        - tileset.first_gid as f32;

                                    // This calculation is much simpler we only care about getting the remainder
                                    // and multiplying that by the tile width.
                                    let sprite_sheet_x: f32 =
                                        ((tile % columns) * (tile_width + tile_space) - tile_space)
                                            .floor();

                                    // Calculation here is (tile / columns).round_down * (tile_space + tile_height) - tile_space
                                    // Example: tile 30 / 28 columns = 1.0714 rounded down to 1 * 16 tile_height = 16 Y
                                    // which is the 2nd row in the sprite sheet.
                                    // Example2: tile 10 / 28 columns = 0.3571 rounded down to 0 * 16 tile_height = 0 Y
                                    // which is the 1st row in the sprite sheet.
                                    let sprite_sheet_y: f32 = (tile / columns).floor()
                                        * (tile_height + tile_space)
                                        - tile_space;

                                    // Calculate positions
                                    let (start_x, end_x, start_y, end_y) = match map.orientation {
                                        tiled::Orientation::Orthogonal => {
                                            let center = Map::project_ortho(
                                                Vec2::new(lookup_x as f32, lookup_y as f32),
                                                tile_width,
                                                tile_height,
                                            );

                                            let start = Vec2::new(
                                                center.x,
                                                center.y - tile_height - tile_space,
                                            );

                                            let end = Vec2::new(
                                                center.x + tile_width + tile_space,
                                                center.y,
                                            );

                                            (start.x, end.x, start.y, end.y)
                                        }
                                        tiled::Orientation::Isometric => {
                                            let center = Map::project_iso(
                                                Vec2::new(lookup_x as f32, lookup_y as f32),
                                                tile_width,
                                                tile_height,
                                            );

                                            let start = Vec2::new(
                                                center.x - tile_width / 2.0,
                                                center.y - tile_height,
                                            );

                                            let end =
                                                Vec2::new(center.x + tile_width / 2.0, center.y);

                                            (start.x, end.x, start.y, end.y)
                                        }
                                        _ => {
                                            panic!("Unsupported orientation {:?}", map.orientation)
                                        }
                                    };

                                    // Calculate UV:
                                    let start_u: f32 = sprite_sheet_x / texture_width;
                                    let end_u: f32 = (sprite_sheet_x + tile_width) / texture_width;
                                    let start_v: f32 = sprite_sheet_y / texture_height;
                                    let end_v: f32 =
                                        (sprite_sheet_y + tile_height) / texture_height;

                                    Tile {
                                        tile_id: map_tile.gid,
                                        pos: Vec2::new(tile_x as f32, tile_y as f32),
                                        vertex: Vec4::new(start_x, start_y, end_x, end_y),
                                        uv: Vec4::new(start_u, start_v, end_u, end_v),
                                        flip_d: map_tile.flip_d,
                                        flip_h: map_tile.flip_h,
                                        flip_v: map_tile.flip_v,
                                    }
                                } else {
                                    // Empty tile
                                    Tile {
                                        tile_id: 0,
                                        pos: Vec2::new(tile_x as f32, tile_y as f32),
                                        vertex: Vec4::new(0.0, 0.0, 0.0, 0.0),
                                        uv: Vec4::new(0.0, 0.0, 0.0, 0.0),
                                        flip_d: false,
                                        flip_h: false,
                                        flip_v: false,
                                    }
                                };

                                tiles_y.push(chunk_tile);
                            }
                            tiles.push(tiles_y);
                        }

                        let chunk = Chunk {
                            position: Vec2::new(chunk_x as f32, chunk_y as f32),
                            tiles,
                        };
                        chunks_y.push(chunk);
                    }
                    chunks.push(chunks_y);
                }

                let tileset_layer = TilesetLayer {
                    tile_size: Vec2::new(tile_width, tile_height),
                    chunks,
                    tileset_guid: tileset.first_gid,
                };
                tileset_layers.push(tileset_layer);
            }

            let layer = Layer { tileset_layers };
            layers.push(layer);
        }

        let mut meshes = Vec::new();
        for (layer_id, layer) in layers.iter().enumerate() {
            for tileset_layer in layer.tileset_layers.iter() {
                for x in 0..tileset_layer.chunks.len() {
                    let chunk_x = &tileset_layer.chunks[x];
                    for y in 0..chunk_x.len() {
                        let chunk = &chunk_x[y];

                        let mut positions: Vec<[f32; 3]> = Vec::new();
                        let mut uvs: Vec<[f32; 2]> = Vec::new();
                        let mut indices: Vec<u32> = Vec::new();

                        let mut i = 0;
                        for tile in chunk.tiles.iter().flat_map(|tiles_y| tiles_y.iter()) {
                            if tile.tile_id < tileset_layer.tileset_guid {
                                continue;
                            }

                            // X, Y
                            positions.push([tile.vertex.x, tile.vertex.y, 0.0]);
                            // X, Y + 1
                            positions.push([tile.vertex.x, tile.vertex.w, 0.0]);
                            // X + 1, Y + 1
                            positions.push([tile.vertex.z, tile.vertex.w, 0.0]);
                            // X + 1, Y
                            positions.push([tile.vertex.z, tile.vertex.y, 0.0]);

                            let mut next_uvs = [
                                // X, Y
                                [tile.uv.x, tile.uv.w],
                                // X, Y + 1
                                [tile.uv.x, tile.uv.y],
                                // X + 1, Y + 1
                                [tile.uv.z, tile.uv.y],
                                // X + 1, Y
                                [tile.uv.z, tile.uv.w],
                            ];
                            if tile.flip_d {
                                next_uvs.swap(0, 2);
                            }
                            if tile.flip_h {
                                next_uvs.reverse();
                            }
                            if tile.flip_v {
                                next_uvs.reverse();
                                next_uvs.swap(0, 2);
                                next_uvs.swap(1, 3);
                            }

                            next_uvs.iter().for_each(|uv| uvs.push(*uv));

                            indices.extend_from_slice(&[i + 0, i + 2, i + 1, i + 0, i + 3, i + 2]);

                            i += 4;
                        }

                        if positions.len() > 0 {
                            let mut mesh = Mesh::new(PrimitiveTopology::TriangleList);
                            mesh.set_attribute(
                                "Vertex_Position",
                                VertexAttributeValues::Float3(positions),
                            );
                            mesh.set_attribute("Vertex_Uv", VertexAttributeValues::Float2(uvs));
                            mesh.set_indices(Some(Indices::U32(indices)));
                            meshes.push((layer_id as u32, tileset_layer.tileset_guid, mesh));
                        }
                    }
                }
            }
        }

        let map = Map {
            map,
            meshes,
            layers,
            groups,
            tile_size,
            image_folder,
            asset_dependencies,
        };

        Ok(map)
    }
}

#[derive(Default)]
pub struct TiledMapCenter(pub bool);

#[derive(Debug)]
pub struct ObjectGroup {
    pub name: String,
    opacity: f32,
    pub visible: bool,
    pub objects: Vec<Object>,
}

impl ObjectGroup {
    pub fn new_with_tile_ids(
        inner: &tiled::ObjectGroup,
        tile_gids: &HashMap<u32, u32>,
    ) -> ObjectGroup {
        // println!("grp {}", inner.name.to_string());
        ObjectGroup {
            name: inner.name.to_string(),
            opacity: inner.opacity,
            visible: inner.visible,
            objects: inner
                .objects
                .iter()
                .map(|obj| Object::new_with_tile_ids(obj, tile_gids))
                .collect(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Object {
    pub shape: tiled::ObjectShape,
    pub props: tiled::Properties,
    pub position: Vec2,
    pub name: String,
    pub visible: bool,
    gid: u32,                 // sprite ID from tiled::Object
    tileset_gid: Option<u32>, // AKA first_gid
    sprite_index: Option<u32>,
}

impl Object {
    pub fn new(original_object: &tiled::Object) -> Object {
        // println!("obj {} {}", original_object.name, original_object.visible.to_string());
        Object {
            shape: original_object.shape.clone(),
            props: original_object.properties.clone(),
            gid: original_object.gid, // zero for most non-tile objects
            visible: original_object.visible,
            tileset_gid: None,
            sprite_index: None,
            position: Vec2::new(original_object.x, original_object.y),
            name: original_object.name.clone(),
        }
    }

    pub fn is_shape(&self) -> bool {
        self.tileset_gid.is_none()
    }

    pub fn new_with_tile_ids(
        original_object: &tiled::Object,
        tile_gids: &HashMap<u32, u32>,
    ) -> Object {
        // println!("obj {}", original_object.gid.to_string());
        let mut o = Object::new(original_object);
        o.set_tile_ids(tile_gids);
        o
    }
    pub fn set_tile_ids(&mut self, tile_gids: &HashMap<u32, u32>) {
        self.tileset_gid = tile_gids.get(&self.gid).cloned();
        self.sprite_index = self.tileset_gid.map(|first_gid| &self.gid - first_gid);
    }

    pub fn transform_from_map(
        &self,
        map: &tiled::Map,
        map_transform: &Transform,
        tile_scale: Option<Vec3>,
    ) -> Transform {
        // tile scale being None means this is not a tile object

        // clone entire map transform
        let mut transform = map_transform.clone();

        //// this was made obsolete by Kurble's branch changes
        // let map_tile_width = map.tile_width as f32;
        // let map_tile_height = map.tile_height as f32;
        //// offset transform position by 1/2 map tile
        // transform.translation -= map_transform.scale * Vec3::new(map_tile_width, -map_tile_height, 0.0) / 2.0;

        let map_orientation: tiled::Orientation = map.orientation;
        // replacing map Z with something far in front for objects -- should probably be configurable
        // transform.translation.z = 1000.0;
        let z_relative_to_map = 15.0; // used for a range of 5-25 above tile Z coordinate for items (max 20k map)
        match self.shape {
            tiled::ObjectShape::Rect { width, height } => {
                match map_orientation {
                    tiled::Orientation::Orthogonal => {
                        let mut center_offset = Vec2::new(self.position.x, -self.position.y);
                        match tile_scale {
                            None => {
                                // shape object x/y represent top left corner
                                center_offset += Vec2::new(width, -height) / 2.0;
                            }
                            Some(tile_scale) => {
                                // tile object x/y represents bottom left corner
                                center_offset += Vec2::new(width, height) / 2.0;
                                // tile object scale based on map scale and passed-in scale from image dimensions
                                transform.scale = tile_scale * transform.scale;
                            }
                        }
                        // apply map scale to object position, if this is a tile
                        center_offset *= map_transform.scale.truncate();
                        // offset transform by object position
                        transform.translation +=
                            center_offset.extend(z_relative_to_map - center_offset.y / 2000.0);
                        // ^ HACK only support up to 20k pixels maps, TODO: configure in API
                    }
                    // tiled::Orientation::Isometric => {

                    // }
                    _ => panic!("Sorry, {:?} objects aren't supported -- please hide this object layer for now.", map_orientation),
                }
            }
            tiled::ObjectShape::Ellipse {
                width: _,
                height: _,
            } => {}
            tiled::ObjectShape::Polyline { points: _ } => {}
            tiled::ObjectShape::Polygon { points: _ } => {}
            tiled::ObjectShape::Point(_, _) => {}
        }
        transform
    }

    pub fn spawn<'a, 'b>(
        &self,
        commands: &'b mut Commands<'a>,
        texture_atlas: Option<&Handle<TextureAtlas>>,
        map: &tiled::Map,
        map_handle: Handle<Map>,
        tile_map_transform: &Transform,
        debug_config: &DebugConfig,
    ) -> EntityCommands<'a, 'b> {
        let mut new_entity_commands = if let Some(texture_atlas) = texture_atlas {
            let sprite_index = self.sprite_index.expect("missing sprite index");
            let tileset_gid = self.tileset_gid.expect("missing tileset");

            // fetch tile for this object if it exists
            let object_tile_size = map
                .tilesets
                .iter()
                .find(|ts| ts.first_gid == tileset_gid)
                .map(|ts| Vec2::new(ts.tile_width as f32, ts.tile_height as f32));
            // object dimensions
            let dims = self.dimensions();
            // use object dimensions and tile size to determine extra scale to apply for tile objects
            let tile_scale = if let (Some(dims), Some(size)) = (dims, object_tile_size) {
                Some((dims / size).extend(1.0))
            } else {
                None
            };
            commands.spawn_bundle(SpriteSheetBundle {
                transform: self.transform_from_map(&map, tile_map_transform, tile_scale),
                texture_atlas: texture_atlas.clone(),
                sprite: TextureAtlasSprite {
                    index: sprite_index,
                    ..Default::default()
                },
                visible: Visible {
                    is_visible: self.visible,
                    is_transparent: true,
                    ..Default::default()
                },
                ..Default::default()
            })
        } else {
            // commands.spawn((self.map_transform(&map.map, &tile_map_transform, None), GlobalTransform::default()))
            let dimensions = self
                .dimensions()
                .expect("Don't know how to handle object without dimensions");
            let transform = self.transform_from_map(&map, &tile_map_transform, None);
            commands
                // Debug box.
                .spawn_bundle(SpriteBundle {
                    material: debug_config
                        .material
                        .clone()
                        .unwrap_or_else(|| Handle::<ColorMaterial>::default()),
                    sprite: Sprite::new(dimensions),
                    transform,
                    visible: Visible {
                        is_visible: debug_config.enabled,
                        is_transparent: true,
                        ..Default::default()
                    },
                    ..Default::default()
                })
        };

        new_entity_commands.insert_bundle((map_handle, self.clone()));
        new_entity_commands
    }

    pub fn dimensions(&self) -> Option<Vec2> {
        match self.shape {
            tiled::ObjectShape::Rect { width, height }
            | tiled::ObjectShape::Ellipse { width, height } => Some(Vec2::new(width, height)),
            tiled::ObjectShape::Polyline { points: _ }
            | tiled::ObjectShape::Polygon { points: _ }
            | tiled::ObjectShape::Point(_, _) => Some(Vec2::splat(1.0)),
        }
    }
}

pub struct MapRoot; // used so consuming application can query for parent

pub struct DebugConfig {
    pub enabled: bool,
    pub material: Option<Handle<ColorMaterial>>,
}

impl Default for DebugConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            material: Default::default(),
        }
    }
}

/// A bundle of tiled map entities.
#[derive(Bundle)]
pub struct TiledMapBundle {
    pub map_asset: Handle<Map>,
    pub parent_option: Option<Entity>,
    pub materials: HashMap<u32, Handle<ColorMaterial>>,
    pub atlases: HashMap<u32, Handle<TextureAtlas>>,
    pub origin: Transform,
    pub center: TiledMapCenter,
    pub debug_config: DebugConfig,
    pub created_entities: CreatedMapEntities,
}

impl Default for TiledMapBundle {
    fn default() -> Self {
        Self {
            map_asset: Handle::default(),
            parent_option: None,
            materials: HashMap::default(),
            atlases: HashMap::default(),
            center: TiledMapCenter::default(),
            origin: Transform::default(),
            debug_config: Default::default(),
            created_entities: Default::default(),
        }
    }
}

#[derive(Default, Debug)]
pub struct CreatedMapEntities {
    // maps layer id and tileset_gid to mesh entities
    created_layer_entities: HashMap<(usize, u32), Vec<Entity>>,
    // maps object guid to texture atlas sprite entity
    created_object_entities: HashMap<u32, Vec<Entity>>,
}

#[derive(Bundle)]
pub struct ChunkBundle {
    pub map_parent: Handle<Map>, // tmp:chunks should be child entities of a toplevel map entity.
    pub chunk: TileMapChunk,
    pub main_pass: MainPass,
    pub material: Handle<ColorMaterial>,
    pub render_pipeline: RenderPipelines,
    pub visible: Visible,
    pub draw: Draw,
    pub mesh: Handle<Mesh>,
    pub transform: Transform,
    pub global_transform: GlobalTransform,
}

impl Default for ChunkBundle {
    fn default() -> Self {
        Self {
            map_parent: Handle::default(),
            chunk: TileMapChunk::default(),
            visible: Visible {
                is_transparent: true,
                ..Default::default()
            },
            draw: Default::default(),
            main_pass: MainPass,
            mesh: Handle::default(),
            material: Handle::default(),
            render_pipeline: RenderPipelines::from_pipelines(vec![RenderPipeline::new(
                TILE_MAP_PIPELINE_HANDLE.typed(),
            )]),
            transform: Default::default(),
            global_transform: Default::default(),
        }
    }
}

pub fn process_loaded_tile_maps(
    mut commands: Commands,
    asset_server: Res<AssetServer>,
    mut map_events: EventReader<AssetEvent<Map>>,
    mut ready_events: EventWriter<ObjectReadyEvent>,
    mut map_ready_events: EventWriter<MapReadyEvent>,
    mut maps: ResMut<Assets<Map>>,
    mut meshes: ResMut<Assets<Mesh>>,
    mut materials: ResMut<Assets<ColorMaterial>>,
    mut texture_atlases: ResMut<Assets<TextureAtlas>>,
    mut query: Query<(
        Entity,
        &TiledMapCenter,
        &Handle<Map>,
        &Option<Entity>,
        &mut HashMap<u32, Handle<ColorMaterial>>,
        &mut HashMap<u32, Handle<TextureAtlas>>,
        &Transform,
        &mut DebugConfig,
        &mut CreatedMapEntities,
    )>,
) {
    let mut changed_maps = HashSet::<Handle<Map>>::default();
    for event in map_events.iter() {
        match event {
            AssetEvent::Created { handle } => {
                changed_maps.insert(handle.clone());
            }
            AssetEvent::Modified { handle } => {
                changed_maps.insert(handle.clone());
            }
            AssetEvent::Removed { handle } => {
                // if mesh was modified and removed in the same update, ignore the modification
                // events are ordered so future modification events are ok
                changed_maps.remove(handle);
            }
        }
    }

    let mut new_meshes = HashMap::<&Handle<Map>, Vec<(u32, u32, Handle<Mesh>)>>::default();

    for changed_map in changed_maps.iter() {
        let map = maps.get_mut(changed_map).unwrap();

        for (_, _, map_handle, _, mut materials_map, mut texture_atlas_map, _, _, _) in
            query.iter_mut()
        {
            // only deal with currently changed map
            if map_handle != changed_map {
                continue;
            }

            for tileset in &map.map.tilesets {
                if !materials_map.contains_key(&tileset.first_gid) {
                    let texture_path = map
                        .image_folder
                        .join(tileset.images.first().unwrap().source.as_str());
                    let texture_handle = asset_server.load(texture_path);
                    materials_map.insert(
                        tileset.first_gid,
                        materials.add(texture_handle.clone().into()),
                    );

                    // only generate texture_atlas for tilesets used in objects
                    let object_gids: Vec<_> = map
                        .groups
                        .iter()
                        .flat_map(|og| og.objects.iter().map(|o| o.tileset_gid))
                        .collect();
                    if object_gids.contains(&Some(tileset.first_gid)) {
                        // For simplicity use textureAtlasSprite for object layers
                        // these insertions should be limited to sprites referenced by objects
                        let tile_width = tileset.tile_width as f32;
                        let tile_height = tileset.tile_height as f32;
                        let image = tileset.images.first().unwrap();
                        let texture_width = image.width as f32;
                        let texture_height = image.height as f32;
                        let columns = (texture_width / tile_width).floor() as usize;
                        let rows = (texture_height / tile_height).floor() as usize;

                        let has_new = (0..(columns * rows) as u32).fold(false, |total, next| {
                            total || !texture_atlas_map.contains_key(&(tileset.first_gid + next))
                        });
                        if has_new {
                            let atlas = TextureAtlas::from_grid(
                                texture_handle.clone(),
                                Vec2::new(tile_width, tile_height),
                                columns,
                                rows,
                            );
                            let atlas_handle = texture_atlases.add(atlas);
                            for i in 0..(columns * rows) as u32 {
                                if texture_atlas_map.contains_key(&(tileset.first_gid + i)) {
                                    continue;
                                }
                                // println!("insert: {}", tileset.first_gid + i);
                                texture_atlas_map
                                    .insert(tileset.first_gid + i, atlas_handle.clone());
                            }
                        }
                    }
                }
            }
        }

        for mesh in map.meshes.drain(0..map.meshes.len()) {
            let handle = meshes.add(mesh.2);
            if new_meshes.contains_key(changed_map) {
                let mesh_list = new_meshes.get_mut(changed_map).unwrap();
                mesh_list.push((mesh.0, mesh.1, handle));
            } else {
                let mut mesh_list = Vec::new();
                mesh_list.push((mesh.0, mesh.1, handle));
                new_meshes.insert(changed_map, mesh_list);
            }
        }
    }

    for (
        _,
        center,
        map_handle,
        optional_parent,
        materials_map,
        texture_atlas_map,
        origin,
        mut debug_config,
        mut created_entities,
    ) in query.iter_mut()
    {
        if new_meshes.contains_key(map_handle) {
            let map = maps.get(map_handle).unwrap();

            let tile_map_transform = if center.0 {
                map.center(origin.clone())
            } else {
                origin.clone()
            };

            let mesh_list = new_meshes.get_mut(map_handle).unwrap();

            for (layer_id, layer) in map.layers.iter().enumerate() {
                for tileset_layer in layer.tileset_layers.iter() {
                    let material_handle = materials_map.get(&tileset_layer.tileset_guid).unwrap();
                    // let mut mesh_list = mesh_list.iter_mut().filter(|(mesh_layer_id, _)| *mesh_layer_id == layer_id as u32).drain(0..mesh_list.len()).collect::<Vec<_>>();
                    let chunk_mesh_list = mesh_list
                        .iter()
                        .filter(|(mesh_layer_id, tileset_guid, _)| {
                            *mesh_layer_id == layer_id as u32
                                && *tileset_guid == tileset_layer.tileset_guid
                        })
                        .collect::<Vec<_>>();

                    // removing entities consumes the record of created entities
                    created_entities
                        .created_layer_entities
                        .remove(&(layer_id, tileset_layer.tileset_guid))
                        .map(|entities| {
                            // println!("Despawning previously-created mesh for this chunk");
                            for entity in entities.iter() {
                                // println!("calling despawn on {:?}", entity);
                                commands.entity(*entity).despawn();
                            }
                        });
                    let mut chunk_entities: Vec<Entity> = Default::default();

                    for (_, tileset_guid, mesh) in chunk_mesh_list.iter() {
                        // TODO: Sadly bevy doesn't support multiple meshes on a single entity with multiple materials.
                        // Change this once it does.

                        // Instead for now spawn a new entity per chunk.
                        let chunk_entity = commands
                            .spawn_bundle(ChunkBundle {
                                chunk: TileMapChunk {
                                    // TODO: Support more layers here..
                                    layer_id: layer_id as f32,
                                },
                                material: material_handle.clone(),
                                mesh: mesh.clone(),
                                map_parent: map_handle.clone(),
                                transform: tile_map_transform.clone(),
                                ..Default::default()
                            })
                            .id();

                        // println!("added created_entry after spawn");
                        created_entities
                            .created_layer_entities
                            .entry((layer_id, *tileset_guid))
                            .or_insert_with(|| Vec::new())
                            .push(chunk_entity);
                        chunk_entities.push(chunk_entity);
                    }
                    // if parent was passed in add children and mark it as MapRoot (temp until map bundle returns real entity)
                    if let Some(parent_entity) = optional_parent {
                        commands
                            .entity(parent_entity.clone())
                            .push_children(&chunk_entities)
                            .insert(MapRoot);
                    }
                }
            }

            if debug_config.enabled && debug_config.material.is_none() {
                debug_config.material =
                    Some(materials.add(ColorMaterial::from(Color::rgba(0.4, 0.4, 0.9, 0.5))));
            }
            for object_group in map.groups.iter() {
                for object in object_group.objects.iter() {
                    created_entities
                        .created_object_entities
                        .remove(&object.gid)
                        .map(|entities| {
                            // println!("Despawning previously-created object sprite");
                            for entity in entities.iter() {
                                // println!("calling despawn on {:?}", entity);
                                commands.entity(*entity).despawn();
                            }
                        });
                }
                if !object_group.visible {
                    continue;
                }

                let mut object_entities: Vec<Entity> = Default::default();

                // TODO: use object_group.name, opacity, colour (properties)
                for object in object_group.objects.iter() {
                    // println!("in object_group {}, object {:?}, grp: {}", object_group.name, &object.tileset_gid, object.gid);
                    let atlas_handle = object
                        .tileset_gid
                        .and_then(|tileset_gid| texture_atlas_map.get(&tileset_gid));

                    let entity = object
                        .spawn(
                            &mut commands,
                            atlas_handle,
                            &map.map,
                            map_handle.clone(),
                            &tile_map_transform,
                            &debug_config,
                        )
                        .id();
                    // when done spawning, fire event
                    let evt = ObjectReadyEvent {
                        entity: entity.clone(),
                        map_handle: map_handle.clone(),
                        map_entity_option: optional_parent.clone(),
                    };
                    ready_events.send(evt);

                    created_entities
                        .created_object_entities
                        .entry(object.gid)
                        .or_insert_with(|| Vec::new())
                        .push(entity);
                    object_entities.push(entity);
                }

                // if parent was passed in add children
                if let Some(parent_entity) = optional_parent {
                    commands
                        .entity(parent_entity.clone())
                        .push_children(&object_entities);
                }
            }
            let evt = MapReadyEvent {
                map_handle: map_handle.clone(),
                map_entity_option: optional_parent.clone(),
            };
            map_ready_events.send(evt);
        }
    }
}

// events fired when entity has been created

pub struct ObjectReadyEvent {
    pub entity: Entity,
    pub map_handle: Handle<Map>,
    pub map_entity_option: Option<Entity>,
}

pub struct MapReadyEvent {
    pub map_handle: Handle<Map>,
    pub map_entity_option: Option<Entity>,
}
