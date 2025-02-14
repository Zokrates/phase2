#![allow(clippy::redundant_clone)]

extern crate bellman_ce;
extern crate blake2;
extern crate blake2_rfc;
extern crate byteorder;
extern crate crossbeam;
extern crate num_cpus;
extern crate pairing_ce;
extern crate rand;

use blake2_rfc::blake2b::Blake2b;
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};

use std::{
    io::{self, Read, Write},
    sync::Arc,
};

use bellman_ce::{
    groth16::{Parameters, VerifyingKey},
    worker::Worker,
    Circuit, ConstraintSystem, Index, LinearCombination, SynthesisError, Variable,
};
use pairing_ce::{
    ff::{Field, PrimeField},
    CurveAffine, CurveProjective, EncodedPoint, Engine, Wnaf,
};

use rand::{ChaChaRng, Rand, Rng, SeedableRng};

/// This is our assembly structure that we'll use to synthesize the
/// circuit into a QAP.
struct KeypairAssembly<E: Engine> {
    num_inputs: usize,
    num_aux: usize,
    num_constraints: usize,
    at_inputs: Vec<Vec<(E::Fr, usize)>>,
    bt_inputs: Vec<Vec<(E::Fr, usize)>>,
    ct_inputs: Vec<Vec<(E::Fr, usize)>>,
    at_aux: Vec<Vec<(E::Fr, usize)>>,
    bt_aux: Vec<Vec<(E::Fr, usize)>>,
    ct_aux: Vec<Vec<(E::Fr, usize)>>,
}

impl<E: Engine> ConstraintSystem<E> for KeypairAssembly<E> {
    type Root = Self;

    fn alloc<F, A, AR>(&mut self, _: A, _: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError>,
        A: FnOnce() -> AR,
        AR: Into<String>,
    {
        // There is no assignment, so we don't even invoke the
        // function for obtaining one.

        let index = self.num_aux;
        self.num_aux += 1;

        self.at_aux.push(vec![]);
        self.bt_aux.push(vec![]);
        self.ct_aux.push(vec![]);

        Ok(Variable::new_unchecked(Index::Aux(index)))
    }

    fn alloc_input<F, A, AR>(&mut self, _: A, _: F) -> Result<Variable, SynthesisError>
    where
        F: FnOnce() -> Result<E::Fr, SynthesisError>,
        A: FnOnce() -> AR,
        AR: Into<String>,
    {
        // There is no assignment, so we don't even invoke the
        // function for obtaining one.

        let index = self.num_inputs;
        self.num_inputs += 1;

        self.at_inputs.push(vec![]);
        self.bt_inputs.push(vec![]);
        self.ct_inputs.push(vec![]);

        Ok(Variable::new_unchecked(Index::Input(index)))
    }

    fn enforce<A, AR, LA, LB, LC>(&mut self, _: A, a: LA, b: LB, c: LC)
    where
        A: FnOnce() -> AR,
        AR: Into<String>,
        LA: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
        LB: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
        LC: FnOnce(LinearCombination<E>) -> LinearCombination<E>,
    {
        fn eval<E: Engine>(
            l: LinearCombination<E>,
            inputs: &mut [Vec<(E::Fr, usize)>],
            aux: &mut [Vec<(E::Fr, usize)>],
            this_constraint: usize,
        ) {
            for &(var, coeff) in l.as_ref() {
                match var.get_unchecked() {
                    Index::Input(id) => inputs[id].push((coeff, this_constraint)),
                    Index::Aux(id) => aux[id].push((coeff, this_constraint)),
                }
            }
        }

        eval::<E>(
            a(LinearCombination::zero()),
            &mut self.at_inputs,
            &mut self.at_aux,
            self.num_constraints,
        );
        eval::<E>(
            b(LinearCombination::zero()),
            &mut self.bt_inputs,
            &mut self.bt_aux,
            self.num_constraints,
        );
        eval::<E>(
            c(LinearCombination::zero()),
            &mut self.ct_inputs,
            &mut self.ct_aux,
            self.num_constraints,
        );

        self.num_constraints += 1;
    }

    fn push_namespace<NR, N>(&mut self, _: N)
    where
        NR: Into<String>,
        N: FnOnce() -> NR,
    {
        // Do nothing; we don't care about namespaces in this context.
    }

    fn pop_namespace(&mut self) {
        // Do nothing; we don't care about namespaces in this context.
    }

    fn get_root(&mut self) -> &mut Self::Root {
        self
    }
}

/// MPC parameters are just like bellman `Parameters` except, when serialized,
/// they contain a transcript of contributions at the end, which can be verified.
#[derive(Clone)]
pub struct MPCParameters<E: Engine> {
    params: Parameters<E>,
    cs_hash: [u8; 64],
    contributions: Vec<PublicKey<E>>,
}

impl<E: Engine> PartialEq for MPCParameters<E> {
    fn eq(&self, other: &MPCParameters<E>) -> bool {
        self.params == other.params
            && self.cs_hash[..] == other.cs_hash[..]
            && self.contributions == other.contributions
    }
}

impl<E: Engine> MPCParameters<E> {
    /// Create new Groth16 parameters (compatible with bellman) for a
    /// given circuit. The resulting parameters are unsafe to use
    /// until there are contributions (see `contribute()`).
    pub fn new<C, R: Read>(
        circuit: C,
        phase1_radix: &mut R,
    ) -> Result<MPCParameters<E>, SynthesisError>
    where
        C: Circuit<E>,
    {
        let mut assembly = KeypairAssembly {
            num_inputs: 0,
            num_aux: 0,
            num_constraints: 0,
            at_inputs: vec![],
            bt_inputs: vec![],
            ct_inputs: vec![],
            at_aux: vec![],
            bt_aux: vec![],
            ct_aux: vec![],
        };

        // Allocate the "one" input variable
        assembly.alloc_input(|| "", || Ok(E::Fr::one()))?;

        // Synthesize the circuit.
        circuit.synthesize(&mut assembly)?;

        // Input constraints to ensure full density of IC query
        // x * 0 = 0
        for i in 0..assembly.num_inputs {
            assembly.enforce(
                || "",
                |lc| lc + Variable::new_unchecked(Index::Input(i)),
                |lc| lc,
                |lc| lc,
            );
        }

        // Compute the size of our evaluation domain
        let mut m = 1;
        let mut exp = 0;
        while m < assembly.num_constraints {
            m *= 2;
            exp += 1;

            if exp > E::Fr::S {
                return Err(SynthesisError::PolynomialDegreeTooLarge);
            }
        }

        let read_g1 = |reader: &mut R| -> io::Result<E::G1Affine> {
            let mut repr = <<E as Engine>::G1Affine as CurveAffine>::Uncompressed::empty();
            reader.read_exact(repr.as_mut())?;

            repr.into_affine_unchecked()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                .and_then(|e| {
                    if e.is_zero() {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "point at infinity",
                        ))
                    } else {
                        Ok(e)
                    }
                })
        };

