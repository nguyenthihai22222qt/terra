use crate::cache::{self, PriorityCacheEntry};
use crate::coordinates;
use crate::gpu_state::GpuState;
use cache::LayerType;
use cgmath::Vector3;
use fnv::FnvHashMap;
use serde::{Deserialize, Serialize};
use std::{num::NonZeroU32, sync::Arc};
use types::{Priority, VNode, MAX_QUADTREE_LEVEL};
use vec_map::VecMap;

use super::{GeneratorMask, LayerMask, Levels, TileCache};

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum TextureFormat {
    R8,
    RG8,
    RGBA8,
    RG16F,
    RGBA16F,
    R32,
    R32F,
    RG32F,
    RGBA32F,
    SRGBA,
    BC4,
    BC5,
    UASTC,
}
impl TextureFormat {
    /// Returns the number of bytes in a single texel of the format. Actually reports bytes per
    /// block for compressed formats.
    pub fn bytes_per_block(&self) -> usize {
        match *self {
            TextureFormat::R8 => 1,
            TextureFormat::RG8 => 2,
            TextureFormat::RGBA8 => 4,
            TextureFormat::RG16F => 4,
            TextureFormat::RGBA16F => 8,
            TextureFormat::R32 => 4,
            TextureFormat::R32F => 4,
            TextureFormat::RG32F => 8,
            TextureFormat::RGBA32F => 16,
            TextureFormat::SRGBA => 4,
            TextureFormat::BC4 => 8,
            TextureFormat::BC5 => 16,
            TextureFormat::UASTC => 16,
        }
    }
    pub fn to_wgpu(&self, wgpu_features: wgpu::Features) -> wgpu::TextureFormat {
        match *self {
            TextureFormat::R8 => wgpu::TextureFormat::R8Unorm,
            TextureFormat::RG8 => wgpu::TextureFormat::Rg8Unorm,
            TextureFormat::RGBA8 => wgpu::TextureFormat::Rgba8Unorm,
            TextureFormat::RG16F => wgpu::TextureFormat::Rg16Float,
            TextureFormat::RGBA16F => wgpu::TextureFormat::Rgba16Float,
            TextureFormat::R32 => wgpu::TextureFormat::R32Uint,
            TextureFormat::R32F => wgpu::TextureFormat::R32Float,
            TextureFormat::RG32F => wgpu::TextureFormat::Rg32Float,
            TextureFormat::RGBA32F => wgpu::TextureFormat::Rgba32Float,
            TextureFormat::SRGBA => wgpu::TextureFormat::Rgba8UnormSrgb,
            TextureFormat::BC4 => wgpu::TextureFormat::Bc4RUnorm,
            TextureFormat::BC5 => wgpu::TextureFormat::Bc5RgUnorm,
            TextureFormat::UASTC => {
                if wgpu_features.contains(wgpu::Features::TEXTURE_COMPRESSION_BC) {
                    wgpu::TextureFormat::Bc7RgbaUnorm
                } else if wgpu_features.contains(wgpu::Features::TEXTURE_COMPRESSION_ASTC_LDR) {
                    wgpu::TextureFormat::Astc {
                        block: wgpu::AstcBlock::B4x4,
                        channel: wgpu::AstcChannel::Unorm,
                    }
                } else {
                    unreachable!("Wgpu reports no texture compression support?")
                }
            }
        }
    }
    pub fn block_size(&self) -> u32 {
        match *self {
            TextureFormat::BC4 | TextureFormat::BC5 | TextureFormat::UASTC => 4,
            TextureFormat::R8
            | TextureFormat::RG8
            | TextureFormat::RGBA8
            | TextureFormat::RG16F
            | TextureFormat::RGBA16F
            | TextureFormat::R32F
            | TextureFormat::R32
            | TextureFormat::RG32F
            | TextureFormat::RGBA32F
            | TextureFormat::SRGBA => 1,
        }
    }
    pub fn is_compressed(&self) -> bool {
        match *self {
            TextureFormat::BC4 | TextureFormat::BC5 | TextureFormat::UASTC => true,
            TextureFormat::R8
            | TextureFormat::RG8
            | TextureFormat::RGBA8
            | TextureFormat::RG16F
            | TextureFormat::RGBA16F
            | TextureFormat::R32
            | TextureFormat::R32F
            | TextureFormat::RG32F
            | TextureFormat::RGBA32F
            | TextureFormat::SRGBA => false,
        }
    }
}

