use std::marker::PhantomData;

use p3_field::{ExtensionField, Field, PrimeField64, TwoAdicField};
use p3_symmetric::{Hash, Permutation};
use serde::Deserialize;

use super::{
    UnitToBytes,
    domain_separator::DomainSeparator,
    duplex_sponge::interface::DuplexSpongeInterface,
    errors::{DomainSeparatorMismatch, ProofError, ProofResult},
    pow::traits::PowStrategy,
    sho::HashStateWithInstructions,
    utils::{bytes_uniform_modp, from_be_bytes_mod_order, from_le_bytes_mod_order},
};
use crate::fiat_shamir::duplex_sponge::interface::Unit;

/// [`VerifierState`] is the verifier state.
///
/// Internally, it simply contains a stateful hash.
/// Given as input an [`DomainSeparator`] and a NARG string, it allows to
/// de-serialize elements from the NARG string and make them available to the zero-knowledge
/// verifier.
#[derive(Debug)]
pub struct VerifierState<'a, EF, F, Perm, H, U, const WIDTH: usize>
where
    U: Unit,
    Perm: Permutation<[U; WIDTH]>,
    H: DuplexSpongeInterface<Perm, U, WIDTH>,
{
    /// Internal sponge transcript that tracks the domain separator state and absorbs values.
    ///
    /// This manages the full Fiat-Shamir interaction logic, such as absorbing inputs and
    /// squeezing challenges. It also stores the domain separator instructions to enforce
    /// consistency between prover and verifier.
    pub(crate) hash_state: HashStateWithInstructions<H, Perm, U, WIDTH>,

    /// The "NARG" string: raw serialized input provided by the prover.
    ///
    /// This byte slice contains encoded values (scalars, digests, etc.) that are deserialized
    /// during verification. Each call to `next_units`, `fill_next_scalars`, etc., reads from this.
    pub(crate) narg_string: &'a [u8],

    /// Marker for the base field `F`.
    ///
    /// This field is never read or written; it ensures type correctness for field-level operations.
    _field: PhantomData<F>,

    /// Marker for the extension field `EF`.
    ///
    /// Like `_field`, this is only for type-level bookkeeping. The extension field is used
    /// to deserialize and operate on scalars in high-dimensional protocols.
    _extension_field: PhantomData<EF>,
}

