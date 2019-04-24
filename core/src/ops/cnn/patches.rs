use crate::internal::*;
use crate::ops::cnn::PaddingSpec;
use crate::ops::nn::{DataFormat, Shape};
use ndarray::prelude::*;
#[cfg(not(debug_assertions))]
use no_panic::no_panic;

use std::ops::Range;

use itertools::zip;

#[derive(Debug, Clone, PartialEq)]
pub struct PatchSpec {
    pub input_shape: TVec<usize>,
    pub input_inner_stride: usize,
    pub output_inner_stride: usize,
    pub kernel_shape: TVec<usize>,
    pub strides: TVec<usize>,
    pub dilations: TVec<usize>,
    pub padding: PaddingSpec,
}

impl PatchSpec {
    pub fn for_full_shape(data_format: DataFormat, input_full_shape: &[usize]) -> PatchSpec {
        let shape = data_format.shape(input_full_shape.into());
        Self::for_data_shape(shape)
    }

    pub fn for_data_shape(data_shape: Shape) -> PatchSpec {
        let input_shape: TVec<usize> = data_shape.hw_dims().into();
        PatchSpec {
            kernel_shape: tvec!(1; input_shape.len()),
            input_inner_stride: data_shape.w_stride(),
            output_inner_stride: 1,
            strides: tvec!(1; input_shape.len()),
            dilations: tvec!(1; input_shape.len()),
            padding: PaddingSpec::Valid,
            input_shape,
        }
    }

    pub fn with_kernel_shape(self, kernel_shape: TVec<usize>) -> PatchSpec {
        PatchSpec { kernel_shape, ..self }
    }

    pub fn with_dilations(self, dilations: TVec<usize>) -> PatchSpec {
        PatchSpec { dilations, ..self }
    }

    pub fn with_strides(self, strides: TVec<usize>) -> PatchSpec {
        PatchSpec { strides, ..self }
    }

    pub fn with_padding(self, padding: PaddingSpec) -> PatchSpec {
        PatchSpec { padding, ..self }
    }

    pub fn with_output_inner_stride(self, output_inner_stride: usize) -> PatchSpec {
        PatchSpec { output_inner_stride, ..self }
    }