        let read_g2 = |reader: &mut R| -> io::Result<E::G2Affine> {
            let mut repr = <<E as Engine>::G2Affine as CurveAffine>::Uncompressed::empty();
            reader.read_exact(repr.as_mut())?;

            repr.into_affine_unchecked()
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
                .and_then(|e| {
                    if e.is_zero() {
                        Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "point at infinity",
                        ))
                    } else {
                        Ok(e)
                    }
                })
        };

        let alpha = read_g1(phase1_radix)?;
        let beta_g1 = read_g1(phase1_radix)?;
        let beta_g2 = read_g2(phase1_radix)?;

        let mut coeffs_g1 = Vec::with_capacity(m);
        for _ in 0..m {
            coeffs_g1.push(read_g1(phase1_radix)?);
        }

        let mut coeffs_g2 = Vec::with_capacity(m);
        for _ in 0..m {
            coeffs_g2.push(read_g2(phase1_radix)?);
        }

        let mut alpha_coeffs_g1 = Vec::with_capacity(m);
        for _ in 0..m {
            alpha_coeffs_g1.push(read_g1(phase1_radix)?);
        }

        let mut beta_coeffs_g1 = Vec::with_capacity(m);
        for _ in 0..m {
            beta_coeffs_g1.push(read_g1(phase1_radix)?);
        }

        // These are `Arc` so that later it'll be easier
        // to use multiexp during QAP evaluation (which
        // requires a futures-based API)
        let coeffs_g1 = Arc::new(coeffs_g1);
        let coeffs_g2 = Arc::new(coeffs_g2);
        let alpha_coeffs_g1 = Arc::new(alpha_coeffs_g1);
        let beta_coeffs_g1 = Arc::new(beta_coeffs_g1);

        let mut h = Vec::with_capacity(m - 1);
        for _ in 0..(m - 1) {
            h.push(read_g1(phase1_radix)?);
        }

        let mut ic = vec![E::G1::zero(); assembly.num_inputs];
        let mut l = vec![E::G1::zero(); assembly.num_aux];
        let mut a_g1 = vec![E::G1::zero(); assembly.num_inputs + assembly.num_aux];
        let mut b_g1 = vec![E::G1::zero(); assembly.num_inputs + assembly.num_aux];
        let mut b_g2 = vec![E::G2::zero(); assembly.num_inputs + assembly.num_aux];

        #[allow(clippy::too_many_arguments)]
        fn eval<E: Engine>(
            // Lagrange coefficients for tau
            coeffs_g1: Arc<Vec<E::G1Affine>>,
            coeffs_g2: Arc<Vec<E::G2Affine>>,
            alpha_coeffs_g1: Arc<Vec<E::G1Affine>>,
            beta_coeffs_g1: Arc<Vec<E::G1Affine>>,

            // QAP polynomials
            at: &[Vec<(E::Fr, usize)>],
            bt: &[Vec<(E::Fr, usize)>],
            ct: &[Vec<(E::Fr, usize)>],

            // Resulting evaluated QAP polynomials
            a_g1: &mut [E::G1],
            b_g1: &mut [E::G1],
            b_g2: &mut [E::G2],
            ext: &mut [E::G1],

            // Worker
            worker: &Worker,
        ) {
            // Sanity check
            assert_eq!(a_g1.len(), at.len());
            assert_eq!(a_g1.len(), bt.len());
            assert_eq!(a_g1.len(), ct.len());
            assert_eq!(a_g1.len(), b_g1.len());
            assert_eq!(a_g1.len(), b_g2.len());
            assert_eq!(a_g1.len(), ext.len());

            // Evaluate polynomials in multiple threads
            worker.scope(a_g1.len(), |scope, chunk| {
                for ((((((a_g1, b_g1), b_g2), ext), at), bt), ct) in a_g1
                    .chunks_mut(chunk)
                    .zip(b_g1.chunks_mut(chunk))
                    .zip(b_g2.chunks_mut(chunk))
                    .zip(ext.chunks_mut(chunk))
                    .zip(at.chunks(chunk))
                    .zip(bt.chunks(chunk))
                    .zip(ct.chunks(chunk))
                {
                    let coeffs_g1 = coeffs_g1.clone();
                    let coeffs_g2 = coeffs_g2.clone();
                    let alpha_coeffs_g1 = alpha_coeffs_g1.clone();
                    let beta_coeffs_g1 = beta_coeffs_g1.clone();

                    scope.spawn(move |_| {
                        for ((((((a_g1, b_g1), b_g2), ext), at), bt), ct) in a_g1
                            .iter_mut()
                            .zip(b_g1.iter_mut())
                            .zip(b_g2.iter_mut())
                            .zip(ext.iter_mut())
                            .zip(at.iter())
                            .zip(bt.iter())
                            .zip(ct.iter())
                        {
                            for &(coeff, lag) in at {
                                a_g1.add_assign(&coeffs_g1[lag].mul(coeff));
                                ext.add_assign(&beta_coeffs_g1[lag].mul(coeff));
                            }

                            for &(coeff, lag) in bt {
                                b_g1.add_assign(&coeffs_g1[lag].mul(coeff));
                                b_g2.add_assign(&coeffs_g2[lag].mul(coeff));
                                ext.add_assign(&alpha_coeffs_g1[lag].mul(coeff));
                            }

                            for &(coeff, lag) in ct {
                                ext.add_assign(&coeffs_g1[lag].mul(coeff));
                            }
                        }

                        // Batch normalize
                        E::G1::batch_normalization(a_g1);
                        E::G1::batch_normalization(b_g1);
                        E::G2::batch_normalization(b_g2);
                        E::G1::batch_normalization(ext);
                    });
                }
            });
        }

        let worker = Worker::new();

        // Evaluate for inputs.
        eval::<E>(
            coeffs_g1.clone(),
            coeffs_g2.clone(),
            alpha_coeffs_g1.clone(),
            beta_coeffs_g1.clone(),
            &assembly.at_inputs,
            &assembly.bt_inputs,
            &assembly.ct_inputs,
            &mut a_g1[0..assembly.num_inputs],
            &mut b_g1[0..assembly.num_inputs],
            &mut b_g2[0..assembly.num_inputs],
            &mut ic,
            &worker,
        );

        // Evaluate for auxillary variables.
        eval::<E>(
            coeffs_g1.clone(),
            coeffs_g2.clone(),
            alpha_coeffs_g1.clone(),
            beta_coeffs_g1.clone(),
            &assembly.at_aux,
            &assembly.bt_aux,
            &assembly.ct_aux,
            &mut a_g1[assembly.num_inputs..],
            &mut b_g1[assembly.num_inputs..],
            &mut b_g2[assembly.num_inputs..],
            &mut l,
            &worker,
        );

        // Don't allow any elements be unconstrained, so that
        // the L query is always fully dense.
        for e in l.iter() {
            if e.is_zero() {
                return Err(SynthesisError::UnconstrainedVariable);
            }
        }

        let vk = VerifyingKey {
            alpha_g1: alpha,
            beta_g1,
            beta_g2,
            gamma_g2: E::G2Affine::one(),
            delta_g1: E::G1Affine::one(),
            delta_g2: E::G2Affine::one(),
            ic: ic.into_iter().map(|e| e.into_affine()).collect(),
        };

        let params = Parameters {
            vk,
            h: Arc::new(h),
            l: Arc::new(l.into_iter().map(|e| e.into_affine()).collect()),

            // Filter points at infinity away from A/B queries
            a: Arc::new(
                a_g1.into_iter()
                    .filter(|e| !e.is_zero())
                    .map(|e| e.into_affine())
                    .collect(),
            ),
            b_g1: Arc::new(
                b_g1.into_iter()
                    .filter(|e| !e.is_zero())
                    .map(|e| e.into_affine())
                    .collect(),
            ),
            b_g2: Arc::new(
                b_g2.into_iter()
                    .filter(|e| !e.is_zero())
                    .map(|e| e.into_affine())
                    .collect(),
            ),
        };

        let h = {
            let sink = io::sink();
            let mut sink = HashWriter::new(sink);

            params.write(&mut sink).unwrap();
            sink.into_hash()
        };

        let mut cs_hash = [0; 64];
        cs_hash.copy_from_slice(h.as_ref());

        Ok(MPCParameters {
            params,
            cs_hash,
            contributions: vec![],
        })
    }

    /// Get the underlying Groth16 `Parameters`
    pub fn get_params(&self) -> &Parameters<E> {
        &self.params
    }

    /// Contributes some randomness to the parameters. Only one
    /// contributor needs to be honest for the parameters to be
    /// secure.
    ///
    /// This function returns a "hash" that is bound to the
    /// contribution. Contributors can use this hash to make
    /// sure their contribution is in the final parameters, by
    /// checking to see if it appears in the output of
    /// `MPCParameters::verify`.
    pub fn contribute<R: Rng>(&mut self, rng: &mut R) -> [u8; 64] {
        // Generate a keypair
        let (pubkey, privkey) = keypair(rng, self);

        #[cfg(not(target_arch = "wasm32"))]
        fn batch_exp<C: CurveAffine>(bases: &mut [C], coeff: C::Scalar) {
            let coeff = coeff.into_repr();

            let mut projective = vec![C::Projective::zero(); bases.len()];
            let cpus = num_cpus::get();
            let chunk_size = if bases.len() < cpus {
                1
            } else {
                bases.len() / cpus
            };

            // Perform wNAF over multiple cores, placing results into `projective`.
            crossbeam::scope(|scope| {
                for (bases, projective) in bases
                    .chunks_mut(chunk_size)
                    .zip(projective.chunks_mut(chunk_size))
                {
                    scope.spawn(move |_| {
                        let mut wnaf = Wnaf::new();

                        for (base, projective) in bases.iter_mut().zip(projective.iter_mut()) {
                            *projective = wnaf.base(base.into_projective(), 1).scalar(coeff);
                        }
                    });
                }
            })
            .unwrap();

            // Perform batch normalization
            crossbeam::scope(|scope| {
                for projective in projective.chunks_mut(chunk_size) {
                    scope.spawn(move |_| {
                        C::Projective::batch_normalization(projective);
                    });
                }
            })
            .unwrap();

            // Turn it all back into affine points
            for (projective, affine) in projective.iter().zip(bases.iter_mut()) {
                *affine = projective.into_affine();
            }
        }

        #[cfg(target_arch = "wasm32")]
        fn batch_exp<C: CurveAffine>(bases: &mut [C], coeff: C::Scalar) {
            let coeff = coeff.into_repr();

            let mut projective = vec![C::Projective::zero(); bases.len()];

            // Perform wNAF, placing results into `projective`.
            let mut wnaf = Wnaf::new();
            for (base, projective) in bases.iter_mut().zip(projective.iter_mut()) {
                *projective = wnaf.base(base.into_projective(), 1).scalar(coeff);
            }

            // Perform batch normalization
            C::Projective::batch_normalization(&mut projective);

            // Turn it all back into affine points
            for (projective, affine) in projective.iter().zip(bases.iter_mut()) {
                *affine = projective.into_affine();
            }
        }

        let delta_inv = privkey.delta.inverse().expect("nonzero");
        let mut l = (&self.params.l[..]).to_vec();
        let mut h = (&self.params.h[..]).to_vec();

        batch_exp(&mut l, delta_inv);
        batch_exp(&mut h, delta_inv);

        self.params.l = Arc::new(l);
        self.params.h = Arc::new(h);
        self.params.vk.delta_g1 = self.params.vk.delta_g1.mul(privkey.delta).into_affine();
        self.params.vk.delta_g2 = self.params.vk.delta_g2.mul(privkey.delta).into_affine();

        self.contributions.push(pubkey.clone());

        // Calculate the hash of the public key and return it
        {
            let sink = io::sink();
            let mut sink = HashWriter::new(sink);
            pubkey.write(&mut sink).unwrap();
            let h = sink.into_hash();
            let mut response = [0u8; 64];
            response.copy_from_slice(h.as_ref());
            response
        }
    }

    /// Verify the correctness of the parameters, given a circuit
    /// instance. This will return all of the hashes that
    /// contributors obtained when they ran
    /// `MPCParameters::contribute`, for ensuring that contributions
    /// exist in the final parameters.
    #[allow(clippy::result_unit_err)]
    pub fn verify<C: Circuit<E>, R: Read>(
        &self,
        circuit: C,
        phase1_radix: &mut R,
    ) -> Result<Vec<[u8; 64]>, ()> {
        let initial_params = MPCParameters::new(circuit, phase1_radix).map_err(|_| ())?;

        // H/L will change, but should have same length
        if initial_params.params.h.len() != self.params.h.len() {
            return Err(());
        }
        if initial_params.params.l.len() != self.params.l.len() {
            return Err(());
        }

        // A/B_G1/B_G2 doesn't change at all
        if initial_params.params.a != self.params.a {
            return Err(());
        }
        if initial_params.params.b_g1 != self.params.b_g1 {
            return Err(());
        }
        if initial_params.params.b_g2 != self.params.b_g2 {
            return Err(());
        }

        // alpha/beta/gamma don't change
        if initial_params.params.vk.alpha_g1 != self.params.vk.alpha_g1 {
            return Err(());
        }
        if initial_params.params.vk.beta_g1 != self.params.vk.beta_g1 {
            return Err(());
        }
        if initial_params.params.vk.beta_g2 != self.params.vk.beta_g2 {
            return Err(());
        }
        if initial_params.params.vk.gamma_g2 != self.params.vk.gamma_g2 {
            return Err(());
        }

        // IC shouldn't change, as gamma doesn't change
        if initial_params.params.vk.ic != self.params.vk.ic {
            return Err(());
        }

        // cs_hash should be the same
        if initial_params.cs_hash[..] != self.cs_hash[..] {
            return Err(());
        }

        let sink = io::sink();
        let mut sink = HashWriter::new(sink);
        sink.write_all(&initial_params.cs_hash[..]).unwrap();

        let mut current_delta = E::G1Affine::one();
        let mut result = vec![];

        for pubkey in &self.contributions {
            let mut our_sink = sink.clone();
            our_sink
                .write_all(pubkey.s.into_uncompressed().as_ref())
                .unwrap();
            our_sink
                .write_all(pubkey.s_delta.into_uncompressed().as_ref())
                .unwrap();

            pubkey.write(&mut sink).unwrap();

            let h = our_sink.into_hash();

            // The transcript must be consistent
            if &pubkey.transcript[..] != h.as_ref() {
                return Err(());
            }

            let r = hash_to_g2::<E>(h.as_ref()).into_affine();

            // Check the signature of knowledge
            if !same_ratio((r, pubkey.r_delta), (pubkey.s, pubkey.s_delta)) {
                return Err(());
            }

            // Check the change from the old delta is consistent
            if !same_ratio((current_delta, pubkey.delta_after), (r, pubkey.r_delta)) {
                return Err(());
            }

            current_delta = pubkey.delta_after;

            {
                let sink = io::sink();
                let mut sink = HashWriter::new(sink);
                pubkey.write(&mut sink).unwrap();
                let h = sink.into_hash();
                let mut response = [0u8; 64];
                response.copy_from_slice(h.as_ref());
                result.push(response);
            }
        }

        // Current parameters should have consistent delta in G1
        if current_delta != self.params.vk.delta_g1 {
            return Err(());
        }

        // Current parameters should have consistent delta in G2
        if !same_ratio(
            (E::G1Affine::one(), current_delta),
            (E::G2Affine::one(), self.params.vk.delta_g2),
        ) {
            return Err(());
        }

        // H and L queries should be updated with delta^-1
        if !same_ratio(
            merge_pairs(&initial_params.params.h, &self.params.h),
            (self.params.vk.delta_g2, E::G2Affine::one()), // reversed for inverse
        ) {
            return Err(());
        }

        if !same_ratio(
            merge_pairs(&initial_params.params.l, &self.params.l),
            (self.params.vk.delta_g2, E::G2Affine::one()), // reversed for inverse
        ) {
            return Err(());
        }

        Ok(result)
    }

    /// Serialize these parameters. The serialized parameters
    /// can be read by bellman as Groth16 `Parameters`.
    pub fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        self.params.write(&mut writer)?;
        writer.write_all(&self.cs_hash)?;

        writer.write_u32::<BigEndian>(self.contributions.len() as u32)?;
        for pubkey in &self.contributions {
            pubkey.write(&mut writer)?;
        }

        Ok(())
    }

    /// Deserialize these parameters. If `checked` is false,
    /// we won't perform curve validity and group order
    /// checks.
    pub fn read<R: Read>(mut reader: R, checked: bool) -> io::Result<MPCParameters<E>> {
        let params = Parameters::read(&mut reader, checked)?;

        let mut cs_hash = [0u8; 64];
        reader.read_exact(&mut cs_hash)?;

        let contributions_len = reader.read_u32::<BigEndian>()? as usize;

        let mut contributions = vec![];
        for _ in 0..contributions_len {
            contributions.push(PublicKey::read(&mut reader)?);
        }

        Ok(MPCParameters {
            params,
            cs_hash,
            contributions,
        })
    }
}