impl<'a, EF, F, Perm, H, U, const WIDTH: usize> VerifierState<'a, EF, F, Perm, H, U, WIDTH>
where
    U: Unit + Default + Copy,
    Perm: Permutation<[U; WIDTH]>,
    H: DuplexSpongeInterface<Perm, U, WIDTH>,
    EF: ExtensionField<F> + TwoAdicField,
    F: PrimeField64 + TwoAdicField,
{
    /// Creates a new [`VerifierState`] instance with the given sponge and IO Pattern.
    ///
    /// The resulting object will act as the verifier in a zero-knowledge protocol.
    ///
    /// ```ignore
    /// # use spongefish::*;
    ///
    /// let domsep = DomainSeparator::<DefaultHash>::new("📝").absorb(1, "inhale 🫁").squeeze(32, "exhale 🎏");
    /// // A silly NARG string for the example.
    /// let narg_string = &[0x42];
    /// let mut verifier_state = domsep.to_verifier_state::<H,32>(narg_string);
    /// assert_eq!(verifier_state.next_units().unwrap(), [0x42]);
    /// let challenge = verifier_state.challenge_units::<32>();
    /// assert!(challenge.is_ok());
    /// assert_ne!(challenge.unwrap(), [0; 32]);
    /// ```
    #[must_use]
    pub fn new<const IV_SIZE: usize>(
        domain_separator: &DomainSeparator<EF, F, Perm, U, WIDTH>,
        narg_string: &'a [u8],
        perm: Perm,
    ) -> Self {
        Self {
            hash_state: HashStateWithInstructions::new::<_, _, IV_SIZE>(domain_separator, perm),
            narg_string,
            _field: PhantomData,
            _extension_field: PhantomData,
        }
    }

    /// Read `input.len()` bytes from the NARG transcript and absorb them.
    #[inline]
    pub fn fill_next_units(&mut self, input: &mut [U]) -> Result<(), DomainSeparatorMismatch> {
        U::read(&mut self.narg_string, input)?;
        self.hash_state.absorb(input)?;
        Ok(())
    }

    /// Read a fixed-size byte array from the transcript.
    pub fn next_units<const N: usize>(&mut self) -> Result<[U; N], DomainSeparatorMismatch> {
        let mut input = [U::default(); N];
        self.fill_next_units(&mut input)?;
        Ok(input)
    }

    /// Deserialize a list of extension scalars from the transcript and absorb them.
    pub fn fill_next_scalars(&mut self, output: &mut [EF]) -> ProofResult<()> {
        // Size of one base field element in bytes
        let base_bytes = F::NUM_BYTES;

        // Number of coefficients (1 for base field, >1 for extension field)
        let ext_degree = EF::DIMENSION;

        // Size of full F element = D * base field size
        let scalar_size = ext_degree * base_bytes;

        // Temporary buffer to deserialize each F element
        let mut u_buf = vec![U::default(); scalar_size];

        for out in output.iter_mut() {
            // Fetch the next group of bytes from the transcript
            self.fill_next_units(&mut u_buf)?;

            // Convert &[U] → &[u8]
            let byte_buf: &[u8] = U::slice_to_u8_slice(&u_buf);

            // Interpret each chunk as a base field coefficient
            let coeffs = byte_buf.chunks(base_bytes).map(from_le_bytes_mod_order);

            // Reconstruct the field element from its base field coefficients
            *out = EF::from_basis_coefficients_iter(coeffs).unwrap();
        }

        Ok(())
    }

    /// Read `N` extension scalars from the transcript.
    pub fn next_scalars<const N: usize>(&mut self) -> ProofResult<[EF; N]> {
        let mut output = [EF::default(); N];
        self.fill_next_scalars(&mut output)?;
        Ok(output)
    }

    /// Perform a PoW challenge check using a derived challenge and 64-bit nonce.
    pub fn challenge_pow<S: PowStrategy>(&mut self, bits: f64) -> ProofResult<()> {
        let challenge = self.challenge_units()?;
        let nonce = u64::from_be_bytes(U::array_to_u8_array(&self.next_units()?));
        if S::new(U::array_to_u8_array(&challenge), bits).check(nonce) {
            Ok(())
        } else {
            Err(ProofError::InvalidProof)
        }
    }

    /// Read a digest from the transcript as raw bytes.
    pub fn read_digest<const DIGEST_ELEMS: usize>(
        &mut self,
    ) -> ProofResult<Hash<F, U, DIGEST_ELEMS>> {
        let mut digest = [U::default(); DIGEST_ELEMS];
        self.fill_next_units(&mut digest)?;
        Ok(digest.into())
    }

    /// Derive a fixed-size byte array from the sponge as a Fiat-Shamir challenge.
    pub fn challenge_units<const N: usize>(&mut self) -> Result<[U; N], DomainSeparatorMismatch> {
        let mut output = [U::default(); N];
        self.fill_challenge_units(&mut output)?;
        Ok(output)
    }

    /// Sample extension scalars uniformly at random using Fiat-Shamir challenge output.
    pub fn fill_challenge_scalars(&mut self, output: &mut [EF]) -> ProofResult<()> {
        // How many bytes are needed to sample a single base field element
        let base_field_size = bytes_uniform_modp(F::bits() as u32);

        // Total bytes needed for one EF element = extension degree × base field size
        let field_unit_len = EF::DIMENSION * base_field_size;

        // Temporary buffer to hold bytes for each field element
        let mut u_buf = vec![U::default(); field_unit_len];

        // Fill each output element from fresh transcript randomness
        for o in output.iter_mut() {
            // Draw uniform bytes from the transcript
            self.fill_challenge_units(&mut u_buf)?;

            // Reinterpret as bytes (safe because U must be u8-width)
            let byte_buf = U::slice_to_u8_slice(&u_buf);

            // For each chunk, convert to base field element via modular reduction
            let base_coeffs = byte_buf
                .chunks(base_field_size)
                .map(from_be_bytes_mod_order);

            // Reconstruct the full field element using canonical basis
            *o = EF::from_basis_coefficients_iter(base_coeffs).unwrap();
        }

        Ok(())
    }

    /// Sample `N` extension scalars using Fiat-Shamir challenge randomness.
    pub fn challenge_scalars<const N: usize>(&mut self) -> ProofResult<[EF; N]> {
        let mut output = [EF::default(); N];
        self.fill_challenge_scalars(&mut output)?;
        Ok(output)
    }

    /// Serialize and absorb public scalar values into the sponge, returning their byte encoding.
    pub fn public_scalars(&mut self, input: &[EF]) -> ProofResult<Vec<u8>> {
        // Build the byte vector by flattening all basis coefficients.
        //
        // For each extension field element:
        // - Decompose it into its canonical basis over the base field (returns a slice of F coefficients).
        //
        // For each base field coefficient:
        // - Convert it to a canonical little-endian u64 byte array (8 bytes).
        // - Truncate the byte array to `num_bytes` (only keep the low significant part).
        // - Collect all these truncated bytes into a flat vector.
        //
        // Example:
        // - BabyBear: one limb → 4 bytes.
        // - EF4 over BabyBear: 4 limbs → 16 bytes.
        let bytes: Vec<u8> = input
            .iter()
            .flat_map(p3_field::BasedVectorSpace::as_basis_coefficients_slice)
            .flat_map(|coeff| coeff.as_canonical_u64().to_le_bytes()[..F::NUM_BYTES].to_vec())
            .collect();

        // Absorb the serialized bytes into the Fiat-Shamir transcript sponge
        self.hash_state.absorb(&U::slice_from_u8_slice(&bytes))?;

        // Return the serialized byte representation
        Ok(bytes)
    }

    /// Read a hint from the NARG string. Returns the number of units read.
    pub fn hint_bytes(&mut self) -> Result<&'a [u8], DomainSeparatorMismatch> {
        self.hash_state.hint()?;

        // Ensure at least 4 bytes are available for the length prefix
        if self.narg_string.len() < 4 {
            return Err("Insufficient transcript remaining for hint".into());
        }

        // Read 4-byte little-endian length prefix
        let len = u32::from_le_bytes(self.narg_string[..4].try_into().unwrap()) as usize;
        let rest = &self.narg_string[4..];

        // Ensure the rest of the slice has `len` bytes
        if rest.len() < len {
            return Err(format!(
                "Insufficient transcript remaining, got {}, need {len}",
                rest.len()
            )
            .into());
        }

        // Split the hint and advance the transcript
        let (hint, remaining) = rest.split_at(len);
        self.narg_string = remaining;

        Ok(hint)
    }

    /// Deserialize and return a structured hint from the transcript.
    ///
    /// This function reads the next hint from the prover-supplied NARG string,
    /// which was previously serialized using `bincode` and written via `ProverState::hint`.
    ///
    /// The expected format is:
    /// - 4-byte little-endian length prefix
    /// - Followed by that many bytes of bincode-encoded data
    ///
    /// It verifies that the current domain separator instruction expects a hint,
    /// then attempts to decode the object.
    ///
    /// # Type Parameters
    /// - `T`: A type implementing `serde::Deserialize`, such as field elements,
    ///   extension fields, or vectors of such types.
    ///
    /// # Errors
    /// - Returns `ProofError::SerializationError` if decoding fails
    /// - Returns `DomainSeparatorMismatch` if the domain separator doesn't expect a hint
    pub fn hint<T: for<'de> Deserialize<'de>>(&mut self) -> ProofResult<T> {
        // Read the raw bytes for the next hint. This enforces that a `.hint()` op was expected,
        // and parses a 4-byte little-endian length prefix followed by the encoded payload.
        let bytes = self.hint_bytes()?;

        // Decode the bincode-encoded value into a strongly typed structure of type `T`.
        // We discard the second return value (number of bytes read), because it's known.
        let (value, _) = bincode::serde::decode_from_slice(bytes, bincode::config::standard())
            .map_err(|_| ProofError::SerializationError)?;

        // Return the successfully decoded value.
        Ok(value)
    }
}

