//! The `OctreeSet` type is a memory-efficient set of points.
//!
//! The typical workflow for using an `Octree` is to construct it from an `Array3`, then insert it
//! into an `OctreeDBVT` in order to perform spatial queries like raycasting.

use crate::{access::GetUncheckedRelease, prelude::*, IsEmpty};

use building_blocks_core::prelude::*;

use fnv::FnvHashMap;

/// A sparse set of voxel coordinates (3D integer points). Supports spatial queries.
///
/// The octree is a cube shape and the edge lengths can only be a power of 2, at most 64. When an
/// entire octant is full, it will be stored in a collapsed representation, so the leaves of the
/// tree can be differently sized octants.
pub struct OctreeSet {
    extent: Extent3i,
    power: u8,
    root_exists: bool,
    // Save memory by using 2-byte location codes as hash map keys instead of 64-bit node pointers.
    // The total memory usage can be approximated as 4 bytes per node, assuming the hashbrown table
    // has 1 byte of overhead per entry.
    nodes: FnvHashMap<LocationCode, ChildBitMask>,
}

impl OctreeSet {
    // TODO: from_height_map

    /// Constructs an `Octree` which contains all of the points in `extent` which are not empty (as
    /// defined by the `IsEmpty` trait). `extent` must be cube-shaped with edge length being a power
    /// of 2. For exponent E where edge length is 2^E, we must have `0 < E <= 6`, because there is a
    /// maximum fixed depth of the octree.
    pub fn from_array3<A, T>(array: &A, extent: Extent3i) -> Self
    where
        A: Array<[i32; 3]> + GetUncheckedRelease<Stride, T>,
        T: Clone + IsEmpty,
    {
        assert!(extent.shape.dimensions_are_powers_of_2());
        assert!(extent.shape.is_cube());
        let power = extent.shape.x().trailing_zeros() as u8;
        // Constrained by 16-bit location code.
        assert!(power > 0 && power <= 6);

        let edge_len = 1 << power;

        // These are the corners of the root octant, in local coordinates.
        let corner_offsets: Vec<_> = Point3i::corner_offsets()
            .into_iter()
            .map(|p| Local(p * edge_len))
            .collect();
        // Convert into strides for indexing efficiency.
        let mut corner_strides = [Stride(0); 8];
        array.strides_from_local_points(&corner_offsets, &mut corner_strides);

        let mut nodes = FnvHashMap::default();
        let min_local = Local(extent.minimum - array.extent().minimum);
        let root_minimum = array.stride_from_local_point(&min_local);
        let root_location = LocationCode(1);
        let (root_exists, _full) = Self::partition_array(
            root_location,
            root_minimum,
            edge_len,
            &corner_strides,
            array,
            &mut nodes,
        );

        Self {
            power,
            root_exists,
            extent,
            nodes,
        }
    }

    fn partition_array<A, T>(
        location: LocationCode,
        minimum: Stride,
        edge_len: i32,
        corner_strides: &[Stride],
        array: &A,
        nodes: &mut FnvHashMap<LocationCode, ChildBitMask>,
    ) -> (bool, bool)
    where
        A: Array<[i32; 3]> + GetUncheckedRelease<Stride, T>,
        T: Clone + IsEmpty,
    {
        // Base case where the octant is a single voxel.
        if edge_len == 1 {
            let exists = !array.get_unchecked_release(minimum).is_empty();
            return (exists, exists);
        }

        let mut octant_corner_strides = [Stride(0); 8];
        for (child_corner, parent_corner) in
            octant_corner_strides.iter_mut().zip(corner_strides.iter())
        {
            *child_corner = Stride(parent_corner.0 >> 1);
        }

        let half_edge_len = edge_len >> 1;
        let mut child_bitmask = 0;
        let mut all_children_full = true;
        let extended_location = location.extend();
        for (octant, offset) in octant_corner_strides.iter().enumerate() {
            let octant_min = minimum + *offset;
            let octant_location = extended_location.with_lowest_octant(octant as u16);
            let (child_exists, child_full) = Self::partition_array(
                octant_location,
                octant_min,
                half_edge_len,
                &octant_corner_strides,
                array,
                nodes,
            );
            child_bitmask |= (child_exists as u8) << octant;
            all_children_full = all_children_full && child_full;
        }

        let exists = child_bitmask != 0;

        if exists && !all_children_full {
            nodes.insert(location, child_bitmask);
        }

        (exists, all_children_full)
    }