/// This allows others to verify that you contributed. The hash produced
/// by `MPCParameters::contribute` is just a BLAKE2b hash of this object.
#[derive(Clone)]
pub struct PublicKey<E: Engine> {
    /// This is the delta (in G1) after the transformation, kept so that we
    /// can check correctness of the public keys without having the entire
    /// interstitial parameters for each contribution.
    pub delta_after: E::G1Affine,

    /// Random element chosen by the contributor.
    pub s: E::G1Affine,

    /// That element, taken to the contributor's secret delta.
    pub s_delta: E::G1Affine,

    /// r is H(last_pubkey | s | s_delta), r_delta proves knowledge of delta
    pub r_delta: E::G2Affine,

    /// Hash of the transcript (used for mapping to r)
    pub transcript: [u8; 64],
}

impl<E: Engine> PublicKey<E> {
    fn write<W: Write>(&self, mut writer: W) -> io::Result<()> {
        writer.write_all(self.delta_after.into_uncompressed().as_ref())?;
        writer.write_all(self.s.into_uncompressed().as_ref())?;
        writer.write_all(self.s_delta.into_uncompressed().as_ref())?;
        writer.write_all(self.r_delta.into_uncompressed().as_ref())?;
        writer.write_all(&self.transcript)?;

        Ok(())
    }