    pub fn into_patch(self) -> Patch {
        let dims = self.padding.compute(
            &*self.input_shape,
            &*self.kernel_shape,
            &*self.dilations,
            &*self.strides,
        );
        let output: TVec<usize> = dims.iter().map(|d| d.output).collect();
        let pad_before: TVec<usize> = dims.iter().map(|d| d.pad_before).collect();
        let pad_after: TVec<usize> = dims.iter().map(|d| d.pad_after).collect();

        let data_field: Vec<isize> = ::ndarray::indices(&*self.kernel_shape)
            .into_iter()
            .flat_map(|coords| {
                coords
                    .slice()
                    .to_vec()
                    .into_iter()
                    .enumerate()
                    .map(|(ix, c)| (c * self.dilations[ix]) as isize - pad_before[ix] as isize)
            })
            .collect();
        let data_field = Array2::from_shape_vec(
            (self.kernel_shape.iter().cloned().product(), self.kernel_shape.len()),
            data_field,
        )
        .unwrap();
        let data_field_min_max: TVec<_> = data_field
            .gencolumns()
            .into_iter()
            .map(|col| (col.iter().min().cloned().unwrap(), col.iter().max().cloned().unwrap()))
            .collect();

        let mut input_layout_strides: Vec<isize> = vec![self.input_inner_stride as isize];
        for dim in self.input_shape.iter().skip(1).rev() {
            let previous = input_layout_strides.last().unwrap();
            input_layout_strides.push(*dim as isize * previous);
        }
        input_layout_strides.reverse();
        let standard_layout_data_field: Vec<isize> = data_field
            .outer_iter()
            .map(|coords| zip(coords, &input_layout_strides).map(|(a, b)| a * b).sum::<isize>())
            .collect();

        let mut valid_output_zone = tvec!();
        let mut invalid_output_zones = tvec!();
        for ix in 0..self.input_shape.len() {
            let min_max = data_field_min_max[ix];
            let min = (-min_max.0 as usize).div_ceil(self.strides[ix]) as usize;
            let max =
                (self.input_shape[ix] - min_max.1 as usize).div_ceil(self.strides[ix]) as usize;
            if min != 0 {
                let mut invalid = valid_output_zone.clone();
                invalid.push(0..min);
                while invalid.len() < output.len() {
                    invalid.push(0..output[invalid.len()])
                }
                invalid_output_zones.push(invalid);
            }
            if max < output[ix] {
                let mut invalid = valid_output_zone.clone();
                invalid.push(max..output[ix]);
                while invalid.len() < output.len() {
                    invalid.push(0..output[invalid.len()])
                }
                invalid_output_zones.push(invalid);
            }
            valid_output_zone.push(min..max)
        }

        let op_strides_times_input_storage_strides =
            zip(&self.strides, &input_layout_strides).map(|(a, b)| (*a as isize * b)).collect();

        Patch {
            spec: self,
            padded: pad_before.iter().any(|&p| p != 0) || pad_after.iter().any(|&p| p != 0),
            pad_before,
            pad_after,
            output_shape: output,
            data_field,
            data_field_min_max,
            standard_layout_data_field,
            op_strides_times_input_storage_strides,
            valid_output_zone,
            invalid_output_zones,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Patch {
    pub spec: PatchSpec,
    pub pad_before: TVec<usize>,
    pub pad_after: TVec<usize>,
    pub padded: bool,
    pub output_shape: TVec<usize>,
    pub data_field: Array2<isize>,
    pub data_field_min_max: TVec<(isize, isize)>,
    pub standard_layout_data_field: Vec<isize>,
    pub op_strides_times_input_storage_strides: TVec<isize>,
    pub valid_output_zone: TVec<Range<usize>>,
    pub invalid_output_zones: TVec<TVec<Range<usize>>>,
}

impl Patch {
    pub fn rank(&self) -> usize {
        self.spec.input_shape.len()
    }

    unsafe fn is_valid(&self, coords: &[usize]) -> bool {
        for ix in 0..self.rank() {
            let c = *coords.get_unchecked(ix) as isize;
            let strides = *self.spec.strides.get_unchecked(ix) as isize;
            let pos = c * strides;
            let min_max = self.data_field_min_max.get_unchecked(ix);
            if pos + min_max.0 < 0
                || pos + min_max.1 >= *self.spec.input_shape.get_unchecked(ix) as isize
            {
                return false;
            }
        }
        true
    }

    pub fn visit_zone_1<'p>(
        &'p self,
        zone: &'p [Range<usize>],
        valid_hint: Option<bool>,
    ) -> impl Iterator<Item = (usize, Option<bool>)> + 'p {
        let shape = zone[0].end - zone[0].start;
        ndarray::indices(shape)
            .into_iter()
            .map(move |coords| (unsafe { zone.get_unchecked(0).start + coords }, valid_hint))
    }

    pub fn visit_zone_2<'p>(
        &'p self,
        zone: &'p [Range<usize>],
        valid_hint: Option<bool>,
    ) -> impl Iterator<Item = ((usize, usize), Option<bool>)> + 'p {
        let shape = (zone[0].end - zone[0].start, zone[1].end - zone[1].start);
        ndarray::indices(shape).into_iter().map(move |coords| {
            (
                unsafe {
                    (zone.get_unchecked(0).start + coords.0, zone.get_unchecked(1).start + coords.1)
                },
                valid_hint,
            )
        })
    }

    pub fn visit_zone_d<'p>(
        &'p self,
        zone: &'p [Range<usize>],
        valid_hint: Option<bool>,
    ) -> impl Iterator<Item = (TVec<usize>, Option<bool>)> + 'p {
        let shape: Vec<usize> = zone.iter().map(|z| z.end - z.start).collect();
        ndarray::indices(shape).into_iter().map(move |coords| {
            let mut coords: TVec<usize> = coords.slice().into();
            for i in 0..coords.len() {
                coords[i] += zone[i].start;
            }
            (coords, valid_hint)
        })
    }

    pub fn visit_all_1(&self) -> impl Iterator<Item = (usize, Option<bool>)> + '_ {
        self.visit_valid_1().chain(self.visit_invalid_1())
    }

    pub fn visit_valid_1(&self) -> impl Iterator<Item = (usize, Option<bool>)> + '_ {
        self.visit_zone_1(&*self.valid_output_zone, Some(true))
    }

    pub fn visit_invalid_1(&self) -> impl Iterator<Item = (usize, Option<bool>)> + '_ {
        self.invalid_output_zones.iter().flat_map(move |z| self.visit_zone_1(z, Some(false)))
    }

    pub fn visit_all_2(&self) -> impl Iterator<Item = ((usize, usize), Option<bool>)> + '_ {
        self.visit_valid_2().chain(self.visit_invalid_2())
    }

    pub fn visit_valid_2(&self) -> impl Iterator<Item = ((usize, usize), Option<bool>)> + '_ {
        self.visit_zone_2(&*self.valid_output_zone, Some(true))
    }

    pub fn visit_invalid_2(&self) -> impl Iterator<Item = ((usize, usize), Option<bool>)> + '_ {
        self.invalid_output_zones.iter().flat_map(move |z| self.visit_zone_2(z, Some(false)))
    }

    pub fn visit_all_d(&self) -> impl Iterator<Item = (TVec<usize>, Option<bool>)> + '_ {
        self.visit_valid_d().chain(self.visit_invalid_d())
    }

    pub fn visit_valid_d(&self) -> impl Iterator<Item = (TVec<usize>, Option<bool>)> + '_ {
        self.visit_zone_d(&*self.valid_output_zone, Some(true))
    }

    pub fn visit_invalid_d(&self) -> impl Iterator<Item = (TVec<usize>, Option<bool>)> + '_ {
        self.invalid_output_zones.iter().flat_map(move |z| self.visit_zone_d(z, Some(false)))
    }

    pub fn at<'p>(&'p self, coords: &[usize]) -> PatchIterator<'p> {
        self.at_hint(coords, None)
    }

    pub fn at_hint<'p>(&'p self, coords: &[usize], hint: Option<bool>) -> PatchIterator<'p> {
        unsafe {
            assert_eq!(coords.len(), self.spec.kernel_shape.len());
            let mut center = 0;
            for i in 0..self.op_strides_times_input_storage_strides.len() {
                center += *self.op_strides_times_input_storage_strides.get_unchecked(i)
                    * *coords.get_unchecked(i) as isize;
            }
            let valid = hint.unwrap_or_else(|| !self.padded || self.is_valid(coords));
            if valid {
                PatchIterator::Fast(FastPatchIterator { patch: &self, center, item: 0 })
            } else {
                let mut input_patch_center: TVec<_> = coords.into();
                input_patch_center
                    .iter_mut()
                    .zip(self.spec.strides.iter())
                    .for_each(|(a, &b)| *a *= b as usize);
                PatchIterator::Safe(SafePatchIterator {
                    patch: self,
                    item: 0,
                    input_patch_center,
                    center,
                })
            }
        }
    }

    pub fn global_offset_for(&self, coords: &[usize], patch_index: usize) -> usize {
        assert_eq!(coords.len(), self.spec.kernel_shape.len());
        let center = zip(coords, &self.op_strides_times_input_storage_strides)
            .map(|(a, b)| *a as isize * *b)
            .sum::<isize>();
        (center + self.standard_layout_data_field[patch_index]) as usize
    }
}

