/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

use crate::{binding_model::BindEntryMap, FastHashMap};
use spirv_headers as spirv;
use thiserror::Error;
use wgt::{BindGroupLayoutEntry, BindingType};

#[derive(Clone, Debug, Error)]
#[error("buffer usage is {actual:?} which does not contain required usage {expected:?}")]
pub struct MissingBufferUsageError {
    pub(crate) actual: wgt::BufferUsage,
    pub(crate) expected: wgt::BufferUsage,
}

/// Checks that the given buffer usage contains the required buffer usage,
/// returns an error otherwise.
pub fn check_buffer_usage(
    actual: wgt::BufferUsage,
    expected: wgt::BufferUsage,
) -> Result<(), MissingBufferUsageError> {
    if !actual.contains(expected) {
        Err(MissingBufferUsageError { actual, expected })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Error)]
#[error("texture usage is {actual:?} which does not contain required usage {expected:?}")]
pub struct MissingTextureUsageError {
    pub(crate) actual: wgt::TextureUsage,
    pub(crate) expected: wgt::TextureUsage,
}

/// Checks that the given texture usage contains the required texture usage,
/// returns an error otherwise.
pub fn check_texture_usage(
    actual: wgt::TextureUsage,
    expected: wgt::TextureUsage,
) -> Result<(), MissingTextureUsageError> {
    if !actual.contains(expected) {
        Err(MissingTextureUsageError { actual, expected })
    } else {
        Ok(())
    }
}

#[derive(Clone, Debug, Error)]
pub enum BindingError {
    #[error("binding is missing from the pipeline layout")]
    Missing,
    #[error("visibility flags don't include the shader stage")]
    Invisible,
    #[error("load/store access flags {0:?} don't match the shader")]
    WrongUsage(naga::GlobalUse),
    #[error("type on the shader side does not match the pipeline binding")]
    WrongType,
    #[error("buffer structure size {0}, added to one element of an unbound array, if it's the last field, ended up greater than the given `min_binding_size`")]
    WrongBufferSize(wgt::BufferAddress),
    #[error("view dimension {dim:?} (is array: {is_array}) doesn't match the shader")]
    WrongTextureViewDimension { dim: spirv::Dim, is_array: bool },
    #[error("component type {0:?} of a sampled texture doesn't match the shader")]
    WrongTextureComponentType(Option<naga::ScalarKind>),
    #[error("texture sampling capability doesn't match the shader")]
    WrongTextureSampled,
    #[error("multisampled flag doesn't match the shader")]
    WrongTextureMultisampled,
    #[error("comparison flag doesn't match the shader")]
    WrongSamplerComparison,
}

#[derive(Clone, Debug, Error)]
pub enum InputError {
    #[error("input is not provided by the earlier stage in the pipeline")]
    Missing,
    #[error("input type is not compatible with the provided")]
    WrongType,
}

/// Errors produced when validating a programmable stage of a pipeline.
#[derive(Clone, Debug, Error)]
pub enum StageError {
    #[error("unable to find an entry point matching the {0:?} execution model")]
    MissingEntryPoint(spirv::ExecutionModel),
    #[error("error matching global binding at index {binding} in set {set} against the pipeline layout: {error}")]
    Binding {
        set: u32,
        binding: u32,
        error: BindingError,
    },
    #[error(
        "error matching the stage input at {location} against the previous stage outputs: {error}"
    )]
    Input {
        location: wgt::ShaderLocation,
        error: InputError,
    },
}