    pub fn edge_length(&self) -> i32 {
        1 << self.power
    }

    /// The entire octant spanned by the octree.
    pub fn octant(&self) -> Octant {
        Octant {
            minimum: self.extent.minimum,
            edge_length: self.edge_length(),
        }
    }

    /// The extent spanned by the octree.
    pub fn extent(&self) -> &Extent3i {
        &self.extent
    }

    /// Returns `true` iff the octree contains zero points.
    pub fn is_empty(&self) -> bool {
        !self.root_exists
    }

    /// Visit every non-empty octant of the octree.
    pub fn visit(&self, visitor: &mut impl OctreeVisitor) -> VisitStatus {
        if !self.root_exists {
            return VisitStatus::Continue;
        }

        let minimum = self.extent.minimum;
        let edge_len = self.edge_length();
        let corner_offsets: Vec<_> = Point3i::corner_offsets()
            .into_iter()
            .map(|p| p * edge_len)
            .collect();

        self._visit(LocationCode(1), minimum, edge_len, &corner_offsets, visitor)
    }

    /// Same as `visit`, but visit only the octants that overlap `extent`.
    pub fn visit_extent(&self, extent: &Extent3i, visitor: &mut impl OctreeVisitor) -> VisitStatus {
        self.visit(&mut |octant: Octant, is_leaf: bool| {
            if Extent3i::from(octant).intersection(extent).is_empty() {
                return VisitStatus::Stop;
            }

            visitor.visit_octant(octant, is_leaf)
        })
    }

    fn _visit(
        &self,
        location: LocationCode,
        minimum: Point3i,
        edge_length: i32,
        corner_offsets: &[Point3i],
        visitor: &mut impl OctreeVisitor,
    ) -> VisitStatus {
        // Precondition: location exists.

        let octant = Octant {
            minimum,
            edge_length,
        };

        // Base case where the octant is a single leaf voxel.
        if edge_length == 1 {
            return visitor.visit_octant(octant, true);
        }

        // Continue traversal of this branch.

        let child_bitmask = if let Some(child_bitmask) = self.nodes.get(&location) {
            child_bitmask
        } else {
            // Since we know that location exists, but it's not in the nodes map, this means that we
            // can assume the entire octant is full. This is an implicit leaf node.
            return visitor.visit_octant(octant, true);
        };

        // Definitely not at a leaf node.
        let status = visitor.visit_octant(octant, false);
        if status != VisitStatus::Continue {
            return status;
        }

        let mut octant_corner_offsets = [PointN([0; 3]); 8];
        for (child_corner, parent_corner) in
            octant_corner_offsets.iter_mut().zip(corner_offsets.iter())
        {
            *child_corner = parent_corner.scalar_right_shift(1);
        }

        let half_edge_length = edge_length >> 1;
        let extended_location = location.extend();
        for (octant, offset) in octant_corner_offsets.iter().enumerate() {
            if (child_bitmask & (1 << octant)) == 0 {
                // This child does not exist.
                continue;
            }

            let octant_min = minimum + *offset;
            let octant_location = extended_location.with_lowest_octant(octant as u16);
            if self._visit(
                octant_location,
                octant_min,
                half_edge_length,
                &octant_corner_offsets,
                visitor,
            ) == VisitStatus::ExitEarly
            {
                return VisitStatus::ExitEarly;
            }
        }

        // Continue with the rest of the tree.
        VisitStatus::Continue
    }