struct Window {
    pub output_center_offset: isize,
    pub input_center_offset: isize,
    pub valid_hint: Option<bool>,
}

#[derive(Debug)]
pub enum PatchIterator<'p> {
    Fast(FastPatchIterator<'p>),
    Safe(SafePatchIterator<'p>),
}

impl<'p> Iterator for PatchIterator<'p> {
    type Item = Option<isize>;
    #[inline(always)]
    fn next(&mut self) -> Option<Option<isize>> {
        match self {
            &mut PatchIterator::Fast(ref mut it) => it.next(),
            &mut PatchIterator::Safe(ref mut it) => it.next(),
        }
    }
}

#[derive(Debug)]
pub struct FastPatchIterator<'p> {
    patch: &'p Patch,
    center: isize,
    item: usize,
}

impl<'p> Iterator for FastPatchIterator<'p> {
    type Item = Option<isize>;
    #[inline(always)]
    #[cfg_attr(not(debug_assertions), no_panic)]
    fn next(&mut self) -> Option<Option<isize>> {
        if self.item == self.patch.standard_layout_data_field.len() {
            return None;
        }
        unsafe {
            let position =
                self.center + self.patch.standard_layout_data_field.get_unchecked(self.item);
            self.item += 1;
            Some(Some(position))
        }
    }
}

