// SPDX-License-Identifier: MIT
//! Fixed-layout little-endian messages: the structs of the guest's protocol headers as Rust values, each with its
//! bytes both ways. A struct made with [`wire_struct!`] lays its fields out in order with no padding (the headers'
//! structs have none: their test checks each offset against the C header).

/// A field type of a wire struct.
pub trait Le: Sized + Copy + Default {
    const SIZE: usize;
    fn put(self, out: &mut [u8]);
    fn get(bytes: &[u8]) -> Self;
}

macro_rules! le_int {
    ($($t:ty),*) => {$(
        impl Le for $t {
            const SIZE: usize = std::mem::size_of::<$t>();
            fn put(self, out: &mut [u8]) {
                out[..Self::SIZE].copy_from_slice(&self.to_le_bytes());
            }
            fn get(bytes: &[u8]) -> Self {
                <$t>::from_le_bytes(bytes[..Self::SIZE].try_into().expect("a field's bytes"))
            }
        }
    )*};
}
le_int!(u8, u16, u32, i32, u64);

impl<const N: usize> Le for [u8; N]
where
    [u8; N]: Default,
{
    const SIZE: usize = N;
    fn put(self, out: &mut [u8]) {
        out[..N].copy_from_slice(&self);
    }
    fn get(bytes: &[u8]) -> Self {
        bytes[..N].try_into().expect("a field's bytes")
    }
}

/// A message struct of the wire.
pub trait Wire: Sized {
    /// Its size in bytes (C's sizeof).
    const SIZE: usize;
    /// Each field's name, offset and size, in order (the header test checks them against C).
    fn layout() -> Vec<(&'static str, usize, usize)>;
    fn write_to(&self, out: &mut [u8]);
    fn read_from(bytes: &[u8]) -> Self;

    /// Its bytes.
    fn to_bytes(&self) -> Vec<u8> {
        let mut out = vec![0; Self::SIZE];
        self.write_to(&mut out);
        out
    }

    /// The struct at the start of `bytes`; None when they are shorter.
    fn parse(bytes: &[u8]) -> Option<Self> {
        (bytes.len() >= Self::SIZE).then(|| Self::read_from(bytes))
    }

    /// The struct at the start of `bytes`, the missing tail as zeroes (C's memcpy of what there is into a zeroed
    /// struct).
    fn parse_short(bytes: &[u8]) -> Self {
        let mut full = vec![0; Self::SIZE];
        let n = bytes.len().min(Self::SIZE);
        full[..n].copy_from_slice(&bytes[..n]);
        Self::read_from(&full)
    }
}

/// `wire_struct! { /// doc  pub struct Name { pub field: u32, ... } }`: the struct, Default, and its [`Wire`].
#[macro_export]
macro_rules! wire_struct {
    ($(#[$m:meta])* pub struct $name:ident { $($(#[$fm:meta])* pub $f:ident: $t:ty),* $(,)? }) => {
        $(#[$m])*
        #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
        pub struct $name { $($(#[$fm])* pub $f: $t),* }

        impl $crate::proto::wire::Wire for $name {
            const SIZE: usize = 0 $(+ <$t as $crate::proto::wire::Le>::SIZE)*;
            #[allow(clippy::vec_init_then_push)] // each field's offset is its predecessor's plus its size
            fn layout() -> ::std::vec::Vec<(&'static str, usize, usize)> {
                let mut at = 0;
                let mut out = ::std::vec::Vec::new();
                $(
                    out.push((stringify!($f), at, <$t as $crate::proto::wire::Le>::SIZE));
                    at += <$t as $crate::proto::wire::Le>::SIZE;
                )*
                let _ = at;
                out
            }
            #[allow(unused_assignments)]
            fn write_to(&self, out: &mut [u8]) {
                let mut at = 0;
                $(
                    $crate::proto::wire::Le::put(self.$f, &mut out[at..]);
                    at += <$t as $crate::proto::wire::Le>::SIZE;
                )*
            }
            #[allow(unused_assignments)]
            fn read_from(bytes: &[u8]) -> Self {
                let mut at = 0;
                $(
                    let $f = <$t as $crate::proto::wire::Le>::get(&bytes[at..]);
                    at += <$t as $crate::proto::wire::Le>::SIZE;
                )*
                $name { $($f),* }
            }
        }
    };
}
