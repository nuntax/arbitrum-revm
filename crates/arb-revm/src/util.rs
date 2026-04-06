use eyre::Result;
use revm::primitives::{Address, FixedBytes, I256, U256};

use crate::constants::ADDRESS_ALIAS_OFFSET_HEX;

pub fn address_to_u256(address: Address) -> U256 {
    let mut bytes = [0_u8; 32];
    bytes[12..].copy_from_slice(address.as_slice());
    U256::from_be_bytes(bytes)
}

pub fn remap_l1_address(l1_address: Address) -> Result<Address> {
    let offset = U256::from_be_slice(hex::decode(ADDRESS_ALIAS_OFFSET_HEX)?.as_slice());
    let remapped = fixed_bytes_to_u256(l1_address.into()).wrapping_add(offset);
    let bytes = remapped.to_be_bytes::<32>();
    Ok(Address::from_slice(&bytes[12..]))
}

pub fn inverse_remap_l1_address(aliased_address: Address) -> Result<Address> {
    let offset = U256::from_be_slice(hex::decode(ADDRESS_ALIAS_OFFSET_HEX)?.as_slice());
    let inverse_offset = (U256::from(1) << 160) - offset;
    let unaliased = fixed_bytes_to_u256(aliased_address.into()).wrapping_add(inverse_offset);
    let bytes = unaliased.to_be_bytes::<32>();
    Ok(Address::from_slice(&bytes[12..]))
}

pub fn i256_to_u256_twos_complement(value: I256) -> U256 {
    if value >= I256::ZERO {
        U256::from(value)
    } else {
        let abs = (-value).unsigned_abs();
        U256::ZERO.wrapping_sub(U256::from(abs))
    }
}

pub fn u256_twos_complement_to_i256(value: U256) -> I256 {
    let two_to_255 = U256::ONE << 255;
    if value < two_to_255 {
        I256::from(value)
    } else {
        let abs = U256::MAX - value + U256::ONE;
        -I256::from(abs)
    }
}

fn fixed_bytes_to_u256<const N: usize>(bytes: FixedBytes<N>) -> U256 {
    U256::from_be_slice(bytes.as_slice())
}