#[derive(Debug)]
pub struct SafePatchIterator<'p> {
    patch: &'p Patch,
    item: usize,
    input_patch_center: TVec<usize>,
    center: isize,
}

impl<'p> Iterator for SafePatchIterator<'p> {
    type Item = Option<isize>;
    #[cfg_attr(not(debug_assertions), no_panic)]
    fn next(&mut self) -> Option<Option<isize>> {
        unsafe {
            if self.item == self.patch.standard_layout_data_field.len() {
                return None;
            }
            let input_shape = &self.patch.spec.input_shape;
            let img_offset =
                self.patch.data_field.as_ptr().offset((self.item * input_shape.len()) as isize);

            for ix in 0..input_shape.len() {
                let pos = *self.input_patch_center.get_unchecked(ix) as isize
                    + *img_offset.offset(ix as isize);
                if pos < 0 || pos as usize >= *input_shape.get_unchecked(ix) {
                    self.item += 1;
                    return Some(None);
                }
            }
            let position =
                self.center + self.patch.standard_layout_data_field.get_unchecked(self.item);
            self.item += 1;
            Some(Some(position))
        }
    }
}

#[cfg(test)]
pub mod test {
    use super::*;
    use crate::ops::nn::DataFormat::NCHW;
    use proptest::prelude::*;
    use proptest::*;

    fn compute_output_spatial_dim(
        input: usize,
        dilation: usize,
        kdim: usize,
        pad_before: usize,
        bad_after: usize,
        stride: usize,
    ) -> usize {
        let patch = PatchSpec::for_full_shape(NCHW, &[1, 1, input])
            .with_dilations(tvec!(dilation))
            .with_kernel_shape(tvec!(kdim))
            .with_padding(PaddingSpec::Explicit(tvec![pad_before], tvec![bad_after]))
            .with_strides(tvec![stride])
            .into_patch();
        patch.output_shape[0]
    }

    #[test]
    fn basic() {
        assert_eq!(compute_output_spatial_dim(5, 1, 3, 0, 0, 1), 3);
    }

    #[test]
    fn strides() {
        assert_eq!(compute_output_spatial_dim(7, 1, 3, 0, 0, 2), 3);
    }

    #[test]
    fn padding() {
        assert_eq!(compute_output_spatial_dim(5, 1, 3, 1, 1, 1), 5);
    }

    #[test]
    fn strides_and_padding() {
        assert_eq!(compute_output_spatial_dim(7, 1, 3, 1, 1, 2), 4);
    }

    fn field(kdim: &[usize], dilations: &[usize]) -> Array2<isize> {
        let patch = PatchSpec::for_data_shape(NCHW.from_n_c_hw(1, 1, tvec![10; kdim.len()]))
            .with_dilations(dilations.into())
            .with_kernel_shape(kdim.into())
            .with_padding(PaddingSpec::Explicit(tvec![0; kdim.len()], tvec![0; kdim.len()]))
            .with_strides(tvec![1; kdim.len()])
            .into_patch();
        patch.data_field
    }

    #[test]
    fn test_field() {
        assert_eq!(field(&[3], &[1]), arr2(&[[0], [1], [2]]));
        assert_eq!(field(&[3], &[2]), arr2(&[[0], [2], [4]]));
        assert_eq!(field(&[2, 2], &[1, 1]), arr2(&[[0, 0], [0, 1], [1, 0], [1, 1]]));
        assert_eq!(field(&[2, 2], &[2, 1]), arr2(&[[0, 0], [0, 1], [2, 0], [2, 1]]));
    }