    /// The `OctreeNode` of the root, if it exists.
    pub fn root_node(&self) -> Option<OctreeNode> {
        if self.root_exists {
            Some(OctreeNode {
                location: LocationCode(1),
                octant: self.octant(),
                child_bitmask: self.nodes.get(&LocationCode(1)).cloned().unwrap_or(0),
                level: self.power,
            })
        } else {
            None
        }
    }

    /// Returns the child `OctreeNode` of `parent` at the given `child_octant`. `offset_table` is a
    /// constant that can be constructed by `self.offset_table()` and reused with any octree of the
    /// same size, indefinitely.
    pub fn get_child(
        &self,
        offset_table: &OffsetTable,
        parent: &OctreeNode,
        child_octant: u8,
    ) -> Option<OctreeNode> {
        debug_assert!(child_octant < 8);

        if parent.child_bitmask & (1 << child_octant) == 0 {
            return None;
        }

        let child_level = parent.level - 1;

        let child_location = parent
            .location
            .extend()
            .with_lowest_octant(child_octant as u16);

        let child_octant = Octant {
            minimum: parent.octant.minimum
                + offset_table.get_octant_offset(child_level, child_octant),
            edge_length: parent.octant.edge_length >> 1,
        };

        let mut child_node = OctreeNode {
            location: child_location,
            octant: child_octant,
            child_bitmask: 0,
            level: child_level,
        };

        if let Some(bitmask) = self.nodes.get(&child_node.location) {
            child_node.child_bitmask = *bitmask;
        }

        Some(child_node)
    }

    /// Returns the `OffsetTable` for this octree's shape.
    pub fn offset_table(&self) -> OffsetTable {
        OffsetTable::for_power(self.power)
    }
}

/// A cache of offset values used for calculating octant minimums. These offsets never change for a
/// given octree shape.
pub struct OffsetTable {
    levels: Vec<OctantOffsets>,
}

impl OffsetTable {
    fn for_power(power: u8) -> Self {
        Self {
            levels: (0..power)
                .map(|pow| OctantOffsets::with_edge_length(1 << pow))
                .collect(),
        }
    }

    fn get_octant_offset(&self, level: u8, octant: u8) -> Point3i {
        self.levels[level as usize].get_octant_offset(octant)
    }
}

#[derive(Clone, Copy)]
struct OctantOffsets {
    offsets: [Point3i; 8],
}

impl OctantOffsets {
    fn with_edge_length(edge_length: i32) -> Self {
        let mut offsets = [PointN([0; 3]); 8];
        for (dst, src) in offsets
            .iter_mut()
            .zip(Point3i::corner_offsets().into_iter())
        {
            *dst = src * edge_length;
        }

        OctantOffsets { offsets }
    }

    fn get_octant_offset(&self, octant: u8) -> Point3i {
        self.offsets[octant as usize]
    }
}

pub struct OctreeNode {
    location: LocationCode,
    octant: Octant,
    child_bitmask: ChildBitMask,
    level: u8,
}

impl OctreeNode {
    pub fn is_leaf(&self) -> bool {
        self.child_bitmask == 0
    }

    pub fn octant(&self) -> Octant {
        self.octant
    }
}

type ChildBitMask = u8;

/// Uniquely identifies a location in a given octree.
///
/// Supports an octree with at most 6 levels.
/// ```text
/// level N:
///   loc = 0b1
/// level N-1:
///   loc = 0b1000, 0b1001, 0b1010, 0b1011, 0b1100, 0b1101, 0b1110, 0b1111
/// level N-2:
///   loc = 0b1000000, ...
/// ...
/// level N-5:
///   loc = 0b1000000000000000, ...
/// ```
#[derive(Clone, Copy, Debug, Hash, Eq, PartialEq)]
struct LocationCode(u16);

