use ark_crypto_primitives::{CryptoError, Error, SignatureScheme, Vec};
use ark_crypto_primitives::{crh::pedersen::CRH};
use ark_crypto_primitives::{commitment::pedersen::CRH};
use ark_ec::{AffineCurve, ProjectiveCurve};
use ark_ff::{
    bytes::ToBytes,
    fields::{Field, PrimeField},
    to_bytes, One, ToConstraintField, UniformRand, Zero,
};
use ark_std::io::{Result as IoResult, Write};
use ark_std::rand::Rng;
use ark_std::{hash::Hash, marker::PhantomData};
use digest::Digest;

#[cfg(feature = "r1cs")]
pub mod constraints;

pub struct Schnorr<C: ProjectiveCurve, D: Digest> {
    _group: PhantomData<C>,
    _hash: PhantomData<D>,
}

#[derive(Derivative)]
#[derivative(Clone(bound = "C: ProjectiveCurve, H: Digest"), Debug)]
pub struct Parameters<C: ProjectiveCurve, H: Digest> {
    _hash: PhantomData<H>,
    pub generator: C::Affine,
    pub salt: Option<[u8; 32]>,
    pub ensure_verifier_randomness_unbiased: bool,
}

pub type PublicKey<C> = <C as ProjectiveCurve>::Affine;

#[derive(Clone, Default, Debug)]
pub struct SecretKey<C: ProjectiveCurve>(pub C::ScalarField);

impl<C: ProjectiveCurve> ToBytes for SecretKey<C> {
    #[inline]
    fn write<W: Write>(&self, writer: W) -> IoResult<()> {
        self.0.write(writer)
    }
}

#[derive(Clone, Default, Debug)]
pub struct Signature<C: ProjectiveCurve> {
    pub prover_response: C::ScalarField,
    pub verifier_challenge: [u8; 32],
}

impl<C: ProjectiveCurve + Hash, D: Digest + Send + Sync> SignatureScheme for Schnorr<C, D>
where
    C::ScalarField: PrimeField,
{
    type Parameters = Parameters<C, D>;
    type PublicKey = PublicKey<C>;
    type SecretKey = SecretKey<C>;
    type Signature = Signature<C>;

    fn setup<R: Rng>(rng: &mut R) -> Result<Self::Parameters, Error> {
        let setup_time = start_timer!(|| "SchnorrSig::Setup");

        let salt = None;
        // This is set to false for better consistency with constraints
        let ensure_verifier_randomness_unbiased = false;
        let generator = C::prime_subgroup_generator().into();

        end_timer!(setup_time);
        Ok(Parameters {
            _hash: PhantomData,
            generator,
            salt,
            ensure_verifier_randomness_unbiased,
        })
    }

    fn keygen<R: Rng>(
        parameters: &Self::Parameters,
        rng: &mut R,
    ) -> Result<(Self::PublicKey, Self::SecretKey), Error> {
        let keygen_time = start_timer!(|| "SchnorrSig::KeyGen");

        // Secret is a random scalar x
        // the pubkey is y = xG
        let secret_key = C::ScalarField::rand(rng);
        let public_key = parameters.generator.mul(secret_key).into();

        end_timer!(keygen_time);
        Ok((public_key, SecretKey(secret_key)))
    }

    fn sign<R: Rng>(
        parameters: &Self::Parameters,
        sk: &Self::SecretKey,
        message: &[u8],
        rng: &mut R,
    ) -> Result<Self::Signature, Error> {
        let sign_time = start_timer!(|| "SchnorrSig::Sign");
        // (k, e);
        let (random_scalar, verifier_challenge) = loop {
            // Sample a random scalar `k` from the prime scalar field.
            let random_scalar: C::ScalarField = C::ScalarField::rand(rng);
            // Commit to the random scalar via r := k · G.
            // This is the prover's first msg in the Sigma protocol.
            let prover_commitment = parameters.generator.mul(random_scalar).into_affine();

            // Hash everything to get verifier challenge.
            // e := H(salt || r || msg);
            let mut hash_input = Vec::new();
            if parameters.salt != None {
                hash_input.extend_from_slice(&parameters.salt.unwrap());
            }
            hash_input.extend_from_slice(&to_bytes![prover_commitment]?);
            hash_input.extend_from_slice(message);

            let hash_digest = D::digest(&hash_input);
            assert!(hash_digest.len() >= 32);
            let mut verifier_challenge = [0u8; 32];
            verifier_challenge.copy_from_slice(&hash_digest);

            // In this case, we don't have to worry about the verifier challenge being larger
            // than the modulus.
            if !parameters.ensure_verifier_randomness_unbiased {
                break (random_scalar, verifier_challenge);
            }

            // We use rejection sampling if this scalar is less than the modulus
            if C::ScalarField::from_random_bytes(&verifier_challenge) != None {
                break (random_scalar, verifier_challenge);
            }
        };

        let verifier_challenge_fe =
            C::ScalarField::from_random_bytes(&verifier_challenge).unwrap();

        // k - xe;
        let prover_response = random_scalar - (verifier_challenge_fe * sk.0);
        let signature = Signature {
            prover_response,
            verifier_challenge,
        };

        end_timer!(sign_time);
        Ok(signature)
    }

    fn verify(
        parameters: &Self::Parameters,
        pk: &Self::PublicKey,
        message: &[u8],
        signature: &Self::Signature,
    ) -> Result<bool, Error> {
        let verify_time = start_timer!(|| "SchnorrSig::Verify");

        let Signature {
            prover_response,
            verifier_challenge,
        } = signature;
        let verifier_challenge_wrapped = C::ScalarField::from_random_bytes(verifier_challenge);
        if verifier_challenge_wrapped == None {
            return Ok(false);
        }
        let verifier_challenge = verifier_challenge_wrapped.unwrap();
        // sG = kG - eY
        // kG = sG + eY
        // so we first solve for kG.
        let mut claimed_prover_commitment = parameters.generator.mul(*prover_response);
        let public_key_times_verifier_challenge = pk.mul(&verifier_challenge);
        claimed_prover_commitment += &public_key_times_verifier_challenge;
        let claimed_prover_commitment = claimed_prover_commitment.into_affine();

        // e = H(salt, kG, msg)
        let mut hash_input = Vec::new();
        if parameters.salt != None {
            hash_input.extend_from_slice(&parameters.salt.unwrap());
        }
        hash_input.extend_from_slice(&to_bytes![claimed_prover_commitment]?);
        hash_input.extend_from_slice(&message);

        // cast the hash output to get e
        let obtained_verifier_challenge = if let Some(obtained_verifier_challenge) =
            C::ScalarField::from_random_bytes(&D::digest(&hash_input))
        {
            obtained_verifier_challenge
        } else {
            return Ok(false);
        };
        end_timer!(verify_time);
        // The signature is valid iff the computed verifier challenge is the same as the one
        // provided in the signature
        Ok(verifier_challenge == obtained_verifier_challenge)
    }
}

pub fn bytes_to_bits(bytes: &[u8]) -> Vec<bool> {
    let mut bits = Vec::with_capacity(bytes.len() * 8);
    for byte in bytes {
        for i in 0..8 {
            let bit = (*byte >> (8 - i - 1)) & 1;
            bits.push(bit == 1);
        }
    }
    bits
}

impl<ConstraintF: Field, C: ProjectiveCurve + ToConstraintField<ConstraintF>, D: Digest>
    ToConstraintField<ConstraintF> for Parameters<C, D>
{
    #[inline]
    fn to_field_elements(&self) -> Option<Vec<ConstraintF>> {
        self.generator.into_projective().to_field_elements()
    }
}
