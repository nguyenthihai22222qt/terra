use memmap::MmapViewSync;
use vec_map::VecMap;

use terrain::quadtree::{Node, NodeId};

#[derive(Clone, Copy, PartialEq, PartialOrd, Serialize, Deserialize)]
pub struct Priority(f32);
impl Priority {
    pub fn cutoff() -> Self {
        Priority(1.0)
    }
    pub fn none() -> Self {
        Priority(-1.0)
    }
    pub fn from_f32(value: f32) -> Self {
        assert!(value.is_finite());
        Priority(value)
    }
}
impl Eq for Priority {}
impl Ord for Priority {
    fn cmp(&self, other: &Self) -> ::std::cmp::Ordering {
        self.partial_cmp(other).unwrap()
    }
}

pub const HEIGHTS_LAYER: usize = 0;
#[allow(unused)]
pub const NORMALS_LAYER: usize = 1;
#[allow(unused)]
pub const SPLATS_LAYER: usize = 2;
pub const NUM_LAYERS: usize = 3;

#[derive(Clone, Default, Serialize, Deserialize)]
pub(crate) struct LayerParams {
    /// Byte offset from start of file.
    pub offset: usize,
    /// Number of tiles in layer.
    pub tile_count: usize,
    /// Number of samples in each dimension, per tile.
    pub tile_resolution: u32,
    /// Number of bytes in each sample.
    pub sample_bytes: usize,
    /// Number of bytes per tile
    pub tile_bytes: usize,
}

#[derive(Serialize, Deserialize)]
pub(crate) struct TileHeader {
    pub layers: [LayerParams; NUM_LAYERS],
    pub nodes: Vec<Node>,
}

pub(crate) struct TileCache {
    /// Maximum number of slots in this `TileCache`.
    size: usize,
    /// Actually contents of the cache.
    slots: Vec<(Priority, NodeId)>,
    /// Which index each node is at in the cache (if any).
    reverse: VecMap<usize>,
    /// Nodes that should be added to the cache.
    missing: Vec<(Priority, NodeId)>,
    /// Smallest priority among all nodes in the cache.
    min_priority: Priority,

    /// Resolution of each tile in this cache.
    layer_params: LayerParams,

    /// Section of memory map that holds the data for this layer.
    data_file: MmapViewSync,
}
impl TileCache {
    pub fn new(cache_size: usize, params: LayerParams, data_file: MmapViewSync) -> Self {
        Self {
            size: cache_size,
            slots: Vec::new(),
            reverse: VecMap::new(),
            missing: Vec::new(),
            min_priority: Priority::none(),
            layer_params: params,
            data_file,
        }
    }

    pub fn update_priorities(&mut self, nodes: &mut Vec<Node>) {
        for &mut (ref mut priority, id) in self.slots.iter_mut() {
            *priority = nodes[id].priority();
        }

        self.min_priority = self.slots.iter().map(|s| s.0).min().unwrap_or(
            Priority::none(),
        );
    }

    pub fn add_missing(&mut self, element: (Priority, NodeId)) {
        if element.0 > self.min_priority || self.slots.len() < self.size {
            self.missing.push(element);
        }
    }

    pub fn load_missing(&mut self, nodes: &mut Vec<Node>) {
        if self.slots.len() + self.missing.len() < self.size {
            while let Some(m) = self.missing.pop() {
                let index = self.slots.len();
                self.load(m.1, &mut nodes[m.1], index);
            }
        } else {
            let mut possible: Vec<_> = self.slots
                .iter()
                .cloned()
                .chain(self.missing.iter().cloned())
                .collect();
            possible.sort();

            // Anything >= to cutoff should be included.
            let cutoff = possible[possible.len() - self.size];

            let mut index = 0;
            while let Some(m) = self.missing.pop() {
                if cutoff >= m {
                    continue;
                }

                // Find the next element to evict.
                while self.slots[index] >= cutoff {
                    index += 1;
                }

                self.load(m.1, &mut nodes[m.1], index);
                index += 1;
            }
        }
    }

    fn load(&mut self, id: NodeId, _node: &mut Node, slot: usize) {
        if slot < self.slots.len() {
            self.reverse.remove(self.slots[slot].1.index());
        }
        self.reverse.insert(id.index(), slot);
        unimplemented!()
    }

    pub fn contains(&self, id: NodeId) -> bool {
        self.reverse.contains_key(id.index())
    }

    #[allow(unused)]
    pub fn resolution(&self) -> u32 {
        self.layer_params.tile_resolution
    }
}
