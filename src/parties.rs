use std::marker::PhantomData;

use ark_crypto_primitives::sponge::poseidon::PoseidonSponge;
use ark_crypto_primitives::sponge::Absorb;
use ark_ff::PrimeField;

use ark_poly::MultilinearExtension;

use crate::utils::{interpolate_uni_poly, test_sponge, Transcript};

pub struct Prover<F: PrimeField + Absorb, MLE: MultilinearExtension<F>> {
    f: MLE,
    g: MLE,
    sponge: PoseidonSponge<F>,
    transcript: Transcript<F>,
    le_indices: Vec<usize>
}

pub trait Oracle<F> {
    fn divinate(&self, x: &[F]) -> F;
    fn num_vars(&self) -> usize;
}

struct PolyOracle<F: PrimeField, MLE: MultilinearExtension<F>> {
    f: MLE,
    g: MLE,
    phantom: PhantomData<F>,
}

impl<F: PrimeField, MLE: MultilinearExtension<F>> PolyOracle<F, MLE> {
    fn new(f: MLE, g: MLE) -> Self {
        Self {
            f,
            g,
            phantom: PhantomData,
        }
    }
}

impl<F: PrimeField, MLE: MultilinearExtension<F>> Oracle<F> for PolyOracle<F, MLE> {
    fn divinate(&self, x: &[F]) -> F {
        self.f.evaluate(&x).unwrap() * self.g.evaluate(&x).unwrap()
    }

    // returns the number of variables of EACH of the two polynomials
    // the total number of variables of the product is twice that
    fn num_vars(&self) -> usize {
        self.f.num_vars()
    }
}

pub struct Verifier<F: PrimeField + Absorb, O: Oracle<F>> {
    oracle: O,
    sponge: PoseidonSponge<F>,
    transcript: Transcript<F>,
}

impl<F: PrimeField + Absorb, MLE: MultilinearExtension<F>> Prover<F, MLE> {

    fn new(f: MLE, g: MLE, sponge: PoseidonSponge<F>) -> Self {
        let v = f.num_vars();
        assert_eq!(v, g.num_vars(), "number of variables mismatched: {v} vs {}", g.num_vars());

        // preparing index conversion big-endian -> little-endian
        // we do this because `DenseMultilinearExtension::from_evaluations_slice` expects little-endian notation for the evaluation slice
        let le_indices: Vec<usize> = (0usize..(1 << v))
            .into_iter()
            .map(|i| i.reverse_bits() >> (usize::BITS as usize - v))
            .collect();

        Self {
            f,
            g,
            sponge,
            transcript: Transcript::new(),
            le_indices 
        }

    }
    fn sumcheck_prod(&mut self, f: &MLE, g: &MLE) {
    //fn sumcheck_prod(&mut self) {

        let v = self.f.num_vars();

        let mut af = f.to_evaluations();
        let mut ag = g.to_evaluations();

        // first round; claimed_value contains the value the Prover wants to prove
        let claimed_value = (0..1 << v).map(|i| af[i] * ag[i]).sum();

        self.transcript
            .update(&[claimed_value], &mut self.sponge);

        let mut f_values = vec![vec![F::zero(); 1 << (v - 1)];3];
        let mut g_values = vec![vec![F::zero(); 1 << (v - 1)];3];

        for i in 1..=v {
            // i-th round

            // Algorithm 1
            let le_indices = &self.le_indices;

            for b in 0..(1 << (v - i)) {
                f_values[0][b] = af[le_indices[b]];
                f_values[1][b] = af[le_indices[b + (1 << (v - i))]];
                f_values[2][b] = -af[le_indices[b]] + af[le_indices[b + (1 << (v - i))]] * F::from(2u64);

                g_values[0][b] = ag[le_indices[b]];
                g_values[1][b] = ag[le_indices[b + (1 << (v - i))]];
                g_values[2][b] = -ag[le_indices[b]] + ag[le_indices[b + (1 << (v - i))]] * F::from(2u64);
            }

            // Algorithm 3
            let values: Vec<F> = (0..=2)
                .map(|t| ((0..1 << (v - i)).map(|b| f_values[t][b] * g_values[t][b])).sum()).collect();

            let r_i = self.transcript.update(&values, &mut self.sponge);

            // Algorithm 1, part 2
            for b in 0..(1 << (v - i)) {
                af[le_indices[b]] =
                    af[le_indices[b]] * (F::one() - r_i) + af[le_indices[b + (1 << (v - i))]] * r_i;
                ag[le_indices[b]] =
                    ag[le_indices[b]] * (F::one() - r_i) + ag[le_indices[b + (1 << (v - i))]] * r_i;    
            }
        }
    }

    fn run(&mut self) -> Transcript<F> {

        self.sumcheck_prod(&self.f.clone(), &self.g.clone());

        println!("Prover finished successfully");

        self.transcript.clone()
    }
}

impl<F: PrimeField + Absorb, O: Oracle<F>> Verifier<F, O> {
    fn run(&mut self) -> bool {
        self.transcript.verify(&mut self.sponge);

        let v = self.oracle.num_vars();

        // last_pol is initialised to a singleton list containing the claimed value
        // in subsequent rounds, it contains the values of the would-be-sent polynomial at 0, 1 and 2
        let mut last_pol = Vec::from(&self.transcript.values[0][..]);

        let mut last_sent_scalar = self.transcript.challenges[0];

        // round numbering in line with Thaler's book (necessarily inconsistent variable indexing!)
        for r in 1..=v {
            let new_pol_evals = self.transcript.values[r].clone();
            let claimed_sum = new_pol_evals[0] + new_pol_evals[1];
            let new_pol = Vec::from(&new_pol_evals[..]);

            if claimed_sum != interpolate_uni_poly(&last_pol, last_sent_scalar) {
                println!("Verifier found inconsistent evaluation in round {r}: received univariate polynomial is f_n = {new_pol:?},\n  \
                          previous one is f_o = {last_pol:?}. The equation f_n(0) + f_n(1) = f_o({last_sent_scalar}) fails to hold. Aborting."
                        );
                return false;
            }

            // cannot fail to unwrap due to round count
            last_sent_scalar = self.transcript.challenges[r];

            last_pol = new_pol;
        }

        if interpolate_uni_poly(&last_pol, last_sent_scalar)
            != self.oracle.divinate(&self.transcript.challenges[1..])
        {
            println!(
                "Verifier found inconsistent evaluation in the oracle call: \
                        received univariate polynomial {last_pol:?} evaluates to {},\n  \
                        whereas original multivariate one evaluates to {} on {:?}. Aborting.",
                interpolate_uni_poly(&last_pol, last_sent_scalar),
                self.oracle.divinate(&self.transcript.challenges[1..]),
                self.transcript.challenges[1..].to_vec(),
            );
            return false;
        }

        println!(
            "Verifier finished successfully and is confident in the value {}",
            self.transcript.values[0][0]
        );

        true
    }
}

// run the protocol and return true iff the verifier does not abort
pub fn run_sumcheck_protocol<F: PrimeField + Absorb, MLE: MultilinearExtension<F>>(f: MLE, g: MLE) {
    println!("Initiated sumcheck protocol on the product of the polynomials \n\t{f:?}\nand\n\t{g:?}");

    let mut prover = Prover::new(f.clone(), g.clone(), test_sponge());

    let transcript = prover.run();

    let mut verifier = Verifier {
        oracle: PolyOracle::new(f, g), // TODO: decide if passing & is enough
        sponge: test_sponge(),
        transcript,
    };

    assert!(verifier.run());
}

