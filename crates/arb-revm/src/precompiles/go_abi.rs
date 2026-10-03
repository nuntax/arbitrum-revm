//! Nitro decodes precompile arguments with go-ethereum's `abi.Arguments.Unpack`
//! (`accounts/abi/unpack.go`). That decoder is stricter than alloy's default `abi_decode` in two
//! places and looser in none that the precompile ABIs can reach:
//!
//! * `uint8/16/32/64` and `int64` words must fit their type (`errBadUint64`, ...); alloy truncates.
//! * a `bool` word must be exactly 0 or 1 (`errBadBool`); alloy reads any non-zero word as true.
//!
//! Addresses with dirty high bytes, `bytes32`, `bytes`, `string` (raw bytes, no UTF-8 check) and
//! trailing calldata are accepted by both. A Go decode failure ends the call with
//! `ErrExecutionReverted` and no gas left, before the method body runs, so an owner setter given an
//! out-of-range word must revert rather than store the truncated value. [`args_ok`] replays Go's
//! walk over the calldata, offsets and bounds included, and answers whether `Unpack` succeeds.
//!
//! The parameter types come from alloy's generated `SIGNATURE`, which is the canonical Solidity
//! signature (`f(uint64,(uint8,uint64)[])`), the same type text go-ethereum parses from the ABI.

#[derive(Debug, Clone, PartialEq, Eq)]
enum Ty {
    Uint(u16),
    Int(u16),
    Bool,
    Address,
    FixedBytes,
    Bytes,
    String,
    Array(Box<Ty>, usize),
    Slice(Box<Ty>),
    Tuple(Vec<Ty>),
}

impl Ty {
    fn is_dynamic(&self) -> bool {
        match self {
            Ty::Bytes | Ty::String | Ty::Slice(_) => true,
            Ty::Array(elem, _) => elem.is_dynamic(),
            Ty::Tuple(elems) => elems.iter().any(Ty::is_dynamic),
            _ => false,
        }
    }

    /// go-ethereum `getTypeSize`: the head size of a statically sized array or tuple, else 32.
    fn head_size(&self) -> usize {
        match self {
            Ty::Array(elem, n) if !elem.is_dynamic() => match **elem {
                Ty::Array(..) | Ty::Tuple(_) => n.saturating_mul(elem.head_size()),
                _ => n.saturating_mul(32),
            },
            Ty::Tuple(elems) if !self.is_dynamic() => elems.iter().map(Ty::head_size).sum(),
            _ => 32,
        }
    }
}

/// Parses the comma-separated parameter list of a canonical signature.
fn parse_params(signature: &str) -> Option<Vec<Ty>> {
    let open = signature.find('(')?;
    let mut p = Parser {
        s: signature.as_bytes(),
        i: open,
    };
    let params = p.tuple()?;
    (p.i == p.s.len()).then_some(params)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn tuple(&mut self) -> Option<Vec<Ty>> {
        if self.s.get(self.i) != Some(&b'(') {
            return None;
        }
        self.i += 1;
        let mut elems = Vec::new();
        if self.s.get(self.i) == Some(&b')') {
            self.i += 1;
            return Some(elems);
        }
        loop {
            elems.push(self.ty()?);
            match self.s.get(self.i)? {
                b',' => self.i += 1,
                b')' => {
                    self.i += 1;
                    return Some(elems);
                }
                _ => return None,
            }
        }
    }

    fn ty(&mut self) -> Option<Ty> {
        let mut ty = if self.s.get(self.i) == Some(&b'(') {
            Ty::Tuple(self.tuple()?)
        } else {
            let start = self.i;
            while self
                .s
                .get(self.i)
                .is_some_and(|c| c.is_ascii_alphanumeric())
            {
                self.i += 1;
            }
            let name = std::str::from_utf8(&self.s[start..self.i]).ok()?;
            match name {
                "bool" => Ty::Bool,
                "address" => Ty::Address,
                "bytes" => Ty::Bytes,
                "string" => Ty::String,
                _ if name.starts_with("uint") => Ty::Uint(name[4..].parse().ok()?),
                _ if name.starts_with("int") => Ty::Int(name[3..].parse().ok()?),
                _ if name.starts_with("bytes") => Ty::FixedBytes,
                _ => return None,
            }
        };
        while self.s.get(self.i) == Some(&b'[') {
            self.i += 1;
            let start = self.i;
            while self.s.get(self.i).is_some_and(u8::is_ascii_digit) {
                self.i += 1;
            }
            let size = &self.s[start..self.i];
            if self.s.get(self.i) != Some(&b']') {
                return None;
            }
            self.i += 1;
            ty = if size.is_empty() {
                Ty::Slice(Box::new(ty))
            } else {
                Ty::Array(Box::new(ty), std::str::from_utf8(size).ok()?.parse().ok()?)
            };
        }
        Some(ty)
    }
}

