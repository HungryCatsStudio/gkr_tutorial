use ark_crypto_primitives::sponge::poseidon::PoseidonSponge;
use ark_crypto_primitives::sponge::Absorb;
use ark_ff::PrimeField;

use ark_poly::{MultilinearExtension, SparseMultilinearExtension};

use crate::{
    data_structures::{GKRVerifierProduct, GKRVerifierSumOfProducts, Product, SumOfProducts},
    oracles::{CombinedPolyOracle, GKROracle, Oracle, PolyOracle},
    utils::{interpolate_uni_poly, test_sponge, usize_to_zxy, SumcheckProof, Transcript},
};

pub struct Prover<F: PrimeField + Absorb, MLE: MultilinearExtension<F>> {
    sum_of_products: SumOfProducts<F, MLE>,
    sponge: PoseidonSponge<F>,
    transcript: Transcript<F>,
}

pub struct Verifier<F: PrimeField + Absorb, O: Oracle<F>> {
    oracle: O,
    sponge: PoseidonSponge<F>,
    transcript: Transcript<F>,
}

pub(crate) fn to_le_indices(v: usize) -> Vec<usize> {
    // preparing index conversion big-endian -> little-endian
    // we do this because `DenseMultilinearExtension::from_evaluations_slice` expects little-endian notation for the evaluation slice
    (0usize..(1 << v))
        .map(|i| i.reverse_bits() >> (usize::BITS as usize - v))
        .collect()
}

impl<F: PrimeField + Absorb, MLE: MultilinearExtension<F>> Prover<F, MLE> {
    pub fn new(sum_of_products: SumOfProducts<F, MLE>, sponge: PoseidonSponge<F>) -> Self {
        Self {
            sum_of_products,
            sponge,
            transcript: Transcript::new(),
        }
    }

    #[allow(non_snake_case)]
    fn sumcheck_prod(
        &mut self,
        A_fs: &mut Vec<Vec<F>>,
        A_gs: &mut Vec<Vec<F>>,
        v: usize,
    ) -> (F, Vec<F>) {
        // first round; claimed_value contains the value the Prover wants to prove
        let num_summands = A_fs.len();
        assert_eq!(num_summands, A_gs.len());

        let claimed_value = (0..num_summands)
            .map(|summand| {
                (0..1 << v)
                    .map(|i| A_fs[summand][i] * A_gs[summand][i])
                    .sum::<F>()
            })
            .sum();

        let le_indices = to_le_indices(v);
        let mut random_challenges = Vec::new();

        for i in 1..=v {
            let mut values = vec![F::zero(); 3];

            for summand in 0..num_summands {
                let mut f_values = vec![vec![F::zero(); 1 << (v - 1)]; 3];
                let mut g_values = vec![vec![F::zero(); 1 << (v - 1)]; 3];
                let A_f = &A_fs[summand];
                let A_g = &A_gs[summand];
                // Algorithm 1
                for b in 0..(1 << (v - i)) {
                    f_values[0][b] = A_f[le_indices[b]];
                    f_values[1][b] = A_f[le_indices[b + (1 << (v - i))]];
                    f_values[2][b] =
                        -A_f[le_indices[b]] + A_f[le_indices[b + (1 << (v - i))]] * F::from(2u64);

                    g_values[0][b] = A_g[le_indices[b]];
                    g_values[1][b] = A_g[le_indices[b + (1 << (v - i))]];
                    g_values[2][b] =
                        -A_g[le_indices[b]] + A_g[le_indices[b + (1 << (v - i))]] * F::from(2u64);
                }

                // Algorithm 3
                // evaluations at t = 0,1,2
                let temp_values: Vec<F> = (0..=2)
                    .map(|t| ((0..1 << (v - i)).map(|b| f_values[t][b] * g_values[t][b])).sum())
                    .collect();

                values[0] += temp_values[0];
                values[1] += temp_values[1];
                values[2] += temp_values[2];
            }

            let r_i = self.transcript.update(&values, &mut self.sponge);
            random_challenges.push(r_i);

            // Algorithm 1, part 2
            for summand in 0..num_summands {
                let A_f = &mut A_fs[summand];
                let A_g = &mut A_gs[summand];

                for b in 0..(1 << (v - i)) {
                    A_f[le_indices[b]] = A_f[le_indices[b]] * (F::one() - r_i)
                        + A_f[le_indices[b + (1 << (v - i))]] * r_i;
                    A_g[le_indices[b]] = A_g[le_indices[b]] * (F::one() - r_i)
                        + A_g[le_indices[b + (1 << (v - i))]] * r_i;
                }
            }
        }

        (claimed_value, random_challenges)
    }