#[derive(Copy, Clone)]
#[repr(C, align(4))]
pub(crate) struct NodeSlot {
    pub(super) node_center: [f64; 3],
    pub(super) padding0: f64,

    pub(super) layer_origins: [[f32; 2]; 48],
    pub(super) layer_ratios: [f32; 48],
    pub(super) layer_slots: [i32; 48],

    pub(super) relative_position: [f32; 3],
    pub(super) min_distance: f32,

    pub(super) mesh_valid_mask: [u32; 4],

    pub(super) face: u32,
    pub(super) level: u32,
    pub(super) coords: [u32; 2],

    pub(super) parent: i32,
    pub(super) padding: [u32; 43],
}
unsafe impl bytemuck::Pod for NodeSlot {}
unsafe impl bytemuck::Zeroable for NodeSlot {}

#[derive(Copy, Clone, Debug, Serialize, Deserialize)]
pub(crate) struct ByteRange {
    pub offset: usize,
    pub length: usize,
}

#[derive(Clone, Debug)]
pub(crate) struct LayerParams {
    /// What kind of layer this is. There can be at most one of each layer type in a file.
    pub layer_type: LayerType,
    /// Number of samples in each dimension, per tile.
    pub texture_resolution: u32,
    /// Number of samples outside the tile on each side.
    pub texture_border_size: u32,
    /// Format used by this layer.
    pub texture_format: &'static [TextureFormat],

    pub grid_registration: bool,

    pub min_level: u8,
    pub max_level: u8,
}

#[derive(Clone)]
pub(super) enum CpuHeightmap {
    I16 { min: f32, max: f32, heights: Vec<i16> },
    F32 { min: f32, max: f32, heights: Arc<Vec<f32>> },
}

#[derive(Clone)]
pub(super) struct Entry {
    /// How imporant this entry is for the current frame.
    pub(super) priority: Priority,
    /// The node this entry is for.
    pub(super) node: VNode,
    /// bitmask of whether the tile for each layer is valid.
    pub(super) valid: LayerMask,
    /// bitmask of whether the tile for each layer is currently being streamed.
    streaming: bool,
    /// A CPU copy of the heightmap tile, useful for collision detection and such.
    heightmap: Option<CpuHeightmap>,
    /// Map from layer to the generators that were used (perhaps indirectly) to produce it.
    pub(super) generators: VecMap<GeneratorMask>,
}
impl Entry {
    pub(super) fn new(node: VNode, priority: Priority) -> Self {
        Self {
            node,
            priority,
            valid: LayerMask::empty(),
            streaming: false,
            heightmap: None,
            generators: VecMap::new(),
        }
    }
}
impl PriorityCacheEntry for Entry {
    type Key = VNode;
    fn priority(&self) -> Priority {
        self.priority
    }
    fn key(&self) -> VNode {
        self.node
    }
}