impl<EF, F, Perm, H, U, const WIDTH: usize> UnitToBytes<U>
    for VerifierState<'_, EF, F, Perm, H, U, WIDTH>
where
    U: Unit + Default + Copy,
    Perm: Permutation<[U; WIDTH]>,
    H: DuplexSpongeInterface<Perm, U, WIDTH>,
    EF: ExtensionField<F>,
    F: Field,
{
    #[inline]
    fn fill_challenge_units(&mut self, input: &mut [U]) -> Result<(), DomainSeparatorMismatch> {
        self.hash_state.squeeze(input)
    }
}

#[cfg(test)]
#[allow(clippy::unreadable_literal)]
mod tests {
    use std::{cell::RefCell, rc::Rc};

    use p3_baby_bear::BabyBear;
    use p3_field::{BasedVectorSpace, PrimeCharacteristicRing, extension::BinomialExtensionField};
    use p3_goldilocks::Goldilocks;
    use p3_keccak::KeccakF;
    use rand::{Rng, SeedableRng, rngs::SmallRng};

    use super::*;
    use crate::fiat_shamir::{DefaultHash, DefaultPerm};

    type F = BabyBear;
    type EF4 = BinomialExtensionField<F, 4>;

    type G = Goldilocks;
    type EG2 = BinomialExtensionField<G, 2>;

    type H = DefaultHash;

    #[derive(Default, Clone)]
    struct DummySponge {
        pub absorbed: Rc<RefCell<Vec<u8>>>,
        pub squeezed: Rc<RefCell<Vec<u8>>>,
        pub ratcheted: Rc<RefCell<bool>>,
    }

    impl zeroize::Zeroize for DummySponge {
        fn zeroize(&mut self) {
            self.absorbed.borrow_mut().clear();
            self.squeezed.borrow_mut().clear();
            *self.ratcheted.borrow_mut() = false;
        }
    }

    impl DummySponge {
        fn new_inner() -> Self {
            Self {
                absorbed: Rc::new(RefCell::new(Vec::new())),
                squeezed: Rc::new(RefCell::new(Vec::new())),
                ratcheted: Rc::new(RefCell::new(false)),
            }
        }
    }