    // Algorithm 6 (Sumcheck GKR) adapted to a sum of products
    #[allow(non_snake_case)]
    pub fn run(&mut self, g1: &[F], g2: &[F], alpha: F, beta: F) -> (SumcheckProof<F>, Vec<F>) {
        let Product(_, pol, _) = &self.sum_of_products.terms[0];
        let k: usize = pol.num_vars();

        let mut A_hs: Vec<Vec<F>> = Vec::new();
        let mut A_f2s: Vec<Vec<F>> = Vec::new();

        // phase 1
        for Product(f1, f2, f3) in self.sum_of_products.terms.iter() {
            // TODO can we share precomputation for the first two products?
            A_hs.push(initialise_phase_1(f1, f3, g1, g2, alpha, beta));
            A_f2s.push(f2.to_evaluations());
        }

        let (out, u) = self.sumcheck_prod(&mut A_hs, &mut A_f2s, k);

        // the final claim comes from the first run, since the second is summing over a "different" `f1` (with `x` fixed to some random `u`)
        self.transcript.set_claim(out);

        // phase 2

        let _A_f3s: Vec<Vec<F>> = Vec::new();
        let mut A_f1s: Vec<Vec<F>> = Vec::new();
        // let f2s_u: Vec<F> = Vec::new();
        let mut A_f3_f2_us: Vec<Vec<F>> = Vec::new();
        for Product(f1, f2, f3) in &self.sum_of_products.terms {
            let f2_u = f2.evaluate(&u).unwrap();
            let A_f3 = f3.to_evaluations();

            A_f1s.push(initialise_phase_2::<F>(f1, g1, g2, &u, alpha, beta));
            A_f3_f2_us.push(A_f3.iter().map(|x| *x * f2_u).collect::<Vec<F>>());
        }

        let (_, v) = self.sumcheck_prod(&mut A_f1s, &mut A_f3_f2_us, k);

        println!("Prover finished successfully");

        (self.transcript.proof.clone(), [u, v].concat())
    }
}

impl<F: PrimeField + Absorb, O: Oracle<F>> Verifier<F, O> {
    pub fn new(oracle: O, sponge: PoseidonSponge<F>, proof: SumcheckProof<F>) -> Self {
        Self {
            oracle,
            sponge,
            transcript: Transcript { proof },
        }
    }

