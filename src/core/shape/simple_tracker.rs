use std::collections::HashMap;

use num::BigInt;
use savage_core::expression::Expression;
use tinyvec::ArrayVec;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dim {
    Known(usize),
    Unknown(char),
}

impl Dim {
    pub fn to_usize(self) -> Option<usize> {
        match self {
            Dim::Known(n) => Some(n),
            Dim::Unknown(_) => None,
        }
    }
}

impl Default for Dim {
    fn default() -> Self {
        Self::Unknown('-')
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ShapeTracker {
    pub dims: ArrayVec<[Dim; 10]>,
    pub indexes: ArrayVec<[usize; 10]>,
    pub fake: ArrayVec<[bool; 10]>,
    pub slices: ArrayVec<[(usize, usize); 10]>,
    pub padding: ArrayVec<[(usize, usize); 10]>,
}

impl ShapeTracker {
    pub fn new(dims: &[Dim]) -> Self {
        let mut s = Self {
            dims: Default::default(),
            indexes: Default::default(),
            fake: Default::default(),
            slices: Default::default(),
            padding: Default::default(),
        };
        for (i, d) in dims.iter().enumerate() {
            s.dims.push(*d);
            s.indexes.push(i);
            s.fake.push(false);
            s.slices.push((0, i32::MAX as usize)); // Unset upper bound slices are i32::MAX
            s.padding.push((0, 0));
        }
        s
    }

    /// Create a shape tracker where all dims are fake
    pub fn fake(dims: &[Dim]) -> Self {
        let mut s = Self::new(dims);
        for i in 0..dims.len() {
            s.fake[i] = true;
        }
        s
    }

    /// Add dim along a certian axis
    pub fn expand(&mut self, axis: usize, dim: Dim) {
        self.indexes.insert(axis, self.dims.len());
        self.dims.push(dim);
        self.fake.push(true);
        self.slices.push((0, i32::MAX as usize));
        self.padding.push((0, 0));
    }

    /// Remove a dimension
    pub fn remove_dim(&mut self, axis: usize) {
        let index = self.indexes.remove(axis);
        self.dims.remove(index);
        self.fake.remove(index);
        for i in self.indexes.iter_mut() {
            if *i > index {
                *i -= 1;
            }
        }
    }

    /// Permute the dimensions
    pub fn permute(&mut self, axes: &[usize]) {
        let new_indexes = axes.iter().map(|i| self.indexes[*i]).collect::<Vec<_>>();
        self.indexes.copy_from_slice(&new_indexes);
    }

    /// Strides without permute applied
    fn unordered_strides(&self) -> Vec<usize> {
        let mut strides = self
            .dims
            .iter()
            .enumerate()
            .rev()
            .scan(1, |state, (i, x)| {
                let ret = *state;
                if !self.fake[i] {
                    *state *= x
                        .to_usize()
                        .expect("All dims must be known before indexing!");
                }
                Some(ret)
            })
            .collect::<Vec<_>>();
        strides.reverse();
        strides
    }

    /// Once all dims are known, compute strides
    pub fn strides(&self) -> Vec<usize> {
        let strides = self.unordered_strides();
        self.indexes.into_iter().map(|i| strides[i]).collect()
    }

    /// Create an indexer to convert logical indexes into physical indexes
    pub fn indexer(&self) -> Indexer {
        let strides = self.unordered_strides();
        Indexer {
            data: self
                .indexes
                .into_iter()
                .rev()
                .map(|i| {
                    (
                        self.dims[i].to_usize().unwrap(),
                        strides[i],
                        self.padding[i],
                        self.slices[i],
                        self.fake[i],
                    )
                })
                .collect(),
        }
    }

    pub fn index_expression(&self) -> Expression {
        let mut strides = self
            .dims
            .iter()
            .enumerate()
            .rev()
            .scan(Expression::Integer(BigInt::from(1)), |state, (i, x)| {
                let ret = state.clone();
                if !self.fake[i] {
                    *state *= match x {
                        Dim::Known(n) => Expression::Integer(BigInt::from(*n)),
                        Dim::Unknown(c) => Expression::Variable(c.to_string()),
                    };
                }
                Some(ret)
            })
            .collect::<Vec<_>>();
        strides.reverse();
        let mut ret = Expression::Integer(BigInt::from(0));
        let mut acc = Expression::Integer(BigInt::from(1));
        let logical = Expression::Variable("idx".to_string());
        for (sh, stride, padding, slice, fake) in self.indexes.into_iter().rev().map(|i| {
            (
                match self.dims[i] {
                    Dim::Known(n) => Expression::Integer(BigInt::from(n)),
                    Dim::Unknown(c) => Expression::Variable(c.to_string()),
                },
                strides[i].clone(),
                self.padding[i],
                self.slices[i],
                self.fake[i],
            )
        }) {
            let logical_sh = (sh
                + Expression::Integer(BigInt::from(padding.0))
                + Expression::Integer(BigInt::from(padding.1)))
            .min(Expression::Integer(BigInt::from(slice.1)))
                - Expression::Integer(BigInt::from(slice.0));
            if !fake {
                let dim_ind = (logical.clone() / acc.clone()) % logical_sh.clone();
                ret += (dim_ind - Expression::Integer(BigInt::from(padding.0))
                    + Expression::Integer(BigInt::from(slice.0.saturating_sub(padding.0))))
                    * stride;
            }
            acc *= logical_sh;
        }
        ret
    }

    /// If this expression evaluates to 0, the logical index is invalid. Otherwise it is valid
    pub fn valid_expression(&self) -> Expression {
        let mut ret = Expression::Integer(BigInt::from(1));
        let mut acc = Expression::Integer(BigInt::from(1));
        let logical = Expression::Variable("idx".to_string());
        for (sh, padding, slice, fake) in self.indexes.into_iter().rev().map(|i| {
            (
                match self.dims[i] {
                    Dim::Known(n) => Expression::Integer(BigInt::from(n)),
                    Dim::Unknown(c) => Expression::Variable(c.to_string()),
                },
                self.padding[i],
                self.slices[i],
                self.fake[i],
            )
        }) {
            let logical_sh = (sh.clone()
                + Expression::Integer(BigInt::from(padding.0))
                + Expression::Integer(BigInt::from(padding.1)))
            .min(Expression::Integer(BigInt::from(slice.1)))
                - Expression::Integer(BigInt::from(slice.0));
            if !fake {
                let dim_ind = (logical.clone() / acc.clone()) % logical_sh.clone();
                ret = Expression::And(
                    ret.into(),
                    Expression::GreaterThanOrEqual(
                        dim_ind.clone().into(),
                        Expression::Integer(BigInt::from(padding.0.saturating_sub(slice.0))).into(),
                    )
                    .into(),
                );
                ret = Expression::And(
                    ret.into(),
                    Expression::LessThan(
                        dim_ind.clone().into(),
                        (sh + Expression::Integer(BigInt::from(padding.0)))
                            .min(Expression::Integer(BigInt::from(slice.1)))
                            .into(),
                    )
                    .into(),
                );
            }
            acc *= logical_sh;
        }
        ret
    }

    /// The number of elements in this tensor, including pads and slices. Counts unknown dims as size 0
    pub fn n_elements(&self) -> usize {
        self.indexes
            .into_iter()
            // Filter out unknowns
            .filter_map(|i| {
                if let Dim::Known(n) = self.dims[i] {
                    Some((i, n))
                } else {
                    None
                }
            })
            // Add pads
            .map(|(ind, dim)| (ind, dim + self.padding[ind].0 + self.padding[ind].1))
            // Slice
            .map(|(ind, dim)| dim.min(self.slices[ind].1) - self.slices[ind].0)
            .product()
    }

    /// The number of elements in this tensor, not including pads and slices. Counts unknown dims as size 0
    pub fn n_physical_elements(&self) -> usize {
        self.dims
            .into_iter()
            // Filter out fake dimensions
            .enumerate()
            .filter(|(i, _)| !self.fake[*i])
            .flat_map(|(_, i)| i.to_usize())
            .product()
    }

    /// The number of dimensions
    pub fn len(&self) -> usize {
        self.dims.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn realize(mut self, dims: &[Dim]) -> Self {
        for (i, ind) in self.indexes.iter().enumerate() {
            self.dims[*ind] = dims[i];
        }
        self
    }

    /// Create a contiguous version
    pub fn contiguous(self) -> Self {
        let new_dims = self
            .indexes
            .into_iter()
            .map(|i| match self.dims[i] {
                Dim::Known(n) => Dim::Known(n.min(self.slices[i].1 - self.slices[i].0)),
                Dim::Unknown(c) => Dim::Unknown(c),
            })
            .collect::<Vec<_>>();
        Self::new(&new_dims)
    }

    /// Check if contiguous
    pub fn is_contiguous(&self) -> bool {
        self.indexes.iter().enumerate().all(|(a, b)| a == *b)
    }

    /// Realize the true shape
    pub fn shape(&self) -> Vec<Dim> {
        self.indexes.into_iter().map(|i| self.dims[i]).collect()
    }

    /// Take a slice
    pub fn slice(&mut self, slices: &[(usize, usize)]) {
        for (i, (s, e)) in slices.iter().enumerate() {
            // If we are slicing into a padded dim, remove as much padding as possible
            let mut s = *s;
            if self.padding[self.indexes[i]].0 != 0 {
                let padding = self.padding[self.indexes[i]].0;
                self.padding[self.indexes[i]].0 = self.padding[self.indexes[i]].0.saturating_sub(s);
                s = s.saturating_sub(padding);
            }
            self.slices[self.indexes[i]].0 += s;
            self.slices[self.indexes[i]].1 = self.slices[self.indexes[i]].1.min(*e);
        }
    }

    /// Add padding
    pub fn pad(&mut self, padding: &[(usize, usize)]) {
        for (i, (s, e)) in padding.iter().enumerate() {
            self.padding[self.indexes[i]].0 += *s;
            if *e != 0 && self.slices[self.indexes[i]].1 as i32 != i32::MAX {
                panic!("Adding padding to a slice isn't supported")
            }
            self.padding[self.indexes[i]].1 += *e;
        }
    }

    /// Given a dyn dim map, resolve global dyn dims into known dims
    pub fn resolve_global_dyn_dims(mut self, dyn_dim_map: &HashMap<char, usize>) -> Self {
        for d in self.dims.iter_mut() {
            if let Dim::Unknown(u) = *d {
                *d = Dim::Known(dyn_dim_map[&u]);
            }
        }
        self
    }

    pub fn is_sliced(&self) -> bool {
        self.slices
            .iter()
            .any(|(b, e)| *b != 0 || *e as i32 != i32::MAX)
    }
}

/// Resolve shapes between the two trackers to the best of our ability
pub fn resolve_local_dyn_dims(a: &mut ShapeTracker, b: &mut ShapeTracker, default_to_one: bool) {
    // B to A
    for i in 0..a.dims.len() {
        if matches!(a.dims[a.indexes[i]], Dim::Unknown('-')) {
            a.dims[a.indexes[i]] = b.dims[b.indexes[i]];
            if matches!(a.dims[a.indexes[i]], Dim::Unknown('-')) && default_to_one {
                a.dims[a.indexes[i]] = Dim::Known(1);
            }
        }
    }

    // A to B
    for i in 0..a.dims.len() {
        if matches!(b.dims[b.indexes[i]], Dim::Unknown('-')) {
            b.dims[b.indexes[i]] = a.dims[a.indexes[i]];
            if matches!(b.dims[b.indexes[i]], Dim::Unknown('-')) && default_to_one {
                b.dims[b.indexes[i]] = Dim::Known(1);
            }
        }
    }
}

pub struct Indexer {
    #[allow(clippy::type_complexity)]
    data: ArrayVec<[(usize, usize, (usize, usize), (usize, usize), bool); 10]>,
}

impl Indexer {
    /// Convert a logical index into a physical index
    pub fn index(&self, logical: usize) -> Option<usize> {
        let mut ret = 0;
        let mut acc = 1;
        for (sh, stride, padding, slice, fake) in self.data.into_iter() {
            let logical_sh = (sh + padding.0 + padding.1).min(slice.1) - slice.0;
            if !fake {
                let dim_ind = (logical / acc) % logical_sh;
                // Over top or under bottom
                if dim_ind >= (sh + padding.0).min(slice.1)
                    || dim_ind < padding.0.saturating_sub(slice.0)
                {
                    return None;
                }
                ret += (dim_ind - padding.0 + (slice.0.saturating_sub(padding.0))) * stride;
            }
            acc *= logical_sh;
        }
        Some(ret)
    }
}
