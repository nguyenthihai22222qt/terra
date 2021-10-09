use std::{
    borrow::Cow,
    collections::HashMap,
    mem,
    num::{NonZeroU32, NonZeroU64},
};

use super::{LayerMask, LayerParams, LayerType, MeshCache};
use crate::{
    cache::{mesh::MeshGenerateUniforms, MeshType, TileCache},
    generate::{
        GenDisplacementsUniforms, GenHeightmapsUniforms, GenMaterialsUniforms,
        GenNormalsUniforms,
    },
    gpu_state::{DrawIndexedIndirect, GpuState},
    terrain::quadtree::VNode,
};
use bytemuck::Pod;
use cgmath::Vector2;
use maplit::hashmap;
use rshader::{ShaderSet, ShaderSource};
use std::convert::TryFrom;
use vec_map::VecMap;
use wgpu::util::DeviceExt;

pub(crate) trait GenerateTile: Send {
    /// Layers generated by this object. Zero means generate cannot operate for nodes of this level.
    fn outputs(&self, level: u8) -> LayerMask;
    /// Layers required to be present at `level` when generating a tile at `level`.
    fn peer_inputs(&self, level: u8) -> LayerMask;
    /// Layers required to be present at `level-1` when generating a tile at `level`.
    fn parent_inputs(&self, level: u8) -> LayerMask;
    /// Layers that must be present at some
    fn ancestor_dependencies(&self, level: u8) -> LayerMask;
    /// Returns whether previously generated tiles from this generator are still valid.
    fn needs_refresh(&mut self) -> bool;
    /// Run the generator for `node`.
    fn generate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        state: &GpuState,
        layers: &VecMap<LayerParams>,
        node: VNode,
        slot: usize,
        parent_slot: Option<usize>,
        output_mask: LayerMask,
        uniform_data: &mut Vec<u8>,
    );
}

struct MeshGen {
    shaders: Vec<ShaderSet>,
    dimensions: Vec<(u32, u32, u32)>,
    bindgroup_pipeline: Vec<Option<(wgpu::BindGroup, wgpu::ComputePipeline)>>,
    peer_inputs: LayerMask,
    ancestor_dependencies: LayerMask,
    outputs: LayerMask,
    name: String,

    min_level: u8,
    base_entry: u32,
    entries_per_node: u32,