    pub fn read<R: Read>(mut reader: R) -> io::Result<PublicKey<E>> {
        let mut g1_repr = <<E as Engine>::G1Affine as CurveAffine>::Uncompressed::empty();
        let mut g2_repr = <<E as Engine>::G2Affine as CurveAffine>::Uncompressed::empty();

        reader.read_exact(g1_repr.as_mut())?;
        let delta_after = g1_repr
            .into_affine()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if delta_after.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "point at infinity",
            ));
        }

        reader.read_exact(g1_repr.as_mut())?;
        let s = g1_repr
            .into_affine()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if s.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "point at infinity",
            ));
        }

        reader.read_exact(g1_repr.as_mut())?;
        let s_delta = g1_repr
            .into_affine()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if s_delta.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "point at infinity",
            ));
        }

        reader.read_exact(g2_repr.as_mut())?;
        let r_delta = g2_repr
            .into_affine()
            .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;

        if r_delta.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "point at infinity",
            ));
        }

        let mut transcript = [0u8; 64];
        reader.read_exact(&mut transcript)?;

        Ok(PublicKey {
            delta_after,
            s,
            s_delta,
            r_delta,
            transcript,
        })
    }
}

impl<E: Engine> PartialEq for PublicKey<E> {
    fn eq(&self, other: &PublicKey<E>) -> bool {
        self.delta_after == other.delta_after
            && self.s == other.s
            && self.s_delta == other.s_delta
            && self.r_delta == other.r_delta
            && self.transcript[..] == other.transcript[..]
    }
}

