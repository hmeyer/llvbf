//! Type sizes and layouts according to the module's data layout.

use anyhow::{bail, Result};
use llvm_ir::types::NamedStructDef;
use llvm_ir::{Module, Type, TypeRef};

pub struct TypeInfo<'m> {
    module: &'m Module,
    pub ptr_bytes: usize,
}

impl<'m> TypeInfo<'m> {
    pub fn new(module: &'m Module) -> Self {
        let ptr_bits = module.data_layout.alignments.ptr_alignment(0).size;
        TypeInfo {
            module,
            ptr_bytes: (ptr_bits as usize + 7) / 8,
        }
    }

    /// Resolve named struct types to their definitions.
    pub fn resolve(&self, ty: &'m TypeRef) -> Result<&'m Type> {
        match ty.as_ref() {
            Type::NamedStructType { name } => match self.module.types.named_struct_def(name) {
                Some(NamedStructDef::Defined(t)) => self.resolve(t),
                Some(NamedStructDef::Opaque) => bail!("opaque struct type %{}", name),
                None => bail!("unknown struct type %{}", name),
            },
            t => Ok(t),
        }
    }

    /// Resolve one level, returning an owned TypeRef (for element types).
    fn resolve_ref(&self, ty: &TypeRef) -> Result<TypeRef> {
        match ty.as_ref() {
            Type::NamedStructType { name } => match self.module.types.named_struct_def(name) {
                Some(NamedStructDef::Defined(t)) => self.resolve_ref(t),
                Some(NamedStructDef::Opaque) => bail!("opaque struct type %{}", name),
                None => bail!("unknown struct type %{}", name),
            },
            _ => Ok(ty.clone()),
        }
    }

    /// ABI alignment in bytes.
    pub fn align_of(&self, ty: &TypeRef) -> Result<usize> {
        let ty = self.resolve_ref(ty)?;
        Ok(match ty.as_ref() {
            Type::IntegerType { bits } => {
                (self.module.data_layout.alignments.int_alignment(*bits).abi as usize).max(8) / 8
            }
            Type::PointerType { addr_space } => {
                (self
                    .module
                    .data_layout
                    .alignments
                    .ptr_alignment(*addr_space)
                    .alignment
                    .abi as usize)
                    .max(8)
                    / 8
            }
            Type::ArrayType { element_type, .. } => self.align_of(element_type)?,
            Type::StructType {
                element_types,
                is_packed,
            } => {
                if *is_packed {
                    1
                } else {
                    let mut a = 1;
                    for e in element_types {
                        a = a.max(self.align_of(e)?);
                    }
                    a
                }
            }
            Type::FuncType { .. } => 1,
            t => bail!("unsupported type {}", t),
        })
    }

    /// Allocation size in bytes.
    pub fn size_of(&self, ty: &TypeRef) -> Result<usize> {
        let ty = self.resolve_ref(ty)?;
        Ok(match ty.as_ref() {
            Type::IntegerType { bits } => {
                let raw = (*bits as usize + 7) / 8;
                let a = self.align_of(&ty)?;
                (raw + a - 1) / a * a
            }
            Type::PointerType { .. } => self.ptr_bytes,
            Type::ArrayType {
                element_type,
                num_elements,
            } => self.size_of(element_type)? * num_elements,
            Type::StructType { .. } => self.struct_layout(&ty)?.1,
            Type::VoidType => 0,
            t => bail!("unsupported type {}", t),
        })
    }

    /// Field offsets and total size of a struct type.
    pub fn struct_layout(&self, ty: &TypeRef) -> Result<(Vec<usize>, usize)> {
        let ty = self.resolve_ref(ty)?;
        match ty.as_ref() {
            Type::StructType {
                element_types,
                is_packed,
            } => {
                let mut offsets = Vec::with_capacity(element_types.len());
                let mut off = 0usize;
                let mut max_align = 1usize;
                for e in element_types {
                    let a = if *is_packed { 1 } else { self.align_of(e)? };
                    max_align = max_align.max(a);
                    off = (off + a - 1) / a * a;
                    offsets.push(off);
                    off += self.size_of(e)?;
                }
                off = (off + max_align - 1) / max_align * max_align;
                Ok((offsets, off))
            }
            t => bail!("not a struct type: {}", t),
        }
    }

    /// Element type and byte offset of index `idx` within an aggregate.
    pub fn element(&self, ty: &TypeRef, idx: usize) -> Result<(TypeRef, usize)> {
        let ty = self.resolve_ref(ty)?;
        match ty.as_ref() {
            Type::StructType { element_types, .. } => {
                let (offsets, _) = self.struct_layout(&ty)?;
                if idx >= element_types.len() {
                    bail!("struct index {} out of range", idx);
                }
                Ok((element_types[idx].clone(), offsets[idx]))
            }
            Type::ArrayType { element_type, .. } => {
                Ok((element_type.clone(), idx * self.size_of(element_type)?))
            }
            t => bail!("cannot index into type {}", t),
        }
    }

    /// Element type of an array/vector (for GEP with variable indices).
    pub fn array_element(&self, ty: &TypeRef) -> Result<TypeRef> {
        let ty = self.resolve_ref(ty)?;
        match ty.as_ref() {
            Type::ArrayType { element_type, .. } => Ok(element_type.clone()),
            t => bail!("cannot index into type {} with a variable index", t),
        }
    }

    /// Width in bits of a scalar (integer or pointer) type.
    pub fn scalar_bits(&self, ty: &TypeRef) -> Result<u32> {
        match ty.as_ref() {
            Type::IntegerType { bits } => Ok(*bits),
            Type::PointerType { .. } => Ok(8 * self.ptr_bytes as u32),
            t => bail!("not a scalar type: {}", t),
        }
    }

    pub fn is_scalar(&self, ty: &TypeRef) -> bool {
        matches!(
            ty.as_ref(),
            Type::IntegerType { .. } | Type::PointerType { .. }
        )
    }
}