fn get_aligned_type_size(
    module: &naga::Module,
    handle: naga::Handle<naga::Type>,
    allow_unbound: bool,
) -> wgt::BufferAddress {
    use naga::TypeInner as Ti;
    //TODO: take alignment into account!
    match module.types[handle].inner {
        Ti::Scalar { kind: _, width } => width as wgt::BufferAddress / 8,
        Ti::Vector {
            size,
            kind: _,
            width,
        } => size as wgt::BufferAddress * width as wgt::BufferAddress / 8,
        Ti::Matrix {
            rows,
            columns,
            kind: _,
            width,
        } => {
            rows as wgt::BufferAddress * columns as wgt::BufferAddress * width as wgt::BufferAddress
                / 8
        }
        Ti::Pointer { .. } => 4,
        Ti::Array {
            base,
            size: naga::ArraySize::Static(count),
            stride,
        } => {
            let base_size = match stride {
                Some(stride) => stride.get() as wgt::BufferAddress,
                None => get_aligned_type_size(module, base, false),
            };
            base_size * count as wgt::BufferAddress
        }
        Ti::Array {
            base,
            size: naga::ArraySize::Dynamic,
            stride,
        } if allow_unbound => match stride {
            Some(stride) => stride.get() as wgt::BufferAddress,
            None => get_aligned_type_size(module, base, false),
        },
        Ti::Struct { ref members } => members.last().map_or(0, |member| {
            member.offset as wgt::BufferAddress + get_aligned_type_size(module, member.ty, false)
        }),
        _ => panic!("Unexpected struct field"),
    }
}

fn check_binding(
    module: &naga::Module,
    var: &naga::GlobalVariable,
    entry: &BindGroupLayoutEntry,
    usage: naga::GlobalUse,
) -> Result<(), BindingError> {
    let mut ty_inner = &module.types[var.ty].inner;
    //TODO: change naga's IR to avoid a pointer here
    if let naga::TypeInner::Pointer { base, class: _ } = *ty_inner {
        ty_inner = &module.types[base].inner;
    }
    let allowed_usage = match *ty_inner {
        naga::TypeInner::Struct { ref members } => {
            let (allowed_usage, min_size) = match entry.ty {
                BindingType::UniformBuffer {
                    dynamic: _,
                    min_binding_size,
                } => (naga::GlobalUse::LOAD, min_binding_size),
                BindingType::StorageBuffer {
                    dynamic: _,
                    min_binding_size,
                    readonly,
                } => {
                    let global_use = if readonly {
                        naga::GlobalUse::LOAD
                    } else {
                        naga::GlobalUse::all()
                    };
                    (global_use, min_binding_size)
                }
                _ => return Err(BindingError::WrongType),
            };
            let mut actual_size = 0;
            for (i, member) in members.iter().enumerate() {
                actual_size += get_aligned_type_size(module, member.ty, i + 1 == members.len());
            }
            match min_size {
                Some(non_zero) if non_zero.get() < actual_size => {
                    return Err(BindingError::WrongBufferSize(actual_size))
                }
                _ => (),
            }
            allowed_usage
        }
        naga::TypeInner::Sampler { comparison } => match entry.ty {
            BindingType::Sampler { comparison: cmp } => {
                if cmp == comparison {
                    naga::GlobalUse::empty()
                } else {
                    return Err(BindingError::WrongSamplerComparison);
                }
            }
            _ => return Err(BindingError::WrongType),
        },
        naga::TypeInner::Image { base, dim, flags } => {
            if flags.contains(naga::ImageFlags::MULTISAMPLED) {
                match entry.ty {
                    BindingType::SampledTexture {
                        multisampled: true, ..
                    } => {}
                    _ => return Err(BindingError::WrongTextureMultisampled),
                }
            }
            let view_dimension = match entry.ty {
                BindingType::SampledTexture { dimension, .. }
                | BindingType::StorageTexture { dimension, .. } => dimension,
                _ => {
                    return Err(BindingError::WrongTextureViewDimension {
                        dim,
                        is_array: true,
                    })
                }
            };
            if flags.contains(naga::ImageFlags::ARRAYED) {
                match (dim, view_dimension) {
                    (spirv::Dim::Dim2D, wgt::TextureViewDimension::D2Array) => (),
                    (spirv::Dim::DimCube, wgt::TextureViewDimension::CubeArray) => (),
                    _ => {
                        return Err(BindingError::WrongTextureViewDimension {
                            dim,
                            is_array: true,
                        })
                    }
                }
            } else {
                match (dim, view_dimension) {
                    (spirv::Dim::Dim1D, wgt::TextureViewDimension::D1) => (),
                    (spirv::Dim::Dim2D, wgt::TextureViewDimension::D2) => (),
                    (spirv::Dim::Dim3D, wgt::TextureViewDimension::D3) => (),
                    (spirv::Dim::DimCube, wgt::TextureViewDimension::Cube) => (),
                    _ => {
                        return Err(BindingError::WrongTextureViewDimension {
                            dim,
                            is_array: false,
                        })
                    }
                }
            }
            let (allowed_usage, is_sampled) = match entry.ty {
                BindingType::SampledTexture { component_type, .. } => {
                    let expected_scalar_kind = match component_type {
                        wgt::TextureComponentType::Float => naga::ScalarKind::Float,
                        wgt::TextureComponentType::Sint => naga::ScalarKind::Sint,
                        wgt::TextureComponentType::Uint => naga::ScalarKind::Uint,
                    };
                    match module.types[base].inner {
                        naga::TypeInner::Scalar { kind, .. }
                        | naga::TypeInner::Vector { kind, .. }
                            if kind == expected_scalar_kind => {}
                        naga::TypeInner::Scalar { kind, .. }
                        | naga::TypeInner::Vector { kind, .. } => {
                            return Err(BindingError::WrongTextureComponentType(Some(kind)))
                        }
                        _ => return Err(BindingError::WrongTextureComponentType(None)),
                    };
                    (naga::GlobalUse::LOAD, true)
                }
                BindingType::StorageTexture { readonly, .. } => {
                    if readonly {
                        //TODO: check entry.storage_texture_format
                        (naga::GlobalUse::LOAD, false)
                    } else {
                        (naga::GlobalUse::STORE, false)
                    }
                }
                _ => return Err(BindingError::WrongType),
            };
            if is_sampled != flags.contains(naga::ImageFlags::SAMPLED) {
                return Err(BindingError::WrongTextureSampled);
            }
            allowed_usage
        }
        _ => return Err(BindingError::WrongType),
    };
    if allowed_usage.contains(usage) {
        Ok(())
    } else {
        Err(BindingError::WrongUsage(usage))
    }
}