    impl DuplexSpongeInterface<KeccakF, u8, 200> for DummySponge {
        fn new<const IV_SIZE: usize>(_keccak: KeccakF, _iv: [u8; IV_SIZE]) -> Self {
            Self::new_inner()
        }

        fn absorb_unchecked(&mut self, input: &[u8]) -> &mut Self {
            self.absorbed.borrow_mut().extend_from_slice(input);
            self
        }

        fn squeeze_unchecked(&mut self, output: &mut [u8]) -> &mut Self {
            for (i, byte) in output.iter_mut().enumerate() {
                *byte = i as u8;
            }
            self.squeezed.borrow_mut().extend_from_slice(output);
            self
        }
    }

    #[test]
    fn test_new_verifier_state_constructs_correctly() {
        let ds = DomainSeparator::<F, F, DefaultPerm, u8, 200>::new("test", KeccakF);
        let transcript = b"abc";
        let vs = VerifierState::<F, F, _, DefaultHash, _, 200>::new::<32>(&ds, transcript, KeccakF);
        assert_eq!(vs.narg_string, b"abc");
    }

    #[test]
    fn test_fill_next_units_reads_and_absorbs() {
        let mut ds = DomainSeparator::<F, F, DefaultPerm, u8, 200>::new("x", KeccakF);
        ds.absorb(3, "input");
        let mut vs =
            VerifierState::<F, F, _, DummySponge, u8, 200>::new::<32>(&ds, b"abc", KeccakF);
        let mut buf = [0u8; 3];
        let res = vs.fill_next_units(&mut buf);
        assert!(res.is_ok());
        assert_eq!(buf, *b"abc");
        assert_eq!(*vs.hash_state.ds.absorbed.borrow(), b"abc");
    }

    #[test]
    fn test_fill_next_units_with_insufficient_data_errors() {
        let mut ds = DomainSeparator::<F, F, DefaultPerm, u8, 200>::new("x", KeccakF);
        ds.absorb(4, "fail");
        let mut vs = VerifierState::<F, F, _, DummySponge, u8, 200>::new::<32>(&ds, b"xy", KeccakF);
        let mut buf = [0u8; 4];
        let res = vs.fill_next_units(&mut buf);
        assert!(res.is_err());
    }

    #[test]
    fn test_unit_transcript_fill_challenge_bytes() {
        let mut ds = DomainSeparator::<F, F, DefaultPerm, u8, 200>::new("x", KeccakF);
        ds.squeeze(4, "c");
        let mut vs =
            VerifierState::<F, F, _, DummySponge, u8, 200>::new::<32>(&ds, b"abcd", KeccakF);
        let mut out = [0u8; 4];
        assert!(vs.fill_challenge_units(&mut out).is_ok());
        assert_eq!(out, [0, 1, 2, 3]);
    }

    #[test]
    fn test_fill_next_units_impl() {
        let mut ds = DomainSeparator::<F, F, DefaultPerm, u8, 200>::new("x", KeccakF);
        ds.absorb(3, "byte");
        let mut vs =
            VerifierState::<F, F, _, DummySponge, u8, 200>::new::<32>(&ds, b"xyz", KeccakF);
        let mut out = [0u8; 3];
        assert!(vs.fill_next_units(&mut out).is_ok());
        assert_eq!(out, *b"xyz");
    }

    #[test]
    fn test_fill_next_scalars_babybear() {
        // Step 1: Define two known F scalars to test deserialization
        let values = [F::from_u64(123), F::from_u64(456)];

        // Step 2: Manually serialize the scalars to raw bytes in little-endian u32 format
        // This matches the encoding done in `public_scalars`

        // How many bytes are needed to sample a single base field element
        let num_bytes = F::bits().div_ceil(8);

        let mut raw_bytes = vec![];
        for x in &values {
            let bytes = x.as_canonical_u64().to_le_bytes();
            raw_bytes.extend_from_slice(&bytes[..num_bytes]);
        }

        // Step 3: Create a domain separator that commits to absorbing 2 scalars
        // The label "scalars" is just metadata to distinguish this absorb phase
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("test", KeccakF);
        domsep.add_scalars(values.len(), "scalars");

        // Step 4: Create a verifier from the domain separator, loaded with the raw bytes
        let mut verifier = domsep.to_verifier_state::<H, 32>(&raw_bytes);

        // Step 5: Allocate output buffer and deserialize scalars from transcript
        let mut out = [F::ZERO; 2];
        verifier.fill_next_scalars(&mut out).unwrap();

        // Step 6: Check that deserialized scalars exactly match original input
        assert_eq!(out, values);
    }

