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
    value.into_raw()
}

pub fn u256_twos_complement_to_i256(value: U256) -> I256 {
    I256::from_raw(value)
}

fn fixed_bytes_to_u256<const N: usize>(bytes: FixedBytes<N>) -> U256 {
    U256::from_be_slice(bytes.as_slice())
}

#[cfg(test)]
mod tests {
    use revm::primitives::{Address, address};

    use super::{inverse_remap_l1_address, remap_l1_address};

    #[test]
    fn l1_address_aliasing_round_trips_and_wraps_at_160_bits() {
        for address in [
            Address::ZERO,
            Address::with_last_byte(1),
            address!("1234567890abcdef1234567890abcdef12345678"),
            Address::from([0xff; 20]),
        ] {
            let aliased = remap_l1_address(address).unwrap();
            assert_eq!(inverse_remap_l1_address(aliased).unwrap(), address);
        }
        assert_eq!(
            remap_l1_address(Address::ZERO).unwrap(),
            address!("1111000000000000000000000000000000001111")
        );
    }
}