/// Verify a contribution, given the old parameters and
/// the new parameters. Returns the hash of the contribution.
#[allow(clippy::result_unit_err)]
pub fn verify_contribution<E: Engine>(
    before: &MPCParameters<E>,
    after: &MPCParameters<E>,
) -> Result<[u8; 64], ()> {
    // Transformation involves a single new object
    if after.contributions.len() != (before.contributions.len() + 1) {
        return Err(());
    }

    // None of the previous transformations should change
    if before.contributions[..] != after.contributions[0..before.contributions.len()] {
        return Err(());
    }

    // H/L will change, but should have same length
    if before.params.h.len() != after.params.h.len() {
        return Err(());
    }
    if before.params.l.len() != after.params.l.len() {
        return Err(());
    }

    // A/B_G1/B_G2 doesn't change at all
    if before.params.a != after.params.a {
        return Err(());
    }
    if before.params.b_g1 != after.params.b_g1 {
        return Err(());
    }
    if before.params.b_g2 != after.params.b_g2 {
        return Err(());
    }

    // alpha/beta/gamma don't change
    if before.params.vk.alpha_g1 != after.params.vk.alpha_g1 {
        return Err(());
    }
    if before.params.vk.beta_g1 != after.params.vk.beta_g1 {
        return Err(());
    }
    if before.params.vk.beta_g2 != after.params.vk.beta_g2 {
        return Err(());
    }
    if before.params.vk.gamma_g2 != after.params.vk.gamma_g2 {
        return Err(());
    }

    // IC shouldn't change, as gamma doesn't change
    if before.params.vk.ic != after.params.vk.ic {
        return Err(());
    }

    // cs_hash should be the same
    if before.cs_hash[..] != after.cs_hash[..] {
        return Err(());
    }

    let sink = io::sink();
    let mut sink = HashWriter::new(sink);
    sink.write_all(&before.cs_hash[..]).unwrap();

    for pubkey in &before.contributions {
        pubkey.write(&mut sink).unwrap();
    }

    let pubkey = after.contributions.last().unwrap();
    sink.write_all(pubkey.s.into_uncompressed().as_ref())
        .unwrap();
    sink.write_all(pubkey.s_delta.into_uncompressed().as_ref())
        .unwrap();

    let h = sink.into_hash();

    // The transcript must be consistent
    if &pubkey.transcript[..] != h.as_ref() {
        return Err(());
    }

    let r = hash_to_g2::<E>(h.as_ref()).into_affine();

    // Check the signature of knowledge
    if !same_ratio((r, pubkey.r_delta), (pubkey.s, pubkey.s_delta)) {
        return Err(());
    }

    // Check the change from the old delta is consistent
    if !same_ratio(
        (before.params.vk.delta_g1, pubkey.delta_after),
        (r, pubkey.r_delta),
    ) {
        return Err(());
    }

    // Current parameters should have consistent delta in G1
    if pubkey.delta_after != after.params.vk.delta_g1 {
        return Err(());
    }

    // Current parameters should have consistent delta in G2
    if !same_ratio(
        (E::G1Affine::one(), pubkey.delta_after),
        (E::G2Affine::one(), after.params.vk.delta_g2),
    ) {
        return Err(());
    }

    // H and L queries should be updated with delta^-1
    if !same_ratio(
        merge_pairs(&before.params.h, &after.params.h),
        (after.params.vk.delta_g2, before.params.vk.delta_g2), // reversed for inverse
    ) {
        return Err(());
    }

    if !same_ratio(
        merge_pairs(&before.params.l, &after.params.l),
        (after.params.vk.delta_g2, before.params.vk.delta_g2), // reversed for inverse
    ) {
        return Err(());
    }

    let sink = io::sink();
    let mut sink = HashWriter::new(sink);
    pubkey.write(&mut sink).unwrap();
    let h = sink.into_hash();
    let mut response = [0u8; 64];
    response.copy_from_slice(h.as_ref());

    Ok(response)
}