impl TileCache {
    pub(super) fn generate_tiles(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        gpu_state: &GpuState,
    ) -> (wgpu::CommandBuffer, Vec<(VNode, wgpu::Buffer)>) {
        let mut planned_heightmap_downloads = Vec::new();
        let mut pending_generate = Vec::new();

        for layer in self.layers.values().filter(|l| !l.layer_type.dynamic()) {
            for level in layer.min_level..=layer.max_level {
                for ref mut entry in self.levels.0[level as usize].slots_mut() {
                    if entry.priority() > Priority::cutoff() {
                        let ty = layer.layer_type;
                        if !entry.valid.contains_layer(ty) {
                            if level >= layer.layer_type.streamed_levels() {
                                pending_generate.push(entry.node);
                            } else if !entry.streaming && self.streamer.num_inflight() < 128 {
                                entry.streaming = true;
                                self.streamer.request_tile(entry.node);
                                // println!("Requesting tile level={}", entry.node.level());
                            }
                        }
                    }
                }
            }
        }
        for mesh in self.meshes.values() {
            for level in mesh.desc.min_level..=mesh.desc.max_level {
                for ref mut entry in self.levels.0[level as usize].slots_mut() {
                    if entry.priority() > Priority::cutoff()
                        && !entry.valid.contains_mesh(mesh.desc.ty)
                    {
                        pending_generate.push(entry.node);
                        break;
                    }
                }
            }
        }

        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("encoder.tiles.generate"),
        });

        let mut uniform_data = Vec::new();
        let mut tiles_generated = 0;
        let mut nodes = pending_generate.into_iter().peekable();
        while tiles_generated < 16 && nodes.peek().is_some() {
            let n = nodes.next().unwrap();
            let slot = self.levels.get_slot(n).unwrap();
            let parent_slot = n.parent().and_then(|p| self.levels.get_slot(p.0));
            let level_mask = self.level_masks[n.level() as usize];

            let entry = self.levels.0[n.level() as usize].entry(&n).unwrap();
            let parent_entry = if let Some(p) = n.parent() {
                self.levels.0[p.0.level() as usize].entry(&p.0)
            } else {
                None
            };

            let mut download_buffers_used = 0;
            let mut generators_used = GeneratorMask::empty();
            let mut generated_layers = LayerMask::empty();
            for (generator_index, generator) in self.generators.iter_mut().enumerate() {
                let outputs = generator.outputs();
                let peer_inputs = generator.peer_inputs();
                let parent_inputs = generator.parent_inputs();
                let ancestor_inputs = generator.ancestor_inputs();

                if outputs & !(entry.valid | generated_layers) & level_mask == LayerMask::empty() {
                    continue; // nothing to do
                }
                if peer_inputs & !(entry.valid | generated_layers) != LayerMask::empty() {
                    continue; // missing peer inputs
                }
                if n.level() == 0 && generator.parent_inputs() != LayerMask::empty() {
                    continue; // generator doesn't work on root nodes
                }
                if n.level() > 0
                    && (parent_entry.is_none()
                        || parent_inputs & !parent_entry.as_ref().unwrap().valid
                            != LayerMask::empty())
                {
                    continue; // missing parent inputs
                }
                if outputs.contains_layer(LayerType::Heightmaps)
                    && self.free_download_buffers.is_empty()
                    && self.total_download_buffers + download_buffers_used == 64
                {
                    continue; // no more download buffers
                }
                if !LayerType::iter().filter(|layer| ancestor_inputs.contains_layer(*layer)).all(
                    |layer| {
                        if entry.node.level() < self.layers[layer].min_level {
                            true
                        } else if entry.node.level() <= self.layers[layer].max_level {
                            self.levels.contains_layer(entry.node, layer)
                        } else {
                            let ancestor = entry
                                .node
                                .find_ancestor(|node| node.level() == self.layers[layer].max_level)
                                .unwrap()
                                .0;
                            self.levels.contains_layer(ancestor, layer)
                        }
                    },
                ) {
                    continue; // missing ancestor inputs
                }

                let output_mask =
                    !(entry.valid | generated_layers) & level_mask & generator.outputs();
                generator.generate(
                    device,
                    &mut encoder,
                    gpu_state,
                    &self.layers,
                    slot,
                    parent_slot,
                    &mut uniform_data,
                );

                generators_used |= GeneratorMask::from_index(generator_index);
                generators_used |= self.levels.generator_dependencies(n, peer_inputs);
                if parent_entry.is_some() {
                    generators_used |=
                        self.levels.generator_dependencies(n.parent().unwrap().0, parent_inputs);
                }

                tiles_generated += 1;
                generated_layers |= output_mask;

                if output_mask.contains_layer(LayerType::Heightmaps)
                    && n.level() <= VNode::LEVEL_CELL_1M
                {
                    let bytes_per_pixel = self.layers[LayerType::Heightmaps].texture_format[0]
                        .bytes_per_block() as u64;
                    let resolution = self.layers[LayerType::Heightmaps].texture_resolution as u64;
                    let row_bytes = resolution * bytes_per_pixel;
                    let row_pitch = (row_bytes + 255) & !255;

                    let buffer = self.free_download_buffers.pop().unwrap_or_else(|| {
                        download_buffers_used += 1;
                        device.create_buffer(&wgpu::BufferDescriptor {
                            size: row_pitch * resolution,
                            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
                            label: Some(&format!("buffer.tiles.download")),
                            mapped_at_creation: false,
                        })
                    });
                    encoder.copy_texture_to_buffer(
                        wgpu::ImageCopyTexture {
                            texture: &gpu_state.tile_cache[LayerType::Heightmaps][0].0,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x: 0, y: 0, z: slot as u32 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        wgpu::ImageCopyBuffer {
                            buffer: &buffer,
                            layout: wgpu::ImageDataLayout {
                                offset: 0,
                                bytes_per_row: Some(NonZeroU32::new(row_pitch as u32).unwrap()),
                                rows_per_image: None,
                            },
                        },
                        wgpu::Extent3d {
                            width: resolution as u32,
                            height: resolution as u32,
                            depth_or_array_layers: 1,
                        },
                    );

                    planned_heightmap_downloads.push((n, buffer));
                }
            }

            self.total_download_buffers += download_buffers_used;
            let entry = self.levels.get_mut(n).unwrap();
            entry.valid |= generated_layers;
            for layer in LayerType::iter().filter(|&layer| generated_layers.contains_layer(layer)) {
                entry.generators.insert(layer.index(), generators_used);
            }
        }

        queue.write_buffer(&gpu_state.generate_uniforms, 0, &uniform_data);

        (encoder.finish(), planned_heightmap_downloads)
    }

    pub fn run_dynamic_generators(
        &self,
        queue: &wgpu::Queue,
        encoder: &mut wgpu::CommandEncoder,
        gpu_state: &GpuState,
    ) {
        let mut uniform_data = Vec::new();

        for g in &self.dynamic_generators {
            let mut nodes = Vec::new();
            for level in g.min_level..=g.max_level {
                let base = Levels::base_slot(level);
                for (i, slot) in self.levels.0[level as usize].slots().iter().enumerate() {
                    if slot.priority >= Priority::cutoff()
                        && g.dependency_mask & !slot.valid == LayerMask::empty()
                    {
                        nodes.push((base + i) as u32);
                    }
                }
            }

            if !nodes.is_empty() {
                assert!(nodes.len() <= 1024);
                let uniform_offset = uniform_data.len();
                uniform_data.extend_from_slice(bytemuck::cast_slice(&nodes));
                uniform_data.resize(uniform_offset + 4096, 0);

                let mut cpass =
                    encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
                cpass.set_pipeline(&g.bindgroup_pipeline.as_ref().unwrap().1);
                cpass.set_bind_group(
                    0,
                    &g.bindgroup_pipeline.as_ref().unwrap().0,
                    &[uniform_offset as u32],
                );
                cpass.dispatch_workgroups(g.resolution.0, g.resolution.1, nodes.len() as u32);
            }
        }

        queue.write_buffer(&gpu_state.generate_uniforms, 0, &uniform_data);
    }

    pub(super) fn upload_tiles(
        &mut self,
        queue: &wgpu::Queue,
        textures: &VecMap<Vec<(wgpu::Texture, wgpu::TextureView)>>,
    ) {
        while let Some(tile) = self.streamer.try_complete() {
            if let Some(entry) = self.levels.0[tile.node.level() as usize].entry_mut(&tile.node) {
                entry.streaming = false;
                for layer_index in tile.layers.keys() {
                    let layer = LayerType::from_index(layer_index);
                    if tile.node.level() < self.layers[layer].min_level
                        || tile.node.level() > self.layers[layer].max_level
                    {
                        continue;
                    }
                    entry.valid |= LayerType::from_index(layer_index).bit_mask();
                }

                let index = self.levels.get_slot(tile.node).unwrap();
                for (layer_index, mut data) in tile.layers {
                    let layer = LayerType::from_index(layer_index);
                    let resolution = self.resolution(layer) as usize;
                    let resolution_blocks = self.resolution_blocks(layer) as usize;
                    let bytes_per_block = self.layers[layer].texture_format[0].bytes_per_block();
                    let row_bytes = resolution_blocks * bytes_per_block;

                    if tile.node.level() < self.layers[layer].min_level
                        || tile.node.level() > self.layers[layer].max_level
                    {
                        continue;
                    }

                    if data.is_empty() {
                        data.resize(row_bytes * resolution_blocks, 0);
                    }

                    if cfg!(feature = "small-trace") {
                        for y in 0..resolution_blocks {
                            for x in 0..resolution_blocks {
                                if x % 16 == 0 && y % 16 == 0 {
                                    continue;
                                }
                                let src =
                                    ((x & !15) + (y & !15) * resolution_blocks) * bytes_per_block;
                                let dst = (x + y * resolution_blocks) * bytes_per_block;
                                data.copy_within(src..src + bytes_per_block, dst);
                            }
                        }
                    }
                    assert_eq!(textures[layer].len(), 1);
                    queue.write_texture(
                        wgpu::ImageCopyTexture {
                            texture: &textures[layer][0].0,
                            mip_level: 0,
                            origin: wgpu::Origin3d { x: 0, y: 0, z: index as u32 },
                            aspect: wgpu::TextureAspect::All,
                        },
                        &*data,
                        wgpu::ImageDataLayout {
                            offset: 0,
                            bytes_per_row: Some(NonZeroU32::new(row_bytes as u32).unwrap()),
                            rows_per_image: None,
                        },
                        wgpu::Extent3d {
                            width: resolution as u32,
                            height: resolution as u32,
                            depth_or_array_layers: 1,
                        },
                    );
                }

                if let Some(entry) = self.levels.0[tile.node.level() as usize].entry_mut(&tile.node)
                {
                    let min = *tile.heightmap.iter().min().unwrap() as f32;
                    let max = *tile.heightmap.iter().max().unwrap() as f32;
                    entry.heightmap = Some(CpuHeightmap::I16 { min, max, heights: tile.heightmap });
                }
            }
        }
    }

    pub(super) fn download_tiles(&mut self) {
        while let Ok((node, buffer, heightmap)) = self.completed_downloads_rx.try_recv() {
            if let Some(entry) = self.levels.0[node.level() as usize].entry_mut(&node) {
                self.free_download_buffers.push(buffer);
                entry.heightmap = Some(heightmap);
            }
        }
    }

    pub(crate) fn make_gpu_tile_cache(
        &self,
        device: &wgpu::Device,
    ) -> VecMap<Vec<(wgpu::Texture, wgpu::TextureView)>> {
        self.layers
            .iter()
            .map(|(ty, layer)| {
                assert!(layer.min_level <= layer.max_level);
                let textures = layer
                    .texture_format
                    .iter()
                    .enumerate()
                    .map(|(i, format)| {
                        let texture = device.create_texture(&wgpu::TextureDescriptor {
                            size: wgpu::Extent3d {
                                width: layer.texture_resolution,
                                height: layer.texture_resolution,
                                depth_or_array_layers: (Levels::base_slot(layer.max_level + 1)
                                    - Levels::base_slot(layer.min_level))
                                    as u32,
                            },
                            format: format.to_wgpu(device.features()),
                            mip_level_count: 1,
                            sample_count: 1,
                            dimension: wgpu::TextureDimension::D2,
                            usage: wgpu::TextureUsages::COPY_SRC
                                | wgpu::TextureUsages::COPY_DST
                                | wgpu::TextureUsages::TEXTURE_BINDING
                                | if !format.is_compressed() {
                                    wgpu::TextureUsages::STORAGE_BINDING
                                } else {
                                    wgpu::TextureUsages::empty()
                                },
                            label: Some(&format!(
                                "texture.tiles.{}{}",
                                LayerType::from_index(ty).name(),
                                i
                            )),
                        });
                        let view = texture.create_view(&wgpu::TextureViewDescriptor {
                            label: Some(&format!(
                                "texture.tiles.{}{}.view",
                                LayerType::from_index(ty).name(),
                                i,
                            )),
                            ..Default::default()
                        });
                        (texture, view)
                    })
                    .collect();
                (ty, textures)
            })
            .collect()
    }

    pub fn compute_visible(&self, layer_mask: LayerMask) -> Vec<(VNode, u8)> {
        // Any node with all needed layers in cache is visible...
        let mut node_visibilities: FnvHashMap<VNode, bool> = FnvHashMap::default();
        VNode::breadth_first(|node| match self.levels.0[node.level() as usize].entry(&node) {
            Some(entry) => {
                let visible = (node.level() == 0 || entry.priority >= Priority::cutoff())
                    && layer_mask & !entry.valid == LayerMask::empty();
                node_visibilities.insert(node, visible);
                visible && node.level() < MAX_QUADTREE_LEVEL
            }
            None => {
                node_visibilities.insert(node, false);
                false
            }
        });

        // ...Except if all its children are visible instead.
        let mut visible_nodes = Vec::new();
        VNode::breadth_first(|node| {
            if node.level() < MAX_QUADTREE_LEVEL && node_visibilities[&node] {
                let mut mask = 0;
                for (i, c) in node.children().iter().enumerate() {
                    if !node_visibilities[c] {
                        mask = mask | (1 << i);
                    }
                }

                if mask > 0 {
                    visible_nodes.push((node, mask));
                }

                mask < 15
            } else if node_visibilities[&node] {
                visible_nodes.push((node, 15));
                false
            } else {
                false
            }
        });

        visible_nodes
    }

    fn resolution(&self, ty: LayerType) -> u32 {
        self.layers[ty].texture_resolution
    }
    fn resolution_blocks(&self, ty: LayerType) -> u32 {
        assert_eq!(self.layers[ty].texture_format.len(), 1);
        let resolution = self.layers[ty].texture_resolution;
        let block_size = self.layers[ty].texture_format[0].block_size();
        assert_eq!(resolution % block_size, 0);
        resolution / block_size
    }

    pub fn get_height(&self, latitude: f64, longitude: f64, level: u8) -> Option<f32> {
        let ecef = coordinates::polar_to_ecef(Vector3::new(latitude, longitude, 0.0));
        let cspace = ecef / ecef.x.abs().max(ecef.y.abs()).max(ecef.z.abs());

        let (node, x, y) = VNode::from_cspace(cspace, level);

        let border = self.layers[LayerType::Heightmaps].texture_border_size as usize;
        let resolution = self.layers[LayerType::Heightmaps].texture_resolution as usize;
        let x = (x * (resolution - 2 * border - 1) as f32) + border as f32;
        let y = (y * (resolution - 2 * border - 1) as f32) + border as f32;

        let w00 = (1.0 - x.fract()) * (1.0 - y.fract());
        let w10 = x.fract() * (1.0 - y.fract());
        let w01 = (1.0 - x.fract()) * y.fract();
        let w11 = x.fract() * y.fract();

        let i00 = x.floor() as usize + y.floor() as usize * resolution;
        let i10 = x.ceil() as usize + y.floor() as usize * resolution;
        let i01 = x.floor() as usize + y.ceil() as usize * resolution;
        let i11 = x.ceil() as usize + y.ceil() as usize * resolution;

        self.levels.0[node.level() as usize]
            .entry(&node)
            .and_then(|entry| Some(entry.heightmap.as_ref()?))
            .map(|h| match h {
                CpuHeightmap::I16 { heights: h, .. } => {
                    (h[i00] as f32 * w00
                        + h[i10] as f32 * w10
                        + h[i01] as f32 * w01
                        + h[i11] as f32 * w11)
                        .max(0.0)
                        * 0.25
                }
                CpuHeightmap::F32 { heights: h, .. } => {
                    (h[i00] * w00 + h[i10] * w10 + h[i01] * w01 + h[i11] * w11).max(0.0)
                }
            })
    }

    /// Returns a conservative estimate of the minimum and maximum heights in the given node.
    pub fn get_height_range(&self, node: VNode) -> (f32, f32) {
        let mut node = Some(node);
        while let Some(n) = node {
            if let Some(CpuHeightmap::I16 { min, max, .. } | CpuHeightmap::F32 { min, max, .. }) =
                self.levels.0[n.level() as usize]
                    .entry(&n)
                    .and_then(|entry| Some(entry.heightmap.as_ref()?))
            {
                return (min.min(0.0), *max + 6000.0);
            }
            node = n.parent().map(|p| p.0);
        }
        (0.0, 9000.0)
    }
}