    #[test]
    fn test_fill_next_scalars_ef4() {
        // Step 1: Construct two known EF4 extension field elements from explicit basis coefficients
        // These test that field elements composed of multiple base field limbs are correctly parsed
        let ef0 = EF4::from_basis_coefficients_iter(
            [
                F::from_u64(16231546437525696111),
                F::from_u64(3260480306969229290),
                F::from_u64(16069356457323344778),
                F::from_u64(18093877879687808447),
            ]
            .into_iter(),
        )
        .unwrap();

        let ef1 = EF4::from_basis_coefficients_iter(
            [
                F::from_u64(4745602262162961622),
                F::from_u64(7823278364822041281),
                F::from_u64(2045790219489339023),
                F::from_u64(9614754510566682848),
            ]
            .into_iter(),
        )
        .unwrap();

        // Step 2: Store the known expected values into a slice
        let values = [ef0, ef1];

        // Step 3: Precomputed raw bytes matching the encoding of `public_scalars`
        // Each EF4 element has 4 F limbs, each limb serialized as 4 LE bytes
        // Total = 2 elements * 4 limbs * 4 bytes = 32 bytes
        let raw_bytes = vec![
            106, 13, 109, 83, // limb 0 of ef0
            132, 35, 135, 77, // limb 1 of ef0
            127, 148, 35, 12, // limb 2 of ef0
            40, 78, 103, 12, // limb 3 of ef0
            153, 244, 3, 21, // limb 0 of ef1
            244, 220, 153, 42, // limb 1 of ef1
            30, 27, 16, 97, // limb 2 of ef1
            224, 9, 66, 40, // limb 3 of ef1
        ];

        // Step 4: Create a domain separator for absorbing 2 EF4 values
        let mut domsep: DomainSeparator<EF4, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("ext", KeccakF);
        domsep.add_scalars(values.len(), "ext-scalars");

        // Step 5: Construct a verifier state from the domain separator and raw byte input
        let mut verifier = domsep.to_verifier_state::<H, 32>(&raw_bytes);

        // Step 6: Allocate an output array and deserialize into it from the verifier
        let mut out = [EF4::ZERO; 2];
        verifier.fill_next_scalars(&mut out).unwrap();

        // Step 7: Ensure the decoded extension field elements match the original values
        assert_eq!(out, values);
    }

    #[test]
    fn scalar_challenge_single_basefield_case_1() {
        // Generate a domain separator with known tag and one challenge scalar
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("chal", KeccakF);
        domsep.challenge_scalars(1, "tag");
        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);

        // Sample a single scalar
        let mut out = [F::ZERO; 1];
        prover.fill_challenge_scalars(&mut out).unwrap();