/// Checks if pairs have the same ratio.
fn same_ratio<G1: CurveAffine>(g1: (G1, G1), g2: (G1::Pair, G1::Pair)) -> bool {
    if g1.0.is_zero() || g1.1.is_zero() || g2.0.is_zero() || g2.1.is_zero() {
        return false;
    }
    g1.0.pairing_with(&g2.1) == g1.1.pairing_with(&g2.0)
}

/// Computes a random linear combination over v1/v2.
///
/// Checking that many pairs of elements are exponentiated by
/// the same `x` can be achieved (with high probability) with
/// the following technique:
///
/// Given v1 = [a, b, c] and v2 = [as, bs, cs], compute
/// (a*r1 + b*r2 + c*r3, (as)*r1 + (bs)*r2 + (cs)*r3) for some
/// random r1, r2, r3. Given (g, g^s)...
///
/// e(g, (as)*r1 + (bs)*r2 + (cs)*r3) = e(g^s, a*r1 + b*r2 + c*r3)
///
/// ... with high probability.
#[cfg(not(target_arch = "wasm32"))]
fn merge_pairs<G: CurveAffine>(v1: &[G], v2: &[G]) -> (G, G) {
    use rand::thread_rng;
    use std::sync::Mutex;

    assert_eq!(v1.len(), v2.len());

    let chunk = (v1.len() / num_cpus::get()) + 1;

    let s = Arc::new(Mutex::new(G::Projective::zero()));
    let sx = Arc::new(Mutex::new(G::Projective::zero()));

    crossbeam::scope(|scope| {
        for (v1, v2) in v1.chunks(chunk).zip(v2.chunks(chunk)) {
            let s = s.clone();
            let sx = sx.clone();

            scope.spawn(move |_| {
                // We do not need to be overly cautious of the RNG
                // used for this check.
                let rng = &mut thread_rng();

                let mut wnaf = Wnaf::new();
                let mut local_s = G::Projective::zero();
                let mut local_sx = G::Projective::zero();

                for (v1, v2) in v1.iter().zip(v2.iter()) {
                    let rho = G::Scalar::rand(rng);
                    let mut wnaf = wnaf.scalar(rho.into_repr());
                    let v1 = wnaf.base(v1.into_projective());
                    let v2 = wnaf.base(v2.into_projective());

                    local_s.add_assign(&v1);
                    local_sx.add_assign(&v2);
                }

                s.lock().unwrap().add_assign(&local_s);
                sx.lock().unwrap().add_assign(&local_sx);
            });
        }
    })
    .unwrap();

    let s = s.lock().unwrap().into_affine();
    let sx = sx.lock().unwrap().into_affine();

    (s, sx)
}

