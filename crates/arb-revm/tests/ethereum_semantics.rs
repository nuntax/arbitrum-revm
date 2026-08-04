use revm::precompile::secp256k1::ec_recover_run;

/// Nitro accepts high-S values in the ECRECOVER precompile even though EIP-2 rejects them in
/// transaction signatures. Keep the inherited revm behavior pinned because changing crypto
/// backends or normalizing signatures here would create an execution divergence.
#[test]
fn ecrecover_accepts_nitro_high_s_vector() {
    let mut input = [0_u8; 128];
    input[..32].copy_from_slice(
        &hex::decode("9ddf4164c1e7c21799d886f019ee485c4ba01cdff1ac360b797da8b15212a111").unwrap(),
    );
    input[63] = 27;
    input[95] = 1;
    input[96..].copy_from_slice(
        &hex::decode("fffffffffffffffffffffffffffffffebaaedce6af48a03bbfd25e8cd0364140").unwrap(),
    );

    let output = ec_recover_run(&input, 3_000).unwrap();
    assert_eq!(output.gas_used, 3_000);
    assert_eq!(output.bytes.len(), 32);
}