    pub fn patch_2d() -> BoxedStrategy<(Shape, Patch)> {
        (
            Just(DataFormat::NCHW),
            (1usize..3, 1usize..3),
            1usize..3,
            (1usize..3, 1usize..3),
            //prop_oneof![PaddingSpec::SameLower, PaddingSpec::Valid],
            Just(PaddingSpec::SameLower),
            (1usize..4, 1usize..4),
        )
            .prop_flat_map(|p| {
                let size = p.3;
                (Just(p), (size.0 + 5..=size.0 + 10, size.1 + 5..=size.1 + 10))
            })
            .prop_map(|((fmt, dil, c, ks, pad, strides), inp)| {
                (
                    fmt.shape(tvec!(1, c, inp.0, inp.1)),
                    PatchSpec::for_full_shape(fmt, &[1, c, inp.0, inp.1])
                        .with_dilations(tvec!(dil.0, dil.1))
                        .with_kernel_shape(tvec!(ks.0, ks.1))
                        .with_padding(pad)
                        .with_strides(tvec![strides.0, strides.1])
                        .into_patch(),
                )
            })
            .boxed()
    }

    fn in_zone(coords: &[usize], h_axis: usize, zone: &[Range<usize>]) -> bool {
        for a in 0..zone.len() {
            if coords[h_axis + a] < zone[a].start || coords[h_axis + a] >= zone[a].end {
                return false;
            }
        }
        true
    }

    proptest! {
        #[test]
        fn test_zoning((input_shape, p) in patch_2d()) {
            let valid_zone = &p.valid_output_zone;
            let invalid_zones = &p.invalid_output_zones;
            let output_full_shape = input_shape.fmt.from_n_c_hw(input_shape.n(), 1, &*p.output_shape);
            let h_axis = input_shape.h_axis();
            for coords in ndarray::indices(&*output_full_shape.shape) {
                let inside_valid = in_zone(coords.slice(), h_axis, valid_zone);
                let invalid_count = invalid_zones.iter().filter(|z| in_zone(coords.slice(), h_axis, z)).count();
                unsafe {
                    prop_assert_eq!(inside_valid, p.is_valid(&coords.slice()[input_shape.hw_axes()]), "coords {:?}, valid_zone: {:?} inside_valid: {:?}", coords.slice(), valid_zone, inside_valid);
                }
                if inside_valid {
                    prop_assert_eq!(invalid_count, 0);
                } else {
                    prop_assert_eq!(invalid_count, 1, "coords {:?}, valid_zone: {:?} inside_valid: {:?} invalid_zones: {:?}", coords.slice(), valid_zone, inside_valid, invalid_zones);
                }
            };
        }

        #[test]
        #[ignore]
        fn test_zone_visitor((input_shape, p) in patch_2d()) {
            let output_full_shape = input_shape.fmt.from_n_c_hw(input_shape.n(), 1, &*p.output_shape);
            let mut output = ndarray::ArrayD::<i32>::zeros(&*output_full_shape.shape);
            for (c, _v) in p.visit_all_2() {
                prop_assert!(output[[0, 0, c.0, c.1]] == 0);
                output[[0, 0, c.0, c.1]] = 1;
            }
            assert!(output.iter().all(|&x| x == 1));
        }
    }
    #[test]
    fn test_zone_visitor_1() {
        let p = PatchSpec::for_full_shape(DataFormat::NCHW, &[1, 1, 2, 2])
            .with_kernel_shape(tvec![2, 1])
            .with_padding(PaddingSpec::SameLower)
            .with_strides(tvec![1, 2])
            .into_patch();
        let output_shape = DataFormat::NCHW.from_n_c_hw(1, 1, &*p.output_shape);
        let mut output = ndarray::ArrayD::<i32>::zeros(&*output_shape.shape);
        for (c, _v) in p.visit_all_2() {
            assert!(output[[0, 0, c.0, c.1]] == 0);
            output[[0, 0, c.0, c.1]] = 1;
        }
        assert!(output.iter().all(|&x| x == 1));
    }
}