    clear_indirect_buffer: wgpu::Buffer,
}
impl GenerateTile for MeshGen {
    fn outputs(&self, _level: u8) -> LayerMask {
        self.outputs
    }
    fn peer_inputs(&self, _level: u8) -> LayerMask {
        self.peer_inputs
    }
    fn parent_inputs(&self, _level: u8) -> LayerMask {
        LayerMask::empty()
    }
    fn ancestor_dependencies(&self, _level: u8) -> LayerMask {
        self.ancestor_dependencies
    }
    fn needs_refresh(&mut self) -> bool {
        let mut refreshed = false;
        for (i, shader) in self.shaders.iter_mut().enumerate() {
            if shader.refresh() {
                self.bindgroup_pipeline[i] = None;
                refreshed = true;
            }
        }
        refreshed
    }
    fn generate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        gpu_state: &GpuState,
        _layers: &VecMap<LayerParams>,
        _node: VNode,
        slot: usize,
        _parent_slot: Option<usize>,
        _output_mask: LayerMask,
        uniform_data: &mut Vec<u8>,
    ) {
        let entry = (slot - TileCache::base_slot(self.min_level)) as u32 * self.entries_per_node;
        let uniforms = MeshGenerateUniforms {
            slot: slot as u32,
            storage_base_entry: entry,
            mesh_base_entry: self.base_entry + entry,
            entries_per_node: self.entries_per_node,
        };

        assert!(std::mem::size_of::<MeshGenerateUniforms>() <= 256);
        let uniform_offset = uniform_data.len();
        uniform_data.extend_from_slice(bytemuck::bytes_of(&uniforms));
        uniform_data.resize(uniform_offset + 256, 0);

        encoder.copy_buffer_to_buffer(
            &self.clear_indirect_buffer,
            0,
            &gpu_state.mesh_indirect,
            mem::size_of::<DrawIndexedIndirect>() as u64 * (self.base_entry + entry) as u64,
            mem::size_of::<DrawIndexedIndirect>() as u64 * self.entries_per_node as u64,
        );

        for i in 0..self.shaders.len() {
            if self.bindgroup_pipeline[i].is_none() {
                let (bind_group, bind_group_layout) = gpu_state.bind_group_for_shader(
                    device,
                    &self.shaders[i],
                    hashmap!["ubo".into() => (true, wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                        buffer: &gpu_state.generate_uniforms,
                        offset: 0,
                        size: Some(NonZeroU64::new(mem::size_of::<MeshGenerateUniforms>() as u64).unwrap()),
                    }))],
                    HashMap::new(),
                    &format!("generate.{}", self.name),
                );
                let pipeline = device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                    layout: Some(&device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
                        bind_group_layouts: [&bind_group_layout][..].into(),
                        push_constant_ranges: &[],
                        label: None,
                    })),
                    module: &device.create_shader_module(&wgpu::ShaderModuleDescriptor {
                        label: Some(&format!("shader.generate.{}", self.name)),
                        source: self.shaders[i].compute(),
                    }),
                    // module: &unsafe {device.create_shader_module_spirv(&wgpu::ShaderModuleDescriptorSpirV {
                    //     label: Some(&format!("shader.generate.{}", self.name)),
                    //     source: self.shaders[i].compute().into(),
                    // }) },
                    entry_point: "main",
                    label: Some(&format!("pipeline.generate.{}{}", self.name, i)),
                });
                self.bindgroup_pipeline[i] = Some((bind_group, pipeline));
            }
        }

        let mut cpass = encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
        for i in 0..self.shaders.len() {
            cpass.set_pipeline(&self.bindgroup_pipeline[i].as_ref().unwrap().1);
            cpass.set_bind_group(
                0,
                &self.bindgroup_pipeline[i].as_ref().unwrap().0,
                &[uniform_offset as u32],
            );
            cpass.dispatch(self.dimensions[i].0, self.dimensions[i].1, self.dimensions[i].2);
        }
    }
}