    pub fn run(&mut self) -> (bool, Vec<F>) {
        let v = self.oracle.num_rounds();

        // last_pol is initialised to a singleton list containing the claimed value
        // in subsequent rounds, it contains the values of the would-be-sent polynomial at 0, 1 and 2
        let mut last_pol = vec![self.transcript.proof.claim.unwrap()];

        // dummy scalar to be used when evaluating the constant poly in the first iteration
        let mut last_sent_scalar = F::one();

        let mut random_challenges = Vec::new();

        // round numbering in line with Thaler's book (necessarily inconsistent variable indexing!)
        for r in 0..v {
            let new_pol_evals = self.transcript.proof.values[r].clone();
            println!(
                "Verifier received evaluations of f_{}: {:?}",
                r, new_pol_evals
            );

            let claimed_sum = new_pol_evals[0] + new_pol_evals[1];
            let new_pol = Vec::from(&new_pol_evals[..]);

            if claimed_sum != interpolate_uni_poly(&last_pol, last_sent_scalar) {
                println!("Verifier found inconsistent evaluation in round {r}: received univariate polynomial is f_n = {new_pol:?},\n  \
                          previous one is f_o = {last_pol:?}. The equation f_n(0) + f_n(1) = f_o({last_sent_scalar}) fails to hold. Aborting."
                        );
                return (false, random_challenges);
            }
            last_sent_scalar = self.transcript.update(&new_pol_evals, &mut self.sponge);
            random_challenges.push(last_sent_scalar);

            last_pol = new_pol;
        }

        if interpolate_uni_poly(&last_pol, last_sent_scalar)
            != self
                .oracle
                .divinate(&random_challenges[..(v / 2)], &random_challenges[(v / 2)..])
        {
            println!(
                "Verifier found inconsistent evaluation in the oracle call: \
                        received univariate polynomial {last_pol:?} evaluates to {},\n  \
                        whereas original multivariate one evaluates to {} on {:?}. Aborting.",
                interpolate_uni_poly(&last_pol, last_sent_scalar),
                self.oracle
                    .divinate(&random_challenges[..(v / 2)], &random_challenges[(v / 2)..],),
                random_challenges,
            );
            return (false, random_challenges);
        }

        println!(
            "Verifier finished successfully and is confident in the value {:?} {}",
            self.transcript.proof.claim,
            self.oracle.trust_message()
        );

        (true, random_challenges)
    }
}

pub(crate) fn _old_precompute<F: PrimeField>(vals: &[F]) -> Vec<F> {
    // TODO since we're cloning the new table, maybe the table sizes can be optimized
    // or keep 2 fixed tables and swap pointers in each iteration
    let n = vals.len();

    let mut table = vec![F::zero(); 1 << n];
    let mut new_table: Vec<F> = vec![F::zero(); 1 << n];

    table[0] = F::one();

    for i in 0..n {
        for b in 0..(1 << i) {
            new_table[2 * b] = table[b] * (F::one() - vals[i]);
            new_table[2 * b + 1] = table[b] * vals[i];
        }
        table = new_table.clone();
    }

    new_table
}

pub(crate) fn precompute<F: PrimeField>(vals: &[F]) -> Vec<F> {
    let n = vals.len();

    let mut table_1 = vec![F::zero(); 1 << n];
    let mut table_2: Vec<F> = vec![F::zero(); 1 << n];

    let mut old_table = &mut table_1;
    let mut new_table = &mut table_2;

    old_table[0] = F::one();

    for i in 0..n {
        for b in 0..(1 << i) {
            new_table[2 * b] = old_table[b] * (F::one() - vals[i]);
            new_table[2 * b + 1] = old_table[b] * vals[i];
        }
        std::mem::swap(&mut old_table, &mut new_table);
    }

    if n % 2 == 0 {
        table_1
    } else {
        table_2
    }
}

pub(crate) fn initialise_phase_1<F: PrimeField, MLE: MultilinearExtension<F>>(
    f_1: &SparseMultilinearExtension<F>,
    f_3: &MLE,
    g1: &[F],
    g2: &[F],
    alpha: F,
    beta: F,
) -> Vec<F> {
    let v = f_3.num_vars();
    let le_indices_f1 = to_le_indices(f_1.num_vars);
    let le_indices_f3 = to_le_indices(v);

    let table_g1 = precompute(g1);
    let table_g2 = precompute(g2);
    let table_g = table_g1
        .iter()
        .zip(table_g2.iter())
        .map(|(x, y)| alpha * *x + beta * *y)
        .collect::<Vec<F>>();

    let mut ahg = vec![F::zero(); 1 << v];

    for (idx_le, val) in f_1.evaluations.iter() {
        let idx = le_indices_f1[*idx_le];
        let (z, x, y) = usize_to_zxy(idx, v);
        ahg[le_indices_f3[x]] += table_g[z] * val * f_3.to_evaluations()[le_indices_f3[y]];
    }

    ahg
}

