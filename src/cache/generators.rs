use std::{
    borrow::Cow,
    collections::HashMap,
    mem,
    num::{NonZeroU32, NonZeroU64},
};

use super::{LayerMask, LayerParams, LayerType, MeshCache};
use crate::{
    cache::{mesh::MeshGenerateUniforms, MeshType, TileCache},
    gpu_state::{DrawIndexedIndirect, GpuState},
};
use maplit::hashmap;
use rshader::{ShaderSet, ShaderSource};
use types::VNode;
use vec_map::VecMap;
use wgpu::util::DeviceExt;

pub(crate) trait GenerateTile: Send {
    /// Layers generated by this object. Zero means generate cannot operate for nodes of this level.
    fn outputs(&self) -> LayerMask;
    /// Layers required to be present at `level` when generating a tile at `level`.
    fn peer_inputs(&self) -> LayerMask;
    /// Layers required to be present at `level-1` when generating a tile at `level`.
    fn parent_inputs(&self) -> LayerMask;
    /// Layers that must be present at `level` or the maximum level of the layer (whichever is smaller).
    fn ancestor_inputs(&self) -> LayerMask;
    /// Returns whether previously generated tiles from this generator are still valid.
    fn needs_refresh(&mut self) -> bool;
    /// Run the generator for `node`.
    fn generate(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        state: &GpuState,
        layers: &VecMap<LayerParams>,
        slot: usize,
        parent_slot: Option<usize>,
        uniform_data: &mut Vec<u8>,
    );
}

struct MeshGen {
    shaders: Vec<ShaderSet>,
    dimensions: Vec<(u32, u32, u32)>,
    bindgroup_pipeline: Vec<Option<(wgpu::BindGroup, wgpu::ComputePipeline)>>,
    peer_inputs: LayerMask,
    ancestor_inputs: LayerMask,
    outputs: LayerMask,
    name: String,

    min_level: u8,
    base_entry: u32,
    entries_per_node: u32,

    clear_indirect_buffer: wgpu::Buffer,
}
impl GenerateTile for MeshGen {
    fn outputs(&self) -> LayerMask {
        self.outputs
    }
    fn peer_inputs(&self) -> LayerMask {
        self.peer_inputs
    }
    fn parent_inputs(&self) -> LayerMask {
        LayerMask::empty()
    }
    fn ancestor_inputs(&self) -> LayerMask {
        self.ancestor_inputs
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
        slot: usize,
        _parent_slot: Option<usize>,
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
                    module: &device.create_shader_module(wgpu::ShaderModuleDescriptor {
                        label: Some(&format!("shader.generate.{}", self.name)),
                        source: self.shaders[i].compute(),
                    }),
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
            cpass.dispatch_workgroups(self.dimensions[i].0, self.dimensions[i].1, self.dimensions[i].2);
        }
    }
}