fn is_sub_type(sub: &naga::TypeInner, provided: &naga::TypeInner) -> bool {
    use naga::TypeInner as Ti;

    match (sub, provided) {
        (
            &Ti::Scalar {
                kind: k0,
                width: w0,
            },
            &Ti::Scalar {
                kind: k1,
                width: w1,
            },
        ) => k0 == k1 && w0 <= w1,
        (
            &Ti::Scalar {
                kind: k0,
                width: w0,
            },
            &Ti::Vector {
                size: _,
                kind: k1,
                width: w1,
            },
        ) => k0 == k1 && w0 <= w1,
        (
            &Ti::Vector {
                size: s0,
                kind: k0,
                width: w0,
            },
            &Ti::Vector {
                size: s1,
                kind: k1,
                width: w1,
            },
        ) => s0 as u8 <= s1 as u8 && k0 == k1 && w0 <= w1,
        (
            &Ti::Matrix {
                columns: c0,
                rows: r0,
                kind: k0,
                width: w0,
            },
            &Ti::Matrix {
                columns: c1,
                rows: r1,
                kind: k1,
                width: w1,
            },
        ) => c0 == c1 && r0 == r1 && k0 == k1 && w0 <= w1,
        (&Ti::Struct { members: ref m0 }, &Ti::Struct { members: ref m1 }) => m0 == m1,
        _ => false,
    }
}

pub enum MaybeOwned<'a, T> {
    Owned(T),
    Borrowed(&'a T),
}

impl<'a, T> std::ops::Deref for MaybeOwned<'a, T> {
    type Target = T;
    fn deref(&self) -> &T {
        match *self {
            MaybeOwned::Owned(ref value) => value,
            MaybeOwned::Borrowed(value) => value,
        }
    }
}