struct ShaderGen<T, F: 'static + Send + Fn(VNode, usize, Option<usize>, LayerMask) -> T> {
    shader: ShaderSet,
    bind_group: Option<wgpu::BindGroup>,
    pipeline: Option<wgpu::ComputePipeline>,
    dimensions: u32,
    peer_inputs: LayerMask,
    parent_inputs: LayerMask,
    outputs: LayerMask,
    /// Used instead of outputs for root nodes
    root_outputs: LayerMask,
    /// Used instead of peer_inputs for root nodes
    root_peer_inputs: LayerMask,
    blit_from_bc5_staging: Option<LayerType>,
    name: String,
    f: F,
}
impl<T: Pod, F: 'static + Send + Fn(VNode, usize, Option<usize>, LayerMask) -> T> GenerateTile
    for ShaderGen<T, F>
{
    fn outputs(&self, level: u8) -> LayerMask {
        if level > 0 {
            self.outputs
        } else {
            self.root_outputs
        }
    }
    fn peer_inputs(&self, level: u8) -> LayerMask {
        if level > 0 {
            self.peer_inputs
        } else {
            self.root_peer_inputs
        }
    }
    fn parent_inputs(&self, level: u8) -> LayerMask {
        if level > 0 {
            self.parent_inputs
        } else {
            LayerMask::empty()
        }
    }
    fn ancestor_dependencies(&self, _level: u8) -> LayerMask {
        LayerMask::empty()
    }
    fn needs_refresh(&mut self) -> bool {
        if self.shader.refresh() {
            self.pipeline = None;
            self.bind_group = None;
            true
        } else {
            false
        }
    }
    fn generate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        state: &GpuState,
        layers: &VecMap<LayerParams>,
        node: VNode,
        slot: usize,
        parent_slot: Option<usize>,
        output_mask: LayerMask,
        uniform_data: &mut Vec<u8>,
    ) {
        let uniforms = (self.f)(node, slot, parent_slot, output_mask);

        assert!(std::mem::size_of::<T>() <= 256);
        let uniform_offset = uniform_data.len();
        uniform_data.extend_from_slice(bytemuck::bytes_of(&uniforms));
        uniform_data.resize(uniform_offset + 256, 0);

        let views_needed = self.outputs(node.level()) & self.parent_inputs(node.level());
        let mut image_views: HashMap<Cow<str>, _> = HashMap::new();
        if let Some(parent_slot) = parent_slot {
            for layer in layers.values().filter(|l| views_needed.contains_layer(l.layer_type)) {
                image_views.insert(
                    format!("{}_in", layer.layer_type.name()).into(),
                    state.tile_cache[layer.layer_type].0.create_view(
                        &wgpu::TextureViewDescriptor {
                            label: Some(&format!(
                                "view.{}[{}]",
                                layer.layer_type.name(),
                                parent_slot
                            )),
                            base_array_layer: parent_slot as u32,
                            array_layer_count: Some(NonZeroU32::new(1).unwrap()),
                            ..Default::default()
                        },
                    ),
                );
            }
        }
        for layer in layers.values().filter(|l| views_needed.contains_layer(l.layer_type)) {
            image_views.insert(
                format!("{}_out", layer.layer_type.name()).into(),
                state.tile_cache[layer.layer_type].0.create_view(&wgpu::TextureViewDescriptor {
                    label: Some(&format!("view.{}[{}]", layer.layer_type.name(), slot)),
                    base_array_layer: slot as u32,
                    array_layer_count: Some(NonZeroU32::new(1).unwrap()),
                    ..Default::default()
                }),
            );
        }

        if self.bind_group.is_some() && self.pipeline.is_some() {
            let mut cpass =
                encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
            cpass.set_pipeline(self.pipeline.as_ref().unwrap());
            cpass.set_bind_group(0, self.bind_group.as_ref().unwrap(), &[uniform_offset as u32]);
            cpass.dispatch(self.dimensions, self.dimensions, 1);
        } else {
            let (bind_group, bind_group_layout) = state.bind_group_for_shader(
                device,
                &self.shader,
                hashmap!["ubo".into() => (true, wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &state.generate_uniforms,
                    offset: 0,
                    size: Some(NonZeroU64::new(mem::size_of::<T>() as u64).unwrap()),
                }))],
                image_views.iter().map(|(n, v)| (n.clone(), v)).collect(),
                &format!("generate.{}", self.name),
            );

            if self.pipeline.is_none() {
                self.pipeline =
                    Some(device.create_compute_pipeline(&wgpu::ComputePipelineDescriptor {
                        layout: Some(&device.create_pipeline_layout(
                            &wgpu::PipelineLayoutDescriptor {
                                bind_group_layouts: [&bind_group_layout][..].into(),
                                push_constant_ranges: &[],
                                label: None,
                            },
                        )),
                        // module: &device.create_shader_module(&wgpu::ShaderModuleDescriptor {
                        //     label: Some(&format!("shader.generate.{}", self.name)),
                        //     source: wgpu::ShaderSource::SpirV(self.shader.compute().into()),
                        // }),
                        module: &unsafe {device.create_shader_module_spirv(&wgpu::ShaderModuleDescriptorSpirV {
                            label: Some(&format!("shader.generate.{}", self.name)),
                            source: match self.shader.compute() {
                                wgpu::ShaderSource::SpirV(s) => s,
                                _ => unreachable!(),
                            },
                        }) },
                        entry_point: "main",
                        label: Some(&format!("pipeline.generate.{}", self.name)),
                    }));
            }

            {
                let mut cpass =
                    encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
                cpass.set_pipeline(&self.pipeline.as_ref().unwrap());
                cpass.set_bind_group(0, &bind_group, &[uniform_offset as u32]);
                cpass.dispatch(self.dimensions, self.dimensions, 1);
            }

            if image_views.is_empty() {
                self.bind_group = Some(bind_group);
            }
        }

        if let Some(layer) = self.blit_from_bc5_staging {
            let resolution = layers[layer].texture_resolution;
            let resolution_blocks = (resolution + 3) / 4;
            let row_pitch = (resolution_blocks * 16 + 255) & !255;
            assert!(resolution % 4 == 0);
            encoder.copy_texture_to_buffer(
                wgpu::ImageCopyTexture {
                    texture: &state.bc5_staging.0,
                    mip_level: 0,
                    origin: wgpu::Origin3d::default(),
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::ImageCopyBuffer {
                    buffer: &state.staging_buffer,
                    layout: wgpu::ImageDataLayout {
                        bytes_per_row: Some(NonZeroU32::new(row_pitch).unwrap()),
                        rows_per_image: None,
                        offset: 0,
                    },
                },
                wgpu::Extent3d {
                    width: resolution_blocks,
                    height: resolution_blocks,
                    depth_or_array_layers: 1,
                },
            );
            encoder.copy_buffer_to_texture(
                wgpu::ImageCopyBuffer {
                    buffer: &state.staging_buffer,
                    layout: wgpu::ImageDataLayout {
                        bytes_per_row: Some(NonZeroU32::new(row_pitch).unwrap()),
                        rows_per_image: Some(NonZeroU32::new(resolution).unwrap()),
                        offset: 0,
                    },
                },
                wgpu::ImageCopyTexture {
                    texture: &state.tile_cache[LayerType::Normals].0,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x: 0, y: 0, z: slot as u32 },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::Extent3d { width: resolution, height: resolution, depth_or_array_layers: 1 },
            );
        }
    }
}