#[cfg(target_arch = "wasm32")]
fn merge_pairs<G: CurveAffine>(v1: &[G], v2: &[G]) -> (G, G) {
    use std::mem::transmute;
    assert_eq!(v1.len(), v2.len());

    let mut wnaf = Wnaf::new();
    let mut s = G::Projective::zero();
    let mut sx = G::Projective::zero();

    for (v1, v2) in v1.iter().zip(v2.iter()) {
        // We do not need to be overly cautious of the RNG
        // used for this check.
        let mut seed = [0u8; 32];
        getrandom::getrandom(&mut seed).expect("could not get random seed");

        let seed: [u32; 8] = unsafe { transmute(seed) };
        let mut rng = ChaChaRng::from_seed(&seed);

        let rho = G::Scalar::rand(&mut rng);
        let mut wnaf = wnaf.scalar(rho.into_repr());
        let v1 = wnaf.base(v1.into_projective());
        let v2 = wnaf.base(v2.into_projective());

        s.add_assign(&v1);
        sx.add_assign(&v2);
    }

    (s.into_affine(), sx.into_affine())
}

/// This needs to be destroyed by at least one participant
/// for the final parameters to be secure.
pub struct PrivateKey<E: Engine> {
    pub delta: E::Fr,
}

/// Compute a keypair, given the current parameters. Keypairs
/// cannot be reused for multiple contributions or contributions
/// in different parameters.
fn keypair<E: Engine, R: Rng>(
    rng: &mut R,
    current: &MPCParameters<E>,
) -> (PublicKey<E>, PrivateKey<E>) {
    // Sample random delta
    let delta: E::Fr = rng.gen();

    // Compute delta s-pair in G1
    let s = E::G1::rand(rng).into_affine();
    let s_delta = s.mul(delta).into_affine();

    // H(cs_hash | <previous pubkeys> | s | s_delta)
    let h = {
        let sink = io::sink();
        let mut sink = HashWriter::new(sink);

        sink.write_all(&current.cs_hash[..]).unwrap();
        for pubkey in &current.contributions {
            pubkey.write(&mut sink).unwrap();
        }
        sink.write_all(s.into_uncompressed().as_ref()).unwrap();
        sink.write_all(s_delta.into_uncompressed().as_ref())
            .unwrap();

        sink.into_hash()
    };

    // This avoids making a weird assumption about the hash into the
    // group.
    let mut transcript = [0; 64];
    transcript.copy_from_slice(h.as_ref());

    // Compute delta s-pair in G2
    let r = hash_to_g2::<E>(h.as_ref()).into_affine();
    let r_delta = r.mul(delta).into_affine();

    (
        PublicKey {
            delta_after: current.params.vk.delta_g1.mul(delta).into_affine(),
            s,
            s_delta,
            r_delta,
            transcript,
        },
        PrivateKey { delta },
    )
}