pub fn map_vertex_format(format: wgt::VertexFormat) -> naga::TypeInner {
    use naga::TypeInner as Ti;
    use wgt::VertexFormat as Vf;
    match format {
        Vf::Uchar2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Uint,
            width: 8,
        },
        Vf::Uchar4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Uint,
            width: 8,
        },
        Vf::Char2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Sint,
            width: 8,
        },
        Vf::Char4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Sint,
            width: 8,
        },
        Vf::Uchar2Norm => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Float,
            width: 8,
        },
        Vf::Uchar4Norm => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Float,
            width: 8,
        },
        Vf::Char2Norm => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Float,
            width: 8,
        },
        Vf::Char4Norm => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Float,
            width: 8,
        },
        Vf::Ushort2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Uint,
            width: 16,
        },
        Vf::Ushort4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Uint,
            width: 16,
        },
        Vf::Short2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Sint,
            width: 16,
        },
        Vf::Short4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Sint,
            width: 16,
        },
        Vf::Ushort2Norm | Vf::Short2Norm | Vf::Half2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Float,
            width: 16,
        },
        Vf::Ushort4Norm | Vf::Short4Norm | Vf::Half4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Float,
            width: 16,
        },
        Vf::Float => Ti::Scalar {
            kind: naga::ScalarKind::Float,
            width: 32,
        },
        Vf::Float2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Float,
            width: 32,
        },
        Vf::Float3 => Ti::Vector {
            size: naga::VectorSize::Tri,
            kind: naga::ScalarKind::Float,
            width: 32,
        },
        Vf::Float4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Float,
            width: 32,
        },
        Vf::Uint => Ti::Scalar {
            kind: naga::ScalarKind::Uint,
            width: 32,
        },
        Vf::Uint2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Uint,
            width: 32,
        },
        Vf::Uint3 => Ti::Vector {
            size: naga::VectorSize::Tri,
            kind: naga::ScalarKind::Uint,
            width: 32,
        },
        Vf::Uint4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Uint,
            width: 32,
        },
        Vf::Int => Ti::Scalar {
            kind: naga::ScalarKind::Sint,
            width: 32,
        },
        Vf::Int2 => Ti::Vector {
            size: naga::VectorSize::Bi,
            kind: naga::ScalarKind::Sint,
            width: 32,
        },
        Vf::Int3 => Ti::Vector {
            size: naga::VectorSize::Tri,
            kind: naga::ScalarKind::Sint,
            width: 32,
        },
        Vf::Int4 => Ti::Vector {
            size: naga::VectorSize::Quad,
            kind: naga::ScalarKind::Sint,
            width: 32,
        },
    }
}

fn map_texture_format(format: wgt::TextureFormat) -> naga::TypeInner {
    use naga::{ScalarKind as Sk, TypeInner as Ti, VectorSize as Vs};
    use wgt::TextureFormat as Tf;

    match format {
        Tf::R8Unorm | Tf::R8Snorm => Ti::Scalar {
            kind: Sk::Float,
            width: 8,
        },
        Tf::R8Uint => Ti::Scalar {
            kind: Sk::Uint,
            width: 8,
        },
        Tf::R8Sint => Ti::Scalar {
            kind: Sk::Sint,
            width: 8,
        },
        Tf::R16Uint => Ti::Scalar {
            kind: Sk::Uint,
            width: 16,
        },
        Tf::R16Sint => Ti::Scalar {
            kind: Sk::Sint,
            width: 16,
        },
        Tf::R16Float => Ti::Scalar {
            kind: Sk::Float,
            width: 16,
        },
        Tf::Rg8Unorm | Tf::Rg8Snorm => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Float,
            width: 8,
        },
        Tf::Rg8Uint => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Uint,
            width: 8,
        },
        Tf::Rg8Sint => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Sint,
            width: 8,
        },
        Tf::R32Uint => Ti::Scalar {
            kind: Sk::Uint,
            width: 32,
        },
        Tf::R32Sint => Ti::Scalar {
            kind: Sk::Sint,
            width: 32,
        },
        Tf::R32Float => Ti::Scalar {
            kind: Sk::Float,
            width: 32,
        },
        Tf::Rg16Uint => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Uint,
            width: 16,
        },
        Tf::Rg16Sint => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Sint,
            width: 16,
        },
        Tf::Rg16Float => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Float,
            width: 16,
        },
        Tf::Rgba8Unorm
        | Tf::Rgba8UnormSrgb
        | Tf::Rgba8Snorm
        | Tf::Bgra8Unorm
        | Tf::Bgra8UnormSrgb => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Float,
            width: 8,
        },
        Tf::Rgba8Uint => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Uint,
            width: 8,
        },
        Tf::Rgba8Sint => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Sint,
            width: 8,
        },
        Tf::Rgb10a2Unorm => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Float,
            width: 10,
        },
        Tf::Rg11b10Float => Ti::Vector {
            size: Vs::Tri,
            kind: Sk::Float,
            width: 11,
        },
        Tf::Rg32Uint => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Uint,
            width: 32,
        },
        Tf::Rg32Sint => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Sint,
            width: 32,
        },
        Tf::Rg32Float => Ti::Vector {
            size: Vs::Bi,
            kind: Sk::Float,
            width: 32,
        },
        Tf::Rgba16Uint => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Uint,
            width: 16,
        },
        Tf::Rgba16Sint => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Sint,
            width: 16,
        },
        Tf::Rgba16Float => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Float,
            width: 16,
        },
        Tf::Rgba32Uint => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Uint,
            width: 32,
        },
        Tf::Rgba32Sint => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Sint,
            width: 32,
        },
        Tf::Rgba32Float => Ti::Vector {
            size: Vs::Quad,
            kind: Sk::Float,
            width: 32,
        },
        Tf::Depth32Float | Tf::Depth24Plus | Tf::Depth24PlusStencil8 => {
            panic!("Unexpected depth format")
        }
    }
}