struct ShaderGenBuilder {
    name: String,
    dimensions: u32,
    shader: ShaderSource,
    peer_inputs: LayerMask,
    parent_inputs: LayerMask,
    outputs: LayerMask,
    root_outputs: Option<LayerMask>,
    root_peer_inputs: Option<LayerMask>,
    blit_from_bc5_staging: Option<LayerType>,
}
impl ShaderGenBuilder {
    fn new(name: String, shader: ShaderSource) -> Self {
        Self {
            name,
            dimensions: 0,
            outputs: LayerMask::empty(),
            shader,
            peer_inputs: LayerMask::empty(),
            parent_inputs: LayerMask::empty(),
            root_outputs: None,
            root_peer_inputs: None,
            blit_from_bc5_staging: None,
        }
    }
    fn dimensions(mut self, dimensions: u32) -> Self {
        self.dimensions = dimensions;
        self
    }
    fn outputs(mut self, outputs: LayerMask) -> Self {
        self.outputs = outputs;
        self
    }
    fn root_outputs(mut self, root_outputs: LayerMask) -> Self {
        self.root_outputs = Some(root_outputs);
        self
    }
    fn root_peer_inputs(mut self, root_peer_inputs: LayerMask) -> Self {
        self.root_peer_inputs = Some(root_peer_inputs);
        self
    }
    fn peer_inputs(mut self, peer_inputs: LayerMask) -> Self {
        self.peer_inputs = peer_inputs;
        self
    }
    fn parent_inputs(mut self, parent_inputs: LayerMask) -> Self {
        self.parent_inputs = parent_inputs;
        self
    }
    fn blit_from_bc5_staging(mut self, layer: LayerType) -> Self {
        self.blit_from_bc5_staging = Some(layer);
        self
    }
    fn build<T: Pod, F: 'static + Send + Fn(VNode, usize, Option<usize>, LayerMask) -> T>(
        self,
        f: F,
    ) -> Box<dyn GenerateTile> {
        Box::new(ShaderGen {
            name: self.name,
            shader: ShaderSet::compute_only(self.shader).unwrap(),
            bind_group: None,
            pipeline: None,
            outputs: self.outputs,
            peer_inputs: self.peer_inputs,
            parent_inputs: self.parent_inputs,
            dimensions: self.dimensions,
            root_outputs: self.root_outputs.unwrap_or(
                if self.parent_inputs == LayerMask::empty() {
                    self.outputs
                } else {
                    LayerMask::empty()
                },
            ),
            root_peer_inputs: self.root_peer_inputs.unwrap_or(self.peer_inputs),
            blit_from_bc5_staging: self.blit_from_bc5_staging,
            f,
        })
    }
}

