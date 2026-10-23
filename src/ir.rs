//! The snapshot IR: a plain, owned, lifetime-free view of an LLVM module that
//! contains exactly what the Brainfuck lowering consumes.
//!
//! It is built once by `loader.rs` from an LLVM module (through `inkwell`) and
//! is the only representation the rest of the compiler ever sees. Everything
//! LLVM-specific that is awkward to consume from Rust is resolved at load time:
//! types are interned with their sizes and offsets already computed by LLVM's
//! `TargetData`, constant expressions are folded to byte offsets, and operands
//! carry their type. The model is deliberately narrow; a construct that is not
//! representable here is rejected by the loader with the LLVM text of the
//! offending value.

use anyhow::{anyhow, bail, Result};
use std::fmt;

/// Index into [`TypeTable`].
pub type TypeId = usize;
/// Index into [`Module::functions`].
pub type FuncId = usize;
/// Index into [`Module::globals`].
pub type GlobalId = usize;
/// Index into [`Function::blocks`].
pub type BlockId = usize;

/// An SSA value of a function (a parameter or an instruction result), numbered
/// per function in definition order.
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug, PartialOrd, Ord)]
pub struct Value(pub u32);

#[derive(Clone, Debug)]
pub struct ValueInfo {
    pub ty: TypeId,
    /// LLVM name, for comments and error messages.
    pub name: String,
}

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum TypeKind {
    Void,
    Int {
        bits: u32,
    },
    Ptr,
    Array {
        elem: TypeId,
        len: usize,
    },
    Struct {
        fields: Vec<Field>,
        packed: bool,
    },
    Func {
        ret: TypeId,
        params: Vec<TypeId>,
        variadic: bool,
    },
    /// Anything the backend does not support (floats, vectors, ...). Kept so that
    /// errors can name the type.
    Unsupported,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Field {
    pub ty: TypeId,
    pub offset: usize,
}

#[derive(Clone, Debug)]
pub struct TypeDef {
    pub kind: TypeKind,
    /// Allocation size in bytes (as LLVM's `TargetData` computes it).
    pub size: usize,
    /// ABI alignment in bytes.
    pub align: usize,
    /// LLVM's textual form, for messages.
    pub name: String,
}

/// All types of a module, interned.
#[derive(Clone, Debug, Default)]
pub struct TypeTable {
    pub types: Vec<TypeDef>,
    pub ptr_bytes: usize,
}

impl TypeTable {
    pub fn def(&self, id: TypeId) -> &TypeDef {
        &self.types[id]
    }
    pub fn kind(&self, id: TypeId) -> &TypeKind {
        &self.types[id].kind
    }
    pub fn size(&self, id: TypeId) -> usize {
        self.types[id].size
    }
    pub fn align(&self, id: TypeId) -> usize {
        self.types[id].align
    }
    pub fn name(&self, id: TypeId) -> &str {
        &self.types[id].name
    }
    pub fn is_void(&self, id: TypeId) -> bool {
        matches!(self.kind(id), TypeKind::Void)
    }
    /// Integers and pointers.
    pub fn is_scalar(&self, id: TypeId) -> bool {
        matches!(self.kind(id), TypeKind::Int { .. } | TypeKind::Ptr)
    }
    /// Width in bits of an integer or pointer type.
    pub fn bits(&self, id: TypeId) -> Result<u32> {
        match self.kind(id) {
            TypeKind::Int { bits } => Ok(*bits),
            TypeKind::Ptr => Ok(8 * self.ptr_bytes as u32),
            _ => bail!("not a scalar type: {}", self.name(id)),
        }
    }
    /// Type and byte offset of element `idx` of a struct or array.
    pub fn element(&self, id: TypeId, idx: usize) -> Result<(TypeId, usize)> {
        match self.kind(id) {
            TypeKind::Struct { fields, .. } => fields
                .get(idx)
                .map(|f| (f.ty, f.offset))
                .ok_or_else(|| anyhow!("field index {} out of range in {}", idx, self.name(id))),
            TypeKind::Array { elem, .. } => Ok((*elem, idx * self.size(*elem))),
            _ => bail!("cannot index into type {}", self.name(id)),
        }
    }
    /// Element type of an array (for variable indices).
    pub fn array_elem(&self, id: TypeId) -> Result<TypeId> {
        match self.kind(id) {
            TypeKind::Array { elem, .. } => Ok(*elem),
            _ => bail!(
                "cannot index into type {} with a variable index",
                self.name(id)
            ),
        }
    }
    /// Parameter types and variadic flag of a function type.
    pub fn func_sig(&self, id: TypeId) -> Result<(&[TypeId], bool)> {
        match self.kind(id) {
            TypeKind::Func {
                params, variadic, ..
            } => Ok((params, *variadic)),
            _ => bail!("not a function type: {}", self.name(id)),
        }
    }
}

// ---------------------------------------------------------------------------
// Constants and operands
// ---------------------------------------------------------------------------

#[derive(Clone, Debug, PartialEq)]
pub enum Constant {
    /// An integer, zero-extended to 64 bits (also used for `i1`).
    Int(u64),
    /// `null`, `undef`, `poison`, `zeroinitializer` — all bytes zero.
    Zero,
    /// The address of a global variable.
    Global(GlobalId),
    /// A function used as a value (a function pointer).
    Func(FuncId),
    /// A constant `getelementptr`: `base` plus a byte offset.
    Offset { base: Box<Constant>, bytes: i64 },
    /// A constant data array of integers, as its in-memory bytes.
    Bytes(Vec<u8>),
    /// A struct or array constant: scalar leaves at byte offsets (padding is zero).
    Agg(Vec<AggLeaf>),
}

#[derive(Clone, Debug, PartialEq)]
pub struct AggLeaf {
    pub offset: usize,
    pub ty: TypeId,
    pub value: Constant,
}

#[derive(Clone, Debug, PartialEq)]
pub enum OperandKind {
    Value(Value),
    Const(Constant),
}

/// A typed operand.
#[derive(Clone, Debug, PartialEq)]
pub struct Operand {
    pub ty: TypeId,
    pub kind: OperandKind,
}

impl Operand {
    pub fn value(ty: TypeId, v: Value) -> Self {
        Operand {
            ty,
            kind: OperandKind::Value(v),
        }
    }
    pub fn constant(ty: TypeId, c: Constant) -> Self {
        Operand {
            ty,
            kind: OperandKind::Const(c),
        }
    }
    pub fn as_value(&self) -> Option<Value> {
        match &self.kind {
            OperandKind::Value(v) => Some(*v),
            _ => None,
        }
    }
    pub fn as_const(&self) -> Option<&Constant> {
        match &self.kind {
            OperandKind::Const(c) => Some(c),
            _ => None,
        }
    }
}

// ---------------------------------------------------------------------------
// Instructions
// ---------------------------------------------------------------------------

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    UDiv,
    SDiv,
    URem,
    SRem,
    And,
    Or,
    Xor,
    Shl,
    LShr,
    AShr,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum CastKind {
    ZExt,
    SExt,
    Trunc,
    /// `ptrtoint`, `inttoptr`, `bitcast`, `addrspacecast`: the bits are kept,
    /// zero-extended or truncated to the destination width.
    Reinterpret,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Pred {
    Eq,
    Ne,
    Ult,
    Ule,
    Ugt,
    Uge,
    Slt,
    Sle,
    Sgt,
    Sge,
}

impl Pred {
    pub fn is_signed(self) -> bool {
        matches!(self, Pred::Slt | Pred::Sle | Pred::Sgt | Pred::Sge)
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum Callee {
    /// A function of this module (possibly only declared).
    Direct(FuncId),
    /// A call through a function pointer.
    Indirect(Operand),
    /// An LLVM intrinsic (`llvm.*`) or an llvbf builtin (`__llvbf_*`), by name.
    Intrinsic(String),
}

#[derive(Clone, Debug, PartialEq)]
pub enum Op {
    Binary {
        kind: BinOp,
        a: Operand,
        b: Operand,
    },
    ICmp {
        pred: Pred,
        a: Operand,
        b: Operand,
    },
    Cast {
        kind: CastKind,
        x: Operand,
    },
    Load {
        addr: Operand,
        align: usize,
    },
    Store {
        addr: Operand,
        value: Operand,
        align: usize,
    },
    Gep {
        base: Operand,
        source_ty: TypeId,
        indices: Vec<Operand>,
    },
    Call {
        callee: Callee,
        fn_ty: TypeId,
        args: Vec<Operand>,
    },
    Alloca {
        ty: TypeId,
        count: u64,
        align: usize,
    },
    Select {
        cond: Operand,
        if_true: Operand,
        if_false: Operand,
    },
    ExtractValue {
        agg: Operand,
        indices: Vec<u32>,
    },
    InsertValue {
        agg: Operand,
        elem: Operand,
        indices: Vec<u32>,
    },
    VaArg {
        list: Operand,
    },
    Freeze {
        x: Operand,
    },
}

#[derive(Clone, Debug)]
pub struct Instr {
    /// The defined value, if the instruction produces one.
    pub dest: Option<Value>,
    /// Type of the result (`Void` for none).
    pub ty: TypeId,
    pub op: Op,
    /// LLVM's textual form, for comments and error messages.
    pub text: String,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Terminator {
    Ret(Option<Operand>),
    Br(BlockId),
    CondBr {
        cond: Operand,
        if_true: BlockId,
        if_false: BlockId,
    },
    Switch {
        value: Operand,
        cases: Vec<(u64, BlockId)>,
        default: BlockId,
    },
    Unreachable,
}

#[derive(Clone, Debug)]
pub struct Phi {
    pub dest: Value,
    pub ty: TypeId,
    pub incoming: Vec<(Operand, BlockId)>,
}

#[derive(Clone, Debug)]
pub struct Block {
    pub name: String,
    /// Phis come first in a block; they are kept separately from the instructions.
    pub phis: Vec<Phi>,
    pub instrs: Vec<Instr>,
    pub term: Terminator,
    pub term_text: String,
}

#[derive(Clone, Debug)]
pub struct Function {
    pub name: String,
    /// The function's own type (a `TypeKind::Func`).
    pub ty: TypeId,
    pub params: Vec<Value>,
    pub ret: TypeId,
    pub variadic: bool,
    pub values: Vec<ValueInfo>,
    /// Empty for declarations.
    pub blocks: Vec<Block>,
}

impl Function {
    pub fn is_declaration(&self) -> bool {
        self.blocks.is_empty()
    }
    pub fn value_ty(&self, v: Value) -> TypeId {
        self.values[v.0 as usize].ty
    }
    pub fn value_name(&self, v: Value) -> &str {
        &self.values[v.0 as usize].name
    }
}

#[derive(Clone, Debug)]
pub struct Global {
    pub name: String,
    /// Type of the stored value (not the pointer).
    pub ty: TypeId,
    pub align: usize,
    /// `None` for external declarations.
    pub init: Option<Constant>,
}

#[derive(Clone, Debug)]
pub struct Module {
    pub types: TypeTable,
    pub functions: Vec<Function>,
    pub globals: Vec<Global>,
    pub target_triple: String,
}

impl Module {
    pub fn func_by_name(&self, name: &str) -> Option<FuncId> {
        self.functions.iter().position(|f| f.name == name)
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "%{}", self.0)
    }
}