/// Hashes to G2 using the first 32 bytes of `digest`. Panics if `digest` is less
/// than 32 bytes.
fn hash_to_g2<E: Engine>(mut digest: &[u8]) -> E::G2 {
    assert!(digest.len() >= 32);

    let mut seed = Vec::with_capacity(8);

    for _ in 0..8 {
        seed.push(
            digest
                .read_u32::<BigEndian>()
                .expect("assertion above guarantees this to work"),
        );
    }

    ChaChaRng::from_seed(&seed).gen()
}

/// Abstraction over a writer which hashes the data being written.
struct HashWriter<W: Write> {
    writer: W,
    hasher: Blake2b,
}

impl Clone for HashWriter<io::Sink> {
    fn clone(&self) -> HashWriter<io::Sink> {
        HashWriter {
            writer: io::sink(),
            hasher: self.hasher.clone(),
        }
    }
}

impl<W: Write> HashWriter<W> {
    /// Construct a new `HashWriter` given an existing `writer` by value.
    pub fn new(writer: W) -> Self {
        HashWriter {
            writer,
            hasher: Blake2b::new(64),
        }
    }

    /// Destroy this writer and return the hash of what was written.
    pub fn into_hash(self) -> [u8; 64] {
        let mut tmp = [0u8; 64];
        tmp.copy_from_slice(self.hasher.finalize().as_ref());
        tmp
    }
}

impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let bytes = self.writer.write(buf)?;

        if bytes > 0 {
            self.hasher.update(&buf[0..bytes]);
        }

        Ok(bytes)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer.flush()
    }
}

/// This is a cheap helper utility that exists purely
/// because Rust still doesn't have type-level integers
/// and so doesn't implement `PartialEq` for `[T; 64]`
pub fn contains_contribution(contributions: &[[u8; 64]], my_contribution: &[u8; 64]) -> bool {
    for contrib in contributions {
        if contrib[..] == my_contribution[..] {
            return true;
        }
    }
    false
}