impl LocationCode {
    fn extend(self) -> Self {
        LocationCode(self.0 << 3)
    }

    fn with_lowest_octant(self, octant: u16) -> Self {
        LocationCode(self.0 | octant)
    }
}

/// A cube-shaped extent which is an octant at some level of an octree. As a leaf node, it
/// represents a totally full set of points.
#[derive(Clone, Copy)]
pub struct Octant {
    minimum: Point3i,
    edge_length: i32,
}

impl Octant {
    pub fn minimum(&self) -> Point3i {
        self.minimum
    }

    pub fn edge_length(&self) -> i32 {
        self.edge_length
    }
}

impl From<Octant> for Extent3i {
    fn from(octant: Octant) -> Self {
        Extent3i::from_min_and_shape(octant.minimum, PointN([octant.edge_length; 3]))
    }
}

pub trait OctreeVisitor {
    /// Visit any octant that contains points in the octree.
    fn visit_octant(&mut self, octant: Octant, is_leaf: bool) -> VisitStatus;
}

impl<F> OctreeVisitor for F
where
    F: FnMut(Octant, bool) -> VisitStatus,
{
    fn visit_octant(&mut self, octant: Octant, is_leaf: bool) -> VisitStatus {
        (self)(octant, is_leaf)
    }
}

#[derive(Eq, PartialEq)]
pub enum VisitStatus {
    /// Continue traversing this branch.
    Continue,
    /// Stop traversing this branch.
    Stop,
    /// Stop traversing the entire tree. No further nodes will be visited.
    ExitEarly,
}

// ████████╗███████╗███████╗████████╗
// ╚══██╔══╝██╔════╝██╔════╝╚══██╔══╝
//    ██║   █████╗  ███████╗   ██║
//    ██║   ██╔══╝  ╚════██║   ██║
//    ██║   ███████╗███████║   ██║
//    ╚═╝   ╚══════╝╚══════╝   ╚═╝

#[cfg(test)]
mod tests {
    use super::*;

    use rand::Rng;
    use std::collections::HashSet;

    #[test]
    fn octants_occupied_iff_not_empty() {
        let voxels = random_voxels();
        let octree = OctreeSet::from_array3(&voxels, *voxels.extent());

        let mut non_empty_voxels = HashSet::new();

        voxels.for_each(voxels.extent(), |p: Point3i, v: Voxel| {
            if !v.is_empty() {
                non_empty_voxels.insert(p);
            }
        });

        let mut octant_voxels = HashSet::new();

        octree.visit(&mut |octant: Octant, is_leaf: bool| {
            if is_leaf {
                for p in Extent3i::from(octant).iter_points() {
                    octant_voxels.insert(p);
                }
            }

            VisitStatus::Continue
        });

        assert_eq!(non_empty_voxels, octant_voxels);

        // Now do the same test with a manual node traversal.
        let mut octant_voxels = HashSet::new();

        let offset_table = octree.offset_table();
        let mut queue = vec![octree.root_node()];
        loop {
            if let Some(node) = queue.pop() {
                if let Some(node) = node {
                    if node.is_leaf() {
                        for p in Extent3i::from(node.octant()).iter_points() {
                            octant_voxels.insert(p);
                        }
                    }
                    for octant in 0..8 {
                        queue.push(octree.get_child(&offset_table, &node, octant));
                    }
                }
            } else {
                break;
            }
        }

        assert_eq!(non_empty_voxels, octant_voxels);
    }

    fn random_voxels() -> Array3<Voxel> {
        let mut rng = rand::thread_rng();
        let extent = Extent3i::from_min_and_shape(PointN([0; 3]), PointN([16; 3]));

        Array3::fill_with(extent, |_| Voxel(rng.gen()))
    }

    #[derive(Clone)]
    struct Voxel(bool);

    impl IsEmpty for Voxel {
        fn is_empty(&self) -> bool {
            !self.0
        }
    }
}