        // Expected value checked with reference implementation
        assert_eq!(out, [F::from_u64(941695054)]);
    }

    #[test]
    fn scalar_challenge_single_basefield_case_2() {
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("chal2", KeccakF);
        domsep.challenge_scalars(1, "tag");
        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);

        let mut out = [F::ZERO; 1];
        prover.fill_challenge_scalars(&mut out).unwrap();

        assert_eq!(out, [F::from_u64(1368000007)]);
    }

    #[test]
    fn scalar_challenge_multiple_basefield_scalars() {
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("chal", KeccakF);
        domsep.challenge_scalars(10, "tag");
        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);

        let mut out = [F::ZERO; 10];
        prover.fill_challenge_scalars(&mut out).unwrap();

        assert_eq!(
            out,
            [
                F::from_u64(1339394730),
                F::from_u64(299253387),
                F::from_u64(639309475),
                F::from_u64(291978),
                F::from_u64(693273190),
                F::from_u64(79969777),
                F::from_u64(1282539175),
                F::from_u64(1950046278),
                F::from_u64(1245120766),
                F::from_u64(1108619098)
            ]
        );
    }

    #[test]
    fn scalar_challenge_single_extension_scalar() {
        let mut domsep: DomainSeparator<EF4, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("chal", KeccakF);
        domsep.challenge_scalars(1, "tag");
        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);

        let mut out = [EF4::ZERO; 1];
        prover.fill_challenge_scalars(&mut out).unwrap();

        let expected = EF4::from_basis_coefficients_iter(
            [
                F::new(766723793),
                F::new(142148826),
                F::new(1747592655),
                F::new(1079604003),
            ]
            .into_iter(),
        )
        .unwrap();

        assert_eq!(out, [expected]);
    }

    #[test]
    fn scalar_challenge_multiple_extension_scalars() {
        let mut domsep: DomainSeparator<EF4, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("chal", KeccakF);
        domsep.challenge_scalars(5, "tag");
        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);

        let mut out = [EF4::ZERO; 5];
        prover.fill_challenge_scalars(&mut out).unwrap();

        let ef0 = EF4::from_basis_coefficients_iter(
            [
                F::new(221219480),
                F::new(1982332342),
                F::new(625475973),
                F::new(421782538),
            ]
            .into_iter(),
        )
        .unwrap();

        let ef1 = EF4::from_basis_coefficients_iter(
            [
                F::new(1967478349),
                F::new(966593806),
                F::new(1839663095),
                F::new(878608238),
            ]
            .into_iter(),
        )
        .unwrap();

        let ef2 = EF4::from_basis_coefficients_iter(
            [
                F::new(1330039744),
                F::new(410562161),
                F::new(825994336),
                F::new(1112934023),
            ]
            .into_iter(),
        )
        .unwrap();

        let ef3 = EF4::from_basis_coefficients_iter(
            [
                F::new(111882429),
                F::new(1246071646),
                F::new(1718768295),
                F::new(1127778746),
            ]
            .into_iter(),
        )
        .unwrap();

        let ef4 = EF4::from_basis_coefficients_iter(
            [
                F::new(1533982496),
                F::new(1606406037),
                F::new(1075981915),
                F::new(1199951082),
            ]
            .into_iter(),
        )
        .unwrap();

        // Result obtained via a script to double check the result
        assert_eq!(out, [ef0, ef1, ef2, ef3, ef4]);
    }

    #[test]
    fn test_common_field_to_unit_bytes_babybear() {
        // Generate some random F values
        let values = [F::from_u64(111), F::from_u64(222)];

        // Create a domain separator indicating we will absorb 2 public scalars
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("field", KeccakF);
        domsep.add_scalars(2, "test");

        // Create prover and serialize expected values manually
        let expected_bytes = [111, 0, 0, 0, 222, 0, 0, 0];

        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);
        let actual = prover.public_scalars(&values).unwrap();

        assert_eq!(
            actual, expected_bytes,
            "Public scalars should serialize to expected bytes"
        );

        // Determinism: same input, same transcript = same output
        let mut prover2 = domsep.to_verifier_state::<H, 32>(&[]);
        let actual2 = prover2.public_scalars(&values).unwrap();

        assert_eq!(
            actual, actual2,
            "Transcript serialization should be deterministic"
        );
    }

    #[test]
    fn test_common_field_to_unit_bytes_goldilocks() {
        // Generate some random Goldilocks values
        let values = [G::from_u64(111), G::from_u64(222)];

        // Create a domain separator indicating we will absorb 2 public scalars
        let mut domsep: DomainSeparator<G, G, DefaultPerm, u8, 200> =
            DomainSeparator::new("field", KeccakF);
        domsep.add_scalars(2, "test");

        // Create prover and serialize expected values manually
        let expected_bytes = [111, 0, 0, 0, 0, 0, 0, 0, 222, 0, 0, 0, 0, 0, 0, 0];

        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);
        let actual = prover.public_scalars(&values).unwrap();

        assert_eq!(
            actual, expected_bytes,
            "Public scalars should serialize to expected bytes"
        );

        // Determinism: same input, same transcript = same output
        let mut prover2 = domsep.to_verifier_state::<H, 32>(&[]);
        let actual2 = prover2.public_scalars(&values).unwrap();

        assert_eq!(
            actual, actual2,
            "Transcript serialization should be deterministic"
        );
    }

    #[test]
    fn test_common_field_to_unit_bytes_babybear_extension() {
        // Construct two extension field elements using known u64 inputs
        let values = [EF4::from_u64(111), EF4::from_u64(222)];

        // Create a domain separator committing to 2 public scalars
        let mut domsep: DomainSeparator<EF4, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("field", KeccakF);
        domsep.add_scalars(2, "test");

        // Compute expected bytes manually: serialize each coefficient of EF4
        let expected_bytes = [
            111, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 222, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ];

        // Serialize the values through the transcript
        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);
        let actual = prover.public_scalars(&values).unwrap();

        // Check that the actual bytes match expected ones
        assert_eq!(
            actual, expected_bytes,
            "Public scalars should serialize to expected bytes"
        );

        // Check determinism: same input = same output
        let mut prover2 = domsep.to_verifier_state::<H, 32>(&[]);
        let actual2 = prover2.public_scalars(&values).unwrap();

        assert_eq!(
            actual, actual2,
            "Transcript serialization should be deterministic"
        );
    }

    #[test]
    fn test_common_field_to_unit_bytes_goldilocks_extension() {
        // Construct two extension field elements using known u64 inputs
        let values = [EG2::from_u64(111), EG2::from_u64(222)];

        // Create a domain separator committing to 2 public scalars
        let mut domsep: DomainSeparator<EG2, G, DefaultPerm, u8, 200> =
            DomainSeparator::new("field", KeccakF);
        domsep.add_scalars(2, "test");

        // Compute expected bytes manually: serialize each coefficient of EF4
        let expected_bytes = [
            111, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 222, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
            0, 0, 0, 0,
        ];

        // Serialize the values through the transcript
        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);
        let actual = prover.public_scalars(&values).unwrap();

        // Check that the actual bytes match expected ones
        assert_eq!(
            actual, expected_bytes,
            "Public scalars should serialize to expected bytes"
        );

        // Check determinism: same input = same output
        let mut prover2 = domsep.to_verifier_state::<H, 32>(&[]);
        let actual2 = prover2.public_scalars(&values).unwrap();

        assert_eq!(
            actual, actual2,
            "Transcript serialization should be deterministic"
        );
    }

    #[test]
    fn test_common_field_to_unit_mixed_values() {
        let values = [F::ZERO, F::ONE, F::from_u64(123456), F::from_u64(7891011)];

        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("mixed", KeccakF);
        domsep.add_scalars(values.len(), "mix");

        let mut prover = domsep.to_verifier_state::<H, 32>(&[]);
        let actual = prover.public_scalars(&values).unwrap();

        let expected = vec![0, 0, 0, 0, 1, 0, 0, 0, 64, 226, 1, 0, 67, 104, 120, 0];

        assert_eq!(actual, expected, "Mixed values should serialize correctly");

        let mut prover2 = domsep.to_verifier_state::<H, 32>(&[]);
        assert_eq!(
            actual,
            prover2.public_scalars(&values).unwrap(),
            "Serialization must be deterministic"
        );
    }

    #[test]
    fn test_hint_bytes_verifier_valid_hint() {
        // Domain separator commits to a hint
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("valid", KeccakF);
        domsep.hint("hint");

        let mut prover = domsep.to_prover_state::<H, 32>();

        let hint = b"abc123";
        prover.hint_bytes(hint).unwrap();

        let narg = prover.narg_string();

        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);
        let result = verifier.hint_bytes().unwrap();
        assert_eq!(result, hint);
    }

    #[test]
    fn test_hint_bytes_verifier_empty_hint() {
        // Commit to a hint instruction
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("empty", KeccakF);
        domsep.hint("hint");

        let mut prover = domsep.to_prover_state::<H, 32>();

        let hint = b"";
        prover.hint_bytes(hint).unwrap();

        let narg = prover.narg_string();

        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);
        let result = verifier.hint_bytes().unwrap();
        assert_eq!(result, b"");
    }

    #[test]
    fn test_hint_bytes_verifier_no_hint_op() {
        // No hint instruction in domain separator
        let domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("nohint", KeccakF);

        // Manually construct a hint buffer (length = 6, followed by bytes)
        let mut narg = vec![6, 0, 0, 0]; // length prefix for 6
        narg.extend_from_slice(b"abc123");

        let mut verifier = domsep.to_verifier_state::<H, 32>(&narg);

        assert!(verifier.hint_bytes().is_err());
    }

    #[test]
    fn test_hint_bytes_verifier_length_prefix_too_short() {
        // Valid hint domain separator
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("short", KeccakF);
        domsep.hint("hint");

        // Provide only 3 bytes, which is not enough for a u32 length
        let narg = &[1, 2, 3]; // less than 4 bytes

        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);

        let err = verifier.hint_bytes().unwrap_err();
        assert!(
            format!("{err}").contains("Insufficient transcript remaining for hint"),
            "Expected error for short prefix, got: {err}"
        );
    }

    #[test]
    fn test_hint_bytes_verifier_declared_hint_too_long() {
        // Valid hint domain separator
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("overflow", KeccakF);
        domsep.hint("hint");

        // Prefix says "5 bytes", but we only supply 2
        let narg = &[5, 0, 0, 0, b'a', b'b'];

        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);

        let err = verifier.hint_bytes().unwrap_err();
        assert!(
            format!("{err}").contains("Insufficient transcript remaining"),
            "Expected error for hint length > actual NARG bytes, got: {err}"
        );
    }

    #[test]
    fn test_hint_single_field_and_extension_round_trip() {
        // Create a domain separator tagged with "hint-single"
        // This will record all instructions for the Fiat-Shamir transcript
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("hint-single", KeccakF);

        // Register two hints in the domain separator: one for the base field, one for the extension field
        domsep.hint("base-field-hint");
        domsep.hint("extension-field-hint");

        // Construct the prover state from the domain separator
        let mut prover = domsep.to_prover_state::<H, 32>();

        // Define a single base field element to be used as a hint
        let elem = F::from_u64(42);

        // Define a corresponding extension field element (EF4)
        let elem_extension = EF4::from_u64(420);

        // Serialize and insert the base field hint into the prover transcript
        prover.hint(&elem).unwrap();

        // Serialize and insert the extension field hint into the prover transcript
        prover.hint(&elem_extension).unwrap();

        // Finalize the prover and extract the serialized transcript (narg string)
        let narg = prover.narg_string();

        // Create a verifier state using the same domain separator and prover-generated NARG string
        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);

        // Read and deserialize the base field hint on the verifier side
        let hint = verifier.hint::<F>().unwrap();

        // Ensure the deserialized base field element matches the original
        assert_eq!(hint, elem);

        // Read and deserialize the extension field hint on the verifier side
        let hint_extension = verifier.hint::<EF4>().unwrap();

        // Ensure the deserialized extension field element matches the original
        assert_eq!(hint_extension, elem_extension);
    }

    #[test]
    fn test_hint_vec_field_and_extension_round_trip() {
        // Create domain separator labeled "hint-vec"
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("hint-vec", KeccakF);

        // Register two hints: one for Vec<F> and one for Vec<EF4>
        domsep.hint("vec-base");
        domsep.hint("vec-extension");

        // Create prover from the domain separator
        let mut prover = domsep.to_prover_state::<H, 32>();

        // Base field vector to hint
        let elems = vec![
            F::from_u64(1),
            F::from_u64(2),
            F::from_u64(3),
            F::from_u64(42),
        ];

        // Extension field vector to hint
        let elems_extension = vec![
            EF4::from_u64(5),
            EF4::from_u64(6),
            EF4::from_u64(7),
            EF4::from_u64(88),
        ];

        // Write base field vector to prover hint
        prover.hint(&elems).unwrap();

        // Write extension field vector to prover hint
        prover.hint(&elems_extension).unwrap();

        // Finalize prover
        let narg = prover.narg_string();

        // Create verifier using the same domain separator
        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);

        // Read and check base field vector hint
        let hint = verifier.hint::<Vec<F>>().unwrap();
        assert_eq!(hint, elems);

        // Read and check extension field vector hint
        let hint_extension = verifier.hint::<Vec<EF4>>().unwrap();
        assert_eq!(hint_extension, elems_extension);
    }

    #[test]
    fn test_hint_vec_vec_field_and_extension_round_trip() {
        // Domain separator for nested vectors
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("hint-vec-vec", KeccakF);

        // Register hints for nested base and extension field vectors
        domsep.hint("nested-base");
        domsep.hint("nested-extension");

        // Build prover state
        let mut prover = domsep.to_prover_state::<H, 32>();

        // Nested base field vector
        let elems = vec![
            vec![F::from_u64(1), F::from_u64(2)],
            vec![F::from_u64(3)],
            vec![],
            vec![F::from_u64(42)],
        ];

        // Nested extension field vector
        let elems_extension = vec![
            vec![EF4::from_u64(10), EF4::from_u64(20)],
            vec![],
            vec![EF4::from_u64(30)],
        ];

        // Serialize both into prover
        prover.hint(&elems).unwrap();
        prover.hint(&elems_extension).unwrap();

        // Finalize and extract NARG string
        let narg = prover.narg_string();

        // Build verifier
        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);

        // Check nested base field round-trip
        let hint = verifier.hint::<Vec<Vec<F>>>().unwrap();
        assert_eq!(hint, elems);

        // Check nested extension field round-trip
        let hint_extension = verifier.hint::<Vec<Vec<EF4>>>().unwrap();
        assert_eq!(hint_extension, elems_extension);
    }

    #[test]
    fn test_hint_vec_vec_digest_round_trip() {
        const DIGEST_ELEMS: usize = 4;

        // Domain separator for digest hint
        let mut domsep: DomainSeparator<F, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("hint-digest", KeccakF);

        // Register hint labeled "digest"
        domsep.hint("digest");

        // Build prover
        let mut prover = domsep.to_prover_state::<H, 32>();

        // Nested digest array hint
        let elems: Vec<Vec<[u8; DIGEST_ELEMS]>> = vec![
            vec![[1, 2, 3, 4], [5, 6, 7, 8]],
            vec![[9, 10, 11, 12]],
            vec![],
        ];

        // Write to prover
        prover.hint(&elems).unwrap();

        // Finalize and serialize
        let narg = prover.narg_string();

        // Build verifier
        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);

        // Deserialize and verify match
        let hint = verifier.hint::<Vec<Vec<[u8; DIGEST_ELEMS]>>>().unwrap();
        assert_eq!(hint, elems);
    }

    #[test]
    fn test_public_scalars_round_trip_to_verifier_challenge_scalars() {
        // Number of public scalars to generate
        const NUM_SCALARS: usize = 5;

        let mut rng = SmallRng::seed_from_u64(1);

        // Create domain separator
        let mut domsep: DomainSeparator<EF4, F, DefaultPerm, u8, 200> =
            DomainSeparator::new("roundtrip", KeccakF);

        // Generate random EF4 scalars
        let random_scalars: Vec<EF4> = (0..NUM_SCALARS).map(|_| rng.random()).collect();

        // Record public scalars in the transcript
        domsep.add_scalars(NUM_SCALARS, "public-scalars");

        // Create prover and absorb public scalars
        let mut prover = domsep.to_prover_state::<H, 32>();
        prover.add_scalars(&random_scalars).unwrap();

        // Extract transcript (narg string)
        let narg = prover.narg_string();

        // Create verifier
        let mut verifier = domsep.to_verifier_state::<H, 32>(narg);

        // Read back absorbed scalars
        let mut verifier_scalars = [EF4::ZERO; NUM_SCALARS];
        verifier.fill_next_scalars(&mut verifier_scalars).unwrap();
        assert_eq!(
            verifier_scalars.to_vec(),
            random_scalars,
            "Verifier absorbed scalars should match"
        );
    }
}
