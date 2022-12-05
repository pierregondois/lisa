use core::{fmt, ops::Deref};
use std::sync::Arc;

use bytemuck::cast_slice;
use thiserror::Error;

use crate::{
    array::Array,
    buffer::BufferError,
    cparser,
    cparser::{ArrayKind, Expr, ExtensionMacroKind, Type},
    header::{Abi, Endianness, FileSize, Header, Identifier, LongSize, Signedness},
    print::{PrintError, PrintFmtError},
    scratch::{OwnedScratchBox, ScratchAlloc, ScratchBox},
    str::Str,
};

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompileError {
    #[error("Cannot this handle expression in its context: {0:?}")]
    CannotHandleExpr(Expr),

    #[error("Cannot dereference an expression of type {0:?}: {1:?}")]
    CannotDeref(Type, Expr),

    #[error("Not an array: {0:?}")]
    NotAnArray(Type),

    #[error("Type not supported as array item: {0:?}")]
    InvalidArrayItem(Type),

    #[error("Size of this type is unknown: {0:?}")]
    UnknownSize(Type),

    #[error("Non arithmetic operand used with arithmetic operator")]
    NonArithmeticOperand,

    #[error("Mismatching types in operands of {0:?}: {1:?} and {2:?}")]
    MismatchingOperandType(Expr, Type, Type),

    #[error("Cannot cast arithmetic type on pointer or vice versa")]
    CastPointerArith,

    #[error("Cannot cast between incompatible pointer types: {0:?} => {1:?}")]
    IncompatiblePointerCast(Type, Type),

    #[error("This field does not exist")]
    UnknownField,

    #[error("Expression could not be simplified")]
    CannotSimplify,
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum EvalError {
    #[error("Illegal type")]
    IllegalType,

    #[error("Cannot convert this value to a signed as it is too big: {0}")]
    CannotConvertToSigned(u64),

    #[error("Attempted to index a scalar value")]
    CannotIndexScalar,

    #[error("Array index out of bonds")]
    OutOfBondIndex,

    #[error("Could not dereference pointer as it points to unknown data")]
    CannotDeref,

    #[error("Event data not available")]
    NoEventData,

    #[error("Error while decoding buffer: {0}")]
    BufferError(Box<BufferError>),

    #[error("Error while parsing a vbin buffer format: {0}")]
    PrintFmtError(Box<PrintFmtError>),

    #[error("Error while evaluating a vbin buffer: {0}")]
    PrintError(Box<PrintError>),

    #[error("No header available")]
    NoHeader,
}

impl From<BufferError> for EvalError {
    fn from(x: BufferError) -> EvalError {
        EvalError::BufferError(Box::new(x))
    }
}

impl From<PrintError> for EvalError {
    fn from(x: PrintError) -> EvalError {
        EvalError::PrintError(Box::new(x))
    }
}

impl From<PrintFmtError> for EvalError {
    fn from(x: PrintFmtError) -> EvalError {
        EvalError::PrintFmtError(Box::new(x))
    }
}

impl From<fmt::Error> for EvalError {
    fn from(x: fmt::Error) -> EvalError {
        EvalError::PrintError(Box::new(x.into()))
    }
}

#[derive(Error, Debug, Clone, PartialEq, Eq)]
pub enum InterpError {
    #[error("Could not compile: {0}")]
    CompileError(Box<CompileError>),
    #[error("Could not evaluate: {0}")]
    EvalError(Box<EvalError>),
}

impl From<EvalError> for InterpError {
    fn from(x: EvalError) -> InterpError {
        InterpError::EvalError(Box::new(x))
    }
}

impl From<CompileError> for InterpError {
    fn from(x: CompileError) -> InterpError {
        InterpError::CompileError(Box::new(x))
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum SockAddrFamily {
    Ipv4,
    Ipv6,
}

impl SockAddrFamily {
    #[inline]
    fn from_raw(code: u16) -> Result<Self, BufferError> {
        match code {
            2 => Ok(SockAddrFamily::Ipv4),
            10 => Ok(SockAddrFamily::Ipv6),
            _ => Err(BufferError::UnknownSockAddrFamily(code)),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SockAddrKind {
    Full,
    Ipv4AddrOnly,
    Ipv6AddrOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SockAddr<'a> {
    family: SockAddrFamily,
    kind: SockAddrKind,
    endianness: Endianness,
    data: &'a [u8],
}

#[derive(thiserror::Error, Debug, PartialEq, Eq, Clone)]
#[non_exhaustive]
pub enum SockAddrError {
    #[error("Could not convert value")]
    CannotConvert,
}

macro_rules! get_array {
    ($slice:expr, $len:expr) => {{
        let slice: &[u8] = $slice;
        let slice = slice.get(..$len).ok_or(SockAddrError::CannotConvert)?;
        let arr: [u8; $len] = slice.try_into().map_err(|_| SockAddrError::CannotConvert)?;
        arr
    }};
}

impl<'a> SockAddr<'a> {
    #[inline]
    pub fn from_bytes(
        data: &'a [u8],
        endianness: Endianness,
        kind: SockAddrKind,
    ) -> Result<Self, BufferError> {
        let family = match kind {
            SockAddrKind::Full => {
                let (_data, family) = endianness
                    .parse_u16(data)
                    .map_err(|_| BufferError::SockAddrTooSmall)?;
                SockAddrFamily::from_raw(family)
            }
            SockAddrKind::Ipv4AddrOnly => Ok(SockAddrFamily::Ipv4),
            SockAddrKind::Ipv6AddrOnly => Ok(SockAddrFamily::Ipv6),
        }?;

        Ok(SockAddr {
            family,
            kind,
            data,
            endianness,
        })
    }

    // Format of the structs described at:
    // https://www.gnu.org/software/libc/manual/html_node/Internet-Address-Formats.html
    // The order of struct members is different in the kernel struct.

    pub fn to_socketaddr(&self) -> Result<std::net::SocketAddr, SockAddrError> {
        match (&self.kind, &self.family) {
            (SockAddrKind::Full, SockAddrFamily::Ipv4) => {
                let port = u16::from_be_bytes(get_array!(&self.data[2..], 2));

                // The kernel structs use network endianness but the user
                // might pass a little endian buffer and ask for that
                // explicitly.
                let (_, addr) = self
                    .endianness
                    .parse_u32(&self.data[4..])
                    .map_err(|_| SockAddrError::CannotConvert)?;

                Ok(std::net::SocketAddr::V4(std::net::SocketAddrV4::new(
                    addr.into(),
                    port,
                )))
            }
            (SockAddrKind::Full, SockAddrFamily::Ipv6) => {
                let port = u16::from_be_bytes(get_array!(&self.data[2..], 2));
                let flowinfo = u32::from_be_bytes(get_array!(&self.data[4..], 4));
                let addr = u128::from_be_bytes(get_array!(&self.data[8..], 16));
                let (_, scope_id) = self
                    .endianness
                    .parse_u32(&self.data[24..])
                    .map_err(|_| SockAddrError::CannotConvert)?;

                Ok(std::net::SocketAddr::V6(std::net::SocketAddrV6::new(
                    addr.into(),
                    port,
                    flowinfo,
                    scope_id,
                )))
            }
            _ => Err(SockAddrError::CannotConvert),
        }
    }

    pub fn to_ipaddr(&self) -> Result<std::net::IpAddr, SockAddrError> {
        match self.to_socketaddr() {
            Ok(sockaddr) => Ok(sockaddr.ip()),
            _ => match (&self.kind, &self.family) {
                (SockAddrKind::Ipv4AddrOnly, SockAddrFamily::Ipv4) => {
                    // The kernel structs use network endianness but the user
                    // might pass a little endian buffer and ask for that
                    // explicitly.
                    let (_, addr) = self
                        .endianness
                        .parse_u32(self.data)
                        .map_err(|_| SockAddrError::CannotConvert)?;
                    let addr: std::net::Ipv4Addr = addr.into();
                    Ok(addr.into())
                }

                (SockAddrKind::Ipv6AddrOnly, SockAddrFamily::Ipv6) => {
                    let data = get_array!(&self.data, 16);
                    // struct in6_addr is always encoded in big endian. The
                    // h/n/b/l printk specifiers are documented to be ignored
                    // for IPv6
                    let addr = u128::from_be_bytes(data);
                    let addr: std::net::Ipv6Addr = addr.into();
                    Ok(addr.into())
                }
                _ => panic!("Inconsistent sockaddr kind and family"),
            },
        }
    }
}

impl<'a> fmt::Display for SockAddr<'a> {
    #[inline]
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        match self.to_socketaddr() {
            Ok(addr) => fmt::Display::fmt(&addr, f),
            Err(err) => write!(f, "ERROR<{err:?}>"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Bitmap<'a> {
    data: &'a [u8],
    pub(crate) chunk_size: LongSize,
    endianness: Endianness,
}

impl<'a> Bitmap<'a> {
    #[inline]
    pub(crate) fn from_bytes<'abi>(data: &'a [u8], abi: &'abi Abi) -> Self {
        let chunk_size: usize = abi.long_size.into();
        assert!(data.len() % chunk_size == 0);
        Bitmap {
            data,
            chunk_size: abi.long_size,
            endianness: abi.endianness,
        }
    }

    #[inline]
    pub(crate) fn len(&self) -> usize {
        self.data.len()
    }
}

impl<'a> fmt::Display for Bitmap<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        let mut range_start = None;
        let mut prev = None;
        let mut sep = "";

        let mut print_range = |range_start, prev, sep| match range_start {
            Some(range_start) if range_start == prev => {
                write!(f, "{sep}{prev}")
            }
            None => write!(f, "{sep}{prev}"),
            Some(range_start) => {
                write!(f, "{sep}{range_start}-{prev}")
            }
        };

        for curr in self {
            match prev {
                None => range_start = Some(curr),
                Some(prev) => {
                    if curr != prev + 1 {
                        print_range(range_start, prev, sep)?;
                        sep = ",";
                        range_start = Some(curr);
                    }
                }
            };
            prev = Some(curr);
        }
        if let Some(prev) = prev {
            print_range(range_start, prev, sep)?
        }
        Ok(())
    }
}

impl<'a> IntoIterator for &'a Bitmap<'a> {
    type Item = <BitmapIterator<'a> as Iterator>::Item;
    type IntoIter = BitmapIterator<'a>;

    #[inline]
    fn into_iter(self) -> Self::IntoIter {
        BitmapIterator {
            bitmap: self,
            curr_chunk: None,
            next_chunk_index: 0,
            bit_index: 0,
        }
    }
}

pub struct BitmapIterator<'a> {
    bitmap: &'a Bitmap<'a>,

    curr_chunk: Option<u64>,
    next_chunk_index: usize,
    bit_index: usize,
}

impl<'a> BitmapIterator<'a> {
    fn next_chunk(&mut self) -> Option<u64> {
        let chunk_size = self.bitmap.chunk_size;
        let chunk_usize: usize = chunk_size.into();
        let data = self.bitmap.data;
        let base = self.next_chunk_index * chunk_usize;
        if base < data.len() {
            self.bit_index = 0;
            self.next_chunk_index += 1;
            let chunk = &data[base..base + chunk_usize];

            Some(match chunk_size {
                LongSize::Bits64 => {
                    let chunk = chunk.try_into().unwrap();
                    match self.bitmap.endianness {
                        Endianness::Little => u64::from_le_bytes(chunk),
                        Endianness::Big => u64::from_be_bytes(chunk),
                    }
                }
                LongSize::Bits32 => {
                    let chunk = chunk.try_into().unwrap();
                    match self.bitmap.endianness {
                        Endianness::Little => u32::from_le_bytes(chunk) as u64,
                        Endianness::Big => u32::from_be_bytes(chunk) as u64,
                    }
                }
            })
        } else {
            None
        }
    }

    #[inline]
    pub fn as_chunks(&'a mut self) -> impl Iterator<Item = u64> + 'a {
        core::iter::from_fn(move || self.next_chunk())
    }
}

impl<'a> Iterator for BitmapIterator<'a> {
    type Item = usize;

    fn next(&mut self) -> Option<Self::Item> {
        let chunk_size: usize = self.bitmap.chunk_size.into();
        loop {
            match self.curr_chunk {
                Some(chunk) => {
                    if self.bit_index < chunk_size - 1 {
                        let bit_index = self.bit_index;
                        self.bit_index += 1;

                        let is_set = (chunk & (1 << bit_index)) != 0;
                        if is_set {
                            let global_index = bit_index + (self.next_chunk_index - 1) * chunk_size;
                            break Some(global_index);
                        }
                    } else {
                        self.curr_chunk = Some(self.next_chunk()?);
                    }
                }
                None => {
                    self.curr_chunk = Some(self.next_chunk()?);
                }
            }
        }
    }
}

// Pointers fall into 3 categories:
// 1. Pointers to unknown values. They are represented by an Value::U64Scalar at
//    runtime.
// 2. Pointers to known arrays, e.g. char* pointing at string constants. They
//    are represented by Value::XXArray at runtime.
// 3. Pointers to known scalar values: this does not happen often in trace.dat.
//    They are only currently supported when appearing in "*&x" that gets
//    simplified into "x", or as &x (evaluates to a symbolic address).

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Value<'a> {
    U64Scalar(u64),
    I64Scalar(i64),

    // Similar to U8Array but will act as a null-terminated string, and is
    // guaranteed to be utf-8 encoded.
    Str(Str<'a>),

    U8Array(Array<'a, u8>),
    I8Array(Array<'a, i8>),

    U16Array(Array<'a, u16>),
    I16Array(Array<'a, i16>),

    U32Array(Array<'a, u32>),
    I32Array(Array<'a, i32>),

    U64Array(Array<'a, u64>),
    I64Array(Array<'a, i64>),

    // Variable, usually REC, and usually erased at compile time when it appears
    // in the pattern REC->foobar
    Variable(Identifier),

    // Symbolic address of a value
    Addr(ScratchBox<'a, Value<'a>>),

    // Kernel bitmap, such as cpumask_t
    Bitmap(Bitmap<'a>),

    // Kernel struct sockaddr. We don't use std::net::SockAddr as we need to be
    // able to represent any socket type the kernel can handle, which goes
    // beyond IP. Also, we want a zero-copy value.
    SockAddr(SockAddr<'a>),

    // Used for runtime decoding of event field values.
    Raw(Arc<Type>, Array<'a, u8>),
    Unknown,
}

impl<'a> Value<'a> {
    #[inline]
    pub fn deref_ptr<'ee, EE>(&'a self, env: &'ee EE) -> Result<Value<'a>, EvalError>
    where
        'ee: 'a,
        EE: EvalEnv<'ee> + ?Sized,
    {
        match self {
            Value::Addr(sbox) => Ok(sbox.deref().clone()),
            Value::Str(s) => Ok(Value::Str(Str::new_borrowed(s))),

            Value::U64Scalar(addr) => env.deref_static(*addr),
            Value::I64Scalar(addr) => env.deref_static(*addr as u64),

            Value::U8Array(arr) => Ok(Value::U8Array(Array::Borrowed(arr))),
            Value::I8Array(arr) => Ok(Value::I8Array(Array::Borrowed(arr))),

            Value::U16Array(arr) => Ok(Value::U16Array(Array::Borrowed(arr))),
            Value::I16Array(arr) => Ok(Value::I16Array(Array::Borrowed(arr))),

            Value::U32Array(arr) => Ok(Value::U32Array(Array::Borrowed(arr))),
            Value::I32Array(arr) => Ok(Value::I32Array(Array::Borrowed(arr))),

            Value::U64Array(arr) => Ok(Value::U64Array(Array::Borrowed(arr))),
            Value::I64Array(arr) => Ok(Value::I64Array(Array::Borrowed(arr))),
            _ => Err(EvalError::IllegalType),
        }
    }

    pub fn to_bytes(&self) -> Option<impl Iterator<Item = u8> + '_> {
        use Value::*;

        let (add_null, slice) = match self {
            Str(s) => Some((true, s.as_bytes())),
            Raw(_, arr) => Some((false, arr.deref())),

            U8Array(arr) => Some((false, arr.deref())),
            I8Array(arr) => Some((false, cast_slice(arr))),

            U16Array(arr) => Some((false, cast_slice(arr))),
            I16Array(arr) => Some((false, cast_slice(arr))),

            U32Array(arr) => Some((false, cast_slice(arr))),
            I32Array(arr) => Some((false, cast_slice(arr))),

            U64Array(arr) => Some((false, cast_slice(arr))),
            I64Array(arr) => Some((false, cast_slice(arr))),
            _ => None,
        }?;
        let mut iter = slice.iter();
        Some(core::iter::from_fn(move || match iter.next().copied() {
            Some(x) => Some(x),
            None if add_null => Some(0),
            _ => None,
        }))
    }

    pub fn to_str(&self) -> Option<&str> {
        macro_rules! from_array {
            ($s:expr) => {
                if let Some(s) = $s.split(|c| *c == 0).next() {
                    if let Ok(s) = std::str::from_utf8(s) {
                        Some(s)
                    } else {
                        None
                    }
                } else {
                    None
                }
            };
        }
        match self {
            Value::U8Array(s) => from_array!(s),
            Value::I8Array(s) => from_array!(cast_slice(s)),
            Value::Str(s) => Some(s),
            _ => None,
        }
    }

    pub fn into_static(self) -> Result<Value<'static>, Value<'a>> {
        use Value::*;

        macro_rules! array {
            ($variant:ident, $arr:expr) => {
                Ok($variant($arr.into_static()))
            };
        }
        match self {
            U64Scalar(x) => Ok(U64Scalar(x)),
            I64Scalar(x) => Ok(I64Scalar(x)),

            Str(s) => Ok(Str(s.into_static())),

            U8Array(arr) => array!(U8Array, arr),
            I8Array(arr) => array!(I8Array, arr),

            U16Array(arr) => array!(U16Array, arr),
            I16Array(arr) => array!(I16Array, arr),

            U32Array(arr) => array!(U32Array, arr),
            I32Array(arr) => array!(I32Array, arr),

            U64Array(arr) => array!(U64Array, arr),
            I64Array(arr) => array!(I64Array, arr),

            Raw(typ, arr) => Ok(Raw(typ, arr.into_static())),
            Addr(addr) => {
                let addr = addr.deref().clone();
                let addr = addr.into_static()?;
                Ok(Addr(ScratchBox::Arc(Arc::new(addr))))
            }
            Variable(id) => Ok(Variable(id)),
            Unknown => Ok(Unknown),

            // The only bitmaps that exist are created by the kernel and stored
            // in a field, they are never synthesized by any expression that
            // could be evaluated ahead of time.
            bitmap @ Bitmap(_) => Err(bitmap),
            sockaddr @ SockAddr(_) => Err(sockaddr),
        }
    }

    fn get<EE: EvalEnv<'a> + ?Sized>(
        self,
        env: &'a EE,
        i: usize,
    ) -> Result<Value<'a>, (Value<'a>, EvalError)> {
        macro_rules! match_ {
            ($(($array_ctor:tt, $scalar_ctor:tt)),*) => {
                match self {
                    $(
                        Value::$array_ctor(vec) => {
                            match vec.deref().get(i) {
                                None => Err((Value::$array_ctor(vec), EvalError::OutOfBondIndex)),
                                Some(x) => Ok(Value::$scalar_ctor(x.clone().into()))
                            }
                        }
                    ),*
                    Value::Str(s) => {
                        match s.as_bytes().get(i) {
                            None => {
                                if i == s.len() {
                                    Ok(Value::U64Scalar(0))
                                } else {
                                    Err((Value::Str(s), EvalError::OutOfBondIndex))
                                }
                            }
                            Some(c) => Ok(Value::U64Scalar((*c).into())),
                        }
                    }
                    Value::U64Scalar(addr) => env.deref_static(addr).map_err(|err| (Value::U64Scalar(addr), err))?.get(env, i),
                    Value::I64Scalar(addr) => {
                        env.deref_static(addr as u64).map_err(|err| (Value::I64Scalar(addr), err))?.get(env, i)
                    },
                    Value::Addr(val) => {
                        if i == 0 {
                            Ok(val.into_inner())
                        } else {
                            Err((Value::Addr(val), EvalError::OutOfBondIndex))
                        }
                    },
                    val => Err((val, EvalError::CannotIndexScalar))
                }
            }
        }

        match_! {
            (I8Array, I64Scalar),
            (U8Array, U64Scalar),

            (I16Array, I64Scalar),
            (U16Array, U64Scalar),

            (I32Array, I64Scalar),
            (U32Array, U64Scalar),

            (I64Array, I64Scalar),
            (U64Array, U64Scalar)
        }
    }
}

impl<'a> fmt::Display for Value<'a> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> Result<(), fmt::Error> {
        macro_rules! display {
            ($x:expr) => {{
                fmt::Display::fmt(&$x, f)
            }};
        }

        match self {
            Value::U64Scalar(x) => display!(x),
            Value::I64Scalar(x) => display!(x),
            Value::Str(x) => display!(x),
            Value::U8Array(x) => display!(x),
            Value::I8Array(x) => display!(x),
            Value::U16Array(x) => display!(x),
            Value::I16Array(x) => display!(x),
            Value::U32Array(x) => display!(x),
            Value::I32Array(x) => display!(x),
            Value::U64Array(x) => display!(x),
            Value::I64Array(x) => display!(x),
            Value::Variable(x) => display!(x),
            Value::Addr(x) => write!(f, "<ADDRESS OFF<{}>>", x.deref()),
            Value::Bitmap(x) => display!(x),
            Value::SockAddr(x) => display!(x),
            Value::Raw(typ, data) => {
                write!(f, "<RAW DATA {typ:?} [")?;
                for (i, x) in data.iter().enumerate() {
                    if i != 0 {
                        write!(f, ",")?;
                    }
                    write!(f, "{x:#04x}")?;
                }
                write!(f, "]>")?;
                Ok(())
            }
            Value::Unknown => write!(f, "<UNKNOWN>"),
        }
    }
}

pub trait CompileEnv<'ce>: EvalEnv<'ce>
where
    Self: 'ce,
{
    fn field_typ(&self, id: &str) -> Result<Type, CompileError>;
    fn field_getter(&self, id: &str) -> Result<Box<dyn Evaluator>, CompileError>;
}

pub trait EvalEnv<'ee>
where
    Self: 'ee + Send + Sync,
{
    // fn field_getter<EE: EvalEnv>(&self, id: &str) -> Result<Box<dyn Fn(&EE) -> Result<Value, EvalError>>, CompileError>;
    fn deref_static(&self, _addr: u64) -> Result<Value<'_>, EvalError>;
    fn event_data(&self) -> Result<&[u8], EvalError> {
        Err(EvalError::NoEventData)
    }

    fn scratch(&self) -> &ScratchAlloc;

    fn header(&self) -> Result<&Header, EvalError>;
}

impl<'ee, 'eeref> EvalEnv<'eeref> for &'eeref (dyn CompileEnv<'ee> + 'ee) {
    #[inline]
    fn deref_static(&self, addr: u64) -> Result<Value<'_>, EvalError> {
        (*self).deref_static(addr)
    }

    #[inline]
    fn event_data(&self) -> Result<&[u8], EvalError> {
        (*self).event_data()
    }

    #[inline]
    fn scratch(&self) -> &ScratchAlloc {
        (*self).scratch()
    }

    #[inline]
    fn header(&self) -> Result<&Header, EvalError> {
        (*self).header()
    }
}

impl<'ce, 'ceref> CompileEnv<'ceref> for &'ceref (dyn CompileEnv<'ce> + 'ce) {
    fn field_typ(&self, id: &str) -> Result<Type, CompileError> {
        (*self).field_typ(id)
    }
    fn field_getter(&self, id: &str) -> Result<Box<dyn Evaluator>, CompileError> {
        (*self).field_getter(id)
    }
}

pub struct EmptyEnv {
    scratch: ScratchAlloc,
}

impl<'ee> EvalEnv<'ee> for EmptyEnv {
    #[inline]
    fn scratch(&self) -> &ScratchAlloc {
        &self.scratch
    }

    fn header(&self) -> Result<&Header, EvalError> {
        Err(EvalError::NoHeader)
    }

    fn deref_static(&self, _addr: u64) -> Result<Value<'_>, EvalError> {
        Err(EvalError::CannotDeref)
    }
}

impl Default for EmptyEnv {
    fn default() -> Self {
        Self::new()
    }
}

impl EmptyEnv {
    pub fn new() -> Self {
        EmptyEnv {
            scratch: ScratchAlloc::new(),
        }
    }
}

impl<'ce> CompileEnv<'ce> for EmptyEnv {
    #[inline]
    fn field_typ(&self, _id: &str) -> Result<Type, CompileError> {
        Ok(Type::Unknown)
    }

    #[inline]
    fn field_getter(&self, _id: &str) -> Result<Box<dyn Evaluator>, CompileError> {
        Err(CompileError::UnknownField)
    }
}

pub struct BufferEnv<'a> {
    scratch: &'a ScratchAlloc,
    header: &'a Header,
    data: &'a [u8],
}
impl<'a> BufferEnv<'a> {
    pub fn new(scratch: &'a ScratchAlloc, header: &'a Header, data: &'a [u8]) -> Self {
        BufferEnv {
            scratch,
            header,
            data,
        }
    }
}

impl<'ee> EvalEnv<'ee> for BufferEnv<'ee> {
    #[inline]
    fn scratch(&self) -> &ScratchAlloc {
        self.scratch
    }

    #[inline]
    fn deref_static(&self, addr: u64) -> Result<Value<'_>, EvalError> {
        self.header.deref_static(addr)
    }

    fn event_data(&self) -> Result<&[u8], EvalError> {
        Ok(self.data)
    }

    fn header(&self) -> Result<&Header, EvalError> {
        Ok(self.header)
    }
}

pub struct ArithInfo<'a> {
    typ: &'a Type,
    rank: u32,
    width: FileSize,
    signed: Type,
    unsigned: Type,
}

impl<'a> ArithInfo<'a> {
    #[inline]
    pub fn is_signed(&self) -> bool {
        self.typ == &self.signed
    }

    #[inline]
    pub fn signedness(&self) -> Signedness {
        if self.is_signed() {
            Signedness::Signed
        } else {
            Signedness::Unsigned
        }
    }
}

impl Type {
    pub fn arith_info(&self) -> Option<ArithInfo> {
        let typ = self.resolve_wrapper();

        use Type::*;
        match typ {
            U8 | I8 | Bool => Some(ArithInfo {
                typ,
                rank: 0,
                signed: I8,
                unsigned: U8,
                width: 8,
            }),
            U16 | I16 => Some(ArithInfo {
                typ,
                rank: 1,
                signed: I16,
                unsigned: U16,
                width: 16,
            }),
            U32 | I32 => Some(ArithInfo {
                typ,
                rank: 2,
                signed: I32,
                unsigned: U32,
                width: 32,
            }),
            U64 | I64 => Some(ArithInfo {
                typ,
                rank: 3,
                signed: I64,
                unsigned: U64,
                width: 64,
            }),
            _ => None,
        }
    }

    pub fn promote(self) -> Type {
        match self.arith_info() {
            Some(info) => {
                if info.width <= 32 {
                    if info.is_signed() {
                        Type::I32
                    } else {
                        Type::U32
                    }
                } else {
                    self
                }
            }
            None => self,
        }
    }

    pub fn resolve_wrapper(&self) -> &Self {
        match self {
            Type::Typedef(typ, _) | Type::Enum(typ, _) => typ.resolve_wrapper(),
            _ => self,
        }
    }

    pub fn decay_to_ptr(self) -> Type {
        match self {
            Type::Array(typ, ..) => Type::Pointer(typ),
            typ => typ,
        }
    }
}

type ArithConverter = dyn for<'a> Fn(Value<'a>) -> Result<Value<'a>, EvalError> + Send + Sync;

#[inline]
fn convert_arith(dst: &Type) -> Result<Box<ArithConverter>, CompileError> {
    macro_rules! convert {
        ($typ:ty, $ctor:ident) => {
            Ok(Box::new(|x| {
                let x = match x {
                    Value::I64Scalar(x) => x as $typ,
                    Value::U64Scalar(x) => x as $typ,
                    _ => return Err(EvalError::IllegalType),
                };
                Ok(Value::$ctor(x.into()))
            }))
        };
    }

    use Type::*;
    match dst.resolve_wrapper() {
        Bool => convert!(u8, U64Scalar),
        I8 => convert!(i8, I64Scalar),
        U8 => convert!(u8, U64Scalar),
        I16 => convert!(i16, I64Scalar),
        U16 => convert!(u16, U64Scalar),
        I32 => convert!(i32, I64Scalar),
        U32 => convert!(u32, U64Scalar),
        I64 => convert!(i64, I64Scalar),
        U64 => convert!(u64, U64Scalar),
        _ => Err(CompileError::NonArithmeticOperand),
    }
}

fn usual_arith_conv(lhs: Type, rhs: Type) -> Result<Type, CompileError> {
    let lhs = lhs.promote();
    let rhs = rhs.promote();

    match (lhs.arith_info(), rhs.arith_info()) {
        (Some(lhs_info), Some(rhs_info)) => Ok({
            if lhs == rhs {
                lhs
            } else if lhs_info.is_signed() == rhs_info.is_signed() {
                if lhs_info.rank > rhs_info.rank {
                    lhs
                } else {
                    rhs
                }
            } else {
                let (styp, styp_info, _utyp, utyp_info) = if lhs_info.is_signed() {
                    (&lhs, lhs_info, &rhs, rhs_info)
                } else {
                    (&rhs, rhs_info, &lhs, lhs_info)
                };

                if styp_info.width > utyp_info.width + 1 {
                    styp.clone()
                } else {
                    styp_info.unsigned
                }
            }
        }),
        _ => Err(CompileError::NonArithmeticOperand),
    }
}

#[inline]
fn convert_arith_ops(
    _abi: &Abi,
    lhs: Type,
    rhs: Type,
) -> Result<(Type, Box<ArithConverter>, Box<ArithConverter>), CompileError> {
    let typ = usual_arith_conv(lhs.clone(), rhs.clone())?;
    Ok((typ.clone(), convert_arith(&typ)?, convert_arith(&typ)?))
}

#[inline]
fn convert_arith_op<'ce, CE>(
    abi: &Abi,
    cenv: &CE,
    expr: &Expr,
) -> Result<(Type, Box<ArithConverter>), CompileError>
where
    CE: CompileEnv<'ce>,
{
    let typ = expr.typ(cenv, abi)?;
    let promoted = typ.promote();
    Ok((promoted.clone(), convert_arith(&promoted)?))
}

impl Type {
    pub fn size(&self, abi: &Abi) -> Result<FileSize, CompileError> {
        let typ = self.resolve_wrapper();
        use Type::*;
        match typ {
            Pointer(_) => Ok(abi.long_size.into()),
            Array(typ, size) => match size {
                ArrayKind::Fixed(Ok(size)) => {
                    let item = typ.size(abi)?;
                    Ok(size * item)
                }
                _ => Err(CompileError::UnknownSize(*typ.clone())),
            },
            _ => {
                let info = typ
                    .arith_info()
                    .ok_or_else(|| CompileError::UnknownSize(typ.clone()))?;
                Ok(info.width / 8)
            }
        }
    }

    fn to_arith(&self, abi: &Abi) -> Result<Type, CompileError> {
        match self.resolve_wrapper() {
            Type::Pointer(_) | Type::Array(..) => match abi.long_size {
                LongSize::Bits32 => Ok(Type::U32),
                LongSize::Bits64 => Ok(Type::U64),
            },
            typ => {
                // Check it's an arithmetic type
                typ.arith_info()
                    .ok_or_else(|| CompileError::NonArithmeticOperand)?;
                Ok(typ.clone())
            }
        }
    }
}

impl Expr {
    pub fn typ<'ce, CE>(&self, cenv: &CE, abi: &Abi) -> Result<Type, CompileError>
    where
        CE: CompileEnv<'ce>,
    {
        use Expr::*;

        let recurse = |expr: &Expr| expr.typ(cenv, abi);

        match self {
            Evaluated(typ, _) => Ok(typ.clone()),
            Uninit => Ok(Type::Unknown),
            Variable(typ, _id) => Ok(typ.clone()),

            InitializerList(_) => Ok(Type::Unknown),
            DesignatedInitializer(_, init) => recurse(init),
            CompoundLiteral(typ, _) => Ok(typ.clone()),

            IntConstant(typ, _) | CharConstant(typ, _) | EnumConstant(typ, _) => Ok(typ.clone()),
            StringLiteral(str) => {
                let len: u64 = str.len().try_into().unwrap();
                // null terminator
                let len = len + 1;
                Ok(Type::Array(
                    Box::new(abi.char_typ()),
                    ArrayKind::Fixed(Ok(len)),
                ))
            }

            Addr(expr) => Ok(Type::Pointer(Box::new(recurse(expr)?))),
            Deref(expr) => match recurse(expr)?.resolve_wrapper() {
                Type::Pointer(typ) | Type::Array(typ, _) => Ok(*typ.clone()),
                typ => Err(CompileError::CannotDeref(typ.clone(), *expr.clone())),
            },
            Plus(expr) | Minus(expr) | Tilde(expr) => Ok(recurse(expr)?.promote()),
            Bang(_) => Ok(Type::I32),
            Cast(typ, _) => Ok(typ.clone()),
            SizeofType(..) | SizeofExpr(_) => Ok(match &abi.long_size {
                LongSize::Bits32 => Type::U32,
                LongSize::Bits64 => Type::U64,
            }),
            PreInc(expr) | PreDec(expr) | PostInc(expr) | PostDec(expr) => recurse(expr),

            MemberAccess(expr, member) => match recurse(expr)?.resolve_wrapper() {
                Type::Variable(id) if id == "REC" => Ok(cenv.field_typ(member)?),
                _ => Ok(Type::Unknown),
            },
            FuncCall(..) => Ok(Type::Unknown),
            Subscript(expr, idx) => {
                let idx = recurse(idx)?;
                idx.arith_info()
                    .ok_or_else(|| CompileError::NonArithmeticOperand)?;

                match recurse(expr)?.resolve_wrapper() {
                    Type::Array(typ, _) | Type::Pointer(typ) => Ok(*typ.clone()),
                    typ => Err(CompileError::NotAnArray(typ.clone())),
                }
            }

            Assign(_lhs, rhs) => recurse(rhs),

            Eq(..) | NEq(..) | LoEq(..) | HiEq(..) | Hi(..) | Lo(..) | And(..) | Or(..) => {
                Ok(Type::I32)
            }

            LShift(expr, _) | RShift(expr, _) => Ok(recurse(expr)?.promote()),

            Mul(lhs, rhs)
            | Div(lhs, rhs)
            | Mod(lhs, rhs)
            | Add(lhs, rhs)
            | Sub(lhs, rhs)
            | BitAnd(lhs, rhs)
            | BitOr(lhs, rhs)
            | BitXor(lhs, rhs) => {
                let lhs = recurse(lhs)?.resolve_wrapper().clone().decay_to_ptr();
                let rhs = recurse(rhs)?.resolve_wrapper().clone().decay_to_ptr();

                match usual_arith_conv(lhs.clone(), rhs.clone()) {
                    Ok(typ) => Ok(typ),
                    Err(_) => {
                        use Type::*;
                        match (&lhs, &rhs) {
                            // If we have types that cannot be converted to a common
                            // type and one of them is arithmetic, that is an error.
                            (Bool | I8 | U8 | I16 | U16 | I32 | U32 | I64 | U64, _)
                            | (_, Bool | I8 | U8 | I16 | U16 | I32 | U32 | I64 | U64) => {
                                Err(CompileError::NonArithmeticOperand)
                            }
                            // However, if the types were not arithmetic but are
                            // equal (such as pointer) it is ok
                            (lhs_, rhs_) if lhs_ == rhs_ => Ok(lhs),
                            (Pointer(lhs_), Pointer(rhs_))
                                if lhs_.resolve_wrapper() == rhs_.resolve_wrapper() =>
                            {
                                Ok(Pointer(Box::new(lhs)))
                            }
                            _ => Err(CompileError::MismatchingOperandType(self.clone(), lhs, rhs)),
                        }
                    }
                }
            }
            Ternary(_, lhs, rhs) => {
                let lhs = recurse(lhs)?.resolve_wrapper().clone().decay_to_ptr();
                let rhs = recurse(rhs)?.resolve_wrapper().clone().decay_to_ptr();

                use Type::*;
                match usual_arith_conv(lhs.clone(), rhs.clone()) {
                    Ok(typ) => Ok(typ),
                    Err(_) => match (&lhs, &rhs) {
                        (lhs_, rhs_) if lhs_ == rhs_ => Ok(lhs),
                        (Pointer(lhs_), Pointer(rhs_))
                            if lhs_.resolve_wrapper() == rhs_.resolve_wrapper() =>
                        {
                            Ok(Pointer(Box::new(lhs)))
                        }
                        _ => Err(CompileError::MismatchingOperandType(self.clone(), lhs, rhs)),
                    },
                }
            }
            CommaExpr(exprs) => recurse(exprs.last().unwrap()),

            ExtensionMacro(desc) => match &desc.kind {
                ExtensionMacroKind::ObjectLike { typ, .. } => Ok(typ.clone()),
                ExtensionMacroKind::FunctionLike { .. } => Ok(Type::Unknown),
            },
            ExtensionMacroCall(cparser::ExtensionMacroCall { compiler, .. }) => {
                compiler.ret_typ.typ(cenv, abi)
            }
        }
    }
}

pub trait Evaluator: Send + Sync {
    fn eval<'eeref, 'ee>(
        &self,
        env: &'eeref (dyn EvalEnv<'ee> + 'eeref),
    ) -> Result<Value<'eeref>, EvalError>;
}

impl<F> Evaluator for F
where
    F: for<'ee, 'eeref> Fn(&'eeref (dyn EvalEnv<'ee> + 'eeref)) -> Result<Value<'eeref>, EvalError>
        + Send
        + Sync,
{
    fn eval<'eeref, 'ee>(
        &self,
        env: &'eeref (dyn EvalEnv<'ee> + 'eeref),
    ) -> Result<Value<'eeref>, EvalError> {
        self(env)
    }
}

// TODO: Re-assess: Maybe we should just use the closure!() macro for that purpose instead of
// adding types that we will have to maintain given that Rust will one day allow explicit HRTB in
// closure syntax
/// Newtype wrapper to ease HRTB inference
pub struct EvalF<F>(F);

impl<F> EvalF<F> {
    #[inline]
    pub fn new(f: F) -> Self {
        EvalF(f)
    }

    #[inline]
    pub fn new_dyn(f: F) -> Box<dyn Evaluator>
    where
        EvalF<F>: Evaluator,
        F: 'static,
    {
        Box::new(EvalF::new(f))
    }
}

impl<F> Evaluator for EvalF<F>
where
    F: for<'ee, 'eeref> Fn(&'eeref (dyn EvalEnv<'ee> + 'eeref)) -> Result<Value<'eeref>, EvalError>
        + Send
        + Sync,
{
    fn eval<'eeref, 'ee>(
        &self,
        env: &'eeref (dyn EvalEnv<'ee> + 'eeref),
    ) -> Result<Value<'eeref>, EvalError> {
        self.0(env)
    }
}

impl Expr {
    pub fn eval_const<T, F>(self, abi: &Abi, f: F) -> T
    where
        F: for<'a> FnOnce(Result<Value<'a>, InterpError>) -> T,
    {
        let env = EmptyEnv::new();
        let eval = || -> Result<_, InterpError> {
            let eval = self.compile(&env, abi)?;
            Ok(eval.eval(&env)?)
        };
        f(eval())
    }

    pub fn simplify<'ce, CE>(self, cenv: &'ce CE, abi: &Abi) -> Expr
    where
        CE: CompileEnv<'ce>,
    {
        let compiled = self.clone().compile(cenv, abi);
        self._do_simplify(cenv, abi, compiled)
    }

    fn _simplify<'ce, CE>(self, cenv: &'ce CE, abi: &Abi) -> Expr
    where
        CE: CompileEnv<'ce>,
    {
        let compiled = self.clone()._compile(cenv, abi);
        self._do_simplify(cenv, abi, compiled)
    }

    fn _do_simplify<'ce, CE>(
        self,
        cenv: &'ce CE,
        abi: &Abi,
        compiled: Result<Box<dyn Evaluator>, CompileError>,
    ) -> Expr
    where
        CE: CompileEnv<'ce>,
    {
        match compiled {
            Ok(eval) => match self.typ(cenv, abi) {
                Ok(typ) => match eval.eval(cenv) {
                    Ok(value) => match value.into_static() {
                        Ok(value) => Expr::Evaluated(typ, value),
                        Err(_) => self,
                    },
                    Err(_) => self,
                },
                Err(_) => self,
            },
            Err(_) => self,
        }
    }

    pub fn compile<'ce, CE>(
        self,
        cenv: &'ce CE,
        abi: &Abi,
    ) -> Result<Box<dyn Evaluator>, CompileError>
    where
        CE: CompileEnv<'ce>,
    {
        // Type check the AST. This should be done only once on the root node, so any recursive
        // compilation invocations are done via _compile() to avoid re-doing it and avoid an O(N^2)
        // complexity
        self.typ(cenv, abi)?;
        self._compile(cenv, abi)
    }

    fn _compile<'ce, CE>(self, cenv: &'ce CE, abi: &Abi) -> Result<Box<dyn Evaluator>, CompileError>
    where
        CE: CompileEnv<'ce>,
    {
        use Expr::*;
        let cannot_handle = |expr| Err(CompileError::CannotHandleExpr(expr));
        let recurse = |expr: Expr| expr._compile(cenv, abi);
        let simplify = |expr: Expr| expr._simplify(cenv, abi);

        fn to_signed(x: u64) -> Result<i64, EvalError> {
            x.try_into()
                .map_err(|_| EvalError::CannotConvertToSigned(x))
        }

        macro_rules! binop {
            ($lhs:expr, $rhs:expr, $op:expr) => {{
                let lhs = *$lhs;
                let rhs = *$rhs;
                let eval_lhs = recurse(lhs.clone())?;
                let eval_rhs = recurse(rhs.clone())?;

                let lhs = lhs.typ(cenv, abi)?;
                let rhs = rhs.typ(cenv, abi)?;

                let (_typ, conv_lhs, conv_rhs) = convert_arith_ops(abi, lhs, rhs)?;

                Ok(EvalF::new_dyn(move |env| {
                    let lhs = conv_lhs(eval_lhs.eval(env)?)?;
                    let rhs = conv_rhs(eval_rhs.eval(env)?)?;

                    match (lhs, rhs) {
                        (Value::U64Scalar(x), Value::U64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::U64Scalar($op(x, y)))
                        }
                        (Value::I64Scalar(x), Value::I64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::I64Scalar($op(x, y)))
                        }
                        _ => Err(EvalError::IllegalType),
                    }
                }))
            }};
        }

        macro_rules! comp {
            ($lhs:expr, $rhs:expr, $op:expr) => {{
                let lhs = *$lhs;
                let rhs = *$rhs;
                let eval_lhs = recurse(lhs.clone())?;
                let eval_rhs = recurse(rhs.clone())?;

                let lhs = lhs.typ(cenv, abi)?.to_arith(abi)?;
                let rhs = rhs.typ(cenv, abi)?.to_arith(abi)?;

                let (_typ, conv_lhs, conv_rhs) = convert_arith_ops(abi, lhs, rhs)?;

                Ok(EvalF::new_dyn(move |env| {
                    let lhs = conv_lhs(eval_lhs.eval(env)?)?;
                    let rhs = conv_rhs(eval_rhs.eval(env)?)?;

                    match (lhs, rhs) {
                        (Value::U64Scalar(x), Value::U64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::I64Scalar($op(x, y)))
                        }
                        (Value::I64Scalar(x), Value::I64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::I64Scalar($op(x, y)))
                        }
                        _ => Err(EvalError::IllegalType),
                    }
                }))
            }};
        }

        macro_rules! shift {
            ($lhs:expr, $rhs:expr, $op:expr) => {{
                let lhs = *$lhs;
                let rhs = *$rhs;
                let eval_lhs = recurse(lhs.clone())?;
                let eval_rhs = recurse(rhs.clone())?;

                let (_typ, conv_lhs) = convert_arith_op(abi, cenv, &lhs)?;
                let (_, conv_rhs) = convert_arith_op(abi, cenv, &rhs)?;

                Ok(EvalF::new_dyn(move |env| {
                    let lhs = conv_lhs(eval_lhs.eval(env)?)?;
                    let rhs = conv_rhs(eval_rhs.eval(env)?)?;

                    match (lhs, rhs) {
                        (Value::U64Scalar(x), Value::U64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::U64Scalar($op(x, y)))
                        }
                        (Value::U64Scalar(x), Value::I64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::U64Scalar($op(x, y)))
                        }

                        (Value::I64Scalar(x), Value::U64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::I64Scalar($op(x, y)))
                        }
                        (Value::I64Scalar(x), Value::I64Scalar(y)) =>
                        {
                            #[allow(clippy::redundant_closure_call)]
                            Ok(Value::I64Scalar($op(x, y)))
                        }
                        _ => Err(EvalError::IllegalType),
                    }
                }))
            }};
        }

        let eval = match self {
            Evaluated(_typ, value) => Ok(EvalF::new_dyn(move |_| Ok(value.clone()))),
            Variable(_typ, id) => Ok(EvalF::new_dyn(move |_| Ok(Value::Variable(id.clone())))),

            MemberAccess(expr, member) => {
                let expr = simplify(*expr);
                match &expr {
                    Variable(_, id) | Evaluated(_, Value::Variable(id)) if id == "REC" => {
                        cenv.field_getter(&member)
                    }
                    _ => cannot_handle(expr),
                }
            }

            expr @ (Uninit
            | InitializerList(_)
            | DesignatedInitializer(..)
            | CompoundLiteral(..)) => cannot_handle(expr),
            IntConstant(typ, x) | CharConstant(typ, x) => {
                let typ = typ.to_arith(abi)?;
                let info = typ
                    .arith_info()
                    .ok_or_else(|| CompileError::NonArithmeticOperand)?;
                Ok(if info.is_signed() {
                    EvalF::new_dyn(move |_| Ok(Value::I64Scalar(to_signed(x)?)))
                } else {
                    EvalF::new_dyn(move |_| Ok(Value::U64Scalar(x)))
                })
            }
            StringLiteral(s) => {
                let s: Arc<str> = Arc::from(s.as_ref());
                Ok(EvalF::new_dyn(move |_| {
                    Ok(Value::Str(Str::new_arc(Arc::clone(&s))))
                }))
            }
            expr @ EnumConstant(..) => cannot_handle(expr),
            SizeofType(typ) => {
                let size = Ok(Value::U64Scalar(typ.size(abi)?));
                Ok(EvalF::new_dyn(move |_| size.clone()))
            }
            SizeofExpr(expr) => {
                let typ = expr.typ(cenv, abi)?;
                recurse(SizeofType(typ))
            }
            Cast(typ, expr) => {
                let expr = *expr;
                let typ = typ.decay_to_ptr();
                let expr_typ = expr.typ(cenv, abi)?.decay_to_ptr();

                let expr_typ: &Type = expr_typ.resolve_wrapper();
                let typ: &Type = typ.resolve_wrapper();

                match (typ, expr) {
                    (typ, expr) if typ == expr_typ => recurse(expr),
                    // Chains of cast (T1*)(T2*)...x is equivalent to (T1*)x
                    (Type::Pointer(_), Cast(Type::Pointer(_), expr)) => {
                        recurse(Cast(typ.clone(), expr))
                    }
                    (typ, expr) => match (typ, expr_typ) {
                        (
                            Type::Pointer(typ),
                            Type::Pointer(expr_typ) | Type::Array(expr_typ, _),
                        ) => {
                            let typ: &Type = typ.deref().resolve_wrapper();
                            let expr_typ = expr_typ.resolve_wrapper();
                            match (expr_typ, typ) {
                                (expr_typ, typ) if typ == expr_typ => recurse(expr),
                                // (void *)(T *)x is treated the same as (T *)x
                                (_, Type::Void) => recurse(expr),

                                // For integer types:
                                // T x;
                                // (T2*)&x == &(T2)x
                                // Note that this is only well defined if T2 is char in
                                // first approximation
                                (
                                    Type::Bool
                                    | Type::U8
                                    | Type::I8
                                    | Type::U16
                                    | Type::I16
                                    | Type::U32
                                    | Type::I32
                                    | Type::I64
                                    | Type::U64,
                                    typ,
                                ) if typ == &Type::Bool || typ == &Type::U8 || typ == &Type::I8 => {
                                    recurse(Addr(Box::new(Cast(
                                        typ.clone(),
                                        Box::new(Deref(Box::new(expr))),
                                    ))))
                                }
                                (expr_typ, typ) => Err(CompileError::IncompatiblePointerCast(
                                    expr_typ.clone(),
                                    typ.clone(),
                                )),
                            }
                        }
                        (typ, _expr_typ) => {
                            // Convert potential pointers to an integer type for the
                            // sake of value conversion.
                            let typ = typ.to_arith(abi)?;
                            let conv = convert_arith(&typ)?;
                            let eval = recurse(expr)?;
                            Ok(EvalF::new_dyn(move |x| conv(eval.eval(x)?)))
                        }
                    },
                }
            }
            Plus(expr) => {
                let expr = *expr;
                let (_typ, conv) = convert_arith_op(abi, cenv, &expr)?;
                let eval = recurse(expr)?;

                Ok(EvalF::new_dyn(move |x| conv(eval.eval(x)?)))
            }
            Minus(expr) => {
                let (_typ, conv) = convert_arith_op(abi, cenv, &expr)?;

                macro_rules! negate {
                    ($value:expr) => {
                        match $value {
                            Value::I64Scalar(x) => conv(Value::I64Scalar(-x)),
                            Value::U64Scalar(x) => conv(Value::I64Scalar(-(x as i64))),
                            _ => Err(EvalError::IllegalType),
                        }
                    };
                }

                let eval = recurse(*expr)?;
                match eval.eval(cenv) {
                    Err(_) => Ok(EvalF::new_dyn(move |env| {
                        let value = eval.eval(env)?;
                        negate!(value)
                    })),
                    Ok(value) => {
                        let value = negate!(value);
                        Ok(EvalF::new_dyn(move |_| value.clone()))
                    }
                }
            }
            Tilde(expr) => {
                let expr = *expr;
                let (typ, _conv) = convert_arith_op(abi, cenv, &expr)?;
                let eval = recurse(expr)?;

                macro_rules! complement {
                    ($unsigned:ty, $signed:ty) => {
                        Ok(EvalF::new_dyn(move |env| match eval.eval(env)? {
                            Value::I64Scalar(x) => Ok(Value::I64Scalar((!(x as $signed)) as i64)),
                            Value::U64Scalar(x) => Ok(Value::U64Scalar((!(x as $unsigned)) as u64)),
                            _ => Err(EvalError::IllegalType),
                        }))
                    };
                }

                use Type::*;
                match typ {
                    Bool => complement!(u8, i8),
                    U8 | I8 => complement!(u8, i8),
                    U16 | I16 => complement!(u16, i16),
                    U32 | I32 => complement!(u32, i32),
                    U64 | I64 => complement!(u64, i64),
                    _ => Err(CompileError::NonArithmeticOperand),
                }
            }
            Bang(expr) => {
                let eval = recurse(*expr)?;

                Ok(EvalF::new_dyn(move |env| match eval.eval(env)? {
                    Value::U64Scalar(x) => Ok(Value::I64Scalar((x == 0).into())),
                    Value::I64Scalar(x) => Ok(Value::I64Scalar((x == 0).into())),
                    _ => Err(EvalError::IllegalType),
                }))
            }

            Addr(expr) => {
                let eval = recurse(*expr)?;
                Ok(EvalF::new_dyn(move |env| {
                    let val = eval.eval(env)?;
                    let val = ScratchBox::Owned(OwnedScratchBox::new_in(val, env.scratch()));
                    Ok(Value::Addr(val))
                }))
            }
            Deref(expr) => recurse(Subscript(expr, Box::new(IntConstant(Type::I32, 0)))),

            // Since there can be sequence points inside an expression in a number
            // of ways, we would need a mutable environment to keep track of it, so
            // ignore it for now as this does not seem to be used in current
            // kernels.
            // https://port70.net/~nsz/c/c11/n1570.html#C
            expr @ (PostInc(_) | PostDec(_) | PreInc(_) | PreDec(_)) => cannot_handle(expr),
            expr @ Assign(..) => cannot_handle(expr),

            Ternary(cond, lhs, rhs) => {
                let lhs = *lhs;
                let rhs = *rhs;

                let eval_cond = recurse(*cond)?;
                let eval_lhs = recurse(lhs.clone())?;
                let eval_rhs = recurse(rhs.clone())?;

                let lhs_typ = lhs.typ(cenv, abi)?;
                let rhs_typ = rhs.typ(cenv, abi)?;

                let lhs_info = lhs_typ.arith_info();
                let rhs_info = rhs_typ.arith_info();

                match (lhs_info, rhs_info) {
                    (Some(_), Some(_)) => {
                        let (_, conv_lhs, conv_rhs) = convert_arith_ops(abi, lhs_typ, rhs_typ)?;
                        Ok(EvalF::new_dyn(move |env| match eval_cond.eval(env)? {
                            Value::U64Scalar(0) | Value::I64Scalar(0) => {
                                conv_rhs(eval_rhs.eval(env)?)
                            }
                            _ => conv_lhs(eval_lhs.eval(env)?),
                        }))
                    }
                    _ => Ok(EvalF::new_dyn(move |env| match eval_cond.eval(env)? {
                        Value::U64Scalar(0) | Value::I64Scalar(0) => eval_rhs.eval(env),
                        _ => eval_lhs.eval(env),
                    })),
                }
            }
            CommaExpr(mut exprs) => recurse(exprs.pop().unwrap()),

            Subscript(expr, idx) => {
                let expr = *expr;
                let eval_idx = recurse(*idx)?;
                let eval_expr = recurse(expr.clone())?;

                match eval_idx.eval(cenv) {
                    // If we access element 0 at compile time, that is simply
                    // dereferencing the value as a pointer.
                    Ok(Value::U64Scalar(0) | Value::I64Scalar(0)) => {
                        let expr_typ = expr.typ(cenv, abi)?;
                        let expr_typ = expr_typ.resolve_wrapper();

                        macro_rules! deref {
                            ($env:expr, $val:expr) => {{
                                let val = $val;
                                match val.get($env, 0) {
                                    Ok(item) => item,
                                    Err((val, _err)) => val,
                                }
                            }};
                        }

                        match &expr_typ {
                            Type::Pointer(typ) | Type::Array(typ, ..) => {
                                // We might need the conversion as it is legal to cast e.g. an int* to a char*
                                let conv = convert_arith(typ).unwrap_or(Box::new(|x| Ok(x)));
                                Ok(EvalF::new_dyn(move |env| {
                                    conv(deref!(env, eval_expr.eval(env)?))
                                }))
                            }
                            _ => cannot_handle(expr),
                        }
                    }
                    _ => Ok(EvalF::new_dyn(move |env| {
                        let idx: usize = match eval_idx.eval(env)? {
                            Value::U64Scalar(x) => {
                                x.try_into().map_err(|_| EvalError::OutOfBondIndex)
                            }
                            Value::I64Scalar(x) => {
                                x.try_into().map_err(|_| EvalError::OutOfBondIndex)
                            }
                            _ => Err(EvalError::IllegalType),
                        }?;

                        eval_expr.eval(env)?.get(env, idx).map_err(|(_, err)| err)
                    })),
                }
            }

            Mul(lhs, rhs) => binop!(lhs, rhs, |x, y| x * y),
            Div(lhs, rhs) => binop!(lhs, rhs, |x, y| x / y),
            Mod(lhs, rhs) => binop!(lhs, rhs, |x, y| x % y),
            Add(lhs, rhs) => binop!(lhs, rhs, |x, y| x + y),
            Sub(lhs, rhs) => binop!(lhs, rhs, |x, y| x - y),

            BitAnd(lhs, rhs) => binop!(lhs, rhs, |x, y| x & y),
            BitOr(lhs, rhs) => binop!(lhs, rhs, |x, y| x | y),
            BitXor(lhs, rhs) => binop!(lhs, rhs, |x, y| x ^ y),

            Eq(lhs, rhs) => comp!(lhs, rhs, |x, y| (x == y).into()),
            NEq(lhs, rhs) => comp!(lhs, rhs, |x, y| (x != y).into()),
            LoEq(lhs, rhs) => comp!(lhs, rhs, |x, y| (x <= y).into()),
            HiEq(lhs, rhs) => comp!(lhs, rhs, |x, y| (x >= y).into()),
            Lo(lhs, rhs) => comp!(lhs, rhs, |x, y| (x < y).into()),
            Hi(lhs, rhs) => comp!(lhs, rhs, |x, y| (x > y).into()),

            And(lhs, rhs) => comp!(lhs, rhs, |x, y| ((x != 0) && (y != 0)).into()),
            Or(lhs, rhs) => comp!(lhs, rhs, |x, y| ((x != 0) || (y != 0)).into()),

            LShift(lhs, rhs) => shift!(lhs, rhs, |x, y| x << y),
            RShift(lhs, rhs) => shift!(lhs, rhs, |x, y| x >> y),

            ExtensionMacro(desc) => {
                let kind = &desc.kind;
                match kind {
                    ExtensionMacroKind::ObjectLike { value, .. } => {
                        let value = value.clone();
                        Ok(EvalF::new_dyn(move |_env| Ok(value.clone())))
                    }
                    // We cannot do anything with a bare function-like macro, it has
                    // to be applied to an expression, at which point the parser
                    // gives us a ExtensionMacroCall.
                    ExtensionMacroKind::FunctionLike { .. } => cannot_handle(ExtensionMacro(desc)),
                }
            }
            ExtensionMacroCall(call) => (call.compiler.compiler)(cenv, abi),

            expr @ FuncCall(..) => cannot_handle(expr),
        }?;

        // Compile-time evaluation, if that succeeds we simply replace the evaluator
        // by a closure that clones the precomputed value.
        match eval.eval(cenv) {
            Ok(value) => match value.into_static() {
                Err(_) => Ok(eval),
                Ok(value) => Ok(EvalF::new_dyn(move |_| Ok(value.clone()))),
            },
            Err(_err) => Ok(eval),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use nom::AsBytes;

    use super::*;
    use crate::{
        cparser::{CGrammar, CGrammarCtx, DynamicKind, Type},
        grammar::PackratGrammar as _,
        header::{Abi, Endianness, LongSize},
        parser::tests::{run_parser, zero_copy_to_str},
    };

    #[derive(Clone, Copy)]
    enum Stage {
        Compile,
        Run,
    }

    struct TestEnv {
        string: String,
        scratch: ScratchAlloc,
        stage: Arc<Mutex<Stage>>,
    }

    impl<'ce> CompileEnv<'ce> for TestEnv {
        #[inline]
        fn field_typ(&self, id: &str) -> Result<Type, CompileError> {
            match id {
                "runtime_u32_field" => Ok(Type::U32),
                "runtime_zero_field" => Ok(Type::U32),
                "u32_field" => Ok(Type::U32),
                "u32_array_field" => Ok(Type::Array(Box::new(Type::U32), ArrayKind::Fixed(Ok(2)))),
                "u32_dynarray_field" => Ok(Type::Array(
                    Box::new(Type::U32),
                    ArrayKind::Dynamic(DynamicKind::Dynamic),
                )),
                "str_field" => Ok(Type::Pointer(Box::new(Type::U8))),

                "string_field" => Ok(Type::Pointer(Box::new(Type::U8))),
                "owned_string_field" => Ok(Type::Pointer(Box::new(Type::U8))),
                _ => Err(CompileError::UnknownField),
            }
        }

        #[inline]
        fn field_getter(&self, id: &str) -> Result<Box<dyn Evaluator>, CompileError> {
            match id {
                "runtime_u32_field" => {
                    let stage = Arc::clone(&self.stage);
                    Ok(EvalF::new_dyn(move |_env| match *stage.lock().unwrap() {
                        Stage::Compile => Err(EvalError::NoEventData),
                        Stage::Run => Ok(Value::U64Scalar(44)),
                    }))
                }
                "runtime_zero_field" => {
                    let stage = Arc::clone(&self.stage);
                    Ok(EvalF::new_dyn(move |_env| match *stage.lock().unwrap() {
                        Stage::Compile => Err(EvalError::NoEventData),
                        Stage::Run => Ok(Value::U64Scalar(0)),
                    }))
                }
                "u32_field" => Ok(EvalF::new_dyn(|_| Ok(Value::U64Scalar(42)))),
                "u32_array_field" => Ok(EvalF::new_dyn(|_| {
                    Ok(Value::U32Array([42, 43].as_ref().into()))
                })),
                "u32_dynarray_field" => Ok(EvalF::new_dyn(|_| {
                    Ok(Value::U32Array([42, 43].as_ref().into()))
                })),
                "str_field" => Ok(EvalF::new_dyn(|_| {
                    Ok(Value::Str(Str::new_owned("hello world".into())))
                })),

                "string_field" => {
                    let s = Arc::from(&*self.string);
                    Ok(EvalF::new_dyn(move |_| {
                        Ok(Value::Str(Str::new_arc(Arc::clone(&s))))
                    }))
                }
                "owned_string_field" => {
                    let array = Value::Str(Str::new_owned((&self.string).into()));
                    Ok(EvalF::new_dyn(move |_| Ok(array.clone())))
                }
                _ => Err(CompileError::UnknownField),
            }
        }
    }

    impl<'ee> EvalEnv<'ee> for TestEnv {
        // #[inline]
        // fn field_getter<EE: EvalEnv>(&self, id: &str) -> Result<Box<dyn Fn(&EE) -> Result<Value, EvalError>>, CompileError> {
        //     Ok(Box::new(|_| Ok(Value::U64Scalar(42))))
        // }

        fn header(&self) -> Result<&Header, EvalError> {
            Err(EvalError::NoHeader)
        }

        fn deref_static(&self, addr: u64) -> Result<Value<'_>, EvalError> {
            match addr {
                42 => Ok(Value::Str(Str::new_borrowed("hello world"))),
                43 => Ok(Value::U64Scalar(105)),
                44 => Ok(Value::U64Scalar(257)),
                45 => Ok(Value::U64Scalar(44)),
                _ => Err(EvalError::CannotDeref),
            }
        }

        #[inline]
        fn scratch(&self) -> &ScratchAlloc {
            &self.scratch
        }
    }

    #[test]
    fn interp_test() {
        fn test(src: &[u8], expected: Value<'_>) {
            let stage = Arc::new(Mutex::new(Stage::Compile));
            let env = TestEnv {
                scratch: ScratchAlloc::new(),
                string: "foobar".into(),
                stage: Arc::clone(&stage),
            };
            let abi = Abi {
                long_size: LongSize::Bits64,
                endianness: Endianness::Little,
                char_signedness: Signedness::Unsigned,
            };
            let parser = CGrammar::expr();
            let ctx = CGrammarCtx::new(&abi);
            let input = CGrammar::make_span(src, &ctx);
            let ast = run_parser(input.clone(), parser);
            let input = zero_copy_to_str(input.as_bytes());
            let compiled = ast
                .compile(&env, &abi)
                .unwrap_or_else(|err| panic!("Error while compiling {input:?}: {err}"));

            *stage.lock().unwrap() = Stage::Run;

            let expr = compiled
                .eval(&env)
                .unwrap_or_else(|err| panic!("Error while interpreting {input:?}: {err}"));
            assert_eq!(expr, expected, "while interpreting {input:}")
        }

        fn signed(x: i64) -> Value<'static> {
            Value::I64Scalar(x)
        }
        fn unsigned(x: u64) -> Value<'static> {
            Value::U64Scalar(x)
        }
        fn addr(x: Value<'static>) -> Value<'static> {
            Value::Addr(ScratchBox::Arc(Arc::new(x)))
        }

        let hello_world = Value::Str(Str::new_arc(Arc::from("hello world")));

        // Literals
        test(b"0", signed(0));
        test(b"1", signed(1));
        test(b"-1", signed(-1));
        test(b"-1u", unsigned(4294967295));
        test(b"-(1u)", unsigned(4294967295));
        test(b"-(-(-(1u)))", unsigned(4294967295));
        test(b"-(-(1u))", unsigned(1));
        test(b"-(-(1UL))", unsigned(1));

        test(br#""hello world""#, hello_world.clone());

        test(b"true", signed(1));
        test(b"false", signed(0));

        // Basic arithmetic
        test(b"1+2", signed(3));
        test(b"1u+2u", unsigned(3));
        test(b"1+2u", unsigned(3));
        test(b"1u+2", unsigned(3));
        test(b"(uint16_t)1u+(s32)2", unsigned(3));

        test(b"-1+2", signed(1));
        test(b"1+TASK_COMM_LEN", signed(17));
        test(b"-TASK_COMM_LEN + 1", signed(-15));
        test(b"-(s32)TASK_COMM_LEN + 1", signed(-15));

        test(b"-1-2", signed(-3));
        test(b"1-TASK_COMM_LEN", signed(-15));
        test(b"-TASK_COMM_LEN - 1", signed(-17));

        test(b"-TASK_COMM_LEN - 1u", unsigned(4294967279));

        test(b"10 % 2", signed(0));
        test(b"11 % 2", signed(1));
        test(b"-11 % 2", signed(-1));
        test(b"11 % -2", signed(1));
        test(b"-11 % -2", signed(-1));
        test(b"((s64)(-11)) % ((s16)(-2))", signed(-1));

        // Integer overflow
        test(b"1 == 1", signed(1));
        test(b"1 == 2", signed(0));
        test(b"-1 == 4294967295", signed(0));
        test(b"-1u == 4294967295", signed(1));
        test(b"-1 == 4294967295u", signed(1));
        test(b"-1u == 4294967295u", signed(1));
        test(b"(u64)-1u == (unsigned int)4294967295u", signed(1));

        // Comparisons
        test(b"1 > 2", signed(0));
        test(b"2 > 1", signed(1));
        test(b"1 > -1u", signed(0));
        test(b"-1u > 1", signed(1));
        test(b"-1u < 1", signed(0));
        test(b"(u32)-1u > (s32)1", signed(1));

        // Shifts
        test(b"2 >> 1", signed(1));
        test(b"-2 >> 1", signed(-1));
        test(b"2 << 1", signed(4));
        test(b"-2 << 1", signed(-4));
        test(b"(s8)-2 << (s64)1", signed(-4));
        test(b"(s8)-2 << (u64)1", signed(-4));

        // Bitwise not
        test(b"~0", signed(-1));
        test(b"~0u", unsigned(4294967295));
        test(b"~(u8)0u", unsigned(4294967295));
        test(b"~((u32)0)", unsigned(4294967295));
        test(b"~0 == -1", signed(1));
        test(b"(s8)~0 == -1", signed(1));
        test(b"(u32)~0 == -1u", signed(1));
        test(b"(u64)~0 == -1ull", signed(1));

        // Logical not
        test(b"!0", signed(1));
        test(b"!1", signed(0));
        test(b"!42", signed(0));
        test(b"!(s32)42", signed(0));
        test(b"!(u32)42", signed(0));

        // Logical or
        test(b"1 && 2", signed(1));

        // Ternary
        test(b"1 ? 1 : 0", signed(1));
        test(b"0 ? 1 : 0", signed(0));
        test(b"0 ? 1 : 0u", unsigned(0));
        test(b"-12 ? 42u : 0", unsigned(42));
        test(b"(s32)-12 ? (u8)42 : 0", unsigned(42));

        // Casts
        test(b"(int)0", signed(0));
        test(b"(s8)0", signed(0));
        test(b"(unsigned int)0", unsigned(0));
        test(b"(u32)0", unsigned(0));
        test(b"(unsigned int)-1", unsigned(4294967295));
        test(b"(u32)-1", unsigned(4294967295));
        test(b"(unsigned int)(unsigned char)-1", unsigned(255));
        test(b"(u32)(u8)-1", unsigned(255));
        test(b"(int)(unsigned int)-1", signed(-1));
        test(b"(s32)(u64)-1", signed(-1));
        test(b"(int)4294967295", signed(-1));
        test(b"(s32)4294967295", signed(-1));
        test(b"(int*)&1", addr(Value::I64Scalar(1)));
        test(b"(s32*)&1", addr(Value::I64Scalar(1)));
        test(b"(int*)1", unsigned(1));
        test(b"(s16*)1", unsigned(1));
        test(b"(void *)1ull", unsigned(1));
        test(b"((__u16)(__le16)1)", unsigned(1));

        // Sizeof type
        test(b"sizeof(char)", unsigned(1));
        test(b"sizeof(int)", unsigned(4));
        test(b"sizeof(unsigned long)", unsigned(8));
        test(b"sizeof(int *)", unsigned(8));
        test(b"sizeof(u8)", unsigned(1));
        test(b"sizeof(u64)", unsigned(8));
        test(b"sizeof(u8 *)", unsigned(8));

        // Sizeof expr
        test(b"sizeof(1)", unsigned(4));
        test(b"sizeof 1", unsigned(4));
        test(b"sizeof(1l)", unsigned(8));
        test(b"sizeof((long)1)", unsigned(8));
        test(b"sizeof((u64)1)", unsigned(8));
        test(b"sizeof(&1)", unsigned(8));
        test(b"sizeof((u8)&1)", unsigned(1));

        // Address and deref
        test(b"&1", addr(signed(1)));
        test(b"(void *)&1", addr(signed(1)));
        test(b"*(void *)&1", signed(1));
        test(b"*(u8*)(void *)&1", unsigned(1));
        test(b"*(u8*)(void *)&257", unsigned(1));
        test(b"*(u8*)(void *)&257ull", unsigned(1));
        test(b"*(u64*)(void*)(u64*)(void *)&1ull", unsigned(1));
        test(b"*(u64*)(u8*)(void*)(u64*)(void *)&1ull", unsigned(1));
        test(b"*(u32*)(u8*)(void*)(u32*)(void *)&1u", unsigned(1));
        test(b"&(u32)1", addr(unsigned(1)));
        test(b"&REC->runtime_u32_field", addr(unsigned(44)));
        test(b"*(unsigned int *)REC->runtime_u32_field", unsigned(257));
        test(b"*(u32 *)REC->runtime_u32_field", unsigned(257));
        test(b"*(signed int *)REC->runtime_u32_field", signed(257));
        test(b"*(s32 *)REC->runtime_u32_field", signed(257));
        test(b"*(unsigned int *)&REC->runtime_u32_field", unsigned(44));
        test(b"*(u32 *)&REC->runtime_u32_field", unsigned(44));
        test(b"(signed int)*&REC->runtime_u32_field", signed(44));
        test(b"(s32)*&REC->runtime_u32_field", signed(44));
        test(b"*&1", signed(1));
        test(b"*&*&1", signed(1));
        test(b"(s32)*&*&1", signed(1));
        test(b"*(&1)", signed(1));
        test(b"*(2, &1)", signed(1));
        test(b"*(0 ? &1 : &2)", signed(2));
        test(b"*(1 ? &1 : &2)", signed(1));
        test(b"*(1 ? &(s32)1 : &(s32)2)", signed(1));
        test(b"*(1 ? &(s32)1 : &(int)2)", signed(1));

        test(b"*(char *)42", unsigned(104));
        test(b"*(u8 *)42", unsigned(104));
        test(b"*(unsigned char *)42", unsigned(104));
        test(b"*(signed char *)42", signed(104));
        test(b"*(s8 *)42", signed(104));
        test(b"*(unsigned long *)43", unsigned(105));
        test(b"*(u64 *)43", unsigned(105));
        test(b"*(char *)44", unsigned(1));
        test(b"*(u8 *)44", unsigned(1));
        test(b"*(char *)(int *)(short *)44", unsigned(1));
        test(b"*(char *)(s32 *)(short *)44", unsigned(1));
        test(b"*(u8 *)(int *)(short *)44", unsigned(1));
        test(b"*(u8 *)(s32 *)(short *)44", unsigned(1));
        test(b"*(u8 *)(s32 *)(s16 *)44", unsigned(1));
        test(b"*(char *)(int *)(s16 *)44", unsigned(1));
        test(b"*(u8 *)(int *)(s16 *)44", unsigned(1));

        test(b"((char *)42)[0]", unsigned(104));
        test(b"((u8 *)42)[0]", unsigned(104));
        test(b"((char *)42)[1]", unsigned(101));
        test(b"((u8 *)42)[1]", unsigned(101));
        test(b"*(int *)44", signed(257));
        test(b"*(s32 *)44", signed(257));
        test(b"*(unsigned int *)44", unsigned(257));
        test(b"*(u32 *)44", unsigned(257));
        test(b"( *(unsigned int (*)[10])44 )[0]", unsigned(257));
        test(b"( *(u32 (*)[10])44 )[0]", unsigned(257));
        test(b"( *(int (*)[10])44 )[0]", signed(257));
        test(b"( *(s32 (*)[10])44 )[0]", signed(257));

        test(b"**(unsigned int **)45", unsigned(257));
        test(b"**(u32 **)45", unsigned(257));
        test(b"( *(unsigned int * (*)[10])45 )[0]", unsigned(257));
        test(b"( *(u32 * (*)[10])45 )[0]", unsigned(257));

        // Array
        test(b"(&1)[0]", signed(1));
        test(b"((s32*)&1)[0]", signed(1));
        test(b"(42 ? &1 : &2)[0]", signed(1));
        test(b"(42 ? (s8*)&1 : (s8*)&2)[0]", signed(1));
        test(b"(0 ? &1 : &2)[0]", signed(2));
        test(b"(0 ? (s8*)&1 : (signed char*)&2)[0]", signed(2));
        test(b"(REC->runtime_zero_field ? &1 : &2)[0]", signed(2));
        test(b"((s8)REC->runtime_zero_field ? &1 : &2)[0]", signed(2));

        // Field access
        test(b"REC->u32_field", unsigned(42));
        test(b"(u64)REC->u32_field", unsigned(42));
        test(b"(*&REC) -> u32_field", unsigned(42));
        test(b"(*(0 ? &(REC) : &(REC))) -> u32_field", unsigned(42));
        test(b"(*(1 ? &(REC) : &(REC))) -> u32_field", unsigned(42));

        test(b"sizeof(REC->u32_array_field)", unsigned(4 * 2));
        test(b"sizeof((int [2])REC->u32_array_field)", unsigned(4 * 2));
        test(b"sizeof((u8 [2])REC->u32_array_field)", unsigned(2));
        test(b"REC->u32_array_field[0]", unsigned(42));
        test(b"*REC->u32_array_field", unsigned(42));

        test(b"REC->u32_dynarray_field[0]", unsigned(42));
        test(b"((u32 *)REC->u32_dynarray_field)[0]", unsigned(42));
        test(b"REC->u32_dynarray_field[1]", unsigned(43));
        test(b"((u32 *)REC->u32_dynarray_field)[1]", unsigned(43));
        test(b"*REC->u32_dynarray_field", unsigned(42));
        test(b"*(u32*)REC->u32_dynarray_field", unsigned(42));

        test(b"*REC->owned_string_field", unsigned(102));
        test(b"REC->owned_string_field[6]", unsigned(0));
        test(b"((char *)REC->owned_string_field)[6]", unsigned(0));
        test(b"REC->str_field", hello_world.clone());
        test(b"(char *)REC->str_field", hello_world.clone());
        // Unfortunately, it is not easy to preserve the Array value
        // across a &* chain, as this would either necessitate to not simplify
        // the *& chains in the sub-expression, or be very brittle and strictly
        // match &* with nothing in-between which is quite useless.
        //
        // So we end up with dereferencing the Array, which provides its
        // first item, and then we take the address of that.
        test(b"*&*REC->str_field", unsigned(104));
        test(b"*(u8*)&*REC->str_field", unsigned(104));
        test(b"*REC->str_field", unsigned(104));
        test(b"*(u8*)REC->str_field", unsigned(104));
        test(b"REC->str_field[0]", unsigned(104));
        test(b"((u8*)REC->str_field)[0]", unsigned(104));
        test(b"REC->str_field[1]", unsigned(101));
        test(b"((u8*)REC->str_field)[1]", unsigned(101));
        test(b"REC->str_field[6]", unsigned(119));
        test(b"((u8*)REC->str_field)[6]", unsigned(119));
        test(b"REC->str_field[11]", unsigned(0));
        test(b"((u8*)REC->str_field)[11]", unsigned(0));

        test(b"*(signed char*)REC->str_field", signed(104));
        test(b"*(s8*)REC->str_field", signed(104));
        test(b"(int)*REC->str_field", signed(104));
        test(b"(s32)*REC->str_field", signed(104));
        test(b"(int)REC->str_field[0]", signed(104));
        test(b"(s32)REC->str_field[0]", signed(104));

        // Combined
        test(b"(65536/((1UL) << 12) + 1)", unsigned(17));
        test(b"(65536/((1UL) << 12) + (s32)1)", unsigned(17));

        test(b"*(int*)(&(-1))", signed(-1));
        test(b"*(s32*)((s32 *)&(-1))", signed(-1));
        test(b"*(unsigned int*)(&-1u)", unsigned(4294967295));
        test(b"*(u32 *)(&-1u)", unsigned(4294967295));
        test(b"*(unsigned int*)(char*)(&-1u)", unsigned(4294967295));
        test(b"*(u32 *)(u8 *)(&-1u)", unsigned(4294967295));
        test(b"*(unsigned int *)(u8 *)(&-1u)", unsigned(4294967295));
        // This is not UB since any value can be accessed via a char pointer:
        // https://port70.net/~nsz/c/c11/n1570.html#6.5p7
        test(b"*(unsigned char*)(int*)(&(-1))", unsigned(255));
        test(b"*(u8 *)(int*)(&(-1))", unsigned(255));

        test(b"(int*)1 == (int*)1", signed(1));
        test(b"(s32*)1 == (s32*)1", signed(1));
        test(b"(int*)1 == (char*)1", signed(1));
        test(b"(s32*)1 == (u8*)1", signed(1));

        test(b"(int)(int*)1 == 1", signed(1));
        test(b"(s32)(s32*)1 == 1", signed(1));
        test(b"(int)(s32*)1 == 1", signed(1));
        test(b"(s32)(int*)1 == 1", signed(1));
        test(b"(int)(int*)1 == 2", signed(0));
        test(b"(s32)(int*)1 == 2", signed(0));
        test(b"(int)(s32*)1 == 2", signed(0));
        test(b"(s32)(s32*)1 == 2", signed(0));
        test(b"(char)(int*)256 == 0", signed(1));
        test(b"(u8)(s32*)256 == 0", signed(1));
        test(b"(signed char)(s32*)256 == 0", signed(1));

        test(
            b"*((char)(int*)256 == 0 ? (&42, &43) : &2) == 43",
            signed(1),
        );
        test(
            b"*((u8)(s32*)256 == 0 ? ((s32*)&42, &43) : &(s32)2) == (s64)43",
            signed(1),
        );

        test(b"1 ? '*' : ' '", signed(42));
        test(b"(1 && 2) ? '*' : ' '", signed(42));
        test(b"1 && 2 ? '*' : ' '", signed(42));
        test(b"(int) 1 && (int) 2 ? '*' : ' '", signed(42));

        // Extension macros
        test(b"__builtin_constant_p(sizeof(struct page))", signed(0));
        test(br#"__builtin_constant_p("foo")"#, signed(0));
        test(br#"__builtin_constant_p("(a)")"#, signed(0));
        test(br#"__builtin_constant_p("(a")"#, signed(0));
        test(br#"__builtin_constant_p("a)")"#, signed(0));
        test(br#"__builtin_constant_p("a)\"")"#, signed(0));
        test(br#"__builtin_constant_p("\"a)")"#, signed(0));
        test(br#"__builtin_constant_p(')')"#, signed(0));
        test(br#"__builtin_constant_p('\'', "'")"#, signed(0));
    }
}