/// Return true if the fragment `format` is covered by the provided `output`.
pub fn check_texture_format(format: wgt::TextureFormat, output: &naga::TypeInner) -> bool {
    let required = map_texture_format(format);
    is_sub_type(&required, output)
}

pub type StageInterface<'a> = FastHashMap<wgt::ShaderLocation, MaybeOwned<'a, naga::TypeInner>>;

pub fn check_stage<'a>(
    module: &'a naga::Module,
    group_layouts: &[&BindEntryMap],
    entry_point_name: &str,
    execution_model: spirv::ExecutionModel,
    inputs: StageInterface<'a>,
) -> Result<StageInterface<'a>, StageError> {
    // Since a shader module can have multiple entry points with the same name,
    // we need to look for one with the right execution model.
    let entry_point = module
        .entry_points
        .iter()
        .find(|entry_point| {
            entry_point.name == entry_point_name && entry_point.exec_model == execution_model
        })
        .ok_or(StageError::MissingEntryPoint(execution_model))?;
    let stage_bit = match execution_model {
        spirv::ExecutionModel::Vertex => wgt::ShaderStage::VERTEX,
        spirv::ExecutionModel::Fragment => wgt::ShaderStage::FRAGMENT,
        spirv::ExecutionModel::GLCompute => wgt::ShaderStage::COMPUTE,
        // the entry point wouldn't match otherwise
        _ => unreachable!(),
    };

    let function = &module.functions[entry_point.function];
    let mut outputs = StageInterface::default();
    for ((_, var), &usage) in module.global_variables.iter().zip(&function.global_usage) {
        if usage.is_empty() {
            continue;
        }
        match var.binding {
            Some(naga::Binding::Descriptor { set, binding }) => {
                let result = group_layouts
                    .get(set as usize)
                    .and_then(|map| map.get(&binding))
                    .ok_or(BindingError::Missing)
                    .and_then(|entry| {
                        if entry.visibility.contains(stage_bit) {
                            Ok(entry)
                        } else {
                            Err(BindingError::Invisible)
                        }
                    })
                    .and_then(|entry| check_binding(module, var, entry, usage));
                if let Err(error) = result {
                    return Err(StageError::Binding {
                        set,
                        binding,
                        error,
                    });
                }
            }
            Some(naga::Binding::Location(location)) => {
                let mut ty = &module.types[var.ty].inner;
                //TODO: change naga's IR to not have pointer for varyings
                if let naga::TypeInner::Pointer { base, class: _ } = *ty {
                    ty = &module.types[base].inner;
                }
                if usage.contains(naga::GlobalUse::STORE) {
                    outputs.insert(location, MaybeOwned::Borrowed(ty));
                } else {
                    let result =
                        inputs
                            .get(&location)
                            .ok_or(InputError::Missing)
                            .and_then(|provided| {
                                if is_sub_type(ty, provided) {
                                    Ok(())
                                } else {
                                    Err(InputError::WrongType)
                                }
                            });
                    if let Err(error) = result {
                        return Err(StageError::Input { location, error });
                    }
                }
            }
            _ => {}
        }
    }
    Ok(outputs)
}