struct ShaderGen {
    shader: ShaderSet,
    bind_group: Option<wgpu::BindGroup>,
    pipeline: Option<wgpu::ComputePipeline>,
    dimensions: u32,
    peer_inputs: LayerMask,
    parent_inputs: LayerMask,
    ancestor_inputs: LayerMask,
    outputs: LayerMask,
    name: String,
}
impl GenerateTile for ShaderGen {
    fn outputs(&self) -> LayerMask {
        self.outputs
    }
    fn peer_inputs(&self) -> LayerMask {
        self.peer_inputs
    }
    fn parent_inputs(&self) -> LayerMask {
        self.parent_inputs
    }
    fn ancestor_inputs(&self) -> LayerMask {
        self.ancestor_inputs
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
        slot: usize,
        parent_slot: Option<usize>,
        uniform_data: &mut Vec<u8>,
    ) {
        let uniform_offset = uniform_data.len();
        uniform_data.extend_from_slice(bytemuck::bytes_of(&(slot as u32)));
        uniform_data.resize(uniform_offset + 256, 0);

        let views_needed = self.outputs() & self.parent_inputs();
        let mut image_views: HashMap<Cow<str>, _> = HashMap::new();
        if let Some(parent_slot) = parent_slot {
            for layer in layers.values().filter(|l| views_needed.contains_layer(l.layer_type)) {
                // TODO: handle subsequent images of a layer.
                image_views.insert(
                    format!("{}_in", layer.layer_type.name()).into(),
                    state.tile_cache[layer.layer_type][0].0.create_view(
                        &wgpu::TextureViewDescriptor {
                            label: Some(&format!(
                                "view.{}[{}]",
                                layer.layer_type.name(),
                                parent_slot
                            )),
                            base_array_layer: parent_slot as u32,
                            array_layer_count: Some(NonZeroU32::new(1).unwrap()),
                            dimension: Some(wgpu::TextureViewDimension::D2),
                            ..Default::default()
                        },
                    ),
                );
            }
        }
        for layer in layers.values().filter(|l| views_needed.contains_layer(l.layer_type)) {
            // TODO: handle subsequent images of a layer.
            image_views.insert(
                format!("{}_out", layer.layer_type.name()).into(),
                state.tile_cache[layer.layer_type][0].0.create_view(&wgpu::TextureViewDescriptor {
                    label: Some(&format!("view.{}[{}]", layer.layer_type.name(), slot)),
                    base_array_layer: slot as u32,
                    array_layer_count: Some(NonZeroU32::new(1).unwrap()),
                    dimension: Some(wgpu::TextureViewDimension::D2),
                    ..Default::default()
                }),
            );
        }

        let workgroup_size = self.shader.workgroup_size();

        if self.bind_group.is_some() && self.pipeline.is_some() {
            let mut cpass =
                encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
            cpass.set_pipeline(self.pipeline.as_ref().unwrap());
            cpass.set_bind_group(0, self.bind_group.as_ref().unwrap(), &[uniform_offset as u32]);
            cpass.dispatch_workgroups(
                (self.dimensions + workgroup_size[0] - 1) / workgroup_size[0],
                (self.dimensions + workgroup_size[1] - 1) / workgroup_size[1],
                1,
            );
        } else {
            let (bind_group, bind_group_layout) = state.bind_group_for_shader(
                device,
                &self.shader,
                hashmap!["ubo".into() => (true, wgpu::BindingResource::Buffer(wgpu::BufferBinding {
                    buffer: &state.generate_uniforms,
                    offset: 0,
                    size: Some(NonZeroU64::new(4).unwrap()),
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
                        module: &device.create_shader_module(wgpu::ShaderModuleDescriptor {
                            label: Some(&format!("shader.generate.{}", self.name)),
                            source: self.shader.compute().into(),
                        }),
                        entry_point: "main",
                        label: Some(&format!("pipeline.generate.{}", self.name)),
                    }));
            }

            {
                let mut cpass =
                    encoder.begin_compute_pass(&wgpu::ComputePassDescriptor { label: None });
                cpass.set_pipeline(&self.pipeline.as_ref().unwrap());
                cpass.set_bind_group(0, &bind_group, &[uniform_offset as u32]);
                cpass.dispatch_workgroups(
                    (self.dimensions + workgroup_size[0] - 1) / workgroup_size[0],
                    (self.dimensions + workgroup_size[1] - 1) / workgroup_size[1],
                    1,
                );
            }

            if image_views.is_empty() {
                self.bind_group = Some(bind_group);
            }
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
    ancestor_dependencies: LayerMask,
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
            ancestor_dependencies: LayerMask::empty(),
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
    fn peer_inputs(mut self, peer_inputs: LayerMask) -> Self {
        self.peer_inputs = peer_inputs;
        self
    }
    fn parent_inputs(mut self, parent_inputs: LayerMask) -> Self {
        self.parent_inputs = parent_inputs;
        self
    }
    fn ancestor_inputs(mut self, ancestor_dependencies: LayerMask) -> Self {
        self.ancestor_dependencies = ancestor_dependencies;
        self
    }
    fn build(self) -> Box<dyn GenerateTile> {
        Box::new(ShaderGen {
            name: self.name,
            shader: ShaderSet::compute_only(self.shader).unwrap(),
            bind_group: None,
            pipeline: None,
            outputs: self.outputs,
            peer_inputs: self.peer_inputs,
            parent_inputs: self.parent_inputs,
            dimensions: self.dimensions,
            ancestor_inputs: self.ancestor_dependencies,
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
    let displacements_resolution = layers[LayerType::Displacements].texture_resolution;
    let normals_resolution = layers[LayerType::Normals].texture_resolution;
    let grass_canopy_resolution = layers[LayerType::GrassCanopy].texture_resolution;
    let tree_attributes_resolution = layers[LayerType::GrassCanopy].texture_resolution;

    vec![
        ShaderGenBuilder::new(
            "heightmaps".into(),
            rshader::shader_source!("../shaders", "gen-heightmaps.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::Heightmaps.bit_mask())
        .dimensions(heightmaps_resolution)
        .parent_inputs(LayerType::Heightmaps.bit_mask())
        .build(),
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
        .dimensions(displacements_resolution)
        .peer_inputs(LayerType::Heightmaps.bit_mask())
        .build(),
        ShaderGenBuilder::new(
            "tree-attributes".into(),
            rshader::shader_source!("../shaders", "gen-tree-attributes.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::TreeAttributes.bit_mask())
        .dimensions(tree_attributes_resolution)
        .ancestor_inputs(LayerType::TreeCover.bit_mask())
        .build(),
        ShaderGenBuilder::new(
            "materials".into(),
            rshader::shader_source!("../shaders", "gen-materials.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::Normals.bit_mask() | LayerType::AlbedoRoughness.bit_mask())
        .dimensions(normals_resolution)
        .ancestor_inputs(LayerType::BaseAlbedo.bit_mask() | LayerType::TreeCover.bit_mask() | LayerType::TreeAttributes.bit_mask())
        .peer_inputs(LayerType::Heightmaps.bit_mask())
        .build(),
        ShaderGenBuilder::new(
            "grass-canopy".into(),
            rshader::shader_source!("../shaders", "gen-grass-canopy.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::GrassCanopy.bit_mask())
        .dimensions(grass_canopy_resolution)
        .peer_inputs(LayerType::Normals.bit_mask())
        .build(),
        ShaderGenBuilder::new(
            "bent-normals".into(),
            rshader::shader_source!("../shaders", "gen-bent-normals.comp", "declarations.glsl", "hash.glsl"),
        )
        .outputs(LayerType::BentNormals.bit_mask())
        .dimensions(513)
        .peer_inputs(LayerType::Heightmaps.bit_mask())
        .build(),
        Box::new(MeshGen {
            shaders: vec![
                // ShaderSet::compute_only(rshader::shader_source!(
                //     "../shaders",
                //     "gen-grass.comp",
                //     "declarations.glsl",
                //     "hash.glsl"
                // )).unwrap(),
                ShaderSet::compute_only(rshader::wgsl_source!(
                    "../shaders",
                    "gen-grass.wgsl",
                    "declarations.wgsl"
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
                | LayerType::AlbedoRoughness.bit_mask()
                | LayerType::Normals.bit_mask(),
            ancestor_inputs: LayerType::GrassCanopy.bit_mask(),
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
            ancestor_inputs: LayerMask::empty(),
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
        Box::new(MeshGen {
            shaders: vec![
                ShaderSet::compute_only(rshader::wgsl_source!(
                    "../shaders",
                    "gen-tree-billboards.wgsl",
                    "declarations.wgsl"
                )).unwrap(),
                ShaderSet::compute_only(rshader::shader_source!(
                    "../shaders",
                    "bounding-tree-billboards.comp",
                    "declarations.glsl"
                )).unwrap(),
            ],
            dimensions: vec![(16, 16, 1), (16, 1, 1)],
            bindgroup_pipeline: vec![None, None],
            peer_inputs: LayerType::Displacements.bit_mask(),
            ancestor_inputs: LayerType::TreeAttributes.bit_mask(),
            outputs: MeshType::TreeBillboards.bit_mask(),
            name: "tree-billboards-mesh".to_string(),
            min_level: meshes[MeshType::TreeBillboards].desc.min_level,
            base_entry: meshes[MeshType::TreeBillboards].base_entry as u32,
            entries_per_node: meshes[MeshType::TreeBillboards].desc.entries_per_node as u32,
            clear_indirect_buffer: device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                usage: wgpu::BufferUsages::COPY_SRC,
                label: Some("buffer.tree_billboards.clear_indirect"),
                contents: &vec![0; mem::size_of::<DrawIndexedIndirect>() * 16],
            })
        }),
    ]
}

pub(super) struct DynamicGenerator {
    pub dependency_mask: LayerMask,
    pub min_level: u8,
    pub max_level: u8,

    pub shader: ShaderSet,
    pub resolution: (u32, u32),
    pub bindgroup_pipeline: Option<(wgpu::BindGroup, wgpu::ComputePipeline)>,

    pub name: &'static str,
}

pub(super) fn dynamic_generators() -> Vec<DynamicGenerator> {
    vec![
        DynamicGenerator {
            dependency_mask: LayerMask::empty(),
            min_level: 3,
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
        },
        DynamicGenerator {
            dependency_mask: LayerMask::empty(),
            min_level: 0,
            max_level: 0,
            shader: ShaderSet::compute_only(rshader::shader_source!(
                "../shaders",
                "gen-root-aerial-perspective.comp",
                "declarations.glsl",
                "atmosphere.glsl"
            ))
            .unwrap(),
            resolution: (9, 9),
            bindgroup_pipeline: None,
            name: "root-aerial-perspective",
        },
    ]
}