pub(crate) fn generators(
    device: &wgpu::Device,
    layers: &VecMap<LayerParams>,
    meshes: &VecMap<MeshCache>,
    soft_float64: bool,
) -> Vec<Box<dyn GenerateTile>> {
    let heightmaps_resolution = layers[LayerType::Heightmaps].texture_resolution;
    let heightmaps_border = layers[LayerType::Heightmaps].texture_border_size;
    let displacements_resolution = layers[LayerType::Displacements].texture_resolution;
    let normals_resolution = layers[LayerType::Normals].texture_resolution;
    let normals_border = layers[LayerType::Normals].texture_border_size;
    let grass_canopy_resolution = layers[LayerType::GrassCanopy].texture_resolution;

    let grass_canopy_base_slot =
        TileCache::base_slot(layers[LayerType::GrassCanopy].min_level) as u32;

    vec![
        ShaderGenBuilder::new(
            "heightmaps".into(),
            rshader::shader_source!("../shaders", "gen-heightmaps.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::Heightmaps.bit_mask())
        .dimensions((heightmaps_resolution + 7) / 8)
        .parent_inputs(LayerType::Heightmaps.bit_mask())
        .build(
            move |node: VNode,
                  slot: usize,
                  parent_slot: Option<usize>,
                  _|
                  -> GenHeightmapsUniforms {
                let (_parent, parent_index) = node.parent().expect("root node missing");
                let parent_offset = crate::terrain::quadtree::node::OFFSETS[parent_index as usize];
                let origin = [
                    heightmaps_border as i32 / 2,
                    heightmaps_resolution as i32 / 2 - heightmaps_border as i32 / 2,
                ];
                let spacing = node.aprox_side_length()
                    / (heightmaps_resolution - heightmaps_border * 2 - 1) as f32;
                let resolution = heightmaps_resolution - heightmaps_border * 2 - 1;
                let level_resolution = resolution << node.level();
                GenHeightmapsUniforms {
                    position: [
                        i32::try_from(node.x() as i64 * resolution as i64
                            - level_resolution as i64 / 2
                            - heightmaps_border as i64).unwrap(),
                        i32::try_from(node.y() as i64 * resolution as i64
                            - level_resolution as i64 / 2
                            - heightmaps_border as i64).unwrap(),
                    ],
                    origin: [origin[parent_offset.x as usize], origin[parent_offset.y as usize]],
                    spacing,
                    in_slot: parent_slot.unwrap() as i32,
                    out_slot: slot as i32,
                    level_resolution: level_resolution as i32,
                    face: node.face() as u32,
                }
            },
        ),
        ShaderGenBuilder::new(
            "displacements".into(),
            if soft_float64 {
                rshader::shader_source!(
                    "../shaders",
                    "gen-displacements.comp",
                    "declarations.glsl",
                    "softdouble.glsl";
                    "SOFT_DOUBLE" = "1"
                )
            } else {
                rshader::shader_source!("../shaders", "gen-displacements.comp", "declarations.glsl"; "SOFT_DOUBLE" = "0")
            },
        )
        .outputs(LayerType::Displacements.bit_mask())
        .root_outputs(LayerType::Displacements.bit_mask())
        .dimensions((displacements_resolution + 7) / 8)
        .parent_inputs(LayerType::Heightmaps.bit_mask())
        .root_peer_inputs(LayerType::Heightmaps.bit_mask())
        .build(
            move |node: VNode,
                  slot: usize,
                  parent_slot: Option<usize>,
                  _|
                  -> GenDisplacementsUniforms {
                let base_stride = (heightmaps_resolution - heightmaps_border * 2 - 1)
                    / (displacements_resolution - 1);
                let (offset, stride) = match parent_slot {
                    Some(_) => (Vector2::new(node.x() & 1, node.y() & 1), base_stride / 2),
                    None => (Vector2::new(0, 0), base_stride),
                };
                let world_center = node.center_wspace();
                let resolution = displacements_resolution - 1;
                let level_resolution = resolution << node.level();
                GenDisplacementsUniforms {
                    node_center: world_center.into(),
                    origin: [
                        (heightmaps_border
                            + (heightmaps_resolution - heightmaps_border * 2 - 1) * offset.x / 2)
                            as i32,
                        (heightmaps_border
                            + (heightmaps_resolution - heightmaps_border * 2 - 1) * offset.y / 2)
                            as i32,
                    ],
                    stride: stride as i32,
                    displacements_slot: slot as i32,
                    heightmaps_slot: parent_slot.unwrap_or(slot) as i32,
                    position: [
                        i32::try_from(node.x() as i64 * resolution as i64 - level_resolution as i64 / 2).unwrap(),
                        i32::try_from(node.y() as i64 * resolution as i64 - level_resolution as i64 / 2).unwrap(),
                    ],
                    face: node.face() as i32,
                    level_resolution,
                    padding0: 0.0,
                }
            },
        ),
        ShaderGenBuilder::new(
            "root-normals".into(),
            rshader::shader_source!("../shaders", "gen-root-normals.comp", "declarations.glsl", "hash.glsl"),
        )
        .root_outputs(LayerType::Normals.bit_mask())
        .dimensions((normals_resolution + 3) / 4)
        .peer_inputs(LayerType::Heightmaps.bit_mask())
        .blit_from_bc5_staging(LayerType::Normals)
        .build(move |node: VNode, slot: usize, _, _| -> GenNormalsUniforms {
            let spacing =
                node.aprox_side_length() / (normals_resolution - normals_border * 2) as f32;

            GenNormalsUniforms {
                heightmaps_origin: [
                    (heightmaps_border - normals_border) as i32,
                    (heightmaps_border - normals_border) as i32,
                ],
                spacing,
                heightmaps_slot: slot as i32,
                normals_slot: slot as i32,
                padding: [0.0; 3],
            }
        }),
        ShaderGenBuilder::new(
            "materials".into(),
            rshader::shader_source!("../shaders", "gen-materials.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::Normals.bit_mask() | LayerType::Albedo.bit_mask())
        .dimensions((normals_resolution + 3) / 4)
        .parent_inputs(LayerType::Albedo.bit_mask())
        .peer_inputs(LayerType::Heightmaps.bit_mask())
        .blit_from_bc5_staging(LayerType::Normals)
        .build(
            move |node: VNode,
                  slot: usize,
                  parent_slot: Option<usize>,
                  output_mask: LayerMask|
                  -> GenMaterialsUniforms {
                let spacing =
                    node.aprox_side_length() / (normals_resolution - normals_border * 2) as f32;

                let albedo_slot =
                    if output_mask.contains_layer(LayerType::Albedo) { slot as i32 } else { -1 };

                let parent_index = node.parent().unwrap().1;

                let resolution = heightmaps_resolution - heightmaps_border * 2 - 1;
                let level_resolution = resolution << node.level();
                GenMaterialsUniforms {
                    position: [
                        i32::try_from(node.x() as i64 * resolution as i64
                            - level_resolution as i64 / 2
                            - heightmaps_border as i64).unwrap(),
                        i32::try_from(node.y() as i64 * resolution as i64
                            - level_resolution as i64 / 2
                            - heightmaps_border as i64).unwrap(),
                    ],
                    level_resolution,
                    heightmaps_origin: [
                        (heightmaps_border - normals_border) as i32,
                        (heightmaps_border - normals_border) as i32,
                    ],
                    spacing,
                    heightmaps_slot: slot as i32,
                    normals_slot: slot as i32,
                    albedo_slot,
                    parent_slot: parent_slot.map(|s| s as i32).unwrap_or(-1),
                    parent_origin: [
                        if parent_index % 2 == 0 {
                            normals_border / 2
                        } else {
                            (normals_resolution - normals_border) / 2
                        },
                        if parent_index / 2 == 0 {
                            normals_border / 2
                        } else {
                            (normals_resolution - normals_border) / 2
                        },
                    ],
                    level: node.level() as u32,
                }
            },
        ),
        ShaderGenBuilder::new(
            "grass-canopy".into(),
            rshader::shader_source!("../shaders", "gen-grass-canopy.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::GrassCanopy.bit_mask())
        .dimensions((grass_canopy_resolution + 7) / 8)
        .peer_inputs(LayerType::Normals.bit_mask())
        .build(move |node: VNode, slot: usize, _, _| -> [u32; 2] {
            assert_eq!(node.level(), VNode::LEVEL_CELL_1M);
            [slot as u32, slot as u32 - grass_canopy_base_slot]
        }),
        Box::new(MeshGen {
            shaders: vec![
                ShaderSet::compute_only(rshader::wgsl_source!(
                    "../shaders",
                    "gen-grass.wgsl"
                )).unwrap(),
                ShaderSet::compute_only(rshader::shader_source!(
                    "../shaders",
                    "bounding-sphere.comp",
                    "declarations.glsl"
                )).unwrap(),
            ],
            dimensions: vec![(16, 16, 1), (16, 1, 1)],
            bindgroup_pipeline: vec![None, None],
            peer_inputs: LayerType::Displacements.bit_mask()
                | LayerType::Albedo.bit_mask()
                | LayerType::Normals.bit_mask(),
            ancestor_dependencies: LayerType::GrassCanopy.bit_mask(),
            outputs: MeshType::Grass.bit_mask(),
            name: "grass-mesh".to_string(),
            min_level: meshes[MeshType::Grass].desc.min_level,
            base_entry: meshes[MeshType::Grass].base_entry as u32,
            entries_per_node: meshes[MeshType::Grass].desc.entries_per_node as u32,
            clear_indirect_buffer: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                usage: wgpu::BufferUsages::COPY_SRC,
                label: Some("buffer.grass.clear_indirect"),
                contents: &vec![0; mem::size_of::<DrawIndexedIndirect>() * 16],
            })
        }),
        Box::new(MeshGen {
            shaders: vec![
                ShaderSet::compute_only(rshader::shader_source!(
                    "../shaders",
                    "gen-terrain-bounding.comp",
                    "declarations.glsl"
                )).unwrap()
            ],
            dimensions: vec![(4, 1, 1)],
            bindgroup_pipeline: vec![None],
            peer_inputs: LayerType::Displacements.bit_mask(),
            ancestor_dependencies: LayerMask::empty(),
            outputs: MeshType::Terrain.bit_mask(),
            name: "terrain-mesh".to_string(),
            min_level: meshes[MeshType::Terrain].desc.min_level,
            base_entry: meshes[MeshType::Terrain].base_entry as u32,
            entries_per_node: meshes[MeshType::Terrain].desc.entries_per_node as u32,
            clear_indirect_buffer: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                usage: wgpu::BufferUsages::COPY_SRC,
                label: Some("buffer.terrain.clear_indirect"),
                contents: bytemuck::cast_slice(&(0..4).map(|i| DrawIndexedIndirect {
                    vertex_count: 32 * 32 * 6,
                    instance_count: 1,
                    vertex_offset: 0,
                    base_instance: 0,
                    base_index: 32 * 32 * 6 * i,
                }).collect::<Vec<_>>()),
            })
        }),
    ]
}

pub(super) struct DynamicGenerator {
    pub dependency_mask: LayerMask,
    pub output: LayerType,
    pub min_level: u8,
    pub max_level: u8,

    pub shader: ShaderSet,
    pub resolution: (u32, u32),
    pub bindgroup_pipeline: Option<(wgpu::BindGroup, wgpu::ComputePipeline)>,

    pub name: &'static str,
}

pub(super) fn dynamic_generators() -> Vec<DynamicGenerator> {
    vec![DynamicGenerator {
        dependency_mask: LayerMask::empty(),
        output: LayerType::AerialPerspective,
        min_level: 0,
        max_level: VNode::LEVEL_SIDE_610M,
        shader: ShaderSet::compute_only(rshader::shader_source!(
            "../shaders",
            "gen-aerial-perspective.comp",
            "declarations.glsl",
            "atmosphere.glsl"
        ))
        .unwrap(),
        resolution: (1, 1),
        bindgroup_pipeline: None,
        name: "aerial-perspective",
    }]
}