/// Whether go-ethereum's `Arguments.Unpack` accepts `data` (the calldata after the selector) for
/// the parameters of `signature`. An unparsable signature answers `false`: every signature reaching
/// here is generated from the precompile ABIs, and a test parses all of them.
pub(super) fn args_ok(signature: &str, data: &[u8]) -> bool {
    let Some(params) = parse_params(signature) else {
        return false;
    };
    if data.is_empty() {
        // "abi: attempting to unmarshal an empty string while arguments are expected"
        return params.is_empty();
    }
    tuple_ok(&params, data)
}

/// `UnpackValues` / `forTupleUnpack`: heads are read in order, with statically sized arrays and
/// tuples occupying their whole inline size.
fn tuple_ok(elems: &[Ty], output: &[u8]) -> bool {
    let mut virtual_args = 0usize;
    for (index, elem) in elems.iter().enumerate() {
        let Some(at) = index
            .checked_add(virtual_args)
            .and_then(|w| w.checked_mul(32))
        else {
            return false;
        };
        if !value_ok(at, elem, output) {
            return false;
        }
        if matches!(elem, Ty::Array(..) | Ty::Tuple(_)) && !elem.is_dynamic() {
            virtual_args += elem.head_size() / 32 - 1;
        }
    }
    true
}

fn word_u64(word: &[u8]) -> Option<u64> {
    word[..24]
        .iter()
        .all(|b| *b == 0)
        .then(|| u64::from_be_bytes(word[24..32].try_into().expect("8 bytes")))
}

/// go-ethereum `lengthPrefixPointsTo`: the start and length of a length-prefixed value.
fn length_prefix(index: usize, output: &[u8]) -> Option<(usize, usize)> {
    let offset = word_u64(&output[index..index + 32])?;
    let offset_end = offset.checked_add(32)?;
    if offset_end > output.len() as u64 || offset_end >= 1 << 63 {
        return None;
    }
    let offset_end = offset_end as usize;
    let length = word_u64(&output[offset_end - 32..offset_end])?;
    let total = (offset_end as u64).checked_add(length)?;
    if total >= 1 << 63 || total > output.len() as u64 {
        return None;
    }
    Some((offset_end, length as usize))
}

/// The raw content of the `bytes`/`string` parameter whose head is word `index`, cut out exactly
/// as go-ethereum does (no UTF-8 handling). `None` where Go's unpack would fail.
pub(super) fn raw_bytes_arg(data: &[u8], index: usize) -> Option<&[u8]> {
    let at = index.checked_mul(32)?;
    if at.checked_add(32)? > data.len() {
        return None;
    }
    let (begin, length) = length_prefix(at, data)?;
    data.get(begin..begin + length)
}

/// go-ethereum `toGoType`.
fn value_ok(index: usize, ty: &Ty, output: &[u8]) -> bool {
    if index.checked_add(32).is_none_or(|end| end > output.len()) {
        return false;
    }
    let word = &output[index..index + 32];
    match ty {
        Ty::Tuple(elems) => {
            if ty.is_dynamic() {
                // `tuplePointsTo`
                match word_u64(word) {
                    Some(offset) if offset <= output.len() as u64 && offset < 1 << 63 => {
                        tuple_ok(elems, &output[offset as usize..])
                    }
                    _ => false,
                }
            } else {
                tuple_ok(elems, &output[index..])
            }
        }
        Ty::Slice(elem) => match length_prefix(index, output) {
            Some((begin, length)) => each_ok(elem, &output[begin..], length),
            None => false,
        },
        Ty::Array(elem, size) => {
            if elem.is_dynamic() {
                // Go reads only the low eight bytes of the offset word here.
                let offset = u64::from_be_bytes(word[24..32].try_into().expect("8 bytes"));
                if offset > output.len() as u64 {
                    return false;
                }
                each_ok(elem, &output[offset as usize..], *size)
            } else {
                each_ok(elem, &output[index..], *size)
            }
        }
        Ty::Bytes | Ty::String => length_prefix(index, output).is_some(),
        Ty::Uint(bits) => match bits {
            8 | 16 | 32 | 64 => word_u64(word).is_some_and(|v| *bits == 64 || v >> bits == 0),
            _ => true,
        },
        Ty::Int(bits) => match bits {
            8 | 16 | 32 | 64 => {
                // Sign-extended two's complement that fits in `bits`.
                let negative = word[0] & 0x80 != 0;
                let fill = if negative { 0xff } else { 0x00 };
                let width = usize::from(*bits / 8);
                word[..32 - width].iter().all(|b| *b == fill)
                    && (word[32 - width] & 0x80 != 0) == negative
            }
            _ => true,
        },
        Ty::Bool => word[..31].iter().all(|b| *b == 0) && word[31] <= 1,
        Ty::Address | Ty::FixedBytes => true,
    }
}