pub(crate) fn initialise_phase_2<F: PrimeField>(
    f_1: &SparseMultilinearExtension<F>,
    g1: &[F],
    g2: &[F],
    u: &[F],
    alpha: F,
    beta: F,
) -> Vec<F> {
    let v = g1.len();
    let le_indices_f_1 = to_le_indices(f_1.num_vars);
    let le_indices_g: Vec<usize> = to_le_indices(v);

    let table_g1 = precompute(g1);
    let table_g2 = precompute(g2);
    let table_u = precompute(u);

    let mut af1 = vec![F::zero(); 1 << v];

    for (idx_le, val) in f_1.evaluations.iter() {
        let idx = le_indices_f_1[*idx_le];
        let (z, x, y) = usize_to_zxy(idx, v);
        af1[le_indices_g[y]] += (alpha * table_g1[z] + beta * table_g2[z]) * table_u[x] * val;
    }

    af1
}

// run the protocol and return true iff the verifier does not abort
pub fn run_sumcheck_protocol<F: PrimeField + Absorb, MLE: MultilinearExtension<F>>(
    f1: SparseMultilinearExtension<F>,
    f2: MLE,
    f3: MLE,
    g: &[F],
) {
    let simple_sum = SumOfProducts {
        terms: vec![Product(f1, f2, f3)],
    };

    // Need to use the same sponge, since it's initialized with random values
    let sponge = test_sponge();

    let mut prover = Prover::new(simple_sum.clone(), sponge.clone());

    let (proof, _) = prover.run(g, g, F::one(), F::zero());

    let mut verifier = Verifier::new(
        PolyOracle::<F, MLE>::new(simple_sum, g.to_vec()), // TODO: decide if passing & is enough
        sponge,
        proof,
    );

    let (result, _) = verifier.run();
    assert!(result)
}

// run the protocol and return true iff the verifier does not abort
pub fn run_sumcheck_protocol_combined<F: PrimeField + Absorb, MLE: MultilinearExtension<F>>(
    f1: SparseMultilinearExtension<F>,
    f2: MLE,
    f3: MLE,
    g1: &[F],
    g2: &[F],
    alpha: F,
    beta: F,
) {
    let simple_sum = SumOfProducts {
        terms: vec![Product(f1.clone(), f2.clone(), f3.clone())],
    };

    // Need to use the same sponge, since it's initialized with random values
    let sponge = test_sponge();

    let mut prover = Prover::new(simple_sum, sponge.clone());

    let (proof, random_challenges) = prover.run(g1, g2, alpha, beta);

    let num_challenges = random_challenges.len();

    let (b_star, c_star) = random_challenges.split_at(num_challenges / 2);

    let w_u_i = &f2.evaluate(b_star).unwrap();
    let w_v_i = &f3.evaluate(c_star).unwrap();

    let gkr_prod = GKRVerifierProduct(f1, *w_u_i, *w_v_i);
    let gkr_sum = GKRVerifierSumOfProducts {
        terms: vec![gkr_prod],
    };

    let gkr_oracle = GKROracle::new(gkr_sum, g1.to_vec(), g2.to_vec(), alpha, beta);

    let mut verifier = Verifier::new(gkr_oracle, sponge, proof);

    assert!(verifier.run().0);
}

// run the protocol and return true iff the verifier does not abort
pub fn run_sumcheck_protocol_combined_multiprod<
    F: PrimeField + Absorb,
    MLE: MultilinearExtension<F>,
>(
    sum_of_products: SumOfProducts<F, MLE>,
    g1: &[F],
    g2: &[F],
    alpha: F,
    beta: F,
) {
    // Need to use the same sponge, since it's initialized with random values
    let sponge = test_sponge();
    let mut prover = Prover::new(sum_of_products.clone(), sponge.clone());

    let (proof, _) = prover.run(g1, g2, alpha, beta);

    let mut verifier = Verifier::new(
        CombinedPolyOracle::<F, MLE>::new(sum_of_products, g1.to_vec(), g2.to_vec(), alpha, beta),
        sponge,
        proof,
    );

    assert!(verifier.run().0);
}