/// go-ethereum `forEachUnpack`.
fn each_ok(elem: &Ty, output: &[u8], size: usize) -> bool {
    if size.checked_mul(32).is_none_or(|n| n > output.len()) {
        return false;
    }
    let step = elem.head_size();
    let mut at = 0usize;
    for _ in 0..size {
        if !value_ok(at, elem, output) {
            return false;
        }
        at = match at.checked_add(step) {
            Some(next) => next,
            None => return false,
        };
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn word(tail: &[u8]) -> Vec<u8> {
        let mut w = vec![0u8; 32 - tail.len()];
        w.extend_from_slice(tail);
        w
    }

    #[test]
    fn parses_canonical_signatures() {
        assert_eq!(
            parse_params("f(uint64,(uint8,uint64)[],uint64[3][])"),
            Some(vec![
                Ty::Uint(64),
                Ty::Slice(Box::new(Ty::Tuple(vec![Ty::Uint(8), Ty::Uint(64)]))),
                Ty::Slice(Box::new(Ty::Array(Box::new(Ty::Uint(64)), 3))),
            ])
        );
        assert_eq!(parse_params("f()"), Some(vec![]));
        assert_eq!(parse_params("f(uint64"), None);
    }

    #[test]
    fn integers_and_bools_must_fit_like_go() {
        assert!(args_ok("f(uint64)", &word(&[0xff; 8])));
        assert!(!args_ok("f(uint64)", &word(&[1, 0, 0, 0, 0, 0, 0, 0, 0])));
        assert!(args_ok(
            "f(uint8,uint16)",
            &[word(&[0xff]), word(&[0xff, 0xff])].concat()
        ));
        assert!(!args_ok(
            "f(uint8,uint16)",
            &[word(&[1, 0]), word(&[1])].concat()
        ));
        assert!(!args_ok(
            "f(uint8,uint16)",
            &[word(&[1]), word(&[1, 0, 0])].concat()
        ));
        assert!(args_ok("f(bool)", &word(&[1])));
        assert!(!args_ok("f(bool)", &word(&[2])));
        assert!(!args_ok(
            "f(bool)",
            &[vec![1], vec![0; 30], vec![1]].concat()
        ));
        // int64: -1 is all ones; a positive value with the int64 sign bit set does not fit.
        assert!(args_ok("f(int64)", &[0xffu8; 32]));
        assert!(!args_ok("f(int64)", &word(&[0x80, 0, 0, 0, 0, 0, 0, 0])));
        assert!(args_ok("f(uint256)", &[0xffu8; 32]));
    }

    #[test]
    fn addresses_and_trailing_bytes_are_lax_like_go() {
        assert!(args_ok("f(address)", &[0xffu8; 32]));
        assert!(args_ok("f(address)", &[word(&[1]), vec![1, 2, 3]].concat()));
        assert!(!args_ok("f(address)", &[0u8; 31]));
        assert!(!args_ok("f(address)", &[]));
        assert!(args_ok("f()", &[]));
    }

    #[test]
    fn dynamic_values_follow_go_bounds() {
        // string: offset 0x20, length 2, raw non-UTF-8 bytes are fine.
        let mut s = [word(&[0x20]), word(&[2])].concat();
        s.extend([0xff, 0xfe]);
        assert!(args_ok("f(string)", &s));
        // Go only needs the content bytes, not the padding.
        assert!(args_ok("f(bytes)", &s));
        // Length runs past the end.
        let long = [word(&[0x20]), word(&[3]), vec![0xff, 0xfe]].concat();
        assert!(!args_ok("f(bytes)", &long));

        // uint64[3][] with one element whose middle word overflows uint64.
        let ok = [
            word(&[0x20]),
            word(&[1]),
            word(&[1]),
            word(&[2]),
            word(&[3]),
        ]
        .concat();
        assert!(args_ok("f(uint64[3][])", &ok));
        let bad = [
            word(&[0x20]),
            word(&[1]),
            word(&[1]),
            word(&[1, 0, 0, 0, 0, 0, 0, 0, 0]),
            word(&[3]),
        ]
        .concat();
        assert!(!args_ok("f(uint64[3][])", &bad));
    }

    #[test]
    fn nested_dynamic_tuples_are_walked() {
        // f(((uint8,uint64)[],uint32,uint64,uint64)[]) with one constraint holding one resource.
        let sig = "f(((uint8,uint64)[],uint32,uint64,uint64)[])";
        let constraint = |kind: Vec<u8>| {
            [
                word(&[0x80]), // offset of `resources` within the tuple
                word(&[60]),
                word(&[0x01, 0x00]),
                word(&[0]),
                word(&[1]), // resources.length
                kind,
                word(&[100]),
            ]
            .concat()
        };
        let encode = |c: Vec<u8>| [word(&[0x20]), word(&[1]), word(&[0x20]), c].concat();
        assert!(args_ok(sig, &encode(constraint(word(&[1])))));
        assert!(!args_ok(sig, &encode(constraint(word(&[1, 0])))));
    }
}
